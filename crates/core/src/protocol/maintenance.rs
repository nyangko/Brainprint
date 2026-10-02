//! #56 (I7 task 1): the `doctor` / `rebuild` wire (#9 task 5, `LOCKED --
//! Init / Sync / Rebuild 명령 lifecycle`).
//!
//! `doctor` reports only what the daemon actually observed; a check it
//! could not make is an explicit [`CheckWire::NotMeasured`], never a
//! guessed `Ok`. `rebuild` re-derives the rebuildable `index.db` only.

use serde::{Deserialize, Serialize};

/// Diagnose the Workspace at `path`. Read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorRequest {
    /// Absolute, resolved by the client (as `InitRequest::path`).
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DoctorResponse {
    pub daemon_version: String,
    pub protocol_version: u32,
    /// `global.db`, which every Workspace resolution reads.
    pub global_db: DatabaseCheckWire,
    pub workspace: DoctorWorkspaceWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DoctorWorkspaceWire {
    /// No Workspace is registered at the path (the `NOT_INITIALIZED`
    /// vocabulary); `reason` is the typed cause's message.
    NotInitialized {
        path: String,
        reason: String,
    },
    /// More than one registered Workspace claims the path.
    Ambiguous {
        path: String,
        workspaces: Vec<String>,
    },
    Initialized(Box<WorkspaceDoctorWire>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDoctorWire {
    pub project_id: String,
    pub workspace_id: String,
    pub workspace_root: String,
    pub is_project_home: bool,
    /// The registry identity agrees with `workspace.toml` and with every
    /// Workspace-scoped database's `db_meta` binding.
    pub binding: CheckWire,
    /// project / workspace / index, in that order.
    pub databases: Vec<DatabaseCheckWire>,
    pub index: IndexCheckWire,
    pub runtime: RuntimeCheckWire,
    pub semantic: Vec<BackendCheckWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckWire {
    Ok,
    Failed { detail: String },
    NotMeasured { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseCheckWire {
    /// `global` / `project` / `workspace` / `index`.
    pub kind: String,
    pub path: String,
    pub state: DatabaseStateWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DatabaseStateWire {
    Missing,
    /// Present, but it could not be read as a database of this kind.
    Unreadable {
        detail: String,
    },
    Opened {
        schema: SchemaCheckWire,
        integrity: CheckWire,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SchemaCheckWire {
    Current {
        version: u32,
    },
    /// Older than this binary; the next normal open migrates it.
    MigrationPending {
        version: u32,
        latest: u32,
    },
    /// Newer than this binary knows.
    Newer {
        version: u32,
        latest: u32,
    },
    /// The applied migration ledger disagrees with the compiled one.
    LedgerMismatch {
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IndexCheckWire {
    Current,
    NotCurrent { detail: String },
    NotMeasured { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeCheckWire {
    /// This daemon holds no runtime for the Workspace yet; the next query
    /// activates it.
    Inactive,
    Active {
        watcher: WatcherCheckWire,
    },
    /// A runtime exists but could not bind the Workspace.
    Unavailable {
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WatcherCheckWire {
    Attached,
    /// Degraded: currentness is proven at query time instead.
    Unavailable {
        reason: String,
    },
    /// The runtime was bound but never activated.
    NotStarted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendCheckWire {
    pub family: String,
    pub state: BackendStateWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendStateWire {
    /// Install located and launcher registered; registration starts
    /// nothing.
    Registered,
    Unavailable {
        reason: String,
    },
}

/// Rebuild the Workspace's rebuildable intelligence from current source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildRequest {
    /// Absolute, resolved by the client (as `InitRequest::path`).
    pub path: String,
}

/// A completed rebuild. A failed one is `Response::Error`, and the
/// previous `index.db` is still the one in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildResponse {
    pub project_id: String,
    pub workspace_id: String,
    pub workspace_root: String,
    /// Resources the fresh baseline generation published.
    pub resources: u64,
    pub generation_no: i64,
    pub index: IndexCheckWire,
    pub watcher: WatcherCheckWire,
    /// Semantic facts are re-derived on demand from this index, as after
    /// `init`; this is each family's availability for that.
    pub semantic: Vec<BackendCheckWire>,
}
