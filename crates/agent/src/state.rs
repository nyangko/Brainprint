//! Bounded, ephemeral, rebuildable adapter state (#26 "Ephemeral
//! state"). Not canonical truth: losing it only loses an optimization.
//!
//! Stored: session correlation, mode, bootstrap/reset markers, first
//! route, compact descriptors of Brainprint deliveries (paths, spans,
//! revisions, digests), timestamps, the one-shot bypass flag. Never
//! stored: transcripts, reasoning, source bodies, full MCP payloads,
//! secrets, or copies of Policy/Decision/Working State.

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

use crate::{
    event::{ActionClass, Mode, SearchPattern},
    util,
};

pub const MAX_RECORDS_PER_CLIENT: usize = 256;
pub const RECORD_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_RECORD_BYTES: usize = 32 * 1024;
const MAX_SOURCES: usize = 64;
const MAX_LISTINGS: usize = 16;
const MAX_SEARCHES: usize = 16;
const MAX_SUBAGENTS: usize = 32;
const RECORD_VERSION: u32 = 1;

/// #26 "First-route metric" state for the current reset epoch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteState {
    #[default]
    Unset,
    /// Final: Brainprint answered current/complete before any native
    /// exploration.
    BrainprintFirst,
    /// Pending: Brainprint was asked first but returned a gap.
    BrainprintGap,
    /// Pending: native exploration came first; `native_only` until a
    /// Brainprint call is observed.
    NativeFirst,
    NativeFirstThenBrainprint,
    FallbackRequired,
}

impl RouteState {
    /// The #26 telemetry classification of this epoch so far.
    pub const fn classification(self) -> Option<&'static str> {
        match self {
            Self::Unset => None,
            Self::BrainprintFirst | Self::BrainprintGap => Some("brainprint_first"),
            Self::NativeFirst => Some("native_only"),
            Self::NativeFirstThenBrainprint => Some("native_first_then_brainprint"),
            Self::FallbackRequired => Some("fallback_required"),
        }
    }
}

/// A Brainprint-delivered, verified-current source range. `lines` are
/// the 0-based, end-exclusive lines the delivery covered *completely*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDescriptor {
    pub root: String,
    pub workspace_id: String,
    pub path_rel: String,
    pub revision: String,
    pub lines: (usize, usize),
}

/// A Brainprint-delivered complete/current unfiltered recursive listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListingDescriptor {
    pub root: String,
    pub workspace_id: String,
    pub directory: Option<String>,
    pub limit: usize,
    pub fingerprint: String,
}

/// A Brainprint-delivered complete text search plus the index digest of
/// its scope at delivery time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchDescriptor {
    pub root: String,
    pub workspace_id: String,
    pub pattern: SearchPattern,
    pub case_insensitive: bool,
    pub path_prefix: Option<String>,
    pub scope_limit: usize,
    pub scope_fingerprint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub version: u32,
    pub mode: Option<Mode>,
    pub epoch: u32,
    pub bootstrapped_epoch: Option<u32>,
    pub pending_bootstrap: bool,
    pub bootstrapped_subagents: Vec<String>,
    pub route: RouteState,
    pub advised: Vec<ActionClass>,
    pub bypass: bool,
    pub sources: Vec<SourceDescriptor>,
    pub listings: Vec<ListingDescriptor>,
    pub searches: Vec<SearchDescriptor>,
    pub updated_unix_ms: u64,
}

impl SessionRecord {
    /// A reset boundary: the Agent's context no longer holds what
    /// Brainprint delivered, so no earlier delivery may justify a
    /// suppression any more (#29 "reset은 처음부터 다시 탐색해도 되는
    /// 시점이 아니다" -- but it *is* a point where delivered facts left
    /// the context).
    pub fn start_epoch(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        self.route = RouteState::Unset;
        self.advised.clear();
        self.forget_deliveries();
    }

    pub fn forget_deliveries(&mut self) {
        self.sources.clear();
        self.listings.clear();
        self.searches.clear();
    }

    pub fn forget_path(&mut self, path_rel_or_abs: &str) {
        self.sources.retain(|source| {
            !(path_rel_or_abs == source.path_rel
                || path_rel_or_abs.ends_with(&format!("/{}", source.path_rel)))
        });
    }

    pub fn push_source(&mut self, descriptor: SourceDescriptor) {
        push_bounded(&mut self.sources, descriptor, MAX_SOURCES);
    }

    pub fn push_listing(&mut self, descriptor: ListingDescriptor) {
        push_bounded(&mut self.listings, descriptor, MAX_LISTINGS);
    }

    pub fn push_search(&mut self, descriptor: SearchDescriptor) {
        push_bounded(&mut self.searches, descriptor, MAX_SEARCHES);
    }

    pub fn note_subagent(&mut self, agent_id: &str) -> bool {
        if self
            .bootstrapped_subagents
            .iter()
            .any(|seen| seen == agent_id)
        {
            return false;
        }
        push_bounded(
            &mut self.bootstrapped_subagents,
            agent_id.to_owned(),
            MAX_SUBAGENTS,
        );
        true
    }

    /// Serialize, dropping the oldest descriptors until the record fits
    /// [`MAX_RECORD_BYTES`].
    fn encode_bounded(&mut self) -> Vec<u8> {
        loop {
            let encoded = serde_json::to_vec(self).unwrap_or_default();
            if encoded.len() <= MAX_RECORD_BYTES {
                return encoded;
            }
            if !self.sources.is_empty() {
                self.sources.remove(0);
            } else if !self.searches.is_empty() {
                self.searches.remove(0);
            } else if !self.listings.is_empty() {
                self.listings.remove(0);
            } else if !self.bootstrapped_subagents.is_empty() {
                self.bootstrapped_subagents.remove(0);
            } else {
                return encoded;
            }
        }
    }
}

fn push_bounded<T: PartialEq>(items: &mut Vec<T>, item: T, max: usize) {
    items.retain(|existing| existing != &item);
    items.push(item);
    if items.len() > max {
        let excess = items.len() - max;
        items.drain(..excess);
    }
}

/// `<runtime_root>/adoption/<client>/` -- one file per session.
pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    pub fn new(runtime_root: &Path, client_id: &str) -> Self {
        Self {
            dir: runtime_root.join("adoption").join(sanitize(client_id)),
        }
    }

    pub fn adoption_root(runtime_root: &Path) -> PathBuf {
        runtime_root.join("adoption")
    }

    fn path(&self, session_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.json", util::fingerprint([session_id])))
    }

    pub fn exists(&self, session_id: &str) -> bool {
        self.path(session_id).is_file()
    }

    /// A missing, expired or unreadable record is simply a fresh one.
    pub fn load(&self, session_id: &str) -> SessionRecord {
        let path = self.path(session_id);
        let fresh = || SessionRecord {
            version: RECORD_VERSION,
            ..SessionRecord::default()
        };
        let Ok(metadata) = fs::metadata(&path) else {
            return fresh();
        };
        if is_expired(&metadata) {
            let _ = fs::remove_file(&path);
            return fresh();
        }
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<SessionRecord>(&bytes).ok())
            .filter(|record| record.version == RECORD_VERSION)
            .unwrap_or_else(fresh)
    }

    /// Atomic replace; returns the stored byte count.
    pub fn save(&self, session_id: &str, record: &mut SessionRecord) -> io::Result<usize> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path(session_id);
        let is_new = !path.exists();
        record.version = RECORD_VERSION;
        record.updated_unix_ms = util::now_unix_ms();
        let encoded = record.encode_bounded();
        let temp = path.with_extension(format!("tmp{}", std::process::id()));
        {
            let mut file = fs::File::create(&temp)?;
            file.write_all(&encoded)?;
        }
        fs::rename(&temp, &path)?;
        if is_new {
            self.prune()?;
        }
        Ok(encoded.len())
    }

    /// Drop expired records, then the oldest beyond the per-client cap.
    pub fn prune(&self) -> io::Result<()> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if !metadata.is_file() {
                continue;
            }
            if is_expired(&metadata) {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            records.push((
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                entry.path(),
            ));
        }
        if records.len() > MAX_RECORDS_PER_CLIENT {
            records.sort();
            for (_, path) in records.iter().take(records.len() - MAX_RECORDS_PER_CLIENT) {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }

    pub fn record_count(&self) -> usize {
        fs::read_dir(&self.dir).map_or(0, |entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .count()
        })
    }

    pub fn max_record_bytes(&self) -> u64 {
        fs::read_dir(&self.dir).map_or(0, |entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.metadata().ok())
                .map(|metadata| metadata.len())
                .max()
                .unwrap_or(0)
        })
    }
}

fn is_expired(metadata: &fs::Metadata) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > RECORD_TTL)
}

fn sanitize(client_id: &str) -> String {
    client_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
