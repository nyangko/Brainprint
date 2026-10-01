//! #53 step 2: the daemon-wide ephemeral raw-output store.
//!
//! One per daemon, under `<runtime_root>/artifacts/verification`: never
//! in a Workspace, never in SQLite. One payload file per command artifact
//! (`stdout head | stdout tail | stderr head | stderr tail`), indexed only
//! in memory; a restart expires every handle. A handle is a random UUID
//! looked up in the index, never a path.
//!
//! [`ArtifactStore::new`] touches nothing on disk: the daemon calls
//! [`ArtifactStore::initialize_after_lock`] only once it owns the
//! singleton, so a second `brainprintd` that finds a live one never purges
//! that daemon's artifacts.

use std::{
    collections::{HashMap, VecDeque},
    fmt, fs,
    io::{self, Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use brainprint_engine::output_capture::{self, RetainedStream};

const MIB: u64 = 1024 * 1024;
/// Daemon-wide raw bytes, completed plus reserved in-flight.
pub const MAX_RETAINED_BYTES: u64 = 64 * MIB;
/// Daemon-wide artifacts, completed plus reserved in-flight.
pub const MAX_ARTIFACTS: usize = 64;
/// One command: stdout and stderr at their step 1 bound each.
pub const MAX_COMMAND_BYTES: u64 = 2 * output_capture::MAX_RETAINED as u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactLimits {
    pub max_bytes: u64,
    pub max_artifacts: usize,
}

impl ArtifactLimits {
    pub const PRODUCTION: Self = Self {
        max_bytes: MAX_RETAINED_BYTES,
        max_artifacts: MAX_ARTIFACTS,
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactStream {
    Stdout,
    Stderr,
}

/// The two retained parts of a stream; never contiguous when truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactPart {
    Head,
    Tail,
}

/// A stream's step 1 [`RetainedStream`] facts, without its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamMeta {
    pub observed_bytes: u64,
    pub head_bytes: u64,
    pub tail_bytes: u64,
    pub omitted_bytes: u64,
    pub truncated: bool,
    pub tail_start_offset: u64,
}

impl StreamMeta {
    fn of(stream: &RetainedStream) -> Self {
        Self {
            observed_bytes: stream.observed_bytes,
            head_bytes: stream.head_bytes,
            tail_bytes: stream.tail_bytes,
            omitted_bytes: stream.omitted_bytes,
            truncated: stream.truncated,
            tail_start_offset: stream.tail_start_offset,
        }
    }

    fn retained(&self) -> u64 {
        self.head_bytes + self.tail_bytes
    }
}

/// A stored command artifact: the opaque handle and both streams' facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    pub handle: String,
    pub stdout: StreamMeta,
    pub stderr: StreamMeta,
}

/// The filesystem step that failed; never a path or any output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactIoOp {
    Write,
    Commit,
    Read,
}

/// A store failure. Never a command outcome: step 3 reports it only as
/// capture status / raw handle availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactError {
    /// Unknown, evicted, or from before a restart -- not told apart.
    Unavailable,
    /// More than one command's bound (8 MiB per stream, 16 MiB per
    /// command, or more than was reserved).
    OverCommandLimit,
    /// Completed artifacts all evicted and the in-flight reservations
    /// still leave no room; never waited for.
    CapacityUnavailable,
    Io(ArtifactIoOp, io::ErrorKind),
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("artifact unavailable"),
            Self::OverCommandLimit => formatter.write_str("artifact exceeds the per-command bound"),
            Self::CapacityUnavailable => formatter.write_str("artifact store capacity unavailable"),
            Self::Io(op, kind) => write!(formatter, "artifact store {op:?} failed: {kind}"),
        }
    }
}

impl std::error::Error for ArtifactError {}

#[derive(Debug)]
struct StreamRecord {
    meta: StreamMeta,
    /// Where the head starts in the payload file; the tail follows it.
    offset: u64,
}

#[derive(Debug)]
struct Record {
    file: PathBuf,
    stdout: StreamRecord,
    stderr: StreamRecord,
    bytes: u64,
}

#[derive(Debug, Default)]
struct State {
    /// Bumped by every (re)initialize and shutdown: a reservation from an
    /// earlier one no longer counts and cannot publish.
    generation: u64,
    open: bool,
    completed: HashMap<String, Record>,
    /// Least recently used first.
    lru: VecDeque<String>,
    completed_bytes: u64,
    reserved_bytes: u64,
    reserved_count: usize,
    next_file: u64,
    #[cfg(test)]
    fail_write: bool,
    #[cfg(test)]
    file_opens: usize,
}

impl State {
    fn evict_oldest(&mut self) {
        let handle = self
            .lru
            .pop_front()
            .expect("room is short only while something completed remains");
        let record = self.completed.remove(&handle).expect("lru entry indexed");
        self.completed_bytes -= record.bytes;
        let _ = fs::remove_file(&record.file);
    }

    fn touch(&mut self, handle: &str) {
        if let Some(at) = self.lru.iter().position(|entry| entry == handle) {
            let entry = self.lru.remove(at).expect("position in range");
            self.lru.push_back(entry);
        }
    }
}

pub struct ArtifactStore {
    root: PathBuf,
    limits: ArtifactLimits,
    state: Mutex<State>,
}

// No root path: it names the user's runtime directory.
impl fmt::Debug for ArtifactStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactStore")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ArtifactStore {
    /// The store under `runtime_root`. Inert: nothing on disk changes
    /// until [`Self::initialize_after_lock`].
    #[must_use]
    pub fn new(runtime_root: &Path) -> Self {
        Self::with_limits(runtime_root, ArtifactLimits::PRODUCTION)
    }

    /// Test seam: small limits make LRU cases cheap. Not a setting.
    fn with_limits(runtime_root: &Path, limits: ArtifactLimits) -> Self {
        Self {
            root: runtime_root.join("artifacts").join("verification"),
            limits,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("artifact store mutex poisoned")
    }

    /// Call only once this daemon owns the singleton: purges whatever a
    /// previous (crashed) daemon left and starts empty.
    pub fn initialize_after_lock(&self) -> io::Result<()> {
        let mut state = self.state();
        let generation = state.generation + 1;
        *state = State {
            generation,
            ..State::default()
        };
        match fs::remove_dir_all(&self.root) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        state.open = true;
        Ok(())
    }

    /// Clean shutdown: every handle expires, the directory goes
    /// (best-effort).
    pub fn shutdown(&self) {
        let mut state = self.state();
        let generation = state.generation + 1;
        *state = State {
            generation,
            ..State::default()
        };
        let _ = fs::remove_dir_all(&self.root);
    }

    /// Hold room for one command artifact of up to `bytes`, evicting
    /// completed artifacts (least recently used first) as needed. Never
    /// waits and never touches another in-flight reservation.
    pub fn reserve(&self, bytes: u64) -> Result<Reservation<'_>, ArtifactError> {
        if bytes > MAX_COMMAND_BYTES {
            return Err(ArtifactError::OverCommandLimit);
        }
        let mut state = self.state();
        if !state.open {
            return Err(ArtifactError::Unavailable);
        }
        let limits = self.limits;
        let fits = |state: &State| {
            state.completed_bytes + state.reserved_bytes + bytes <= limits.max_bytes
                && state.completed.len() + state.reserved_count < limits.max_artifacts
        };
        // Fail before evicting anything the reservations alone rule out.
        if state.reserved_bytes + bytes > limits.max_bytes
            || state.reserved_count >= limits.max_artifacts
        {
            return Err(ArtifactError::CapacityUnavailable);
        }
        while !fits(&state) {
            state.evict_oldest();
        }
        state.reserved_bytes += bytes;
        state.reserved_count += 1;
        Ok(Reservation {
            store: self,
            generation: Some(state.generation),
            bytes,
        })
    }

    /// Reserve exactly this artifact's size and store it.
    pub fn insert(
        &self,
        stdout: &RetainedStream,
        stderr: &RetainedStream,
    ) -> Result<ArtifactRef, ArtifactError> {
        let bytes = StreamMeta::of(stdout).retained() + StreamMeta::of(stderr).retained();
        self.reserve(bytes)?.commit(stdout, stderr)
    }

    /// [`Self::read`]'s bytes alone.
    pub fn read_part(
        &self,
        handle: &str,
        stream: ArtifactStream,
        part: ArtifactPart,
        offset: u64,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ArtifactError> {
        self.read(handle, stream, part, offset, max_bytes)
            .map(|read| read.data)
    }

    /// Up to `max_bytes` of one retained part from `offset` within it; a
    /// head read never runs on into the tail. Empty past the part's end.
    /// A successful read makes the artifact most recently used.
    pub fn read(
        &self,
        handle: &str,
        stream: ArtifactStream,
        part: ArtifactPart,
        offset: u64,
        max_bytes: usize,
    ) -> Result<PartRead, ArtifactError> {
        // ponytail: reads hold the store lock; bounded by the caller's
        // max_bytes (step 3: 512 KiB). Per-record locks if reads contend.
        let mut state = self.state();
        let Some(record) = state.completed.get(handle) else {
            return Err(ArtifactError::Unavailable);
        };
        let stream = match stream {
            ArtifactStream::Stdout => &record.stdout,
            ArtifactStream::Stderr => &record.stderr,
        };
        let (start, len, original_start_offset) = match part {
            ArtifactPart::Head => (stream.offset, stream.meta.head_bytes, 0),
            ArtifactPart::Tail => (
                stream.offset + stream.meta.head_bytes,
                stream.meta.tail_bytes,
                stream.meta.tail_start_offset,
            ),
        };
        let take = len.saturating_sub(offset).min(max_bytes as u64);
        let mut data = vec![0_u8; take as usize];
        if take > 0 {
            let file = record.file.clone();
            #[cfg(test)]
            {
                state.file_opens += 1;
            }
            let read = || -> io::Result<()> {
                let mut file = fs::File::open(file)?;
                file.seek(SeekFrom::Start(start + offset))?;
                file.read_exact(&mut data)
            };
            read().map_err(|error| ArtifactError::Io(ArtifactIoOp::Read, error.kind()))?;
        }
        state.touch(handle);
        Ok(PartRead {
            data,
            part_bytes: len,
            original_start_offset,
        })
    }

    /// Whether `handle` is still stored. Not a read: recency unchanged.
    #[must_use]
    pub fn contains(&self, handle: &str) -> bool {
        self.state().completed.contains_key(handle)
    }

    /// Drop one artifact now (an artifact no response will name).
    pub fn remove(&self, handle: &str) {
        let mut state = self.state();
        if let Some(record) = state.completed.remove(handle) {
            state.lru.retain(|entry| entry != handle);
            state.completed_bytes -= record.bytes;
            let _ = fs::remove_file(&record.file);
        }
    }

    /// Completed artifacts and their raw bytes, plus in-flight
    /// reservations and their bytes.
    #[must_use]
    pub fn accounting(&self) -> Accounting {
        let state = self.state();
        Accounting {
            completed_count: state.completed.len(),
            completed_bytes: state.completed_bytes,
            reserved_count: state.reserved_count,
            reserved_bytes: state.reserved_bytes,
        }
    }
}

/// One [`ArtifactStore::read`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartRead {
    pub data: Vec<u8>,
    /// The whole part's size.
    pub part_bytes: u64,
    /// Where the part starts in the original stream.
    pub original_start_offset: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Accounting {
    pub completed_count: usize,
    pub completed_bytes: u64,
    pub reserved_count: usize,
    pub reserved_bytes: u64,
}

/// Room held for one in-flight artifact; released on drop unless
/// committed.
#[derive(Debug)]
pub struct Reservation<'a> {
    store: &'a ArtifactStore,
    /// `None` once committed: the room became a completed artifact.
    generation: Option<u64>,
    bytes: u64,
}

impl Reservation<'_> {
    /// Write the payload, then publish it; nothing half-written is ever
    /// indexed. On failure the reservation is released and the file
    /// removed (best-effort).
    pub fn commit(
        mut self,
        stdout: &RetainedStream,
        stderr: &RetainedStream,
    ) -> Result<ArtifactRef, ArtifactError> {
        let max_stream = output_capture::MAX_RETAINED as u64;
        let (out_meta, err_meta) = (StreamMeta::of(stdout), StreamMeta::of(stderr));
        let bytes = out_meta.retained() + err_meta.retained();
        if out_meta.retained() > max_stream
            || err_meta.retained() > max_stream
            || bytes > self.bytes
        {
            return Err(ArtifactError::OverCommandLimit);
        }

        let store = self.store;
        let (file, temp) = {
            let mut state = store.state();
            if !state.open || Some(state.generation) != self.generation {
                return Err(ArtifactError::Unavailable);
            }
            let name = state.next_file;
            state.next_file += 1;
            (
                store.root.join(format!("{name}.payload")),
                store.root.join(format!("{name}.tmp")),
            )
        };
        #[cfg(test)]
        let fail_write = std::mem::take(&mut store.state().fail_write);
        #[cfg(not(test))]
        let fail_write = false;

        let write = || -> io::Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut out = options.open(&temp)?;
            for part in [stdout.head(), stdout.tail(), stderr.head()] {
                out.write_all(part)?;
            }
            if fail_write {
                return Err(io::Error::other("injected write failure"));
            }
            out.write_all(stderr.tail())
        };
        if let Err(error) = write() {
            let _ = fs::remove_file(&temp);
            return Err(ArtifactError::Io(ArtifactIoOp::Write, error.kind()));
        }
        if let Err(error) = fs::rename(&temp, &file) {
            let _ = fs::remove_file(&temp);
            return Err(ArtifactError::Io(ArtifactIoOp::Commit, error.kind()));
        }

        let mut state = store.state();
        if !state.open || Some(state.generation) != self.generation {
            let _ = fs::remove_file(&file);
            return Err(ArtifactError::Unavailable);
        }
        let handle = uuid::Uuid::new_v4().to_string();
        state.completed.insert(
            handle.clone(),
            Record {
                file,
                stdout: StreamRecord {
                    meta: out_meta,
                    offset: 0,
                },
                stderr: StreamRecord {
                    meta: err_meta,
                    offset: out_meta.retained(),
                },
                bytes,
            },
        );
        state.lru.push_back(handle.clone());
        state.completed_bytes += bytes;
        state.reserved_bytes -= self.bytes;
        state.reserved_count -= 1;
        self.generation = None;
        Ok(ArtifactRef {
            handle,
            stdout: out_meta,
            stderr: err_meta,
        })
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut state = self.store.state();
        if Some(state.generation) == self.generation {
            state.reserved_bytes -= self.bytes;
            state.reserved_count -= 1;
        }
    }
}

#[cfg(test)]
mod tests;
