//! The Relation layer's lifecycle: initial, targeted refresh, reconcile
//! (#17 task 13).
//!
//! Tasks 4-7 and 12 can produce relations; this is what makes them
//! appear, change, and disappear on the *same* generation, revision, and
//! freshness contract as Resources, Symbols, and Occurrences. A
//! publication that leaves new Symbols beside old relations is exactly
//! the half-current state #16 task 14 closed, and nothing here is
//! allowed to reopen it: every relation write happens inside the
//! caller's publication transaction, under the same
//! [`PublicationGrant`].
//!
//! ## Two concerns, not one
//!
//! **Owned evidence** is what a Resource's own source states. It
//! changes only when that Resource changes, and it is replaced through
//! task 3's Resource-owned primitive.
//!
//! **Target resolution** is what that evidence resolved *to*, and it can
//! stop being true without the owner's source moving at all: `B.ts`
//! renames an export and `A.ts`'s resolved edge is now a claim nobody
//! checked. The answer is neither to leave A FRESH nor to re-analyze the
//! Workspace: [`dependents_of`] asks the graph which Resources actually
//! resolved *into* what changed, and only those are revalidated.
//! Revalidation replaces the relation layer of a Resource whose
//! Symbols, Occurrences and revision all stay exactly where they are.
//!
//! Per-Resource relation state lives in the ordinary component model as
//! [`component::RELATION_INDEX`], so "this file's relations are current"
//! and "this file's relations need revalidating" are the same kind of
//! fact as every other freshness claim in the system.
//!
//! ## What is never done
//!
//! No Workspace-wide rebuild on a single-file save. No re-extraction of
//! a Resource nothing happened to. No empty-success: a Resource whose
//! current bytes cannot be analyzed keeps its last valid relations and
//! is marked DIRTY, because "we could not look" and "there is nothing
//! there" are different answers.

use std::{collections::HashMap, fs, path::Path};

use brainprint_core::ResourceId;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    calls::{
        BindingScope, extract_call_sites, extract_import_bindings, extract_local_names,
        resolve_calls,
    },
    component::{self, ComponentRow, FreshnessState, ProcessingState},
    domain::{domain_relations, extract_key_accesses},
    evidence::{EvidenceError, OccurrenceRef, RelationEvidence, replace_resource_graph},
    gaps::{UnresolvedEvidence, call_gaps, import_gaps, type_gaps},
    generation::PublicationGrant,
    graph, identity,
    imports::{WorkspaceModules, extract_imports, import_relations, resolve_imports},
    parser::{self, ParserRegistry, SourceBasis, StructuralCapability},
    resolution::EvidenceBasis,
    resource::{self, Resource, ResourceKind, ResourceState},
    scan::ScanError,
    structural::{self, StructuralState},
    symbol::{self, OccurrenceKind, Symbol},
    types::{extract_type_references, resolve_type_references, type_relations},
};

/// What one publication did to the relation layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelationPublication {
    /// Resources whose own source was re-analyzed.
    pub analyzed: usize,
    /// Resources whose relations were re-resolved because something
    /// they pointed at changed.
    pub revalidated: usize,
    /// Resources that needed revalidating and could not be analyzed
    /// safely. Their previous relations stand, and their relation state
    /// is DIRTY.
    pub deferred: usize,
    /// Deleted Resources whose graph was cleared.
    pub cleared: usize,
    /// Canonical relations removed because their last evidence went.
    pub relations_removed: usize,
    /// Unresolved references written across every analyzed Resource.
    pub unresolved_written: usize,
}

/// What a publication intends to do to the relation layer, decided
/// *before* anything is detached.
///
/// The dependent set has to be computed while the graph still holds the
/// edges that are about to be taken apart: once a changed Resource's
/// entities are gone, nobody can be asked who pointed at them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelationPlan {
    changed: Vec<ResourceId>,
    deleted: Vec<ResourceId>,
    dependents: Vec<ResourceId>,
}

/// Decide the relation work for one publication.
///
/// Read-only, and the first thing a publication does: it asks the graph
/// as it still is.
pub(crate) fn plan_publication(
    connection: &Connection,
    changed: &[ResourceId],
    deleted: &[ResourceId],
    inventory_changed: bool,
) -> Result<RelationPlan, ScanError> {
    let mut touched: Vec<ResourceId> = changed.to_vec();
    touched.extend_from_slice(deleted);
    let dependents = dependents_of(connection, &touched, inventory_changed)?
        .into_iter()
        .filter(|id| !changed.contains(id) && !deleted.contains(id))
        .collect();
    Ok(RelationPlan {
        changed: changed.to_vec(),
        deleted: deleted.to_vec(),
        dependents,
    })
}

/// Publish the relation layer for one publication.
///
/// Runs after the structural publication, inside the same transaction:
/// every changed Resource is re-analyzed from its own current source,
/// and every dependent is re-resolved against the structure that now
/// exists.
pub(crate) fn publish_relations(
    connection: &Connection,
    grant: &PublicationGrant,
    publication_revision: &str,
    workspace_root: &Path,
    plan: &RelationPlan,
) -> Result<RelationPublication, ScanError> {
    let mut report = RelationPublication {
        cleared: plan.deleted.len(),
        ..RelationPublication::default()
    };

    let inventory = Inventory::read(connection)?;

    for resource_id in &plan.changed {
        if publish_one(
            connection,
            grant,
            publication_revision,
            workspace_root,
            *resource_id,
            &inventory,
            &mut report,
        )? {
            report.analyzed += 1;
        }
    }

    // Revalidation: the same replacement, and the same atomicity -- but
    // the owner's source, Symbols, Occurrences and revision all stay
    // exactly where they are. Only what its evidence resolves to is
    // recomputed.
    for resource_id in &plan.dependents {
        if publish_one(
            connection,
            grant,
            publication_revision,
            workspace_root,
            *resource_id,
            &inventory,
            &mut report,
        )? {
            report.revalidated += 1;
        } else {
            report.deferred += 1;
        }
    }

    Ok(report)
}

/// Take apart the relation layer a Resource's structure republication
/// is about to invalidate.
///
/// Called from the structural publication, with the Symbol set that is
/// about to replace the stored one:
///
/// - the Resource's own unresolved references and candidates go (they
///   are anchored to Occurrences that are about to be deleted);
/// - its bindings are released, and any canonical edge whose last
///   evidence that was is removed -- its own re-analysis restores the
///   ones still written in the file;
/// - the entities of declarations that are **gone** are removed, with
///   the edges pointing at them. Their owners are in the plan's
///   dependent set and will state them again, as gaps if the target
///   really has disappeared.
///
/// A declaration whose identity survived is untouched, so a relation
/// into it survives too.
pub(crate) fn detach_before_structure(
    connection: &Connection,
    resource_id: ResourceId,
    surviving: &[Symbol],
) -> Result<(), ScanError> {
    let Some(local) = local_id(connection, resource_id)? else {
        return Ok(());
    };
    clear_owned_gaps(connection, local)?;
    release_bindings(connection, local)?;

    let kept: Vec<brainprint_core::SymbolId> = surviving.iter().map(|symbol| symbol.id).collect();
    for (symbol_id, symbol_local) in
        symbol::local_symbol_rows(connection, local).map_err(ScanError::Symbol)?
    {
        if kept.contains(&symbol_id) {
            continue;
        }
        if let Some(entity) = entity_of_symbol(connection, symbol_local)? {
            drop_entity(connection, entity)?;
        }
    }
    Ok(())
}

/// Analyze and publish one Resource's relations.
///
/// `false` means nothing was published: the Resource is not analyzable
/// (unsupported, container-only, deleted) or its current bytes cannot
/// be trusted to describe the indexed revision. In the second case the
/// previous relations are left exactly where they are and the
/// Resource's relation state is marked DIRTY -- never replaced with an
/// empty success.
fn publish_one(
    connection: &Connection,
    grant: &PublicationGrant,
    publication_revision: &str,
    workspace_root: &Path,
    resource_id: ResourceId,
    inventory: &Inventory,
    report: &mut RelationPublication,
) -> Result<bool, ScanError> {
    let Some(resource) = active_resource(connection, resource_id)? else {
        return Ok(false);
    };
    match analyze(
        connection,
        workspace_root,
        &resource,
        inventory,
        grant.generation_id(),
    )? {
        Analysis::NotApplicable => Ok(false),
        Analysis::Unavailable => {
            // We could not look. Saying "no relations" here would be the
            // false zero #17 forbids.
            write_state(
                connection,
                resource_id,
                publication_revision,
                FreshnessState::Dirty,
            )?;
            Ok(false)
        }
        Analysis::Ready { evidence, gaps } => {
            // Nothing to write and nothing to withdraw: a file that
            // states no relation and never did needs no replacement,
            // and calling one would demand an analysis profile it may
            // not have (an empty module has neither Symbols nor
            // Occurrences).
            if evidence.is_empty() && gaps.is_empty() && !has_graph_evidence(connection, &resource)?
            {
                write_state(
                    connection,
                    resource_id,
                    publication_revision,
                    FreshnessState::Current,
                )?;
                return Ok(true);
            }
            let basis = EvidenceBasis {
                owner_resource: resource.id,
                owner_resource_revision: resource.resource_revision.clone(),
                generation_id: grant.generation_id(),
                analysis_profile_id: profile_of(connection, &resource)?,
                // This tier's resolution reads the file and the current
                // inventory, nothing outside them (#17 tasks 4-6).
                resolution_context_key: None,
            };
            for item in &evidence {
                for endpoint in [&item.relation.source, &item.relation.target] {
                    graph::ensure_entity(connection, endpoint).map_err(graph_failed)?;
                }
            }
            for gap in &gaps {
                for candidate in &gap.candidates {
                    graph::ensure_entity(connection, candidate).map_err(graph_failed)?;
                }
            }
            let replacement = replace_resource_graph(connection, grant, &basis, &evidence, &gaps)
                .map_err(evidence_failed)?;
            report.relations_removed += replacement.relations_removed;
            report.unresolved_written += replacement.unresolved_written;
            write_state(
                connection,
                resource_id,
                publication_revision,
                FreshnessState::Current,
            )?;
            Ok(true)
        }
    }
}

/// What one Resource's current source states, or why it states nothing
/// this publication may use.
enum Analysis {
    /// Not a Resource this tier extracts relations from at all.
    NotApplicable,
    /// It should have relations, but the current bytes cannot be shown
    /// to be the indexed ones.
    Unavailable,
    Ready {
        evidence: Vec<RelationEvidence>,
        gaps: Vec<UnresolvedEvidence>,
    },
}

/// Extract and resolve one Resource's relations from its current,
/// verified source.
///
/// The parse is how resolution is recomputed; nothing structural is
/// republished from it. The Symbols and Occurrences this binds to are
/// the ones already stored for the Resource's current revision, which
/// is what makes revalidation cheap in contract terms even when it
/// costs a parse.
fn analyze(
    connection: &Connection,
    workspace_root: &Path,
    resource: &Resource,
    inventory: &Inventory,
    generation_id: i64,
) -> Result<Analysis, ScanError> {
    if resource.kind != ResourceKind::File || resource.state != ResourceState::Active {
        return Ok(Analysis::NotApplicable);
    }
    let Ok(dialect) = parser::dialect_for_resource(resource) else {
        return Ok(Analysis::NotApplicable);
    };
    if !matches!(dialect.capability(), StructuralCapability::WholeFile) {
        // A container-only Resource states no relations, and that is
        // coverage rather than a finding (#16 task 8).
        return Ok(Analysis::NotApplicable);
    }
    // Relations bind to Occurrences. A Resource whose structure is not
    // the current one has nothing this publication may anchor to.
    let structure = structural::read(connection, resource.id)?;
    if structure.map(|state| state.state) != Some(StructuralState::Complete) {
        return Ok(Analysis::Unavailable);
    }

    let path = workspace_root.join(&resource.path_rel);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(Analysis::Unavailable);
    };
    // The same verification every source read makes: these exact bytes
    // are the ones the index describes.
    if resource.content_hash.as_deref() != Some(identity::content_hash_of(&bytes).as_str()) {
        return Ok(Analysis::Unavailable);
    }
    let Ok(tree) = ParserRegistry::new().parse(dialect, &bytes, SourceBasis::of(resource)) else {
        return Ok(Analysis::Unavailable);
    };

    let occurrences = symbol::list_occurrences_for_resource(connection, resource.id)?;
    let own = inventory.symbols_of(resource.id);
    let empty: Vec<Symbol> = Vec::new();
    let own = own.unwrap_or(&empty);

    let imports = resolve_imports(
        dialect,
        resource,
        &inventory.modules,
        extract_imports(&tree, &bytes),
    );
    let bindings = extract_import_bindings(&tree, &bytes);
    let locals = extract_local_names(&tree, &bytes);
    let scope = BindingScope {
        own_symbols: own,
        module_symbols: &inventory.symbols,
        imports: &bindings,
        resolved_imports: &imports,
        local_names: &locals,
    };
    let calls = resolve_calls(extract_call_sites(&tree, &bytes), &scope);
    let types = resolve_type_references(extract_type_references(&tree, &bytes), &scope);
    let keys = extract_key_accesses(&tree, &bytes);

    let mut spans: Vec<(crate::parser::SourceSpan, crate::graph::Relation)> =
        import_relations(resource.id, &imports, generation_id);
    spans.extend(crate::calls::call_relations(
        resource.id,
        &occurrences,
        &calls,
        generation_id,
    ));
    spans.extend(type_relations(
        resource.id,
        &occurrences,
        own,
        &types,
        generation_id,
    ));
    spans.extend(domain_relations(
        resource.id,
        &occurrences,
        &keys,
        generation_id,
    ));

    let evidence = spans
        .into_iter()
        .filter_map(|(span, relation)| {
            // Every relation binds to an Occurrence the structural
            // publication already wrote. One that has none is not
            // published rather than anchored to something invented.
            let occurrence = occurrences.iter().find(|occurrence| {
                occurrence.span.start_byte == span.start_byte
                    && occurrence.span.end_byte == span.end_byte
                    && anchor_kind(occurrence.kind)
            })?;
            Some(RelationEvidence {
                occurrence: OccurrenceRef {
                    kind: occurrence.kind,
                    start_byte: span.start_byte,
                    end_byte: span.end_byte,
                },
                relation,
            })
        })
        .collect::<Vec<_>>();

    let mut gaps = import_gaps(&imports);
    gaps.extend(call_gaps(&calls));
    gaps.extend(type_gaps(&types));
    // A gap must anchor to a real Occurrence too, and never to one a
    // resolved relation already claims (#17 task 7).
    let claimed: Vec<(usize, usize)> = evidence
        .iter()
        .map(|item| (item.occurrence.start_byte, item.occurrence.end_byte))
        .collect();
    let gaps = gaps
        .into_iter()
        .filter(|gap| {
            !claimed.contains(&(gap.occurrence.start_byte, gap.occurrence.end_byte))
                && occurrences.iter().any(|occurrence| {
                    occurrence.kind == gap.occurrence.kind
                        && occurrence.span.start_byte == gap.occurrence.start_byte
                        && occurrence.span.end_byte == gap.occurrence.end_byte
                })
        })
        .collect();

    Ok(Analysis::Ready { evidence, gaps })
}

/// Occurrence kinds a relation may bind to.
const fn anchor_kind(kind: OccurrenceKind) -> bool {
    matches!(
        kind,
        OccurrenceKind::ImportSite
            | OccurrenceKind::CallSite
            | OccurrenceKind::ReferenceSite
            | OccurrenceKind::TypeSite
            | OccurrenceKind::KeySite
    )
}

/// Everything resolution needs about the rest of the Workspace, read
/// once per publication instead of once per Resource.
struct Inventory {
    modules: WorkspaceModules,
    symbols: HashMap<ResourceId, Vec<Symbol>>,
}

impl Inventory {
    fn read(connection: &Connection) -> Result<Self, ScanError> {
        let active = active_resources(connection)?;
        let mut symbols = HashMap::new();
        for resource in &active {
            symbols.insert(
                resource.id,
                symbol::list_for_resource(connection, resource.id)?,
            );
        }
        Ok(Self {
            modules: WorkspaceModules::from_resources(&active),
            symbols,
        })
    }

    fn symbols_of(&self, resource: ResourceId) -> Option<&Vec<Symbol>> {
        self.symbols.get(&resource)
    }
}

/// The Resource inventory as resolution sees it: which stable ids
/// exist and where they live.
///
/// Compared before and after a publication's identity changes, this is
/// what says whether a module *path* moved -- which is the only thing
/// that can make an unresolved import resolvable (or a resolved one
/// stale) without any file's content changing.
pub(crate) fn inventory_fingerprint(
    connection: &Connection,
) -> Result<Vec<(ResourceId, String)>, ScanError> {
    let mut found: Vec<(ResourceId, String)> = active_resources(connection)?
        .into_iter()
        .map(|resource| (resource.id, resource.path_key))
        .collect();
    found.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(found)
}

/// The Resources whose *resolution* the change to `touched` may have
/// invalidated.
///
/// Two questions, both answered from what is already stored:
///
/// 1. who resolved an edge *into* one of these Resources, or into a
///    Symbol inside one, and
/// 2. who holds an unresolved reference naming one of them as a
///    canonical candidate.
///
/// When the Resource inventory itself changed -- a file created,
/// deleted, or moved -- the owners of *import* gaps are added too: a
/// module that did not exist may exist now. That set is bounded by the
/// gaps that are already stored, not by the size of the Workspace, and
/// it is empty for an ordinary content save.
///
/// Name similarity is never consulted.
fn dependents_of(
    connection: &Connection,
    touched: &[ResourceId],
    inventory_changed: bool,
) -> Result<Vec<ResourceId>, ScanError> {
    let mut found: Vec<ResourceId> = Vec::new();
    for resource_id in touched {
        let Some(local) = local_id(connection, *resource_id)? else {
            continue;
        };
        collect(
            connection,
            &mut found,
            "SELECT DISTINCT owner.uid \
             FROM occurrence \
             JOIN resource owner ON owner.id = occurrence.resource_id \
             JOIN relation ON relation.id = occurrence.relation_id \
             JOIN graph_entity ON graph_entity.id = relation.target_entity_id \
             LEFT JOIN symbol ON symbol.id = graph_entity.symbol_id \
             WHERE owner.state = 'ACTIVE' \
               AND (graph_entity.resource_id = ?1 OR symbol.resource_id = ?1)",
            local,
        )?;
        collect(
            connection,
            &mut found,
            "SELECT DISTINCT owner.uid \
             FROM relation_candidate \
             JOIN unresolved_reference \
                  ON unresolved_reference.id = relation_candidate.unresolved_reference_id \
             JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
             JOIN resource owner ON owner.id = occurrence.resource_id \
             JOIN graph_entity ON graph_entity.id = relation_candidate.target_entity_id \
             LEFT JOIN symbol ON symbol.id = graph_entity.symbol_id \
             WHERE owner.state = 'ACTIVE' \
               AND (graph_entity.resource_id = ?1 OR symbol.resource_id = ?1)",
            local,
        )?;
    }

    if inventory_changed {
        let mut statement = connection
            .prepare(
                "SELECT DISTINCT owner.uid \
                 FROM unresolved_reference \
                 JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
                 JOIN resource owner ON owner.id = occurrence.resource_id \
                 WHERE owner.state = 'ACTIVE' \
                   AND unresolved_reference.intended_relation_kind = 'IMPORTS'",
            )
            .map_err(resource::ResourceError::from)?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(resource::ResourceError::from)?;
        for row in rows {
            push_unique(&mut found, row.map_err(resource::ResourceError::from)?);
        }
    }

    // Deterministic order, over stable identity.
    found.sort_by_key(|id| id.to_bytes());
    Ok(found)
}

fn collect(
    connection: &Connection,
    found: &mut Vec<ResourceId>,
    sql: &str,
    local: i64,
) -> Result<(), ScanError> {
    let mut statement = connection
        .prepare(sql)
        .map_err(resource::ResourceError::from)?;
    let rows = statement
        .query_map(params![local], |row| row.get::<_, Vec<u8>>(0))
        .map_err(resource::ResourceError::from)?;
    for row in rows {
        push_unique(found, row.map_err(resource::ResourceError::from)?);
    }
    Ok(())
}

fn push_unique(found: &mut Vec<ResourceId>, uid: Vec<u8>) {
    let id = ResourceId::from_bytes(sixteen(&uid));
    if !found.contains(&id) {
        found.push(id);
    }
}

/// Remove everything the graph holds about a deleted Resource.
///
/// Three different things go, in the order the foreign keys require:
/// this Resource's own gaps and bindings, the canonical edges that
/// *pointed at* it, and the entities that stood for it. The owners of
/// those incoming edges are in the plan's dependent set: their source
/// has not changed, so they will state the same use sites again -- and
/// this time as unresolved gaps, because the target really is gone.
///
/// An edge that does not touch this Resource is not affected, however
/// many Resources evidence it.
pub(crate) fn clear_resource_graph(
    connection: &Connection,
    resource_id: ResourceId,
) -> Result<usize, ScanError> {
    let Some(local) = local_id(connection, resource_id)? else {
        return Ok(0);
    };
    clear_owned_gaps(connection, local)?;
    let mut removed = release_bindings(connection, local)?;
    for entity in entity_ids_of(connection, local)? {
        removed += drop_entity(connection, entity)?;
    }
    component::delete_scoped(
        connection,
        component::RELATION_INDEX,
        component::RESOURCE_SCOPE_KIND,
        &structural::scope_key(resource_id),
    )?;
    Ok(removed)
}

/// Delete one Resource's unresolved references and their candidates.
/// They are anchored to its Occurrences, so they go before those do.
fn clear_owned_gaps(connection: &Connection, local: i64) -> Result<(), ScanError> {
    connection
        .execute(
            "DELETE FROM relation_candidate WHERE unresolved_reference_id IN \
             (SELECT unresolved_reference.id FROM unresolved_reference \
              JOIN occurrence ON occurrence.id = unresolved_reference.occurrence_id \
              WHERE occurrence.resource_id = ?1)",
            params![local],
        )
        .map_err(resource::ResourceError::from)?;
    connection
        .execute(
            "DELETE FROM unresolved_reference WHERE occurrence_id IN \
             (SELECT id FROM occurrence WHERE resource_id = ?1)",
            params![local],
        )
        .map_err(resource::ResourceError::from)?;
    Ok(())
}

/// Release one Resource's relation bindings, removing every canonical
/// edge whose last evidence that was. Returns how many edges went.
fn release_bindings(connection: &Connection, local: i64) -> Result<usize, ScanError> {
    connection
        .execute(
            "UPDATE occurrence SET relation_id = NULL, resolution_context_id = NULL \
             WHERE resource_id = ?1",
            params![local],
        )
        .map_err(resource::ResourceError::from)?;
    Ok(collect_unevidenced(connection)?)
}

/// Remove an entity and everything that referenced it: the candidate
/// rows naming it, the edges at either end of it, and the bindings of
/// other Resources' Occurrences to those edges.
fn drop_entity(connection: &Connection, entity: i64) -> Result<usize, ScanError> {
    connection
        .execute(
            "UPDATE occurrence SET relation_id = NULL WHERE relation_id IN \
             (SELECT id FROM relation WHERE target_entity_id = ?1 OR source_entity_id = ?1)",
            params![entity],
        )
        .map_err(resource::ResourceError::from)?;
    connection
        .execute(
            "DELETE FROM relation_candidate WHERE target_entity_id = ?1",
            params![entity],
        )
        .map_err(resource::ResourceError::from)?;
    let removed = connection
        .execute(
            "DELETE FROM relation WHERE target_entity_id = ?1 OR source_entity_id = ?1",
            params![entity],
        )
        .map_err(resource::ResourceError::from)?;
    connection
        .execute("DELETE FROM graph_entity WHERE id = ?1", params![entity])
        .map_err(resource::ResourceError::from)?;
    Ok(removed)
}

/// Delete every canonical relation nothing evidences any more.
fn collect_unevidenced(connection: &Connection) -> Result<usize, resource::ResourceError> {
    Ok(connection.execute(
        "DELETE FROM relation WHERE NOT EXISTS \
         (SELECT 1 FROM occurrence WHERE occurrence.relation_id = relation.id)",
        [],
    )?)
}

/// The `graph_entity` standing for one Symbol row, if it has one.
fn entity_of_symbol(connection: &Connection, symbol_local: i64) -> Result<Option<i64>, ScanError> {
    Ok(connection
        .query_row(
            "SELECT id FROM graph_entity WHERE symbol_id = ?1",
            params![symbol_local],
            |row| row.get(0),
        )
        .optional()
        .map_err(resource::ResourceError::from)?)
}

/// Every `graph_entity` that stands for this Resource or a Symbol in it.
fn entity_ids_of(connection: &Connection, local: i64) -> Result<Vec<i64>, ScanError> {
    let mut statement = connection
        .prepare(
            "SELECT graph_entity.id FROM graph_entity \
             LEFT JOIN symbol ON symbol.id = graph_entity.symbol_id \
             WHERE graph_entity.resource_id = ?1 OR symbol.resource_id = ?1",
        )
        .map_err(resource::ResourceError::from)?;
    let rows = statement
        .query_map(params![local], |row| row.get::<_, i64>(0))
        .map_err(resource::ResourceError::from)?;
    let mut found = Vec::new();
    for row in rows {
        found.push(row.map_err(resource::ResourceError::from)?);
    }
    Ok(found)
}

/// One Resource's relation state, or `None` if it has never had one.
///
/// CURRENT means its relations were resolved against the structure as
/// it is now; DIRTY means something they resolved into moved and the
/// re-resolution could not be completed, so what is stored is the last
/// valid answer.
pub fn state_of(
    connection: &Connection,
    resource_id: ResourceId,
) -> Result<Option<FreshnessState>, ScanError> {
    Ok(component::read_scoped(
        connection,
        component::RELATION_INDEX,
        component::RESOURCE_SCOPE_KIND,
        &structural::scope_key(resource_id),
    )?
    .map(|row| row.freshness_state))
}

fn write_state(
    connection: &Connection,
    resource_id: ResourceId,
    publication_revision: &str,
    freshness: FreshnessState,
) -> Result<(), ScanError> {
    component::write_scoped(
        connection,
        component::RELATION_INDEX,
        component::RESOURCE_SCOPE_KIND,
        &structural::scope_key(resource_id),
        &ComponentRow {
            basis_workspace_revision: publication_revision.to_owned(),
            stable_generation_id: None,
            processing_state: match freshness {
                FreshnessState::Current => ProcessingState::Ready,
                FreshnessState::Dirty => ProcessingState::Queued,
            },
            freshness_state: freshness,
            last_error_code: None,
            detail_state: None,
        },
    )?;
    Ok(())
}

/// Whether this Resource currently proves or states anything in the
/// graph -- a bound Occurrence or an unresolved reference of its own.
fn has_graph_evidence(connection: &Connection, resource: &Resource) -> Result<bool, ScanError> {
    let Some(local) = local_id(connection, resource.id)? else {
        return Ok(false);
    };
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM occurrence \
             WHERE resource_id = ?1 AND (relation_id IS NOT NULL OR id IN \
                   (SELECT occurrence_id FROM unresolved_reference))",
            params![local],
            |row| row.get(0),
        )
        .map_err(resource::ResourceError::from)?;
    Ok(count > 0)
}

fn profile_of(connection: &Connection, resource: &Resource) -> Result<i64, ScanError> {
    if let Some(occurrence) =
        symbol::list_occurrences_for_resource(connection, resource.id)?.first()
    {
        return Ok(occurrence.analysis_profile_id);
    }
    if let Some(symbol) = symbol::list_for_resource(connection, resource.id)?.first() {
        return Ok(symbol.analysis_profile_id);
    }
    Err(ScanError::InvariantViolated {
        detail: format!(
            "{} has relation evidence but no analysis profile",
            resource.path_rel
        ),
    })
}

fn active_resources(connection: &Connection) -> Result<Vec<Resource>, ScanError> {
    let mut statement = connection
        .prepare(&format!(
            "{} WHERE r.state = 'ACTIVE'",
            resource::SELECT_RESOURCE_SQL
        ))
        .map_err(resource::ResourceError::from)?;
    let rows = statement
        .query_map([], resource::raw_resource_from_row)
        .map_err(resource::ResourceError::from)?;
    let mut found = Vec::new();
    for row in rows {
        found.push(resource::decode_resource(
            row.map_err(resource::ResourceError::from)?,
        )?);
    }
    Ok(found)
}

fn active_resource(
    connection: &Connection,
    resource_id: ResourceId,
) -> Result<Option<Resource>, ScanError> {
    let raw = connection
        .query_row(
            &format!(
                "{} WHERE r.uid = ?1 AND r.state = 'ACTIVE'",
                resource::SELECT_RESOURCE_SQL
            ),
            params![resource_id.to_bytes().to_vec()],
            resource::raw_resource_from_row,
        )
        .optional()
        .map_err(resource::ResourceError::from)?;
    Ok(raw.map(resource::decode_resource).transpose()?)
}

fn local_id(connection: &Connection, resource_id: ResourceId) -> Result<Option<i64>, ScanError> {
    Ok(connection
        .query_row(
            "SELECT id FROM resource WHERE uid = ?1",
            params![resource_id.to_bytes().to_vec()],
            |row| row.get(0),
        )
        .optional()
        .map_err(resource::ResourceError::from)?)
}

fn graph_failed(error: graph::GraphError) -> ScanError {
    ScanError::InvariantViolated {
        detail: format!("graph entity: {error}"),
    }
}

fn evidence_failed(error: EvidenceError) -> ScanError {
    ScanError::InvariantViolated {
        detail: format!("relation evidence: {error}"),
    }
}

fn sixteen(raw: &[u8]) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    let take = raw.len().min(16);
    bytes[..take].copy_from_slice(&raw[..take]);
    bytes
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        evidence::list_unresolved_for_resource,
        generation::current_stable,
        graph::{GraphEndpoint, GraphStore, RelationKind},
        reconcile::Reconcile,
        refresh::TargetedRefresh,
        relations::RelationIndex,
        resource::ResourceStore,
        scan::BaselineScan,
        symbol::SymbolStore,
        watch::{RawWatchEvent, WatchIngest},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const BASE_TS: &str = "\
export function shared(): number {
  return 1
}

export class Base {}
";

    const APP_TS: &str = "\
import { shared, Base } from './base'

export class App extends Base {
  run(): number {
    const url = process.env.APP_URL
    return shared() + shared()
  }
}
";

    const OTHER_TS: &str = "\
import { shared } from './base'

export function other(): number {
  return shared()
}
";

    /// The config side of task 12, so the lifecycle covers both cheap
    /// domain relations and not only the env one.
    const LOADER_CS: &str = "\
using System.Configuration;

public class Loader
{
    public string Read()
    {
        return ConfigurationManager.AppSettings[\"RETRIES\"];
    }
}
";

    /// Nothing to relate: the control for "was anything rebuilt?".
    const LONE_TS: &str = "\
export function lone(): number {
  return 7
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
                "brainprint-lifecycle-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            std::fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/base.ts", BASE_TS);
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/other.ts", OTHER_TS);
            fixture.write("src/lone.ts", LONE_TS);
            fixture.write("src/Loader.cs", LOADER_CS);
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            std::fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn baseline(&self) {
            BaselineScan::open(&self.db_path())
                .expect("index.db")
                .run_initial_scan(&self.root, &WorkspaceConfig::default(), "workspace-rev-1")
                .expect("baseline scan");
        }

        /// One saved file, through the watcher and the targeted refresh.
        fn save(&self, rel: &str, contents: &str) {
            self.write(rel, contents);
            WatchIngest::open(&self.db_path())
                .expect("index.db")
                .ingest_all(
                    &self.root,
                    &WorkspaceConfig::default(),
                    &[RawWatchEvent::Modified {
                        path: self.path(rel),
                    }],
                )
                .expect("ingest");
            TargetedRefresh::open(&self.db_path())
                .expect("index.db")
                .run(&self.root, &WorkspaceConfig::default())
                .expect("refresh");
        }

        /// A change the watcher never saw.
        fn reconcile(&self) {
            Reconcile::open(&self.db_path())
                .expect("index.db")
                .run(&self.root, &WorkspaceConfig::default())
                .expect("reconcile");
        }

        fn resources(&self) -> ResourceStore {
            ResourceStore::open(&self.db_path()).expect("index.db")
        }

        fn resource(&self, rel: &str) -> Resource {
            self.resources()
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn file(&self, rel: &str) -> GraphEndpoint {
            GraphEndpoint::Resource(self.resource(rel).id)
        }

        fn declaration(&self, rel: &str, qualified_name: &str) -> GraphEndpoint {
            GraphEndpoint::Symbol(
                SymbolStore::open(&self.db_path())
                    .expect("index.db")
                    .list_for_resource(self.resource(rel).id)
                    .expect("symbols")
                    .into_iter()
                    .find(|symbol| symbol.qualified_name == qualified_name)
                    .expect("the declaration is indexed")
                    .id,
            )
        }

        fn index(&self) -> RelationIndex {
            RelationIndex::open(&self.db_path()).expect("index.db")
        }

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.db_path()).expect("index.db")
        }

        /// Every relation leaving one endpoint, as `KIND` labels.
        fn outgoing_kinds(&self, endpoint: &GraphEndpoint) -> Vec<&'static str> {
            let mut kinds: Vec<&'static str> = self
                .index()
                .outgoing(endpoint, &[])
                .expect("query")
                .confirmed
                .iter()
                .map(|relation| relation.kind.as_str())
                .collect();
            kinds.sort_unstable();
            kinds
        }

        fn outgoing(&self, endpoint: &GraphEndpoint, kind: RelationKind) -> Vec<GraphEndpoint> {
            self.index()
                .outgoing(endpoint, &[kind])
                .expect("query")
                .confirmed
                .iter()
                .map(|relation| relation.target.clone())
                .collect()
        }

        fn evidence_count(&self, endpoint: &GraphEndpoint, kind: RelationKind) -> usize {
            self.index()
                .outgoing(endpoint, &[kind])
                .expect("query")
                .confirmed
                .iter()
                .map(|relation| relation.evidence.len())
                .sum()
        }

        fn gaps(&self, rel: &str) -> Vec<String> {
            let store = self.store();
            list_unresolved_for_resource(store.connection(), self.resource(rel).id)
                .expect("gaps")
                .into_iter()
                .map(|gap| format!("{}:{}", gap.intended, gap.lookup_name))
                .collect()
        }

        fn candidates(&self, rel: &str, lookup: &str) -> usize {
            let store = self.store();
            list_unresolved_for_resource(store.connection(), self.resource(rel).id)
                .expect("gaps")
                .into_iter()
                .find(|gap| gap.lookup_name == lookup)
                .map_or(0, |gap| gap.candidates.len())
        }

        fn relation_state(&self, rel: &str) -> Option<FreshnessState> {
            let store = self.store();
            state_of(store.connection(), self.resource(rel).id).expect("state")
        }

        /// The revision a Resource's relation state was last written
        /// against -- what says whether this publication touched it.
        fn relation_basis(&self, rel: &str) -> Option<String> {
            let store = self.store();
            component::read_scoped(
                store.connection(),
                component::RELATION_INDEX,
                component::RESOURCE_SCOPE_KIND,
                &structural::scope_key(self.resource(rel).id),
            )
            .expect("component")
            .map(|row| row.basis_workspace_revision)
        }

        fn relation_count(&self) -> i64 {
            self.store()
                .connection()
                .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))
                .expect("count")
        }

        fn table_count(&self, table: &str) -> i64 {
            self.store()
                .connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn env_key(key: &str) -> GraphEndpoint {
        GraphEndpoint::Domain(crate::graph::DomainEntity {
            kind: "ENV".to_owned(),
            normalized_identity: key.to_owned(),
            namespace: None,
            method: None,
            display_label: format!("ENV {key}"),
        })
    }

    #[test]
    fn initial_indexing_publishes_relations_on_the_stable_generation() {
        let fixture = Fixture::create("initial");
        fixture.baseline();

        let app = fixture.file("src/app.ts");
        let run = fixture.declaration("src/app.ts", "App.run");
        let app_class = fixture.declaration("src/app.ts", "App");

        // Symbols READY while supported Relations are silently absent is
        // the state this task closes.
        assert!(!fixture.outgoing_kinds(&app).is_empty(), "imports");
        assert_eq!(fixture.outgoing_kinds(&app), vec!["IMPORTS"]);
        assert_eq!(
            fixture.outgoing(&app, RelationKind::Imports),
            vec![fixture.file("src/base.ts")]
        );
        assert_eq!(
            fixture.outgoing(&run, RelationKind::Calls),
            vec![fixture.declaration("src/base.ts", "shared")]
        );
        assert_eq!(
            fixture.outgoing(&app_class, RelationKind::Extends),
            vec![fixture.declaration("src/base.ts", "Base")]
        );
        assert_eq!(
            fixture.outgoing(&run, RelationKind::UsesEnv),
            vec![env_key("APP_URL")],
            "the cheap domain relations ride the same publication"
        );

        assert_eq!(
            fixture.outgoing(
                &fixture.declaration("src/Loader.cs", "Loader.Read"),
                RelationKind::UsesConfig
            ),
            vec![GraphEndpoint::Domain(crate::graph::DomainEntity {
                kind: "CONFIG".to_owned(),
                normalized_identity: "RETRIES".to_owned(),
                namespace: Some("AppSettings".to_owned()),
                method: None,
                display_label: "CONFIG AppSettings:RETRIES".to_owned(),
            })],
            "both cheap domain relations ride the initial publication"
        );

        // The same generation basis as the structure they describe.
        let store = fixture.store();
        let stable = current_stable(store.connection())
            .expect("stable")
            .expect("published");
        let created: i64 = store
            .connection()
            .query_row("SELECT MIN(created_generation) FROM relation", [], |row| {
                row.get(0)
            })
            .expect("generation");
        assert_eq!(created, stable.id);
        drop(store);
        assert_eq!(
            fixture.relation_state("src/app.ts"),
            Some(FreshnessState::Current)
        );
    }

    #[test]
    fn a_single_save_replaces_only_that_files_relation_evidence() {
        let fixture = Fixture::create("single-save");
        fixture.baseline();
        let run = fixture.declaration("src/app.ts", "App.run");
        let other = fixture.declaration("src/other.ts", "other");
        let lone_before = fixture.relation_basis("src/lone.ts");
        assert_eq!(fixture.evidence_count(&run, RelationKind::Calls), 2);

        // One call site goes; the edge stays, with one evidence left.
        fixture.save(
            "src/app.ts",
            "import { shared, Base } from './base'\n\nexport class App extends Base {\n  run(): number {\n    const url = process.env.APP_URL\n    return shared()\n  }\n}\n",
        );
        let run = fixture.declaration("src/app.ts", "App.run");
        assert_eq!(fixture.evidence_count(&run, RelationKind::Calls), 1);

        // The other call goes too: now the edge has nothing left.
        fixture.save(
            "src/app.ts",
            "import { Base } from './base'\n\nexport class App extends Base {\n  run(): number {\n    const key = process.env.OTHER_URL\n    return 1\n  }\n}\n",
        );
        let run = fixture.declaration("src/app.ts", "App.run");
        assert!(
            fixture.outgoing(&run, RelationKind::Calls).is_empty(),
            "the last evidence for the edge is gone"
        );
        // A newly written access appears, and the old one does not.
        assert_eq!(
            fixture.outgoing(&run, RelationKind::UsesEnv),
            vec![env_key("OTHER_URL")]
        );

        // Another Resource's evidence is untouched by all of it.
        assert_eq!(
            fixture.outgoing(&other, RelationKind::Calls),
            vec![fixture.declaration("src/base.ts", "shared")],
            "other.ts still calls shared"
        );
        assert_eq!(
            fixture.relation_basis("src/lone.ts"),
            lone_before,
            "a file nothing pointed at is not rebuilt"
        );
    }

    #[test]
    fn a_changed_definition_revalidates_its_dependents_and_nothing_else() {
        let fixture = Fixture::create("invalidation");
        fixture.baseline();
        let app_revision = fixture.resource("src/app.ts").resource_revision.clone();
        let app_symbols = SymbolStore::open(&fixture.db_path())
            .expect("index.db")
            .list_for_resource(fixture.resource("src/app.ts").id)
            .expect("symbols");
        let lone_before = fixture.relation_basis("src/lone.ts");

        // base.ts renames its export. app.ts's source did not move.
        fixture.save(
            "src/base.ts",
            "export function renamed(): number {\n  return 1\n}\n\nexport class Base {}\n",
        );

        let run = fixture.declaration("src/app.ts", "App.run");
        assert!(
            fixture.outgoing(&run, RelationKind::Calls).is_empty(),
            "RESOLVED -> UNRESOLVED: the target it named is gone"
        );
        assert!(
            fixture
                .gaps("src/app.ts")
                .iter()
                .any(|gap| gap.contains("shared")),
            "and it comes back as a gap: {:?}",
            fixture.gaps("src/app.ts")
        );
        // Evidence, not resolution, is what stayed put.
        assert_eq!(
            fixture.resource("src/app.ts").resource_revision,
            app_revision,
            "app.ts was not re-published"
        );
        assert_eq!(
            SymbolStore::open(&fixture.db_path())
                .expect("index.db")
                .list_for_resource(fixture.resource("src/app.ts").id)
                .expect("symbols"),
            app_symbols,
            "its Symbols and spans are the same rows"
        );
        assert_eq!(
            fixture.relation_basis("src/lone.ts"),
            lone_before,
            "an unrelated Resource is not revalidated"
        );

        // UNRESOLVED -> RESOLVED, the same way back.
        fixture.save("src/base.ts", BASE_TS);
        let run = fixture.declaration("src/app.ts", "App.run");
        assert_eq!(
            fixture.outgoing(&run, RelationKind::Calls),
            vec![fixture.declaration("src/base.ts", "shared")]
        );
        assert!(
            !fixture
                .gaps("src/app.ts")
                .iter()
                .any(|gap| gap.contains("shared")),
            "the gap closed"
        );
    }

    #[test]
    fn revalidation_can_change_a_candidate_set() {
        let fixture = Fixture::create("candidates");
        fixture.write(
            "src/base.ts",
            "export function shared(): number {\n  return 1\n}\n\nfunction shared(): number {\n  return 2\n}\n\nexport class Base {}\n",
        );
        fixture.baseline();
        assert_eq!(
            fixture.candidates("src/app.ts", "shared"),
            2,
            "two declarations of the name: ambiguous, with both candidates"
        );

        fixture.save(
            "src/base.ts",
            "export function shared(): number {\n  return 1\n}\n\nfunction shared(): number {\n  return 2\n}\n\nfunction shared(): number {\n  return 3\n}\n\nexport class Base {}\n",
        );
        assert_eq!(
            fixture.candidates("src/app.ts", "shared"),
            3,
            "the candidate set follows the target module"
        );
    }

    #[test]
    fn a_partial_parse_keeps_the_last_valid_relations() {
        let fixture = Fixture::create("partial");
        fixture.baseline();
        let run = fixture.declaration("src/app.ts", "App.run");
        let before = fixture.outgoing(&run, RelationKind::Calls);
        assert!(!before.is_empty());

        // Half-typed source: the Resource advances, the structure stays
        // the last valid one.
        fixture.save(
            "src/app.ts",
            "import { shared, Base } from './base'\n\nexport class App extends Base {\n  run(): number {\n    return shared(\n",
        );

        assert_eq!(
            fixture
                .index()
                .outgoing(&run, &[RelationKind::Calls])
                .expect("query")
                .confirmed
                .len(),
            before.len(),
            "a parse that did not complete is not an empty relation set"
        );
        assert_eq!(
            structural::read(
                fixture.store().connection(),
                fixture.resource("src/app.ts").id
            )
            .expect("state")
            .map(|state| state.state),
            Some(StructuralState::Partial)
        );
    }

    #[test]
    fn an_unreadable_resource_is_marked_dirty_rather_than_emptied() {
        let fixture = Fixture::create("unreadable");
        fixture.baseline();
        let run = fixture.declaration("src/app.ts", "App.run");
        let before = fixture.outgoing(&run, RelationKind::Calls);

        // The dependent cannot be read when its target changes.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                fixture.path("src/app.ts"),
                std::fs::Permissions::from_mode(0o000),
            )
            .expect("chmod");
        }
        fixture.save(
            "src/base.ts",
            "export function shared(): number {\n  return 2\n}\n\nexport class Base {}\n",
        );

        #[cfg(unix)]
        {
            assert_eq!(
                fixture.relation_state("src/app.ts"),
                Some(FreshnessState::Dirty),
                "we could not look, and the state says so"
            );
            assert_eq!(
                fixture.outgoing(&run, RelationKind::Calls),
                before,
                "the last valid relations stand"
            );
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                fixture.path("src/app.ts"),
                std::fs::Permissions::from_mode(0o644),
            )
            .expect("chmod");
        }
    }

    #[test]
    fn an_uncommitted_publication_is_never_visible_to_a_query() {
        let fixture = Fixture::create("building");
        fixture.baseline();
        let before = fixture.relation_count();

        // A writer's open transaction, and a reader on its own
        // connection: the reader sees the last stable graph, never the
        // half-written one.
        let writer = fixture.store();
        let transaction = writer.connection().unchecked_transaction().expect("begin");
        transaction
            .execute("DELETE FROM occurrence WHERE relation_id IS NOT NULL", [])
            .expect("write");
        assert_eq!(
            fixture.relation_count(),
            before,
            "a BUILDING mixture is not current graph truth"
        );
        drop(transaction);
        assert_eq!(fixture.relation_count(), before, "and it rolled back");
    }

    #[test]
    fn deleting_a_resource_clears_its_graph_and_invalidates_what_pointed_at_it() {
        let fixture = Fixture::create("delete");
        fixture.baseline();
        let other = fixture.declaration("src/other.ts", "other");
        let other_import = fixture.file("src/other.ts");
        assert!(!fixture.outgoing(&other, RelationKind::Calls).is_empty());

        std::fs::remove_file(fixture.path("src/base.ts")).expect("delete");
        fixture.reconcile();

        // Nothing of the deleted Resource is left in the graph.
        let deleted = fixture
            .resources()
            .list_active()
            .expect("list")
            .iter()
            .all(|resource| resource.path_key != "src/base.ts");
        assert!(deleted, "the tombstone is not active");

        // What pointed at it no longer resolves -- and says so.
        let run = fixture.declaration("src/app.ts", "App.run");
        assert!(
            fixture.outgoing(&run, RelationKind::Calls).is_empty(),
            "an edge into a deleted target is not current"
        );
        assert!(
            !fixture.gaps("src/app.ts").is_empty(),
            "the use sites are still stated, as gaps"
        );
        // And the rest of the graph is untouched.
        assert_eq!(
            fixture.outgoing_kinds(&other_import),
            Vec::<&str>::new(),
            "other.ts imported the deleted module, so that edge went too"
        );
        assert!(
            !fixture.gaps("src/other.ts").is_empty(),
            "other.ts keeps its own evidence, as gaps"
        );
        assert!(
            fixture.relation_state("src/lone.ts").is_some(),
            "an unrelated Resource still has its own state"
        );
    }

    #[test]
    fn an_identity_preserving_move_keeps_identity_and_uses_the_new_locator() {
        let fixture = Fixture::create("move");
        fixture.baseline();
        let before = fixture.resource("src/base.ts").id;
        let lone_before = fixture.relation_basis("src/lone.ts");

        std::fs::create_dir_all(fixture.root.join("src/core")).expect("dir");
        std::fs::rename(
            fixture.path("src/base.ts"),
            fixture.path("src/core/base.ts"),
        )
        .expect("move");
        // The watcher's rename evidence is what makes this a move
        // rather than a delete and a create (#16 task 4).
        WatchIngest::open(&fixture.db_path())
            .expect("index.db")
            .ingest_all(
                &fixture.root,
                &WorkspaceConfig::default(),
                &[RawWatchEvent::RenamedPair {
                    from: fixture.path("src/base.ts"),
                    to: fixture.path("src/core/base.ts"),
                }],
            )
            .expect("ingest");
        fixture.reconcile();

        let moved = fixture.resource("src/core/base.ts");
        assert_eq!(moved.id, before, "identity is preserved across the move");
        assert_eq!(moved.path_rel, "src/core/base.ts");
        assert!(
            fixture
                .resources()
                .get_active_by_path_key("src/base.ts")
                .expect("lookup")
                .is_none(),
            "the old locator is not current"
        );

        // The importers' specifier no longer names the module, so their
        // resolution was revalidated rather than left pointing at it.
        assert!(
            fixture
                .outgoing(&fixture.file("src/app.ts"), RelationKind::Imports)
                .is_empty(),
            "'./base' does not resolve from src/app.ts any more"
        );
        assert!(
            fixture
                .gaps("src/app.ts")
                .iter()
                .any(|gap| gap.contains("./base")),
            "and the import is stated as a gap: {:?}",
            fixture.gaps("src/app.ts")
        );
        assert_eq!(
            fixture.relation_basis("src/lone.ts"),
            lone_before,
            "a Resource the move does not affect is not rebuilt"
        );
    }

    #[test]
    fn reconcile_recovers_relations_for_changed_resources_only() {
        let fixture = Fixture::create("reconcile");
        fixture.baseline();
        let lone_before = fixture.relation_basis("src/lone.ts");
        let other_before = fixture.relation_basis("src/other.ts");

        // A change the watcher never saw.
        fixture.write(
            "src/other.ts",
            "import { shared } from './base'\n\nexport function other(): number {\n  const port = process.env.PORT\n  return shared() + shared()\n}\n",
        );
        fixture.reconcile();

        let other = fixture.declaration("src/other.ts", "other");
        assert_eq!(
            fixture.evidence_count(&other, RelationKind::Calls),
            2,
            "the missed change is recovered"
        );
        assert_eq!(
            fixture.outgoing(&other, RelationKind::UsesEnv),
            vec![env_key("PORT")]
        );
        assert_ne!(
            fixture.relation_basis("src/other.ts"),
            other_before,
            "the changed Resource was republished"
        );
        assert_eq!(
            fixture.relation_basis("src/lone.ts"),
            lone_before,
            "and nothing else was"
        );
        assert_eq!(
            fixture.relation_state("src/other.ts"),
            Some(FreshnessState::Current)
        );
    }

    #[test]
    fn a_refresh_in_one_workspace_never_touches_another() {
        let first = Fixture::create("isolation-a");
        let second = Fixture::create("isolation-b");
        first.baseline();
        second.baseline();
        let untouched = second.relation_count();
        let second_state = second.relation_basis("src/app.ts");

        first.save(
            "src/app.ts",
            "import { Base } from './base'\n\nexport class App extends Base {\n  run(): number {\n    return 1\n  }\n}\n",
        );

        assert_eq!(
            second.relation_count(),
            untouched,
            "the other Workspace's graph did not move"
        );
        assert_eq!(second.relation_basis("src/app.ts"), second_state);
        assert!(
            !second
                .outgoing(
                    &second.declaration("src/app.ts", "App.run"),
                    RelationKind::Calls
                )
                .is_empty(),
            "identical paths and Symbol names, separate graphs"
        );
    }

    #[test]
    fn the_lifecycle_stores_identity_and_never_source_text() {
        let fixture = Fixture::create("no-source");
        fixture.baseline();
        fixture.save(
            "src/app.ts",
            "import { shared, Base } from './base'\n\nexport class App extends Base {\n  run(): number {\n    return shared()\n  }\n}\n",
        );

        assert!(fixture.relation_count() > 0, "there are relations to find");
        let bytes = std::fs::read(fixture.db_path()).expect("index.db bytes");
        for body in ["return shared()", "export class App extends", "const url ="] {
            assert!(
                !bytes
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db mirrors {body:?}"
            );
        }
    }

    #[test]
    fn gaps_and_candidates_ride_the_same_atomic_replacement() {
        let fixture = Fixture::create("gap-lifecycle");
        fixture.baseline();
        // `obj.foo()` cannot be resolved structurally: a gap, from the
        // ordinary publication path.
        fixture.save(
            "src/app.ts",
            "import { shared, Base } from './base'\n\nexport class App extends Base {\n  run(obj: Thing): number {\n    obj.foo()\n    return shared()\n  }\n}\n",
        );
        assert!(
            fixture
                .gaps("src/app.ts")
                .iter()
                .any(|gap| gap.contains("foo")),
            "{:?}",
            fixture.gaps("src/app.ts")
        );
        let with_gap = fixture.table_count("unresolved_reference");

        // Removing the use site removes the gap with it.
        fixture.save(
            "src/app.ts",
            "import { shared, Base } from './base'\n\nexport class App extends Base {\n  run(): number {\n    return shared()\n  }\n}\n",
        );
        assert!(
            !fixture
                .gaps("src/app.ts")
                .iter()
                .any(|gap| gap.contains("foo")),
            "the gap was replaced along with the evidence"
        );
        assert!(fixture.table_count("unresolved_reference") < with_gap);
    }
}
