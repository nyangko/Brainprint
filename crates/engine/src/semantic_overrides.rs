//! Deriving `OVERRIDES` from proven inheritance.
//!
//! No measured backend answers "which member does this override".
//! #19 task 5 measured Pyright returning `MethodNotFound` for both
//! `textDocument/implementation` and `textDocument/prepareTypeHierarchy`;
//! the TypeScript native LSP answers `implementation` but has no
//! override request either. So in both languages the answer has to be
//! derived -- from facts that are already proven, never from names.
//!
//! The derivation itself is language-neutral and lives here for both
//! backends (#19 tasks 7 and 10). Only the syntax of an override
//! *claim* differs, and each backend recognises its own: Python's
//! `typing.override` decorator is at the bottom of this file, and
//! TypeScript's `override` keyword is in its adapter.
//!
//! The derivation is:
//!
//! ```text
//! subclass method M in class C
//!   → C's ancestors, from canonical EXTENDS edges only
//!   → members declared by those ancestors
//!   → the same name, at the shallowest depth that declares it
//!   → exactly one, and a compatible member kind
//!   → M OVERRIDES that member
//! ```
//!
//! Every step is bounded by a relation someone proved. What is
//! explicitly *not* here is a search of the Workspace for a method
//! called `run`: that is the name guessing this tier exists to replace,
//! and the fixture keeps an unrelated `Other.run` and `Unrelated.run`
//! around so a test fails if it ever creeps back.
//!
//! ## What stays unproven
//!
//! A canonical EXTENDS edge is a *set* membership, not a position in a
//! base list, so C3 linearization is not derivable from the graph. When
//! two ancestors at the same depth declare the same name --
//! `class Multi(Base, Mixin)` where both define `run` -- Python's MRO
//! picks the first base, and this refuses to, because the order is not
//! in the evidence. The site is reported unproven rather than guessed.
//! TypeScript has single inheritance, so the same-depth case cannot
//! arise there at all; the bound costs it nothing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::Connection;

use crate::{
    evidence::OccurrenceRef,
    graph::{GraphEndpoint, GraphError, RelationKind, relations_from},
    resolution::{Dispatch, EvidenceBasis, Support},
    resource::Resource,
    semantic::{SemanticCapability, SemanticEvidence, SemanticOutcome},
    symbol::{Occurrence, OccurrenceKind, Symbol, SymbolKind, list_for_resource},
};

/// How far up an inheritance chain the derivation walks.
///
/// A bound rather than a guess at depth: a cycle in the edges (which a
/// broken or mid-edit Workspace can produce) must not become an
/// infinite walk, and the visited set already stops honest repeats.
const MAX_DEPTH: usize = 16;

/// Why a method that looks like an override did not become one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnprovenReason {
    /// Two or more ancestors at the same inheritance depth declare the
    /// name. Python resolves this by base-list order; the graph does
    /// not record order, so nothing here chooses.
    AmbiguousAncestors { candidates: usize },
    /// The class inherits from something outside the Workspace, so its
    /// members cannot be read without indexing a dependency.
    OpaqueAncestor,
    /// Nothing in the resolved ancestors declares the name.
    NoAncestorMember,
}

/// A site that states an override the evidence could not prove.
///
/// Only produced where the source itself claims one -- a
/// `typing.override` decorator -- so this is a gap someone asserted,
/// not every method that happens not to override anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnprovenOverride {
    pub method: SymbolId,
    pub occurrence: OccurrenceRef,
    pub reason: UnprovenReason,
}

/// What one Resource's override derivation produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Derivation {
    pub evidence: Vec<SemanticEvidence>,
    pub unproven: Vec<UnprovenOverride>,
    /// Ancestor Resources whose declarations the derivation read.
    ///
    /// These belong in the task 3 basis: `Impl.run OVERRIDES Base.run`
    /// is only true while `Base.run` is still declared where it was, so
    /// the publication has to go stale when `base.py` moves.
    pub ancestor_resources: BTreeSet<ResourceId>,
}

/// One class's proven ancestors, nearest first.
struct Ancestry {
    /// Ancestor class Symbols by inheritance depth, 1 = a direct base.
    by_depth: BTreeMap<usize, Vec<SymbolId>>,
    /// Whether any ancestor is outside the Workspace, which makes a
    /// "declares nothing" answer unreliable.
    opaque: bool,
}

/// Derive every override the classes in `owner` can prove.
///
/// `extra_bases` are `(subclass, base)` pairs established by the same
/// refresh that has not merged yet, so a base resolved semantically in
/// this pass counts immediately.
pub fn derive(
    connection: &Connection,
    owner: &Resource,
    occurrences: &[Occurrence],
    extra_bases: &[(SymbolId, SymbolId)],
    context_key: &str,
    basis: &EvidenceBasis,
    declared_overrides: &BTreeSet<SymbolId>,
) -> Result<Derivation, GraphError> {
    let own = list_for_resource(connection, owner.id).map_err(sql)?;
    let mut produced = Derivation::default();
    // One cache per Resource: a base class's member list is read once
    // however many subclasses in this file reach it.
    let mut members: BTreeMap<SymbolId, Vec<Symbol>> = BTreeMap::new();

    for class in own.iter().filter(|symbol| symbol.kind == SymbolKind::Class) {
        // A class with no proven ancestor is still visited: one of its
        // members may claim an override, and that claim is worth
        // reporting precisely because nothing supports it.
        let ancestry = ancestors(connection, class.id, extra_bases)?;
        for member in own
            .iter()
            .filter(|symbol| symbol.parent_id == Some(class.id))
            .filter(|symbol| overridable(symbol.kind))
        {
            let Some(site) = definition_site(occurrences, member.id) else {
                // No Occurrence marks this declaration, so there is
                // nothing to anchor an edge to. Task 4 would refuse it
                // anyway; refusing here keeps the reason local.
                continue;
            };
            match resolve_target(
                connection,
                &ancestry,
                &member.name,
                member.kind,
                &mut members,
                &mut produced.ancestor_resources,
            )? {
                Resolution::Found(target) => produced.evidence.push(SemanticEvidence {
                    context_key: context_key.to_owned(),
                    capability: SemanticCapability::Overrides,
                    relation_kind: Some(RelationKind::Overrides),
                    basis: basis.clone(),
                    occurrence: Some(site),
                    source: Some(GraphEndpoint::Symbol(member.id)),
                    outcome: SemanticOutcome::Resolved {
                        target: GraphEndpoint::Symbol(target),
                    },
                    // Which member a declaration overrides is written,
                    // not dispatched. What the *call* does at run time
                    // is a different question, and a resolved call site
                    // keeps its own honest dispatch.
                    dispatch: Dispatch::Static,
                    support: Support::Supported,
                }),
                Resolution::Unproven(reason) => {
                    // "Nothing declares this name" is the ordinary
                    // answer for a method that overrides nothing, so it
                    // is reported only where the source claims
                    // otherwise. The other two are real "cannot tell"
                    // states and are always reported, because silence
                    // there would read as a settled negative.
                    if reason != UnprovenReason::NoAncestorMember
                        || declared_overrides.contains(&member.id)
                    {
                        produced.unproven.push(UnprovenOverride {
                            method: member.id,
                            occurrence: site,
                            reason,
                        });
                    }
                }
            }
        }
    }
    Ok(produced)
}

enum Resolution {
    Found(SymbolId),
    Unproven(UnprovenReason),
}

/// Which member forms may override one another.
///
/// Python writes `@classmethod`, `@staticmethod` and `@property` as
/// decorators, and I3 records every one of them as
/// [`SymbolKind::Method`] -- the distinction is not in the model. So
/// this can only refuse a mismatch it *can* see (a method against a
/// field), and the `Overrides` capability is reported PARTIAL for
/// exactly that reason rather than the model being widened on a guess.
const fn overridable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Method | SymbolKind::Property | SymbolKind::Field
    )
}

fn resolve_target(
    connection: &Connection,
    ancestry: &Ancestry,
    name: &str,
    kind: SymbolKind,
    cache: &mut BTreeMap<SymbolId, Vec<Symbol>>,
    read: &mut BTreeSet<ResourceId>,
) -> Result<Resolution, GraphError> {
    // Nearest first: in a linear chain the closest declaration is the
    // one overridden, and Python's MRO agrees. A diamond whose shared
    // ancestor declares the name resolves to that one ancestor, which
    // it also agrees with.
    for depth in ancestry.by_depth.keys() {
        let mut found: BTreeSet<SymbolId> = BTreeSet::new();
        for ancestor in &ancestry.by_depth[depth] {
            for member in members_of(connection, *ancestor, cache, read)? {
                if member.name == name && member.kind == kind {
                    found.insert(member.id);
                }
            }
        }
        match found.len() {
            0 => {}
            1 => {
                return Ok(Resolution::Found(
                    found.into_iter().next().expect("exactly one"),
                ));
            }
            candidates => {
                return Ok(Resolution::Unproven(UnprovenReason::AmbiguousAncestors {
                    candidates,
                }));
            }
        }
    }
    Ok(Resolution::Unproven(if ancestry.opaque {
        UnprovenReason::OpaqueAncestor
    } else {
        UnprovenReason::NoAncestorMember
    }))
}

fn members_of<'a>(
    connection: &Connection,
    class: SymbolId,
    cache: &'a mut BTreeMap<SymbolId, Vec<Symbol>>,
    read: &mut BTreeSet<ResourceId>,
) -> Result<&'a [Symbol], GraphError> {
    if !cache.contains_key(&class) {
        let Some(resource) = resource_of(connection, class)? else {
            cache.insert(class, Vec::new());
            return Ok(cache.get(&class).expect("just inserted"));
        };
        read.insert(resource);
        let declared = list_for_resource(connection, resource)
            .map_err(sql)?
            .into_iter()
            .filter(|symbol| symbol.parent_id == Some(class))
            .collect();
        cache.insert(class, declared);
    }
    Ok(cache.get(&class).expect("present"))
}

/// Every proven ancestor of `class`, by depth.
///
/// Read straight off the canonical `EXTENDS` edges, whose source is the
/// subclass Symbol. It was not always: I3 used to publish a Python base
/// list from the Resource, because the walker collects type evidence
/// before it emits the class Symbol. That is fixed at the source, and
/// this reads one inheritance model rather than reconstructing a second
/// one from spans.
fn ancestors(
    connection: &Connection,
    class: SymbolId,
    extra: &[(SymbolId, SymbolId)],
) -> Result<Ancestry, GraphError> {
    let mut by_depth: BTreeMap<usize, Vec<SymbolId>> = BTreeMap::new();
    let mut seen: BTreeSet<SymbolId> = BTreeSet::from([class]);
    let mut opaque = false;
    let mut queue: VecDeque<(SymbolId, usize)> = VecDeque::from([(class, 0)]);

    while let Some((current, depth)) = queue.pop_front() {
        if depth >= MAX_DEPTH {
            continue;
        }
        let mut direct: Vec<SymbolId> = Vec::new();
        for relation in relations_from(
            connection,
            &GraphEndpoint::Symbol(current),
            Some(RelationKind::Extends),
        )? {
            match relation.target {
                GraphEndpoint::Symbol(base) => direct.push(base),
                // A base that is one semantic type with several
                // declarations (#19 task 12). Its members are spread
                // across those declarations, so every one of them is an
                // ancestor to read -- picking one would lose members
                // that really are inherited.
                GraphEndpoint::Logical(base) => direct.extend(
                    crate::logical_symbol::declarations(connection, base).map_err(|error| {
                        GraphError::Sqlite(rusqlite::Error::InvalidParameterName(error.to_string()))
                    })?,
                ),
                // A base outside the Workspace. Its members are not
                // indexed and must not be, so "no ancestor declares
                // this" stops being a reliable answer and the
                // derivation says so instead of pretending otherwise.
                GraphEndpoint::External(_) => opaque = true,
                GraphEndpoint::Resource(_) | GraphEndpoint::Domain(_) => {}
            }
        }
        // A base this same refresh proved and has not merged yet. Same
        // model, same subclass-Symbol source; only the storage differs.
        direct.extend(
            extra
                .iter()
                .filter(|(subclass, _)| *subclass == current)
                .map(|(_, base)| *base),
        );
        for base in direct {
            if !seen.insert(base) {
                continue;
            }
            by_depth.entry(depth + 1).or_default().push(base);
            queue.push_back((base, depth + 1));
        }
    }
    Ok(Ancestry { by_depth, opaque })
}

/// The `DEFINITION` Occurrence that declares `member`.
fn definition_site(occurrences: &[Occurrence], member: SymbolId) -> Option<OccurrenceRef> {
    occurrences
        .iter()
        .find(|occurrence| {
            occurrence.kind == OccurrenceKind::Definition
                && occurrence.containing_symbol_id == Some(member)
        })
        .map(|occurrence| OccurrenceRef {
            kind: occurrence.kind,
            start_byte: occurrence.span.start_byte,
            end_byte: occurrence.span.end_byte,
        })
}

fn resource_of(
    connection: &Connection,
    symbol: SymbolId,
) -> Result<Option<ResourceId>, GraphError> {
    let uid: Option<Vec<u8>> = connection
        .query_row(
            "SELECT resource.uid FROM symbol \
             JOIN resource ON resource.id = symbol.resource_id \
             WHERE symbol.uid = ?1",
            rusqlite::params![symbol.to_bytes().to_vec()],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(uid
        .and_then(|bytes| <[u8; 16]>::try_from(bytes.as_slice()).ok())
        .map(ResourceId::from_bytes))
}

fn sql(error: crate::symbol::SymbolError) -> GraphError {
    GraphError::Sqlite(rusqlite::Error::InvalidParameterName(error.to_string()))
}

// ---------------------------------------------------------------------
// `typing.override`
// ---------------------------------------------------------------------

/// The decorator name a Python `@override` claim would be written as.
/// TypeScript writes a keyword instead; see its adapter.
pub const OVERRIDE_DECORATOR: &str = "override";

/// The modules that declare it.
pub const OVERRIDE_MODULES: [&str; 2] = ["typing", "typing_extensions"];

/// A decorator written above a declaration, as a source span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoratorSite {
    pub member: SymbolId,
    /// The dotted name after the `@`, without any call arguments.
    pub start_byte: usize,
    pub end_byte: usize,
}

/// Every `@override`-shaped decorator written above a member of
/// `owner`.
///
/// Syntax only: this finds where a claim is written, and the caller
/// asks the backend whether the name actually resolves to
/// `typing.override`. A decorator imported under another name is
/// therefore missed, which only costs a report -- it can never create
/// an edge, because the decorator is validation evidence and the
/// ancestor member still has to be proven.
#[must_use]
pub fn override_decorator_sites(owner_text: &str, symbols: &[Symbol]) -> Vec<DecoratorSite> {
    let mut found = Vec::new();
    for member in symbols.iter().filter(|symbol| overridable(symbol.kind)) {
        let mut line_start = owner_text[..member.span.start_byte]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        while line_start > 0 {
            let previous_start = owner_text[..line_start - 1]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            let line = &owner_text[previous_start..line_start - 1];
            let trimmed = line.trim_start();
            if !trimmed.starts_with('@') {
                break;
            }
            let at = previous_start + (line.len() - trimmed.len());
            let name_start = at + 1;
            let name_end = name_start
                + owner_text[name_start..]
                    .find(|character: char| {
                        !(character.is_alphanumeric() || character == '_' || character == '.')
                    })
                    .unwrap_or(owner_text.len() - name_start);
            if owner_text[name_start..name_end]
                .rsplit('.')
                .next()
                .is_some_and(|last| last == OVERRIDE_DECORATOR)
            {
                found.push(DecoratorSite {
                    member: member.id,
                    start_byte: name_start,
                    end_byte: name_end,
                });
            }
            line_start = previous_start;
        }
    }
    found
}
