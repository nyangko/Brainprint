//! Relation evidence joined to current source (#17 task 9).
//!
//! Task 8 answers *where* an edge is proven: owner Resource, exact byte
//! span, line and column. An Agent handed that still has to open the
//! file -- which is the `rg`/`read` loop I3 exists to end. This module
//! closes it: every confirmed evidence location, and the anchor's own
//! declaration, come back with the **current source** already read and
//! verified through I2's contract ([`SourceReader`]).
//!
//! ## Current source, or none
//!
//! Source never comes from `index.db`. It is read from the filesystem,
//! hashed, and compared to the Resource's persisted `content_hash`
//! before a single byte is sliced (#16 task 11). A stored span is a
//! locator, not text.
//!
//! When the current bytes cannot be made to correspond to the stored
//! evidence -- the Resource moved past the revision the evidence was
//! extracted from, the file changed under the index, the Symbol row
//! describes an older revision -- nothing is sliced. The result says
//! [`SourceUnavailable`] instead, because convincing wrong source is
//! worse than none.
//!
//! ## What is prepared
//!
//! - the exact evidence span, always, and it stays authoritative;
//! - the **containing declaration** when the evidence has one, so a call
//!   site arrives with the function it sits in, ready to edit;
//! - the anchor Symbol's own current declaration.
//!
//! Never a whole file, and never a Symbol range invented for a Resource,
//! external package, or domain entity: an external dependency's
//! definition source is not in this Workspace, and Brainprint does not
//! pretend otherwise.
//!
//! ## One read per file
//!
//! Ranges are deduplicated by `(Resource, span)` and grouped by
//! Resource, so a dozen call sites in one file are one verified read
//! ([`SourceReader::read_ranges`]) and one entry per distinct range.
//! Every relation and every piece of evidence keeps a [`RangeId`]
//! pointing at what supports it -- the caller never has to work out
//! which range belongs to which edge.
//!
//! ## What this is not
//!
//! A preparation path, not a projection. Token budgeting, task packets,
//! role/persona shaping, Working State, and the MCP surface are I5.
//! Traversal is task 10 and related tests are task 11; this prepares one
//! direct query's worth of source and stops.

use std::{collections::HashMap, error::Error, fmt, path::Path};

use brainprint_core::ResourceId;

use crate::{
    graph::{GraphEndpoint, RelationKind},
    inspect::{ReadError, SourceReader, SourceVerification},
    parser::SourceSpan,
    query::{Currentness, QueryError},
    relations::{
        Coverage, Direction, EvidenceLocation, RelationAnswer, RelationError, RelationGap,
        RelationIndex, RelationResult,
    },
    symbol::Symbol,
};

/// Where a prepared range sits in [`PreparedInspection::ranges`].
///
/// An index into this one result, so that evidence can point at the
/// source supporting it without repeating the text. Not an identity and
/// not a storage id: it means nothing outside the result that produced
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RangeId(pub usize);

/// Why a prepared range exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeRole {
    /// The exact evidence span -- the authoritative one.
    EvidenceSpan,
    /// The declaration lexically containing an evidence span, so the
    /// evidence can be understood and edited in place.
    ContainingDeclaration,
    /// The query anchor's own current declaration.
    AnchorDeclaration,
}

/// One verified slice of current source, read once and shared by
/// everything that needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRange {
    pub resource: ResourceId,
    /// The Resource's current Workspace-relative path -- the path that
    /// was actually read.
    pub path_rel: String,
    /// The revision the read was verified against.
    pub resource_revision: String,
    pub span: SourceSpan,
    /// The current source of that span. Never from `index.db`.
    pub source: String,
    /// Why this range was prepared. The first reason wins when one
    /// range serves two purposes.
    pub role: RangeRole,
    pub verification: SourceVerification,
}

/// Why no current source could honestly be returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceUnavailable {
    /// The Resource has moved past the revision this evidence was
    /// extracted from. The span describes source that is no longer
    /// there.
    StaleBasis {
        basis_revision: String,
        current_revision: String,
    },
    /// The file's current bytes do not hash to what was indexed.
    /// `RESOURCE_INDEX` has been marked DIRTY by the read attempt.
    SourceChanged {
        expected_content_hash: String,
        observed_content_hash: String,
    },
    /// The Symbol row describes an older revision of its Resource, so
    /// its declaration span is not sliceable.
    SymbolNotCurrent {
        symbol_revision: String,
        resource_revision: String,
    },
    /// The Resource has no readable current source at all: deleted, not
    /// a file, unverifiable, or unreadable.
    NoCurrentSource { detail: String },
    /// The span does not describe the current bytes.
    SpanNotReadable { detail: String },
}

/// One evidence location with the current source prepared for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedEvidence {
    /// Task 8's location, unchanged: owner, containing Symbol,
    /// Occurrence kind, exact span, basis revision, support, freshness.
    pub location: EvidenceLocation,
    /// The exact evidence span's current source.
    pub evidence_range: Option<RangeId>,
    /// The containing declaration's current source, when the evidence
    /// sits inside a known one.
    pub containing_range: Option<RangeId>,
    /// Why no source was prepared, when none was.
    pub unavailable: Option<SourceUnavailable>,
}

/// One confirmed relation with its evidence prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRelation {
    /// Task 8's result, unchanged.
    pub relation: RelationResult,
    pub evidence: Vec<PreparedEvidence>,
}

/// The query anchor's own current declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTarget {
    pub symbol: Symbol,
    pub path_rel: String,
    pub range: Option<RangeId>,
    pub unavailable: Option<SourceUnavailable>,
}

/// One direct relation query, with the source an Agent would otherwise
/// go and read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedInspection {
    pub anchor: GraphEndpoint,
    pub direction: Direction,
    /// The anchor's current declaration source, when the anchor is a
    /// Symbol with one.
    ///
    /// `None` for a Resource, an external package, or a domain entity:
    /// a file is not a declaration, and an external dependency's
    /// definition source is not in this Workspace. Nothing is
    /// fabricated to fill the slot.
    pub target: Option<PreparedTarget>,
    pub relations: Vec<PreparedRelation>,
    /// Every distinct range read, in deterministic order.
    pub ranges: Vec<PreparedRange>,
    /// Task 8's gaps, unchanged: unresolved evidence and candidates.
    pub gaps: Vec<RelationGap>,
    pub coverage: Coverage,
    /// Whether the structural index around all of this may be claimed
    /// current. Separate from the byte-level verification on each range.
    pub currentness: Currentness,
}

impl PreparedInspection {
    /// The range behind an id.
    #[must_use]
    pub fn range(&self, id: RangeId) -> Option<&PreparedRange> {
        self.ranges.get(id.0)
    }

    /// How many confirmed relations were prepared.
    #[must_use]
    pub fn confirmed_count(&self) -> usize {
        self.relations.len()
    }

    /// Whether every confirmed evidence location came back with its
    /// exact current source. False when anything was stale, changed, or
    /// unreadable -- which is also when the prepared set is not the
    /// whole picture.
    #[must_use]
    pub fn source_complete(&self) -> bool {
        self.relations.iter().all(|relation| {
            relation
                .evidence
                .iter()
                .all(|evidence| evidence.evidence_range.is_some())
        })
    }
}

/// Failure preparing an inspection.
#[derive(Debug)]
pub enum PrepareError {
    Relation(RelationError),
    Read(ReadError),
    Query(QueryError),
}

impl fmt::Display for PrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Relation(error) => write!(formatter, "relation query: {error}"),
            Self::Read(error) => write!(formatter, "current source read: {error}"),
            Self::Query(error) => write!(formatter, "index query: {error}"),
        }
    }
}

impl Error for PrepareError {}

impl From<RelationError> for PrepareError {
    fn from(error: RelationError) -> Self {
        Self::Relation(error)
    }
}

impl From<ReadError> for PrepareError {
    fn from(error: ReadError) -> Self {
        Self::Read(error)
    }
}

impl From<QueryError> for PrepareError {
    fn from(error: QueryError) -> Self {
        Self::Query(error)
    }
}

/// Direct relation queries, answered with current source attached.
pub struct InspectPreparer {
    relations: RelationIndex,
    reader: SourceReader,
}

impl InspectPreparer {
    /// Open the `index.db` at `index_path` for the Workspace rooted at
    /// `workspace_root`.
    pub fn open(index_path: &Path, workspace_root: &Path) -> Result<Self, PrepareError> {
        Ok(Self::new(
            RelationIndex::open(index_path)?,
            SourceReader::open(index_path, workspace_root)?,
        ))
    }

    /// Prepare against an already-opened index and reader.
    #[must_use]
    pub fn new(relations: RelationIndex, reader: SourceReader) -> Self {
        Self { relations, reader }
    }

    /// The relation query surface these preparations are built on.
    #[must_use]
    pub const fn relations(&self) -> &RelationIndex {
        &self.relations
    }

    /// Run one direct relation query and prepare its source.
    pub fn prepare(
        &self,
        anchor: &GraphEndpoint,
        direction: Direction,
        kinds: &[RelationKind],
    ) -> Result<PreparedInspection, PrepareError> {
        let answer = match direction {
            Direction::Outgoing => self.relations.outgoing(anchor, kinds)?,
            Direction::Incoming => self.relations.incoming(anchor, kinds)?,
        };
        self.prepare_answer(anchor, &answer)
    }

    /// Prepare the source for an answer some other task-8 query already
    /// produced.
    ///
    /// The answer's confirmed relations, gaps, and coverage pass through
    /// unchanged -- this only adds source.
    pub fn prepare_answer(
        &self,
        anchor: &GraphEndpoint,
        answer: &RelationAnswer,
    ) -> Result<PreparedInspection, PrepareError> {
        let mut plan = ReadPlan::default();

        // The anchor's own declaration first, so it is range 0 whenever
        // there is one.
        let target = self.plan_target(anchor, &mut plan)?;

        let mut planned: Vec<Vec<PlannedEvidence>> = Vec::new();
        for relation in &answer.confirmed {
            let mut items = Vec::new();
            for location in &relation.evidence {
                items.push(self.plan_evidence(location, &mut plan)?);
            }
            planned.push(items);
        }

        let outcome = self.execute(plan)?;

        let relations = answer
            .confirmed
            .iter()
            .zip(planned)
            .map(|(relation, items)| PreparedRelation {
                relation: relation.clone(),
                evidence: items
                    .into_iter()
                    .map(|item| PreparedEvidence {
                        location: item.location,
                        evidence_range: outcome.resolve(item.evidence),
                        containing_range: item.containing.and_then(|id| outcome.resolve(Some(id))),
                        unavailable: item
                            .unavailable
                            .or_else(|| outcome.unavailable(item.evidence)),
                    })
                    .collect(),
            })
            .collect();

        Ok(PreparedInspection {
            anchor: anchor.clone(),
            direction: answer.direction,
            target: target.map(|target| PreparedTarget {
                symbol: target.symbol,
                path_rel: target.path_rel,
                range: outcome.resolve(target.range),
                unavailable: target
                    .unavailable
                    .or_else(|| outcome.unavailable(target.range)),
            }),
            relations,
            ranges: outcome.ranges,
            gaps: answer.gaps.clone(),
            coverage: answer.coverage,
            // The byte-level verification on each range and the
            // currentness of the structure around it are two claims.
            currentness: self.reader.index().currentness()?,
        })
    }

    /// The anchor's own declaration, when the anchor is a Symbol that
    /// still describes its Resource's current revision.
    fn plan_target(
        &self,
        anchor: &GraphEndpoint,
        plan: &mut ReadPlan,
    ) -> Result<Option<PlannedTarget>, PrepareError> {
        let GraphEndpoint::Symbol(symbol_id) = anchor else {
            // A Resource is not a declaration, and an external package's
            // source is not in this Workspace. Nothing is fabricated.
            return Ok(None);
        };
        let metadata = match self.reader.inspect_symbol_metadata(*symbol_id) {
            Ok(metadata) => metadata,
            Err(ReadError::Query(error)) => return Err(PrepareError::Query(error)),
            Err(ReadError::Sqlite(error)) => {
                return Err(PrepareError::Query(QueryError::Sqlite(error)));
            }
            // A Symbol the reader cannot even describe -- unknown, or
            // owned by a Resource with no current source -- has no
            // declaration to prepare. The relations still stand.
            Err(_) => return Ok(None),
        };
        if !metadata.is_current {
            return Ok(Some(PlannedTarget {
                path_rel: metadata.path_rel,
                range: None,
                unavailable: Some(SourceUnavailable::SymbolNotCurrent {
                    symbol_revision: metadata.symbol.resource_revision.clone(),
                    resource_revision: metadata.resource_revision,
                }),
                symbol: metadata.symbol,
            }));
        }
        let range = plan.request(
            metadata.symbol.resource_id,
            &metadata.symbol.resource_revision,
            metadata.symbol.span,
            RangeRole::AnchorDeclaration,
        );
        Ok(Some(PlannedTarget {
            path_rel: metadata.path_rel,
            range: Some(range),
            unavailable: None,
            symbol: metadata.symbol,
        }))
    }

    /// One evidence location: its exact span, and the declaration
    /// containing it when there is one.
    fn plan_evidence(
        &self,
        location: &EvidenceLocation,
        plan: &mut ReadPlan,
    ) -> Result<PlannedEvidence, PrepareError> {
        let evidence = plan.request(
            location.resource,
            &location.basis_revision,
            location.span,
            RangeRole::EvidenceSpan,
        );
        let mut containing = None;
        if let Some(symbol_id) = location.containing_symbol {
            match self.reader.inspect_symbol_metadata(symbol_id) {
                // The bounded wider range: the declaration the evidence
                // sits in, never the file around it.
                Ok(metadata) if metadata.is_current => {
                    containing = Some(plan.request(
                        metadata.symbol.resource_id,
                        &metadata.symbol.resource_revision,
                        metadata.symbol.span,
                        RangeRole::ContainingDeclaration,
                    ));
                }
                // A container that cannot be read does not withhold the
                // evidence span itself, which is the authoritative one.
                Ok(_) | Err(ReadError::UnknownSymbol { .. }) => {}
                Err(ReadError::Query(error)) => return Err(PrepareError::Query(error)),
                Err(ReadError::Sqlite(error)) => {
                    return Err(PrepareError::Query(QueryError::Sqlite(error)));
                }
                Err(_) => {}
            }
        }
        Ok(PlannedEvidence {
            location: location.clone(),
            evidence: Some(evidence),
            containing,
            unavailable: None,
        })
    }

    /// One verified read per Resource, in plan order.
    fn execute(&self, plan: ReadPlan) -> Result<ReadOutcome, PrepareError> {
        let mut outcome = ReadOutcome {
            ranges: Vec::new(),
            slot: vec![None; plan.requests.len()],
            failure: vec![None; plan.requests.len()],
        };
        for group in plan.groups() {
            let spans: Vec<SourceSpan> = group
                .iter()
                .map(|index| plan.requests[*index].span)
                .collect();
            let first = &plan.requests[group[0]];
            match self
                .reader
                .read_ranges(first.resource, &first.revision, &spans)
            {
                Ok(reads) => {
                    for (index, read) in group.iter().zip(reads) {
                        outcome.slot[*index] = Some(RangeId(outcome.ranges.len()));
                        outcome.ranges.push(PreparedRange {
                            resource: read.resource_id,
                            path_rel: read.path_rel,
                            resource_revision: read.resource_revision,
                            span: read.effective_span,
                            source: read.source,
                            role: plan.requests[*index].role,
                            verification: read.verification,
                        });
                    }
                }
                Err(error) => {
                    // The whole group shares one file and one revision,
                    // so one failure is the same failure for all of it.
                    let reason = unavailable_from(error)?;
                    for index in group {
                        outcome.failure[index] = Some(reason.clone());
                    }
                }
            }
        }
        Ok(outcome)
    }
}

/// A distinct `(Resource, revision, span)` read, requested once however
/// many pieces of evidence need it.
struct PlannedRange {
    resource: ResourceId,
    revision: String,
    span: SourceSpan,
    role: RangeRole,
}

#[derive(Default)]
struct ReadPlan {
    requests: Vec<PlannedRange>,
    seen: HashMap<(ResourceId, usize, usize), usize>,
}

impl ReadPlan {
    /// Request a range, reusing an identical one already planned.
    ///
    /// The first role wins: an evidence span that is also somebody's
    /// containing declaration is one read either way.
    fn request(
        &mut self,
        resource: ResourceId,
        revision: &str,
        span: SourceSpan,
        role: RangeRole,
    ) -> usize {
        let key = (resource, span.start_byte, span.end_byte);
        if let Some(existing) = self.seen.get(&key) {
            return *existing;
        }
        let index = self.requests.len();
        self.requests.push(PlannedRange {
            resource,
            revision: revision.to_owned(),
            span,
            role,
        });
        self.seen.insert(key, index);
        index
    }

    /// Requests grouped by Resource and revision, each group in plan
    /// order and the groups themselves in first-appearance order. One
    /// group is one verified file read.
    fn groups(&self) -> Vec<Vec<usize>> {
        let mut order: Vec<(ResourceId, String)> = Vec::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for (index, request) in self.requests.iter().enumerate() {
            let key = (request.resource, request.revision.clone());
            match order.iter().position(|existing| *existing == key) {
                Some(position) => groups[position].push(index),
                None => {
                    order.push(key);
                    groups.push(vec![index]);
                }
            }
        }
        groups
    }
}

struct PlannedEvidence {
    location: EvidenceLocation,
    evidence: Option<usize>,
    containing: Option<usize>,
    unavailable: Option<SourceUnavailable>,
}

struct PlannedTarget {
    symbol: Symbol,
    path_rel: String,
    range: Option<usize>,
    unavailable: Option<SourceUnavailable>,
}

struct ReadOutcome {
    ranges: Vec<PreparedRange>,
    slot: Vec<Option<RangeId>>,
    failure: Vec<Option<SourceUnavailable>>,
}

impl ReadOutcome {
    fn resolve(&self, planned: Option<usize>) -> Option<RangeId> {
        planned.and_then(|index| self.slot[index])
    }

    fn unavailable(&self, planned: Option<usize>) -> Option<SourceUnavailable> {
        planned.and_then(|index| self.failure[index].clone())
    }
}

/// What a failed read means for the caller, or a real error to
/// propagate.
///
/// A file that moved on, changed, or cannot be sliced is a state to
/// report. A broken index is not.
fn unavailable_from(error: ReadError) -> Result<SourceUnavailable, PrepareError> {
    Ok(match error {
        ReadError::RevisionMismatch {
            ref expected,
            ref actual,
            ..
        } => SourceUnavailable::StaleBasis {
            basis_revision: expected.clone(),
            current_revision: actual.clone(),
        },
        ReadError::SourceChanged {
            ref expected_content_hash,
            ref observed_content_hash,
            ..
        } => SourceUnavailable::SourceChanged {
            expected_content_hash: expected_content_hash.clone(),
            observed_content_hash: observed_content_hash.clone(),
        },
        ReadError::SymbolNotCurrent {
            ref symbol_revision,
            ref resource_revision,
            ..
        } => SourceUnavailable::SymbolNotCurrent {
            symbol_revision: symbol_revision.clone(),
            resource_revision: resource_revision.clone(),
        },
        ReadError::ResourceDeleted { .. }
        | ReadError::NotAFile { .. }
        | ReadError::UnverifiableResource { .. }
        | ReadError::UnknownResource { .. }
        | ReadError::Io { .. } => SourceUnavailable::NoCurrentSource {
            detail: error.to_string(),
        },
        ReadError::InvalidSpan { .. }
        | ReadError::SpanOutOfBounds { .. }
        | ReadError::SpanNotUtf8 { .. } => SourceUnavailable::SpanNotReadable {
            detail: error.to_string(),
        },
        // An index that cannot be queried is not a coverage statement.
        ReadError::Query(_) | ReadError::Sqlite(_) | ReadError::UnknownSymbol { .. } => {
            return Err(PrepareError::Read(error));
        }
    })
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use brainprint_core::SymbolId;
    use rusqlite::params;

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        evidence::{OccurrenceRef, RelationEvidence, replace_resource_graph},
        gaps::{IntendedRelation, UnresolvedEvidence, UnresolvedReason},
        generation,
        graph::{self, ExternalEntity, GraphStore, Relation},
        parser::SourcePoint,
        query::NotCurrentReason,
        resolution::{Dispatch, EvidenceBasis},
        resource::ResourceStore,
        scan::BaselineScan,
        symbol::{OccurrenceKind, SymbolStore},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const APP_TS: &str = "\
import { shared } from './shared'

export function run(obj: Thing): number {
  obj.foo()
  return shared() + shared()
}
";

    const OTHER_TS: &str = "\
import { shared } from './shared'
import { useState } from 'react'

export function other(): number {
  return shared()
}
";

    const SHARED_TS: &str = "\
export function shared(): number {
  return 1
}
";

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-prepare-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/other.ts", OTHER_TS);
            fixture.write("src/shared.ts", SHARED_TS);
            BaselineScan::open(&fixture.db_path())
                .expect("index.db")
                .run_initial_scan(
                    &fixture.root,
                    &WorkspaceConfig::default(),
                    "workspace-rev-1",
                )
                .expect("baseline scan");
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        fn resource(&self, rel: &str) -> crate::resource::Resource {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn symbol(&self, rel: &str, qualified_name: &str) -> SymbolId {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource(rel).id)
                .expect("symbols")
                .into_iter()
                .find(|symbol| symbol.qualified_name == qualified_name)
                .expect("the declaration is indexed")
                .id
        }

        fn sites(&self, rel: &str, kind: OccurrenceKind) -> Vec<OccurrenceRef> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_occurrences_for_resource(self.resource(rel).id)
                .expect("occurrences")
                .into_iter()
                .filter(|occurrence| occurrence.kind == kind)
                .map(|occurrence| OccurrenceRef {
                    kind: occurrence.kind,
                    start_byte: occurrence.span.start_byte,
                    end_byte: occurrence.span.end_byte,
                })
                .collect()
        }

        /// Import Occurrences that are the module specifier, not the
        /// imported name: the anchor #17 task 4 binds an IMPORTS edge to.
        fn import_specifiers(&self, rel: &str) -> Vec<OccurrenceRef> {
            let source = fs::read_to_string(self.root.join(rel)).expect("source");
            self.sites(rel, OccurrenceKind::ImportSite)
                .into_iter()
                .filter(|site| source[site.start_byte..site.end_byte].starts_with('\''))
                .collect()
        }

        fn preparer(&self) -> InspectPreparer {
            InspectPreparer::open(&self.db_path(), &self.root).expect("preparer")
        }

        fn basis(&self, rel: &str, generation_id: i64) -> EvidenceBasis {
            let resource = self.resource(rel);
            let profile_id = SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(resource.id)
                .expect("symbols")
                .first()
                .expect("the file declares something")
                .analysis_profile_id;
            EvidenceBasis {
                owner_resource: resource.id,
                owner_resource_revision: resource.resource_revision,
                generation_id,
                analysis_profile_id: profile_id,
                resolution_context_key: None,
            }
        }

        /// The whole fixture graph, published the way #16 task 13/14
        /// publish: two call sites in app.ts proving one edge, another
        /// prover in other.ts, imports, and one unresolved call.
        fn publish_baseline(&self) {
            let app = GraphEndpoint::Resource(self.resource("src/app.ts").id);
            let other = GraphEndpoint::Resource(self.resource("src/other.ts").id);
            let shared_file = GraphEndpoint::Resource(self.resource("src/shared.ts").id);
            let run = GraphEndpoint::Symbol(self.symbol("src/app.ts", "run"));
            let other_fn = GraphEndpoint::Symbol(self.symbol("src/other.ts", "other"));
            let shared = GraphEndpoint::Symbol(self.symbol("src/shared.ts", "shared"));

            let app_calls = self.sites("src/app.ts", OccurrenceKind::CallSite);
            let app_imports = self.import_specifiers("src/app.ts");
            let other_calls = self.sites("src/other.ts", OccurrenceKind::CallSite);
            let other_imports = self.import_specifiers("src/other.ts");

            let store = GraphStore::open(&self.db_path()).expect("index.db");
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");

            let plan = [
                (
                    "src/app.ts",
                    vec![
                        evidence(
                            app_calls[1],
                            edge(RelationKind::Calls, &run, &shared, building.id),
                        ),
                        evidence(
                            app_calls[2],
                            edge(RelationKind::Calls, &run, &shared, building.id),
                        ),
                        evidence(
                            app_imports[0],
                            edge(RelationKind::Imports, &app, &shared_file, building.id),
                        ),
                    ],
                    vec![UnresolvedEvidence {
                        occurrence: app_calls[0],
                        intended: IntendedRelation::Known(RelationKind::Calls),
                        lookup_name: "foo".to_owned(),
                        module_hint: None,
                        reason: UnresolvedReason::ReceiverTypeRequired,
                        candidates: Vec::new(),
                    }],
                ),
                (
                    "src/other.ts",
                    vec![
                        evidence(
                            other_calls[0],
                            edge(RelationKind::Calls, &other_fn, &shared, building.id),
                        ),
                        evidence(
                            other_imports[0],
                            edge(RelationKind::Imports, &other, &shared_file, building.id),
                        ),
                        evidence(
                            other_imports[1],
                            edge(RelationKind::Imports, &other, &react(), building.id),
                        ),
                    ],
                    Vec::new(),
                ),
            ];

            for (rel, resolved, unresolved) in plan {
                for item in &resolved {
                    for endpoint in [&item.relation.source, &item.relation.target] {
                        graph::ensure_entity(&transaction, endpoint).expect("ensure");
                    }
                }
                replace_resource_graph(
                    &transaction,
                    &grant,
                    &self.basis(rel, building.id),
                    &resolved,
                    &unresolved,
                )
                .expect("replace");
            }
            generation::finish_publish_stable(&transaction, &record).expect("stable");
            transaction.commit().expect("commit");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn edge(
        kind: RelationKind,
        source: &GraphEndpoint,
        target: &GraphEndpoint,
        generation: i64,
    ) -> Relation {
        Relation {
            kind,
            source: source.clone(),
            target: target.clone(),
            dispatch: Dispatch::Static,
            created_generation: generation,
        }
    }

    fn evidence(occurrence: OccurrenceRef, relation: Relation) -> RelationEvidence {
        RelationEvidence {
            occurrence,
            relation,
        }
    }

    fn react() -> GraphEndpoint {
        GraphEndpoint::External(ExternalEntity {
            package_identity: "react".to_owned(),
            module_path: None,
            symbol_name: Some("useState".to_owned()),
            qualified_name: None,
            kind: "IMPORTED_NAME".to_owned(),
            resolved_version: None,
            declaration_locator: None,
        })
    }

    fn source_of(prepared: &PreparedInspection, id: Option<RangeId>) -> &str {
        &prepared
            .range(id.expect("a prepared range"))
            .expect("the range is in the result")
            .source
    }

    #[test]
    fn callers_arrive_with_the_current_source_of_the_target_and_every_call_site() {
        let fixture = Fixture::create("callers");
        fixture.publish_baseline();
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));

        let prepared = fixture
            .preparer()
            .prepare(&shared, Direction::Incoming, &[RelationKind::Calls])
            .expect("prepare");

        // The target's own current declaration, not just a locator.
        let target = prepared.target.as_ref().expect("a Symbol anchor");
        assert_eq!(target.path_rel, "src/shared.ts");
        assert_eq!(
            source_of(&prepared, target.range),
            "function shared(): number {\n  return 1\n}"
        );
        assert!(target.unavailable.is_none());

        assert_eq!(prepared.confirmed_count(), 2, "app.ts and other.ts");
        assert!(prepared.source_complete());

        let from_app = prepared
            .relations
            .iter()
            .find(|prepared| {
                prepared.relation.source
                    == GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"))
            })
            .expect("app.ts proves the edge");
        assert_eq!(from_app.evidence.len(), 2, "both call sites");
        for item in &from_app.evidence {
            // The exact evidence span stays authoritative.
            assert_eq!(source_of(&prepared, item.evidence_range), "shared");
            assert_eq!(item.location.occurrence_kind, OccurrenceKind::CallSite);
            // And it arrives inside the declaration it sits in.
            assert_eq!(
                source_of(&prepared, item.containing_range),
                "function run(obj: Thing): number {\n  obj.foo()\n  return shared() + shared()\n}"
            );
            assert!(item.unavailable.is_none());
        }
    }

    #[test]
    fn identical_ranges_are_shared_and_never_read_twice() {
        let fixture = Fixture::create("dedupe");
        fixture.publish_baseline();
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));

        let prepared = fixture
            .preparer()
            .prepare(&shared, Direction::Incoming, &[RelationKind::Calls])
            .expect("prepare");

        let from_app = prepared
            .relations
            .iter()
            .find(|prepared| prepared.evidence.len() == 2)
            .expect("the two-site edge");
        assert_eq!(
            from_app.evidence[0].containing_range, from_app.evidence[1].containing_range,
            "one containing declaration, one range"
        );
        assert_ne!(
            from_app.evidence[0].evidence_range, from_app.evidence[1].evidence_range,
            "two distinct call sites stay distinct"
        );

        // Every prepared range is a distinct (Resource, span).
        let mut keys: Vec<(ResourceId, usize, usize)> = prepared
            .ranges
            .iter()
            .map(|range| (range.resource, range.span.start_byte, range.span.end_byte))
            .collect();
        let before = keys.len();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), before, "no range is prepared twice");
    }

    #[test]
    fn one_resource_is_read_once_however_many_ranges_it_contributes() {
        let mut plan = ReadPlan::default();
        let app = ResourceId::from_bytes([1; 16]);
        let other = ResourceId::from_bytes([2; 16]);
        let first = plan.request(app, "rev-1", span(0, 4), RangeRole::EvidenceSpan);
        let second = plan.request(app, "rev-1", span(8, 12), RangeRole::EvidenceSpan);
        let third = plan.request(other, "rev-1", span(0, 4), RangeRole::EvidenceSpan);
        // The same span again, wanted for another reason, is not a
        // second read -- and the first role stands.
        let repeat = plan.request(app, "rev-1", span(0, 4), RangeRole::ContainingDeclaration);

        assert_eq!(repeat, first);
        assert_eq!(plan.requests.len(), 3);
        assert_eq!(plan.requests[first].role, RangeRole::EvidenceSpan);
        assert_eq!(
            plan.groups(),
            vec![vec![first, second], vec![third]],
            "one verified read per Resource, in plan order"
        );
    }

    #[test]
    fn a_stale_basis_reports_the_state_instead_of_source() {
        let fixture = Fixture::create("stale");
        fixture.publish_baseline();
        let app = fixture.resource("src/app.ts").id;
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));

        // app.ts moves on: its evidence describes source that is no
        // longer where it was.
        GraphStore::open(&fixture.db_path())
            .expect("index.db")
            .connection()
            .execute(
                "UPDATE resource SET resource_revision = 'moved' WHERE uid = ?1",
                params![app.to_bytes().to_vec()],
            )
            .expect("bump revision");

        let prepared = fixture
            .preparer()
            .prepare(&shared, Direction::Incoming, &[RelationKind::Calls])
            .expect("prepare");

        let from_app = prepared
            .relations
            .iter()
            .find(|prepared| prepared.evidence[0].location.resource == app)
            .expect("app.ts evidence");
        for item in &from_app.evidence {
            assert!(item.evidence_range.is_none(), "nothing is sliced");
            assert!(item.containing_range.is_none());
            assert_eq!(
                item.unavailable,
                Some(SourceUnavailable::StaleBasis {
                    basis_revision: item.location.basis_revision.clone(),
                    current_revision: "moved".to_owned(),
                })
            );
        }
        assert!(!prepared.source_complete());

        // The other prover is unaffected: one stale file does not
        // withhold the rest.
        let from_other = prepared
            .relations
            .iter()
            .find(|prepared| prepared.evidence[0].location.resource != app)
            .expect("other.ts evidence");
        assert_eq!(
            source_of(&prepared, from_other.evidence[0].evidence_range),
            "shared"
        );
    }

    #[test]
    fn changed_source_is_reported_rather_than_sliced() {
        let fixture = Fixture::create("changed");
        fixture.publish_baseline();
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));

        // The file is edited behind the index: same revision row, other
        // bytes.
        fixture.write(
            "src/app.ts",
            "import { shared } from './shared'\n\nexport function run(): number {\n  return 0\n}\n",
        );

        let prepared = fixture
            .preparer()
            .prepare(&shared, Direction::Incoming, &[RelationKind::Calls])
            .expect("prepare");

        let from_app = prepared
            .relations
            .iter()
            .find(|prepared| prepared.evidence.len() == 2)
            .expect("app.ts evidence");
        for item in &from_app.evidence {
            assert!(item.evidence_range.is_none(), "no convincing wrong source");
            assert!(matches!(
                item.unavailable,
                Some(SourceUnavailable::SourceChanged { .. })
            ));
        }
        // The failed read demoted the index, and the result says so.
        assert_eq!(
            prepared.currentness,
            Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty)
        );
    }

    #[test]
    fn an_external_anchor_gets_no_fabricated_declaration() {
        let fixture = Fixture::create("external");
        fixture.publish_baseline();

        let prepared = fixture
            .preparer()
            .prepare(&react(), Direction::Incoming, &[RelationKind::Imports])
            .expect("prepare");

        assert!(
            prepared.target.is_none(),
            "an external package's definition is not in this Workspace"
        );
        assert_eq!(prepared.confirmed_count(), 1);
        let item = &prepared.relations[0].evidence[0];
        // The local evidence of the import is ours, and it is prepared.
        assert_eq!(source_of(&prepared, item.evidence_range), "'react'");
        assert!(
            item.containing_range.is_none(),
            "a file-level import sits in no declaration"
        );
    }

    #[test]
    fn a_resource_anchor_prepares_evidence_without_a_symbol_range() {
        let fixture = Fixture::create("resource-anchor");
        fixture.publish_baseline();
        let app = fixture.resource("src/app.ts").id;

        let prepared = fixture
            .preparer()
            .prepare(
                &GraphEndpoint::Resource(app),
                Direction::Outgoing,
                &[RelationKind::Imports],
            )
            .expect("prepare");

        assert!(prepared.target.is_none(), "a file is not a declaration");
        assert_eq!(prepared.confirmed_count(), 1);
        let item = &prepared.relations[0].evidence[0];
        assert_eq!(item.location.occurrence_kind, OccurrenceKind::ImportSite);
        assert_eq!(source_of(&prepared, item.evidence_range), "'./shared'");
        assert!(item.containing_range.is_none());
    }

    #[test]
    fn prepared_ranges_are_bounded_and_never_whole_files() {
        let fixture = Fixture::create("bounded");
        fixture.publish_baseline();
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));

        let prepared = fixture
            .preparer()
            .prepare(&shared, Direction::Incoming, &[RelationKind::Calls])
            .expect("prepare");

        for range in &prepared.ranges {
            let whole = fs::read_to_string(fixture.root.join(&range.path_rel)).expect("file");
            assert!(
                range.source.len() < whole.len(),
                "{} is the whole file, not a bounded range",
                range.path_rel
            );
            assert_eq!(
                &whole[range.span.start_byte..range.span.end_byte],
                range.source,
                "the source is the span's, read from the current file"
            );
        }
    }

    #[test]
    fn gaps_and_coverage_pass_through_untouched_and_ordering_is_stable() {
        let fixture = Fixture::create("passthrough");
        fixture.publish_baseline();
        let preparer = fixture.preparer();
        let run = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));

        let answer = preparer.relations().callees(&run).expect("callees");
        let prepared = preparer
            .prepare(&run, Direction::Outgoing, &[RelationKind::Calls])
            .expect("prepare");

        assert_eq!(prepared.gaps, answer.gaps, "task 8's gaps, unchanged");
        assert_eq!(prepared.coverage, answer.coverage);
        assert_eq!(prepared.gaps.len(), 1);
        assert_eq!(
            prepared.gaps[0].reason,
            UnresolvedReason::ReceiverTypeRequired
        );
        assert!(
            !prepared.coverage.is_complete(),
            "an unresolved call site is still a gap once source is attached"
        );
        assert_eq!(
            prepared.relations[0].relation, answer.confirmed[0],
            "the relation contract is carried, not rebuilt"
        );

        let again = preparer
            .prepare(&run, Direction::Outgoing, &[RelationKind::Calls])
            .expect("prepare");
        assert_eq!(prepared, again, "deterministic ranges and order");
    }

    #[test]
    fn preparing_source_never_writes_it_into_the_index() {
        let fixture = Fixture::create("no-mirror");
        fixture.publish_baseline();
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));

        let prepared = fixture
            .preparer()
            .prepare(&shared, Direction::Incoming, &[RelationKind::Calls])
            .expect("prepare");
        assert!(!prepared.ranges.is_empty(), "source was prepared");

        let stored = fs::read(fixture.db_path()).expect("index.db");
        for needle in ["return shared() + shared()", "export function shared"] {
            assert!(
                !stored
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "index.db mirrors {needle:?}"
            );
        }
    }

    fn span(start_byte: usize, end_byte: usize) -> SourceSpan {
        SourceSpan {
            start_byte,
            end_byte,
            start: SourcePoint { line: 0, column: 0 },
            end: SourcePoint { line: 0, column: 0 },
        }
    }
}
