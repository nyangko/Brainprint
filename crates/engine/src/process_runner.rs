//! The common bounded process runner (#52 step 1). Every command runs in
//! its own process group (unix) or Job Object (Windows), so a timeout, a
//! cancellation or a parent that simply exits never leaves descendants
//! behind. stdout and stderr are always drained concurrently; besides a
//! byte count a stream keeps only what its [`Capture`] allows, or, with
//! [`run_observed`], hands each chunk to an [`OutputSink`] as it is read.
//!
//! Termination: unix sends SIGTERM to the group, waits at most
//! [`TERMINATE_GRACE`], then SIGKILLs the group. Windows terminates the
//! Job (created with `KILL_ON_JOB_CLOSE`, so dropping it ends the job too).
//! The same termination runs after a normal exit, for descendants still in
//! the group/job.

use std::{
    io::{self, Read},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use command_group::{CommandGroup, GroupChild};
#[cfg(unix)]
use command_group::{Signal, UnixChildExt};

/// How long a group has to exit on SIGTERM before it is killed.
pub const TERMINATE_GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(5);
/// The one read buffer per stream; output size never changes memory use.
const CHUNK: usize = 8 * 1024;

/// A cancellation flag shared with whoever may stop a run.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// What a stream keeps besides its byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Nothing: drain and count.
    Count,
    /// The first `n` bytes; the rest is drained and counted.
    Keep(usize),
    /// The first `n` bytes; one byte more ends the run with
    /// [`RunEnd::OutputLimit`].
    Limit(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamOutput {
    /// Bytes read, including those read after the kept prefix.
    pub bytes: u64,
    /// The kept prefix; empty unless the stream is complete.
    pub kept: Vec<u8>,
    /// EOF was reached before the run ended.
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    /// The command exited. A descendant holding a pipe open past the
    /// deadline leaves that stream incomplete.
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
    OutputLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub end: RunEnd,
    pub stdout: StreamOutput,
    pub stderr: StreamOutput,
}

/// Takes each chunk of a stream as it is read (#53). A sink never owns
/// the whole output and cannot fail the drain: whatever it cannot keep is
/// its own fact to report.
pub trait OutputSink: Send + 'static {
    fn write(&mut self, chunk: &[u8]);
}

/// The sink for [`run`]: never called.
struct NoSink;

impl OutputSink for NoSink {
    fn write(&mut self, _: &[u8]) {}
}

/// A [`run_observed`] result: the process fact and the sinks, kept apart.
/// A stream that did not complete (`output.<stream>.complete == false`)
/// left its sink with only what was read before the run ended.
#[derive(Debug)]
pub struct ObservedRun<S> {
    pub output: RunOutput,
    pub stdout: S,
    pub stderr: S,
}

/// [`run`] with both streams counted and every chunk also written to its
/// sink while it is read. Nothing else is kept.
pub fn run_observed<S: OutputSink>(
    command: &mut Command,
    timeout: Duration,
    cancel: &Cancel,
    stdout: S,
    stderr: S,
) -> io::Result<ObservedRun<S>> {
    let sinks = (
        Arc::new(Mutex::new(Some(stdout))),
        Arc::new(Mutex::new(Some(stderr))),
    );
    let output = run_with(
        command,
        timeout,
        cancel,
        (Capture::Count, Capture::Count),
        (Some(Arc::clone(&sinks.0)), Some(Arc::clone(&sinks.1))),
    )?;
    Ok(ObservedRun {
        output,
        stdout: take(&sinks.0),
        stderr: take(&sinks.1),
    })
}

/// Take the sink back; a reader still blocked on its pipe finds it gone
/// and only counts from then on.
fn take<S>(sink: &Mutex<Option<S>>) -> S {
    sink.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("each sink is taken once")
}

type Shared<S> = Option<Arc<Mutex<Option<S>>>>;

/// Run `command` (stdin null, stdout/stderr piped) until it exits and both
/// streams reach EOF, the deadline passes, `cancel` fires or a
/// [`Capture::Limit`] is exceeded; then terminate whatever is left of its
/// group/job. Only a failed spawn is an `Err`.
pub fn run(
    command: &mut Command,
    timeout: Duration,
    cancel: &Cancel,
    stdout: Capture,
    stderr: Capture,
) -> io::Result<RunOutput> {
    run_with::<NoSink>(command, timeout, cancel, (stdout, stderr), (None, None))
}

fn run_with<S: OutputSink>(
    command: &mut Command,
    timeout: Duration,
    cancel: &Cancel,
    (stdout, stderr): (Capture, Capture),
    (stdout_sink, stderr_sink): (Shared<S>, Shared<S>),
) -> io::Result<RunOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_group(command)?;
    let deadline = Instant::now() + timeout;
    let limited = Arc::new(AtomicBool::new(false));
    let inner = child.inner();
    let mut out = Reader::start(
        inner.stdout.take().expect("piped stdout"),
        stdout,
        stdout_sink,
        &limited,
    );
    let mut err = Reader::start(
        inner.stderr.take().expect("piped stderr"),
        stderr,
        stderr_sink,
        &limited,
    );
    let mut status = None;
    let end = loop {
        if limited.load(Ordering::SeqCst) {
            break RunEnd::OutputLimit;
        }
        if cancel.is_cancelled() {
            break RunEnd::Cancelled;
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(exited) => status = exited,
                // An unwaitable child is treated as hung (#51).
                Err(_) => break RunEnd::TimedOut,
            }
        }
        // Both streams are polled every round (no short circuit).
        let complete = out.poll() & err.poll();
        let expired = Instant::now() >= deadline;
        match status {
            Some(status) if complete || expired => break RunEnd::Exited(status),
            None if expired => break RunEnd::TimedOut,
            _ => thread::sleep(POLL),
        }
    };
    terminate(&mut child);
    Ok(RunOutput {
        end,
        stdout: out.finish(),
        stderr: err.finish(),
    })
}

#[cfg(unix)]
fn spawn_group(command: &mut Command) -> io::Result<GroupChild> {
    command.group_spawn()
}

#[cfg(windows)]
fn spawn_group(command: &mut Command) -> io::Result<GroupChild> {
    command.group().kill_on_drop(true).spawn()
}

/// SIGTERM the group; ESRCH (nothing left) ends it at once. Otherwise
/// probe with SIGCONT, which a running process ignores and which lets a
/// stopped one act on the SIGTERM, until it fails or the grace is over,
/// then SIGKILL the group.
#[cfg(unix)]
fn terminate(child: &mut GroupChild) {
    if child.signal(Signal::SIGTERM).is_ok() {
        let until = Instant::now() + TERMINATE_GRACE;
        loop {
            // Reap the leader: its zombie would keep the group alive.
            let _ = child.try_wait();
            if child.signal(Signal::SIGCONT).is_err() {
                break;
            }
            if Instant::now() >= until {
                let _ = child.kill();
                break;
            }
            thread::sleep(POLL);
        }
    }
    let _ = child.wait();
}

#[cfg(windows)]
fn terminate(child: &mut GroupChild) {
    let _ = child.kill();
}

/// One stream drained on its own thread. The count is shared so it is
/// readable even when EOF never comes.
// ponytail: a reader blocked on a pipe held by a process that left the
// group (setsid) stays detached; it ends when that pipe closes.
struct Reader {
    bytes: Arc<AtomicU64>,
    done: mpsc::Receiver<Vec<u8>>,
    kept: Option<Vec<u8>>,
}

impl Reader {
    fn start<S: OutputSink>(
        pipe: impl Read + Send + 'static,
        capture: Capture,
        sink: Shared<S>,
        limited: &Arc<AtomicBool>,
    ) -> Self {
        let bytes = Arc::new(AtomicU64::new(0));
        let (tx, done) = mpsc::channel();
        let (counter, limited) = (Arc::clone(&bytes), Arc::clone(limited));
        thread::spawn(move || {
            if let Some(kept) = drain(pipe, capture, sink.as_deref(), &counter, &limited) {
                let _ = tx.send(kept);
            }
        });
        Self {
            bytes,
            done,
            kept: None,
        }
    }

    fn poll(&mut self) -> bool {
        if self.kept.is_none() {
            self.kept = self.done.try_recv().ok();
        }
        self.kept.is_some()
    }

    fn finish(self) -> StreamOutput {
        StreamOutput {
            bytes: self.bytes.load(Ordering::SeqCst),
            complete: self.kept.is_some(),
            kept: self.kept.unwrap_or_default(),
        }
    }
}

/// Read to EOF (a read error counts as EOF). `None` when a `Limit` was
/// exceeded: the pipe is dropped so the writer fails.
fn drain<S: OutputSink>(
    mut pipe: impl Read,
    capture: Capture,
    sink: Option<&Mutex<Option<S>>>,
    bytes: &AtomicU64,
    limited: &AtomicBool,
) -> Option<Vec<u8>> {
    let (keep, limit) = match capture {
        Capture::Count => (0, false),
        Capture::Keep(keep) => (keep, false),
        Capture::Limit(keep) => (keep, true),
    };
    let mut kept = Vec::new();
    let mut chunk = [0_u8; CHUNK];
    loop {
        let read = match pipe.read(&mut chunk) {
            Ok(0) => return Some(kept),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Some(kept),
        };
        if let Some(sink) = sink
            && let Some(sink) = sink.lock().unwrap_or_else(PoisonError::into_inner).as_mut()
        {
            sink.write(&chunk[..read]);
        }
        bytes.fetch_add(read as u64, Ordering::SeqCst);
        let room = keep - kept.len();
        if limit && read > room {
            limited.store(true, Ordering::SeqCst);
            return None;
        }
        kept.extend_from_slice(&chunk[..read.min(room)]);
    }
}
