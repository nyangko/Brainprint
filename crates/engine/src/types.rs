//! Structural EXTENDS/IMPLEMENTS/USES_TYPE evidence, and the refusal to
//! invent OVERRIDES (#17 task 6).
//!
//! Inheritance and type syntax states relationships outright: `class
//! Child extends Base` is not an inference. This module reads exactly
//! those statements and, using the same binding rules as #17 task 5,
//! confirms a target only when the source settles which type is meant.
//!
//! ## Which relation a position implies
//!
//! The position decides the kind, not the name: a base clause is
//! EXTENDS, an implements/trait clause is IMPLEMENTS, and an explicit
//! parameter, return, or field type is USES_TYPE. C# writes classes and
//! interfaces in one list, so there the kind follows the *resolved
//! target's* own [`SymbolKind`] -- an `I`-prefixed name is a convention,
//! not evidence, and nothing here reads names that way.
//!
//! ## One span, one relation
//!
//! `class Child extends Base` produces EXTENDS(Base) and no USES_TYPE
//! for the same token: a more specific relation is not also a generic
//! one. Each recognized type position yields exactly one `TYPE_SITE`
//! Occurrence, so two relations can never claim the same evidence.
//!
//! ## OVERRIDES is not written here
//!
//! An `override` marker proves that something is overridden. Which base
//! member it overrides needs the type hierarchy -- the very thing a
//! structural parse does not have. Matching by name would produce an
//! edge that looks confirmed and points at whichever same-named member
//! happened to be nearby. So the marker is kept as an unresolved,
//! semantic-needed result for #17 task 7, and no OVERRIDES edge exists
//! at this tier.
//!
//! ## What is refused
//!
//! Generic arguments that need substitution, qualified type paths,
//! inferred types, re-export chains, C# assembly binding, Rust trait
//! resolution, Python MRO/metaclass, and any type whose name merely
//! matches something elsewhere in the Workspace.

use brainprint_core::{ResourceId, SymbolId};
use tree_sitter::Node;

use crate::{
    calls::{BindingScope, CallOutcome, UnresolvedCall, resolve_member, resolve_name},
    extract::span_of,
    graph::{ExternalEntity, GraphEndpoint, Relation, RelationKind},
    parser::{ParseTree, ParserDialect, SourceSpan},
    resolution::Dispatch,
    symbol::{Occurrence, OccurrenceKind, Symbol, SymbolKind},
};

/// What a type position states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeEvidence {
    /// A base class.
    Extends,
    /// An implemented interface or trait.
    Implements,
    /// An explicit parameter, return, field, or annotation type.
    UsesType,
    /// A base list whose entries may be classes or interfaces, so the
    /// relation follows the resolved target's own kind (C#).
    BaseListEntry,
    /// An `override` marker. Evidence that something is overridden, and
    /// never on its own evidence of *what*.
    OverrideMarker,
}

/// One type reference, with the exact token that states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeReference {
    pub evidence: TypeEvidence,
    /// The type as written at this position.
    pub name: String,
    /// For a Rust `impl Trait for Type`, the implementing type: the
    /// relation's source is that type, not the enclosing block (which is
    /// not a Symbol).
    pub implementor: Option<String>,
    pub span: SourceSpan,
}

/// What resolution could establish about one type reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeOutcome {
    Internal(SymbolId),
    External(ExternalEntity),
    Ambiguous(Vec<SymbolId>),
    Unresolved(UnresolvedType),
}

impl TypeOutcome {
    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        matches!(self, Self::Internal(_) | Self::External(_))
    }
}

/// Why a type reference has no confirmed target. Runtime only:
/// persisting these is #17 task 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedType {
    /// Nothing in this file declares it and no import binds it.
    NoStructuralBinding,
    /// An import binds the name, but its module did not resolve.
    ImportTargetUnresolved,
    /// The module resolved and does not declare that name.
    NameNotInModule,
    /// The name is also a local binding, so an outer type of the same
    /// name is not provably meant.
    PossiblyShadowed,
    /// A qualified or generic type that needs more than a name lookup.
    RequiresTypeSemantics,
    /// A base list entry whose target is unknown, so whether it is a
    /// base class or an interface is unknown too.
    RelationKindNotStructural,
    /// An `override` marker: which base member is overridden needs the
    /// type hierarchy (I4).
    OverrideTargetRequiresSemantics,
}

/// One type reference and what resolution made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTypeReference {
    pub reference: TypeReference,
    pub outcome: TypeOutcome,
    /// The relation this would be published as, once the target is
    /// known. `None` for an override marker, and for a base list entry
    /// whose target never resolved.
    pub relation_kind: Option<RelationKind>,
}

/// Every type reference one parse tree states, in source order.
///
/// The spans are exactly the ones #16 task 9's walker publishes as
/// `TYPE_SITE` Occurrences -- both use [`type_references_at`], so there
/// is one definition of what counts and where it sits.
#[must_use]
pub fn extract_type_references(tree: &ParseTree, source: &[u8]) -> Vec<TypeReference> {
    if !tree.descriptor().capability.covers_whole_file() {
        return Vec::new();
    }
    let dialect = tree.descriptor().dialect;
    let mut references = Vec::new();
    walk(tree.syntax_tree().root_node(), &mut |node| {
        references.extend(type_references_at(node, dialect, source));
    });
    references.sort_by_key(|reference| reference.span.start_byte);
    references
}

/// The type references one node states, if any. Shared with the
/// structural extractor so evidence and relations agree on every span.
pub(crate) fn type_references_at(
    node: Node<'_>,
    dialect: ParserDialect,
    source: &[u8],
) -> Vec<TypeReference> {
    let mut found = Vec::new();
    match (dialect, node.kind()) {
        // --- Python: bases, parameter types, return types.
        (ParserDialect::Python, "class_definition") => {
            if let Some(bases) = node.child_by_field_name("superclasses") {
                let mut cursor = bases.walk();
                for base in bases.named_children(&mut cursor) {
                    push_plain(&mut found, TypeEvidence::Extends, base, source);
                }
            }
        }
        (ParserDialect::Python, "typed_parameter") => {
            if let Some(annotation) = node.child_by_field_name("type") {
                push_annotation(&mut found, annotation, source);
            }
        }
        (ParserDialect::Python, "function_definition") => {
            if let Some(annotation) = node.child_by_field_name("return_type") {
                push_annotation(&mut found, annotation, source);
            }
        }
        // --- JS/TS: heritage clauses and explicit annotations.
        (
            ParserDialect::JavaScript
            | ParserDialect::Jsx
            | ParserDialect::TypeScript
            | ParserDialect::Tsx,
            "extends_clause",
        ) => {
            if let Some(value) = node.child_by_field_name("value") {
                push_plain(&mut found, TypeEvidence::Extends, value, source);
            }
        }
        (ParserDialect::TypeScript | ParserDialect::Tsx, "implements_clause") => {
            let mut cursor = node.walk();
            for interface in node.named_children(&mut cursor) {
                push_plain(&mut found, TypeEvidence::Implements, interface, source);
            }
        }
        (ParserDialect::TypeScript | ParserDialect::Tsx, "type_annotation") => {
            let mut cursor = node.walk();
            if let Some(annotated) = node.named_children(&mut cursor).next() {
                push_plain(&mut found, TypeEvidence::UsesType, annotated, source);
            }
        }
        // --- C#: one base list for classes and interfaces alike.
        (ParserDialect::CSharp, "base_list") => {
            let mut cursor = node.walk();
            for entry in node.named_children(&mut cursor) {
                push_plain(&mut found, TypeEvidence::BaseListEntry, entry, source);
            }
        }
        (
            ParserDialect::CSharp,
            "parameter" | "property_declaration" | "variable_declaration" | "method_declaration",
        ) => {
            if let Some(annotated) = node.child_by_field_name("type") {
                push_plain(&mut found, TypeEvidence::UsesType, annotated, source);
            }
            if node.kind() == "method_declaration" {
                push_override_marker(&mut found, node, source);
            }
        }
        // --- Rust: `impl Trait for Type`, fields, parameters, returns.
        (ParserDialect::Rust, "impl_item") => {
            if let Some(contract) = node.child_by_field_name("trait") {
                let implementor = node
                    .child_by_field_name("type")
                    .filter(|node| node.kind() == "type_identifier")
                    .map(|node| text_of(node, source));
                if let Some(mut reference) =
                    plain_reference(TypeEvidence::Implements, contract, source)
                {
                    reference.implementor = implementor;
                    found.push(reference);
                }
            }
        }
        (ParserDialect::Rust, "field_declaration" | "parameter") => {
            if let Some(annotated) = node.child_by_field_name("type") {
                push_plain(&mut found, TypeEvidence::UsesType, annotated, source);
            }
        }
        (ParserDialect::Rust, "function_item") => {
            if let Some(annotated) = node.child_by_field_name("return_type") {
                push_plain(&mut found, TypeEvidence::UsesType, annotated, source);
            }
        }
        _ => {}
    }
    found
}

/// A Python `type` wrapper holds the actual type node; anything more
/// complex than a plain name inside it needs type semantics.
fn push_annotation(found: &mut Vec<TypeReference>, annotation: Node<'_>, source: &[u8]) {
    let mut cursor = annotation.walk();
    let inner = annotation.named_children(&mut cursor).next();
    push_plain(
        found,
        TypeEvidence::UsesType,
        inner.unwrap_or(annotation),
        source,
    );
}

fn push_plain(
    found: &mut Vec<TypeReference>,
    evidence: TypeEvidence,
    node: Node<'_>,
    source: &[u8],
) {
    if let Some(reference) = plain_reference(evidence, node, source) {
        found.push(reference);
    }
}

/// Only a plain type name is evidence this tier can act on. A generic
/// application, a qualified path, a union, a tuple, a built-in keyword
/// -- each of those needs something the syntax does not carry, so none
/// of them produces evidence at all rather than evidence with a guess
/// attached.
fn plain_reference(evidence: TypeEvidence, node: Node<'_>, source: &[u8]) -> Option<TypeReference> {
    if !matches!(node.kind(), "identifier" | "type_identifier") {
        return None;
    }
    Some(TypeReference {
        evidence,
        name: text_of(node, source),
        implementor: None,
        span: span_of(node),
    })
}

/// A C# `override` modifier, recorded at the declaration's own name.
fn push_override_marker(found: &mut Vec<TypeReference>, node: Node<'_>, source: &[u8]) {
    let mut cursor = node.walk();
    let overrides = node
        .children(&mut cursor)
        .any(|child| child.kind() == "modifier" && text_of(child, source) == "override");
    if !overrides {
        return;
    }
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    found.push(TypeReference {
        evidence: TypeEvidence::OverrideMarker,
        name: text_of(name, source),
        implementor: None,
        span: span_of(name),
    });
}

/// Resolve every type reference against the same binding scope calls and
/// references use (#17 task 5).
#[must_use]
pub fn resolve_type_references(
    references: Vec<TypeReference>,
    scope: &BindingScope<'_>,
) -> Vec<ResolvedTypeReference> {
    references
        .into_iter()
        .map(|reference| {
            let outcome = resolve_one(&reference, scope);
            ResolvedTypeReference {
                relation_kind: relation_kind_of(&reference, &outcome, scope),
                reference,
                outcome,
            }
        })
        .collect()
}

fn resolve_one(reference: &TypeReference, scope: &BindingScope<'_>) -> TypeOutcome {
    if reference.evidence == TypeEvidence::OverrideMarker {
        // The marker is real; the target is not structural.
        return TypeOutcome::Unresolved(UnresolvedType::OverrideTargetRequiresSemantics);
    }
    // A qualified name is `receiver.member` at the binding level; the
    // shared rules already refuse anything they cannot prove.
    let outcome = match reference.name.split_once('.') {
        Some((receiver, member)) => resolve_member(receiver, member, scope),
        None => resolve_name(&reference.name, scope),
    };
    outcome.into()
}

impl From<CallOutcome> for TypeOutcome {
    fn from(outcome: CallOutcome) -> Self {
        match outcome {
            CallOutcome::Internal(symbol) => Self::Internal(symbol),
            CallOutcome::External(external) => Self::External(external),
            CallOutcome::Ambiguous(candidates) => Self::Ambiguous(candidates),
            CallOutcome::Unresolved(reason) => Self::Unresolved(match reason {
                UnresolvedCall::PossiblyShadowed => UnresolvedType::PossiblyShadowed,
                UnresolvedCall::ImportTargetUnresolved => UnresolvedType::ImportTargetUnresolved,
                UnresolvedCall::NameNotInModule => UnresolvedType::NameNotInModule,
                UnresolvedCall::NoStructuralBinding => UnresolvedType::NoStructuralBinding,
                UnresolvedCall::ReceiverTypeRequired | UnresolvedCall::NotANameExpression => {
                    UnresolvedType::RequiresTypeSemantics
                }
            }),
        }
    }
}

/// Which relation a reference would be published as.
///
/// For a C# base list entry the answer is the resolved target's own
/// kind: an interface or trait is implemented, a class, struct, or
/// record is extended. When the target is unknown, so is the kind -- and
/// an unknown target produces no edge anyway.
fn relation_kind_of(
    reference: &TypeReference,
    outcome: &TypeOutcome,
    scope: &BindingScope<'_>,
) -> Option<RelationKind> {
    match reference.evidence {
        TypeEvidence::Extends => Some(RelationKind::Extends),
        TypeEvidence::Implements => Some(RelationKind::Implements),
        TypeEvidence::UsesType => Some(RelationKind::UsesType),
        TypeEvidence::OverrideMarker => None,
        TypeEvidence::BaseListEntry => match outcome {
            TypeOutcome::Internal(symbol) => scope
                .own_symbols
                .iter()
                .chain(scope.module_symbols.values().flatten())
                .find(|candidate| candidate.id == *symbol)
                .map(|candidate| match candidate.kind {
                    SymbolKind::Interface | SymbolKind::Trait => RelationKind::Implements,
                    _ => RelationKind::Extends,
                }),
            // An external base is named but not described: whether the
            // package's entry is a class or an interface is not in this
            // source.
            _ => None,
        },
    }
}

/// Turn resolved type references into canonical edges, paired with the
/// span that proves each one.
///
/// The source is the Symbol the evidence sits inside, the implementing
/// type for a Rust `impl ... for`, or the Resource when the position is
/// not inside a declaration. Unresolved, ambiguous, and
/// unknown-kind outcomes produce no edge and are kept in the
/// [`ResolvedTypeReference`] list for #17 task 7.
#[must_use]
pub fn type_relations(
    owner: ResourceId,
    occurrences: &[Occurrence],
    own_symbols: &[Symbol],
    resolved: &[ResolvedTypeReference],
    created_generation: i64,
) -> Vec<(SourceSpan, Relation)> {
    resolved
        .iter()
        .filter_map(|reference| {
            let kind = reference.relation_kind?;
            let target = match &reference.outcome {
                TypeOutcome::Internal(symbol) => GraphEndpoint::Symbol(*symbol),
                TypeOutcome::External(external) => GraphEndpoint::External(external.clone()),
                TypeOutcome::Ambiguous(_) | TypeOutcome::Unresolved(_) => return None,
            };
            let implementor = reference
                .reference
                .implementor
                .as_deref()
                .and_then(|name| unique_top_level(own_symbols, name));
            let containing = occurrences
                .iter()
                .find(|occurrence| {
                    occurrence.kind == OccurrenceKind::TypeSite
                        && occurrence.span.start_byte == reference.reference.span.start_byte
                        && occurrence.span.end_byte == reference.reference.span.end_byte
                })
                .and_then(|occurrence| occurrence.containing_symbol_id);
            let source = implementor
                .or(containing)
                .map_or(GraphEndpoint::Resource(owner), GraphEndpoint::Symbol);
            Some((
                reference.reference.span,
                Relation {
                    kind,
                    source,
                    target,
                    // Inheritance and type use are written, not
                    // dispatched.
                    dispatch: Dispatch::Static,
                    created_generation,
                },
            ))
        })
        .collect()
}

fn unique_top_level(symbols: &[Symbol], name: &str) -> Option<SymbolId> {
    let mut matches = symbols
        .iter()
        .filter(|symbol| symbol.parent_id.is_none() && symbol.name == name);
    let first = matches.next()?;
    matches.next().is_none().then_some(first.id)
}

fn walk(node: Node<'_>, visit: &mut impl FnMut(Node<'_>)) {
    visit(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, visit);
    }
}

fn text_of(node: Node<'_>, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]).into_owned()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{HashMap, HashSet},
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        calls::{extract_import_bindings, extract_local_names},
        config::WorkspaceConfig,
        evidence::{OccurrenceRef, RelationEvidence, replace_resource_evidence},
        extract::{assign_ids, extract},
        generation,
        graph::GraphStore,
        imports::{WorkspaceModules, extract_imports, resolve_imports},
        parser::{ParserRegistry, SourceBasis, dialect_for_path},
        resolution::EvidenceBasis,
        resource::{Resource, ResourceKind, ResourceRole, ResourceState, ResourceStore},
        scan::BaselineScan,
        symbol::SymbolStore,
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    /// A Workspace of source files, resolved in memory.
    struct Scenario {
        files: Vec<(String, String)>,
        resources: Vec<Resource>,
        symbols: HashMap<ResourceId, Vec<Symbol>>,
    }

    impl Scenario {
        fn new(files: &[(&str, &str)]) -> Self {
            let resources: Vec<Resource> = files
                .iter()
                .map(|(path, _)| Resource {
                    id: ResourceId::generate(),
                    path_rel: (*path).to_owned(),
                    path_key: (*path).to_owned(),
                    kind: ResourceKind::File,
                    role: ResourceRole::Source,
                    language: None,
                    size_bytes: 0,
                    mtime_ns: 0,
                    fingerprint: "fp".to_owned(),
                    content_hash: Some("sha256:test".to_owned()),
                    state: ResourceState::Active,
                    resource_revision: "1".to_owned(),
                    generated_kind: None,
                    container_resource_id: None,
                })
                .collect();
            let mut symbols = HashMap::new();
            for (resource, (path, source)) in resources.iter().zip(files) {
                let Ok(dialect) = dialect_for_path(path) else {
                    symbols.insert(resource.id, Vec::new());
                    continue;
                };
                let tree = ParserRegistry::new()
                    .parse(dialect, source.as_bytes(), SourceBasis::of(resource))
                    .expect("parse");
                let extraction = extract(&tree, source.as_bytes());
                symbols.insert(resource.id, assign_ids(&[], &extraction, resource, 1));
            }
            Self {
                files: files
                    .iter()
                    .map(|(path, source)| ((*path).to_owned(), (*source).to_owned()))
                    .collect(),
                resources,
                symbols,
            }
        }

        fn resource(&self, path: &str) -> &Resource {
            self.resources
                .iter()
                .find(|resource| resource.path_key == path)
                .expect("in the scenario")
        }

        fn symbol(&self, path: &str, qualified_name: &str) -> SymbolId {
            self.symbols[&self.resource(path).id]
                .iter()
                .find(|symbol| symbol.qualified_name == qualified_name)
                .unwrap_or_else(|| panic!("{path} declares no {qualified_name}"))
                .id
        }

        fn source(&self, path: &str) -> &str {
            &self
                .files
                .iter()
                .find(|(candidate, _)| candidate == path)
                .expect("in the scenario")
                .1
        }

        fn resolve(&self, path: &str) -> Vec<ResolvedTypeReference> {
            let resource = self.resource(path);
            let source = self.source(path);
            let dialect = dialect_for_path(path).expect("a supported dialect");
            let tree = ParserRegistry::new()
                .parse(dialect, source.as_bytes(), SourceBasis::of(resource))
                .expect("parse");
            let imports = resolve_imports(
                dialect,
                resource,
                &WorkspaceModules::from_resources(&self.resources),
                extract_imports(&tree, source.as_bytes()),
            );
            let bindings = extract_import_bindings(&tree, source.as_bytes());
            let locals = extract_local_names(&tree, source.as_bytes());
            let own = &self.symbols[&resource.id];
            let scope = BindingScope {
                own_symbols: own,
                module_symbols: &self.symbols,
                imports: &bindings,
                resolved_imports: &imports,
                local_names: &locals,
            };
            resolve_type_references(extract_type_references(&tree, source.as_bytes()), &scope)
        }

        /// `(written token, relation kind, outcome)` per reference.
        fn outcomes(&self, path: &str) -> Vec<(String, Option<RelationKind>, TypeOutcome)> {
            let source = self.source(path);
            self.resolve(path)
                .into_iter()
                .map(|reference| {
                    (
                        source[reference.reference.span.start_byte
                            ..reference.reference.span.end_byte]
                            .to_owned(),
                        reference.relation_kind,
                        reference.outcome,
                    )
                })
                .collect()
        }
    }

    #[test]
    fn python_bases_and_annotations_are_extends_and_uses_type() {
        let scenario = Scenario::new(&[(
            "app.py",
            "class Base:\n    pass\n\n\nclass Thing:\n    pass\n\n\n\
             class Child(Base):\n    def go(self, x: Thing) -> Base:\n        return x\n",
        )]);
        let base = scenario.symbol("app.py", "Base");
        let thing = scenario.symbol("app.py", "Thing");

        assert_eq!(
            scenario.outcomes("app.py"),
            vec![
                (
                    "Base".to_owned(),
                    Some(RelationKind::Extends),
                    TypeOutcome::Internal(base)
                ),
                (
                    "Thing".to_owned(),
                    Some(RelationKind::UsesType),
                    TypeOutcome::Internal(thing)
                ),
                (
                    "Base".to_owned(),
                    Some(RelationKind::UsesType),
                    TypeOutcome::Internal(base)
                ),
            ],
            "a base clause is EXTENDS; a parameter and a return type are USES_TYPE"
        );
    }

    #[test]
    fn typescript_states_extends_implements_and_explicit_annotations() {
        let scenario = Scenario::new(&[(
            "src/app.ts",
            "export class Base {}\nexport interface Shape {}\nexport class Thing {}\n\n\
             export class Child extends Base implements Shape {\n  value: Thing\n  \
             go(x: Thing): Base { return new Base() }\n}\n",
        )]);
        let base = scenario.symbol("src/app.ts", "Base");
        let shape = scenario.symbol("src/app.ts", "Shape");
        let thing = scenario.symbol("src/app.ts", "Thing");

        assert_eq!(
            scenario.outcomes("src/app.ts"),
            vec![
                (
                    "Base".to_owned(),
                    Some(RelationKind::Extends),
                    TypeOutcome::Internal(base)
                ),
                (
                    "Shape".to_owned(),
                    Some(RelationKind::Implements),
                    TypeOutcome::Internal(shape)
                ),
                (
                    "Thing".to_owned(),
                    Some(RelationKind::UsesType),
                    TypeOutcome::Internal(thing)
                ),
                (
                    "Thing".to_owned(),
                    Some(RelationKind::UsesType),
                    TypeOutcome::Internal(thing)
                ),
                (
                    "Base".to_owned(),
                    Some(RelationKind::UsesType),
                    TypeOutcome::Internal(base)
                ),
            ]
        );
    }

    #[test]
    fn a_csharp_base_list_follows_the_targets_own_kind() {
        let scenario = Scenario::new(&[(
            "App.cs",
            "public class Base { }\npublic interface IShape { }\npublic class Thing { }\n\n\
             public class Child : Base, IShape\n{\n    public Thing Value { get; }\n}\n",
        )]);

        let outcomes = scenario.outcomes("App.cs");
        assert_eq!(
            outcomes[0],
            (
                "Base".to_owned(),
                Some(RelationKind::Extends),
                TypeOutcome::Internal(scenario.symbol("App.cs", "Base"))
            ),
            "a class in the base list is extended"
        );
        assert_eq!(
            outcomes[1],
            (
                "IShape".to_owned(),
                Some(RelationKind::Implements),
                TypeOutcome::Internal(scenario.symbol("App.cs", "IShape"))
            ),
            "an interface is implemented -- because it is declared as one, not because of its name"
        );
        assert_eq!(
            outcomes[2],
            (
                "Thing".to_owned(),
                Some(RelationKind::UsesType),
                TypeOutcome::Internal(scenario.symbol("App.cs", "Thing"))
            )
        );
    }

    #[test]
    fn a_rust_impl_states_which_type_implements_the_trait() {
        let scenario = Scenario::new(&[(
            "src/lib.rs",
            "pub trait Shape {}\npub struct Other {}\n\npub struct Thing { field: Other }\n\n\
             impl Shape for Thing {\n    fn go(&self, x: Other) -> Other { x }\n}\n",
        )]);

        let resolved = scenario.resolve("src/lib.rs");
        let implements = resolved
            .iter()
            .find(|reference| reference.reference.evidence == TypeEvidence::Implements)
            .expect("the impl states a trait");
        assert_eq!(
            implements.outcome,
            TypeOutcome::Internal(scenario.symbol("src/lib.rs", "Shape"))
        );
        assert_eq!(implements.relation_kind, Some(RelationKind::Implements));
        assert_eq!(
            implements.reference.implementor.as_deref(),
            Some("Thing"),
            "the implementing type is the relation's source, not the impl block"
        );

        // The field, parameter, and return types are plain uses.
        let uses: Vec<&str> = resolved
            .iter()
            .filter(|reference| reference.relation_kind == Some(RelationKind::UsesType))
            .map(|reference| reference.reference.name.as_str())
            .collect();
        assert_eq!(uses, vec!["Other", "Other", "Other"]);
    }

    #[test]
    fn an_imported_type_binds_through_its_alias_to_one_canonical_target() {
        let scenario = Scenario::new(&[
            ("src/m.ts", "export class Base {}\n"),
            (
                "src/app.ts",
                "import { Base as Parent } from './m'\n\nexport class Child extends Parent {}\n",
            ),
        ]);

        assert_eq!(
            scenario.outcomes("src/app.ts"),
            vec![(
                "Parent".to_owned(),
                Some(RelationKind::Extends),
                TypeOutcome::Internal(scenario.symbol("src/m.ts", "Base"))
            )],
            "the alias is a local name; the canonical type is the imported Symbol"
        );
    }

    #[test]
    fn an_external_base_is_named_but_not_described() {
        let scenario = Scenario::new(&[(
            "src/app.ts",
            "import { Component } from 'react'\n\nexport class Child extends Component {}\n",
        )]);

        let outcomes = scenario.outcomes("src/app.ts");
        let TypeOutcome::External(external) = &outcomes[0].2 else {
            panic!("expected a package-level target: {:?}", outcomes[0].2);
        };
        assert_eq!(external.package_identity, "react");
        assert_eq!(external.symbol_name.as_deref(), Some("Component"));
        assert_eq!(outcomes[0].1, Some(RelationKind::Extends));
    }

    #[test]
    fn a_same_named_type_elsewhere_is_never_the_target() {
        let scenario = Scenario::new(&[
            ("src/one.ts", "export class Base {}\n"),
            ("src/two.ts", "export class Base {}\n"),
            ("src/app.ts", "export class Child extends Base {}\n"),
        ]);

        assert_eq!(
            scenario.outcomes("src/app.ts"),
            vec![(
                "Base".to_owned(),
                Some(RelationKind::Extends),
                TypeOutcome::Unresolved(UnresolvedType::NoStructuralBinding)
            )],
            "neither file's Base is provably the base here"
        );
    }

    #[test]
    fn an_ambiguous_or_unresolved_import_never_becomes_a_type_target() {
        // Two declarations of one name in the imported module.
        let ambiguous = Scenario::new(&[
            (
                "src/m.ts",
                "export class Base {}\nexport interface Base { value: number }\n",
            ),
            (
                "src/app.ts",
                "import { Base } from './m'\n\nexport class Child extends Base {}\n",
            ),
        ]);
        let outcomes = ambiguous.outcomes("src/app.ts");
        assert!(
            matches!(outcomes[0].2, TypeOutcome::Ambiguous(ref candidates) if candidates.len() == 2),
            "two declarations is not one answer: {:?}",
            outcomes[0].2
        );

        // An import this tier cannot resolve does not fall back to a
        // same-named Symbol elsewhere.
        let unresolved = Scenario::new(&[
            ("src/m.ts", "export class Base {}\n"),
            (
                "src/app.ts",
                "import { Base } from '/aliased'\n\nexport class Child extends Base {}\n",
            ),
        ]);
        assert_eq!(
            unresolved.outcomes("src/app.ts")[0].2,
            TypeOutcome::Unresolved(UnresolvedType::ImportTargetUnresolved)
        );
    }

    #[test]
    fn an_override_marker_never_produces_an_overrides_edge() {
        let scenario = Scenario::new(&[(
            "App.cs",
            "public class Base\n{\n    public virtual int Go() => 0;\n}\n\n\
             public class Child : Base\n{\n    public override int Go() => 1;\n}\n",
        )]);

        let resolved = scenario.resolve("App.cs");
        let marker = resolved
            .iter()
            .find(|reference| reference.reference.evidence == TypeEvidence::OverrideMarker)
            .expect("the marker is kept as evidence");
        assert_eq!(
            marker.outcome,
            TypeOutcome::Unresolved(UnresolvedType::OverrideTargetRequiresSemantics),
            "Base.Go is not confirmed as the target just because the names match"
        );
        assert_eq!(marker.relation_kind, None);
        assert!(
            !resolved
                .iter()
                .any(|reference| reference.relation_kind == Some(RelationKind::Overrides)),
            "no OVERRIDES edge exists at this tier"
        );
        assert!(
            type_relations(scenario.resource("App.cs").id, &[], &[], &resolved, 1)
                .iter()
                .all(|(_, relation)| relation.kind != RelationKind::Overrides)
        );
    }

    #[test]
    fn generic_and_qualified_types_are_left_to_semantics() {
        let scenario = Scenario::new(&[(
            "src/app.ts",
            "export class Base {}\nexport class Child extends Base {\n  \
             values: Array<Base>\n  other: ns.Base\n}\n",
        )]);

        let names: Vec<&str> = scenario
            .resolve("src/app.ts")
            .iter()
            .map(|reference| reference.reference.name.clone())
            .collect::<Vec<_>>()
            .leak()
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(
            names,
            vec!["Base"],
            "a generic application and a qualified path produce no structural evidence"
        );
    }

    #[test]
    fn a_container_only_resource_states_no_types() {
        let source = "<script lang=\"ts\">\n  export class Child extends Base {}\n</script>\n";
        let dialect = dialect_for_path("Widget.svelte").expect("dialect");
        let tree = ParserRegistry::new()
            .parse(dialect, source.as_bytes(), SourceBasis::default())
            .expect("parse");

        assert!(
            extract_type_references(&tree, source.as_bytes()).is_empty(),
            "zero here is a statement about coverage, not about the component"
        );
    }

    /// A real Workspace, to bind evidence through #17 task 3.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    const APP_TS: &str = "\
import { Base } from './m'

export interface Shape {}

export class Child extends Base implements Shape {
  value: Base

  go(x: Base): Shape {
    return this
  }
}
";

    const OTHER_TS: &str = "\
import { Base } from './m'

export class Other extends Base {}
";

    const M_TS: &str = "export class Base {}\n";

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-types-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/other.ts", OTHER_TS);
            fixture.write("src/m.ts", M_TS);
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

        fn occurrences(&self, rel: &str) -> Vec<Occurrence> {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_occurrences_for_resource(self.resource(rel).id)
                .expect("occurrences")
        }

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.db_path()).expect("index.db")
        }

        fn publish_types(&self, rel: &str) -> crate::evidence::EvidenceReplacement {
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
            let locals: HashSet<String> = extract_local_names(&tree, source.as_bytes());
            let own = self.symbols(rel);
            let scope = BindingScope {
                own_symbols: &own,
                module_symbols: &module_symbols,
                imports: &bindings,
                resolved_imports: &imports,
                local_names: &locals,
            };
            let resolved =
                resolve_type_references(extract_type_references(&tree, source.as_bytes()), &scope);

            let store = self.store();
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");
            let occurrences = self.occurrences(rel);
            let evidence: Vec<RelationEvidence> =
                type_relations(resource.id, &occurrences, &own, &resolved, building.id)
                    .into_iter()
                    .map(|(span, relation)| {
                        for endpoint in [&relation.source, &relation.target] {
                            crate::graph::ensure_entity(&transaction, endpoint).expect("ensure");
                        }
                        RelationEvidence {
                            occurrence: OccurrenceRef {
                                kind: OccurrenceKind::TypeSite,
                                start_byte: span.start_byte,
                                end_byte: span.end_byte,
                            },
                            relation,
                        }
                    })
                    .collect();
            let profile_id = own.first().expect("declarations").analysis_profile_id;
            let report = replace_resource_evidence(
                &transaction,
                &grant,
                &EvidenceBasis {
                    owner_resource: resource.id,
                    owner_resource_revision: resource.resource_revision.clone(),
                    generation_id: building.id,
                    analysis_profile_id: profile_id,
                    resolution_context_key: None,
                },
                &evidence,
            )
            .expect("replacement");
            generation::finish_publish_stable(&transaction, &record).expect("stable");
            transaction.commit().expect("commit");
            report
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn type_evidence_binds_to_published_occurrences_and_shares_canonical_edges() {
        let fixture = Fixture::create("binding");
        let base = fixture
            .symbols("src/m.ts")
            .into_iter()
            .find(|symbol| symbol.name == "Base")
            .expect("Base");
        let child = fixture
            .symbols("src/app.ts")
            .into_iter()
            .find(|symbol| symbol.name == "Child")
            .expect("Child");

        // Every extracted span is an Occurrence the index published.
        let published: Vec<(usize, usize)> = fixture
            .occurrences("src/app.ts")
            .into_iter()
            .filter(|occurrence| occurrence.kind == OccurrenceKind::TypeSite)
            .map(|occurrence| (occurrence.span.start_byte, occurrence.span.end_byte))
            .collect();
        let source = fs::read_to_string(fixture.root.join("src/app.ts")).expect("source");
        let dialect = dialect_for_path("src/app.ts").expect("dialect");
        let tree = ParserRegistry::new()
            .parse(dialect, source.as_bytes(), SourceBasis::default())
            .expect("parse");
        let references = extract_type_references(&tree, source.as_bytes());
        for reference in &references {
            assert!(
                published.contains(&(reference.span.start_byte, reference.span.end_byte)),
                "{:?} is not an Occurrence the structural index published",
                reference.name
            );
            assert_eq!(
                &source[reference.span.start_byte..reference.span.end_byte],
                reference.name,
                "the span is the type token exactly"
            );
        }
        let mut spans: Vec<(usize, usize)> = references
            .iter()
            .map(|reference| (reference.span.start_byte, reference.span.end_byte))
            .collect();
        let before = spans.len();
        spans.sort_unstable();
        spans.dedup();
        assert_eq!(spans.len(), before, "two references share one span");

        let report = fixture.publish_types("src/app.ts");
        // extends Base, implements Shape, value: Base, x: Base, : Shape
        assert_eq!(report.occurrences_bound, 5);
        assert_eq!(
            report.relations_bound, 5,
            "Child extends Base and implements Shape; the field and the method each use a \
             type, and they are their own sources"
        );

        let store = fixture.store();
        let from_child = store
            .relations_from(&GraphEndpoint::Symbol(child.id), None)
            .expect("from");
        let kinds: Vec<RelationKind> = from_child.iter().map(|relation| relation.kind).collect();
        assert!(kinds.contains(&RelationKind::Extends));
        assert!(kinds.contains(&RelationKind::Implements));
        assert!(
            from_child
                .iter()
                .filter(|relation| relation.kind == RelationKind::Extends
                    && relation.target == GraphEndpoint::Symbol(base.id))
                .count()
                == 1,
            "one canonical EXTENDS edge"
        );
        // The base clause proves the more specific relation only: the
        // class itself has no USES_TYPE edge, even though `Base` is
        // named right there in its heritage.
        assert!(
            !kinds.contains(&RelationKind::UsesType),
            "a base clause is EXTENDS, and never also a generic type use: {kinds:?}"
        );
        assert!(
            store
                .relations_to(
                    &GraphEndpoint::Symbol(base.id),
                    Some(RelationKind::UsesType)
                )
                .expect("to")
                .iter()
                .any(|relation| matches!(relation.source, GraphEndpoint::Symbol(_))),
            "the field and parameter annotations are their own evidence, from their own members"
        );

        // Another Resource's evidence for the same edge is its own.
        fixture.publish_types("src/other.ts");
        let store = fixture.store();
        assert_eq!(
            store
                .relations_to(&GraphEndpoint::Symbol(base.id), Some(RelationKind::Extends))
                .expect("to")
                .len(),
            2,
            "two subclasses, each proven by its own file"
        );

        fixture.write("src/app.ts", "export const nothing = 1\n");
        fixture.publish_types("src/app.ts");
        let store = fixture.store();
        assert_eq!(
            store
                .relations_to(&GraphEndpoint::Symbol(base.id), Some(RelationKind::Extends))
                .expect("to")
                .len(),
            1,
            "other.ts still extends Base"
        );
    }

    #[test]
    fn type_relations_store_no_source_text() {
        let fixture = Fixture::create("no-source");
        fixture.publish_types("src/app.ts");

        let database = fs::read(fixture.db_path()).expect("index.db bytes");
        // Declaration headers are stored deliberately (#16 task 8's
        // `Symbol.signature`), so the needles are bodies.
        for body in ["return this", "{\n    return this\n  }"] {
            assert!(
                !database
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db must not mirror source text ({body:?})"
            );
        }
    }
}
