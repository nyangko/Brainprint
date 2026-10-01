//! #53: a bounded read of an ephemeral raw artifact a verification
//! command's `capture` produced. Local IPC only; no Workspace selector:
//! the handle is the daemon's own process-local identity.

use serde::{Deserialize, Serialize};

use super::work::OutputStreamWire;

/// The most raw bytes one read returns. Base64 of it (~683 KiB) plus the
/// envelope stays under the 1 MiB frame.
pub const MAX_ARTIFACT_READ_BYTES: u32 = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReadRequestWire {
    pub handle: String,
    pub stream: OutputStreamWire,
    pub part: ArtifactPartWire,
    /// Within the chosen part, not the original stream.
    pub offset: u32,
    /// `1..=MAX_ARTIFACT_READ_BYTES`.
    pub max_bytes: u32,
}

/// A truncated stream's head and tail are apart in the original stream;
/// a read never runs from one into the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactPartWire {
    Head,
    /// Empty when the stream was not truncated (the head holds it all).
    Tail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactReadResponseWire {
    Data {
        stream: OutputStreamWire,
        part: ArtifactPartWire,
        /// Where the chosen part starts in the original stream (0 for
        /// the head, `tail_start_offset` for the tail) -- the same for
        /// every chunk of the part; the chunk itself starts `offset`
        /// bytes into it.
        original_start_offset: u64,
        /// RFC 4648 standard base64 of the raw bytes.
        data_b64: String,
        /// Raw bytes, not base64 characters.
        returned_bytes: u32,
        /// The offset to continue from; `None` at the end of the part.
        next_offset: Option<u32>,
    },
    Failed {
        error: ArtifactReadErrorWire,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactReadErrorWire {
    /// Unknown, evicted, or from before a daemon restart: not told apart.
    ArtifactUnavailable,
    InvalidRequest {
        reason: String,
    },
    /// A daemon-side storage failure; detail stays out of the wire.
    Internal,
}
