//! #89 acceptance: the background daemon lifecycle and on-demand
//! auto-start, through the real `brainprint`, `brainprintd` and
//! `brainprint-mcp` binaries -- every daemon here is a real detached
//! process, started the way a user's command would start it, and stopped
//! before the test ends.

use std::{
    fs,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use brainprint_core::{
    lifecycle::NO_AUTOSTART_ENV,
    protocol::{self, EndpointPaths, HandshakeResponse, Request, Response},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// An isolated user: its own home, so its own daemon endpoint and log.
/// Dropping it stops whatever daemon it still has.
struct Home(PathBuf);

impl Home {
    fn create(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "bp-i89-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("home");
        Self(path)
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove(NO_AUTOSTART_ENV);
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(&binary("brainprint"))
            .args(args)
            .output()
            .expect("brainprint")
    }

    fn endpoint(&self) -> EndpointPaths {
        EndpointPaths::from_runtime_root(self.0.join(".brainprint").join("runtime"))
    }

    fn log(&self) -> String {
        fs::read_to_string(self.0.join(".brainprint/logs/brainprintd.log")).unwrap_or_default()
    }

    /// The pid `daemon status` reports, or `None` when not running.
    fn running_pid(&self) -> Option<u32> {
        let status = self.cli(&["daemon", "status"]);
        let out = stdout(&status);
        out.starts_with("running: ").then(|| pid_in(&out))
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.cli(&["daemon", "stop"]);
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A binary of this build: they all land next to `brainprintd`.
fn binary(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_brainprintd"))
        .with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "build the workspace first (`cargo build --workspace`): {}",
        path.display()
    );
    path
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The number after the first `pid ` in `text`.
fn pid_in(text: &str) -> u32 {
    let rest = &text[text.find("pid ").expect("a pid") + 4..];
    rest.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("pid")
}

fn listening_lines(home: &Home) -> usize {
    home.log().matches(": listening (pid ").count()
}

#[test]
fn help_version_and_status_never_start_a_daemon() {
    let home = Home::create("passive");
    for args in [
        &["--help"][..],
        &["--version"],
        &["daemon", "status"],
        &["status"],
    ] {
        let _ = home.cli(args);
    }
    for args in ["--help", "--version"] {
        let output = home
            .command(&binary("brainprintd"))
            .arg(args)
            .output()
            .expect("brainprintd");
        assert!(output.status.success(), "{args}");
    }
    let status = home.cli(&["daemon", "status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert_eq!(stdout(&status), "not running\n");
    assert!(home.log().is_empty(), "nothing was started: {}", home.log());
    assert!(!home.endpoint().lock_path.exists());
}

#[test]
fn start_is_idempotent_and_the_daemon_outlives_its_caller() {
    let home = Home::create("start");
    let first = home.cli(&["daemon", "start"]);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(
        stdout(&first).starts_with("started: brainprintd "),
        "{}",
        stdout(&first)
    );
    let pid = pid_in(&stdout(&first));

    let again = home.cli(&["daemon", "start"]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(
        stdout(&again).starts_with("already running: "),
        "{}",
        stdout(&again)
    );
    assert_eq!(pid_in(&stdout(&again)), pid);
    assert_eq!(listening_lines(&home), 1);
    // The `brainprint` that started it has exited; the daemon has not.
    assert_eq!(home.running_pid(), Some(pid));

    #[cfg(unix)]
    {
        // A terminal closing: SIGHUP to the process group of the shell that
        // ran `daemon start`, and to the daemon itself.
        assert!(home.cli(&["daemon", "stop"]).status.success());
        let mut shell = {
            use std::os::unix::process::CommandExt as _;
            home.command(Path::new("/bin/sh"))
                .arg("-c")
                .arg(format!(
                    "'{}' daemon start; sleep 30",
                    binary("brainprint").display()
                ))
                .process_group(0)
                .stdout(Stdio::piped())
                .spawn()
                .expect("shell")
        };
        let mut lines = BufReader::new(shell.stdout.take().expect("stdout")).lines();
        let started = lines.next().expect("a line").expect("utf-8");
        assert!(started.starts_with("started: "), "{started}");
        let pid = pid_in(&started);
        let hangup = |target: String| {
            let status = Command::new("kill")
                .args(["-HUP", "--", &target])
                .status()
                .expect("kill");
            assert!(status.success(), "kill -HUP {target}");
        };
        hangup(format!("-{}", shell.id()));
        let _ = shell.wait();
        hangup(pid.to_string());
        thread::sleep(Duration::from_millis(300));
        assert_eq!(home.running_pid(), Some(pid), "{}", home.log());
    }
}

#[test]
fn concurrent_starts_over_stale_artifacts_leave_exactly_one_daemon() {
    let home = Home::create("race");
    // A crash's leftovers: a lock naming a live process that is not a
    // daemon (this test) and, on Unix, a dead socket file.
    let endpoint = home.endpoint();
    fs::create_dir_all(&endpoint.runtime_root).expect("runtime");
    fs::write(&endpoint.lock_path, std::process::id().to_string()).expect("stale lock");
    #[cfg(unix)]
    {
        fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
        // Bound, then closed: the socket file stays, nothing listens.
        drop(std::os::unix::net::UnixListener::bind(&endpoint.socket_path).expect("socket"));
        assert!(endpoint.socket_path.exists());
    }
    // A stop finds no daemon there and signals nobody.
    let stop = home.cli(&["daemon", "stop"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    assert_eq!(stdout(&stop), "not running\n");

    let starts: Vec<_> = (0..6)
        .map(|_| {
            let binary = binary("brainprint");
            let mut command = home.command(&binary);
            thread::spawn(move || command.args(["daemon", "start"]).output().expect("start"))
        })
        .collect();
    let pids: Vec<u32> = starts
        .into_iter()
        .map(|start| {
            let output = start.join().expect("thread");
            assert!(output.status.success(), "{}", stderr(&output));
            pid_in(&stdout(&output))
        })
        .collect();
    assert!(pids.windows(2).all(|pair| pair[0] == pair[1]), "{pids:?}");
    assert_eq!(listening_lines(&home), 1, "{}", home.log());
    assert_eq!(home.running_pid(), Some(pids[0]));
}

#[test]
fn auto_start_restart_and_stop_keep_the_workspace() {
    let home = Home::create("auto");
    let workspace = home.0.join("ws");
    fs::create_dir_all(workspace.join("src")).expect("ws");
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("src");
    let ws = workspace.to_string_lossy().into_owned();

    // Opted out: the command fails and nothing starts.
    let refused = home
        .command(&binary("brainprint"))
        .env(NO_AUTOSTART_ENV, "1")
        .arg("install")
        .output()
        .expect("install");
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("auto-start is off"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(home.running_pid(), None);
    assert!(home.log().is_empty());

    // Not opted out: the daemon starts on demand and the command succeeds.
    let install = home.cli(&["install"]);
    assert!(install.status.success(), "{}", stderr(&install));
    let first = home.running_pid().expect("auto-started");
    let init = home.cli(&["init", &ws]);
    assert!(init.status.success(), "{}", stderr(&init));
    let identity = fs::read(workspace.join(".brainprint/workspace.toml")).expect("identity");
    let config = fs::read(workspace.join(".brainprint/config.toml")).ok();

    let restart = home.cli(&["daemon", "restart"]);
    assert!(restart.status.success(), "{}", stderr(&restart));
    let out = stdout(&restart);
    assert!(
        out.starts_with(&format!("stopped: brainprintd (pid {first})")),
        "{out}"
    );
    let second = home.running_pid().expect("restarted");
    assert_ne!(second, first);

    // The restarted daemon picks the Workspace up again, unchanged.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let found = home.cli(&["find", "files", "--workspace", &ws]);
        if found.status.success() && stdout(&found).contains("src/lib.rs") {
            break;
        }
        assert!(Instant::now() < deadline, "{}", stderr(&found));
        thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        fs::read(workspace.join(".brainprint/workspace.toml")).expect("identity"),
        identity
    );
    assert_eq!(
        fs::read(workspace.join(".brainprint/config.toml")).ok(),
        config
    );

    let stop = home.cli(&["daemon", "stop"]);
    assert_eq!(
        stdout(&stop),
        format!("stopped: brainprintd (pid {second})\n")
    );
    assert_eq!(home.running_pid(), None);
    let endpoint = home.endpoint();
    assert!(!endpoint.lock_path.exists(), "lock left behind");
    #[cfg(unix)]
    assert!(!endpoint.socket_path.exists(), "socket left behind");
    assert_eq!(stdout(&home.cli(&["daemon", "stop"])), "not running\n");
}

/// A daemon of another protocol (here a stand-in for a 0.1.1 daemon:
/// protocol 14, and no `Shutdown`) is reported and left alone.
#[test]
fn an_incompatible_daemon_is_reported_never_replaced() {
    let home = Home::create("mismatch");
    let endpoint = home.endpoint();
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    #[cfg(unix)]
    let mut listener = {
        fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
        runtime.block_on(async { protocol::Listener::bind(&endpoint.socket_path) })
    }
    .expect("listener");
    #[cfg(windows)]
    let mut listener = runtime
        .block_on(async { protocol::Listener::bind(&endpoint.pipe_name) })
        .expect("listener");
    runtime.spawn(async move {
        loop {
            let Ok(mut connection) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let Ok(Request::Handshake(handshake)) =
                    protocol::framing::read_message::<_, Request>(&mut connection).await
                else {
                    return;
                };
                let _ = protocol::framing::write_message(
                    &mut connection,
                    &Response::Handshake(HandshakeResponse::VersionMismatch {
                        server_protocol_version: 14,
                        client_protocol_version: handshake.protocol_version,
                    }),
                )
                .await;
                // Like a protocol-14 daemon: nothing more on this connection.
            });
        }
    });

    let status = home.cli(&["daemon", "status"]);
    assert!(!status.status.success());
    assert!(
        stdout(&status).contains("running, incompatible: brainprintd speaks protocol 14"),
        "{}",
        stdout(&status)
    );
    for args in [
        &["daemon", "start"][..],
        &["daemon", "stop"],
        &["daemon", "restart"],
    ] {
        let output = home.cli(args);
        assert!(!output.status.success(), "{args:?}");
        assert!(
            stderr(&output).contains("speaking protocol 14")
                && stderr(&output).contains("not replaced"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    let install = home.cli(&["install"]);
    assert!(!install.status.success());
    assert!(
        stderr(&install).contains("protocol version mismatch"),
        "{}",
        stderr(&install)
    );
    assert!(
        home.log().is_empty(),
        "no daemon was started: {}",
        home.log()
    );
    drop(runtime);
}

#[test]
fn mcp_auto_starts_the_daemon_and_the_daemon_outlives_it() {
    let home = Home::create("mcp");
    let mut mcp = home
        .command(&binary("brainprint-mcp"))
        .current_dir(&home.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("brainprint-mcp");
    let mut input = mcp.stdin.take().expect("stdin");
    let mut output = BufReader::new(mcp.stdout.take().expect("stdout")).lines();
    for message in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"i89","version":"0"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"brainprint.context","arguments":{"mode":"status"}}}"#,
    ] {
        writeln!(input, "{message}").expect("write");
    }
    let reply = loop {
        let line = output.next().expect("a reply").expect("utf-8");
        if line.starts_with(r#"{"jsonrpc":"2.0","id":2,"#) {
            break line;
        }
    };
    let reply: serde_json::Value = serde_json::from_str(&reply).expect("json");
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["outcome"], "ok", "{reply}");
    let pid = envelope["payload"]["pid"].as_u64().expect("pid");

    drop(input);
    assert!(mcp.wait().expect("mcp exit").success());
    assert_eq!(
        home.running_pid().map(u64::from),
        Some(pid),
        "the daemon outlives the MCP client"
    );
}
