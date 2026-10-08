//! `brainprint update` (#90): replace this installation's binary set with
//! an exact Brainprint source release.
//!
//! There are no prebuilt artifacts (0.1.x), so an update is: the release
//! tags of the canonical repository (`git ls-remote`) -> the exact tag's
//! commit fetched into a temporary directory and checked against that tag
//! -> `cargo build --release --locked` -> the four staged binaries checked
//! to be one release set -> the daemon stopped (#89) -> all four swapped
//! in, put back together on any failure -> a daemon that was running
//! restarted by the new binaries and checked. Nothing here runs unless
//! the user runs `brainprint update`: no background check, no polling.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fmt, fs,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{self, Child, Command, ExitStatus, Stdio},
    str::FromStr,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use brainprint_core::{
    PACKAGE_VERSION,
    lifecycle::{self, Launch, LifecycleError, NO_AUTOSTART_ENV, Probe, Stopped},
    protocol::EndpointPaths,
};

/// The only source a release build of `brainprint update` reads.
const CANONICAL_SOURCE: &str = "https://github.com/nyangko/Brainprint.git";
/// `[workspace.package] repository` of a Brainprint source tree.
const REPOSITORY: &str = "https://github.com/nyangko/Brainprint";
/// Test-only: an absolute path to a local git repository read instead of
/// [`CANONICAL_SOURCE`] -- the #90 acceptance fixtures. Only debug builds
/// read it; a release build, what users install, always uses the canonical
/// source.
const TEST_SOURCE_ENV: &str = "BRAINPRINT_UPDATE_TEST_SOURCE";
/// Test-only (debug builds): fail the activation once this many binaries
/// have been swapped in.
const TEST_FAIL_ACTIVATION_ENV: &str = "BRAINPRINT_UPDATE_TEST_FAIL_ACTIVATION";

/// The managed binary set: always replaced as one.
const BINARIES: [&str; 4] = [
    "brainprint",
    "brainprintd",
    "brainprint-mcp",
    "brainprint-agent",
];
const MCP_INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"brainprint-update","version":"0"}}}"#;
/// A version query or status answer.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// The new `brainprint daemon restart`: a stop waits up to 30s for
/// verification Jobs, a start up to 15s for the handshake.
const RESTART_TIMEOUT: Duration = Duration::from_secs(90);

type Fallible<T> = Result<T, String>;

/// A stable release version, `MAJOR.MINOR.PATCH`; its tag is `v<version>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl FromStr for Version {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid =
            || format!("`{text}` is not a release version: expected MAJOR.MINOR.PATCH, e.g. 0.1.2");
        let parts = text
            .split('.')
            .map(|part| {
                if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(invalid());
                }
                part.parse::<u64>().map_err(|_| invalid())
            })
            .collect::<Result<Vec<_>, _>>()?;
        match parts[..] {
            [major, minor, patch] => Ok(Self(major, minor, patch)),
            _ => Err(invalid()),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// A release tag and the commit it names.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Release {
    version: Version,
    tag: String,
    commit: String,
}

struct Source {
    location: OsString,
    test: bool,
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.test {
            write!(formatter, "test source {}", self.location.to_string_lossy())
        } else {
            formatter.write_str(CANONICAL_SOURCE)
        }
    }
}

pub async fn run(check: bool, version: Option<Version>) -> i32 {
    match update(check, version).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("brainprint: update: {error}");
            1
        }
    }
}

async fn update(check: bool, requested: Option<Version>) -> Fallible<()> {
    // The new `brainprint daemon restart` starts the daemon: keep this
    // process's own output out of both.
    #[cfg(windows)]
    lifecycle::stop_inheriting_stdio();
    let current: Version = PACKAGE_VERSION.parse()?;
    let source = source()?;
    // A check changes nothing, so it needs no installation and no daemon.
    let install = if check {
        None
    } else {
        Some(Install::locate()?)
    };
    let endpoint = EndpointPaths::resolve().map_err(|error| error.to_string())?;
    if install.is_some()
        && let Probe::Incompatible {
            server_protocol_version,
        } = lifecycle::probe(&endpoint)
            .await
            .map_err(|error| format!("cannot reach the daemon endpoint: {error}"))?
    {
        return Err(LifecycleError::Incompatible {
            server_protocol_version,
        }
        .to_string());
    }

    println!("Brainprint update\n");
    row("current", current);
    let releases = releases(&run_tool(
        git().args(["ls-remote", "--tags"]).arg(&source.location),
        "git ls-remote",
    )?);
    let release = match requested {
        Some(version) => releases
            .into_iter()
            .find(|release| release.version == version)
            .ok_or_else(|| format!("no release {version}: {source} has no tag v{version}"))?,
        None => releases
            .into_iter()
            .max_by_key(|release| release.version)
            .ok_or_else(|| format!("{source} has no release tag (vMAJOR.MINOR.PATCH)"))?,
    };
    row(
        if requested.is_some() {
            "target"
        } else {
            "latest"
        },
        release.version,
    );
    let order = release.version.cmp(&current);
    let Some(install) = install else {
        println!();
        println!(
            "{}",
            match order {
                Ordering::Greater => "update available: run `brainprint update`".to_owned(),
                Ordering::Equal => "already latest".to_owned(),
                Ordering::Less if requested.is_some() => format!(
                    "{} is older than this installation: `brainprint update` does not downgrade",
                    release.version
                ),
                Ordering::Less => "this build is newer than the latest release".to_owned(),
            }
        );
        return Ok(());
    };
    match order {
        Ordering::Equal => {
            println!("\nalready latest: nothing to do");
            return Ok(());
        }
        Ordering::Less if requested.is_some() => {
            return Err(format!(
                "{} is older than the installed {current}: `brainprint update` does not \
                 downgrade -- to go back, build and install that release from source",
                release.version
            ));
        }
        Ordering::Less => {
            println!("\nthis build is newer than the latest release: nothing to do");
            return Ok(());
        }
        Ordering::Greater => {}
    }
    row(
        "source",
        if source.test {
            format!("{} ({source})", release.tag)
        } else {
            release.tag.clone()
        },
    );
    row("revision", &release.commit[..release.commit.len().min(12)]);
    row("install", install.dir.display());
    println!();

    let work = WorkDir::create()?;
    check_set(&install.dir, current, &work.home())
        .map_err(|error| format!("the installed binary set is not one {current} set: {error}"))?;
    let tree = fetch(&source, &release, &work.0)?;
    verify_source(&tree, release.version)?;
    build(&tree, &work.target())?;
    let staged = work.target().join("release");
    check_set(&staged, release.version, &work.home())
        .map_err(|error| format!("the built binaries were not installed: {error}"))?;
    row("built", "ok");

    install.sweep();
    let was_running = match lifecycle::stop(&endpoint)
        .await
        .map_err(|error| error.to_string())?
    {
        Stopped::Stopped { .. } => true,
        Stopped::NotRunning => false,
    };
    let swap = match activate(&install.dir, &staged) {
        Ok(swap) => swap,
        Err(error) => {
            let daemon = restart_previous(&endpoint, &install.dir, was_running).await;
            return Err(format!("{error}{daemon}"));
        }
    };
    row("installed", "ok");
    if was_running {
        match restart_daemon(&install.dir, release.version) {
            Ok(protocol) => row("daemon", format!("restarted · protocol {protocol}")),
            Err(error) => {
                let restored = match swap.restore() {
                    Ok(()) => format!("the {current} binaries were put back"),
                    Err(failed) => format!(
                        "putting the {current} binaries back failed ({failed}): {} may hold a \
                         mixed set -- reinstall from source",
                        install.dir.display()
                    ),
                };
                let daemon = restart_previous(&endpoint, &install.dir, true).await;
                return Err(format!("{error}; {restored}{daemon}"));
            }
        }
    } else {
        row("daemon", "not running · starts on next use");
    }
    swap.finish();
    println!(
        "\nClaude Code / MCP restart required: a brainprint-mcp that is already running keeps \
         serving {current} until its client starts it again."
    );
    Ok(())
}

fn row(label: &str, value: impl fmt::Display) {
    println!("{label:<9} {value}");
}

fn source() -> Fallible<Source> {
    if cfg!(debug_assertions)
        && let Some(path) = env::var_os(TEST_SOURCE_ENV).filter(|path| !path.is_empty())
    {
        let path = PathBuf::from(path);
        if !path.is_absolute() || !path.is_dir() {
            return Err(format!(
                "{TEST_SOURCE_ENV} must be an absolute path to a local repository: {}",
                path.display()
            ));
        }
        return Ok(Source {
            location: path.into_os_string(),
            test: true,
        });
    }
    Ok(Source {
        location: CANONICAL_SOURCE.into(),
        test: false,
    })
}

/// The `vMAJOR.MINOR.PATCH` tags in `git ls-remote --tags` output, each
/// with the commit it names: `<sha>\trefs/tags/<tag>`, plus
/// `<sha>\trefs/tags/<tag>^{}` naming the commit of an annotated tag.
/// Anything else -- a pre-release, another tag -- is not a release.
fn releases(ls_remote: &str) -> Vec<Release> {
    let mut found = BTreeMap::new();
    for line in ls_remote.lines() {
        let Some((sha, tag)) = line
            .split_once('\t')
            .and_then(|(sha, name)| Some((sha, name.strip_prefix("refs/tags/")?)))
        else {
            continue;
        };
        let (tag, peeled) = match tag.strip_suffix("^{}") {
            Some(tag) => (tag, true),
            None => (tag, false),
        };
        let Some(version) = tag
            .strip_prefix('v')
            .and_then(|version| version.parse::<Version>().ok())
        else {
            continue;
        };
        let release = found.entry(version).or_insert_with(|| Release {
            version,
            tag: tag.to_owned(),
            commit: sha.to_owned(),
        });
        if peeled {
            sha.clone_into(&mut release.commit);
        }
    }
    found.into_values().collect()
}

fn git() -> Command {
    let mut command = Command::new("git");
    // Never wait for a credential prompt: the canonical source is public.
    command.env("GIT_TERMINAL_PROMPT", "0");
    command
}

/// `command`'s stdout; a missing tool or a failure is an error naming it.
fn run_tool(command: &mut Command, what: &str) -> Fallible<String> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| missing_tool(command, &error))?;
    if !output.status.success() {
        return Err(format!(
            "{what} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn missing_tool(command: &Command, error: &std::io::Error) -> String {
    let tool = command.get_program().to_string_lossy();
    if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "`{tool}` was not found: `brainprint update` builds the release from source and \
             needs git and a Rust toolchain (https://rustup.rs) on PATH"
        )
    } else {
        format!("cannot run `{tool}`: {error}")
    }
}

/// Fetch exactly `release`'s tag into `work/source` and check it out,
/// checking that both the tag and the checkout are the commit the release
/// list named.
fn fetch(source: &Source, release: &Release, work: &Path) -> Fallible<PathBuf> {
    let tree = work.join("source");
    run_tool(git().arg("init").arg("-q").arg(&tree), "git init")?;
    run_tool(
        git()
            .arg("-C")
            .arg(&tree)
            .args(["fetch", "-q", "--no-tags", "--depth", "1"])
            .arg(&source.location)
            .arg(format!("+refs/tags/{0}:refs/tags/{0}", release.tag)),
        &format!("fetching {}", release.tag),
    )?;
    let fetched = revision(&tree, &format!("refs/tags/{}^{{commit}}", release.tag))?;
    if fetched != release.commit {
        return Err(format!(
            "{} was fetched as commit {fetched}, not the {} the release list named: not built",
            release.tag, release.commit
        ));
    }
    run_tool(
        git()
            .arg("-C")
            .arg(&tree)
            .args([
                "-c",
                "advice.detachedHead=false",
                "checkout",
                "-q",
                "--detach",
            ])
            .arg(&release.commit),
        &format!("checking out {}", release.tag),
    )?;
    let head = revision(&tree, "HEAD")?;
    if head != release.commit {
        return Err(format!(
            "the checkout of {} is at {head}, not {}: not built",
            release.tag, release.commit
        ));
    }
    Ok(tree)
}

fn revision(tree: &Path, name: &str) -> Fallible<String> {
    Ok(run_tool(
        git()
            .arg("-C")
            .arg(tree)
            .args(["rev-parse", "--verify", "-q", name]),
        &format!("resolving {name}"),
    )?
    .trim()
    .to_owned())
}

/// `tree` is a complete Brainprint source tree of `version`: the
/// workspace's repository and version, its lockfile (the build is
/// `--locked`) and a member declaring each of the four binaries.
fn verify_source(tree: &Path, version: Version) -> Fallible<()> {
    let manifest = read_manifest(&tree.join("Cargo.toml"))?;
    let workspace = manifest.get("workspace");
    let package = workspace.and_then(|workspace| workspace.get("package"));
    let field = |name: &str| package.and_then(|package| package.get(name)?.as_str());
    if field("repository") != Some(REPOSITORY) {
        return Err(format!(
            "the fetched tree is not Brainprint source (workspace repository {:?}): not built",
            field("repository").unwrap_or("none")
        ));
    }
    if field("version") != Some(version.to_string().as_str()) {
        return Err(format!(
            "the fetched tree declares version {}, not {version}: not built",
            field("version").unwrap_or("none")
        ));
    }
    if !tree.join("Cargo.lock").is_file() {
        return Err("the fetched tree has no Cargo.lock: incomplete source, not built".to_owned());
    }
    let mut binaries = BTreeSet::new();
    let members = workspace
        .and_then(|workspace| workspace.get("members")?.as_array())
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str);
    for member in members {
        let member = read_manifest(&tree.join(member).join("Cargo.toml"))?;
        binaries.extend(
            member
                .get("bin")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|bin| bin.get("name")?.as_str().map(str::to_owned)),
        );
    }
    match BINARIES.iter().find(|name| !binaries.contains(**name)) {
        Some(name) => Err(format!(
            "the fetched tree builds no `{name}`: incomplete source, not built"
        )),
        None => Ok(()),
    }
}

fn read_manifest(path: &Path) -> Fallible<toml::Table> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "{} is unreadable ({error}): incomplete source, not built",
            path.display()
        )
    })?;
    toml::from_str(&text).map_err(|error| format!("{} is not valid TOML: {error}", path.display()))
}

/// The release build, with the tree's own toolchain file and lockfile.
fn build(tree: &Path, target: &Path) -> Fallible<()> {
    let mut command = Command::new("cargo");
    command
        .args(["build", "--release", "--locked", "--workspace", "--bins"])
        .arg("--target-dir")
        .arg(target)
        .current_dir(tree)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let status = command
        .status()
        .map_err(|error| missing_tool(&command, &error))?;
    if !status.success() {
        return Err(format!(
            "the release build failed ({status}): nothing was installed"
        ));
    }
    Ok(())
}

fn exe(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}{}", env::consts::EXE_SUFFIX))
}

/// Each binary in `dir` reports `version`: `--version` for three, the MCP
/// `initialize` answer's `serverInfo.version` for `brainprint-mcp` (it has
/// no `--version`). They run in `home`, an empty home of their own with
/// auto-start off, so nothing touches this user's daemon or data.
fn check_set(dir: &Path, version: Version, home: &Path) -> Fallible<()> {
    for name in BINARIES {
        let path = exe(dir, name);
        if !path.is_file() {
            return Err(format!("{} is missing", path.display()));
        }
        let mut command = Command::new(&path);
        command
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env_remove("XDG_RUNTIME_DIR")
            .env(NO_AUTOSTART_ENV, "1")
            .current_dir(home);
        let reported = if name == "brainprint-mcp" {
            let reply = first_line(&mut command, Some(MCP_INITIALIZE), |line| {
                line.contains(r#""id":1"#)
            })?;
            serde_json::from_str::<serde_json::Value>(&reply)
                .ok()
                .and_then(|reply| {
                    Some(
                        reply["result"]["serverInfo"]["version"]
                            .as_str()?
                            .to_owned(),
                    )
                })
        } else {
            let prefix = format!("{name} ");
            let line = first_line(command.arg("--version"), None, |_| true)?;
            line.strip_prefix(&prefix).map(str::to_owned)
        };
        if reported.as_deref() != Some(version.to_string().as_str()) {
            return Err(format!(
                "{} reports version {}, not {version}",
                path.display(),
                reported.as_deref().unwrap_or("(none)")
            ));
        }
    }
    Ok(())
}

/// The first stdout line of `command` that is `wanted`, after writing
/// `input` to its stdin; the process is ended once that line arrives, and
/// waited for at most [`PROBE_TIMEOUT`].
fn first_line(
    command: &mut Command,
    input: Option<&str>,
    wanted: impl Fn(&str) -> bool,
) -> Fallible<String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    // Held open until the answer arrived: an MCP server ends at EOF.
    let mut stdin = child.stdin.take();
    if let (Some(stdin), Some(input)) = (stdin.as_mut(), input) {
        let _ = writeln!(stdin, "{input}");
    }
    let (sender, lines) = mpsc::channel();
    let stdout = child.stdout.take().expect("piped stdout");
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let found = loop {
        match lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) if wanted(&line) => break Some(line),
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    found.ok_or_else(|| format!("{program} gave no answer"))
}

fn wait_within(mut child: Child, timeout: Duration) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// The installation `brainprint update` replaces: the real directory of
/// the running `brainprint` (a symlink to it resolved), holding all four
/// binaries as regular files, writable by this user. Never one found on
/// `PATH`.
struct Install {
    dir: PathBuf,
}

impl Install {
    fn locate() -> Fallible<Self> {
        let exe_path = env::current_exe()
            .and_then(fs::canonicalize)
            .map_err(|error| format!("cannot locate this brainprint executable: {error}"))?;
        let dir = exe_path
            .parent()
            .ok_or("the brainprint executable has no directory")?
            .to_path_buf();
        let build_tree = dir.ancestors().any(|ancestor| {
            ancestor.file_name() == Some("target".as_ref())
                && ancestor
                    .parent()
                    .is_some_and(|parent| parent.join("Cargo.toml").is_file())
        });
        if build_tree {
            return Err(format!(
                "this brainprint runs from a cargo build directory ({}): `brainprint update` \
                 replaces an installed binary set, never a build tree -- update the source \
                 checkout with git, or run the installed brainprint",
                dir.display()
            ));
        }
        for name in BINARIES {
            let path = exe(&dir, name);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "{} is a symbolic link: update the installation it points to",
                        path.display()
                    ));
                }
                Ok(metadata) if metadata.is_file() => {}
                _ => {
                    return Err(format!(
                        "{} is missing: this installation is incomplete -- {} are installed \
                         together; install them from source once, then update",
                        path.display(),
                        BINARIES.join(", ")
                    ));
                }
            }
        }
        let probe = dir.join(format!(".brainprint-update-probe-{}", process::id()));
        fs::File::create_new(&probe).map_err(|error| {
            format!(
                "cannot write to {} ({error}): run `brainprint update` as a user who can, or \
                 install Brainprint into a directory you own",
                dir.display()
            )
        })?;
        let _ = fs::remove_file(&probe);
        Ok(Self { dir })
    }

    /// Remove what an earlier update left behind: an original that was
    /// still running then (Windows), staged copies of an interrupted one.
    fn sweep(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let leftover = BINARIES.iter().any(|binary| {
                name.starts_with(&format!("{binary}{}.update-", env::consts::EXE_SUFFIX))
            });
            if leftover {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// An activated binary set and what it replaced.
///
/// Activation copies the four staged binaries into the installation
/// directory under temporary names, then for each binary renames the
/// installed one aside and the staged copy into its place. Only renames
/// within one directory touch the installed names: a running binary
/// cannot be overwritten or deleted on Windows, but it can be renamed --
/// so the running `brainprint` itself, or a client's `brainprint-mcp`, is
/// replaced the same way on every OS, with no helper process.
struct Swap {
    dir: PathBuf,
    /// Renamed aside, in order.
    aside: Vec<&'static str>,
}

impl Swap {
    fn staged_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!(
            "{name}{}.update-new-{}",
            env::consts::EXE_SUFFIX,
            process::id()
        ))
    }

    fn original_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!(
            "{name}{}.update-old-{}",
            env::consts::EXE_SUFFIX,
            process::id()
        ))
    }

    /// Every original back under its name (replacing a new binary already
    /// there), the staged copies removed.
    fn restore(&self) -> Fallible<()> {
        let mut failed = Vec::new();
        for name in self.aside.iter().rev() {
            if let Err(error) = fs::rename(self.original_path(name), exe(&self.dir, name)) {
                failed.push(format!("{}: {error}", exe(&self.dir, name).display()));
            }
        }
        for name in BINARIES {
            let _ = fs::remove_file(self.staged_path(name));
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }

    /// The update stands: the originals go. One still running (Windows)
    /// cannot be deleted yet; the next update removes it.
    fn finish(self) {
        for name in &self.aside {
            let _ = fs::remove_file(self.original_path(name));
        }
    }
}

fn activate(dir: &Path, staged: &Path) -> Fallible<Swap> {
    let mut swap = Swap {
        dir: dir.to_path_buf(),
        aside: Vec::new(),
    };
    let fail_after = cfg!(debug_assertions)
        .then(|| {
            env::var(TEST_FAIL_ACTIVATION_ENV)
                .ok()?
                .parse::<usize>()
                .ok()
        })
        .flatten();
    let mut result = Ok(());
    for name in BINARIES {
        if let Err(error) = fs::copy(exe(staged, name), swap.staged_path(name)) {
            result = Err(format!("copying {name} into {}: {error}", dir.display()));
            break;
        }
    }
    if result.is_ok() {
        for (index, name) in BINARIES.into_iter().enumerate() {
            if fail_after == Some(index) {
                result = Err(format!("{TEST_FAIL_ACTIVATION_ENV}: injected failure"));
                break;
            }
            if let Err(error) = fs::rename(exe(dir, name), swap.original_path(name)) {
                result = Err(format!("moving the installed {name} aside: {error}"));
                break;
            }
            swap.aside.push(name);
            if let Err(error) = fs::rename(swap.staged_path(name), exe(dir, name)) {
                result = Err(format!("moving the new {name} into place: {error}"));
                break;
            }
        }
    }
    match result {
        Ok(()) => Ok(swap),
        Err(error) => Err(match swap.restore() {
            Ok(()) => format!("activation failed ({error}): the installation is unchanged"),
            Err(failed) => format!(
                "activation failed ({error}) and putting the originals back failed ({failed}): \
                 {} may hold a mixed set -- reinstall from source",
                dir.display()
            ),
        }),
    }
}

/// The daemon of the activated set: the new `brainprint` restarts it with
/// its own lifecycle and protocol (#89), so a release that changes the
/// protocol is still started and checked by binaries that speak it.
/// Returns the protocol the restarted daemon speaks.
fn restart_daemon(dir: &Path, version: Version) -> Fallible<u64> {
    let brainprint = exe(dir, "brainprint");
    let restart = Command::new(&brainprint)
        .args(["daemon", "restart"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot run the new brainprint: {error}"))?;
    match wait_within(restart, RESTART_TIMEOUT) {
        Ok(Some(status)) if status.success() => {}
        Ok(Some(status)) => {
            return Err(format!(
                "the new `brainprint daemon restart` failed ({status})"
            ));
        }
        Ok(None) => {
            return Err(format!(
                "the new `brainprint daemon restart` did not finish within {}s",
                RESTART_TIMEOUT.as_secs()
            ));
        }
        Err(error) => return Err(format!("waiting for the daemon restart: {error}")),
    }
    let status = first_line(
        Command::new(&brainprint).args(["status", "--json"]),
        None,
        |line| line.starts_with('{'),
    )?;
    let status: serde_json::Value = serde_json::from_str(&status)
        .map_err(|error| format!("unreadable daemon status: {error}"))?;
    let daemon_version = status["daemon_version"].as_str().unwrap_or("(none)");
    if daemon_version != version.to_string() {
        return Err(format!(
            "the restarted daemon reports {daemon_version}, not {version}"
        ));
    }
    status["protocol_version"]
        .as_u64()
        .ok_or_else(|| "the restarted daemon reports no protocol".to_owned())
}

/// After a failed activation: start the daemon of the (restored)
/// installation again if one ran before. Describes the outcome as a
/// suffix of the error.
async fn restart_previous(endpoint: &EndpointPaths, dir: &Path, was_running: bool) -> String {
    if !was_running {
        return String::new();
    }
    let launch = Launch {
        daemon_exe: exe(dir, "brainprintd"),
        log_path: lifecycle::log_path(endpoint),
    };
    match lifecycle::start(endpoint, &launch).await {
        Ok(_) => "; the previous daemon was started again".to_owned(),
        Err(error) => format!("; starting the previous daemon again failed: {error}"),
    }
}

/// The temporary source checkout, build and validation home of one
/// update, removed when it ends -- success or not.
struct WorkDir(PathBuf);

impl WorkDir {
    fn create() -> Fallible<Self> {
        let path = env::temp_dir().join(format!("brainprint-update-{}", process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("home"))
            .map_err(|error| format!("cannot create {}: {error}", path.display()))?;
        Ok(Self(path))
    }

    fn home(&self) -> PathBuf {
        self.0.join("home")
    }

    fn target(&self) -> PathBuf {
        self.0.join("target")
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_stable_three_part_versions_are_releases() {
        assert_eq!("0.1.2".parse(), Ok(Version(0, 1, 2)));
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        for invalid in [
            "",
            "0.1",
            "0.1.2.3",
            "v0.1.2",
            "0.1.2-rc.1",
            "0.1.x",
            "0..1",
        ] {
            assert!(invalid.parse::<Version>().is_err(), "{invalid}");
        }
    }

    #[test]
    fn releases_are_the_version_tags_with_their_commits() {
        let listing = "\
1111111111111111111111111111111111111111\trefs/tags/v0.1.0
2222222222222222222222222222222222222222\trefs/tags/v0.1.1
3333333333333333333333333333333333333333\trefs/tags/v0.1.1^{}
4444444444444444444444444444444444444444\trefs/tags/v0.2.0-rc.1
5555555555555555555555555555555555555555\trefs/tags/nightly
6666666666666666666666666666666666666666\trefs/heads/master
";
        let releases = releases(listing);
        assert_eq!(
            releases
                .iter()
                .map(|release| (release.tag.as_str(), &release.commit[..1]))
                .collect::<Vec<_>>(),
            [("v0.1.0", "1"), ("v0.1.1", "3")],
            "an annotated tag names its commit, not the tag object"
        );
    }

    /// A Brainprint-shaped tree in a fresh directory.
    fn tree(label: &str, version: &str, repository: &str, binaries: &[&str]) -> PathBuf {
        let root = env::temp_dir().join(format!("bp-update-tree-{label}-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("crates/all")).expect("dir");
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[workspace]\nmembers = [\"crates/all\"]\n\n[workspace.package]\n\
                 version = \"{version}\"\nrepository = \"{repository}\"\n"
            ),
        )
        .expect("manifest");
        let bins: String = binaries
            .iter()
            .map(|name| format!("[[bin]]\nname = \"{name}\"\npath = \"main.rs\"\n"))
            .collect();
        fs::write(
            root.join("crates/all/Cargo.toml"),
            format!("[package]\nname = \"all\"\n{bins}"),
        )
        .expect("member");
        fs::write(root.join("Cargo.lock"), "version = 4\n").expect("lock");
        root
    }

    #[test]
    fn only_a_complete_brainprint_tree_of_the_target_version_is_built() {
        let target = Version(0, 1, 2);
        let good = tree("good", "0.1.2", REPOSITORY, &BINARIES);
        assert_eq!(verify_source(&good, target), Ok(()));

        let version = tree("version", "0.1.3", REPOSITORY, &BINARIES);
        assert!(
            verify_source(&version, target)
                .unwrap_err()
                .contains("0.1.3")
        );

        let other = tree("other", "0.1.2", "https://example.com/x", &BINARIES);
        assert!(
            verify_source(&other, target)
                .unwrap_err()
                .contains("not Brainprint")
        );

        let missing = tree("missing", "0.1.2", REPOSITORY, &BINARIES[..3]);
        assert!(
            verify_source(&missing, target)
                .unwrap_err()
                .contains("brainprint-agent")
        );

        let no_lock = tree("nolock", "0.1.2", REPOSITORY, &BINARIES);
        fs::remove_file(no_lock.join("Cargo.lock")).expect("rm");
        assert!(
            verify_source(&no_lock, target)
                .unwrap_err()
                .contains("Cargo.lock")
        );

        let no_member = tree("nomember", "0.1.2", REPOSITORY, &BINARIES);
        fs::remove_dir_all(no_member.join("crates")).expect("rm");
        assert!(
            verify_source(&no_member, target)
                .unwrap_err()
                .contains("incomplete")
        );

        for root in [good, version, other, missing, no_lock, no_member] {
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn a_fetch_is_checked_against_the_commit_the_release_list_named() {
        let root = tree("fetch", "0.1.2", REPOSITORY, &BINARIES);
        let git_in = |args: &[&str]| {
            run_tool(
                git()
                    .arg("-C")
                    .arg(&root)
                    .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                    .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
                    .args(args),
                "git",
            )
            .expect("git")
        };
        git_in(&["init", "-q"]);
        git_in(&["add", "-A"]);
        git_in(&["commit", "-q", "-m", "release"]);
        git_in(&["tag", "-a", "-m", "v0.1.2", "v0.1.2"]);
        let source = Source {
            location: root.clone().into_os_string(),
            test: true,
        };
        let listed = releases(
            &run_tool(git().args(["ls-remote", "--tags"]).arg(&root), "ls-remote")
                .expect("ls-remote"),
        );
        let [release] = &listed[..] else {
            panic!("{listed:?}")
        };

        let work = WorkDir::create().expect("work");
        let fetched = fetch(&source, release, &work.0).expect("fetch");
        assert_eq!(verify_source(&fetched, Version(0, 1, 2)), Ok(()));

        let work = {
            drop(work);
            WorkDir::create().expect("work")
        };
        let wrong = Release {
            commit: "0".repeat(40),
            ..release.clone()
        };
        let error = fetch(&source, &wrong, &work.0).unwrap_err();
        assert!(error.contains("not the 0000"), "{error}");

        let missing = Release {
            tag: "v9.9.9".to_owned(),
            ..release.clone()
        };
        drop(work);
        let work = WorkDir::create().expect("work");
        assert!(
            fetch(&source, &missing, &work.0)
                .unwrap_err()
                .contains("fetching v9.9.9")
        );
        drop(work);
        let _ = fs::remove_dir_all(root);
    }
}
