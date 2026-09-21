//! Resource-owned atomic replacement of graph evidence (#17 task 3).
//!
//! I2 gave every Occurrence an owner: the Resource whose re-analysis may
//! replace it. A Relation has no owner -- it is canonical, and two
//! different files can both prove that `A` calls `B`. This module is what
//! joins the two without letting either lie about the other.
//!
//! ## What "owned" means here
//!
//! Re-analyzing `A.ts` replaces exactly the graph evidence `A.ts`
//! produced: the relation bindings on its own Occurrences, and the
//! unresolved/candidate rows hanging off them. `C.ts`'s evidence for the
//! same Relation is untouched, because it was never this Resource's to
//! replace.
//!
//! A canonical Relation therefore disappears only when its *last*
//! evidence does. The check is a fact, not a count kept in a column:
//! after the replacement, a relation with no Occurrence pointing at it
//! has nothing left proving it, and only then is it removed.
//!
//! ## One transaction, or nothing
//!
//! Relation rows, Occurrence bindings, unresolved references and their
//! candidates are written through the caller's open publication
//! transaction -- the same pattern #16 task 13/14 use for Symbols and
//! Occurrences, and for the same reason: a half-replaced graph is worse
//! than an out-of-date one. Any error returns before the commit, so the
//! previous evidence survives intact. A [`PublicationGrant`] is required,
//! so this can only run inside a publication that is about to become
//! STABLE -- a BUILDING generation is never exposed as current.
//!
//! ## What this tier does not do
//!
//! It does not extract anything (#17 task 4+), resolve anything (tasks
//! 4-5), or invent candidates (task 7). It is handed evidence that some
//! extractor already produced and decides only where it may be written
//! and what may be deleted. Candidate *creation* arrives in task 7; the
//! deletion side lives here already, because an owner's replacement has
//! to be able to clear what its previous run left behind.

use std::{error::Error, fmt};

use brainprint_core::ResourceId;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    generation::PublicationGrant,
    graph::{self, GraphError, Relation, RelationKey},
    resolution::EvidenceBasis,
    resource::ResourceState,
    symbol::OccurrenceKind,
};

/// Which Occurrence of the owning Resource a piece of evidence is about.
///
/// An Occurrence has no stable identity of its own (#16 task 9): it is
/// evidence about a span of the current source, so it is addressed by
/// what it is -- its kind and its exact byte range within the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OccurrenceRef {
    pub kind: OccurrenceKind,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// One Occurrence proving one canonical Relation.
///
/// Several of these may name the same [`Relation`]: two call sites in one
/// file are two pieces of evidence for one edge, not two edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationEvidence {
    pub occurrence: OccurrenceRef,
    pub relation: Relation,
}

/// What one replacement changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvidenceReplacement {
    /// Canonical relations this owner's evidence now proves.
    pub relations_bound: usize,
    /// Relations newly inserted (the rest already existed, proven by
    /// another Resource or by this one's previous run).
    pub relations_created: usize,
    /// Occurrences of this owner bound to a relation.
    pub occurrences_bound: usize,
    /// Relations removed because this replacement took away their last
    /// remaining evidence.
    pub relations_removed: usize,
    /// Unresolved references (and their candidates) cleared from this
    /// owner's previous run.
    pub unresolved_cleared: usize,
}

/// Failure of a Resource-owned evidence replacement. Every variant
/// returns before the caller's commit, so the previous evidence stands.
#[derive(Debug)]
pub enum EvidenceError {
    Sqlite(rusqlite::Error),
    Graph(GraphError),
    UnknownResource {
        resource_id: ResourceId,
    },
    ResourceNotActive {
        resource_id: ResourceId,
        state: ResourceState,
    },
    /// The basis describes a revision the Resource has moved past. The
    /// analysis read source that is no longer current, so its evidence
    /// is not written.
    RevisionMismatch {
        resource_id: ResourceId,
        basis: String,
        current: String,
    },
    /// The basis names a different generation than the publication this
    /// replacement runs in.
    GenerationMismatch {
        basis: i64,
        publication: i64,
    },
    /// The basis names a resolution context this `index.db` has never
    /// stored. Contexts are ensured before the evidence that depends on
    /// them (#17 task 2).
    UnknownResolutionContext {
        context_key: String,
    },
    /// Evidence named an Occurrence the owner does not have. The
    /// Occurrence set is published first (#16 task 9); evidence may bind
    /// to it, never invent it.
    UnknownOccurrence {
        resource_id: ResourceId,
        occurrence: OccurrenceRef,
    },
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(source) => write!(formatter, "evidence sqlite error: {source}"),
            Self::Graph(source) => write!(formatter, "graph write failed: {source}"),
            Self::UnknownResource { resource_id } => {
                write!(formatter, "no resource row for {resource_id}")
            }
            Self::ResourceNotActive { resource_id, state } => write!(
                formatter,
                "resource {resource_id} is {state}, so it owns no current evidence"
            ),
            Self::RevisionMismatch {
                resource_id,
                basis,
                current,
            } => write!(
                formatter,
                "evidence for {resource_id} was extracted from revision {basis}, \
                 but the resource is at {current}"
            ),
            Self::GenerationMismatch { basis, publication } => write!(
                formatter,
                "evidence names generation {basis}, not the {publication} being published"
            ),
            Self::UnknownResolutionContext { context_key } => {
                write!(formatter, "no resolution context {context_key:?}")
            }
            Self::UnknownOccurrence {
                resource_id,
                occurrence,
            } => write!(
                formatter,
                "resource {resource_id} has no {} occurrence at {}..{}",
                occurrence.kind, occurrence.start_byte, occurrence.end_byte
            ),
        }
    }
}

impl Error for EvidenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(source) => Some(source),
            Self::Graph(source) => Some(source),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for EvidenceError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<GraphError> for EvidenceError {
    fn from(source: GraphError) -> Self {
        Self::Graph(source)
    }
}

/// Replace every piece of graph evidence one Resource owns, in the
/// caller's open publication transaction.
///
/// `basis` is the provenance #17 task 2 defined, and it is checked
/// against reality here rather than trusted: the owner must be ACTIVE at
/// the basis revision, and the basis generation must be the one being
/// published. Evidence extracted from source that has moved on is
/// refused, not written and marked stale afterwards.
///
/// The `resolution_context_id` and `analysis_profile_id` the basis names
/// are written onto each bound Occurrence, so the provenance of a binding
/// survives on the row rather than only in the caller's memory.
pub fn replace_resource_evidence(
    connection: &Connection,
    grant: &PublicationGrant,
    basis: &EvidenceBasis,
    evidence: &[RelationEvidence],
) -> Result<EvidenceReplacement, EvidenceError> {
    if basis.generation_id != grant.generation_id() {
        return Err(EvidenceError::GenerationMismatch {
            basis: basis.generation_id,
            publication: grant.generation_id(),
        });
    }
    let owner = owner_row(connection, basis)?;
    let context_id = resolution_context_id(connection, basis)?;

    // What this owner's evidence currently proves. Anything that stops
    // being proven by the end is a candidate for removal -- but only if
    // nothing else proves it either.
    let previously_bound = bound_relations(connection, owner)?;

    // Clear this owner's bindings and the unresolved rows hanging off
    // them. Candidates go first: they reference the unresolved row.
    let unresolved_cleared = clear_unresolved(connection, owner)?;
    connection.execute(
        "UPDATE occurrence SET relation_id = NULL, resolution_context_id = NULL \
         WHERE resource_id = ?1",
        params![owner],
    )?;

    let mut replacement = EvidenceReplacement {
        unresolved_cleared,
        ..EvidenceReplacement::default()
    };
    let mut bound_now: Vec<i64> = Vec::new();
    for item in evidence {
        // The canonical edge, created only if nothing proves it yet.
        if graph::insert_relation(connection, &item.relation)? {
            replacement.relations_created += 1;
        }
        let relation_id = relation_row_id(connection, &item.relation)?;
        let occurrence_id = occurrence_row_id(connection, owner, basis, item.occurrence)?;
        connection.execute(
            "UPDATE occurrence SET relation_id = ?1, resolution_context_id = ?2, \
                    analysis_profile_id = ?3 \
             WHERE id = ?4",
            params![
                relation_id,
                context_id,
                basis.analysis_profile_id,
                occurrence_id
            ],
        )?;
        replacement.occurrences_bound += 1;
        if !bound_now.contains(&relation_id) {
            bound_now.push(relation_id);
        }
    }
    replacement.relations_bound = bound_now.len();

    // A relation this owner stopped proving survives if anyone else
    // still proves it. Only the ones with no evidence left at all go.
    for relation_id in previously_bound {
        if bound_now.contains(&relation_id) {
            continue;
        }
        if delete_relation_if_unevidenced(connection, relation_id)? {
            replacement.relations_removed += 1;
        }
    }
    Ok(replacement)
}

/// The owner's local row id, after checking it is what the basis claims.
fn owner_row(connection: &Connection, basis: &EvidenceBasis) -> Result<i64, EvidenceError> {
    let row: Option<(i64, String, String)> = connection
        .query_row(
            "SELECT id, state, resource_revision FROM resource WHERE uid = ?1",
            params![basis.owner_resource.to_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((local_id, state, revision)) = row else {
        return Err(EvidenceError::UnknownResource {
            resource_id: basis.owner_resource,
        });
    };
    let state = ResourceState::parse(&state).map_err(|_| EvidenceError::ResourceNotActive {
        resource_id: basis.owner_resource,
        state: ResourceState::Deleted,
    })?;
    if state != ResourceState::Active {
        return Err(EvidenceError::ResourceNotActive {
            resource_id: basis.owner_resource,
            state,
        });
    }
    if revision != basis.owner_resource_revision {
        return Err(EvidenceError::RevisionMismatch {
            resource_id: basis.owner_resource,
            basis: basis.owner_resource_revision.clone(),
            current: revision,
        });
    }
    Ok(local_id)
}

fn resolution_context_id(
    connection: &Connection,
    basis: &EvidenceBasis,
) -> Result<Option<i64>, EvidenceError> {
    let Some(key) = basis.resolution_context_key.as_deref() else {
        return Ok(None);
    };
    let found: Option<i64> = connection
        .query_row(
            "SELECT id FROM resolution_context WHERE context_key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    found
        .ok_or_else(|| EvidenceError::UnknownResolutionContext {
            context_key: key.to_owned(),
        })
        .map(Some)
}

/// Every relation this owner's Occurrences currently point at.
fn bound_relations(connection: &Connection, owner: i64) -> Result<Vec<i64>, EvidenceError> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT relation_id FROM occurrence \
         WHERE resource_id = ?1 AND relation_id IS NOT NULL",
    )?;
    let rows = statement.query_map(params![owner], |row| row.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<i64>>>()?)
}

/// Drop the unresolved references this owner's Occurrences carry, and the
/// candidates attached to them.
fn clear_unresolved(connection: &Connection, owner: i64) -> Result<usize, EvidenceError> {
    connection.execute(
        "DELETE FROM relation_candidate WHERE unresolved_reference_id IN ( \
             SELECT unresolved_reference.id FROM unresolved_reference \
             JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
             WHERE occurrence.resource_id = ?1 \
         )",
        params![owner],
    )?;
    Ok(connection.execute(
        "DELETE FROM unresolved_reference WHERE occurrence_id IN ( \
             SELECT id FROM occurrence WHERE resource_id = ?1 \
         )",
        params![owner],
    )?)
}

fn relation_row_id(connection: &Connection, relation: &Relation) -> Result<i64, EvidenceError> {
    let key = RelationKey {
        kind: relation.kind,
        source: &relation.source,
        target: &relation.target,
    };
    graph::relation_row_id(connection, &key)?.ok_or_else(|| {
        EvidenceError::Graph(GraphError::UnknownEntity {
            endpoint: Box::new(relation.target.clone()),
        })
    })
}

/// The owner's Occurrence at this exact kind and byte range.
fn occurrence_row_id(
    connection: &Connection,
    owner: i64,
    basis: &EvidenceBasis,
    occurrence: OccurrenceRef,
) -> Result<i64, EvidenceError> {
    let found: Option<i64> = connection
        .query_row(
            "SELECT id FROM occurrence \
             WHERE resource_id = ?1 AND kind = ?2 AND start_byte = ?3 AND end_byte = ?4",
            params![
                owner,
                occurrence.kind.as_str(),
                i64::try_from(occurrence.start_byte).unwrap_or(i64::MAX),
                i64::try_from(occurrence.end_byte).unwrap_or(i64::MAX),
            ],
            |row| row.get(0),
        )
        .optional()?;
    found.ok_or(EvidenceError::UnknownOccurrence {
        resource_id: basis.owner_resource,
        occurrence,
    })
}

/// Remove a relation if nothing proves it any more.
fn delete_relation_if_unevidenced(
    connection: &Connection,
    relation_id: i64,
) -> Result<bool, EvidenceError> {
    let changed = connection.execute(
        "DELETE FROM relation WHERE id = ?1 \
         AND NOT EXISTS (SELECT 1 FROM occurrence WHERE relation_id = ?1)",
        params![relation_id],
    )?;
    Ok(changed == 1)
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

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        generation::{self, GenerationState},
        graph::{ExternalEntity, GraphEndpoint, GraphStore, RelationKind},
        resolution::Dispatch,
        resource::ResourceStore,
        scan::BaselineScan,
        symbol::{Occurrence, SymbolStore},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const APP_TS: &str = "\
import { shared } from './shared'

export function run(): number {
  return shared() + shared()
}
";

    const OTHER_TS: &str = "\
import { shared } from './shared'

export function other(): number {
  return shared()
}
";

    const SHARED_TS: &str = "export function shared(): number {\n  return 1\n}\n";

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-evidence-{label}-{}-{sequence}",
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

        fn occurrences(&self, rel: &str) -> Vec<Occurrence> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_occurrences_for_resource(self.resource(rel).id)
                .expect("occurrences")
        }

        /// The owner's call-site evidence, in source order.
        fn call_sites(&self, rel: &str) -> Vec<OccurrenceRef> {
            self.occurrences(rel)
                .into_iter()
                .filter(|occurrence| occurrence.kind == OccurrenceKind::CallSite)
                .map(|occurrence| OccurrenceRef {
                    kind: occurrence.kind,
                    start_byte: occurrence.span.start_byte,
                    end_byte: occurrence.span.end_byte,
                })
                .collect()
        }

        fn import_sites(&self, rel: &str) -> Vec<OccurrenceRef> {
            self.occurrences(rel)
                .into_iter()
                .filter(|occurrence| occurrence.kind == OccurrenceKind::ImportSite)
                .map(|occurrence| OccurrenceRef {
                    kind: occurrence.kind,
                    start_byte: occurrence.span.start_byte,
                    end_byte: occurrence.span.end_byte,
                })
                .collect()
        }

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.db_path()).expect("index.db")
        }

        fn basis(&self, rel: &str, generation_id: i64) -> EvidenceBasis {
            let resource = self.resource(rel);
            let profile_id: i64 = SymbolStore::open(&self.db_path())
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

        fn relation_count(&self) -> i64 {
            self.store().relation_count().expect("count")
        }

        /// Bound relation row ids for one Resource's Occurrences.
        fn bound(&self, rel: &str) -> Vec<i64> {
            let store = self.store();
            let owner: i64 = store
                .connection()
                .query_row(
                    "SELECT id FROM resource WHERE uid = ?1",
                    params![self.resource(rel).id.to_bytes().to_vec()],
                    |row| row.get(0),
                )
                .expect("owner row");
            bound_relations(store.connection(), owner).expect("bound")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// Run `body` inside a real publication: a BUILDING generation, the
    /// caller's transaction, the grant, then STABLE and commit -- the
    /// same shape #16 task 13/14 publish in.
    fn publish<T>(
        fixture: &Fixture,
        body: impl FnOnce(&Connection, &PublicationGrant, i64) -> Result<T, EvidenceError>,
    ) -> Result<T, EvidenceError> {
        let store = fixture.store();
        let connection = store.connection();
        let revision = generation::current_workspace_revision(connection)
            .expect("clock")
            .expect("bootstrapped");
        let building = generation::begin_generation(connection, &revision).expect("begin");
        let transaction = connection.unchecked_transaction().expect("transaction");
        let (record, grant) =
            generation::grant_publication(&transaction, building.id).expect("grant");
        match body(&transaction, &grant, building.id) {
            Ok(value) => {
                generation::finish_publish_stable(&transaction, &record).expect("stable");
                transaction.commit().expect("commit");
                Ok(value)
            }
            Err(error) => {
                // Dropping the transaction rolls back everything the
                // attempt wrote; the generation row predates it.
                drop(transaction);
                generation::abort_generation(connection, building.id, "test").expect("abort");
                Err(error)
            }
        }
    }

    fn external_package() -> GraphEndpoint {
        GraphEndpoint::External(ExternalEntity {
            package_identity: "npm:shared@1.0.0".to_owned(),
            module_path: Some("shared".to_owned()),
            symbol_name: Some("shared".to_owned()),
            qualified_name: None,
            kind: "FUNCTION".to_owned(),
            resolved_version: None,
            declaration_locator: None,
        })
    }

    fn calls(source: &GraphEndpoint, target: &GraphEndpoint, generation: i64) -> Relation {
        Relation {
            kind: RelationKind::Calls,
            source: source.clone(),
            target: target.clone(),
            dispatch: Dispatch::Static,
            created_generation: generation,
        }
    }

    #[test]
    fn replacing_one_resources_evidence_never_touches_another_resources() {
        let fixture = Fixture::create("ownership");
        let store = fixture.store();
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));
        let app = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let other = GraphEndpoint::Symbol(fixture.symbol("src/other.ts", "other"));
        for endpoint in [&shared, &app, &other] {
            store.ensure_entity(endpoint).expect("ensure");
        }
        drop(store);

        // Both files prove an edge to `shared`; only app.ts proves its
        // own extra edge.
        let app_sites = fixture.call_sites("src/app.ts");
        let other_sites = fixture.call_sites("src/other.ts");
        publish(&fixture, |connection, grant, generation| {
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/app.ts", generation),
                &[RelationEvidence {
                    occurrence: app_sites[0],
                    relation: calls(&app, &shared, generation),
                }],
            )?;
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/other.ts", generation),
                &[RelationEvidence {
                    occurrence: other_sites[0],
                    relation: calls(&other, &shared, generation),
                }],
            )
        })
        .expect("publish");
        assert_eq!(fixture.relation_count(), 2);

        // Re-analyzing app.ts with nothing to say removes only what
        // app.ts proved.
        let report = publish(&fixture, |connection, grant, generation| {
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/app.ts", generation),
                &[],
            )
        })
        .expect("publish");

        assert_eq!(report.relations_removed, 1);
        assert_eq!(report.occurrences_bound, 0);
        assert!(fixture.bound("src/app.ts").is_empty());
        assert_eq!(
            fixture.bound("src/other.ts").len(),
            1,
            "other.ts still proves its own edge"
        );
        assert_eq!(fixture.relation_count(), 1);
        let store = fixture.store();
        assert!(
            store
                .relation(&RelationKey {
                    kind: RelationKind::Calls,
                    source: &other,
                    target: &shared,
                })
                .expect("get")
                .is_some(),
            "another Resource's evidence is never this one's to delete"
        );
    }

    #[test]
    fn a_relation_outlives_every_occurrence_but_the_last() {
        let fixture = Fixture::create("last-evidence");
        let store = fixture.store();
        let app = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));
        store.ensure_entity(&app).expect("ensure");
        store.ensure_entity(&shared).expect("ensure");
        drop(store);

        // `run` calls `shared` twice: two Occurrences, one edge.
        let sites = fixture.call_sites("src/app.ts");
        assert_eq!(sites.len(), 2, "the fixture has two call sites");
        let report = publish(&fixture, |connection, grant, generation| {
            let relation = calls(&app, &shared, generation);
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/app.ts", generation),
                &[
                    RelationEvidence {
                        occurrence: sites[0],
                        relation: relation.clone(),
                    },
                    RelationEvidence {
                        occurrence: sites[1],
                        relation,
                    },
                ],
            )
        })
        .expect("publish");
        assert_eq!(report.occurrences_bound, 2);
        assert_eq!(report.relations_bound, 1);
        assert_eq!(report.relations_created, 1);
        assert_eq!(fixture.relation_count(), 1, "two proofs, one edge");

        // One call site goes: the edge is still proven.
        let report = publish(&fixture, |connection, grant, generation| {
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/app.ts", generation),
                &[RelationEvidence {
                    occurrence: sites[0],
                    relation: calls(&app, &shared, generation),
                }],
            )
        })
        .expect("publish");
        assert_eq!(report.relations_removed, 0);
        assert_eq!(report.relations_created, 0, "the edge already existed");
        assert_eq!(fixture.relation_count(), 1);
        assert_eq!(fixture.bound("src/app.ts").len(), 1);

        // The last one goes: nothing proves the edge, so it goes too.
        let report = publish(&fixture, |connection, grant, generation| {
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/app.ts", generation),
                &[],
            )
        })
        .expect("publish");
        assert_eq!(report.relations_removed, 1);
        assert_eq!(fixture.relation_count(), 0);
    }

    #[test]
    fn the_basis_is_checked_and_then_written_onto_the_evidence() {
        let fixture = Fixture::create("basis");
        let store = fixture.store();
        let app = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let package = external_package();
        store.ensure_entity(&app).expect("ensure");
        store.ensure_entity(&package).expect("ensure");
        let context = crate::resolution::ResolutionContext {
            language: "TYPESCRIPT".to_owned(),
            scope_key: "tsconfig.json".to_owned(),
            config_fingerprint: "sha256:config".to_owned(),
            dependency_fingerprint: "sha256:deps".to_owned(),
            environment_fingerprint: "sha256:env".to_owned(),
            module_resolution_fingerprint: "sha256:module".to_owned(),
            backend_snapshot_token: None,
        };
        let stable = generation::current_stable(store.connection())
            .expect("stable")
            .expect("published")
            .id;
        let context_key = store
            .ensure_resolution_context(&context, stable)
            .expect("ensure context");
        drop(store);

        let import = fixture.import_sites("src/app.ts")[0];
        let generation_used = publish(&fixture, |connection, grant, generation| {
            let mut basis = fixture.basis("src/app.ts", generation);
            basis.resolution_context_key = Some(context_key.clone());
            replace_resource_evidence(
                connection,
                grant,
                &basis,
                &[RelationEvidence {
                    occurrence: import,
                    relation: Relation {
                        kind: RelationKind::Imports,
                        source: app.clone(),
                        target: package.clone(),
                        dispatch: Dispatch::Unknown,
                        created_generation: generation,
                    },
                }],
            )?;
            Ok(generation)
        })
        .expect("publish");

        // The provenance is on the row, not only in the caller's head.
        let store = fixture.store();
        let (relation_id, context_id, profile_id, revision, generation_id): (
            Option<i64>,
            Option<i64>,
            i64,
            String,
            i64,
        ) = store
            .connection()
            .query_row(
                "SELECT relation_id, resolution_context_id, analysis_profile_id, \
                        resource_revision, generation \
                 FROM occurrence WHERE kind = 'IMPORT_SITE' AND start_byte = ?1",
                params![i64::try_from(import.start_byte).expect("offset")],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("occurrence row");
        assert!(relation_id.is_some());
        assert!(context_id.is_some());
        assert_eq!(
            profile_id,
            fixture
                .basis("src/app.ts", generation_used)
                .analysis_profile_id
        );
        assert_eq!(revision, fixture.resource("src/app.ts").resource_revision);
        assert!(generation_id > 0);
        assert_eq!(
            store
                .resolution_context(&context_key)
                .expect("read")
                .as_ref(),
            Some(&context),
            "and the context it depended on is still the one that was ensured"
        );

        // A BUILDING generation is never what a reader sees as current.
        let states: Vec<String> = {
            let mut statement = store
                .connection()
                .prepare("SELECT state FROM generation")
                .expect("prepare");
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query");
            rows.map(|row| row.expect("row")).collect()
        };
        assert!(!states.iter().any(|state| state == "BUILDING"));
        assert_eq!(
            generation::current_stable(store.connection())
                .expect("stable")
                .expect("published")
                .state,
            GenerationState::Stable
        );
    }

    #[test]
    fn a_basis_that_does_not_describe_the_current_resource_is_refused() {
        let fixture = Fixture::create("refusals");
        let store = fixture.store();
        let app = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));
        store.ensure_entity(&app).expect("ensure");
        store.ensure_entity(&shared).expect("ensure");
        drop(store);
        let site = fixture.call_sites("src/app.ts")[0];

        // A revision the Resource has moved past.
        let failure = publish(&fixture, |connection, grant, generation| {
            let mut basis = fixture.basis("src/app.ts", generation);
            basis.owner_resource_revision = "999".to_owned();
            replace_resource_evidence(
                connection,
                grant,
                &basis,
                &[RelationEvidence {
                    occurrence: site,
                    relation: calls(&app, &shared, generation),
                }],
            )
        })
        .expect_err("stale evidence is not written");
        assert!(matches!(failure, EvidenceError::RevisionMismatch { .. }));

        // A generation that is not the one being published.
        let failure = publish(&fixture, |connection, grant, generation| {
            let mut basis = fixture.basis("src/app.ts", generation);
            basis.generation_id = generation + 1_000;
            replace_resource_evidence(connection, grant, &basis, &[])
        })
        .expect_err("a basis from another publication is refused");
        assert!(matches!(failure, EvidenceError::GenerationMismatch { .. }));

        // An Occurrence the owner does not have.
        let failure = publish(&fixture, |connection, grant, generation| {
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/app.ts", generation),
                &[RelationEvidence {
                    occurrence: OccurrenceRef {
                        kind: OccurrenceKind::CallSite,
                        start_byte: 99_999,
                        end_byte: 100_000,
                    },
                    relation: calls(&app, &shared, generation),
                }],
            )
        })
        .expect_err("evidence may bind to an Occurrence, never invent one");
        assert!(matches!(failure, EvidenceError::UnknownOccurrence { .. }));

        // A context that was never ensured.
        let failure = publish(&fixture, |connection, grant, generation| {
            let mut basis = fixture.basis("src/app.ts", generation);
            basis.resolution_context_key = Some("sha256-rc1:missing".to_owned());
            replace_resource_evidence(connection, grant, &basis, &[])
        })
        .expect_err("a context is ensured before the evidence that needs it");
        assert!(matches!(
            failure,
            EvidenceError::UnknownResolutionContext { .. }
        ));

        assert_eq!(fixture.relation_count(), 0);
        assert!(fixture.bound("src/app.ts").is_empty());
    }

    #[test]
    fn a_failed_replacement_leaves_the_last_valid_evidence_in_place() {
        let fixture = Fixture::create("rollback");
        let store = fixture.store();
        let app = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));
        store.ensure_entity(&app).expect("ensure");
        store.ensure_entity(&shared).expect("ensure");
        drop(store);
        let sites = fixture.call_sites("src/app.ts");

        publish(&fixture, |connection, grant, generation| {
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/app.ts", generation),
                &[RelationEvidence {
                    occurrence: sites[0],
                    relation: calls(&app, &shared, generation),
                }],
            )
        })
        .expect("publish");
        let before = fixture.bound("src/app.ts");
        assert_eq!(before.len(), 1);

        // A replacement that gets partway -- one valid binding, then an
        // Occurrence that does not exist.
        let failure = publish(&fixture, |connection, grant, generation| {
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/app.ts", generation),
                &[
                    RelationEvidence {
                        occurrence: sites[1],
                        relation: calls(&app, &shared, generation),
                    },
                    RelationEvidence {
                        occurrence: OccurrenceRef {
                            kind: OccurrenceKind::CallSite,
                            start_byte: 88_888,
                            end_byte: 88_889,
                        },
                        relation: calls(&app, &shared, generation),
                    },
                ],
            )
        })
        .expect_err("the replacement fails");
        assert!(matches!(failure, EvidenceError::UnknownOccurrence { .. }));

        // Nothing half-applied: the clear, the rebind, and the failure
        // were one transaction, so the previous evidence is still there.
        assert_eq!(
            fixture.bound("src/app.ts"),
            before,
            "a failed replacement is not an empty success"
        );
        assert_eq!(fixture.relation_count(), 1);
        let bound_occurrences: i64 = fixture
            .store()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM occurrence WHERE relation_id IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(bound_occurrences, 1, "exactly the binding from before");
    }

    #[test]
    fn unresolved_rows_belong_to_their_owner_and_go_with_its_replacement() {
        let fixture = Fixture::create("unresolved");
        let store = fixture.store();
        let app = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        store.ensure_entity(&app).expect("ensure");
        let app_entity: i64 = store
            .connection()
            .query_row(
                "SELECT id FROM graph_entity WHERE entity_kind = 'SYMBOL' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("entity");

        // Rows as #17 task 7 will write them, for two different owners.
        for path in ["src/app.ts", "src/other.ts"] {
            let owner_uid = fixture.resource(path).id.to_bytes().to_vec();
            store
                .connection()
                .execute(
                    "INSERT INTO unresolved_reference \
                     (occurrence_id, intended_relation_kind, lookup_name, reason) \
                     SELECT occurrence.id, 'CALLS', 'shared', 'NO_TARGET' \
                     FROM occurrence \
                     JOIN resource ON resource.id = occurrence.resource_id \
                     WHERE resource.uid = ?1 AND occurrence.kind = 'CALL_SITE' LIMIT 1",
                    params![owner_uid],
                )
                .expect("unresolved row");
            let unresolved_id = store.connection().last_insert_rowid();
            store
                .connection()
                .execute(
                    "INSERT INTO relation_candidate \
                     (unresolved_reference_id, target_entity_id, evidence_kind, ordinal) \
                     VALUES (?1, ?2, 'NAME_MATCH', 0)",
                    params![unresolved_id, app_entity],
                )
                .expect("candidate row");
        }
        drop(store);
        assert_eq!(count(&fixture, "unresolved_reference"), 2);
        assert_eq!(count(&fixture, "relation_candidate"), 2);

        let report = publish(&fixture, |connection, grant, generation| {
            replace_resource_evidence(
                connection,
                grant,
                &fixture.basis("src/app.ts", generation),
                &[],
            )
        })
        .expect("publish");

        assert_eq!(report.unresolved_cleared, 1);
        assert_eq!(
            count(&fixture, "unresolved_reference"),
            1,
            "the other Resource's gap is not this replacement's to clear"
        );
        assert_eq!(count(&fixture, "relation_candidate"), 1);
        let owner_of_survivor: String = fixture
            .store()
            .connection()
            .query_row(
                "SELECT resource.path_rel FROM unresolved_reference \
                 JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
                 JOIN resource ON resource.id = occurrence.resource_id",
                [],
                |row| row.get(0),
            )
            .expect("survivor");
        assert_eq!(owner_of_survivor, "src/other.ts");
    }

    #[test]
    fn a_rolled_back_publication_leaves_no_half_state_anywhere() {
        let fixture = Fixture::create("half-state");
        let store = fixture.store();
        let app = GraphEndpoint::Symbol(fixture.symbol("src/app.ts", "run"));
        let shared = GraphEndpoint::Symbol(fixture.symbol("src/shared.ts", "shared"));
        store.ensure_entity(&app).expect("ensure");
        store.ensure_entity(&shared).expect("ensure");
        let connection = store.connection();
        let site = fixture.call_sites("src/app.ts")[0];
        let revision = generation::current_workspace_revision(connection)
            .expect("clock")
            .expect("bootstrapped");
        let building = generation::begin_generation(connection, &revision).expect("begin");

        {
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (_record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");
            let basis = EvidenceBasis {
                generation_id: building.id,
                ..fixture.basis("src/app.ts", building.id)
            };
            let report = replace_resource_evidence(
                &transaction,
                &grant,
                &basis,
                &[RelationEvidence {
                    occurrence: site,
                    relation: calls(&app, &shared, building.id),
                }],
            )
            .expect("the write itself succeeds");
            assert_eq!(report.occurrences_bound, 1);
            // ... and then the publication is abandoned.
        }

        assert_eq!(
            fixture.relation_count(),
            0,
            "the relation row went back with the transaction"
        );
        assert!(fixture.bound("src/app.ts").is_empty());
        let bound_occurrences: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM occurrence WHERE relation_id IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(bound_occurrences, 0, "no binding survived either");
    }

    fn count(fixture: &Fixture, table: &str) -> i64 {
        fixture
            .store()
            .connection()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count")
    }
}
