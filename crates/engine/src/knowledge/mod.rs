//! I5 durable Project Intelligence: typed model + storage runtime (#20
//! task 1).
//!
//! Ownership (#20 D1-D5, #13 I5 amendment):
//! - [`GlobalKnowledgeStore`] -- global.db: user Policy, user Preference,
//!   reusable Blueprint.
//! - [`ProjectKnowledgeStore`] -- the project-home project.db: Policy,
//!   Decision, project-local Blueprint, Blueprint Application, Project
//!   State. A secondary worktree opens the *same* file through
//!   [`ProjectKnowledgeStore::open_project_home`]; it never gets its own.
//! - [`WorkspaceKnowledgeStore`] -- one Workspace's workspace.db: WorkItem,
//!   Working State, Work Resource/Result/Handoff, Work Note, workspace-local
//!   Project State.
//!
//! None of these touch index.db, so an index rebuild cannot remove durable
//! knowledge, and none of them start a semantic backend.
//!
//! Provided here: create, get by stable uid, bounded (`LIMIT`) lists by
//! exact scope/status/key/topic, explicit status transitions, explicit
//! lineage, current-state upsert. Precedence/applicability is the read-only
//! resolver in [`resolve()`] (task 2). The WorkItem lifecycle is
//! [`WorkRuntime`] (task 3). Not provided: promotion (task 4),
//! search/ranking of any kind.

mod global;
mod model;
mod project;
mod resolve;
mod work;
mod workspace;

use std::{error::Error, fmt, path::PathBuf};

use brainprint_core::{
    BlueprintApplicationId, BlueprintId, DecisionId, IndexIncarnationId, PolicyId, ProjectId,
    ProjectStateId, ResourceId, UserPreferenceId, WorkItemId, WorkNoteId, WorkspaceId,
};
use rusqlite::{Connection, OptionalExtension, Params, Row, params};

pub use global::GlobalKnowledgeStore;
pub use model::*;
pub use project::ProjectKnowledgeStore;
pub use resolve::{
    ApplicabilityContext, BlueprintDefinitionState, BlueprintEvidence, ConflictKind,
    DirectiveTarget, EvidenceCategory, EvidenceRef, KnowledgeConflict, KnowledgeSources, Origin,
    RequestDirective, ResolutionReason, ResolveError, ResolveRequest, Resolved, ResolvedKnowledge,
    ShadowedItem, WorkItemEvidence, resolve,
};
pub use work::{
    GenerationReference, GenerationReferenceState, NotReady, ResourceEvidence, ResourceObservation,
    ResultObservation, Staleness, StartObservation, WorkError, WorkOverlap, WorkProgress,
    WorkRuntime, WorkSnapshot,
};
pub use workspace::WorkspaceKnowledgeStore;

use crate::{db, db::DbOpenError, registry::RegistryError, resolution::UnknownAxisValue};

/// Failure reading, writing, or opening a durable knowledge store.
#[derive(Debug)]
pub enum KnowledgeError {
    Open(DbOpenError),
    Registry(RegistryError),
    Sqlite(rusqlite::Error),
    /// A stored or supplied value outside its closed vocabulary.
    UnknownValue {
        vocabulary: &'static str,
        raw: String,
    },
    /// A scope kind/key pair that no [`KnowledgeScope`] represents.
    InvalidScope {
        kind: &'static str,
        key: Option<String>,
    },
    /// A JSON column whose content does not match its typed shape.
    InvalidJson {
        what: &'static str,
        reason: String,
    },
    /// A stored uid that is not 16 bytes.
    CorruptUid {
        table: &'static str,
    },
    /// A row's columns contradict each other (e.g. PROMOTED without target).
    Inconsistent {
        table: &'static str,
        reason: String,
    },
    NotFound {
        what: &'static str,
        uid: String,
    },
    InvalidTransition {
        what: &'static str,
        reason: String,
    },
    /// The Project's registered project-home has no project.db. Never
    /// papered over with a fresh empty file.
    ProjectHomeMissing {
        project_id: ProjectId,
        home_locator: PathBuf,
    },
    /// project-home project.db is bound to a different Project.
    ProjectIdentityMismatch {
        expected: ProjectId,
        found: Option<ProjectId>,
    },
}

impl fmt::Display for KnowledgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(source) => write!(formatter, "failed to open knowledge store: {source}"),
            Self::Registry(source) => write!(formatter, "registry lookup failed: {source}"),
            Self::Sqlite(source) => write!(formatter, "knowledge store sqlite error: {source}"),
            Self::UnknownValue { vocabulary, raw } => {
                write!(formatter, "unknown {vocabulary} value {raw:?}")
            }
            Self::InvalidScope { kind, key } => {
                write!(formatter, "invalid scope {kind} with key {key:?}")
            }
            Self::InvalidJson { what, reason } => write!(formatter, "invalid {what}: {reason}"),
            Self::CorruptUid { table } => write!(formatter, "{table} holds a non-16-byte uid"),
            Self::Inconsistent { table, reason } => {
                write!(formatter, "inconsistent {table} row: {reason}")
            }
            Self::NotFound { what, uid } => write!(formatter, "no {what} with uid {uid}"),
            Self::InvalidTransition { what, reason } => {
                write!(formatter, "invalid {what} transition: {reason}")
            }
            Self::ProjectHomeMissing {
                project_id,
                home_locator,
            } => write!(
                formatter,
                "project {project_id} has no project.db at its project-home {}",
                home_locator.display()
            ),
            Self::ProjectIdentityMismatch { expected, found } => write!(
                formatter,
                "project-home project.db is bound to {found:?}, expected {expected}"
            ),
        }
    }
}

impl Error for KnowledgeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(source) => Some(source),
            Self::Registry(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            _ => None,
        }
    }
}

impl From<DbOpenError> for KnowledgeError {
    fn from(source: DbOpenError) -> Self {
        Self::Open(source)
    }
}

impl From<RegistryError> for KnowledgeError {
    fn from(source: RegistryError) -> Self {
        Self::Registry(source)
    }
}

impl From<UnknownAxisValue> for KnowledgeError {
    fn from(source: UnknownAxisValue) -> Self {
        Self::UnknownValue {
            vocabulary: source.axis,
            raw: source.raw,
        }
    }
}

impl From<rusqlite::Error> for KnowledgeError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

/// Stable 128-bit ids stored as 16-byte BLOBs.
pub(crate) trait Uid: Copy + fmt::Display {
    fn from_uid_bytes(bytes: [u8; 16]) -> Self;
    fn uid_bytes(self) -> [u8; 16];
}

macro_rules! impl_uid {
    ($($id:ty),+) => {
        $(impl Uid for $id {
            fn from_uid_bytes(bytes: [u8; 16]) -> Self {
                Self::from_bytes(bytes)
            }
            fn uid_bytes(self) -> [u8; 16] {
                self.to_bytes()
            }
        })+
    };
}

impl_uid!(
    PolicyId,
    DecisionId,
    BlueprintId,
    BlueprintApplicationId,
    ProjectStateId,
    UserPreferenceId,
    WorkItemId,
    WorkNoteId,
    ResourceId,
    ProjectId,
    WorkspaceId,
    IndexIncarnationId
);

pub(crate) fn blob<T: Uid>(id: T) -> Vec<u8> {
    id.uid_bytes().to_vec()
}

pub(crate) fn uid_from_blob<T: Uid>(
    bytes: &[u8],
    table: &'static str,
) -> Result<T, KnowledgeError> {
    let array: [u8; 16] = bytes
        .try_into()
        .map_err(|_| KnowledgeError::CorruptUid { table })?;
    Ok(T::from_uid_bytes(array))
}

fn uid_column<T: Uid>(
    row: &Row<'_>,
    index: usize,
    table: &'static str,
) -> Result<T, KnowledgeError> {
    uid_from_blob(&row.get::<_, Vec<u8>>(index)?, table)
}

fn scope_columns(row: &Row<'_>, kind: usize) -> Result<KnowledgeScope, KnowledgeError> {
    KnowledgeScope::from_parts(&row.get::<_, String>(kind)?, row.get(kind + 1)?)
}

fn provenance_columns(row: &Row<'_>, kind: usize) -> Result<Provenance, KnowledgeError> {
    Ok(Provenance {
        source_kind: SourceKind::parse(&row.get::<_, String>(kind)?)?,
        locator: row.get(kind + 1)?,
        revision: row.get(kind + 2)?,
    })
}

pub(crate) fn query_one<T>(
    connection: &Connection,
    sql: &str,
    params: impl Params,
    decode: fn(&Row<'_>) -> Result<T, KnowledgeError>,
) -> Result<Option<T>, KnowledgeError> {
    let mut statement = connection.prepare_cached(sql)?;
    let mut rows = statement.query(params)?;
    rows.next()?.map(decode).transpose()
}

pub(crate) fn query_all<T>(
    connection: &Connection,
    sql: &str,
    params: impl Params,
    decode: fn(&Row<'_>) -> Result<T, KnowledgeError>,
) -> Result<Vec<T>, KnowledgeError> {
    let mut statement = connection.prepare_cached(sql)?;
    let mut rows = statement.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(decode(row)?);
    }
    Ok(out)
}

fn to_json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("closed typed shapes always serialize")
}

fn from_json<T: serde::de::DeserializeOwned>(
    raw: &str,
    what: &'static str,
) -> Result<T, KnowledgeError> {
    serde_json::from_str(raw).map_err(|source| KnowledgeError::InvalidJson {
        what,
        reason: source.to_string(),
    })
}

/// Which durable store owns a row. Enforces #13's canonical-owner rule on
/// scope: global.db never holds project-bound knowledge, and project.db /
/// workspace.db never hold GLOBAL knowledge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Store {
    Global,
    Project,
    Workspace,
}

pub(crate) fn require_owner(scope: &KnowledgeScope, store: Store) -> Result<(), KnowledgeError> {
    let allowed = match store {
        // A global item applies everywhere or to a declared domain; any
        // project/workspace/path-bound scope is project knowledge.
        Store::Global => matches!(scope.kind(), ScopeKind::Global | ScopeKind::Domain),
        Store::Project | Store::Workspace => scope.kind() != ScopeKind::Global,
    };
    if allowed {
        Ok(())
    } else {
        Err(KnowledgeError::InvalidScope {
            kind: scope.kind().as_str(),
            key: scope.key().map(str::to_owned),
        })
    }
}

/// One table + its self-referencing link table, for explicit lineage.
pub(crate) struct LinkTable {
    pub store: Store,
    pub item: &'static str,
    pub link: &'static str,
    pub from: &'static str,
    pub to: &'static str,
}

pub(crate) const PROJECT_POLICY: LinkTable = LinkTable {
    store: Store::Project,
    item: "policy",
    link: "policy_link",
    from: "policy_id",
    to: "related_policy_id",
};

pub(crate) const USER_POLICY: LinkTable = LinkTable {
    store: Store::Global,
    item: "user_policy",
    link: "user_policy_link",
    from: "user_policy_id",
    to: "related_user_policy_id",
};

pub(crate) const DECISION: LinkTable = LinkTable {
    store: Store::Project,
    item: "decision",
    link: "decision_link",
    from: "decision_id",
    to: "related_decision_id",
};

fn row_id_and_status(
    connection: &Connection,
    table: &str,
    uid: &[u8],
) -> Result<Option<(i64, String)>, KnowledgeError> {
    Ok(connection
        .query_row(
            &format!("SELECT id, status FROM {table} WHERE uid = ?1"),
            params![uid],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// Atomically record `from --link_kind--> to` and, when `retire_to` is
/// set, move `to` to that status. Both rows must exist and be ACTIVE; any
/// failure rolls back both writes, so lineage is never half-applied.
fn link_and_retire<T: Uid>(
    connection: &Connection,
    table: &LinkTable,
    from: T,
    to: T,
    link_kind: &str,
    retire_to: Option<&str>,
) -> Result<(), KnowledgeError> {
    let invalid = |reason: String| KnowledgeError::InvalidTransition {
        what: table.item,
        reason,
    };
    if from.uid_bytes() == to.uid_bytes() {
        return Err(invalid(format!("{from} cannot {link_kind} itself")));
    }
    let transaction = connection.unchecked_transaction()?;
    let lookup = |uid: T| -> Result<(i64, String), KnowledgeError> {
        row_id_and_status(&transaction, table.item, &blob(uid))?.ok_or(KnowledgeError::NotFound {
            what: table.item,
            uid: uid.to_string(),
        })
    };
    let (from_id, from_status) = lookup(from)?;
    let (to_id, to_status) = lookup(to)?;
    if from_status != "ACTIVE" || to_status != "ACTIVE" {
        return Err(invalid(format!(
            "{link_kind} needs two ACTIVE rows, found {from_status} -> {to_status}"
        )));
    }
    transaction.execute(
        &format!(
            "INSERT INTO {} ({}, {}, link_kind) VALUES (?1, ?2, ?3)",
            table.link, table.from, table.to
        ),
        params![from_id, to_id, link_kind],
    )?;
    if let Some(status) = retire_to {
        transaction.execute(
            &format!(
                "UPDATE {} SET status = ?1, updated_at = ?2 WHERE id = ?3",
                table.item
            ),
            params![status, db::now_millis_text(), to_id],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// `(other uid, link_kind)` edges of one row. `outgoing` = rows this one
/// points at; otherwise rows pointing at it.
fn lineage_edges<T: Uid>(
    connection: &Connection,
    table: &LinkTable,
    uid: T,
    outgoing: bool,
) -> Result<Vec<(T, String)>, KnowledgeError> {
    let (self_column, other_column) = if outgoing {
        (table.from, table.to)
    } else {
        (table.to, table.from)
    };
    let sql = format!(
        "SELECT other.uid, l.link_kind FROM {item} this \
         JOIN {link} l ON l.{self_column} = this.id \
         JOIN {item} other ON other.id = l.{other_column} \
         WHERE this.uid = ?1 ORDER BY other.id, l.link_kind",
        item = table.item,
        link = table.link,
    );
    let mut statement = connection.prepare_cached(&sql)?;
    let mut rows = statement.query(params![blob(uid)])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push((uid_column(row, 0, table.link)?, row.get(1)?));
    }
    Ok(out)
}

// ---- Policy (project.db `policy` / global.db `user_policy`) ----

const POLICY_COLUMNS: &str = "uid, scope_kind, scope_key, policy_key, title, rule_text, \
    structured_rule_json, protection_class, priority_class, status, source_kind, \
    source_locator, source_revision, created_at, updated_at";

fn decode_policy(row: &Row<'_>) -> Result<Policy, KnowledgeError> {
    Ok(Policy {
        uid: uid_column(row, 0, "policy")?,
        scope: scope_columns(row, 1)?,
        policy_key: row.get(3)?,
        title: row.get(4)?,
        rule_text: row.get(5)?,
        structured_rule: row
            .get::<_, Option<String>>(6)?
            .map(|raw| from_json(&raw, "structured rule"))
            .transpose()?,
        protection_class: ProtectionClass::parse(&row.get::<_, String>(7)?)?,
        priority_class: PriorityClass::parse(&row.get::<_, String>(8)?)?,
        status: PolicyStatus::parse(&row.get::<_, String>(9)?)?,
        provenance: provenance_columns(row, 10)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

fn insert_policy(
    connection: &Connection,
    table: &LinkTable,
    new: &NewPolicy,
) -> Result<Policy, KnowledgeError> {
    require_owner(&new.scope, table.store)?;
    let uid = PolicyId::generate();
    let now = db::now_millis_text();
    connection.execute(
        &format!(
            "INSERT INTO {} ({POLICY_COLUMNS}) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
            table.item
        ),
        params![
            blob(uid),
            new.scope.kind().as_str(),
            new.scope.key(),
            new.policy_key,
            new.title,
            new.rule_text,
            new.structured_rule.as_ref().map(to_json),
            new.protection_class.as_str(),
            new.priority_class.as_str(),
            PolicyStatus::Active.as_str(),
            new.provenance.source_kind.as_str(),
            new.provenance.locator,
            new.provenance.revision,
            now,
        ],
    )?;
    get_policy(connection, table, uid)?.ok_or(KnowledgeError::NotFound {
        what: table.item,
        uid: uid.to_string(),
    })
}

fn get_policy(
    connection: &Connection,
    table: &LinkTable,
    uid: PolicyId,
) -> Result<Option<Policy>, KnowledgeError> {
    query_one(
        connection,
        &format!("SELECT {POLICY_COLUMNS} FROM {} WHERE uid = ?1", table.item),
        params![blob(uid)],
        decode_policy,
    )
}

fn list_policies(
    connection: &Connection,
    table: &LinkTable,
    scope: &KnowledgeScope,
    status: PolicyStatus,
    limit: u32,
) -> Result<Vec<Policy>, KnowledgeError> {
    query_all(
        connection,
        &format!(
            "SELECT {POLICY_COLUMNS} FROM {} \
             WHERE scope_kind = ?1 AND scope_key IS ?2 AND status = ?3 ORDER BY id LIMIT ?4",
            table.item
        ),
        params![scope.kind().as_str(), scope.key(), status.as_str(), limit],
        decode_policy,
    )
}

/// ACTIVE <-> DISABLED only. SUPERSEDED is terminal and reachable only via
/// [`supersede_policy`], which records the lineage.
fn set_policy_status(
    connection: &Connection,
    table: &LinkTable,
    uid: PolicyId,
    next: PolicyStatus,
) -> Result<Policy, KnowledgeError> {
    let current = get_policy(connection, table, uid)?.ok_or(KnowledgeError::NotFound {
        what: table.item,
        uid: uid.to_string(),
    })?;
    let allowed = matches!(
        (current.status, next),
        (PolicyStatus::Active, PolicyStatus::Disabled)
            | (PolicyStatus::Disabled, PolicyStatus::Active)
    );
    if !allowed {
        return Err(KnowledgeError::InvalidTransition {
            what: table.item,
            reason: format!(
                "{} -> {} (SUPERSEDED needs explicit supersession)",
                current.status.as_str(),
                next.as_str()
            ),
        });
    }
    connection.execute(
        &format!(
            "UPDATE {} SET status = ?1, updated_at = ?2 WHERE uid = ?3",
            table.item
        ),
        params![next.as_str(), db::now_millis_text(), blob(uid)],
    )?;
    get_policy(connection, table, uid)?.ok_or(KnowledgeError::NotFound {
        what: table.item,
        uid: uid.to_string(),
    })
}

fn supersede_policy(
    connection: &Connection,
    table: &LinkTable,
    replacement: PolicyId,
    superseded: PolicyId,
) -> Result<(), KnowledgeError> {
    link_and_retire(
        connection,
        table,
        replacement,
        superseded,
        PolicyLinkKind::Supersedes.as_str(),
        Some(PolicyStatus::Superseded.as_str()),
    )
}

fn policy_lineage(
    connection: &Connection,
    table: &LinkTable,
    uid: PolicyId,
) -> Result<PolicyLineage, KnowledgeError> {
    let only_supersedes = |edges: Vec<(PolicyId, String)>| {
        edges
            .into_iter()
            .map(|(other, kind)| PolicyLinkKind::parse(&kind).map(|_| other))
            .collect::<Result<Vec<_>, _>>()
    };
    Ok(PolicyLineage {
        supersedes: only_supersedes(lineage_edges(connection, table, uid, true)?)?,
        superseded_by: only_supersedes(lineage_edges(connection, table, uid, false)?)?,
    })
}

// ---- Blueprint definition (global.db and project.db `blueprint`) ----

const BLUEPRINT_COLUMNS: &str = "uid, scope_kind, scope_key, blueprint_key, title, intent, \
    definition_json, status, version, source_kind, source_locator, source_revision, \
    created_at, updated_at";

fn decode_blueprint(row: &Row<'_>) -> Result<Blueprint, KnowledgeError> {
    let definition: BlueprintDefinition =
        from_json(&row.get::<_, String>(6)?, "blueprint definition")?;
    definition.validate()?;
    Ok(Blueprint {
        uid: uid_column(row, 0, "blueprint")?,
        scope: scope_columns(row, 1)?,
        blueprint_key: row.get(3)?,
        title: row.get(4)?,
        intent: row.get(5)?,
        definition,
        status: BlueprintStatus::parse(&row.get::<_, String>(7)?)?,
        version: row.get(8)?,
        provenance: provenance_columns(row, 9)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

fn insert_blueprint(
    connection: &Connection,
    store: Store,
    new: &NewBlueprint,
) -> Result<Blueprint, KnowledgeError> {
    require_owner(&new.scope, store)?;
    new.definition.validate()?;
    if new.status == BlueprintStatus::Retired {
        return Err(KnowledgeError::InvalidTransition {
            what: "blueprint",
            reason: "a Blueprint is created DRAFT or ACTIVE, never RETIRED".to_owned(),
        });
    }
    let uid = BlueprintId::generate();
    connection.execute(
        &format!(
            "INSERT INTO blueprint ({BLUEPRINT_COLUMNS}) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)"
        ),
        params![
            blob(uid),
            new.scope.kind().as_str(),
            new.scope.key(),
            new.blueprint_key,
            new.title,
            new.intent,
            to_json(&new.definition),
            new.status.as_str(),
            new.version,
            new.provenance.source_kind.as_str(),
            new.provenance.locator,
            new.provenance.revision,
            db::now_millis_text(),
        ],
    )?;
    get_blueprint(connection, uid)?.ok_or(KnowledgeError::NotFound {
        what: "blueprint",
        uid: uid.to_string(),
    })
}

fn get_blueprint(
    connection: &Connection,
    uid: BlueprintId,
) -> Result<Option<Blueprint>, KnowledgeError> {
    query_one(
        connection,
        &format!("SELECT {BLUEPRINT_COLUMNS} FROM blueprint WHERE uid = ?1"),
        params![blob(uid)],
        decode_blueprint,
    )
}

fn list_blueprints(
    connection: &Connection,
    scope: &KnowledgeScope,
    status: BlueprintStatus,
    limit: u32,
) -> Result<Vec<Blueprint>, KnowledgeError> {
    query_all(
        connection,
        &format!(
            "SELECT {BLUEPRINT_COLUMNS} FROM blueprint \
             WHERE scope_kind = ?1 AND scope_key IS ?2 AND status = ?3 ORDER BY id LIMIT ?4"
        ),
        params![scope.kind().as_str(), scope.key(), status.as_str(), limit],
        decode_blueprint,
    )
}

/// Forward only: DRAFT -> ACTIVE -> RETIRED, or DRAFT -> RETIRED.
fn set_blueprint_status(
    connection: &Connection,
    uid: BlueprintId,
    next: BlueprintStatus,
) -> Result<Blueprint, KnowledgeError> {
    let not_found = || KnowledgeError::NotFound {
        what: "blueprint",
        uid: uid.to_string(),
    };
    let current = get_blueprint(connection, uid)?.ok_or_else(not_found)?;
    let allowed = matches!(
        (current.status, next),
        (
            BlueprintStatus::Draft,
            BlueprintStatus::Active | BlueprintStatus::Retired
        ) | (BlueprintStatus::Active, BlueprintStatus::Retired)
    );
    if !allowed {
        return Err(KnowledgeError::InvalidTransition {
            what: "blueprint",
            reason: format!("{} -> {}", current.status.as_str(), next.as_str()),
        });
    }
    connection.execute(
        "UPDATE blueprint SET status = ?1, updated_at = ?2 WHERE uid = ?3",
        params![next.as_str(), db::now_millis_text(), blob(uid)],
    )?;
    get_blueprint(connection, uid)?.ok_or_else(not_found)
}

// ---- Project State (project.db `project_state` / workspace.db
// `workspace_project_state`) ----
//
// Identity is `(scope_kind, ifnull(scope_key, ''), state_key)`, enforced
// by a unique expression index in both tables; every query below uses the
// same expression so it can use that index.

const STATE_COLUMNS: &str = "uid, state_key, scope_kind, scope_key, value_type, value_json, \
    status, source_kind, source_locator, observed_revision, updated_at";

fn decode_state(row: &Row<'_>) -> Result<ProjectState, KnowledgeError> {
    Ok(ProjectState {
        uid: row
            .get::<_, Option<Vec<u8>>>(0)?
            .ok_or(KnowledgeError::CorruptUid {
                table: "project_state",
            })
            .and_then(|bytes| uid_from_blob(&bytes, "project_state"))?,
        key: row.get(1)?,
        scope: scope_columns(row, 2)?,
        value: TypedValue::from_parts(&row.get::<_, String>(4)?, &row.get::<_, String>(5)?)?,
        status: ProjectStateStatus::parse(&row.get::<_, String>(6)?)?,
        provenance: provenance_columns(row, 7)?,
        updated_at: row.get(10)?,
    })
}

fn get_state(
    connection: &Connection,
    table: &str,
    key: &str,
    scope: &KnowledgeScope,
) -> Result<Option<ProjectState>, KnowledgeError> {
    query_one(
        connection,
        &format!(
            "SELECT {STATE_COLUMNS} FROM {table} \
             WHERE scope_kind = ?1 AND ifnull(scope_key, '') = ?2 AND state_key = ?3"
        ),
        params![scope.kind().as_str(), scope.key().unwrap_or_default(), key],
        decode_state,
    )
}

fn list_state(
    connection: &Connection,
    table: &str,
    scope: &KnowledgeScope,
    limit: u32,
) -> Result<Vec<ProjectState>, KnowledgeError> {
    query_all(
        connection,
        &format!(
            "SELECT {STATE_COLUMNS} FROM {table} \
             WHERE scope_kind = ?1 AND ifnull(scope_key, '') = ?2 ORDER BY state_key LIMIT ?3"
        ),
        params![
            scope.kind().as_str(),
            scope.key().unwrap_or_default(),
            limit
        ],
        decode_state,
    )
}

/// Set the current value for `(key, scope)`. An existing entry keeps its
/// uid; nothing about precedence is decided here.
fn upsert_state(
    connection: &Connection,
    table: &str,
    store: Store,
    update: &ProjectStateUpdate,
) -> Result<ProjectState, KnowledgeError> {
    require_owner(&update.scope, store)?;
    connection.execute(
        &format!(
            "INSERT INTO {table} ({STATE_COLUMNS}) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT (scope_kind, ifnull(scope_key, ''), state_key) DO UPDATE SET \
               value_type = excluded.value_type, value_json = excluded.value_json, \
               status = excluded.status, source_kind = excluded.source_kind, \
               source_locator = excluded.source_locator, \
               observed_revision = excluded.observed_revision, updated_at = excluded.updated_at"
        ),
        params![
            blob(ProjectStateId::generate()),
            update.key,
            update.scope.kind().as_str(),
            update.scope.key(),
            update.value.value_type().as_str(),
            update.value.to_json(),
            update.status.as_str(),
            update.provenance.source_kind.as_str(),
            update.provenance.locator,
            update.provenance.revision,
            db::now_millis_text(),
        ],
    )?;
    get_state(connection, table, &update.key, &update.scope)?.ok_or(KnowledgeError::NotFound {
        what: "project state",
        uid: update.key.clone(),
    })
}

#[cfg(test)]
mod tests;
