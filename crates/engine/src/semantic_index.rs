//! Semantic freshness, analysis profile, and stable publication (#19
//! task 3).
//!
//! Task 2 owns whether a backend is *running*. This module owns a
//! different question entirely:
//!
//! > Can this semantic result be trusted as current for this source,
//! > config, environment and profile?
//!
//! The two are separate axes and stay separate. A READY runtime with a
//! DIRTY semantic index is ordinary. So is a STOPPED runtime with a
//! CURRENT published one -- a publication is persisted state, and a
//! process going away does not un-compute it.
//!
//! ## Structural truth is never hidden
//!
//! Semantic currentness lives in its own `component_state` component
//! ([`component::SEMANTIC_INDEX`]), scoped per [`AnalysisContext`], so:
//!
//! ```text
//! STRUCTURE  CURRENT @ 1042
//! RELATION   CURRENT @ 1042
//! SEMANTIC   DIRTY   @ 1041
//! ```
//!
//! is a state the model can hold and a query can read. Semantic going
//! stale takes nothing structural with it.
//!
//! ## What a publication is bound to
//!
//! A [`SemanticBasis`] names every input that could change the answer:
//! the Workspace, the context, each Resource revision the analysis read,
//! the project config fingerprint, the environment fingerprint, and --
//! where a backend's semantics depend on the project's module inventory
//! rather than on named files -- an inventory fingerprint. None of it is
//! wall-clock time, and none of it is an absolute path.
//!
//! A candidate that cannot prove its basis still holds is discarded. The
//! previous publication stays readable as the last valid one; it simply
//! stops being described as current. That is the difference between
//! "last known" and "current", and collapsing it is how a stale answer
//! gets served with a straight face.
//!
//! ## What this module does not do
//!
//! It writes no Relations, resolves no gaps, and merges nothing: task 4
//! owns structural+semantic merge. Nothing here starts a backend, and a
//! demonstrably current publication is answerable without one.

use std::{collections::BTreeMap, error::Error, fmt, path::Path};

use brainprint_core::{ResourceId, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    component::{self, ComponentRow, FreshnessState, ProcessingState},
    db::{self, DbOpenError},
    generation::{self, GenerationError, GenerationRecord},
    resolution::Support,
    resource::ResourceError,
    runtime::RuntimeState,
    schema,
    semantic::{AnalysisContext, CapabilityReport},
    symbol::{self, AnalysisProfile, SymbolError},
};

/// `last_error_code` for a publication invalidated because a Resource it
/// read moved on.
pub const SOURCE_MOVED_CODE: &str = "SEMANTIC_SOURCE_MOVED";
/// `last_error_code` for one invalidated by a project/config change.
pub const CONFIG_CHANGED_CODE: &str = "SEMANTIC_CONFIG_CHANGED";
/// `last_error_code` for one invalidated by an environment/toolchain
/// change.
pub const ENVIRONMENT_CHANGED_CODE: &str = "SEMANTIC_ENVIRONMENT_CHANGED";
/// `last_error_code` for one whose analysis profile is no longer
/// comparable with the current semantics.
pub const PROFILE_INCOMPATIBLE_CODE: &str = "SEMANTIC_PROFILE_INCOMPATIBLE";
/// `last_error_code` for a context whose backend cannot currently
/// produce semantic truth at all. Infrastructure, not source.
pub const BACKEND_UNAVAILABLE_CODE: &str = "SEMANTIC_BACKEND_UNAVAILABLE";

// ---------------------------------------------------------------------
// Currentness
// ---------------------------------------------------------------------

/// Whether an AnalysisContext's semantic results describe the current
/// inputs.
///
/// Freshness only. How *much* the backend covered is [`Support`], a
/// separate axis carried by the publication, because a partial result
/// can be perfectly current and a complete one perfectly stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticState {
    /// Nothing has ever been published for this context.
    None,
    /// A candidate exists and no publication does. Never current: a
    /// BUILDING candidate is invisible to readers by construction, since
    /// it writes nothing until it publishes.
    Building,
    Current,
    /// Superseded inputs. The last publication is still readable as the
    /// last valid one.
    Dirty,
    /// The backend cannot produce current semantic truth -- crashed,
    /// degraded, unsupported. Distinct from [`Self::Dirty`], which is
    /// about inputs moving rather than about infrastructure.
    Unavailable,
}

impl SemanticState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Building => "BUILDING",
            Self::Current => "CURRENT",
            Self::Dirty => "DIRTY",
            Self::Unavailable => "UNAVAILABLE",
        }
    }

    fn parse(raw: &str) -> Result<Self, SemanticIndexError> {
        match raw {
            "CURRENT" => Ok(Self::Current),
            "DIRTY" => Ok(Self::Dirty),
            "UNAVAILABLE" => Ok(Self::Unavailable),
            other => Err(SemanticIndexError::UnknownSemanticState {
                raw: other.to_owned(),
            }),
        }
    }

    /// The two shared `component_state` axes this state implies.
    ///
    /// Only the three persisted states have them: [`Self::None`] writes
    /// no row, and [`Self::Building`] deliberately writes nothing at all
    /// until it publishes.
    const fn axes(self) -> Option<(ProcessingState, FreshnessState)> {
        match self {
            Self::Current => Some((ProcessingState::Ready, FreshnessState::Current)),
            Self::Dirty | Self::Unavailable => {
                Some((ProcessingState::Queued, FreshnessState::Dirty))
            }
            Self::None | Self::Building => None,
        }
    }
}

impl fmt::Display for SemanticState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The semantic consequence of a runtime lifecycle state.
///
/// Deliberately a one-way derivation and not a copy of the runtime
/// vocabulary: the only thing semantic freshness needs from task 2 is
/// whether the backend can currently produce a new answer. Everything
/// else about a runtime -- how many restarts, which backoff window --
/// stays on the runtime axis.
#[must_use]
pub const fn runtime_can_produce(state: RuntimeState) -> bool {
    match state {
        RuntimeState::Cold
        | RuntimeState::Starting
        | RuntimeState::Ready
        | RuntimeState::Busy
        | RuntimeState::Idle
        | RuntimeState::Stopped => true,
        // Crashed past its restart budget, or waiting out a backoff
        // window: no new semantic truth is coming from here right now.
        RuntimeState::Backoff | RuntimeState::Degraded => false,
    }
}

// ---------------------------------------------------------------------
// Backend compatibility
// ---------------------------------------------------------------------

/// What a change of analysis profile means for an existing publication.
///
/// A deterministic policy primitive, not an installer: nothing here
/// inspects a package manager, downloads anything, or decides when to
/// upgrade a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendCompatibility {
    /// The stored results still mean what they meant. Eligible to stay
    /// current.
    Compatible,
    /// Still comparable in kind, but the declared capabilities moved, so
    /// what was concluded has to be checked again.
    Revalidate,
    /// Not comparable: a different backend family, semantics version, or
    /// compatibility class. The publication is invalid.
    Rebuild,
    /// Not decidable -- no stored profile, or one this build cannot
    /// interpret. Fails toward revalidation, never toward "current".
    Unknown,
}

impl BackendCompatibility {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compatible => "COMPATIBLE",
            Self::Revalidate => "REVALIDATE",
            Self::Rebuild => "REBUILD",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Whether an existing publication may remain current on this
    /// verdict alone. Only [`Self::Compatible`] may.
    #[must_use]
    pub const fn keeps_publication(self) -> bool {
        matches!(self, Self::Compatible)
    }

    /// Whether the stored results must be thrown away rather than
    /// re-checked.
    #[must_use]
    pub const fn requires_rebuild(self) -> bool {
        matches!(self, Self::Rebuild)
    }
}

impl fmt::Display for BackendCompatibility {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One `analysis_profile` row as stored, before any interpretation.
///
/// Read as text on purpose: a profile written by another build may carry
/// a semantics version this one has never heard of, and the useful
/// answer to that is "not comparable", not a decode failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredProfile {
    pub profile_key: String,
    pub language: String,
    pub analysis_mode: String,
    pub semantic_backend: Option<String>,
    /// Recorded, and never part of the comparison: a patch release
    /// inside one compatibility class is not a change of meaning.
    pub semantic_backend_version: Option<String>,
    pub extractor_semantics_version: String,
    pub adapter_semantics_version: String,
    pub backend_compatibility_class: String,
    pub capability_fingerprint: String,
}

impl StoredProfile {
    /// How a publication made under `self` relates to `current`.
    #[must_use]
    pub fn compatibility_with(&self, current: &AnalysisProfile) -> BackendCompatibility {
        if self.profile_key == current.profile_key {
            return BackendCompatibility::Compatible;
        }
        let comparable = self.language == current.language.to_string()
            && self.analysis_mode == current.analysis_mode
            && self.semantic_backend == current.semantic_backend
            && self.backend_compatibility_class == current.backend_compatibility_class
            && self.extractor_semantics_version == current.extractor_semantics_version
            && self.adapter_semantics_version == current.adapter_semantics_version;
        if !comparable {
            return BackendCompatibility::Rebuild;
        }
        // Same backend, same class, same semantics: what moved is the
        // capability set, and what it concluded has to be re-checked
        // rather than discarded.
        BackendCompatibility::Revalidate
    }
}

// ---------------------------------------------------------------------
// Fingerprints
// ---------------------------------------------------------------------

/// The project/config inputs one AnalysisContext's semantics depend on.
///
/// Backend-neutral: this module never discovers a `pyproject.toml`, a
/// `tsconfig.json`, or a `.csproj`. A later language adapter says which
/// inputs matter and what each one's fingerprint is, and only those
/// participate -- which is exactly why an unrelated file changing
/// somewhere else in the Workspace does not invalidate this context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigBasis {
    inputs: BTreeMap<String, String>,
}

impl ConfigBasis {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare one named input and its fingerprint. Re-declaring a name
    /// replaces it.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, fingerprint: impl Into<String>) -> Self {
        self.inputs.insert(name.into(), fingerprint.into());
        self
    }

    /// The names taking part, in canonical order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.inputs.keys().map(String::as_str)
    }

    /// The deterministic fingerprint of these inputs.
    ///
    /// Ordered by name, so the order a caller happened to discover them
    /// in -- or a hash map's iteration order -- cannot change the
    /// answer.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let fields: Vec<(&str, &str)> = self
            .inputs
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        db::fingerprint("semantic-config-1", &fields)
    }
}

// ---------------------------------------------------------------------
// Basis
// ---------------------------------------------------------------------

/// Everything a semantic publication was computed from.
///
/// Immutable once a candidate begins: the point is to compare it against
/// the world later, and a basis that can be edited proves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticBasis {
    pub workspace: WorkspaceId,
    pub context_key: String,
    /// Exact Resource revisions the analysis read.
    pub sources: BTreeMap<ResourceId, String>,
    pub config_fingerprint: String,
    pub environment_fingerprint: String,
    /// For a backend whose semantics depend on the project's whole
    /// module inventory rather than on a list of files. `None` when the
    /// named Resources are the whole dependency.
    pub inventory_fingerprint: Option<String>,
}

impl SemanticBasis {
    /// A basis for `context` over no sources yet.
    #[must_use]
    pub fn new(context: &AnalysisContext, config: &ConfigBasis) -> Self {
        Self {
            workspace: context.workspace,
            context_key: context.context_key(),
            sources: BTreeMap::new(),
            config_fingerprint: config.fingerprint(),
            environment_fingerprint: context.toolchain.fingerprint(),
            inventory_fingerprint: None,
        }
    }

    /// Record that the analysis read `resource` at `revision`.
    #[must_use]
    pub fn with_source(mut self, resource: ResourceId, revision: impl Into<String>) -> Self {
        self.sources.insert(resource, revision.into());
        self
    }

    #[must_use]
    pub fn with_inventory(mut self, fingerprint: impl Into<String>) -> Self {
        self.inventory_fingerprint = Some(fingerprint.into());
        self
    }

    /// The deterministic identity of this whole basis.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut fields: Vec<(String, String)> = vec![
            ("workspace".to_owned(), self.workspace.to_string()),
            ("context".to_owned(), self.context_key.clone()),
            ("config".to_owned(), self.config_fingerprint.clone()),
            (
                "environment".to_owned(),
                self.environment_fingerprint.clone(),
            ),
            (
                "inventory".to_owned(),
                self.inventory_fingerprint
                    .clone()
                    .unwrap_or_else(|| "-".to_owned()),
            ),
        ];
        // BTreeMap iteration is ordered by ResourceId, so the same
        // dependency set always fingerprints the same way.
        for (resource, revision) in &self.sources {
            fields.push((format!("source:{resource}"), revision.clone()));
        }
        let borrowed: Vec<(&str, &str)> = fields
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        db::fingerprint("semantic-basis-1", &borrowed)
    }
}

/// The inputs as they are *now*, to validate a basis against.
///
/// Resource revisions are deliberately absent: those are read from the
/// index inside the publication transaction, because the whole question
/// is whether the caller's view of them is still the index's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentInputs {
    pub workspace: WorkspaceId,
    pub config_fingerprint: String,
    pub environment_fingerprint: String,
    pub inventory_fingerprint: Option<String>,
    pub profile: AnalysisProfile,
}

impl CurrentInputs {
    /// The inputs for `context`, as the caller currently observes them.
    #[must_use]
    pub fn new(
        context: &AnalysisContext,
        config: &ConfigBasis,
        capabilities: &CapabilityReport,
    ) -> Self {
        Self {
            workspace: context.workspace,
            config_fingerprint: config.fingerprint(),
            environment_fingerprint: context.toolchain.fingerprint(),
            inventory_fingerprint: None,
            profile: AnalysisProfile::semantic(context, capabilities),
        }
    }

    #[must_use]
    pub fn with_inventory(mut self, fingerprint: impl Into<String>) -> Self {
        self.inventory_fingerprint = Some(fingerprint.into());
        self
    }
}

/// Why a candidate or a persisted publication is not applicable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObsoleteReason {
    WorkspaceMismatch {
        basis: WorkspaceId,
        current: WorkspaceId,
    },
    /// A Resource the analysis read has moved on since.
    SourceRevisionMoved {
        resource: ResourceId,
        basis: String,
        current: String,
    },
    /// A Resource the analysis read is no longer in the inventory.
    SourceGone {
        resource: ResourceId,
    },
    ConfigChanged {
        basis: String,
        current: String,
    },
    EnvironmentChanged {
        basis: String,
        current: String,
    },
    InventoryChanged,
    ProfileIncompatible {
        compatibility: BackendCompatibility,
    },
    /// The Workspace clock moved while the analysis ran.
    WorkspaceRevisionMoved {
        basis: String,
        current: String,
    },
}

impl ObsoleteReason {
    /// The `last_error_code` this reason is recorded under.
    #[must_use]
    pub const fn error_code(&self) -> &'static str {
        match self {
            Self::SourceRevisionMoved { .. }
            | Self::SourceGone { .. }
            | Self::WorkspaceRevisionMoved { .. }
            | Self::WorkspaceMismatch { .. } => SOURCE_MOVED_CODE,
            Self::ConfigChanged { .. } | Self::InventoryChanged => CONFIG_CHANGED_CODE,
            Self::EnvironmentChanged { .. } => ENVIRONMENT_CHANGED_CODE,
            Self::ProfileIncompatible { .. } => PROFILE_INCOMPATIBLE_CODE,
        }
    }
}

impl fmt::Display for ObsoleteReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceMismatch { basis, current } => write!(
                formatter,
                "basis workspace {basis} is not the current workspace {current}"
            ),
            Self::SourceRevisionMoved {
                resource,
                basis,
                current,
            } => write!(
                formatter,
                "resource {resource} moved from revision {basis} to {current}"
            ),
            Self::SourceGone { resource } => {
                write!(formatter, "resource {resource} is no longer active")
            }
            Self::ConfigChanged { basis, current } => write!(
                formatter,
                "config fingerprint moved from {basis} to {current}"
            ),
            Self::EnvironmentChanged { basis, current } => write!(
                formatter,
                "environment fingerprint moved from {basis} to {current}"
            ),
            Self::InventoryChanged => formatter.write_str("project module inventory changed"),
            Self::ProfileIncompatible { compatibility } => {
                write!(formatter, "analysis profile is {compatibility}")
            }
            Self::WorkspaceRevisionMoved { basis, current } => write!(
                formatter,
                "workspace revision moved from {basis} to {current}"
            ),
        }
    }
}

// ---------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------

#[derive(Debug)]
pub enum SemanticIndexError {
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    Generation(GenerationError),
    Component(component::ComponentError),
    Symbol(SymbolError),
    Resource(ResourceError),
    /// The candidate's basis no longer holds. Nothing was written, and
    /// the previous publication is untouched.
    Obsolete {
        context_key: String,
        reason: ObsoleteReason,
    },
    UnknownSemanticState {
        raw: String,
    },
    /// A stored `support` value outside the closed vocabulary.
    UnknownSupport {
        raw: String,
    },
    /// A publication row referenced a profile row that is not there.
    MissingProfile {
        profile_id: i64,
    },
}

impl fmt::Display for SemanticIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open(source) => write!(formatter, "index.db open failed: {source}"),
            Self::Sqlite(source) => write!(formatter, "semantic index sqlite error: {source}"),
            Self::Generation(source) => write!(formatter, "generation failed: {source}"),
            Self::Component(source) => write!(formatter, "component_state failed: {source}"),
            Self::Symbol(source) => write!(formatter, "analysis profile failed: {source}"),
            Self::Resource(source) => write!(formatter, "resource store failed: {source}"),
            Self::Obsolete {
                context_key,
                reason,
            } => write!(
                formatter,
                "semantic candidate for {context_key} is obsolete: {reason}"
            ),
            Self::UnknownSemanticState { raw } => {
                write!(formatter, "unknown semantic state {raw:?}")
            }
            Self::UnknownSupport { raw } => write!(formatter, "unknown support value {raw:?}"),
            Self::MissingProfile { profile_id } => {
                write!(formatter, "analysis profile {profile_id} is missing")
            }
        }
    }
}

impl Error for SemanticIndexError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            Self::Generation(source) => Some(source),
            Self::Component(source) => Some(source),
            Self::Symbol(source) => Some(source),
            Self::Resource(source) => Some(source),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for SemanticIndexError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<GenerationError> for SemanticIndexError {
    fn from(source: GenerationError) -> Self {
        Self::Generation(source)
    }
}

impl From<component::ComponentError> for SemanticIndexError {
    fn from(source: component::ComponentError) -> Self {
        Self::Component(source)
    }
}

impl From<SymbolError> for SemanticIndexError {
    fn from(source: SymbolError) -> Self {
        Self::Symbol(source)
    }
}

impl From<ResourceError> for SemanticIndexError {
    fn from(source: ResourceError) -> Self {
        Self::Resource(source)
    }
}

// ---------------------------------------------------------------------
// Candidate and publication
// ---------------------------------------------------------------------

/// An in-progress semantic generation.
///
/// Holding one changes nothing a reader can see: no `component_state`
/// row moves, no publication row is written, and the stable pointer
/// stays where it was. The only visible trace is a BUILDING `generation`
/// row, which is never read as current by anything.
#[derive(Debug)]
pub struct SemanticCandidate {
    generation: GenerationRecord,
    basis: SemanticBasis,
    profile: AnalysisProfile,
    support: Support,
}

impl SemanticCandidate {
    /// Always [`SemanticState::Building`]. A candidate is not current,
    /// and there is no method that says otherwise.
    #[must_use]
    pub const fn state(&self) -> SemanticState {
        SemanticState::Building
    }

    #[must_use]
    pub const fn generation_id(&self) -> i64 {
        self.generation.id
    }

    #[must_use]
    pub fn basis(&self) -> &SemanticBasis {
        &self.basis
    }

    /// How much of its scope the backend covered. A coherent PARTIAL
    /// result is publishable: "the backend reported diagnostics" is not
    /// the same as "the backend failed", and requiring zero diagnostics
    /// before publishing would make a project with one type error have
    /// no semantics at all.
    #[must_use]
    pub const fn support(&self) -> Support {
        self.support
    }
}

/// One published semantic generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticPublication {
    pub context_key: String,
    pub generation_id: i64,
    pub basis: SemanticBasis,
    pub basis_workspace_revision: String,
    pub profile_key: String,
    pub support: Support,
}

/// What a reader sees about one context's semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticStatus {
    pub state: SemanticState,
    /// The last publication's coverage, preserved while DIRTY -- the
    /// last valid answer keeps describing itself honestly.
    pub support: Option<Support>,
    pub stable_generation_id: Option<i64>,
    /// The basis the last publication was computed from. Persisted, so
    /// it survives a restart and can be revalidated afterwards.
    pub basis: Option<SemanticBasis>,
    pub last_error_code: Option<String>,
}

impl SemanticStatus {
    /// Nothing has ever been published for this context.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            state: SemanticState::None,
            support: None,
            stable_generation_id: None,
            basis: None,
            last_error_code: None,
        }
    }

    /// Whether semantic results may be presented as current.
    #[must_use]
    pub const fn is_current(&self) -> bool {
        matches!(self.state, SemanticState::Current)
    }

    /// Whether a last-valid publication exists, current or not.
    #[must_use]
    pub const fn has_last_valid(&self) -> bool {
        self.stable_generation_id.is_some()
    }
}

// ---------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------

/// Semantic freshness and publication over one `index.db`.
///
/// Never constructed by I2/I3: structural indexing neither needs nor
/// creates one.
pub struct SemanticIndex {
    connection: Connection,
}

impl SemanticIndex {
    pub fn open(path: &Path) -> Result<Self, SemanticIndexError> {
        let opened = schema::index::open(path).map_err(SemanticIndexError::Open)?;
        Ok(Self::from_connection(opened.connection))
    }

    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self { connection }
    }

    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Begin a candidate generation for `basis`.
    ///
    /// The profile row is ensured now, so that a publication can point
    /// at the semantics it was produced under even if this build is
    /// meeting them for the first time.
    pub fn begin_candidate(
        &self,
        basis: SemanticBasis,
        profile: AnalysisProfile,
        support: Support,
    ) -> Result<SemanticCandidate, SemanticIndexError> {
        let revision = generation::current_workspace_revision(&self.connection)?
            .ok_or(GenerationError::ClockNotBootstrapped)?;
        symbol::ensure_profile(&self.connection, &profile)?;
        let generation = generation::begin_generation(&self.connection, &revision)?;
        Ok(SemanticCandidate {
            generation,
            basis,
            profile,
            support,
        })
    }

    /// Validate the candidate's basis and publish it atomically.
    ///
    /// Everything happens in one transaction: the basis re-check against
    /// the index, the publication row, the dependency set, the component
    /// state, and the generation going STABLE. A reader therefore sees
    /// generation N with N's dependency set, or N+1 with N+1's, and
    /// never a mixture.
    ///
    /// On an obsolete basis nothing is written, the candidate's
    /// generation is ABORTED, and the previous publication stays exactly
    /// as it was -- still readable, still describing itself as whatever
    /// it already was.
    pub fn publish(
        &self,
        candidate: SemanticCandidate,
        current: &CurrentInputs,
    ) -> Result<SemanticPublication, SemanticIndexError> {
        let transaction = self.connection.unchecked_transaction()?;

        if let Err(reason) = validate_basis(&transaction, &candidate.basis, current)? {
            drop(transaction);
            generation::abort_generation(
                &self.connection,
                candidate.generation.id,
                &reason.to_string(),
            )?;
            return Err(SemanticIndexError::Obsolete {
                context_key: candidate.basis.context_key.clone(),
                reason,
            });
        }

        // The Workspace clock is the last gate, and the grant is what
        // proves the generation is still BUILDING against a basis that
        // is still current -- inside this transaction, where nothing can
        // move between the check and the writes it authorizes.
        let (record, grant) =
            match generation::grant_publication(&transaction, candidate.generation.id) {
                Ok(granted) => granted,
                Err(GenerationError::ObsoleteBasisRevision {
                    generation_id,
                    basis,
                    current: clock,
                }) => {
                    drop(transaction);
                    let reason = ObsoleteReason::WorkspaceRevisionMoved {
                        basis: basis.clone(),
                        current: clock.clone(),
                    };
                    generation::abort_generation(
                        &self.connection,
                        generation_id,
                        &reason.to_string(),
                    )?;
                    return Err(SemanticIndexError::Obsolete {
                        context_key: candidate.basis.context_key.clone(),
                        reason,
                    });
                }
                Err(source) => return Err(source.into()),
            };

        let profile_id = symbol::ensure_profile(&transaction, &candidate.profile)?;
        write_publication(
            &transaction,
            &candidate,
            profile_id,
            grant.basis_workspace_revision(),
        )?;
        write_component(
            &transaction,
            &candidate.basis.context_key,
            SemanticState::Current,
            grant.basis_workspace_revision(),
            Some(record.id),
            None,
        )?;
        generation::finish_publish_stable(&transaction, &record)?;
        transaction.commit()?;

        Ok(SemanticPublication {
            context_key: candidate.basis.context_key.clone(),
            generation_id: record.id,
            basis: candidate.basis,
            basis_workspace_revision: record.basis_workspace_revision,
            profile_key: candidate.profile.profile_key,
            support: candidate.support,
        })
    }

    /// Abandon a candidate without publishing. The previous publication
    /// is untouched -- a failed analysis never replaces a real answer
    /// with an empty success.
    pub fn discard(
        &self,
        candidate: SemanticCandidate,
        reason: &str,
    ) -> Result<(), SemanticIndexError> {
        generation::abort_generation(&self.connection, candidate.generation.id, reason)?;
        Ok(())
    }

    /// What a reader sees for one context.
    ///
    /// CURRENT is reported only while the component's basis Workspace
    /// revision is still the clock's. A publication whose Workspace moved
    /// on -- while the daemon was down, for instance -- reads DIRTY
    /// without anything having to mark it, because over-claiming is the
    /// one failure mode this axis exists to prevent.
    pub fn status(&self, context_key: &str) -> Result<SemanticStatus, SemanticIndexError> {
        let Some(row) = component::read_scoped(
            &self.connection,
            component::SEMANTIC_INDEX,
            component::ANALYSIS_CONTEXT_SCOPE_KIND,
            context_key,
        )?
        else {
            return Ok(SemanticStatus::none());
        };

        let stored = row
            .detail_state
            .as_deref()
            .map_or(Ok(SemanticState::Dirty), SemanticState::parse)?;
        let clock = generation::current_workspace_revision(&self.connection)?;
        let state = match (stored, clock) {
            (SemanticState::Current, Some(clock)) if clock != row.basis_workspace_revision => {
                SemanticState::Dirty
            }
            (state, _) => state,
        };

        let publication = read_publication(&self.connection, context_key)?;
        Ok(SemanticStatus {
            state,
            support: publication.as_ref().map(|stored| stored.support),
            stable_generation_id: row.stable_generation_id,
            basis: publication.map(|stored| stored.basis),
            last_error_code: row.last_error_code,
        })
    }

    /// The stored profile a context's publication was produced under.
    pub fn published_profile(
        &self,
        context_key: &str,
    ) -> Result<Option<StoredProfile>, SemanticIndexError> {
        let Some(stored) = read_publication(&self.connection, context_key)? else {
            return Ok(None);
        };
        read_profile(&self.connection, stored.profile_id)?
            .ok_or(SemanticIndexError::MissingProfile {
                profile_id: stored.profile_id,
            })
            .map(Some)
    }

    /// Mark one context not current, preserving its last valid
    /// publication.
    ///
    /// Targeted by design: dirtying a Python context says nothing about
    /// the TypeScript context beside it.
    pub fn mark_dirty(
        &self,
        context_key: &str,
        error_code: &str,
    ) -> Result<(), SemanticIndexError> {
        self.mark(context_key, SemanticState::Dirty, error_code)
    }

    /// Mark one context unable to produce current semantic truth. Its
    /// last valid publication stays readable.
    pub fn mark_unavailable(
        &self,
        context_key: &str,
        error_code: &str,
    ) -> Result<(), SemanticIndexError> {
        self.mark(context_key, SemanticState::Unavailable, error_code)
    }

    fn mark(
        &self,
        context_key: &str,
        state: SemanticState,
        error_code: &str,
    ) -> Result<(), SemanticIndexError> {
        let Some(row) = component::read_scoped(
            &self.connection,
            component::SEMANTIC_INDEX,
            component::ANALYSIS_CONTEXT_SCOPE_KIND,
            context_key,
        )?
        else {
            // Nothing published, nothing to invalidate. Writing a DIRTY
            // row for a context that never had results would invent a
            // last-valid snapshot that does not exist.
            return Ok(());
        };
        write_component(
            &self.connection,
            context_key,
            state,
            &row.basis_workspace_revision,
            row.stable_generation_id,
            Some(error_code),
        )
    }

    /// Mark every context whose publication read `resource` not current.
    ///
    /// Returns the context keys affected. Contexts that never read it
    /// are left alone: invalidation is scoped by what a publication
    /// actually depended on, not by the Workspace having changed at all.
    pub fn invalidate_resource(
        &self,
        resource: ResourceId,
    ) -> Result<Vec<String>, SemanticIndexError> {
        let mut statement = self.connection.prepare(
            "SELECT p.context_key FROM semantic_publication p \
             JOIN semantic_publication_source s ON s.publication_id = p.id \
             JOIN resource r ON r.id = s.resource_id \
             WHERE r.uid = ?1 ORDER BY p.context_key",
        )?;
        let keys: Vec<String> = statement
            .query_map(params![resource.to_bytes().to_vec()], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        drop(statement);

        for key in &keys {
            self.mark_dirty(key, SOURCE_MOVED_CODE)?;
        }
        Ok(keys)
    }

    /// Re-check a persisted publication against the inputs as they are
    /// now, and record the answer.
    ///
    /// This is what a restart runs. A persisted publication is not
    /// current because it says so on disk -- process-local backend state
    /// is gone, and the world may have moved while nothing was watching.
    /// It is current only if its stored basis still holds and its
    /// profile is still [`BackendCompatibility::Compatible`].
    pub fn revalidate(
        &self,
        context_key: &str,
        current: &CurrentInputs,
    ) -> Result<SemanticStatus, SemanticIndexError> {
        let Some(stored) = read_publication(&self.connection, context_key)? else {
            return Ok(SemanticStatus::none());
        };

        let compatibility = match read_profile(&self.connection, stored.profile_id)? {
            Some(profile) => profile.compatibility_with(&current.profile),
            None => BackendCompatibility::Unknown,
        };
        if !compatibility.keeps_publication() {
            self.mark_dirty(context_key, PROFILE_INCOMPATIBLE_CODE)?;
            return self.status(context_key);
        }

        if let Err(reason) = validate_basis(&self.connection, &stored.basis, current)? {
            self.mark_dirty(context_key, reason.error_code())?;
            return self.status(context_key);
        }

        // The basis holds and the semantics are comparable: restore
        // CURRENT against the Workspace revision the publication was
        // made for, which `status` re-checks against the clock anyway.
        write_component(
            &self.connection,
            context_key,
            SemanticState::Current,
            &stored.basis_workspace_revision,
            Some(stored.generation_id),
            None,
        )?;
        self.status(context_key)
    }
}

// ---------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------

/// A `semantic_publication` row, decoded.
struct StoredPublication {
    generation_id: i64,
    profile_id: i64,
    basis_workspace_revision: String,
    support: Support,
    basis: SemanticBasis,
}

fn validate_basis(
    connection: &Connection,
    basis: &SemanticBasis,
    current: &CurrentInputs,
) -> Result<Result<(), ObsoleteReason>, SemanticIndexError> {
    if basis.workspace != current.workspace {
        return Ok(Err(ObsoleteReason::WorkspaceMismatch {
            basis: basis.workspace,
            current: current.workspace,
        }));
    }
    if basis.config_fingerprint != current.config_fingerprint {
        return Ok(Err(ObsoleteReason::ConfigChanged {
            basis: basis.config_fingerprint.clone(),
            current: current.config_fingerprint.clone(),
        }));
    }
    if basis.environment_fingerprint != current.environment_fingerprint {
        return Ok(Err(ObsoleteReason::EnvironmentChanged {
            basis: basis.environment_fingerprint.clone(),
            current: current.environment_fingerprint.clone(),
        }));
    }
    if basis.inventory_fingerprint != current.inventory_fingerprint {
        return Ok(Err(ObsoleteReason::InventoryChanged));
    }

    for (resource, revision) in &basis.sources {
        let stored: Option<(String, String)> = connection
            .query_row(
                "SELECT resource_revision, state FROM resource WHERE uid = ?1",
                params![resource.to_bytes().to_vec()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((current_revision, state)) = stored else {
            return Ok(Err(ObsoleteReason::SourceGone {
                resource: *resource,
            }));
        };
        if state != "ACTIVE" {
            return Ok(Err(ObsoleteReason::SourceGone {
                resource: *resource,
            }));
        }
        if &current_revision != revision {
            return Ok(Err(ObsoleteReason::SourceRevisionMoved {
                resource: *resource,
                basis: revision.clone(),
                current: current_revision,
            }));
        }
    }

    Ok(Ok(()))
}

fn write_publication(
    connection: &Connection,
    candidate: &SemanticCandidate,
    profile_id: i64,
    basis_workspace_revision: &str,
) -> Result<(), SemanticIndexError> {
    let basis = &candidate.basis;
    // One row per context: publishing N+1 replaces N entirely, together
    // with its dependency set, in this one transaction.
    connection.execute(
        "DELETE FROM semantic_publication_source WHERE publication_id IN \
         (SELECT id FROM semantic_publication WHERE context_key = ?1)",
        params![basis.context_key],
    )?;
    connection.execute(
        "DELETE FROM semantic_publication WHERE context_key = ?1",
        params![basis.context_key],
    )?;
    connection.execute(
        "INSERT INTO semantic_publication \
         (context_key, workspace_uid, analysis_profile_id, generation_id, \
          basis_workspace_revision, basis_fingerprint, config_fingerprint, \
          environment_fingerprint, inventory_fingerprint, support, published_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            basis.context_key,
            basis.workspace.to_bytes().to_vec(),
            profile_id,
            candidate.generation.id,
            basis_workspace_revision,
            basis.fingerprint(),
            basis.config_fingerprint,
            basis.environment_fingerprint,
            basis.inventory_fingerprint,
            candidate.support.as_str(),
            db::now_millis_text(),
        ],
    )?;
    let publication_id = connection.last_insert_rowid();

    for (resource, revision) in &basis.sources {
        let row_id: Option<i64> = connection
            .query_row(
                "SELECT id FROM resource WHERE uid = ?1",
                params![resource.to_bytes().to_vec()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(row_id) = row_id else {
            // Validation already refused an unknown Resource; reaching
            // here would mean the inventory moved inside the
            // transaction, which it cannot.
            return Err(SemanticIndexError::Obsolete {
                context_key: basis.context_key.clone(),
                reason: ObsoleteReason::SourceGone {
                    resource: *resource,
                },
            });
        };
        connection.execute(
            "INSERT INTO semantic_publication_source \
             (publication_id, resource_id, resource_revision) VALUES (?1, ?2, ?3)",
            params![publication_id, row_id, revision],
        )?;
    }
    Ok(())
}

/// `semantic_publication`'s columns as stored, before decoding: id,
/// generation, profile, workspace uid, basis revision, the three
/// fingerprints, and support.
type RawPublicationRow = (
    i64,
    i64,
    i64,
    Vec<u8>,
    String,
    String,
    String,
    Option<String>,
    String,
);

fn read_publication(
    connection: &Connection,
    context_key: &str,
) -> Result<Option<StoredPublication>, SemanticIndexError> {
    let row: Option<RawPublicationRow> = connection
        .query_row(
            "SELECT id, generation_id, analysis_profile_id, workspace_uid, \
                    basis_workspace_revision, config_fingerprint, environment_fingerprint, \
                    inventory_fingerprint, support \
             FROM semantic_publication WHERE context_key = ?1",
            params![context_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()?;

    let Some(row) = row else {
        return Ok(None);
    };
    let workspace = workspace_id(&row.3)?;
    let support = Support::parse(&row.8)
        .map_err(|source| SemanticIndexError::UnknownSupport { raw: source.raw })?;

    let mut statement = connection.prepare(
        "SELECT r.uid, s.resource_revision FROM semantic_publication_source s \
         JOIN resource r ON r.id = s.resource_id WHERE s.publication_id = ?1",
    )?;
    let mut sources = BTreeMap::new();
    for entry in statement.query_map(params![row.0], |source| {
        Ok((source.get::<_, Vec<u8>>(0)?, source.get::<_, String>(1)?))
    })? {
        let (uid, revision) = entry?;
        sources.insert(resource_id(&uid)?, revision);
    }
    drop(statement);

    Ok(Some(StoredPublication {
        generation_id: row.1,
        profile_id: row.2,
        basis_workspace_revision: row.4.clone(),
        support,
        basis: SemanticBasis {
            workspace,
            context_key: context_key.to_owned(),
            sources,
            config_fingerprint: row.5,
            environment_fingerprint: row.6,
            inventory_fingerprint: row.7,
        },
    }))
}

fn read_profile(
    connection: &Connection,
    profile_id: i64,
) -> Result<Option<StoredProfile>, SemanticIndexError> {
    Ok(connection
        .query_row(
            "SELECT profile_key, language, analysis_mode, semantic_backend, \
                    semantic_backend_version, extractor_semantics_version, \
                    adapter_semantics_version, backend_compatibility_class, \
                    capability_fingerprint \
             FROM analysis_profile WHERE id = ?1",
            params![profile_id],
            |row| {
                Ok(StoredProfile {
                    profile_key: row.get(0)?,
                    language: row.get(1)?,
                    analysis_mode: row.get(2)?,
                    semantic_backend: row.get(3)?,
                    semantic_backend_version: row.get(4)?,
                    extractor_semantics_version: row.get(5)?,
                    adapter_semantics_version: row.get(6)?,
                    backend_compatibility_class: row.get(7)?,
                    capability_fingerprint: row.get(8)?,
                })
            },
        )
        .optional()?)
}

fn write_component(
    connection: &Connection,
    context_key: &str,
    state: SemanticState,
    basis_workspace_revision: &str,
    stable_generation_id: Option<i64>,
    error_code: Option<&str>,
) -> Result<(), SemanticIndexError> {
    let (processing_state, freshness_state) = state
        .axes()
        .expect("only persisted semantic states are written to component_state");
    component::write_scoped(
        connection,
        component::SEMANTIC_INDEX,
        component::ANALYSIS_CONTEXT_SCOPE_KIND,
        context_key,
        &ComponentRow {
            basis_workspace_revision: basis_workspace_revision.to_owned(),
            // Preserved across DIRTY: the last published generation stays
            // available as the last valid snapshot, it just stops being
            // described as current.
            stable_generation_id,
            processing_state,
            freshness_state,
            last_error_code: error_code.map(ToOwned::to_owned),
            detail_state: Some(state.as_str().to_owned()),
        },
    )?;
    Ok(())
}

fn workspace_id(raw: &[u8]) -> Result<WorkspaceId, SemanticIndexError> {
    let bytes: [u8; 16] = raw
        .try_into()
        .map_err(|_| SemanticIndexError::UnknownSupport {
            raw: "workspace_uid is not 16 bytes".to_owned(),
        })?;
    Ok(WorkspaceId::from_bytes(bytes))
}

fn resource_id(raw: &[u8]) -> Result<ResourceId, SemanticIndexError> {
    let bytes: [u8; 16] = raw
        .try_into()
        .map_err(|_| SemanticIndexError::UnknownSupport {
            raw: "resource uid is not 16 bytes".to_owned(),
        })?;
    Ok(ResourceId::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        generation::GenerationStore,
        resource::{Resource, ResourceLanguage, ResourceStore},
        runtime::{
            HostError, HostHealth, RuntimePolicy, RuntimeRequest, RuntimeResponse, RuntimeState,
            SemanticBackendLauncher, SemanticRuntimeHost, SemanticRuntimeSupervisor,
        },
        scan::BaselineScan,
        semantic::{
            AnalysisContext, AnalysisContextBinding, CapabilityReport, ProjectRootIdentity,
            SemanticBackendKind, SemanticCapability, ToolchainIdentity,
        },
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    /// A Workspace with a published structural baseline, so semantic
    /// publication has real Resource revisions to bind to.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-semantic-index-{label}-{}-{sequence}",
                std::process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("workspace root");
            let fixture = Self { base, root };
            fixture.write("src/app.py", "def go():\n    return 1\n");
            fixture.write("src/other.py", "def stay():\n    return 2\n");
            fixture.index();
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        fn index(&self) {
            let scan = BaselineScan::open(&self.db_path()).expect("index.db");
            scan.run_initial_scan(&self.root, &WorkspaceConfig::default(), "workspace-rev-1")
                .expect("baseline scan");
        }

        fn resource(&self, rel: &str) -> Resource {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn semantic(&self) -> SemanticIndex {
            SemanticIndex::open(&self.db_path()).expect("index.db")
        }

        /// Move one Resource's revision, as a real edit would.
        fn bump_revision(&self, rel: &str, revision: &str) {
            let resource = self.resource(rel);
            let store = ResourceStore::open(&self.db_path()).expect("index.db");
            store
                .connection()
                .execute(
                    "UPDATE resource SET resource_revision = ?1 WHERE uid = ?2",
                    params![revision, resource.id.to_bytes().to_vec()],
                )
                .expect("revision bump");
        }

        fn advance_workspace_revision(&self, revision: &str) {
            GenerationStore::open(&self.db_path())
                .expect("index.db")
                .set_current_workspace_revision(revision)
                .expect("clock");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn toolchain(environment: &str) -> ToolchainIdentity {
        ToolchainIdentity {
            backend_version: "1.0.0".to_owned(),
            backend_compatibility_class: "fake-python:1".to_owned(),
            environment_fingerprint: environment.to_owned(),
        }
    }

    fn context_with(workspace: [u8; 16], root: [u8; 16], environment: &str) -> AnalysisContext {
        AnalysisContext {
            workspace: WorkspaceId::from_bytes(workspace),
            backend: SemanticBackendKind::Python,
            language: ResourceLanguage::Python,
            project_root: ProjectRootIdentity::Config(ResourceId::from_bytes(root)),
            toolchain: toolchain(environment),
        }
    }

    fn context() -> AnalysisContext {
        context_with([1; 16], [2; 16], "sha256:venv")
    }

    fn capabilities(context: &AnalysisContext, support: Support) -> CapabilityReport {
        let mut report = CapabilityReport::new(context);
        report.declare(SemanticCapability::ImportBinding, support);
        report
    }

    fn config() -> ConfigBasis {
        ConfigBasis::new().with("pyproject.toml", "sha256:config-a")
    }

    fn inputs(context: &AnalysisContext, config: &ConfigBasis) -> CurrentInputs {
        CurrentInputs::new(context, config, &capabilities(context, Support::Supported))
    }

    /// Publish one semantic generation over `sources`, returning it.
    fn publish(
        fixture: &Fixture,
        semantic: &SemanticIndex,
        context: &AnalysisContext,
        config: &ConfigBasis,
        support: Support,
        sources: &[&str],
    ) -> Result<SemanticPublication, SemanticIndexError> {
        let mut basis = SemanticBasis::new(context, config);
        for rel in sources {
            let resource = fixture.resource(rel);
            basis = basis.with_source(resource.id, resource.resource_revision.clone());
        }
        let profile = AnalysisProfile::semantic(context, &capabilities(context, support));
        let candidate = semantic
            .begin_candidate(basis, profile, support)
            .expect("candidate");
        semantic.publish(candidate, &inputs(context, config))
    }

    // -----------------------------------------------------------------
    // Axes stay separate (tests 1-3, 24, 25)
    // -----------------------------------------------------------------

    #[test]
    fn structural_current_and_semantic_dirty_coexist() {
        let fixture = Fixture::create("coexist");
        let semantic = fixture.semantic();
        let key = context().context_key();
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");

        semantic
            .mark_dirty(&key, CONFIG_CHANGED_CODE)
            .expect("dirty");

        // Semantic is not current...
        let status = semantic.status(&key).expect("status");
        assert_eq!(status.state, SemanticState::Dirty);
        // ...and the structural index is untouched and still readable.
        let resource = fixture.resource("src/app.py");
        assert_eq!(resource.resource_revision, "1");
        assert_eq!(
            crate::structural::read(semantic.connection(), resource.id)
                .expect("structural state")
                .map(|state| state.freshness_state),
            Some(FreshnessState::Current),
            "semantic going stale takes nothing structural with it"
        );
    }

    #[test]
    fn a_ready_runtime_does_not_make_semantics_current() {
        let fixture = Fixture::create("ready-not-current");
        let semantic = fixture.semantic();
        let key = context().context_key();
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");
        semantic.mark_dirty(&key, SOURCE_MOVED_CODE).expect("dirty");

        let (supervisor, _launcher) = supervisor();
        let lease = supervisor.acquire(&binding()).expect("start");

        assert_eq!(lease.state(), RuntimeState::Ready);
        assert_eq!(
            semantic.status(&key).expect("status").state,
            SemanticState::Dirty,
            "a running backend is not an up-to-date index"
        );
    }

    #[test]
    fn a_stopped_runtime_does_not_delete_a_current_publication() {
        let fixture = Fixture::create("stopped-keeps");
        let semantic = fixture.semantic();
        let key = context().context_key();
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");

        let (supervisor, _launcher) = supervisor();
        drop(supervisor.acquire(&binding()).expect("start"));
        supervisor.shutdown();

        assert_eq!(
            supervisor.state(&binding().context.context_key()),
            RuntimeState::Stopped
        );
        let status = semantic.status(&key).expect("status");
        assert!(status.is_current(), "a publication is persisted state");
        assert!(status.basis.is_some());
    }

    #[test]
    fn a_crashed_backend_leaves_structural_truth_and_last_valid_semantics() {
        let fixture = Fixture::create("crash-keeps");
        let semantic = fixture.semantic();
        let key = context().context_key();
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");

        // The backend is down: it cannot produce current truth, so the
        // semantic consequence is UNAVAILABLE -- and nothing else moves.
        assert!(!runtime_can_produce(RuntimeState::Backoff));
        assert!(!runtime_can_produce(RuntimeState::Degraded));
        semantic
            .mark_unavailable(&key, BACKEND_UNAVAILABLE_CODE)
            .expect("unavailable");

        let status = semantic.status(&key).expect("status");
        assert_eq!(status.state, SemanticState::Unavailable);
        assert!(
            status.has_last_valid(),
            "the last semantic answer is kept, it is just not current"
        );
        assert_eq!(status.support, Some(Support::Supported));
        assert_eq!(
            status.last_error_code.as_deref(),
            Some(BACKEND_UNAVAILABLE_CODE)
        );

        // Structural truth is entirely unaffected: the Resource is still
        // active at the revision it was, and its structural component
        // still reads CURRENT.
        let resource = fixture.resource("src/app.py");
        assert_eq!(resource.resource_revision, "1");
        assert_eq!(
            crate::structural::read(semantic.connection(), resource.id)
                .expect("structural state")
                .map(|state| state.freshness_state),
            Some(FreshnessState::Current),
            "a dead backend does not delete structural truth"
        );
    }

    #[test]
    fn a_crash_never_relabels_a_publication_current_once_its_basis_moved() {
        let fixture = Fixture::create("crash-not-current");
        let semantic = fixture.semantic();
        let key = context().context_key();
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");

        fixture.bump_revision("src/app.py", "2");
        semantic
            .mark_unavailable(&key, BACKEND_UNAVAILABLE_CODE)
            .expect("unavailable");

        let status = semantic
            .revalidate(&key, &inputs(&context(), &config()))
            .expect("revalidate");
        assert_ne!(status.state, SemanticState::Current);
        assert!(status.has_last_valid());
    }

    // -----------------------------------------------------------------
    // Profile identity and compatibility (tests 4-9)
    // -----------------------------------------------------------------

    #[test]
    fn the_same_inputs_produce_the_same_profile_identity() {
        let one =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));
        let two =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));

        assert_eq!(one.profile_key, two.profile_key);
        assert_eq!(one.analysis_mode, crate::symbol::ANALYSIS_MODE_SEMANTIC);
        assert_eq!(one.semantic_backend.as_deref(), Some("PYTHON"));
    }

    #[test]
    fn a_capability_change_changes_the_profile_and_a_patch_release_does_not() {
        let base =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));

        let weaker =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Partial));
        assert_ne!(
            base.profile_key, weaker.profile_key,
            "a different capability set is a different semantics"
        );

        let mut upgraded = context();
        upgraded.toolchain.backend_version = "1.0.1".to_owned();
        let patched =
            AnalysisProfile::semantic(&upgraded, &capabilities(&upgraded, Support::Supported));
        assert_eq!(
            base.profile_key, patched.profile_key,
            "a backend patch inside its compatibility class is not a rebuild"
        );
        assert_eq!(patched.semantic_backend_version.as_deref(), Some("1.0.1"));
    }

    fn stored_of(profile: &AnalysisProfile) -> StoredProfile {
        StoredProfile {
            profile_key: profile.profile_key.clone(),
            language: profile.language.to_string(),
            analysis_mode: profile.analysis_mode.to_owned(),
            semantic_backend: profile.semantic_backend.clone(),
            semantic_backend_version: profile.semantic_backend_version.clone(),
            extractor_semantics_version: profile.extractor_semantics_version.to_owned(),
            adapter_semantics_version: profile.adapter_semantics_version.to_owned(),
            backend_compatibility_class: profile.backend_compatibility_class.clone(),
            capability_fingerprint: profile.capability_fingerprint.clone(),
        }
    }

    #[test]
    fn compatibility_is_a_deterministic_verdict_over_what_changed() {
        let base =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));

        // Identical semantics, possibly a newer patch: COMPATIBLE.
        let mut upgraded = context();
        upgraded.toolchain.backend_version = "1.0.1".to_owned();
        let patched =
            AnalysisProfile::semantic(&upgraded, &capabilities(&upgraded, Support::Supported));
        assert_eq!(
            stored_of(&base).compatibility_with(&patched),
            BackendCompatibility::Compatible
        );
        assert!(BackendCompatibility::Compatible.keeps_publication());

        // Capability set moved: still comparable, must be re-checked.
        let weaker =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Partial));
        assert_eq!(
            stored_of(&base).compatibility_with(&weaker),
            BackendCompatibility::Revalidate
        );
        assert!(!BackendCompatibility::Revalidate.keeps_publication());

        // Different compatibility class: not comparable at all.
        let mut reclassed = context();
        reclassed.toolchain.backend_compatibility_class = "fake-python:2".to_owned();
        let rebuilt =
            AnalysisProfile::semantic(&reclassed, &capabilities(&reclassed, Support::Supported));
        assert_eq!(
            stored_of(&base).compatibility_with(&rebuilt),
            BackendCompatibility::Rebuild
        );
        assert!(BackendCompatibility::Rebuild.requires_rebuild());

        // A different backend family is likewise a rebuild.
        let mut rust = context();
        rust.backend = SemanticBackendKind::Rust;
        rust.language = ResourceLanguage::Rust;
        let other_backend =
            AnalysisProfile::semantic(&rust, &capabilities(&rust, Support::Supported));
        assert_eq!(
            stored_of(&other_backend).compatibility_with(&base),
            BackendCompatibility::Rebuild
        );

        // And UNKNOWN never keeps a publication current.
        assert!(!BackendCompatibility::Unknown.keeps_publication());
        assert!(!BackendCompatibility::Unknown.requires_rebuild());
    }

    #[test]
    fn an_unreadable_profile_fails_safe_toward_revalidation() {
        let fixture = Fixture::create("unknown-profile");
        let semantic = fixture.semantic();
        let key = context().context_key();
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");

        // The publication's profile row is unreadable -- a database
        // that was restored, hand-edited, or half-migrated. The verdict
        // is UNKNOWN, and UNKNOWN never reads as current.
        semantic
            .connection()
            .execute_batch("PRAGMA foreign_keys = OFF")
            .expect("relax integrity for the corruption this simulates");
        semantic
            .connection()
            .execute(
                "UPDATE semantic_publication SET analysis_profile_id = 9999 WHERE context_key = ?1",
                params![key],
            )
            .expect("repoint");

        let status = semantic
            .revalidate(&key, &inputs(&context(), &config()))
            .expect("revalidate");
        assert_eq!(status.state, SemanticState::Dirty);
        assert_eq!(
            status.last_error_code.as_deref(),
            Some(PROFILE_INCOMPATIBLE_CODE)
        );
    }

    #[test]
    fn a_revalidate_verdict_marks_the_scope_and_a_rebuild_invalidates_it() {
        let fixture = Fixture::create("revalidate-rebuild");
        let semantic = fixture.semantic();
        let key = context().context_key();
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");
        assert!(semantic.status(&key).expect("status").is_current());

        // REVALIDATE: the capability set moved.
        let weaker = CurrentInputs::new(
            &context(),
            &config(),
            &capabilities(&context(), Support::Partial),
        );
        let status = semantic.revalidate(&key, &weaker).expect("revalidate");
        assert_eq!(status.state, SemanticState::Dirty);
        assert!(
            status.has_last_valid(),
            "revalidation needed is not data loss"
        );

        // REBUILD: a different compatibility class.
        let mut reclassed = context();
        reclassed.toolchain.backend_compatibility_class = "fake-python:2".to_owned();
        let rebuilt = CurrentInputs::new(
            &reclassed,
            &config(),
            &capabilities(&reclassed, Support::Supported),
        );
        let status = semantic.revalidate(&key, &rebuilt).expect("revalidate");
        assert_eq!(status.state, SemanticState::Dirty);
        assert_eq!(
            status.last_error_code.as_deref(),
            Some(PROFILE_INCOMPATIBLE_CODE)
        );
    }

    #[test]
    fn a_compatible_profile_keeps_an_eligible_publication_current() {
        let fixture = Fixture::create("compatible-keeps");
        let semantic = fixture.semantic();
        let key = context().context_key();
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");
        semantic
            .mark_dirty(&key, PROFILE_INCOMPATIBLE_CODE)
            .expect("dirty");

        let mut upgraded = context();
        upgraded.toolchain.backend_version = "1.0.1".to_owned();
        // A patch release inside the class: the environment fingerprint
        // does move, so a publication bound to the old one is not
        // current -- but the *profile* verdict is COMPATIBLE.
        let stored = semantic
            .published_profile(&key)
            .expect("profile")
            .expect("published profile");
        let current =
            AnalysisProfile::semantic(&upgraded, &capabilities(&upgraded, Support::Supported));
        assert_eq!(
            stored.compatibility_with(&current),
            BackendCompatibility::Compatible
        );

        // With nothing else moved, revalidation restores CURRENT.
        let status = semantic
            .revalidate(&key, &inputs(&context(), &config()))
            .expect("revalidate");
        assert!(status.is_current());
    }

    // -----------------------------------------------------------------
    // Fingerprints (tests 10-13, 36)
    // -----------------------------------------------------------------

    #[test]
    fn a_config_fingerprint_is_order_independent_and_scoped() {
        let forwards = ConfigBasis::new()
            .with("pyproject.toml", "sha256:a")
            .with("requirements.txt", "sha256:b");
        let backwards = ConfigBasis::new()
            .with("requirements.txt", "sha256:b")
            .with("pyproject.toml", "sha256:a");

        assert_eq!(forwards.fingerprint(), backwards.fingerprint());
        assert_eq!(forwards.fingerprint(), forwards.fingerprint());
        assert_eq!(
            forwards.names().collect::<Vec<_>>(),
            vec!["pyproject.toml", "requirements.txt"]
        );

        // A relevant input changing changes it...
        let changed = ConfigBasis::new()
            .with("pyproject.toml", "sha256:CHANGED")
            .with("requirements.txt", "sha256:b");
        assert_ne!(changed.fingerprint(), forwards.fingerprint());

        // ...and a file nobody declared relevant does not, because it is
        // not an input at all. That is what keeps an unrelated edit from
        // invalidating this context.
        assert_eq!(
            forwards.fingerprint(),
            ConfigBasis::new()
                .with("pyproject.toml", "sha256:a")
                .with("requirements.txt", "sha256:b")
                .fingerprint()
        );
    }

    #[test]
    fn environment_identity_participates_in_the_basis() {
        let basis = SemanticBasis::new(&context(), &config());
        let mut other_environment = context();
        other_environment.toolchain.environment_fingerprint = "sha256:other-venv".to_owned();
        let other = SemanticBasis::new(&other_environment, &config());

        assert_ne!(basis.environment_fingerprint, other.environment_fingerprint);
        assert_ne!(basis.fingerprint(), other.fingerprint());
    }

    #[test]
    fn a_basis_fingerprint_is_deterministic_over_its_whole_dependency_set() {
        let one = ResourceId::from_bytes([3; 16]);
        let two = ResourceId::from_bytes([4; 16]);

        let forwards = SemanticBasis::new(&context(), &config())
            .with_source(one, "1")
            .with_source(two, "7");
        let backwards = SemanticBasis::new(&context(), &config())
            .with_source(two, "7")
            .with_source(one, "1");
        assert_eq!(forwards.fingerprint(), backwards.fingerprint());
        assert_eq!(forwards.fingerprint(), forwards.fingerprint());

        let moved = SemanticBasis::new(&context(), &config())
            .with_source(one, "2")
            .with_source(two, "7");
        assert_ne!(moved.fingerprint(), forwards.fingerprint());

        let project_wide = forwards.clone().with_inventory("sha256:modules");
        assert_ne!(project_wide.fingerprint(), forwards.fingerprint());
    }

    // -----------------------------------------------------------------
    // Candidate / publication (tests 14-19, 22, 26, 35)
    // -----------------------------------------------------------------

    #[test]
    fn a_candidate_is_not_current_and_publishing_makes_it_so() {
        let fixture = Fixture::create("candidate");
        let semantic = fixture.semantic();
        let key = context().context_key();
        let resource = fixture.resource("src/app.py");

        let basis = SemanticBasis::new(&context(), &config())
            .with_source(resource.id, resource.resource_revision.clone());
        let profile =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));
        let candidate = semantic
            .begin_candidate(basis, profile, Support::Supported)
            .expect("candidate");

        assert_eq!(candidate.state(), SemanticState::Building);
        assert_eq!(
            semantic.status(&key).expect("status").state,
            SemanticState::None,
            "a candidate publishes nothing a reader can see"
        );

        let published = semantic
            .publish(candidate, &inputs(&context(), &config()))
            .expect("publish");
        let status = semantic.status(&key).expect("status");
        assert!(status.is_current());
        assert_eq!(status.stable_generation_id, Some(published.generation_id));
        assert_eq!(status.support, Some(Support::Supported));
    }

    #[test]
    fn a_building_candidate_is_invisible_to_a_reader_until_it_commits() {
        let fixture = Fixture::create("building-invisible");
        let semantic = fixture.semantic();
        let key = context().context_key();

        let first = publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish N");

        // A second reader, on its own connection, sees N.
        let reader = fixture.semantic();
        assert_eq!(
            reader.status(&key).expect("status").stable_generation_id,
            Some(first.generation_id)
        );

        // N+1 begins building.
        let resource = fixture.resource("src/app.py");
        let basis = SemanticBasis::new(&context(), &config())
            .with_source(resource.id, resource.resource_revision.clone());
        let profile =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));
        let candidate = semantic
            .begin_candidate(basis, profile, Support::Supported)
            .expect("candidate");
        let candidate_generation = candidate.generation_id();
        assert_ne!(candidate_generation, first.generation_id);

        // During the build the reader still sees N, never a mixture.
        let during = reader.status(&key).expect("status");
        assert_eq!(during.stable_generation_id, Some(first.generation_id));
        assert!(during.is_current());

        let second = semantic
            .publish(candidate, &inputs(&context(), &config()))
            .expect("publish N+1");
        assert_eq!(second.generation_id, candidate_generation);

        let after = reader.status(&key).expect("status");
        assert_eq!(after.stable_generation_id, Some(second.generation_id));
        assert!(after.is_current());
    }

    #[test]
    fn a_source_revision_moving_before_publication_rejects_the_candidate() {
        let fixture = Fixture::create("source-moved");
        let semantic = fixture.semantic();
        let key = context().context_key();
        let resource = fixture.resource("src/app.py");

        let basis = SemanticBasis::new(&context(), &config())
            .with_source(resource.id, resource.resource_revision.clone());
        let profile =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));
        let candidate = semantic
            .begin_candidate(basis, profile, Support::Supported)
            .expect("candidate");

        // The file changes while the backend is thinking. The answer
        // that arrives describes a revision that no longer exists --
        // late, obsolete, and not publishable, whether or not the
        // cancellation ever reached the backend.
        fixture.bump_revision("src/app.py", "2");

        match semantic.publish(candidate, &inputs(&context(), &config())) {
            Err(SemanticIndexError::Obsolete {
                reason: ObsoleteReason::SourceRevisionMoved { basis, current, .. },
                ..
            }) => {
                assert_eq!(basis, "1");
                assert_eq!(current, "2");
            }
            other => panic!("expected an obsolete candidate, got {other:?}"),
        }
        assert_eq!(
            semantic.status(&key).expect("status").state,
            SemanticState::None,
            "a rejected candidate publishes nothing"
        );
    }

    #[test]
    fn a_config_or_environment_change_before_publication_rejects_the_candidate() {
        let fixture = Fixture::create("config-moved");
        let semantic = fixture.semantic();
        let resource = fixture.resource("src/app.py");

        let make_candidate = || {
            let basis = SemanticBasis::new(&context(), &config())
                .with_source(resource.id, resource.resource_revision.clone());
            let profile = AnalysisProfile::semantic(
                &context(),
                &capabilities(&context(), Support::Supported),
            );
            semantic
                .begin_candidate(basis, profile, Support::Supported)
                .expect("candidate")
        };

        let moved_config = ConfigBasis::new().with("pyproject.toml", "sha256:config-b");
        match semantic.publish(make_candidate(), &inputs(&context(), &moved_config)) {
            Err(SemanticIndexError::Obsolete {
                reason: ObsoleteReason::ConfigChanged { .. },
                ..
            }) => {}
            other => panic!("expected a config obsolescence, got {other:?}"),
        }

        let mut other_environment = context();
        other_environment.toolchain.environment_fingerprint = "sha256:other-venv".to_owned();
        match semantic.publish(make_candidate(), &inputs(&other_environment, &config())) {
            Err(SemanticIndexError::Obsolete {
                reason: ObsoleteReason::EnvironmentChanged { .. },
                ..
            }) => {}
            other => panic!("expected an environment obsolescence, got {other:?}"),
        }

        let project_wide = inputs(&context(), &config()).with_inventory("sha256:modules");
        match semantic.publish(make_candidate(), &project_wide) {
            Err(SemanticIndexError::Obsolete {
                reason: ObsoleteReason::InventoryChanged,
                ..
            }) => {}
            other => panic!("expected an inventory obsolescence, got {other:?}"),
        }
    }

    #[test]
    fn a_workspace_revision_moving_before_publication_rejects_the_candidate() {
        let fixture = Fixture::create("workspace-moved");
        let semantic = fixture.semantic();
        let resource = fixture.resource("src/app.py");

        let basis = SemanticBasis::new(&context(), &config())
            .with_source(resource.id, resource.resource_revision.clone());
        let profile =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));
        let candidate = semantic
            .begin_candidate(basis, profile, Support::Supported)
            .expect("candidate");

        fixture.advance_workspace_revision("workspace-rev-2");

        match semantic.publish(candidate, &inputs(&context(), &config())) {
            Err(SemanticIndexError::Obsolete {
                reason: ObsoleteReason::WorkspaceRevisionMoved { .. },
                ..
            }) => {}
            other => panic!("expected a workspace revision obsolescence, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_analysis_never_replaces_a_publication_with_an_empty_success() {
        let fixture = Fixture::create("failure-keeps");
        let semantic = fixture.semantic();
        let key = context().context_key();
        let first = publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py", "src/other.py"],
        )
        .expect("publish");

        // The backend failed. The candidate is discarded, and N stays
        // exactly as it was -- same generation, same dependency set.
        let resource = fixture.resource("src/app.py");
        let basis = SemanticBasis::new(&context(), &config())
            .with_source(resource.id, resource.resource_revision.clone());
        let profile =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));
        let candidate = semantic
            .begin_candidate(basis, profile, Support::Supported)
            .expect("candidate");
        semantic
            .discard(candidate, "backend crashed")
            .expect("discard");

        let status = semantic.status(&key).expect("status");
        assert!(status.is_current());
        assert_eq!(status.stable_generation_id, Some(first.generation_id));
        assert_eq!(
            status.basis.expect("basis").sources.len(),
            2,
            "the previous result set is intact, not half-replaced"
        );
    }

    #[test]
    fn a_coherent_partial_result_publishes_as_partial() {
        let fixture = Fixture::create("partial");
        let semantic = fixture.semantic();
        let key = context().context_key();

        // The source has type errors; the backend still understood most
        // of it. That is a partial answer, not an infrastructure
        // failure, and refusing to publish it would leave the project
        // with no semantics at all.
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Partial,
            &["src/app.py"],
        )
        .expect("publish");

        let status = semantic.status(&key).expect("status");
        assert!(status.is_current(), "partial coverage is still current");
        assert_eq!(status.support, Some(Support::Partial));

        // Infrastructure failure is a different state entirely.
        semantic
            .mark_unavailable(&key, BACKEND_UNAVAILABLE_CODE)
            .expect("unavailable");
        let broken = semantic.status(&key).expect("status");
        assert_eq!(broken.state, SemanticState::Unavailable);
        assert_eq!(
            broken.support,
            Some(Support::Partial),
            "the last answer still describes its own coverage honestly"
        );
    }

    /// Row counts for every canonical graph table task 4 will own.
    fn counts(semantic: &SemanticIndex) -> Vec<(&'static str, i64)> {
        [
            "relation",
            "occurrence",
            "unresolved_reference",
            "relation_candidate",
            "graph_entity",
        ]
        .into_iter()
        .map(|table| {
            let count: i64 = semantic
                .connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count");
            (table, count)
        })
        .collect()
    }

    #[test]
    fn publishing_semantics_creates_no_relation_rows() {
        let fixture = Fixture::create("no-relations");
        let semantic = fixture.semantic();
        let before: i64 = semantic
            .connection()
            .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))
            .expect("count");
        let structural = counts(&semantic);

        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");

        let after: i64 = semantic
            .connection()
            .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))
            .expect("count");
        assert_eq!(before, after, "task 3 publishes freshness, not edges");
        assert_eq!(
            counts(&semantic),
            structural,
            "publishing semantics leaves every canonical graph table exactly as it was"
        );
    }

    // -----------------------------------------------------------------
    // Obsolete/late results and targeted invalidation (tests 20, 21, 23, 29)
    // -----------------------------------------------------------------

    #[test]
    fn a_late_response_from_a_superseded_basis_cannot_publish() {
        let fixture = Fixture::create("late-response");
        let semantic = fixture.semantic();
        let key = context().context_key();
        let resource = fixture.resource("src/app.py");

        // A request begins at revision 1 and the runtime is asked to
        // cancel it. Suppose the cancel never lands -- the backend has
        // no such capability, or the answer was already on the wire.
        let basis = SemanticBasis::new(&context(), &config())
            .with_source(resource.id, resource.resource_revision.clone());
        let profile =
            AnalysisProfile::semantic(&context(), &capabilities(&context(), Support::Supported));
        let candidate = semantic
            .begin_candidate(basis, profile, Support::Supported)
            .expect("candidate");
        let token = crate::runtime::CancelToken::new();
        token.cancel();
        assert!(token.is_cancelled());

        fixture.bump_revision("src/app.py", "2");

        // The late answer arrives anyway. It is refused here, which is
        // why correctness never depended on the cancel landing.
        assert!(matches!(
            semantic.publish(candidate, &inputs(&context(), &config())),
            Err(SemanticIndexError::Obsolete { .. })
        ));
        assert_eq!(
            semantic.status(&key).expect("status").state,
            SemanticState::None
        );
    }

    #[test]
    fn invalidation_is_scoped_to_what_a_publication_actually_read() {
        let fixture = Fixture::create("scoped-invalidation");
        let semantic = fixture.semantic();

        let reader = context();
        let mut other = context_with([1; 16], [5; 16], "sha256:venv");
        other.language = ResourceLanguage::TypeScript;
        other.backend = SemanticBackendKind::TypeScriptJavaScript;

        publish(
            &fixture,
            &semantic,
            &reader,
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish A");
        publish(
            &fixture,
            &semantic,
            &other,
            &config(),
            Support::Supported,
            &["src/other.py"],
        )
        .expect("publish B");

        let touched = fixture.resource("src/app.py");
        let dirtied = semantic
            .invalidate_resource(touched.id)
            .expect("invalidate");
        assert_eq!(dirtied, vec![reader.context_key()]);

        assert_eq!(
            semantic
                .status(&reader.context_key())
                .expect("status")
                .state,
            SemanticState::Dirty
        );
        assert!(
            semantic
                .status(&other.context_key())
                .expect("status")
                .is_current(),
            "a context that never read the file is untouched"
        );
    }

    #[test]
    fn a_dirty_context_keeps_its_last_valid_answer() {
        let fixture = Fixture::create("last-valid");
        let semantic = fixture.semantic();
        let key = context().context_key();
        let published = publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");

        semantic.mark_dirty(&key, SOURCE_MOVED_CODE).expect("dirty");

        let status = semantic.status(&key).expect("status");
        assert_eq!(status.state, SemanticState::Dirty);
        assert_eq!(status.stable_generation_id, Some(published.generation_id));
        assert_eq!(
            status.basis.expect("basis").fingerprint(),
            published.basis.fingerprint(),
            "last known and current are different claims about the same data"
        );
    }

    // -----------------------------------------------------------------
    // Isolation (test 28)
    // -----------------------------------------------------------------

    #[test]
    fn one_workspaces_publication_cannot_touch_another() {
        let fixture = Fixture::create("workspace-isolation");
        let semantic = fixture.semantic();

        let main = context();
        let worktree = context_with([9; 16], [2; 16], "sha256:venv");
        assert_ne!(main.context_key(), worktree.context_key());

        publish(
            &fixture,
            &semantic,
            &main,
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish main");

        // The other worktree's basis names its own WorkspaceId, which is
        // not this index's: it cannot publish here at all.
        let resource = fixture.resource("src/app.py");
        let basis = SemanticBasis::new(&worktree, &config())
            .with_source(resource.id, resource.resource_revision.clone());
        let profile =
            AnalysisProfile::semantic(&worktree, &capabilities(&worktree, Support::Supported));
        let candidate = semantic
            .begin_candidate(basis, profile, Support::Supported)
            .expect("candidate");
        match semantic.publish(candidate, &inputs(&main, &config())) {
            Err(SemanticIndexError::Obsolete {
                reason: ObsoleteReason::WorkspaceMismatch { .. },
                ..
            }) => {}
            other => panic!("expected a workspace mismatch, got {other:?}"),
        }

        assert!(
            semantic
                .status(&main.context_key())
                .expect("status")
                .is_current(),
            "the other worktree changed nothing here"
        );
        assert_eq!(
            semantic
                .status(&worktree.context_key())
                .expect("status")
                .state,
            SemanticState::None
        );
    }

    // -----------------------------------------------------------------
    // Restart and persistence (tests 30-34)
    // -----------------------------------------------------------------

    #[test]
    fn a_reopened_index_recovers_its_publication_metadata() {
        let fixture = Fixture::create("reopen");
        let key = context().context_key();
        let published = {
            let semantic = fixture.semantic();
            publish(
                &fixture,
                &semantic,
                &context(),
                &config(),
                Support::Supported,
                &["src/app.py"],
            )
            .expect("publish")
        };

        // The daemon is gone; every runtime with it. What is on disk is
        // metadata, and it comes back.
        let restarted = fixture.semantic();
        let status = restarted.status(&key).expect("status");
        assert_eq!(status.stable_generation_id, Some(published.generation_id));
        let basis = status.basis.expect("persisted basis");
        assert_eq!(basis.fingerprint(), published.basis.fingerprint());
        assert_eq!(basis.sources.len(), 1);
    }

    #[test]
    fn a_recovered_publication_is_current_only_once_its_basis_validates() {
        let fixture = Fixture::create("recover-validate");
        let key = context().context_key();
        {
            let semantic = fixture.semantic();
            publish(
                &fixture,
                &semantic,
                &context(),
                &config(),
                Support::Supported,
                &["src/app.py"],
            )
            .expect("publish");
        }

        // The world moved while nothing was watching.
        fixture.bump_revision("src/app.py", "2");

        let restarted = fixture.semantic();
        let status = restarted
            .revalidate(&key, &inputs(&context(), &config()))
            .expect("revalidate");
        assert_eq!(status.state, SemanticState::Dirty);
        assert_eq!(status.last_error_code.as_deref(), Some(SOURCE_MOVED_CODE));
        assert!(status.has_last_valid());

        // Nothing moved for the other scenario: revalidation confirms it
        // without any backend existing anywhere.
        let untouched = Fixture::create("recover-ok");
        {
            let semantic = untouched.semantic();
            publish(
                &untouched,
                &semantic,
                &context(),
                &config(),
                Support::Supported,
                &["src/app.py"],
            )
            .expect("publish");
        }
        let restarted = untouched.semantic();
        assert!(
            restarted
                .revalidate(&key, &inputs(&context(), &config()))
                .expect("revalidate")
                .is_current()
        );
    }

    #[test]
    fn a_publication_whose_workspace_revision_moved_never_reads_current() {
        let fixture = Fixture::create("clock-moved");
        let key = context().context_key();
        {
            let semantic = fixture.semantic();
            publish(
                &fixture,
                &semantic,
                &context(),
                &config(),
                Support::Supported,
                &["src/app.py"],
            )
            .expect("publish");
        }

        fixture.advance_workspace_revision("workspace-rev-2");

        // Nothing marked it dirty -- the daemon was not even running.
        // It still must not claim to be current.
        let restarted = fixture.semantic();
        let status = restarted.status(&key).expect("status");
        assert_eq!(status.state, SemanticState::Dirty);
        assert!(status.has_last_valid());
    }

    #[test]
    fn nothing_process_local_or_raw_is_persisted() {
        let fixture = Fixture::create("persistence-boundary");
        let semantic = fixture.semantic();
        publish(
            &fixture,
            &semantic,
            &context(),
            &config(),
            Support::Supported,
            &["src/app.py"],
        )
        .expect("publish");

        // Everything stored for a publication, as text.
        let mut statement = semantic
            .connection()
            .prepare(
                "SELECT context_key, basis_fingerprint, config_fingerprint, \
                        environment_fingerprint, COALESCE(inventory_fingerprint, ''), support \
                 FROM semantic_publication",
            )
            .expect("prepare");
        let rows: Vec<String> = statement
            .query_map([], |row| {
                Ok([
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ]
                .join("|"))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        drop(statement);

        assert_eq!(rows.len(), 1);
        let stored = &rows[0];
        // No source body, no backend response, no process-local handle.
        for forbidden in ["def go", "return 1", "RequestId", "pid", "/tmp/"] {
            assert!(
                !stored.contains(forbidden),
                "{forbidden:?} must never be persisted as semantic identity: {stored}"
            );
        }
        // What is stored is fingerprints and a closed vocabulary.
        assert!(stored.contains("semantic-basis-1:"));
        assert!(stored.contains("SUPPORTED"));
    }

    // -----------------------------------------------------------------
    // The runtime's own fake, reused so the two axes can be crossed.
    // -----------------------------------------------------------------

    struct SilentHost;

    impl SemanticRuntimeHost for SilentHost {
        fn execute(
            &self,
            _request: &RuntimeRequest,
            _cancel: &crate::runtime::CancelToken,
        ) -> Result<RuntimeResponse, HostError> {
            Ok(RuntimeResponse {
                payload: Vec::new(),
            })
        }

        fn health(&self) -> HostHealth {
            HostHealth::Healthy
        }

        fn shutdown(&self) {}
    }

    struct SilentLauncher;

    impl SemanticBackendLauncher for SilentLauncher {
        fn kind(&self) -> SemanticBackendKind {
            SemanticBackendKind::Python
        }

        fn launch(
            &self,
            _binding: &AnalysisContextBinding,
        ) -> Result<Arc<dyn SemanticRuntimeHost>, HostError> {
            Ok(Arc::new(SilentHost))
        }
    }

    fn binding() -> AnalysisContextBinding {
        AnalysisContextBinding {
            context: context(),
            project_root_rel: "src".to_owned(),
            config_file_rel: Some("pyproject.toml".to_owned()),
        }
    }

    fn supervisor() -> (SemanticRuntimeSupervisor, Arc<SilentLauncher>) {
        let launcher = Arc::new(SilentLauncher);
        let supervisor = SemanticRuntimeSupervisor::new(RuntimePolicy::default())
            .with_backend(Arc::clone(&launcher) as Arc<dyn SemanticBackendLauncher>);
        (supervisor, launcher)
    }
}
