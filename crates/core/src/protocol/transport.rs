//! Local-only IPC transport: Unix domain socket on macOS/Linux, Windows
//! named pipe on Windows (#15 task 9 / #13 task 4 §2).
//!
//! P0 never opens a TCP listener -- there is no `tokio::net::TcpListener`
//! anywhere in this module, by construction, so the daemon endpoint is
//! never network-reachable. Platform differences are contained entirely
//! here: [`Listener`]/[`ServerConnection`]/[`ClientConnection`] present the
//! same shape on every target, so [`crate::protocol::framing`] and any
//! handshake/status logic built on top are written once and never branch
//! on platform.
//!
//! This module only knows how to bind/accept/connect a raw byte stream at
//! an already-resolved endpoint (a filesystem path on Unix, a pipe name on
//! Windows). Resolving *where* that endpoint lives (XDG runtime dir vs.
//! `~/.brainprint/runtime`, the pipe namespace, singleton/stale-artifact
//! recovery) is daemon startup policy, not transport, and lives in
//! `brainprint-daemon` instead.

#[cfg(unix)]
mod platform {
    use std::{io, path::Path};

    use tokio::net::{UnixListener, UnixStream};

    #[derive(Debug)]
    pub struct Listener(UnixListener);
    #[derive(Debug)]
    pub struct ServerConnection(UnixStream);
    #[derive(Debug)]
    pub struct ClientConnection(UnixStream);

    impl Listener {
        /// Bind the Unix domain socket at `socket_path`.
        ///
        /// Fails with [`std::io::ErrorKind::AddrInUse`] if a file already
        /// exists at that path -- live or stale, `bind` cannot tell the
        /// difference, which is exactly the signal daemon startup policy
        /// needs to decide whether to probe liveness and recover.
        pub fn bind(socket_path: &Path) -> io::Result<Self> {
            UnixListener::bind(socket_path).map(Self)
        }

        pub async fn accept(&mut self) -> io::Result<ServerConnection> {
            let (stream, _addr) = self.0.accept().await?;
            Ok(ServerConnection(stream))
        }
    }

    impl ClientConnection {
        pub async fn connect(socket_path: &Path) -> io::Result<Self> {
            UnixStream::connect(socket_path).await.map(Self)
        }
    }

    super::impl_async_io_delegate!(ServerConnection);
    super::impl_async_io_delegate!(ClientConnection);
}

#[cfg(windows)]
mod platform {
    use std::io;

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    #[derive(Debug)]
    pub struct Listener {
        pipe_name: String,
        /// The instance created by [`Self::bind`] with `first_pipe_instance`,
        /// held until the first [`Self::accept`] claims it. Named pipes are
        /// instance-based (unlike a socket listener), so this is what keeps
        /// the pipe name continuously claimed between bind and the first
        /// accepted connection -- dropping it here instead would leave a
        /// window where another process could bind the same name.
        first_instance: Option<NamedPipeServer>,
    }
    #[derive(Debug)]
    pub struct ServerConnection(NamedPipeServer);
    #[derive(Debug)]
    pub struct ClientConnection(NamedPipeClient);

    impl Listener {
        /// Claim `pipe_name` as this daemon's endpoint.
        ///
        /// `first_pipe_instance(true)` mirrors Unix `bind`'s all-or-nothing
        /// semantics: creation fails if another server already owns this
        /// pipe name, live or stale, rather than silently joining an
        /// existing instance pool.
        pub fn bind(pipe_name: &str) -> io::Result<Self> {
            let first_instance = ServerOptions::new()
                .first_pipe_instance(true)
                .create(pipe_name)?;
            Ok(Self {
                pipe_name: pipe_name.to_owned(),
                first_instance: Some(first_instance),
            })
        }

        pub async fn accept(&mut self) -> io::Result<ServerConnection> {
            // Named pipes are instance-based, not listener+accept: each
            // accepted connection consumes one instance, so a fresh one is
            // created for the *next* client before waiting on this one.
            let server = match self.first_instance.take() {
                Some(server) => server,
                None => ServerOptions::new().create(&self.pipe_name)?,
            };
            server.connect().await?;
            Ok(ServerConnection(server))
        }
    }

    impl ClientConnection {
        pub async fn connect(pipe_name: &str) -> io::Result<Self> {
            let client = ClientOptions::new().open(pipe_name)?;
            Ok(Self(client))
        }
    }

    super::impl_async_io_delegate!(ServerConnection);
    super::impl_async_io_delegate!(ClientConnection);
}

pub use platform::{ClientConnection, Listener, ServerConnection};

/// Delegates [`AsyncRead`]/[`AsyncWrite`] from a single-field newtype to
/// its inner stream. Safe: the field is `Unpin` (tokio's stream types all
/// are), so projecting through `Pin::get_mut` never moves pinned data.
macro_rules! impl_async_io_delegate {
    ($ty:ident) => {
        impl tokio::io::AsyncRead for $ty {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::pin::Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
            }
        }

        impl tokio::io::AsyncWrite for $ty {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                std::pin::Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
            }

            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::pin::Pin::new(&mut self.get_mut().0).poll_flush(cx)
            }

            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::pin::Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
            }
        }
    };
}
use impl_async_io_delegate;

#[cfg(all(test, unix))]
mod tests {
    use std::{
        env, fs, io, process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    fn temp_socket_path(label: &str) -> std::path::PathBuf {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "brainprint-transport-{label}-{}-{sequence}",
            process::id()
        ));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir.join("test.sock")
    }

    #[tokio::test]
    async fn accepted_connection_round_trips_bytes() {
        let path = temp_socket_path("roundtrip");
        let mut listener = Listener::bind(&path).expect("bind should succeed");

        let accept_task = tokio::spawn(async move {
            let mut server_side = listener.accept().await.expect("accept should succeed");
            let mut buf = [0_u8; 5];
            server_side
                .read_exact(&mut buf)
                .await
                .expect("server read should succeed");
            server_side
                .write_all(b"world")
                .await
                .expect("server write should succeed");
            buf
        });

        let mut client_side = ClientConnection::connect(&path)
            .await
            .expect("connect should succeed");
        client_side
            .write_all(b"hello")
            .await
            .expect("client write should succeed");
        let mut reply = [0_u8; 5];
        client_side
            .read_exact(&mut reply)
            .await
            .expect("client read should succeed");

        let server_saw = accept_task.await.expect("accept task should not panic");
        assert_eq!(&server_saw, b"hello");
        assert_eq!(&reply, b"world");
    }

    #[tokio::test]
    async fn binding_an_already_bound_path_fails_with_addr_in_use() {
        let path = temp_socket_path("addr-in-use");
        let _first = Listener::bind(&path).expect("first bind should succeed");

        let error = Listener::bind(&path).expect_err("second bind must fail");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn connecting_to_a_missing_endpoint_fails_explicitly() {
        let path = temp_socket_path("missing");

        let error = ClientConnection::connect(&path)
            .await
            .expect_err("connecting to a nonexistent socket must fail");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
