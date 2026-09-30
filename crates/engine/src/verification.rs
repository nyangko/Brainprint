//! Caller-named verification commands (#52 step 2). The only commands run
//! are the argv the caller wrote; nothing is read from the repository or
//! from stored knowledge to find, pick or add one. No shell: `argv[0]` is
//! the program and the rest are its arguments as given.
//!
//! [`prepare`] checks the whole batch (every command, label, argv, env,
//! timeout and cwd) before anything runs; only a [`PreparedVerification`]
//! can run. Commands run in order on the common runner, output counted,
//! never kept; the first one that does not pass skips the rest.
//! argv, env and cwd are never echoed into results, summary or errors.

use std::{
    collections::HashSet,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::{Duration, Instant},
};

use crate::{
    git_observation::normalized_path,
    git_status::plain_path,
    process_runner::{self, Cancel, Capture, RunEnd},
};

pub const MAX_COMMANDS: usize = 16;
pub const MAX_LABEL_CHARS: usize = 64;
pub const MAX_ARGV: usize = 256;
pub const MAX_ARG_BYTES: usize = 4 * 1024;
pub const MAX_TIMEOUT_SECS: u32 = 3600;
pub const MAX_TOTAL_TIMEOUT_SECS: u64 = 3600;
pub const MAX_ENV: usize = 32;
pub const MAX_ENV_VALUE_BYTES: usize = 4 * 1024;
/// The summary bound. The input bounds keep every summary under it.
pub const MAX_SUMMARY_BYTES: usize = 4 * 1024;

/// One command as the caller wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationCommand {
    pub label: String,
    pub argv: Vec<String>,
    /// Workspace-relative; `None` is the Workspace root.
    pub cwd: Option<String>,
    /// Overrides on top of the inherited environment.
    pub env: Vec<(String, String)>,
    pub timeout_secs: u32,
}

/// Why a batch was refused. Nothing ran. Names the command by position
/// and label only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidVerification(pub String);

impl fmt::Display for InvalidVerification {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid verification: {}", self.0)
    }
}

impl Error for InvalidVerification {}

/// The run was cancelled: the running tree was ended, later commands did
/// not run, and there is no result to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("verification cancelled")
    }
}

impl Error for Cancelled {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotStartedReason {
    NotFound,
    PermissionDenied,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationOutcome {
    Passed,
    Failed {
        exit_code: i32,
    },
    /// Ended by a signal Brainprint did not send (unix).
    Signaled {
        signal: i32,
    },
    TimedOut,
    NotStarted {
        reason: NotStartedReason,
    },
    /// An earlier command did not pass; this one never ran.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub label: String,
    pub outcome: VerificationOutcome,
    pub duration_ms: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    pub results: Vec<CommandResult>,
    /// The `verification_summary` text: labels, outcomes, codes, times.
    pub summary: String,
}

/// A batch that passed every check; its cwds are canonical and inside the
/// Workspace root.
#[derive(Debug)]
pub struct PreparedVerification {
    commands: Vec<PreparedCommand>,
}

#[derive(Debug)]
struct PreparedCommand {
    label: String,
    argv: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    timeout: Duration,
}

/// Check the whole batch against `workspace_root`. Any violation refuses
/// the batch before a single command can run.
pub fn prepare(
    workspace_root: &Path,
    commands: &[VerificationCommand],
) -> Result<PreparedVerification, InvalidVerification> {
    let invalid = |reason: String| Err(InvalidVerification(reason));
    if !(1..=MAX_COMMANDS).contains(&commands.len()) {
        return invalid(format!(
            "{} commands; 1..={MAX_COMMANDS} allowed",
            commands.len()
        ));
    }
    let root = fs::canonicalize(workspace_root)
        .map_err(|_| InvalidVerification("the Workspace root cannot be resolved".to_owned()))?;
    let mut labels = HashSet::new();
    let mut total_secs = 0_u64;
    let mut prepared = Vec::with_capacity(commands.len());
    for (index, command) in commands.iter().enumerate() {
        let at = |reason: &str| {
            InvalidVerification(format!(
                "command {} ({:?}): {reason}",
                index + 1,
                command.label
            ))
        };
        if !valid_label(&command.label) {
            // An invalid label is not echoed.
            return invalid(format!(
                "command {}: label must be 1..={MAX_LABEL_CHARS} of [A-Za-z0-9._-]",
                index + 1
            ));
        }
        if !labels.insert(command.label.as_str()) {
            return Err(at("duplicate label"));
        }
        check_argv(&command.argv).map_err(|reason| at(&reason))?;
        check_env(&command.env).map_err(|reason| at(&reason))?;
        if !(1..=MAX_TIMEOUT_SECS).contains(&command.timeout_secs) {
            return Err(at(&format!("timeout_secs must be 1..={MAX_TIMEOUT_SECS}")));
        }
        total_secs += u64::from(command.timeout_secs);
        let cwd = resolve_cwd(&root, command.cwd.as_deref()).map_err(at)?;
        prepared.push(PreparedCommand {
            label: command.label.clone(),
            argv: command.argv.clone(),
            cwd,
            env: command.env.clone(),
            timeout: Duration::from_secs(command.timeout_secs.into()),
        });
    }
    if total_secs > MAX_TOTAL_TIMEOUT_SECS {
        return invalid(format!(
            "total timeout {total_secs}s exceeds {MAX_TOTAL_TIMEOUT_SECS}s"
        ));
    }
    Ok(PreparedVerification { commands: prepared })
}

fn valid_label(label: &str) -> bool {
    (1..=MAX_LABEL_CHARS).contains(&label.len())
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn check_argv(argv: &[String]) -> Result<(), String> {
    if !(1..=MAX_ARGV).contains(&argv.len()) {
        return Err(format!("argv must have 1..={MAX_ARGV} elements"));
    }
    if argv[0].is_empty() {
        return Err("argv[0] is empty".to_owned());
    }
    if let Some(position) = argv.iter().position(|arg| arg.len() > MAX_ARG_BYTES) {
        return Err(format!("argv[{position}] exceeds {MAX_ARG_BYTES} bytes"));
    }
    if let Some(position) = argv.iter().position(|arg| arg.contains('\0')) {
        return Err(format!("argv[{position}] contains NUL"));
    }
    Ok(())
}

fn check_env(env: &[(String, String)]) -> Result<(), String> {
    if env.len() > MAX_ENV {
        return Err(format!("at most {MAX_ENV} env pairs"));
    }
    for (position, (key, value)) in env.iter().enumerate() {
        let mut bytes = key.bytes();
        let valid_key = bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if !valid_key {
            return Err(format!(
                "env[{position}] key must match [A-Za-z_][A-Za-z0-9_]*"
            ));
        }
        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(format!(
                "env[{position}] value exceeds {MAX_ENV_VALUE_BYTES} bytes"
            ));
        }
    }
    Ok(())
}

/// `root` joined with the #50-rule relative `cwd`, canonicalized, must be
/// a directory inside `root` (component-wise, so symlinks resolve first).
fn resolve_cwd(root: &Path, cwd: Option<&str>) -> Result<PathBuf, &'static str> {
    let Some(cwd) = cwd else {
        return Ok(root.to_path_buf());
    };
    let relative =
        normalized_path(cwd).map_err(|_| "cwd must be a Workspace-relative path without `..`")?;
    let resolved = fs::canonicalize(root.join(relative)).map_err(|_| "cwd does not exist")?;
    if !resolved.starts_with(root) {
        return Err("cwd resolves outside the Workspace");
    }
    if !resolved.is_dir() {
        return Err("cwd is not a directory");
    }
    Ok(resolved)
}

impl PreparedVerification {
    /// Run the commands in order. `Err(Cancelled)` when `cancel` fires:
    /// the running tree is ended and nothing later runs.
    pub fn run(&self, cancel: &Cancel) -> Result<VerificationReport, Cancelled> {
        let mut results = Vec::with_capacity(self.commands.len());
        let mut passing = true;
        for command in &self.commands {
            let result = if passing {
                command.run(cancel)?
            } else {
                CommandResult {
                    label: command.label.clone(),
                    outcome: VerificationOutcome::Skipped,
                    duration_ms: 0,
                    stdout_bytes: 0,
                    stderr_bytes: 0,
                }
            };
            passing = result.outcome == VerificationOutcome::Passed;
            results.push(result);
        }
        let summary = summary(&results);
        Ok(VerificationReport { results, summary })
    }
}

impl PreparedCommand {
    fn run(&self, cancel: &Cancel) -> Result<CommandResult, Cancelled> {
        if cancel.is_cancelled() {
            return Err(Cancelled);
        }
        let mut command = Command::new(&self.argv[0]);
        command
            .args(&self.argv[1..])
            .current_dir(plain_path(&self.cwd))
            .envs(self.env.iter().map(|(key, value)| (key, value)));
        let started = Instant::now();
        let run = process_runner::run(
            &mut command,
            self.timeout,
            cancel,
            Capture::Count,
            Capture::Count,
        );
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (outcome, stdout_bytes, stderr_bytes) = match run {
            Err(error) => (
                VerificationOutcome::NotStarted {
                    reason: not_started(&error),
                },
                0,
                0,
            ),
            Ok(output) => {
                let outcome = match output.end {
                    RunEnd::Exited(status) => exited(status),
                    RunEnd::TimedOut => VerificationOutcome::TimedOut,
                    RunEnd::Cancelled => return Err(Cancelled),
                    RunEnd::OutputLimit => unreachable!("Capture::Count has no limit"),
                };
                (outcome, output.stdout.bytes, output.stderr.bytes)
            }
        };
        Ok(CommandResult {
            label: self.label.clone(),
            outcome,
            duration_ms,
            stdout_bytes,
            stderr_bytes,
        })
    }
}

fn not_started(error: &io::Error) -> NotStartedReason {
    match error.kind() {
        io::ErrorKind::NotFound => NotStartedReason::NotFound,
        io::ErrorKind::PermissionDenied => NotStartedReason::PermissionDenied,
        _ => NotStartedReason::Other,
    }
}

fn exited(status: ExitStatus) -> VerificationOutcome {
    match status.code() {
        Some(0) => VerificationOutcome::Passed,
        Some(exit_code) => VerificationOutcome::Failed { exit_code },
        // Only unix exits without a code: a signal ended it.
        None => VerificationOutcome::Signaled {
            signal: signal(status),
        },
    }
}

#[cfg(unix)]
fn signal(status: ExitStatus) -> i32 {
    std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0)
}

#[cfg(not(unix))]
fn signal(_: ExitStatus) -> i32 {
    0
}

/// `test: passed 12340ms; clippy: failed exit=101 3120ms; build: skipped`.
/// No argv, env, cwd, output, byte counts or OS error text.
fn summary(results: &[CommandResult]) -> String {
    let parts: Vec<String> = results
        .iter()
        .map(|result| {
            let label = &result.label;
            let ms = result.duration_ms;
            match result.outcome {
                VerificationOutcome::Passed => format!("{label}: passed {ms}ms"),
                VerificationOutcome::Failed { exit_code } => {
                    format!("{label}: failed exit={exit_code} {ms}ms")
                }
                VerificationOutcome::Signaled { signal } => {
                    format!("{label}: signaled signal={signal} {ms}ms")
                }
                VerificationOutcome::TimedOut => format!("{label}: timed-out {ms}ms"),
                VerificationOutcome::NotStarted { reason } => {
                    let reason = match reason {
                        NotStartedReason::NotFound => "not-found",
                        NotStartedReason::PermissionDenied => "permission-denied",
                        NotStartedReason::Other => "other",
                    };
                    format!("{label}: not-started {reason} {ms}ms")
                }
                VerificationOutcome::Skipped => format!("{label}: skipped"),
            }
        })
        .collect();
    let summary = parts.join("; ");
    debug_assert!(summary.len() <= MAX_SUMMARY_BYTES);
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(label: &str, outcome: VerificationOutcome, duration_ms: u64) -> CommandResult {
        CommandResult {
            label: label.to_owned(),
            outcome,
            duration_ms,
            stdout_bytes: 123,
            stderr_bytes: 456,
        }
    }

    #[test]
    fn summary_format_is_fixed_and_carries_no_byte_counts() {
        let results = [
            result("test", VerificationOutcome::Passed, 12340),
            result(
                "clippy",
                VerificationOutcome::Failed { exit_code: 101 },
                3120,
            ),
            result("sig", VerificationOutcome::Signaled { signal: 15 }, 210),
            result("slow", VerificationOutcome::TimedOut, 5000),
            result(
                "gone",
                VerificationOutcome::NotStarted {
                    reason: NotStartedReason::NotFound,
                },
                2,
            ),
            result(
                "denied",
                VerificationOutcome::NotStarted {
                    reason: NotStartedReason::PermissionDenied,
                },
                1,
            ),
            result(
                "odd",
                VerificationOutcome::NotStarted {
                    reason: NotStartedReason::Other,
                },
                0,
            ),
            result("build", VerificationOutcome::Skipped, 0),
        ];
        assert_eq!(
            summary(&results),
            "test: passed 12340ms; clippy: failed exit=101 3120ms; \
             sig: signaled signal=15 210ms; slow: timed-out 5000ms; \
             gone: not-started not-found 2ms; denied: not-started permission-denied 1ms; \
             odd: not-started other 0ms; build: skipped"
        );
        assert_eq!(summary(&results), summary(&results));
    }

    #[test]
    fn the_largest_possible_summary_fits_4_kib() {
        let results: Vec<_> = (0..MAX_COMMANDS)
            .map(|index| {
                result(
                    &format!("{index:0>64}"),
                    VerificationOutcome::Failed {
                        exit_code: i32::MIN,
                    },
                    u64::MAX,
                )
            })
            .collect();
        let summary = summary(&results);
        assert!(summary.len() <= MAX_SUMMARY_BYTES, "{}", summary.len());
    }

    #[test]
    fn labels_and_env_keys_follow_their_grammar() {
        assert!(valid_label("a.B_9-z"));
        assert!(valid_label(&"x".repeat(64)));
        for bad in ["", "a b", "é", "a/b", "a;b", &"x".repeat(65)] {
            assert!(!valid_label(bad), "{bad:?}");
        }
        let env = |key: &str| check_env(&[(key.to_owned(), String::new())]);
        assert!(env("_A1").is_ok() && env("a").is_ok());
        for bad in ["", "1A", "A-B", "A B", "A=B"] {
            assert!(env(bad).is_err(), "{bad:?}");
        }
    }
}
