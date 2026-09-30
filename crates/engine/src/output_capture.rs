//! Opt-in output capture (#53 step 1) on the common runner's sink seam:
//! per stream, an optional bounded raw head/tail and an optional
//! diagnostics parser, both fed chunk by chunk as the pipe is read.
//!
//! The process fact ([`RunOutput`]) and the capture fact
//! ([`CommandCapture`]) stay apart: a capture that could not finish never
//! changes how the command ended.

use std::{io, path::Path, process::Command, time::Duration};

use crate::{
    diagnostics::{self, DiagnosticFormat, DiagnosticParser, DiagnosticSummary, Stream},
    process_runner::{self, Cancel, OutputSink, RunOutput},
};

/// The first bytes of a stream kept.
pub const MAX_HEAD: usize = 4 * 1024 * 1024;
/// The last bytes of a stream kept.
pub const MAX_TAIL: usize = 4 * 1024 * 1024;
/// Per stream; a stream up to this size is kept whole.
pub const MAX_RETAINED: usize = MAX_HEAD + MAX_TAIL;

/// The first [`MAX_HEAD`] and last [`MAX_TAIL`] bytes of a stream, in one
/// buffer that never grows past [`MAX_RETAINED`]. Up to that size it is
/// the whole stream; past it the second half turns into a ring holding the
/// latest bytes, and the middle is dropped as it arrives.
#[derive(Debug, Default)]
pub struct HeadTail {
    buffer: Vec<u8>,
    observed: u64,
    /// Once wrapped: where the oldest tail byte sits in the ring.
    ring: Option<usize>,
}

impl HeadTail {
    pub fn push(&mut self, mut chunk: &[u8]) {
        self.observed += chunk.len() as u64;
        if self.ring.is_none() {
            let take = chunk.len().min(MAX_RETAINED - self.buffer.len());
            if self.buffer.len() + take > self.buffer.capacity() {
                // Doubling, but never past the bound.
                let wanted = (self.buffer.capacity() * 2)
                    .max(self.buffer.len() + take)
                    .min(MAX_RETAINED);
                self.buffer.reserve_exact(wanted - self.buffer.len());
            }
            self.buffer.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            if chunk.is_empty() {
                return;
            }
            self.ring = Some(0);
        }
        let ring = &mut self.buffer[MAX_HEAD..];
        if chunk.len() >= MAX_TAIL {
            ring.copy_from_slice(&chunk[chunk.len() - MAX_TAIL..]);
            self.ring = Some(0);
            return;
        }
        let at = self.ring.expect("wrapped");
        let first = chunk.len().min(MAX_TAIL - at);
        ring[at..at + first].copy_from_slice(&chunk[..first]);
        ring[..chunk.len() - first].copy_from_slice(&chunk[first..]);
        self.ring = Some((at + chunk.len()) % MAX_TAIL);
    }

    /// Bytes allocated; never more than [`MAX_RETAINED`].
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }

    pub fn finish(mut self) -> RetainedStream {
        let head_bytes = match self.ring {
            Some(at) => {
                self.buffer[MAX_HEAD..].rotate_left(at);
                MAX_HEAD
            }
            None => self.buffer.len(),
        };
        let retained = self.buffer.len() as u64;
        let tail_bytes = retained - head_bytes as u64;
        RetainedStream {
            observed_bytes: self.observed,
            head_bytes: head_bytes as u64,
            tail_bytes,
            omitted_bytes: self.observed - retained,
            truncated: self.ring.is_some(),
            tail_start_offset: self.observed - tail_bytes,
            data: self.buffer,
        }
    }
}

/// A stream's retained bytes. Not truncated: the whole stream is the head
/// and the tail is empty. Truncated: head and tail are apart in the
/// original stream by `omitted_bytes`; they are never contiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedStream {
    pub observed_bytes: u64,
    pub head_bytes: u64,
    pub tail_bytes: u64,
    pub omitted_bytes: u64,
    pub truncated: bool,
    /// Where the tail starts in the original stream.
    pub tail_start_offset: u64,
    data: Vec<u8>,
}

impl RetainedStream {
    pub fn head(&self) -> &[u8] {
        &self.data[..self.head_bytes as usize]
    }

    pub fn tail(&self) -> &[u8] {
        &self.data[self.head_bytes as usize..]
    }
}

/// What to capture from a command; the same for both streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRequest {
    pub raw: bool,
    pub diagnostics: Option<DiagnosticFormat>,
}

/// The sink of one stream.
#[derive(Debug)]
struct StreamCapture {
    raw: Option<HeadTail>,
    diagnostics: Option<DiagnosticParser>,
}

impl StreamCapture {
    fn new(request: CaptureRequest, stream: Stream) -> Self {
        Self {
            raw: request.raw.then(HeadTail::default),
            diagnostics: request
                .diagnostics
                .map(|format| DiagnosticParser::new(format, stream)),
        }
    }
}

impl OutputSink for StreamCapture {
    fn write(&mut self, chunk: &[u8]) {
        if let Some(raw) = &mut self.raw {
            raw.push(chunk);
        }
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.write(chunk);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureStatus {
    /// Both streams were read to EOF.
    Complete,
    /// A stream ended with the run (deadline, cancel, a descendant holding
    /// the pipe): its capture holds only what was read before.
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCapture {
    pub status: CaptureStatus,
    pub stdout: Option<RetainedStream>,
    pub stderr: Option<RetainedStream>,
    pub diagnostics: Option<DiagnosticSummary>,
}

/// The process fact and the capture fact of one run, kept apart.
#[derive(Debug)]
pub struct CapturedRun {
    pub output: RunOutput,
    pub capture: CommandCapture,
}

/// [`process_runner::run_observed`] with a [`StreamCapture`] per stream.
/// Diagnostic paths resolve against `base` (the command's cwd) and
/// `root` (the Workspace), both canonical.
pub fn run_captured(
    command: &mut Command,
    timeout: Duration,
    cancel: &Cancel,
    request: CaptureRequest,
    root: &Path,
    base: &Path,
) -> io::Result<CapturedRun> {
    let run = process_runner::run_observed(
        command,
        timeout,
        cancel,
        StreamCapture::new(request, Stream::Stdout),
        StreamCapture::new(request, Stream::Stderr),
    )?;
    let complete = (run.output.stdout.complete, run.output.stderr.complete);
    let (stdout, stdout_diagnostics) = finish(run.stdout, complete.0);
    let (stderr, stderr_diagnostics) = finish(run.stderr, complete.1);
    let diagnostics = request.diagnostics.map(|_| {
        diagnostics::summarize(
            stdout_diagnostics.into_iter().chain(stderr_diagnostics),
            root,
            base,
        )
    });
    Ok(CapturedRun {
        output: run.output,
        capture: CommandCapture {
            status: if complete.0 && complete.1 {
                CaptureStatus::Complete
            } else {
                CaptureStatus::Partial
            },
            stdout,
            stderr,
            diagnostics,
        },
    })
}

fn finish(
    capture: StreamCapture,
    complete: bool,
) -> (
    Option<RetainedStream>,
    Option<diagnostics::StreamDiagnostics>,
) {
    (
        capture.raw.map(HeadTail::finish),
        capture
            .diagnostics
            .map(|diagnostics| diagnostics.finish(complete)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    /// Byte `i` of a generated stream: never periodic at a power of two,
    /// so a wrong offset shows.
    fn pattern(i: u64) -> u8 {
        (i % 251) as u8
    }

    fn feed(total: u64) -> (RetainedStream, usize) {
        let mut collector = HeadTail::default();
        let mut chunk = vec![0_u8; 8 * 1024 + 13];
        let mut at = 0_u64;
        let mut max_capacity = 0;
        while at < total {
            let size = chunk.len().min((total - at) as usize);
            for (offset, byte) in chunk[..size].iter_mut().enumerate() {
                *byte = pattern(at + offset as u64);
            }
            collector.push(&chunk[..size]);
            max_capacity = max_capacity.max(collector.capacity());
            at += size as u64;
        }
        (collector.finish(), max_capacity)
    }

    fn assert_pattern(bytes: &[u8], start: u64) {
        assert!(
            bytes
                .iter()
                .enumerate()
                .all(|(offset, &byte)| byte == pattern(start + offset as u64)),
            "bytes from {start} differ"
        );
    }

    #[test]
    fn streams_up_to_8_mib_are_kept_whole() {
        for total in [0, 1, MIB - 7, 4 * MIB, 4 * MIB + 1, 6 * MIB + 3, 8 * MIB] {
            let (kept, capacity) = feed(total as u64);
            assert_eq!(kept.observed_bytes, total as u64, "{total}");
            assert!(!kept.truncated);
            assert_eq!((kept.head_bytes, kept.tail_bytes), (total as u64, 0));
            assert_eq!(kept.omitted_bytes, 0);
            assert_eq!(kept.tail_start_offset, total as u64);
            assert!(kept.tail().is_empty());
            assert_pattern(kept.head(), 0);
            assert!(capacity <= MAX_RETAINED, "{total}: {capacity}");
        }
    }

    #[test]
    fn past_8_mib_only_the_first_and_last_4_mib_remain() {
        for total in [8 * MIB + 1, 9 * MIB + 12_345, 64 * MIB + 7] {
            let total = total as u64;
            let (kept, capacity) = feed(total);
            assert_eq!(kept.observed_bytes, total);
            assert!(kept.truncated);
            assert_eq!(
                (kept.head_bytes, kept.tail_bytes),
                (MAX_HEAD as u64, MAX_TAIL as u64)
            );
            assert_eq!(kept.omitted_bytes, total - MAX_RETAINED as u64);
            assert_eq!(kept.tail_start_offset, total - MAX_TAIL as u64);
            assert_pattern(kept.head(), 0);
            assert_pattern(kept.tail(), kept.tail_start_offset);
            // The byte right after the head is not the first tail byte.
            assert_ne!(kept.tail()[0], pattern(MAX_HEAD as u64));
            assert_eq!(capacity, MAX_RETAINED);
        }
    }

    #[test]
    fn a_chunk_larger_than_the_tail_keeps_its_own_end() {
        let big: Vec<u8> = (0..(12 * MIB) as u64).map(pattern).collect();
        let mut collector = HeadTail::default();
        collector.push(&big[..10]);
        collector.push(&big[10..]);
        let kept = collector.finish();
        assert_eq!(kept.observed_bytes, big.len() as u64);
        assert_eq!(kept.head(), &big[..MAX_HEAD]);
        assert_eq!(kept.tail(), &big[big.len() - MAX_TAIL..]);
    }
}
