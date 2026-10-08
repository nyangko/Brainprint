//! #90 acceptance: `brainprint update` through the real binaries of this
//! build, installed as a simulated binary set in a home of its own, and
//! release fixtures: local git repositories read through the debug-only
//! test source override, tagged `v<version>`.
//!
//! None of this is a v0.1.1 self-update: 0.1.1 has no `update` command.
//! The installed set is this build (the updater bootstrap); the fixtures
//! stand in for a later release.
//!
//! Every process wait here is bounded.

use std::{
    env, fs,
    io::Read as _,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use brainprint_core::{
    PROTOCOL_VERSION,
    lifecycle::NO_AUTOSTART_ENV,
    protocol::{
        query::WorkItemSourceKindWire,
        work::{GitObservationWire, NewWorkItemWire, WorkStartItemWire, WorkStartWire},
    },
};

const BINARIES: [&str; 4] = [
    "brainprint",
    "brainprintd",
    "brainprint-mcp",
    "brainprint-agent",
];
const TEST_SOURCE_ENV: &str = "BRAINPRINT_UPDATE_TEST_SOURCE";
const TEST_FAIL_ACTIVATION_ENV: &str = "BRAINPRINT_UPDATE_TEST_FAIL_ACTIVATION";
const QUICK: Duration = Duration::from_secs(120);
/// A whole-workspace release build from source.
const RELEASE_BUILD: Duration = Duration::from_secs(40 * 60);

static NEXT: AtomicU64 = AtomicU64::new(0);

/// This build's version, the one a fixture release follows, and one before.
fn versions() -> (String, String) {
    let current = env!("CARGO_PKG_VERSION");
    let mut parts = current
        .split('.')
        .map(|part| part.parse::<u64>().expect("x.y.z"));
    let (major, minor, patch) = (
        parts.next().expect("major"),
        parts.next().expect("minor"),
        parts.next().expect("patch"),
    );
    (current.to_owned(), format!("{major}.{minor}.{}", patch + 1))
}

/// An isolated user with a simulated installation of this build's four
/// binaries in `<home>/install`. Dropping it stops its daemon.
struct Home {
    root: PathBuf,
    install: PathBuf,
}

impl Home {
    fn create(label: &str) -> Self {
        let root = env::temp_dir().join(format!(
            "bp-i90-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let install = root.join("install");
        fs::create_dir_all(&install).expect("install dir");
        for name in BINARIES {
            fs::copy(built(name), exe(&install, name)).expect("install a binary");
        }
        Self { root, install }
    }

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env_remove("XDG_RUNTIME_DIR")
            .env_remove(NO_AUTOSTART_ENV)
            .env_remove(TEST_SOURCE_ENV)
            .env_remove(TEST_FAIL_ACTIVATION_ENV)
            // The update's cargo and rustup live in the real home.
            .env("CARGO_HOME", cargo_home())
            .env("RUSTUP_HOME", rustup_home())
            .current_dir(&self.root);
        command
    }

    /// The installed `brainprint`.
    fn cli(&self, args: &[&str]) -> Output {
        let mut command = self.command(&exe(&self.install, "brainprint"));
        command.args(args);
        run(command, QUICK)
    }

    fn update(&self, source: &Path, args: &[&str]) -> Output {
        let mut command = self.command(&exe(&self.install, "brainprint"));
        command
            .arg("update")
            .args(args)
            .env(TEST_SOURCE_ENV, source);
        run(command, QUICK)
    }

    fn snapshot(&self) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = fs::read_dir(&self.install)
            .expect("install dir")
            .map(|entry| {
                let entry = entry.expect("entry");
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).expect("read"),
                )
            })
            .collect();
        files.sort();
        files
    }

    fn running_version(&self) -> Option<String> {
        let status = self.cli(&["daemon", "status"]);
        let out = stdout(&status);
        out.strip_prefix("running: brainprintd ")
            .map(|rest| rest.split(' ').next().expect("version").to_owned())
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.join(".brainprint/logs/brainprintd.log")).unwrap_or_default()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // Whichever set is installed now still has a `brainprint`; a
        // fixture's refuses, so try this build's too.
        for brainprint in [exe(&self.install, "brainprint"), built("brainprint")] {
            let _ = self
                .command(&brainprint)
                .args(["daemon", "stop"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn exe(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{}", env::consts::EXE_SUFFIX))
}

/// A binary of this build: they all land next to `brainprintd`.
fn built(name: &str) -> PathBuf {
    let path = exe(
        Path::new(env!("CARGO_BIN_EXE_brainprintd"))
            .parent()
            .expect("dir"),
        name,
    );
    assert!(
        path.is_file(),
        "build the workspace first (`cargo build --workspace`): {}",
        path.display()
    );
    path
}

/// Where cargo and rustup default to: `%USERPROFILE%` on Windows (a
/// POSIX shell's `HOME` there is no Windows path), `$HOME` elsewhere.
fn real_home() -> PathBuf {
    env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .expect("a home directory")
}

fn cargo_home() -> PathBuf {
    env::var_os("CARGO_HOME").map_or_else(|| real_home().join(".cargo"), PathBuf::from)
}

fn rustup_home() -> PathBuf {
    env::var_os("RUSTUP_HOME").map_or_else(|| real_home().join(".rustup"), PathBuf::from)
}

/// `command`'s output; it must exit within `timeout`, and its pipes close
/// within 30s after.
fn run(mut command: Command, timeout: Duration) -> Output {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let (sender, pipes) = mpsc::channel();
    for (index, pipe) in [
        Box::new(child.stdout.take().expect("stdout")) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().expect("stderr")),
    ]
    .into_iter()
    .enumerate()
    {
        let sender = sender.clone();
        thread::spawn(move || {
            let mut pipe = pipe;
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            let _ = sender.send((index, bytes));
        });
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("{command:?} did not exit within {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(50));
    };
    let mut out = [None, None];
    let deadline = Instant::now() + Duration::from_secs(30);
    while out.iter().any(Option::is_none) {
        let (index, bytes) = pipes
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| panic!("{command:?} exited but its output pipes stayed open"));
        out[index] = Some(bytes);
    }
    let [stdout, stderr] = out.map(Option::unwrap_or_default);
    Output {
        status,
        stdout,
        stderr,
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn git(dir: &Path, args: &[&str]) {
    let output = run(
        {
            let mut command = Command::new("git");
            command
                .arg("-C")
                .arg(dir)
                .args([
                    "-c",
                    "user.name=i90",
                    "-c",
                    "user.email=i90@example.invalid",
                ])
                .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
                .args(["-c", "core.autocrlf=false"])
                .args(args);
            command
        },
        QUICK,
    );
    assert!(output.status.success(), "git {args:?}: {}", stderr(&output));
}

fn commit_and_tag(dir: &Path, tag: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", tag]);
    // Annotated: the release list must name its commit, not the tag object.
    git(dir, &["tag", "-a", "-m", tag, tag]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fixture {
    Good,
    CompileError,
    /// `brainprint-agent` is declared but cargo skips it.
    MissingBinary,
    /// `brainprintd` is built at another version.
    DaemonVersion,
    /// The tree under the tag declares another version.
    PackageVersion,
    NotBrainprint,
}

/// A tiny Brainprint-shaped release in `<home>/<label>`, tagged
/// `v<version>`: four binaries that answer `--version` (and, for
/// `brainprint-mcp`, the MCP `initialize`) and refuse anything else -- so
/// a `daemon restart` by the new set fails.
fn fake_release(home: &Home, label: &str, version: &str, fixture: Fixture) -> PathBuf {
    let dir = home.root.join(label);
    fs::create_dir_all(&dir).expect("fixture");
    git(&dir, &["init", "-q"]);
    let declared = if fixture == Fixture::PackageVersion {
        "9.9.9"
    } else {
        version
    };
    let repository = if fixture == Fixture::NotBrainprint {
        "https://example.invalid/other"
    } else {
        "https://github.com/nyangko/Brainprint"
    };
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[workspace]\nmembers = [\"crates/cli\", \"crates/daemon\", \"crates/mcp\", \
             \"crates/agent\"]\nresolver = \"3\"\n\n[workspace.package]\nversion = \
             \"{declared}\"\nedition = \"2024\"\nrepository = \"{repository}\"\n"
        ),
    )
    .expect("manifest");
    for (krate, bin) in [
        ("cli", "brainprint"),
        ("daemon", "brainprintd"),
        ("mcp", "brainprint-mcp"),
        ("agent", "brainprint-agent"),
    ] {
        let version = if fixture == Fixture::DaemonVersion && krate == "daemon" {
            "version = \"0.0.1\"".to_owned()
        } else {
            "version.workspace = true".to_owned()
        };
        let skipped = if fixture == Fixture::MissingBinary && krate == "agent" {
            "required-features = [\"absent\"]\n\n[features]\nabsent = []\n"
        } else {
            ""
        };
        fs::create_dir_all(dir.join("crates").join(krate)).expect("crate");
        fs::write(
            dir.join("crates").join(krate).join("Cargo.toml"),
            format!(
                "[package]\nname = \"fixture-{krate}\"\n{version}\nedition.workspace = true\n\n\
                 [[bin]]\nname = \"{bin}\"\npath = \"../../fixture.rs\"\n{skipped}"
            ),
        )
        .expect("crate manifest");
    }
    let broken = if fixture == Fixture::CompileError {
        "compile_error!(\"release fixture\");\n"
    } else {
        ""
    };
    fs::write(
        dir.join("fixture.rs"),
        format!(
            r##"{broken}use std::io::BufRead as _;

fn main() {{
    let name = env!("CARGO_BIN_NAME");
    let version = env!("CARGO_PKG_VERSION");
    if name == "brainprint-mcp" {{
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        println!(r#"{{{{"jsonrpc":"2.0","id":1,"result":{{{{"serverInfo":{{{{"name":"brainprint-mcp","version":"{{version}}"}}}}}}}}}}}}"#);
        return;
    }}
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--version"] {{
        println!("{{name}} {{version}}");
        return;
    }}
    eprintln!("{{name}} {{version}} is a release fixture: no {{args:?}}");
    std::process::exit(1);
}}
"##
        ),
    )
    .expect("fixture source");
    let mut lock = Command::new("cargo");
    lock.args(["generate-lockfile", "--offline"])
        .current_dir(&dir);
    let locked = run(lock, QUICK);
    assert!(locked.status.success(), "{}", stderr(&locked));
    commit_and_tag(&dir, &format!("v{version}"));
    dir
}

/// This repository's source as release `v<version>`: its tracked and
/// not-ignored new files (uncommitted work under test), top-level dot
/// entries aside, with the version bumped in the workspace manifest and
/// the lockfile.
fn real_release(home: &Home, version: &str) -> PathBuf {
    let (current, _) = versions();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = home.root.join("release");
    let mut list = Command::new("git");
    list.arg("-C")
        .arg(&repo)
        .args(["ls-files", "-z", "--cached"]);
    list.args(["--others", "--exclude-standard", "--", ".", ":!:.*"]);
    let listed = run(list, QUICK);
    assert!(listed.status.success(), "{}", stderr(&listed));
    for file in stdout(&listed).split('\0').filter(|file| !file.is_empty()) {
        let from = repo.join(file);
        if !from.is_file() {
            continue;
        }
        let to = dir.join(file);
        fs::create_dir_all(to.parent().expect("parent")).expect("dir");
        fs::copy(&from, &to).expect("copy");
    }
    let bump = |file: &str, after_line: &dyn Fn(&str) -> bool| {
        let text = fs::read_to_string(dir.join(file)).expect("read");
        let mut previous = "";
        let mut bumped = 0;
        let lines: Vec<String> = text
            .lines()
            .map(|line| {
                let line = line.trim_end_matches('\r');
                let out = if line == format!("version = \"{current}\"") && after_line(previous) {
                    bumped += 1;
                    format!("version = \"{version}\"")
                } else {
                    line.to_owned()
                };
                previous = line;
                out
            })
            .collect();
        assert!(bumped > 0, "{file}: no version {current} to bump");
        fs::write(dir.join(file), lines.join("\n") + "\n").expect("write");
    };
    bump("Cargo.toml", &|_| true);
    bump("Cargo.lock", &|previous| {
        previous.starts_with("name = \"brainprint")
    });
    git(&dir, &["init", "-q"]);
    commit_and_tag(&dir, &format!("v{version}"));
    dir
}

fn leftovers(home: &Home) -> Vec<String> {
    home.snapshot()
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| {
            !BINARIES
                .iter()
                .any(|binary| *name == format!("{binary}{}", env::consts::EXE_SUFFIX))
        })
        .collect()
}

/// A long-running `brainprint-mcp` of the installed set, as an MCP client
/// holds it.
fn running_mcp(home: &Home) -> Child {
    home.command(&exe(&home.install, "brainprint-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("brainprint-mcp")
}

/// Windows: a running binary cannot be overwritten or deleted -- the
/// constraint the rename-aside activation is built around.
#[cfg(windows)]
fn assert_locked_while_running(path: &Path) {
    let overwrite = fs::OpenOptions::new().write(true).open(path);
    assert!(
        overwrite.is_err(),
        "{} opened for writing while running",
        path.display()
    );
    assert!(
        fs::remove_file(path).is_err(),
        "{} deleted while running",
        path.display()
    );
}

#[test]
fn check_reports_releases_and_changes_nothing() {
    let home = Home::create("check");
    let (current, next) = versions();
    let source = fake_release(&home, "source", "0.0.1", Fixture::Good);
    commit_and_tag(&source, &format!("v{current}"));
    let before = home.snapshot();

    let check = home.update(&source, &["--check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    let out = stdout(&check);
    assert!(out.contains(&format!("current   {current}")), "{out}");
    assert!(out.contains(&format!("latest    {current}")), "{out}");
    assert!(out.contains("already latest"), "{out}");

    // A pre-release tag is not a release.
    git(&source, &["tag", &format!("v{next}-rc.1")]);
    assert!(stdout(&home.update(&source, &["--check"])).contains("already latest"));

    commit_and_tag(&source, &format!("v{next}"));
    let check = home.update(&source, &["--check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    let out = stdout(&check);
    assert!(out.contains(&format!("latest    {next}")), "{out}");
    assert!(out.contains("update available"), "{out}");

    let missing = home.update(&source, &["--check", "--version", "9.9.9"]);
    assert!(!missing.status.success());
    assert!(
        stderr(&missing).contains("no release 9.9.9"),
        "{}",
        stderr(&missing)
    );

    let malformed = home.update(&source, &["--version", "1.2"]);
    assert_eq!(malformed.status.code(), Some(2), "{}", stderr(&malformed));
    assert!(stderr(&malformed).contains("not a release version"));

    let downgrade = home.update(&source, &["--version", "0.0.1"]);
    assert!(!downgrade.status.success());
    assert!(
        stderr(&downgrade).contains("does not downgrade"),
        "{}",
        stderr(&downgrade)
    );

    // Already at the requested release: an idempotent no-op.
    let same = home.update(&source, &["--version", &current]);
    assert!(same.status.success(), "{}", stderr(&same));
    assert!(stdout(&same).contains("already latest: nothing to do"));

    // Git failure: the source is no repository.
    let not_a_repo = home.root.join("empty");
    fs::create_dir_all(&not_a_repo).expect("dir");
    let failed = home.update(&not_a_repo, &["--check"]);
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains("git ls-remote failed"),
        "{}",
        stderr(&failed)
    );

    assert_eq!(
        home.snapshot(),
        before,
        "a check or refusal changed the installation"
    );
    assert!(
        home.log().is_empty(),
        "no daemon was started: {}",
        home.log()
    );
}

#[test]
fn failed_builds_and_bad_sources_leave_the_installation_untouched() {
    let home = Home::create("fail");
    let (_, next) = versions();
    let before = home.snapshot();
    for (fixture, expected) in [
        (Fixture::CompileError, "the release build failed"),
        (Fixture::MissingBinary, "brainprint-agent"),
        (Fixture::DaemonVersion, "reports version 0.0.1"),
        (Fixture::PackageVersion, "declares version 9.9.9"),
        (Fixture::NotBrainprint, "not Brainprint source"),
    ] {
        let label = format!("source-{expected}").replace(' ', "-");
        let source = fake_release(&home, &label, &next, fixture);
        let update = home.update(&source, &[]);
        assert!(!update.status.success(), "{expected}: {}", stdout(&update));
        assert!(
            stderr(&update).contains(expected),
            "{expected}: {}",
            stderr(&update)
        );
        assert!(
            !stdout(&update).contains("installed"),
            "{}",
            stdout(&update)
        );
        assert_eq!(
            home.snapshot(),
            before,
            "{expected}: the installation changed"
        );
    }
    assert!(
        home.log().is_empty(),
        "no daemon was started: {}",
        home.log()
    );
}

#[test]
fn only_a_complete_writable_installation_is_updated() {
    let home = Home::create("identity");
    let (_, next) = versions();
    let source = fake_release(&home, "source", &next, Fixture::Good);

    // Run from a cargo build directory: refused before anything is fetched.
    let mut from_tree = home.command(&built("brainprint"));
    from_tree.arg("update").env(TEST_SOURCE_ENV, &source);
    let refused = run(from_tree, QUICK);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("cargo build directory"),
        "{}",
        stderr(&refused)
    );

    // A missing sibling: the set is incomplete.
    let agent = exe(&home.install, "brainprint-agent");
    let kept = fs::read(&agent).expect("agent");
    fs::remove_file(&agent).expect("rm");
    let refused = home.update(&source, &[]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("is missing"),
        "{}",
        stderr(&refused)
    );
    fs::write(&agent, kept).expect("restore");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).expect("chmod");

        // Not writable by this user: refused, nothing changed.
        let before = home.snapshot();
        fs::set_permissions(&home.install, fs::Permissions::from_mode(0o555)).expect("chmod");
        let refused = home.update(&source, &[]);
        fs::set_permissions(&home.install, fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(!refused.status.success());
        assert!(
            stderr(&refused).contains("cannot write to"),
            "{}",
            stderr(&refused)
        );
        assert_eq!(home.snapshot(), before);
    }
}

#[test]
fn a_failed_activation_or_daemon_restart_puts_the_previous_set_back() {
    let home = Home::create("rollback");
    let (current, next) = versions();
    let source = fake_release(&home, "source", &next, Fixture::Good);
    let start = home.cli(&["daemon", "start"]);
    assert!(start.status.success(), "{}", stderr(&start));
    let before = home.snapshot();

    // Activation fails after two of the four binaries were swapped in.
    let mut command = home.command(&exe(&home.install, "brainprint"));
    command
        .arg("update")
        .env(TEST_SOURCE_ENV, &source)
        .env(TEST_FAIL_ACTIVATION_ENV, "2");
    let failed = run(command, QUICK);
    assert!(!failed.status.success());
    let error = stderr(&failed);
    assert!(
        error.contains("injected failure") && error.contains("installation is unchanged"),
        "{error}"
    );
    assert!(
        error.contains("previous daemon was started again"),
        "{error}"
    );
    assert_eq!(home.snapshot(), before, "a partial activation stayed");
    assert_eq!(home.running_version().as_deref(), Some(current.as_str()));

    // The new set activates, but its daemon restart fails: the previous
    // set goes back and its daemon is started again.
    let failed = home.update(&source, &[]);
    assert!(!failed.status.success());
    let error = stderr(&failed);
    assert!(
        stdout(&failed).contains("installed ok"),
        "{}",
        stdout(&failed)
    );
    assert!(
        error.contains("`brainprint daemon restart` failed"),
        "{error}"
    );
    assert!(
        error.contains(&format!("the {current} binaries were put back")),
        "{error}"
    );
    assert!(
        error.contains("previous daemon was started again"),
        "{error}"
    );
    assert_eq!(home.snapshot(), before, "the failed set stayed");
    assert_eq!(home.running_version().as_deref(), Some(current.as_str()));

    // No daemon running: the set activates and nothing is started.
    let stop = home.cli(&["daemon", "stop"]);
    assert!(stop.status.success(), "{}", stderr(&stop));
    let mut mcp = running_mcp(&home);
    #[cfg(windows)]
    assert_locked_while_running(&exe(&home.install, "brainprint-mcp"));
    let updated = home.update(&source, &[]);
    assert!(updated.status.success(), "{}", stderr(&updated));
    let out = stdout(&updated);
    assert!(out.contains("installed ok"), "{out}");
    assert!(out.contains("daemon    not running"), "{out}");
    assert!(out.contains("restart required"), "{out}");
    for name in ["brainprint", "brainprintd", "brainprint-agent"] {
        let mut version = Command::new(exe(&home.install, name));
        version.arg("--version");
        assert_eq!(stdout(&run(version, QUICK)), format!("{name} {next}\n"));
    }
    assert!(home.log().lines().all(|line| !line.contains(&next)));
    // The running MCP server is the old process, not replaced in place.
    assert!(
        mcp.try_wait().expect("mcp").is_none(),
        "the running MCP server exited"
    );
    let _ = mcp.kill();
    let _ = mcp.wait();
    // What is left: originals still running when the update ended -- on
    // Windows they cannot be deleted yet; the next update removes them.
    let left = leftovers(&home);
    if cfg!(windows) {
        assert!(
            left.iter().all(|name| name.contains(".update-old-")),
            "{left:?}"
        );
    } else {
        assert!(left.is_empty(), "{left:?}");
    }
}

#[test]
fn a_release_built_from_source_replaces_the_set_and_restarts_the_daemon() {
    let home = Home::create("release");
    let (current, next) = versions();
    let source = real_release(&home, &next);

    let workspace = home.root.join("ws");
    fs::create_dir_all(workspace.join("src")).expect("ws");
    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .expect("src");
    let ws = workspace.to_string_lossy().into_owned();
    for args in [&["install"][..], &["init", &ws]] {
        let output = home.cli(args);
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    }
    let start = serde_json::to_string(&WorkStartWire {
        work_item: WorkStartItemWire::New(NewWorkItemWire {
            source_kind: WorkItemSourceKindWire::Issue,
            source_ref: Some("#90".to_owned()),
            title: None,
            goal: "survive the update".to_owned(),
        }),
        head: None,
        git: GitObservationWire::Unknown,
        owner_agent: None,
    })
    .expect("json");
    let input = home.root.join("start.json");
    fs::write(&input, start).expect("input");
    let input = input.to_string_lossy().into_owned();
    let started = home.cli(&["work", "start", "--workspace", &ws, "--input", &input]);
    assert!(started.status.success(), "{}", stderr(&started));
    let preserved = |home: &Home| {
        let status = home.cli(&["status", &ws, "--json"]);
        assert!(status.status.success(), "{}", stderr(&status));
        let status: serde_json::Value = serde_json::from_slice(&status.stdout).expect("json");
        let report = &status["workspace"]["Initialized"];
        let items = home.cli(&[
            "knowledge",
            "work-items",
            "--status",
            "active",
            "--workspace",
            &ws,
        ]);
        assert!(items.status.success(), "{}", stderr(&items));
        (
            status["daemon_version"]
                .as_str()
                .expect("version")
                .to_owned(),
            [
                report["project_id"].clone(),
                report["workspace_id"].clone(),
                report["basis"].clone(),
            ],
            stdout(&items).contains("survive the update"),
            fs::read(workspace.join(".brainprint/workspace.toml")).expect("identity"),
            fs::read(workspace.join(".brainprint/config.toml")).ok(),
            fs::read(home.root.join(".brainprint/config.toml")).expect("global config"),
        )
    };
    let before = preserved(&home);
    assert_eq!(before.0, current);
    assert!(before.2, "the work item was recorded");

    let mut mcp = running_mcp(&home);
    #[cfg(windows)]
    assert_locked_while_running(&exe(&home.install, "brainprint-mcp"));

    // Another installation first on PATH is never touched.
    let other = home.root.join("other");
    fs::create_dir_all(&other).expect("other");
    fs::copy(built("brainprint"), exe(&other, "brainprint")).expect("other copy");
    let other_bytes = fs::read(exe(&other, "brainprint")).expect("other");
    let path = env::join_paths(
        std::iter::once(other.clone())
            .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .expect("PATH");

    let mut update = home.command(&exe(&home.install, "brainprint"));
    update
        .arg("update")
        .env(TEST_SOURCE_ENV, &source)
        .env("PATH", path)
        // Test build speed, and the new set keeps reading the test source.
        .env("CARGO_PROFILE_RELEASE_OPT_LEVEL", "0")
        .env("CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS", "true");
    let updated = run(update, RELEASE_BUILD);
    let out = stdout(&updated);
    assert!(updated.status.success(), "{out}\n{}", stderr(&updated));
    for line in [
        format!("current   {current}"),
        format!("latest    {next}"),
        format!("source    v{next}"),
        "built     ok".to_owned(),
        "installed ok".to_owned(),
        format!("daemon    restarted · protocol {PROTOCOL_VERSION}"),
        "Claude Code / MCP restart required".to_owned(),
    ] {
        assert!(out.contains(&line), "{line}: {out}");
    }

    for name in ["brainprint", "brainprintd", "brainprint-agent"] {
        let mut version = Command::new(exe(&home.install, name));
        version.arg("--version");
        assert_eq!(stdout(&run(version, QUICK)), format!("{name} {next}\n"));
    }
    // The restarted daemon picks the Workspace up again, unchanged: same
    // identities, stored basis (no rebuild), knowledge and config.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let found = home.cli(&["find", "files", "--workspace", &ws]);
        if found.status.success() && stdout(&found).contains("src/lib.rs") {
            break;
        }
        assert!(Instant::now() < deadline, "{}", stderr(&found));
        thread::sleep(Duration::from_millis(200));
    }
    let after = preserved(&home);
    assert_eq!(after.0, next);
    assert_eq!(after.1, before.1, "identity or stored basis changed");
    assert!(after.2, "the work item is gone");
    assert_eq!(after.3, before.3);
    assert_eq!(after.4, before.4);
    assert_eq!(after.5, before.5);

    assert_eq!(
        fs::read(exe(&other, "brainprint")).expect("other"),
        other_bytes
    );
    assert!(
        mcp.try_wait().expect("mcp").is_none(),
        "the running MCP server exited"
    );
    let _ = mcp.kill();
    let _ = mcp.wait();

    // The updated set is itself an updater: already latest, a no-op.
    let again = home.update(&source, &[]);
    assert!(again.status.success(), "{}", stderr(&again));
    assert!(
        stdout(&again).contains("already latest: nothing to do"),
        "{}",
        stdout(&again)
    );
}

#[test]
fn an_older_protocol_daemon_is_reported_never_stopped() {
    let home = Home::create("p14");
    let (_, next) = versions();
    let source = fake_release(&home, "source", &next, Fixture::Good);
    let endpoint = brainprint_core::protocol::EndpointPaths::from_runtime_root(
        home.root.join(".brainprint").join("runtime"),
    );
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    #[cfg(unix)]
    let mut listener = {
        fs::create_dir_all(endpoint.socket_path.parent().expect("parent")).expect("dir");
        runtime.block_on(async { brainprint_core::protocol::Listener::bind(&endpoint.socket_path) })
    }
    .expect("listener");
    #[cfg(windows)]
    let mut listener = runtime
        .block_on(async { brainprint_core::protocol::Listener::bind(&endpoint.pipe_name) })
        .expect("listener");
    runtime.spawn(async move {
        use brainprint_core::protocol::{HandshakeResponse, Request, Response, framing};
        loop {
            let Ok(mut connection) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let Ok(Request::Handshake(handshake)) =
                    framing::read_message::<_, Request>(&mut connection).await
                else {
                    return;
                };
                // Like a 0.1.1 daemon: protocol 14, nothing more.
                let _ = framing::write_message(
                    &mut connection,
                    &Response::Handshake(HandshakeResponse::VersionMismatch {
                        server_protocol_version: 14,
                        client_protocol_version: handshake.protocol_version,
                    }),
                )
                .await;
            });
        }
    });
    let before = home.snapshot();
    let refused = home.update(&source, &[]);
    assert!(!refused.status.success());
    let error = stderr(&refused);
    assert!(
        error.contains("protocol 14") && error.contains("not replaced"),
        "{error}"
    );
    assert!(error.contains("end its brainprintd process"), "{error}");
    assert_eq!(home.snapshot(), before);
    drop(runtime);
}
