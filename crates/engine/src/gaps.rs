//! Persisted unresolved evidence and bounded candidate sets (#17 task 7).
//!
//! Tasks 4-6 produce two kinds of answer: a target they can prove, and a
//! target they cannot. Only the first becomes a Relation. This module is
//! what keeps the second from vanishing.
//!
//! ## Why the gaps are stored at all
//!
//! "No CALLS edges from this function" and "three call sites whose
//! targets need a type checker" are different facts, and an agent that
//! reads the second as the first deletes code that is called. So each
//! unresolved use site is persisted against the exact Occurrence that
//! states it, with the *specific* reason it could not be resolved --
//! never flattened into a generic unknown.
//!
//! ## Candidates are not answers
//!
//! When a resolver knows the canonical identities it is choosing
//! between -- two declarations of one name, an overload set -- those are
//! stored as `relation_candidate` rows. They are possibilities, and one
//! remaining candidate is still a possibility: nothing here promotes a
//! single candidate to a Relation.
//!
//! Candidates only ever come from a resolver that already had canonical
//! identities in hand. Nothing searches the Workspace for same-named
//! Symbols to fill the list: `obj.foo()` with an unknown receiver has no
//! candidates, because "every `foo` in the repository" is not a set of
//! possible targets, it is a text search wearing one as a costume.
//!
//! ## Bounded, deterministic, and honest about it
//!
//! A candidate list is deduplicated by canonical identity, ordered
//! deterministically, and cut at [`MAX_CANDIDATES`]. Whether it *was*
//! cut is stored, so "three candidates" and "three kept, more dropped"
//! are different rows rather than the same row read hopefully.

use std::fmt;

use brainprint_core::{ResourceId, SymbolId};

use crate::{
    calls::{CallOutcome, ResolvedCall, UnresolvedCall},
    evidence::OccurrenceRef,
    graph::{GraphEndpoint, RelationKind},
    imports::{ImportOutcome, ResolvedImport, UnresolvedImport},
    symbol::OccurrenceKind,
    types::{ResolvedTypeReference, TypeEvidence, TypeOutcome, UnresolvedType},
};

/// How many canonical candidates one unresolved reference keeps.
///
/// A default to tune, not a contract: the point is that the list is
/// finite and that being cut is recorded. Sixteen is enough for every
/// overload set worth showing a human and small enough that a pathological
/// one cannot flood the table.
pub const MAX_CANDIDATES: usize = 16;

/// `relation_candidate.evidence_kind` for a candidate the structural
/// resolver itself produced. There is no other producer at this tier,
/// and in particular no name-similarity search.
pub const STRUCTURAL_CANDIDATE: &str = "STRUCTURAL_RESOLVER";

/// What relation the evidence would have been, had the target been
/// known.
///
/// A superset of [`RelationKind`] by exactly one value: a C# base list
/// entry states inheritance, but whether it is a base class or an
/// interface follows the target's own kind (#17 task 6), so an
/// unresolved entry cannot claim either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntendedRelation {
    Known(RelationKind),
    /// A base list entry whose class/interface distinction is not
    /// structural because its target is unknown.
    Inheritance,
}

impl IntendedRelation {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Known(kind) => kind.as_str(),
            Self::Inheritance => "INHERITANCE",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, GapError> {
        if raw == "INHERITANCE" {
            return Ok(Self::Inheritance);
        }
        RelationKind::parse(raw)
            .map(Self::Known)
            .map_err(|_| GapError::UnknownIntendedRelation {
                raw: raw.to_owned(),
            })
    }
}

impl fmt::Display for IntendedRelation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why one use site has no confirmed target.
///
/// One closed vocabulary across imports, calls, references and types:
/// the reason is the thing a reader acts on, so it is preserved exactly
/// rather than collapsed. The three resolvers' own reason enums map into
/// this without losing which one it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedReason {
    /// A relative import that matches no Resource.
    MissingRelativeTarget,
    /// A Rust `crate::`/`self::`/`super::` path: the module tree is
    /// semantics, not syntax.
    ModuleTreeRequiresSemantics,
    /// A C# `using`: which assembly or file provides the namespace is
    /// not in the syntax.
    NamespaceRequiresSemantics,
    /// A specifier whose meaning depends on build configuration this
    /// tier does not read.
    ConfigDependentSpecifier,
    /// A grouped or glob import whose module set is not one specifier.
    CompoundSpecifier,
    /// Several canonical candidates, and picking one would be a guess.
    AmbiguousCandidates,
    /// The name is also a local binding, so an outer declaration is not
    /// provably meant.
    PossiblyShadowed,
    /// `obj.foo()`: the receiver's type decides, and it is not stated.
    ReceiverTypeRequired,
    /// Nothing in scope declares the name and no import binds it.
    NoStructuralBinding,
    /// An import binds the name, but its module did not resolve.
    ImportTargetUnresolved,
    /// The module resolved and does not declare that name.
    NameNotInModule,
    /// The callee is not a name at all.
    NotANameExpression,
    /// A generic, inferred, or qualified type that needs a type checker.
    TypeSemanticsRequired,
    /// An inheritance entry whose class/interface distinction depends on
    /// a target that is unknown.
    RelationKindNotStructural,
    /// An `override` marker: which base member it overrides needs the
    /// type hierarchy.
    OverrideTargetRequiresSemantics,
}

impl UnresolvedReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingRelativeTarget => "MISSING_RELATIVE_TARGET",
            Self::ModuleTreeRequiresSemantics => "MODULE_TREE_REQUIRES_SEMANTICS",
            Self::NamespaceRequiresSemantics => "NAMESPACE_REQUIRES_SEMANTICS",
            Self::ConfigDependentSpecifier => "CONFIG_DEPENDENT_SPECIFIER",
            Self::CompoundSpecifier => "COMPOUND_SPECIFIER",
            Self::AmbiguousCandidates => "AMBIGUOUS_CANDIDATES",
            Self::PossiblyShadowed => "POSSIBLY_SHADOWED",
            Self::ReceiverTypeRequired => "RECEIVER_TYPE_REQUIRED",
            Self::NoStructuralBinding => "NO_STRUCTURAL_BINDING",
            Self::ImportTargetUnresolved => "IMPORT_TARGET_UNRESOLVED",
            Self::NameNotInModule => "NAME_NOT_IN_MODULE",
            Self::NotANameExpression => "NOT_A_NAME_EXPRESSION",
            Self::TypeSemanticsRequired => "TYPE_SEMANTICS_REQUIRED",
            Self::RelationKindNotStructural => "RELATION_KIND_NOT_STRUCTURAL",
            Self::OverrideTargetRequiresSemantics => "OVERRIDE_TARGET_REQUIRES_SEMANTICS",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, GapError> {
        Ok(match raw {
            "MISSING_RELATIVE_TARGET" => Self::MissingRelativeTarget,
            "MODULE_TREE_REQUIRES_SEMANTICS" => Self::ModuleTreeRequiresSemantics,
            "NAMESPACE_REQUIRES_SEMANTICS" => Self::NamespaceRequiresSemantics,
            "CONFIG_DEPENDENT_SPECIFIER" => Self::ConfigDependentSpecifier,
            "COMPOUND_SPECIFIER" => Self::CompoundSpecifier,
            "AMBIGUOUS_CANDIDATES" => Self::AmbiguousCandidates,
            "POSSIBLY_SHADOWED" => Self::PossiblyShadowed,
            "RECEIVER_TYPE_REQUIRED" => Self::ReceiverTypeRequired,
            "NO_STRUCTURAL_BINDING" => Self::NoStructuralBinding,
            "IMPORT_TARGET_UNRESOLVED" => Self::ImportTargetUnresolved,
            "NAME_NOT_IN_MODULE" => Self::NameNotInModule,
            "NOT_A_NAME_EXPRESSION" => Self::NotANameExpression,
            "TYPE_SEMANTICS_REQUIRED" => Self::TypeSemanticsRequired,
            "RELATION_KIND_NOT_STRUCTURAL" => Self::RelationKindNotStructural,
            "OVERRIDE_TARGET_REQUIRES_SEMANTICS" => Self::OverrideTargetRequiresSemantics,
            other => {
                return Err(GapError::UnknownReason {
                    raw: other.to_owned(),
                });
            }
        })
    }

    /// Whether answering this needs a semantic backend (I4) rather than
    /// more structural work. Part of what keeps a coverage answer honest
    /// (#17 task 8/14).
    #[must_use]
    pub const fn requires_semantics(self) -> bool {
        matches!(
            self,
            Self::ModuleTreeRequiresSemantics
                | Self::NamespaceRequiresSemantics
                | Self::ReceiverTypeRequired
                | Self::TypeSemanticsRequired
                | Self::OverrideTargetRequiresSemantics
                | Self::ConfigDependentSpecifier
        )
    }

    /// Whether the construct itself is outside what this tier models --
    /// as opposed to a target it looked for and could not confirm.
    #[must_use]
    pub const fn is_unsupported_construct(self) -> bool {
        matches!(
            self,
            Self::CompoundSpecifier | Self::NotANameExpression | Self::RelationKindNotStructural
        )
    }
}

impl fmt::Display for UnresolvedReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One use site with no confirmed target, ready to be persisted against
/// its Occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedEvidence {
    /// The exact Occurrence that states it -- the same anchor a resolved
    /// relation would have bound to.
    pub occurrence: OccurrenceRef,
    pub intended: IntendedRelation,
    /// The name or specifier as written.
    pub lookup_name: String,
    /// The module the name was looked for in, when the source states
    /// one.
    pub module_hint: Option<String>,
    pub reason: UnresolvedReason,
    /// Canonical identities the resolver was choosing between. Empty
    /// unless it actually had a set in hand.
    pub candidates: Vec<GraphEndpoint>,
}

impl UnresolvedEvidence {
    /// Deduplicate, order deterministically, and cut at
    /// [`MAX_CANDIDATES`], reporting whether anything was dropped.
    #[must_use]
    pub fn bounded_candidates(&self) -> (Vec<GraphEndpoint>, bool) {
        let mut kept: Vec<GraphEndpoint> = Vec::new();
        for candidate in &self.candidates {
            // Canonical identity, not object identity: the same Symbol
            // offered twice is one candidate.
            if !kept.contains(candidate) {
                kept.push(candidate.clone());
            }
        }
        kept.sort_by_key(candidate_order);
        let truncated = kept.len() > MAX_CANDIDATES;
        kept.truncate(MAX_CANDIDATES);
        (kept, truncated)
    }
}

/// A total, stable order over canonical endpoints, so the same
/// candidate set is cut the same way on every machine and every run.
fn candidate_order(endpoint: &GraphEndpoint) -> (u8, Vec<u8>) {
    match endpoint {
        GraphEndpoint::Resource(id) => (0, id.to_bytes().to_vec()),
        GraphEndpoint::Symbol(id) => (1, id.to_bytes().to_vec()),
        GraphEndpoint::External(external) => (
            2,
            format!(
                "{}\u{1f}{}\u{1f}{}",
                external.package_identity,
                external.module_path.as_deref().unwrap_or_default(),
                external.symbol_name.as_deref().unwrap_or_default()
            )
            .into_bytes(),
        ),
        GraphEndpoint::Domain(domain) => (
            3,
            format!("{}\u{1f}{}", domain.kind, domain.normalized_identity).into_bytes(),
        ),
    }
}

/// One persisted unresolved reference, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedUnresolved {
    pub occurrence: OccurrenceRef,
    pub intended: IntendedRelation,
    pub lookup_name: String,
    pub module_hint: Option<String>,
    pub reason: UnresolvedReason,
    /// Whether the candidate list was cut. `3 candidates, complete` and
    /// `3 kept, more dropped` are different rows.
    pub candidate_truncated: bool,
    /// In the stored order.
    pub candidates: Vec<GraphEndpoint>,
    /// The resolution context the attempt depended on, if any.
    pub resolution_context_key: Option<String>,
}

/// Failure decoding a stored gap row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GapError {
    UnknownReason { raw: String },
    UnknownIntendedRelation { raw: String },
}

impl fmt::Display for GapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownReason { raw } => write!(formatter, "unknown unresolved reason {raw:?}"),
            Self::UnknownIntendedRelation { raw } => {
                write!(formatter, "unknown intended relation {raw:?}")
            }
        }
    }
}

impl std::error::Error for GapError {}

/// The gaps #17 task 4's import resolution left.
///
/// The anchor is the `IMPORT_SITE` Occurrence the specifier already
/// published, so a gap and a resolved edge are the same evidence read
/// two ways.
#[must_use]
pub fn import_gaps(resolved: &[ResolvedImport]) -> Vec<UnresolvedEvidence> {
    resolved
        .iter()
        .filter_map(|import| {
            let (reason, candidates) = match &import.outcome {
                ImportOutcome::Internal(_) | ImportOutcome::External(_) => return None,
                ImportOutcome::Ambiguous(resources) => (
                    UnresolvedReason::AmbiguousCandidates,
                    resources
                        .iter()
                        .map(|resource| GraphEndpoint::Resource(*resource))
                        .collect(),
                ),
                ImportOutcome::Unresolved(reason) => (reason_of_import(*reason), Vec::new()),
            };
            Some(UnresolvedEvidence {
                occurrence: OccurrenceRef {
                    kind: OccurrenceKind::ImportSite,
                    start_byte: import.statement.span.start_byte,
                    end_byte: import.statement.span.end_byte,
                },
                intended: IntendedRelation::Known(RelationKind::Imports),
                lookup_name: import.statement.specifier.clone(),
                module_hint: None,
                reason,
                candidates,
            })
        })
        .collect()
}

const fn reason_of_import(reason: UnresolvedImport) -> UnresolvedReason {
    match reason {
        UnresolvedImport::MissingRelativeTarget => UnresolvedReason::MissingRelativeTarget,
        UnresolvedImport::RustModuleTree => UnresolvedReason::ModuleTreeRequiresSemantics,
        UnresolvedImport::CSharpNamespace => UnresolvedReason::NamespaceRequiresSemantics,
        UnresolvedImport::ConfigDependentSpecifier => UnresolvedReason::ConfigDependentSpecifier,
        UnresolvedImport::CompoundSpecifier => UnresolvedReason::CompoundSpecifier,
    }
}

/// The gaps #17 task 5's call and reference resolution left.
#[must_use]
pub fn call_gaps(resolved: &[ResolvedCall]) -> Vec<UnresolvedEvidence> {
    resolved
        .iter()
        .filter_map(|call| {
            let (reason, candidates) = match &call.outcome {
                CallOutcome::Internal(_) | CallOutcome::External(_) => return None,
                CallOutcome::Ambiguous(symbols) => (
                    UnresolvedReason::AmbiguousCandidates,
                    symbols
                        .iter()
                        .map(|symbol| GraphEndpoint::Symbol(*symbol))
                        .collect(),
                ),
                CallOutcome::Unresolved(reason) => (reason_of_call(*reason), Vec::new()),
            };
            let (lookup_name, module_hint) = match &call.site.callee {
                crate::calls::Callee::Name(name) => (name.clone(), None),
                crate::calls::Callee::Member { receiver, member } => {
                    // The receiver is where the name was looked for, and
                    // it is a hint, never a resolved module.
                    (member.clone(), Some(receiver.clone()))
                }
                crate::calls::Callee::Other => (String::new(), None),
            };
            Some(UnresolvedEvidence {
                occurrence: OccurrenceRef {
                    kind: call.site.occurrence_kind,
                    start_byte: call.site.span.start_byte,
                    end_byte: call.site.span.end_byte,
                },
                intended: IntendedRelation::Known(call.site.relation_kind()),
                lookup_name,
                module_hint,
                reason,
                candidates,
            })
        })
        .collect()
}

const fn reason_of_call(reason: UnresolvedCall) -> UnresolvedReason {
    match reason {
        UnresolvedCall::PossiblyShadowed => UnresolvedReason::PossiblyShadowed,
        UnresolvedCall::ReceiverTypeRequired => UnresolvedReason::ReceiverTypeRequired,
        UnresolvedCall::NoStructuralBinding => UnresolvedReason::NoStructuralBinding,
        UnresolvedCall::ImportTargetUnresolved => UnresolvedReason::ImportTargetUnresolved,
        UnresolvedCall::NameNotInModule => UnresolvedReason::NameNotInModule,
        UnresolvedCall::NotANameExpression => UnresolvedReason::NotANameExpression,
    }
}

/// The gaps #17 task 6's type resolution left, including the override
/// markers it deliberately refuses to turn into edges.
#[must_use]
pub fn type_gaps(resolved: &[ResolvedTypeReference]) -> Vec<UnresolvedEvidence> {
    resolved
        .iter()
        .filter_map(|reference| {
            let resolved_target = reference.outcome.is_resolved();
            // A resolved target whose relation kind is not structural is
            // still a gap: the edge cannot be published either way.
            if resolved_target && reference.relation_kind.is_some() {
                return None;
            }
            // An override marker has no Occurrence of its own (#17 task
            // 6): there is no span the index published for it, so there
            // is nothing to anchor a row to. It is reported by the
            // resolver and stays a runtime fact.
            if reference.reference.evidence == TypeEvidence::OverrideMarker {
                return Some(UnresolvedEvidence {
                    occurrence: OccurrenceRef {
                        kind: OccurrenceKind::TypeSite,
                        start_byte: reference.reference.span.start_byte,
                        end_byte: reference.reference.span.end_byte,
                    },
                    intended: IntendedRelation::Known(RelationKind::Overrides),
                    lookup_name: reference.reference.name.clone(),
                    module_hint: None,
                    reason: UnresolvedReason::OverrideTargetRequiresSemantics,
                    candidates: Vec::new(),
                });
            }
            let (reason, candidates) = match &reference.outcome {
                TypeOutcome::Ambiguous(symbols) => (
                    UnresolvedReason::AmbiguousCandidates,
                    symbols
                        .iter()
                        .map(|symbol| GraphEndpoint::Symbol(*symbol))
                        .collect(),
                ),
                TypeOutcome::Unresolved(reason) => (reason_of_type(*reason), Vec::new()),
                // Resolved, but the kind is not structural.
                TypeOutcome::Internal(_) | TypeOutcome::External(_) => {
                    (UnresolvedReason::RelationKindNotStructural, Vec::new())
                }
            };
            Some(UnresolvedEvidence {
                occurrence: OccurrenceRef {
                    kind: OccurrenceKind::TypeSite,
                    start_byte: reference.reference.span.start_byte,
                    end_byte: reference.reference.span.end_byte,
                },
                intended: reference
                    .relation_kind
                    .map_or(IntendedRelation::Inheritance, IntendedRelation::Known),
                lookup_name: reference.reference.name.clone(),
                module_hint: None,
                reason,
                candidates,
            })
        })
        .collect()
}

const fn reason_of_type(reason: UnresolvedType) -> UnresolvedReason {
    match reason {
        UnresolvedType::NoStructuralBinding => UnresolvedReason::NoStructuralBinding,
        UnresolvedType::ImportTargetUnresolved => UnresolvedReason::ImportTargetUnresolved,
        UnresolvedType::NameNotInModule => UnresolvedReason::NameNotInModule,
        UnresolvedType::PossiblyShadowed => UnresolvedReason::PossiblyShadowed,
        UnresolvedType::RequiresTypeSemantics => UnresolvedReason::TypeSemanticsRequired,
        UnresolvedType::RelationKindNotStructural => UnresolvedReason::RelationKindNotStructural,
        UnresolvedType::OverrideTargetRequiresSemantics => {
            UnresolvedReason::OverrideTargetRequiresSemantics
        }
    }
}

/// The canonical endpoints a resolver offered, for a caller that wants
/// the candidate set without the row.
#[must_use]
pub fn candidate_symbols(evidence: &UnresolvedEvidence) -> Vec<SymbolId> {
    evidence
        .candidates
        .iter()
        .filter_map(|candidate| match candidate {
            GraphEndpoint::Symbol(symbol) => Some(*symbol),
            _ => None,
        })
        .collect()
}

/// The Resource candidates, likewise.
#[must_use]
pub fn candidate_resources(evidence: &UnresolvedEvidence) -> Vec<ResourceId> {
    evidence
        .candidates
        .iter()
        .filter_map(|candidate| match candidate {
            GraphEndpoint::Resource(resource) => Some(*resource),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use rusqlite::params;

    use super::*;
    use crate::{
        calls::{
            BindingScope, extract_call_sites, extract_import_bindings, extract_local_names,
            resolve_calls,
        },
        config::WorkspaceConfig,
        evidence::{
            EvidenceError, RelationEvidence, list_unresolved_for_resource, replace_resource_graph,
        },
        generation,
        graph::GraphStore,
        imports::{WorkspaceModules, extract_imports, resolve_imports},
        parser::{ParserRegistry, SourceBasis, dialect_for_path},
        resolution::EvidenceBasis,
        resource::{Resource, ResourceStore},
        scan::BaselineScan,
        symbol::{Symbol, SymbolStore},
        types::{extract_type_references, resolve_type_references, type_relations},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    /// A Workspace whose files state every gap shape at once.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    /// Unresolved import (a missing relative module), an unresolved call
    /// (unknown receiver), an unresolved reference (a name nothing
    /// declares), and an unresolved type (a base nothing declares).
    const APP_TS: &str = "\
import { gone } from './missing'

export class Child extends Unknown {
  go(obj: Thing): number {
    obj.foo()
    register(handler)
    return 1
  }
}
";

    const OTHER_TS: &str = "\
import { alsoGone } from './missing-too'

export function other(obj: Thing): number {
  obj.bar()
  return 2
}
";

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-gaps-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/other.ts", OTHER_TS);
            // Enough real declarations to overflow the candidate limit
            // with canonical identities rather than invented ones.
            let many: String = (0..MAX_CANDIDATES + 5)
                .map(|index| {
                    format!("export function f{index}(): number {{\n  return {index}\n}}\n")
                })
                .collect();
            fixture.write("src/many.ts", &many);
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

        fn resources(&self) -> Vec<Resource> {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .list_active()
                .expect("list")
        }

        fn resource(&self, rel: &str) -> Resource {
            self.resources()
                .into_iter()
                .find(|resource| resource.path_key == rel)
                .expect("the fixture file is a Resource")
        }

        fn symbols(&self, rel: &str) -> Vec<Symbol> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_for_resource(self.resource(rel).id)
                .expect("symbols")
        }

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.db_path()).expect("index.db")
        }

        /// Everything tasks 4-6 make of one file.
        fn analyze(&self, rel: &str) -> Analysis {
            let resource = self.resource(rel);
            let source = fs::read_to_string(self.root.join(rel)).expect("source");
            let dialect = dialect_for_path(rel).expect("dialect");
            let tree = ParserRegistry::new()
                .parse(dialect, source.as_bytes(), SourceBasis::of(&resource))
                .expect("parse");
            let resources = self.resources();
            let mut module_symbols = HashMap::new();
            for other in &resources {
                module_symbols.insert(other.id, self.symbols(&other.path_key));
            }
            let imports = resolve_imports(
                dialect,
                &resource,
                &WorkspaceModules::from_resources(&resources),
                extract_imports(&tree, source.as_bytes()),
            );
            let bindings = extract_import_bindings(&tree, source.as_bytes());
            let locals = extract_local_names(&tree, source.as_bytes());
            let own = self.symbols(rel);
            let calls = {
                let scope = BindingScope {
                    own_symbols: &own,
                    module_symbols: &module_symbols,
                    imports: &bindings,
                    resolved_imports: &imports,
                    local_names: &locals,
                };
                resolve_calls(extract_call_sites(&tree, source.as_bytes()), &scope)
            };
            let types = {
                let scope = BindingScope {
                    own_symbols: &own,
                    module_symbols: &module_symbols,
                    imports: &bindings,
                    resolved_imports: &imports,
                    local_names: &locals,
                };
                resolve_type_references(extract_type_references(&tree, source.as_bytes()), &scope)
            };
            let mut gaps = import_gaps(&imports);
            gaps.extend(call_gaps(&calls));
            gaps.extend(type_gaps(&types));
            Analysis {
                resource,
                own,
                types,
                gaps,
            }
        }

        /// Publish one file's resolved and unresolved evidence together.
        fn publish(
            &self,
            rel: &str,
            extra_gaps: &[UnresolvedEvidence],
        ) -> Result<crate::evidence::EvidenceReplacement, EvidenceError> {
            let analysis = self.analyze(rel);
            let occurrences = SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_occurrences_for_resource(analysis.resource.id)
                .expect("occurrences");
            let store = self.store();
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");

            let resolved: Vec<RelationEvidence> = type_relations(
                analysis.resource.id,
                &occurrences,
                &analysis.own,
                &analysis.types,
                building.id,
            )
            .into_iter()
            .map(|(span, relation)| {
                for endpoint in [&relation.source, &relation.target] {
                    crate::graph::ensure_entity(&transaction, endpoint).expect("ensure");
                }
                RelationEvidence {
                    occurrence: crate::evidence::OccurrenceRef {
                        kind: OccurrenceKind::TypeSite,
                        start_byte: span.start_byte,
                        end_byte: span.end_byte,
                    },
                    relation,
                }
            })
            .collect();

            let mut gaps = analysis.gaps.clone();
            gaps.extend_from_slice(extra_gaps);
            let profile_id = analysis
                .own
                .first()
                .map_or(1, |symbol| symbol.analysis_profile_id);
            let outcome = replace_resource_graph(
                &transaction,
                &grant,
                &EvidenceBasis {
                    owner_resource: analysis.resource.id,
                    owner_resource_revision: analysis.resource.resource_revision.clone(),
                    generation_id: building.id,
                    analysis_profile_id: profile_id,
                    resolution_context_key: None,
                },
                &resolved,
                &gaps,
            );
            match outcome {
                Ok(report) => {
                    generation::finish_publish_stable(&transaction, &record).expect("stable");
                    transaction.commit().expect("commit");
                    Ok(report)
                }
                Err(error) => {
                    drop(transaction);
                    generation::abort_generation(connection, building.id, "test").expect("abort");
                    Err(error)
                }
            }
        }

        fn unresolved(&self, rel: &str) -> Vec<PersistedUnresolved> {
            let store = self.store();
            list_unresolved_for_resource(store.connection(), self.resource(rel).id)
                .expect("read back")
        }

        fn count(&self, table: &str) -> i64 {
            self.store()
                .connection()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count")
        }
    }

    struct Analysis {
        resource: Resource,
        own: Vec<Symbol>,
        types: Vec<crate::types::ResolvedTypeReference>,
        gaps: Vec<UnresolvedEvidence>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn reasons(rows: &[PersistedUnresolved]) -> Vec<(OccurrenceKind, UnresolvedReason)> {
        rows.iter()
            .map(|row| (row.occurrence.kind, row.reason))
            .collect()
    }

    #[test]
    fn every_kind_of_gap_persists_against_its_exact_occurrence() {
        let fixture = Fixture::create("anchors");
        let report = fixture.publish("src/app.ts", &[]).expect("publish");
        assert!(report.unresolved_written >= 4);

        let rows = fixture.unresolved("src/app.ts");
        let source = APP_TS;
        for row in &rows {
            // Every row is anchored to a real Occurrence, and its span
            // is the text the resolver looked at.
            let text = &source[row.occurrence.start_byte..row.occurrence.end_byte];
            assert!(!text.is_empty());
            assert!(
                text.trim_matches(['\'', '"']).contains(&row.lookup_name),
                "{text:?} does not contain the name {:?} the row looked for",
                row.lookup_name
            );
        }

        let found = reasons(&rows);
        assert!(
            found.contains(&(
                OccurrenceKind::ImportSite,
                UnresolvedReason::MissingRelativeTarget
            )),
            "the unresolved import is anchored to its IMPORT_SITE: {found:?}"
        );
        assert!(
            found.contains(&(
                OccurrenceKind::CallSite,
                UnresolvedReason::ReceiverTypeRequired
            )),
            "the unknown-receiver call is anchored to its CALL_SITE: {found:?}"
        );
        assert!(
            found.contains(&(
                OccurrenceKind::ReferenceSite,
                UnresolvedReason::NoStructuralBinding
            )),
            "the passed name is anchored to its REFERENCE_SITE: {found:?}"
        );
        assert!(
            found.contains(&(
                OccurrenceKind::TypeSite,
                UnresolvedReason::NoStructuralBinding
            )),
            "the unknown base type is anchored to its TYPE_SITE: {found:?}"
        );

        // The specific reason survives: nothing is flattened into one
        // generic unknown.
        assert!(
            rows.iter()
                .map(|row| row.reason)
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| pair[0] != pair[1])
        );
        // And an intended relation kind is recorded for each.
        assert!(rows.iter().any(
            |row| row.intended == IntendedRelation::Known(crate::graph::RelationKind::Imports)
        ));
        assert!(
            rows.iter()
                .any(|row| row.intended
                    == IntendedRelation::Known(crate::graph::RelationKind::Calls))
        );
        assert!(
            rows.iter().any(|row| row.intended
                == IntendedRelation::Known(crate::graph::RelationKind::References))
        );
        assert!(rows.iter().any(
            |row| row.intended == IntendedRelation::Known(crate::graph::RelationKind::Extends)
        ));
    }

    #[test]
    fn an_unknown_receiver_produces_no_workspace_wide_candidates() {
        let fixture = Fixture::create("no-name-search");
        fixture.publish("src/app.ts", &[]).expect("publish");

        let rows = fixture.unresolved("src/app.ts");
        let receiver_gap = rows
            .iter()
            .find(|row| row.reason == UnresolvedReason::ReceiverTypeRequired)
            .expect("obj.foo() is a gap");
        assert!(
            receiver_gap.candidates.is_empty(),
            "every `foo` in the Workspace is a text search, not a candidate set"
        );
        assert!(!receiver_gap.candidate_truncated);
        assert!(receiver_gap.reason.requires_semantics());

        // Likewise a name nothing declares.
        let missing = rows
            .iter()
            .find(|row| row.reason == UnresolvedReason::NoStructuralBinding)
            .expect("an unbound name is a gap");
        assert!(missing.candidates.is_empty());
        assert!(!missing.reason.requires_semantics());
    }

    #[test]
    fn ambiguity_persists_the_canonical_candidates_without_promoting_one() {
        let fixture = Fixture::create("candidates");
        let symbols = fixture.symbols("src/app.ts");
        let first = symbols[0].id;
        let second = symbols.get(1).map_or(first, |symbol| symbol.id);
        // An ambiguous gap the resolver could have produced, with the
        // canonical identities it was choosing between.
        let ambiguous = UnresolvedEvidence {
            occurrence: crate::evidence::OccurrenceRef {
                kind: OccurrenceKind::CallSite,
                start_byte: APP_TS.find("obj.foo").expect("call site"),
                end_byte: APP_TS.find("obj.foo").expect("call site") + "obj.foo".len(),
            },
            intended: IntendedRelation::Known(crate::graph::RelationKind::Calls),
            lookup_name: "foo".to_owned(),
            module_hint: Some("obj".to_owned()),
            reason: UnresolvedReason::AmbiguousCandidates,
            // The same identity twice, plus a second one.
            candidates: vec![
                GraphEndpoint::Symbol(first),
                GraphEndpoint::Symbol(second),
                GraphEndpoint::Symbol(first),
            ],
        };

        // Publishing replaces the resolver's own gap for that span.
        let report = fixture
            .publish_only(std::slice::from_ref(&ambiguous))
            .expect("a gap-only publication");
        assert_eq!(report.unresolved_written, 1);
        assert_eq!(
            report.candidates_written,
            if first == second { 1 } else { 2 },
            "the duplicate identity is stored once"
        );

        let rows = fixture.unresolved("src/app.ts");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reason, UnresolvedReason::AmbiguousCandidates);
        assert!(!rows[0].candidate_truncated, "a complete set says so");
        assert_eq!(rows[0].module_hint.as_deref(), Some("obj"));
        // A candidate is not a Relation, however few of them there are.
        assert_eq!(fixture.count("relation"), 0);
    }

    #[test]
    fn a_single_candidate_is_still_only_a_candidate() {
        let fixture = Fixture::create("single-candidate");
        let only = fixture.symbols("src/app.ts")[0].id;
        let gap = UnresolvedEvidence {
            occurrence: crate::evidence::OccurrenceRef {
                kind: OccurrenceKind::CallSite,
                start_byte: APP_TS.find("obj.foo").expect("call site"),
                end_byte: APP_TS.find("obj.foo").expect("call site") + "obj.foo".len(),
            },
            intended: IntendedRelation::Known(crate::graph::RelationKind::Calls),
            lookup_name: "foo".to_owned(),
            module_hint: None,
            reason: UnresolvedReason::AmbiguousCandidates,
            candidates: vec![GraphEndpoint::Symbol(only)],
        };

        fixture.publish_only(&[gap]).expect("publish");

        let rows = fixture.unresolved("src/app.ts");
        assert_eq!(rows[0].candidates.len(), 1);
        assert_eq!(
            fixture.count("relation"),
            0,
            "one remaining possibility is a possibility, not an answer"
        );
    }

    #[test]
    fn a_candidate_list_is_bounded_deterministically_and_says_when_it_was_cut() {
        let fixture = Fixture::create("truncation");
        let many: Vec<GraphEndpoint> = fixture
            .symbols("src/many.ts")
            .into_iter()
            .map(|symbol| GraphEndpoint::Symbol(symbol.id))
            .collect();
        assert!(
            many.len() > MAX_CANDIDATES,
            "the fixture overflows the limit"
        );
        let gap = UnresolvedEvidence {
            occurrence: crate::evidence::OccurrenceRef {
                kind: OccurrenceKind::CallSite,
                start_byte: APP_TS.find("obj.foo").expect("call site"),
                end_byte: APP_TS.find("obj.foo").expect("call site") + "obj.foo".len(),
            },
            intended: IntendedRelation::Known(crate::graph::RelationKind::Calls),
            lookup_name: "foo".to_owned(),
            module_hint: None,
            reason: UnresolvedReason::AmbiguousCandidates,
            candidates: many.clone(),
        };

        // Bounding is deterministic before it is persisted: the same
        // input always keeps the same subset, in the same order.
        let (first, truncated) = gap.bounded_candidates();
        let (again, _) = gap.bounded_candidates();
        assert_eq!(first, again);
        assert!(truncated);
        assert_eq!(first.len(), MAX_CANDIDATES);
        let mut shuffled = gap.clone();
        shuffled.candidates.reverse();
        assert_eq!(
            shuffled.bounded_candidates().0,
            first,
            "the order the resolver happened to produce does not change the kept set"
        );

        let report = fixture.publish_only(&[gap]).expect("publish");
        assert_eq!(report.candidates_written, MAX_CANDIDATES);
        assert_eq!(report.candidate_sets_truncated, 1);

        let rows = fixture.unresolved("src/app.ts");
        assert!(
            rows[0].candidate_truncated,
            "kept-with-more-dropped is not the same row as complete"
        );
        assert_eq!(rows[0].candidates.len(), MAX_CANDIDATES);
        assert_eq!(rows[0].candidates, first, "stored in the bounded order");
    }

    #[test]
    fn coverage_states_are_distinguishable_from_one_another() {
        // The four things #17 task 8 has to tell apart.
        assert!(UnresolvedReason::ReceiverTypeRequired.requires_semantics());
        assert!(UnresolvedReason::OverrideTargetRequiresSemantics.requires_semantics());
        assert!(!UnresolvedReason::ReceiverTypeRequired.is_unsupported_construct());
        assert!(UnresolvedReason::CompoundSpecifier.is_unsupported_construct());
        assert!(!UnresolvedReason::CompoundSpecifier.requires_semantics());
        assert!(!UnresolvedReason::NoStructuralBinding.requires_semantics());
        assert!(!UnresolvedReason::NoStructuralBinding.is_unsupported_construct());
        assert_eq!(
            UnresolvedReason::AmbiguousCandidates,
            UnresolvedReason::parse("AMBIGUOUS_CANDIDATES").expect("round trip")
        );
        assert!(UnresolvedReason::parse("SOMETHING_ELSE").is_err());
        assert_eq!(
            IntendedRelation::parse("INHERITANCE").expect("round trip"),
            IntendedRelation::Inheritance
        );
        assert_eq!(
            IntendedRelation::parse("CALLS").expect("round trip"),
            IntendedRelation::Known(crate::graph::RelationKind::Calls)
        );
        assert!(IntendedRelation::parse("CALLED_BY").is_err());
    }

    #[test]
    fn an_override_marker_is_a_gap_and_never_an_edge() {
        let fixture = Fixture::create("override");
        fixture.write(
            "src/app.cs",
            "public class Base\n{\n    public virtual int Go() => 0;\n}\n\n\
             public class Child : Base\n{\n    public override int Go() => 1;\n}\n",
        );
        // The marker has no Occurrence of its own, so it is filtered out
        // before persistence -- and it is still not an edge.
        let analysis = fixture.analyze("src/app.ts");
        assert!(!analysis.gaps.iter().any(|gap| gap.intended
            == IntendedRelation::Known(crate::graph::RelationKind::Overrides)
            && gap.reason != UnresolvedReason::OverrideTargetRequiresSemantics));
        fixture.publish("src/app.ts", &[]).expect("publish");
        let overrides: i64 = fixture
            .store()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM relation WHERE kind = 'OVERRIDES'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(overrides, 0);
    }

    #[test]
    fn a_republish_replaces_only_its_owners_gaps() {
        let fixture = Fixture::create("ownership");
        fixture.publish("src/app.ts", &[]).expect("publish");
        fixture.publish("src/other.ts", &[]).expect("publish");
        let app_before = fixture.unresolved("src/app.ts").len();
        let other_before = fixture.unresolved("src/other.ts").len();
        assert!(app_before > 0 && other_before > 0);

        // app.ts loses every gap it had.
        fixture.write("src/app.ts", "export const nothing = 1\n");
        fixture.publish("src/app.ts", &[]).expect("publish");

        assert!(
            fixture.unresolved("src/app.ts").is_empty(),
            "stale gaps and their candidates go with the re-analysis"
        );
        assert_eq!(
            fixture.unresolved("src/other.ts").len(),
            other_before,
            "another Resource's gaps are not this replacement's to clear"
        );
    }

    #[test]
    fn one_occurrence_is_never_both_resolved_and_unresolved() {
        let fixture = Fixture::create("exclusive");
        let analysis = fixture.analyze("src/app.ts");
        // Claim the base-type occurrence as a gap while the publication
        // also resolves it.
        let resolved_span = analysis
            .types
            .iter()
            .find(|reference| reference.relation_kind.is_some())
            .expect("a type reference")
            .reference
            .span;
        let conflicting = UnresolvedEvidence {
            occurrence: crate::evidence::OccurrenceRef {
                kind: OccurrenceKind::TypeSite,
                start_byte: resolved_span.start_byte,
                end_byte: resolved_span.end_byte,
            },
            intended: IntendedRelation::Known(crate::graph::RelationKind::Extends),
            lookup_name: "Unknown".to_owned(),
            module_hint: None,
            reason: UnresolvedReason::NoStructuralBinding,
            candidates: Vec::new(),
        };

        // With nothing resolved at that span it is simply a gap...
        fixture
            .publish_only(std::slice::from_ref(&conflicting))
            .expect("a gap alone is fine");
        // ...but it cannot be both at once.
        let store = fixture.store();
        let connection = store.connection();
        let revision = generation::current_workspace_revision(connection)
            .expect("clock")
            .expect("bootstrapped");
        let building = generation::begin_generation(connection, &revision).expect("begin");
        let transaction = connection.unchecked_transaction().expect("transaction");
        let (_record, grant) =
            generation::grant_publication(&transaction, building.id).expect("grant");
        let resource = fixture.resource("src/app.ts");
        let endpoint = GraphEndpoint::Resource(resource.id);
        crate::graph::ensure_entity(&transaction, &endpoint).expect("ensure");
        let failure = replace_resource_graph(
            &transaction,
            &grant,
            &EvidenceBasis {
                owner_resource: resource.id,
                owner_resource_revision: resource.resource_revision.clone(),
                generation_id: building.id,
                analysis_profile_id: fixture.symbols("src/app.ts")[0].analysis_profile_id,
                resolution_context_key: None,
            },
            &[RelationEvidence {
                occurrence: conflicting.occurrence,
                relation: crate::graph::Relation {
                    kind: crate::graph::RelationKind::Extends,
                    source: endpoint.clone(),
                    target: endpoint,
                    dispatch: crate::resolution::Dispatch::Static,
                    created_generation: building.id,
                },
            }],
            &[conflicting],
        )
        .expect_err("one occurrence, one answer");
        drop(transaction);
        assert!(matches!(
            failure,
            EvidenceError::OccurrenceAlreadyResolved { .. }
        ));
    }

    #[test]
    fn a_failed_publication_keeps_the_previous_gaps_and_relations() {
        let fixture = Fixture::create("rollback");
        fixture.publish("src/app.ts", &[]).expect("publish");
        let before = fixture.unresolved("src/app.ts");
        let relations_before = fixture.count("relation");
        assert!(!before.is_empty());

        // A gap naming an Occurrence the owner does not have.
        let bogus = UnresolvedEvidence {
            occurrence: crate::evidence::OccurrenceRef {
                kind: OccurrenceKind::CallSite,
                start_byte: 90_000,
                end_byte: 90_010,
            },
            intended: IntendedRelation::Known(crate::graph::RelationKind::Calls),
            lookup_name: "nowhere".to_owned(),
            module_hint: None,
            reason: UnresolvedReason::NoStructuralBinding,
            candidates: Vec::new(),
        };
        let failure = fixture
            .publish("src/app.ts", &[bogus])
            .expect_err("the publication is rejected");
        assert!(matches!(failure, EvidenceError::UnknownOccurrence { .. }));

        assert_eq!(
            fixture.unresolved("src/app.ts"),
            before,
            "the previous gaps are exactly as they were"
        );
        assert_eq!(fixture.count("relation"), relations_before);
        assert_eq!(
            fixture.count("relation_candidate"),
            0,
            "and no candidate row survived the rollback"
        );
    }

    #[test]
    fn a_stale_basis_is_refused_before_any_gap_is_written() {
        let fixture = Fixture::create("basis");
        let resource = fixture.resource("src/app.ts");
        let store = fixture.store();
        let connection = store.connection();
        let revision = generation::current_workspace_revision(connection)
            .expect("clock")
            .expect("bootstrapped");
        let building = generation::begin_generation(connection, &revision).expect("begin");
        let transaction = connection.unchecked_transaction().expect("transaction");
        let (_record, grant) =
            generation::grant_publication(&transaction, building.id).expect("grant");

        let failure = replace_resource_graph(
            &transaction,
            &grant,
            &EvidenceBasis {
                owner_resource: resource.id,
                owner_resource_revision: "999".to_owned(),
                generation_id: building.id,
                analysis_profile_id: 1,
                resolution_context_key: None,
            },
            &[],
            &fixture.analyze("src/app.ts").gaps,
        )
        .expect_err("stale evidence is not written");
        drop(transaction);

        assert!(matches!(failure, EvidenceError::RevisionMismatch { .. }));
        assert_eq!(fixture.count("unresolved_reference"), 0);
    }

    #[test]
    fn gaps_store_no_source_text() {
        let fixture = Fixture::create("no-source");
        fixture.publish("src/app.ts", &[]).expect("publish");

        let database = fs::read(fixture.db_path()).expect("index.db bytes");
        for body in ["register(handler)", "return 1", "obj.foo()"] {
            assert!(
                !database
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db must not mirror source text ({body:?})"
            );
        }
    }

    impl Fixture {
        /// Publish only the given gaps for `src/app.ts`, with no
        /// resolved evidence -- for the candidate-shape tests.
        fn publish_only(
            &self,
            gaps: &[UnresolvedEvidence],
        ) -> Result<crate::evidence::EvidenceReplacement, EvidenceError> {
            let resource = self.resource("src/app.ts");
            let store = self.store();
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");
            let profile_id = self.symbols("src/app.ts")[0].analysis_profile_id;
            let outcome = replace_resource_graph(
                &transaction,
                &grant,
                &EvidenceBasis {
                    owner_resource: resource.id,
                    owner_resource_revision: resource.resource_revision.clone(),
                    generation_id: building.id,
                    analysis_profile_id: profile_id,
                    resolution_context_key: None,
                },
                &[],
                gaps,
            );
            match outcome {
                Ok(report) => {
                    generation::finish_publish_stable(&transaction, &record).expect("stable");
                    transaction.commit().expect("commit");
                    Ok(report)
                }
                Err(error) => {
                    drop(transaction);
                    generation::abort_generation(connection, building.id, "test").expect("abort");
                    Err(error)
                }
            }
        }
    }

    #[test]
    fn stored_rows_carry_their_provenance_context_when_there_is_one() {
        let fixture = Fixture::create("provenance");
        let store = fixture.store();
        let stable = generation::current_stable(store.connection())
            .expect("stable")
            .expect("published")
            .id;
        let context = crate::resolution::ResolutionContext {
            language: "TYPESCRIPT".to_owned(),
            scope_key: "tsconfig.json".to_owned(),
            config_fingerprint: "sha256:config".to_owned(),
            dependency_fingerprint: "sha256:deps".to_owned(),
            environment_fingerprint: "sha256:env".to_owned(),
            module_resolution_fingerprint: "sha256:module".to_owned(),
            backend_snapshot_token: None,
        };
        let key = store
            .ensure_resolution_context(&context, stable)
            .expect("ensure");
        drop(store);

        let resource = fixture.resource("src/app.ts");
        let gaps = fixture.analyze("src/app.ts").gaps;
        let store = fixture.store();
        let connection = store.connection();
        let revision = generation::current_workspace_revision(connection)
            .expect("clock")
            .expect("bootstrapped");
        let building = generation::begin_generation(connection, &revision).expect("begin");
        let transaction = connection.unchecked_transaction().expect("transaction");
        let (record, grant) =
            generation::grant_publication(&transaction, building.id).expect("grant");
        replace_resource_graph(
            &transaction,
            &grant,
            &EvidenceBasis {
                owner_resource: resource.id,
                owner_resource_revision: resource.resource_revision.clone(),
                generation_id: building.id,
                analysis_profile_id: fixture.symbols("src/app.ts")[0].analysis_profile_id,
                resolution_context_key: Some(key.clone()),
            },
            &[],
            &gaps,
        )
        .expect("publish");
        generation::finish_publish_stable(&transaction, &record).expect("stable");
        transaction.commit().expect("commit");
        drop(store);

        let rows = fixture.unresolved("src/app.ts");
        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|row| row.resolution_context_key.as_deref() == Some(key.as_str())),
            "the context the attempt depended on is on the row"
        );
        // And the occurrence rows carry it too (#17 task 3).
        let bound: i64 = fixture
            .store()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM occurrence WHERE resolution_context_id IS NOT NULL",
                params![],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(bound, 0, "no relation was bound, so none carries a context");
    }
}
