//! Brainprint's own Git observation (#51, I6 task 2): read a Workspace's
//! HEAD and work-tree state and turn it into the #50 input
//! (`head` + [`GitObservation`]). Storage, fingerprint and path
//! resolution stay the unchanged #50 path.
//!
//! One observation is exactly three bounded `git` calls, never more:
//! `rev-parse --show-toplevel` (Git's work tree must be the Workspace
//! root), then the same `status --porcelain=v2 -z` twice. The two status
//! reads must agree. Agreement means two consecutive reads returned the
//! same entries; it is **not** an atomic snapshot of the work tree.
//!
//! The fingerprint is #50's entry-level one: further content edits to an
//! already-dirty path do not change it.

use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::Path,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::git_observation::{
    self, GIT_ENTRY_BOUND, GitEntry, GitEntryStatus, GitObservation, GitObservationError,
};

/// Wall time per `git` call.
pub const GIT_CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// stdout bytes read per `git` call.
pub const GIT_OUTPUT_CAP: usize = 4 * 1024 * 1024;
/// stderr bytes kept per call (daemon log only, never on the wire).
const STDERR_CAP: usize = 1024;

const TOPLEVEL_ARGS: &[&str] = &["rev-parse", "--show-toplevel"];
const STATUS_ARGS: &[&str] = &[
    "status",
    "--porcelain=v2",
    "-z",
    "--branch",
    "--no-ahead-behind",
    "--untracked-files=normal",
    "--ignore-submodules=none",
    "--find-renames",
    "--",
    ".",
    ":(exclude,top).brainprint",
];

/// HEAD plus the work-tree observation. `head` is `None` only for an
/// unborn branch (no commit yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatus {
    pub head: Option<String>,
    /// `Clean` or `Dirty` with canonical entries; never `Unknown`.
    pub observation: GitObservation,
}

/// Why no observation was produced. Nothing is ever recorded instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitStatusError {
    NotAGitWorkspace,
    /// Git's work tree is not the Workspace root.
    WorkspaceBoundaryMismatch,
    /// `git` could not be started (detail for the daemon log).
    GitUnavailable(String),
    /// Non-zero exit. `stderr` is truncated and never sent on the wire.
    GitFailed {
        exit_code: Option<i32>,
        stderr: String,
    },
    Timeout,
    OutputTooLarge,
    TooManyEntries,
    UnrepresentablePath,
    Unparsable(&'static str),
    /// The two status reads differed. Retryable.
    ChangedDuringObservation,
}

impl std::fmt::Display for GitStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitUnavailable(detail) => write!(formatter, "git could not start: {detail}"),
            Self::GitFailed { exit_code, stderr } => {
                write!(formatter, "git exited with {exit_code:?}: {stderr}")
            }
            Self::Unparsable(what) => write!(formatter, "unparsable git status: {what}"),
            other => write!(formatter, "{other:?}"),
        }
    }
}

impl std::error::Error for GitStatusError {}

/// Which of the two commands a call runs (the test seam's argument).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCall {
    TopLevel,
    Status,
}

impl GitCall {
    const fn args(self) -> &'static [&'static str] {
        match self {
            Self::TopLevel => TOPLEVEL_ARGS,
            Self::Status => STATUS_ARGS,
        }
    }
}

/// Observe `root` (the canonical Workspace root) with the real `git`.
pub fn observe(root: &Path) -> Result<GitStatus, GitStatusError> {
    observe_with(root, |call| {
        run_bounded(
            git_command(root, call.args(), std::env::vars_os()),
            GIT_CALL_TIMEOUT,
            GIT_OUTPUT_CAP,
        )
    })
}

/// The observation protocol over any runner: boundary, then two status
/// reads that must agree. A failing step stops before the next call.
pub fn observe_with(
    root: &Path,
    mut run: impl FnMut(GitCall) -> Result<Vec<u8>, GitStatusError>,
) -> Result<GitStatus, GitStatusError> {
    if !root.join(".git").exists() {
        return Err(GitStatusError::NotAGitWorkspace);
    }
    let toplevel = run(GitCall::TopLevel)?;
    let toplevel = std::str::from_utf8(&toplevel)
        .map_err(|_| GitStatusError::UnrepresentablePath)?
        .trim_end_matches(['\n', '\r']);
    if toplevel.is_empty() {
        return Err(GitStatusError::Unparsable("empty toplevel"));
    }
    let same_root = match (fs::canonicalize(toplevel), fs::canonicalize(root)) {
        (Ok(toplevel), Ok(root)) => toplevel == root,
        _ => false,
    };
    if !same_root {
        return Err(GitStatusError::WorkspaceBoundaryMismatch);
    }
    let first = parse_porcelain_v2(&run(GitCall::Status)?)?;
    let second = parse_porcelain_v2(&run(GitCall::Status)?)?;
    if first == second {
        Ok(first)
    } else {
        Err(GitStatusError::ChangedDuringObservation)
    }
}

// ------------------------------------------------------------- parser

/// Parse `git status --porcelain=v2 -z --branch` output into HEAD and
/// canonical entries. Any record it cannot represent fails the whole
/// observation: dropping one would report a cleaner tree than observed.
pub fn parse_porcelain_v2(output: &[u8]) -> Result<GitStatus, GitStatusError> {
    let mut records = output.split(|byte| *byte == 0);
    let mut head: Option<Option<String>> = None;
    let mut entries = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(header) = record.strip_prefix(b"# ") {
            if let Some(oid) = header.strip_prefix(b"branch.oid ") {
                if head.is_some() {
                    return Err(GitStatusError::Unparsable("duplicate branch.oid"));
                }
                head = Some(parse_oid(oid)?);
            }
            continue;
        }
        let entry = match record.first() {
            Some(b'1') => ordinary(record)?,
            Some(b'2') => {
                let orig = records
                    .next()
                    .filter(|orig| !orig.is_empty())
                    .ok_or(GitStatusError::Unparsable("rename without original path"))?;
                renamed_or_copied(record, orig)?
            }
            Some(b'u') => {
                let fields = fields(record, 11)?;
                valid_xy(fields[1])?;
                entry(GitEntryStatus::Conflicted, fields[10], None)?
            }
            Some(b'?') => {
                let path = record
                    .strip_prefix(b"? ")
                    .ok_or(GitStatusError::Unparsable("untracked record"))?;
                entry(GitEntryStatus::Untracked, path, None)?
            }
            _ => return Err(GitStatusError::Unparsable("unknown record type")),
        };
        entries.push(entry);
        if entries.len() > GIT_ENTRY_BOUND {
            return Err(GitStatusError::TooManyEntries);
        }
    }
    let head = head.ok_or(GitStatusError::Unparsable("missing branch.oid"))?;
    let observation = if entries.is_empty() {
        GitObservation::Clean
    } else {
        let canonical =
            git_observation::canonical_entries(&entries).map_err(|error| match error {
                GitObservationError::TooManyEntries { .. } => GitStatusError::TooManyEntries,
                _ => GitStatusError::Unparsable("path outside the #50 path rules"),
            })?;
        GitObservation::Dirty(canonical)
    };
    Ok(GitStatus { head, observation })
}

/// `(initial)` is an unborn branch; otherwise lowercase SHA-1/SHA-256 hex.
fn parse_oid(oid: &[u8]) -> Result<Option<String>, GitStatusError> {
    if oid == b"(initial)" {
        return Ok(None);
    }
    let hex = (oid.len() == 40 || oid.len() == 64)
        && oid
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte));
    if !hex {
        return Err(GitStatusError::Unparsable("branch.oid is not a commit id"));
    }
    Ok(Some(String::from_utf8(oid.to_vec()).expect("ascii hex")))
}

/// `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`: A → added, D in
/// either column → deleted, otherwise M/T → modified.
fn ordinary(record: &[u8]) -> Result<GitEntry, GitStatusError> {
    let fields = fields(record, 9)?;
    let (x, y) = valid_xy(fields[1])?;
    let status = if x == b'A' {
        GitEntryStatus::Added
    } else if x == b'D' || y == b'D' {
        GitEntryStatus::Deleted
    } else if [x, y].iter().any(|column| matches!(column, b'M' | b'T')) {
        GitEntryStatus::Modified
    } else {
        return Err(GitStatusError::Unparsable(
            "ordinary record without a change",
        ));
    };
    entry(status, fields[8], None)
}

/// `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <R|C><score> <path>` + orig:
/// a rename keeps its origin, a copy is an added path.
fn renamed_or_copied(record: &[u8], orig: &[u8]) -> Result<GitEntry, GitStatusError> {
    let fields = fields(record, 10)?;
    valid_xy(fields[1])?;
    match fields[8].first() {
        Some(b'R') => entry(GitEntryStatus::Renamed, fields[9], Some(orig)),
        Some(b'C') => entry(GitEntryStatus::Added, fields[9], None),
        _ => Err(GitStatusError::Unparsable("rename/copy score")),
    }
}

/// Split on the first `count - 1` spaces; the last field (the path)
/// keeps any spaces it contains.
fn fields(record: &[u8], count: usize) -> Result<Vec<&[u8]>, GitStatusError> {
    let fields: Vec<&[u8]> = record.splitn(count, |byte| *byte == b' ').collect();
    if fields.len() == count && fields.iter().all(|field| !field.is_empty()) {
        Ok(fields)
    } else {
        Err(GitStatusError::Unparsable("truncated record"))
    }
}

fn valid_xy(xy: &[u8]) -> Result<(u8, u8), GitStatusError> {
    match xy {
        [x, y] if b".MTADRCU".contains(x) && b".MTADRCU".contains(y) => Ok((*x, *y)),
        _ => Err(GitStatusError::Unparsable("XY status")),
    }
}

fn entry(
    status: GitEntryStatus,
    path: &[u8],
    old_path: Option<&[u8]>,
) -> Result<GitEntry, GitStatusError> {
    let text = |bytes: &[u8]| {
        String::from_utf8(bytes.to_vec()).map_err(|_| GitStatusError::UnrepresentablePath)
    };
    Ok(GitEntry {
        path: text(path)?,
        old_path: old_path.map(text).transpose()?,
        status,
    })
}

// ------------------------------------------------------------- runner

/// `git --no-optional-locks -C <root> -c core.fsmonitor=false <args>`,
/// no shell. Every inherited `GIT_*` variable is dropped so the caller's
/// environment cannot point Git at another repository; discovery may not
/// climb above `root`.
fn git_command(
    root: &Path,
    args: &[&str],
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> Command {
    let root = plain_path(root);
    let mut command = Command::new("git");
    command
        .env_clear()
        .envs(inherited.into_iter().filter(|(key, _)| {
            !key.to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
        }));
    if let Some(parent) = root.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(&root)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Windows `canonicalize` yields `\\?\C:\...` / `\\?\UNC\...` verbatim
/// paths, which Git does not take as `-C`; hand Git the plain form.
fn plain_path(path: &Path) -> std::path::PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}").into();
    }
    text.strip_prefix(r"\\?\")
        .map_or_else(|| path.to_path_buf(), Into::into)
}

/// Run with a wall-time bound and a stdout cap. On timeout the process is
/// killed; past the cap reading stops and the process is killed.
fn run_bounded(
    mut command: Command,
    timeout: Duration,
    cap: usize,
) -> Result<Vec<u8>, GitStatusError> {
    let mut child = command
        .spawn()
        .map_err(|error| GitStatusError::GitUnavailable(error.to_string()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out = thread::spawn(move || read_capped(stdout, cap, true));
    let err = thread::spawn(move || read_capped(stderr, STDERR_CAP, false));
    let deadline = Instant::now() + timeout;
    // Past the cap the reader drops the pipe, so git fails its next write
    // and exits; only a hung process reaches the deadline.
    let status: Option<ExitStatus> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) | Err(_) => break None,
        }
    };
    if status.is_none() {
        // Kill before joining: the readers end only when the pipes close.
        let _ = child.kill();
        let _ = child.wait();
    }
    let (stdout, overflowed) = out.join().unwrap_or_default();
    let (stderr, _) = err.join().unwrap_or_default();
    if overflowed {
        return Err(GitStatusError::OutputTooLarge);
    }
    let Some(status) = status else {
        return Err(GitStatusError::Timeout);
    };
    if !status.success() {
        return Err(GitStatusError::GitFailed {
            exit_code: status.code(),
            stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
        });
    }
    Ok(stdout)
}

/// Read at most `cap` bytes. `stop` returns at the cap (dropping the pipe
/// so the writer fails); otherwise the rest is drained and discarded.
fn read_capped(mut reader: impl Read, cap: usize, stop: bool) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut overflowed = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut chunk) {
            Ok(0) | Err(_) => return (kept, overflowed),
            Ok(read) => read,
        };
        let room = cap - kept.len().min(cap);
        kept.extend_from_slice(&chunk[..read.min(room)]);
        if read > room {
            overflowed = true;
            if stop {
                return (kept, overflowed);
            }
        }
    }
}

#[cfg(test)]
mod tests;
