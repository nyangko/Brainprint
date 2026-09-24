//! Length-prefixed JSON framing shared by both ends of the local IPC
//! connection (#15 task 9).
//!
//! Generic over any [`tokio::io::AsyncRead`]/[`AsyncWrite`] stream, so the
//! exact same read/write logic runs unmodified over a Unix domain socket
//! or a Windows named pipe -- only [`crate::protocol::transport`] differs
//! per platform.

use std::io;

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Messages larger than this are refused rather than trusted to allocate
/// an attacker/bug-controlled buffer size. Handshake/status payloads are a
/// few hundred bytes; this leaves generous headroom without being
/// unbounded.
///
/// #24 Task 11 §10: the bound applies symmetrically to both directions.
/// A query result that would encode larger than this is never written --
/// the daemon adapter preflights it and substitutes a typed
/// `RESULT_TOO_LARGE` error response before calling [`write_message`], so
/// this function's own check is a defensive backstop, not the primary
/// mechanism.
pub const MAX_MESSAGE_BYTES: u32 = 1024 * 1024;

/// Serialize `message` as length-prefixed JSON and write it to `stream`.
pub async fn write_message<S, T>(stream: &mut S, message: &T) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(message).map_err(to_io_error)?;
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too large to frame"))?;
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("message of {len} bytes exceeds the {MAX_MESSAGE_BYTES}-byte frame limit"),
        ));
    }

    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&body).await?;
    stream.flush().await
}

/// Read one length-prefixed JSON message from `stream`.
pub async fn read_message<S, T>(stream: &mut S) -> io::Result<T>
where
    S: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_bytes = [0_u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes);
    if len > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message of {len} bytes exceeds the {MAX_MESSAGE_BYTES}-byte frame limit"),
        ));
    }

    let mut body = vec![0_u8; len as usize];
    stream.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(to_io_error)
}

fn to_io_error(source: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, source)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tokio::io::duplex;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Sample {
        value: String,
    }

    #[tokio::test]
    async fn a_written_message_reads_back_identical() {
        let (mut a, mut b) = duplex(4096);
        let message = Sample {
            value: "hello".to_owned(),
        };

        write_message(&mut a, &message)
            .await
            .expect("write should succeed");
        let decoded: Sample = read_message(&mut b).await.expect("read should succeed");

        assert_eq!(decoded, message);
    }

    #[tokio::test]
    async fn two_messages_in_sequence_do_not_interfere() {
        let (mut a, mut b) = duplex(4096);
        let first = Sample {
            value: "first".to_owned(),
        };
        let second = Sample {
            value: "second".to_owned(),
        };

        write_message(&mut a, &first).await.expect("write 1 ok");
        write_message(&mut a, &second).await.expect("write 2 ok");

        let decoded_first: Sample = read_message(&mut b).await.expect("read 1 ok");
        let decoded_second: Sample = read_message(&mut b).await.expect("read 2 ok");

        assert_eq!(decoded_first, first);
        assert_eq!(decoded_second, second);
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected() {
        let (mut a, mut b) = duplex(4096);
        a.write_all(&(MAX_MESSAGE_BYTES + 1).to_be_bytes())
            .await
            .expect("length prefix should write");

        let result: io::Result<Sample> = read_message(&mut b).await;
        assert!(result.is_err());
    }
}
