//! Structural CALLS/REFERENCES extraction with a safe binding baseline
//! (#17 task 5).
//!
//! I2 already records *where* a call is written. This module decides, for
//! the narrow set of shapes where the source itself settles the question,
//! *what it reaches* -- and refuses everything else.
//!
//! ## What may be confirmed
//!
//! Exactly three shapes:
//!
//! - a bare name declared at the top level of the same file;
//! - a bare name bound by an explicit import, where the imported module
//!   resolved internally (#17 task 4) and declares exactly one Symbol
//!   with that name -- or resolved externally, in which case the package
//!   and the imported name are what the source states and nothing more;
//! - a member on a *type declared in this file* (`Helper.run()`), where
//!   that type declares exactly one member of that name.
//!
//! ## What is refused, and why it must be
//!
//! `obj.foo()` needs the receiver's type. `self.foo()` needs the class
//! hierarchy. An overload set needs argument types. A Rust method needs
//! trait resolution; a C# extension method needs assembly binding. None
//! of that is in the syntax, and the one thing worse than not knowing is
//! an edge that looks confirmed and points at the wrong function.
//!
//! Name uniqueness across the Workspace is not evidence either: that two
//! files each declare `run` and only one is called here is a coincidence,
//! not a binding.
//!
//! Shadowing is handled by refusing rather than by modelling: every
//! parameter and local binding name in the file is collected, and a name
//! in that set is never bound to an outer declaration. That
//! over-approximates -- a name shadowed in one function blocks resolution
//! in another -- which under-resolves and never mis-resolves. Real
//! lexical scoping is a compiler's job (I4).
//!
//! ## CALLS and REFERENCES never describe the same evidence
//!
//! `save()` is a CALL_SITE at the callee and produces CALLS only.
//! `register(save)` is a CALL_SITE at `register` plus a REFERENCE_SITE at
//! `save`: two spans, two edges, and no generic REFERENCES duplicating
//! the call. Nothing infers that `register` calls `save`.
//!
//! ## What this tier does not do
//!
//! Inheritance and type relations (#17 task 6), persisting unresolved
//! references and candidates (task 7), queries (task 8), lifecycle wiring
//! (task 13), and anything a semantic backend would answer (I4). The
//! unresolved outcomes here are runtime values for task 7 to consume, and
//! are written nowhere.

use std::collections::{HashMap, HashSet};

use brainprint_core::{ResourceId, SymbolId};
use tree_sitter::Node;

use crate::{
    extract::{bare_identifier_arguments, span_of},
    graph::{ExternalEntity, GraphEndpoint, Relation, RelationKind},
    imports::{ImportOutcome, ImportStatement, ResolvedImport},
    parser::{ParseTree, ParserDialect, SourceSpan},
    resolution::Dispatch,
    symbol::{Occurrence, OccurrenceKind, Symbol},
};

/// `external_entity.kind` for a name an import explicitly brings in from
/// a package. The source proves the name; it proves nothing about what
/// kind of thing it is, so the kind says only that.
pub const EXTERNAL_IMPORTED_NAME_KIND: &str = "IMPORTED_NAME";

/// How a callee or reference is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callee {
    /// A bare name: `save()`, `register(save)`.
    Name(String),
    /// `receiver.member()` or `receiver::member()`, written with a
    /// single-segment receiver.
    Member { receiver: String, member: String },
    /// Anything else -- a call on a call, an index expression, a
    /// parenthesized lambda. There is no name to bind.
    Other,
}

/// One call or reference, with the exact span that proves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    /// Which Occurrence kind published this span: `CALL_SITE` for a
    /// call, `REFERENCE_SITE` for a name passed as a value.
    pub occurrence_kind: OccurrenceKind,
    pub callee: Callee,
    pub span: SourceSpan,
}

impl CallSite {
    /// The relation a confirmed target would be published as.
    #[must_use]
    pub const fn relation_kind(&self) -> RelationKind {
        match self.occurrence_kind {
            OccurrenceKind::CallSite => RelationKind::Calls,
            _ => RelationKind::References,
        }
    }
}

/// What an `import` binds a local name to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBinding {
    /// The name as used in this file -- the alias when there is one.
    pub local_name: String,
    /// The name in the imported module. `None` for a namespace import,
    /// whose members are named at each use site.
    pub imported_name: Option<String>,
    /// The module specifier this binding came from, so it can be paired
    /// with #17 task 4's resolution of that specifier.
    pub specifier: String,
}

/// What resolution could establish about one call or reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    /// Exactly one Symbol in this Workspace.
    Internal(SymbolId),
    /// A name an import explicitly brings in from a package. Never a
    /// claim about where it is defined or which overload it is.
    External(ExternalEntity),
    /// Several Symbols could be meant. Reported, never narrowed.
    Ambiguous(Vec<SymbolId>),
    Unresolved(UnresolvedCall),
}

impl CallOutcome {
    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        matches!(self, Self::Internal(_) | Self::External(_))
    }
}

/// Why a call or reference has no confirmed target. Runtime only:
/// persisting these is #17 task 7.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedCall {
    /// The name is bound as a parameter or local somewhere in this file,
    /// so an outer declaration of the same name is not provably what is
    /// meant.
    PossiblyShadowed,
    /// `obj.foo()`: which type `obj` has is not in the syntax.
    ReceiverTypeRequired,
    /// The name is not declared in this file and no import binds it.
    NoStructuralBinding,
    /// An import binds the name, but its module did not resolve to one
    /// internal Resource (#17 task 4).
    ImportTargetUnresolved,
    /// The imported module resolved, but nothing in it declares that
    /// name -- or the module's Symbols are not indexed.
    NameNotInModule,
    /// The callee is not a name at all.
    NotANameExpression,
}

/// One call or reference and what resolution made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCall {
    pub site: CallSite,
    pub outcome: CallOutcome,
}

/// Everything the binding rules may consult: this file's own Symbols,
/// the Symbols of modules it imports, the import bindings, and the names
/// that are locally bound.
///
/// Deliberately a plain borrow of already-published data -- the resolver
/// is pure, so what it confirms depends on nothing but the source and
/// the current index.
pub struct BindingScope<'a> {
    /// The owner Resource's published Symbols.
    pub own_symbols: &'a [Symbol],
    /// Symbols of the Resources its imports resolved to.
    pub module_symbols: &'a HashMap<ResourceId, Vec<Symbol>>,
    /// Local name → what an import binds it to.
    pub imports: &'a [ImportBinding],
    /// #17 task 4's resolution of each module specifier.
    pub resolved_imports: &'a [ResolvedImport],
    /// Every parameter and local binding name written in this file.
    pub local_names: &'a HashSet<String>,
}

/// Every call and reference one parse tree states, in source order.
///
/// The spans are exactly the ones #16 task 9's walker publishes as
/// `CALL_SITE`/`REFERENCE_SITE` Occurrences, so a resolved target binds
/// to evidence that already exists.
#[must_use]
pub fn extract_call_sites(tree: &ParseTree, source: &[u8]) -> Vec<CallSite> {
    if !tree.descriptor().capability.covers_whole_file() {
        return Vec::new();
    }
    let dialect = tree.descriptor().dialect;
    let mut sites = Vec::new();
    walk(tree.syntax_tree().root_node(), &mut |node| {
        if !is_call(node, dialect) {
            return;
        }
        if let Some(callee) = node.child_by_field_name("function") {
            sites.push(CallSite {
                occurrence_kind: OccurrenceKind::CallSite,
                callee: callee_of(callee, source),
                span: span_of(callee),
            });
        }
        for argument in bare_identifier_arguments(node) {
            sites.push(CallSite {
                occurrence_kind: OccurrenceKind::ReferenceSite,
                callee: Callee::Name(text_of(argument, source)),
                span: span_of(argument),
            });
        }
    });
    sites.sort_by_key(|site| site.span.start_byte);
    sites
}

/// Every local name an `import` introduces, with what it names.
#[must_use]
pub fn extract_import_bindings(tree: &ParseTree, source: &[u8]) -> Vec<ImportBinding> {
    if !tree.descriptor().capability.covers_whole_file() {
        return Vec::new();
    }
    let dialect = tree.descriptor().dialect;
    let mut bindings = Vec::new();
    walk(tree.syntax_tree().root_node(), &mut |node| {
        collect_bindings(node, dialect, source, &mut bindings);
    });
    bindings
}

/// Every parameter and local binding name written in this file.
///
/// An over-approximation on purpose: it is the set a name must *not* be
/// in for an outer declaration to be provably what a use site means.
#[must_use]
pub fn extract_local_names(tree: &ParseTree, source: &[u8]) -> HashSet<String> {
    if !tree.descriptor().capability.covers_whole_file() {
        return HashSet::new();
    }
    let dialect = tree.descriptor().dialect;
    let mut names = HashSet::new();
    walk(tree.syntax_tree().root_node(), &mut |node| {
        collect_local_names(node, dialect, source, &mut names);
    });
    names
}

/// Resolve every call and reference against the binding scope.
#[must_use]
pub fn resolve_calls(sites: Vec<CallSite>, scope: &BindingScope<'_>) -> Vec<ResolvedCall> {
    sites
        .into_iter()
        .map(|site| ResolvedCall {
            outcome: resolve_site(&site, scope),
            site,
        })
        .collect()
}

fn resolve_site(site: &CallSite, scope: &BindingScope<'_>) -> CallOutcome {
    match &site.callee {
        Callee::Other => CallOutcome::Unresolved(UnresolvedCall::NotANameExpression),
        Callee::Name(name) => resolve_name(name, scope),
        Callee::Member { receiver, member } => resolve_member(receiver, member, scope),
    }
}

/// A bare name: an explicit import binding first, then this file's own
/// top-level declarations. Either way, a locally bound name is refused.
///
/// Public because the binding rules are the same wherever a name has to
/// be bound structurally -- #17 task 6 resolves type names through this
/// exact path rather than growing a second, subtly different one.
pub fn resolve_name(name: &str, scope: &BindingScope<'_>) -> CallOutcome {
    if scope.local_names.contains(name) {
        return CallOutcome::Unresolved(UnresolvedCall::PossiblyShadowed);
    }
    if let Some(binding) = scope
        .imports
        .iter()
        .find(|binding| binding.local_name == name)
    {
        let Some(imported) = binding.imported_name.as_deref() else {
            // A namespace binding names no member by itself.
            return CallOutcome::Unresolved(UnresolvedCall::ReceiverTypeRequired);
        };
        return resolve_imported(binding, imported, scope);
    }
    match unique_top_level(scope.own_symbols, name) {
        Ok(Some(symbol)) => CallOutcome::Internal(symbol),
        Ok(None) => CallOutcome::Unresolved(UnresolvedCall::NoStructuralBinding),
        Err(candidates) => CallOutcome::Ambiguous(candidates),
    }
}

/// `receiver.member()`. Two shapes are structural: a namespace import's
/// member, and a member of a type declared in this file. Everything else
/// needs the receiver's type.
pub fn resolve_member(receiver: &str, member: &str, scope: &BindingScope<'_>) -> CallOutcome {
    if scope.local_names.contains(receiver) {
        // `obj.foo()` where `obj` is a parameter or a local: the type is
        // exactly what is not known here.
        return CallOutcome::Unresolved(UnresolvedCall::ReceiverTypeRequired);
    }
    if let Some(binding) = scope
        .imports
        .iter()
        .find(|binding| binding.local_name == receiver && binding.imported_name.is_none())
    {
        return resolve_imported(binding, member, scope);
    }
    // A type declared right here: `Helper.run()` is unambiguous when
    // `Helper` declares exactly one `run`.
    if unique_top_level(scope.own_symbols, receiver)
        .unwrap_or_default()
        .is_some()
    {
        let qualified = format!("{receiver}.{member}");
        let matches: Vec<SymbolId> = scope
            .own_symbols
            .iter()
            .filter(|symbol| symbol.qualified_name == qualified)
            .map(|symbol| symbol.id)
            .collect();
        return match matches.len() {
            1 => CallOutcome::Internal(matches[0]),
            0 => CallOutcome::Unresolved(UnresolvedCall::ReceiverTypeRequired),
            _ => CallOutcome::Ambiguous(matches),
        };
    }
    CallOutcome::Unresolved(UnresolvedCall::ReceiverTypeRequired)
}

/// Follow an import binding to what the source proves it names.
fn resolve_imported(
    binding: &ImportBinding,
    imported_name: &str,
    scope: &BindingScope<'_>,
) -> CallOutcome {
    let Some(import) = scope
        .resolved_imports
        .iter()
        .find(|import| import.statement.specifier == binding.specifier)
    else {
        return CallOutcome::Unresolved(UnresolvedCall::ImportTargetUnresolved);
    };
    match &import.outcome {
        ImportOutcome::Internal(resource) => {
            let Some(symbols) = scope.module_symbols.get(resource) else {
                // The module resolved, but its Symbols are not indexed,
                // so nothing in it can be confirmed.
                return CallOutcome::Unresolved(UnresolvedCall::NameNotInModule);
            };
            match unique_top_level(symbols, imported_name) {
                Ok(Some(symbol)) => CallOutcome::Internal(symbol),
                Ok(None) => CallOutcome::Unresolved(UnresolvedCall::NameNotInModule),
                Err(candidates) => CallOutcome::Ambiguous(candidates),
            }
        }
        ImportOutcome::External(module) => CallOutcome::External(ExternalEntity {
            // The package and the imported name are exactly what the
            // import statement says. Where it is defined, and which
            // overload it is, are not.
            symbol_name: Some(imported_name.to_owned()),
            kind: EXTERNAL_IMPORTED_NAME_KIND.to_owned(),
            ..module.clone()
        }),
        ImportOutcome::Ambiguous(_) | ImportOutcome::Unresolved(_) => {
            CallOutcome::Unresolved(UnresolvedCall::ImportTargetUnresolved)
        }
    }
}

/// The one top-level Symbol with this name, `None` if there is none, or
/// the candidates when there are several.
///
/// Top level only: a method named `run` is not what a bare `run()`
/// means, and deciding that it is would need a receiver.
fn unique_top_level(symbols: &[Symbol], name: &str) -> Result<Option<SymbolId>, Vec<SymbolId>> {
    let matches: Vec<SymbolId> = symbols
        .iter()
        .filter(|symbol| symbol.parent_id.is_none() && symbol.name == name)
        .map(|symbol| symbol.id)
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => Err(matches),
    }
}

/// Turn resolved calls and references into canonical edges, paired with
/// the span that proves each one.
///
/// The source endpoint is the Symbol the evidence sits inside when there
/// is one, and the Resource itself otherwise -- module-level code is not
/// given an invented Symbol to be the caller (#17 "Entity").
///
/// Ambiguous and unresolved outcomes produce no edge and are not
/// downgraded into a guess; they stay in the [`ResolvedCall`] list for
/// #17 task 7.
#[must_use]
pub fn call_relations(
    owner: ResourceId,
    occurrences: &[Occurrence],
    resolved: &[ResolvedCall],
    created_generation: i64,
) -> Vec<(SourceSpan, Relation)> {
    resolved
        .iter()
        .filter_map(|call| {
            let target = match &call.outcome {
                CallOutcome::Internal(symbol) => GraphEndpoint::Symbol(*symbol),
                CallOutcome::External(external) => GraphEndpoint::External(external.clone()),
                CallOutcome::Ambiguous(_) | CallOutcome::Unresolved(_) => return None,
            };
            let containing = occurrences
                .iter()
                .find(|occurrence| {
                    occurrence.kind == call.site.occurrence_kind
                        && occurrence.span.start_byte == call.site.span.start_byte
                        && occurrence.span.end_byte == call.site.span.end_byte
                })
                .and_then(|occurrence| occurrence.containing_symbol_id);
            let source = containing.map_or(GraphEndpoint::Resource(owner), GraphEndpoint::Symbol);
            Some((
                call.site.span,
                Relation {
                    kind: call.site.relation_kind(),
                    source,
                    target,
                    // Whether the binding is static is a claim about
                    // dispatch, and only the shapes this tier resolves
                    // are provably direct.
                    dispatch: Dispatch::Static,
                    created_generation,
                },
            ))
        })
        .collect()
}

/// Visit every named node, outermost first.
fn walk(node: Node<'_>, visit: &mut impl FnMut(Node<'_>)) {
    visit(node);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, visit);
    }
}

fn is_call(node: Node<'_>, dialect: ParserDialect) -> bool {
    matches!(
        (dialect, node.kind()),
        (ParserDialect::Python, "call")
            | (
                ParserDialect::JavaScript
                    | ParserDialect::Jsx
                    | ParserDialect::TypeScript
                    | ParserDialect::Tsx
                    | ParserDialect::Rust,
                "call_expression"
            )
            | (ParserDialect::CSharp, "invocation_expression")
    )
}

/// The shape of a callee expression, as written.
fn callee_of(node: Node<'_>, source: &[u8]) -> Callee {
    match node.kind() {
        "identifier" => Callee::Name(text_of(node, source)),
        // `a.b` in Python/JS/TS/C#, `a::b` in Rust.
        "attribute" | "member_expression" | "member_access_expression" | "scoped_identifier" => {
            let (receiver, member) = match node.kind() {
                "attribute" => (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("attribute"),
                ),
                "member_expression" => (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("property"),
                ),
                "member_access_expression" => (
                    node.child_by_field_name("expression"),
                    node.child_by_field_name("name"),
                ),
                _ => (
                    node.child_by_field_name("path"),
                    node.child_by_field_name("name"),
                ),
            };
            match (receiver, member) {
                // Only a single-segment receiver is a name this tier can
                // reason about: `a.b.c()` is a chain, and chains need
                // types.
                (Some(receiver), Some(member)) if receiver.kind() == "identifier" => {
                    Callee::Member {
                        receiver: text_of(receiver, source),
                        member: text_of(member, source),
                    }
                }
                _ => Callee::Other,
            }
        }
        _ => Callee::Other,
    }
}

fn collect_bindings(
    node: Node<'_>,
    dialect: ParserDialect,
    source: &[u8],
    bindings: &mut Vec<ImportBinding>,
) {
    match (dialect, node.kind()) {
        (ParserDialect::Python, "import_from_statement") => {
            let Some(module) = node.child_by_field_name("module_name") else {
                return;
            };
            let specifier = text_of(module, source);
            let specifier = specifier.trim_start_matches('.').to_owned();
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                let (imported, local) = aliased_pair(name, source, "name", "alias");
                bindings.push(ImportBinding {
                    local_name: local,
                    imported_name: Some(imported),
                    specifier: specifier.clone(),
                });
            }
        }
        (
            ParserDialect::JavaScript
            | ParserDialect::Jsx
            | ParserDialect::TypeScript
            | ParserDialect::Tsx,
            "import_statement",
        ) => {
            let Some(source_node) = node.child_by_field_name("source") else {
                return;
            };
            let specifier = text_of(source_node, source)
                .trim_matches(['\'', '"', '`'])
                .to_owned();
            let mut cursor = node.walk();
            let mut stack: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            while let Some(current) = stack.pop() {
                match current.kind() {
                    "import_specifier" => {
                        let (imported, local) = aliased_pair(current, source, "name", "alias");
                        bindings.push(ImportBinding {
                            local_name: local,
                            imported_name: Some(imported),
                            specifier: specifier.clone(),
                        });
                    }
                    "namespace_import" => {
                        let mut inner = current.walk();
                        if let Some(alias) = current
                            .named_children(&mut inner)
                            .find(|child| child.kind() == "identifier")
                        {
                            bindings.push(ImportBinding {
                                local_name: text_of(alias, source),
                                imported_name: None,
                                specifier: specifier.clone(),
                            });
                        }
                    }
                    _ => {
                        let mut inner = current.walk();
                        stack.extend(current.named_children(&mut inner));
                    }
                }
            }
        }
        _ => {}
    }
}

/// A `name as alias` pair, or the same name twice when there is no
/// alias. The alias is a local binding; the imported name is canonical.
fn aliased_pair(
    node: Node<'_>,
    source: &[u8],
    name_field: &str,
    alias_field: &str,
) -> (String, String) {
    let imported = node
        .child_by_field_name(name_field)
        .map_or_else(|| text_of(node, source), |child| text_of(child, source));
    let local = node
        .child_by_field_name(alias_field)
        .map_or_else(|| imported.clone(), |child| text_of(child, source));
    (imported, local)
}

fn collect_local_names(
    node: Node<'_>,
    dialect: ParserDialect,
    source: &[u8],
    names: &mut HashSet<String>,
) {
    // Parameters, in every dialect: whatever a parameter list contains
    // that is a plain name.
    if matches!(
        node.kind(),
        "parameters" | "formal_parameters" | "parameter_list" | "lambda_parameters"
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            push_pattern_names(child, source, names);
        }
    }
    match (dialect, node.kind()) {
        (ParserDialect::Python, "assignment" | "for_statement") => {
            if let Some(left) = node.child_by_field_name("left") {
                push_pattern_names(left, source, names);
            }
        }
        (
            ParserDialect::JavaScript
            | ParserDialect::Jsx
            | ParserDialect::TypeScript
            | ParserDialect::Tsx,
            "variable_declarator",
        ) => {
            if let Some(name) = node.child_by_field_name("name") {
                push_pattern_names(name, source, names);
            }
        }
        (ParserDialect::Rust, "let_declaration") => {
            if let Some(pattern) = node.child_by_field_name("pattern") {
                push_pattern_names(pattern, source, names);
            }
        }
        (ParserDialect::CSharp, "variable_declarator") => {
            push_pattern_names(node, source, names);
        }
        _ => {}
    }
}

/// Every plain name a binding pattern introduces.
fn push_pattern_names(node: Node<'_>, source: &[u8], names: &mut HashSet<String>) {
    match node.kind() {
        "identifier" => {
            names.insert(text_of(node, source));
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                // A parameter's type annotation is not a binding.
                if matches!(child.kind(), "type" | "type_annotation" | "type_identifier") {
                    continue;
                }
                push_pattern_names(child, source, names);
            }
        }
    }
}

fn text_of(node: Node<'_>, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]).into_owned()
}

/// Pair #17 task 4's module resolution with the local bindings this
/// module extracted, so a caller can build a [`BindingScope`] without
/// re-deriving either.
#[must_use]
pub fn import_statements(resolved: &[ResolvedImport]) -> Vec<&ImportStatement> {
    resolved.iter().map(|import| &import.statement).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
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

    /// A Workspace of source files, resolved purely in memory: no
    /// database, so what the resolver confirms depends only on the
    /// source and the inventory.
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
                symbols.insert(resource.id, symbols_of(path, source, resource));
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

        /// Extract and resolve one file's calls and references.
        fn resolve(&self, path: &str) -> Vec<ResolvedCall> {
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
            resolve_calls(extract_call_sites(&tree, source.as_bytes()), &scope)
        }

        /// One file's outcomes as `(written text, kind, outcome)`.
        fn outcomes(&self, path: &str) -> Vec<(String, RelationKind, CallOutcome)> {
            let source = self.source(path);
            self.resolve(path)
                .into_iter()
                .map(|call| {
                    (
                        source[call.site.span.start_byte..call.site.span.end_byte].to_owned(),
                        call.site.relation_kind(),
                        call.outcome,
                    )
                })
                .collect()
        }
    }

    fn symbols_of(path: &str, source: &str, resource: &Resource) -> Vec<Symbol> {
        let Ok(dialect) = dialect_for_path(path) else {
            return Vec::new();
        };
        let tree = ParserRegistry::new()
            .parse(dialect, source.as_bytes(), SourceBasis::of(resource))
            .expect("parse");
        let extraction = extract(&tree, source.as_bytes());
        assign_ids(&[], &extraction, resource, 1)
    }

    #[test]
    fn a_same_file_direct_call_binds_to_its_declaration() {
        let scenario = Scenario::new(&[(
            "app.py",
            "def save():\n    return 1\n\n\ndef run():\n    return save()\n",
        )]);

        assert_eq!(
            scenario.outcomes("app.py"),
            vec![(
                "save".to_owned(),
                RelationKind::Calls,
                CallOutcome::Internal(scenario.symbol("app.py", "save"))
            )]
        );

        // Rust and C# get the same treatment for the same shape.
        let rust = Scenario::new(&[(
            "src/lib.rs",
            "fn helper() -> i32 { 1 }\n\npub fn run() -> i32 { helper() }\n",
        )]);
        assert_eq!(
            rust.outcomes("src/lib.rs"),
            vec![(
                "helper".to_owned(),
                RelationKind::Calls,
                CallOutcome::Internal(rust.symbol("src/lib.rs", "helper"))
            )]
        );
    }

    #[test]
    fn a_csharp_call_on_a_type_declared_here_is_structural() {
        let scenario = Scenario::new(&[(
            "App.cs",
            "public class Helper\n{\n    public static int Run() => 1;\n}\n\n\
             public class App\n{\n    public int Go() => Helper.Run();\n}\n",
        )]);

        let outcomes = scenario.outcomes("App.cs");
        assert_eq!(
            outcomes,
            vec![(
                "Helper.Run".to_owned(),
                RelationKind::Calls,
                CallOutcome::Internal(scenario.symbol("App.cs", "Helper.Run"))
            )],
            "the receiver is a type declared in this file with one such member"
        );
    }

    #[test]
    fn an_explicit_import_binds_and_its_alias_does_not_move_the_target() {
        let files = [
            ("pkg/m.py", "def save():\n    return 1\n"),
            (
                "pkg/app.py",
                "from .m import save\n\n\ndef run():\n    return save()\n",
            ),
            (
                "pkg/aliased.py",
                "from .m import save as store\n\n\ndef run():\n    return store()\n",
            ),
        ];
        let scenario = Scenario::new(&files);
        let target = scenario.symbol("pkg/m.py", "save");

        assert_eq!(
            scenario.outcomes("pkg/app.py"),
            vec![(
                "save".to_owned(),
                RelationKind::Calls,
                CallOutcome::Internal(target)
            )]
        );
        assert_eq!(
            scenario.outcomes("pkg/aliased.py"),
            vec![(
                "store".to_owned(),
                RelationKind::Calls,
                CallOutcome::Internal(target)
            )],
            "the alias is a local name; the canonical target is the same Symbol"
        );
    }

    #[test]
    fn a_typescript_named_import_alias_binds_to_the_imported_symbol() {
        let scenario = Scenario::new(&[
            (
                "src/m.ts",
                "export function save(): number {\n  return 1\n}\n",
            ),
            (
                "src/app.ts",
                "import { save as store } from './m'\n\nexport function run(): number {\n  \
                 return store()\n}\n",
            ),
        ]);

        assert_eq!(
            scenario.outcomes("src/app.ts"),
            vec![(
                "store".to_owned(),
                RelationKind::Calls,
                CallOutcome::Internal(scenario.symbol("src/m.ts", "save"))
            )]
        );
    }

    #[test]
    fn a_namespace_import_member_is_structural_when_the_module_declares_it_once() {
        let scenario = Scenario::new(&[
            (
                "src/m.ts",
                "export function save(): number {\n  return 1\n}\n",
            ),
            (
                "src/app.ts",
                "import * as store from './m'\n\nexport function run(): number {\n  \
                 return store.save()\n}\n",
            ),
        ]);

        assert_eq!(
            scenario.outcomes("src/app.ts"),
            vec![(
                "store.save".to_owned(),
                RelationKind::Calls,
                CallOutcome::Internal(scenario.symbol("src/m.ts", "save"))
            )]
        );
    }

    #[test]
    fn an_external_named_import_is_stated_no_further_than_the_source_does() {
        let scenario = Scenario::new(&[(
            "src/app.ts",
            "import { leftPad } from 'left-pad'\n\nexport function run(): string {\n  \
             return leftPad('x')\n}\n",
        )]);

        let outcomes = scenario.outcomes("src/app.ts");
        let CallOutcome::External(external) = &outcomes[0].2 else {
            panic!("expected a package-level target: {:?}", outcomes[0].2);
        };
        assert_eq!(external.package_identity, "left-pad");
        assert_eq!(external.symbol_name.as_deref(), Some("leftPad"));
        assert_eq!(
            external.declaration_locator, None,
            "the source proves the name, not where it is defined"
        );
        assert_eq!(external.resolved_version, None);
    }

    #[test]
    fn a_call_produces_calls_and_a_passed_name_produces_references() {
        let scenario = Scenario::new(&[(
            "app.py",
            "def save():\n    return 1\n\n\ndef register(handler):\n    return handler\n\n\n\
             def run():\n    save()\n    register(save)\n",
        )]);
        let save = scenario.symbol("app.py", "save");
        let register = scenario.symbol("app.py", "register");

        assert_eq!(
            scenario.outcomes("app.py"),
            vec![
                // `save()` is a call, and nothing else.
                (
                    "save".to_owned(),
                    RelationKind::Calls,
                    CallOutcome::Internal(save)
                ),
                // `register(save)` calls register and references save.
                (
                    "register".to_owned(),
                    RelationKind::Calls,
                    CallOutcome::Internal(register)
                ),
                (
                    "save".to_owned(),
                    RelationKind::References,
                    CallOutcome::Internal(save)
                ),
            ]
        );

        // No REFERENCES duplicates the call: the callee span appears
        // once, as a CALL_SITE.
        let sites = scenario.resolve("app.py");
        let call_spans: Vec<(usize, usize)> = sites
            .iter()
            .filter(|call| call.site.occurrence_kind == OccurrenceKind::CallSite)
            .map(|call| (call.site.span.start_byte, call.site.span.end_byte))
            .collect();
        let reference_spans: Vec<(usize, usize)> = sites
            .iter()
            .filter(|call| call.site.occurrence_kind == OccurrenceKind::ReferenceSite)
            .map(|call| (call.site.span.start_byte, call.site.span.end_byte))
            .collect();
        assert!(
            !reference_spans.iter().any(|span| call_spans.contains(span)),
            "a callee is never also recorded as a reference"
        );
        assert_eq!(reference_spans.len(), 1);

        // Every span is exactly the name as written.
        let source = scenario.source("app.py");
        for call in &sites {
            let text = &source[call.site.span.start_byte..call.site.span.end_byte];
            assert!(
                text == "save" || text == "register",
                "unexpected evidence span {text:?}"
            );
        }
    }

    #[test]
    fn a_locally_bound_name_never_resolves_to_an_outer_declaration() {
        let scenario = Scenario::new(&[(
            "app.py",
            "def save():\n    return 1\n\n\ndef run(save):\n    return save()\n",
        )]);

        assert_eq!(
            scenario.outcomes("app.py"),
            vec![(
                "save".to_owned(),
                RelationKind::Calls,
                CallOutcome::Unresolved(UnresolvedCall::PossiblyShadowed)
            )],
            "the parameter may be what is called, so neither answer is provable here"
        );

        // A local variable shadows just as effectively.
        let typescript = Scenario::new(&[(
            "src/app.ts",
            "export function save(): number {\n  return 1\n}\n\n\
             export function run(): number {\n  const save = () => 2\n  return save()\n}\n",
        )]);
        assert_eq!(
            typescript.outcomes("src/app.ts")[0].2,
            CallOutcome::Unresolved(UnresolvedCall::PossiblyShadowed)
        );
    }

    #[test]
    fn a_receiver_whose_type_is_unknown_is_never_bound_by_name() {
        let scenario = Scenario::new(&[(
            "app.py",
            "class Thing:\n    def foo(self):\n        return 1\n\n\n\
             def run(obj):\n    obj.foo()\n    return self_free()\n",
        )]);

        let outcomes = scenario.outcomes("app.py");
        assert_eq!(
            outcomes[0].2,
            CallOutcome::Unresolved(UnresolvedCall::ReceiverTypeRequired),
            "obj.foo() must not be bound to Thing.foo by name"
        );
        assert_eq!(
            outcomes[1].2,
            CallOutcome::Unresolved(UnresolvedCall::NoStructuralBinding)
        );

        // `self.foo()` is the same refusal: the class hierarchy decides.
        let method = Scenario::new(&[(
            "app.py",
            "class Thing:\n    def foo(self):\n        return 1\n\n    def bar(self):\n        \
             return self.foo()\n",
        )]);
        assert_eq!(
            method.outcomes("app.py")[0].2,
            CallOutcome::Unresolved(UnresolvedCall::ReceiverTypeRequired)
        );
    }

    #[test]
    fn a_same_name_declaration_elsewhere_is_not_a_binding() {
        // Two files declare `save`; app.py imports neither.
        let scenario = Scenario::new(&[
            ("src/one.py", "def save():\n    return 1\n"),
            ("src/two.py", "def save():\n    return 2\n"),
            ("src/app.py", "def run():\n    return save()\n"),
        ]);

        assert_eq!(
            scenario.outcomes("src/app.py")[0].2,
            CallOutcome::Unresolved(UnresolvedCall::NoStructuralBinding),
            "uniqueness across the Workspace would not make it a binding either"
        );
    }

    #[test]
    fn two_declarations_of_one_name_in_a_module_stay_ambiguous() {
        // An overload set, or a redefinition: either way, which one a
        // call reaches is not in the syntax.
        let scenario = Scenario::new(&[(
            "App.cs",
            "public class Api\n{\n    public static int Run(int a) => a;\n    \
                 public static int Run(string a) => 0;\n}\n\n\
                 public class Caller\n{\n    public int Go() => Api.Run(1);\n}\n",
        )]);

        let outcomes = scenario.outcomes("App.cs");
        let CallOutcome::Ambiguous(candidates) = &outcomes[0].2 else {
            panic!("an overload set must not be resolved: {:?}", outcomes[0].2);
        };
        assert_eq!(candidates.len(), 2);
        assert!(!outcomes[0].2.is_resolved());
    }

    #[test]
    fn an_unresolved_import_leaves_its_uses_unresolved_rather_than_guessing() {
        let scenario = Scenario::new(&[
            (
                "src/m.ts",
                "export function save(): number {\n  return 1\n}\n",
            ),
            (
                "src/app.ts",
                "import { save } from '/aliased'\n\nexport function run(): number {\n  \
                 return save()\n}\n",
            ),
        ]);

        assert_eq!(
            scenario.outcomes("src/app.ts")[0].2,
            CallOutcome::Unresolved(UnresolvedCall::ImportTargetUnresolved),
            "a config-dependent specifier does not become src/m.ts by name"
        );
    }

    #[test]
    fn unresolved_outcomes_are_kept_for_the_gap_lifecycle_to_consume() {
        let scenario = Scenario::new(&[(
            "app.py",
            "def run(obj):\n    obj.foo()\n    missing()\n    return 1\n",
        )]);

        let resolved = scenario.resolve("app.py");
        assert_eq!(resolved.len(), 2, "nothing is dropped for being unresolved");
        assert!(resolved.iter().all(|call| !call.outcome.is_resolved()));
        assert!(
            call_relations(scenario.resource("app.py").id, &[], &resolved, 1).is_empty(),
            "and none of it becomes an edge"
        );
    }

    #[test]
    fn a_container_only_resource_states_no_calls() {
        let source = "<script lang=\"ts\">\n  save()\n</script>\n";
        let dialect = dialect_for_path("Widget.svelte").expect("dialect");
        let tree = ParserRegistry::new()
            .parse(dialect, source.as_bytes(), SourceBasis::default())
            .expect("parse");

        assert!(extract_call_sites(&tree, source.as_bytes()).is_empty());
        assert!(extract_import_bindings(&tree, source.as_bytes()).is_empty());
    }

    /// A real Workspace, so evidence can be bound through #17 task 3's
    /// replacement against the Occurrences #16 published.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    const APP_TS: &str = "\
import { save } from './m'

export function run(): number {
  save()
  register(save)
  return save()
}

export function register(handler: () => number): number {
  return handler()
}
";

    const OTHER_TS: &str = "\
import { save } from './m'

export function other(): number {
  return save()
}
";

    const M_TS: &str = "export function save(): number {\n  return 1\n}\n";

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-calls-{label}-{}-{sequence}",
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

        /// Extract, resolve, and publish one file's call/reference
        /// evidence through the Resource-owned replacement.
        fn publish_calls(&self, rel: &str) -> crate::evidence::EvidenceReplacement {
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
            let scope = BindingScope {
                own_symbols: &own,
                module_symbols: &module_symbols,
                imports: &bindings,
                resolved_imports: &imports,
                local_names: &locals,
            };
            let resolved = resolve_calls(extract_call_sites(&tree, source.as_bytes()), &scope);

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
                call_relations(resource.id, &occurrences, &resolved, building.id)
                    .into_iter()
                    .map(|(span, relation)| {
                        for endpoint in [&relation.source, &relation.target] {
                            crate::graph::ensure_entity(&transaction, endpoint).expect("ensure");
                        }
                        let kind = resolved
                            .iter()
                            .find(|call| call.site.span.start_byte == span.start_byte)
                            .expect("the span came from one of these")
                            .site
                            .occurrence_kind;
                        RelationEvidence {
                            occurrence: OccurrenceRef {
                                kind,
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
    fn evidence_binds_to_published_occurrences_and_shares_one_canonical_edge() {
        let fixture = Fixture::create("binding");
        let app = fixture.resource("src/app.ts");
        let save = fixture
            .symbols("src/m.ts")
            .into_iter()
            .find(|symbol| symbol.name == "save")
            .expect("save");
        let run = fixture
            .symbols("src/app.ts")
            .into_iter()
            .find(|symbol| symbol.name == "run")
            .expect("run");

        let report = fixture.publish_calls("src/app.ts");
        // `save()` twice and `register(save)` once: three proofs.
        assert_eq!(report.occurrences_bound, 4);
        assert_eq!(
            report.relations_bound, 3,
            "CALLS(save) and REFERENCES(save) from run, plus CALLS(register)"
        );

        let store = fixture.store();
        let from_run = store
            .relations_from(&GraphEndpoint::Symbol(run.id), None)
            .expect("from");
        let kinds: Vec<RelationKind> = from_run.iter().map(|relation| relation.kind).collect();
        assert!(kinds.contains(&RelationKind::Calls));
        assert!(
            kinds.contains(&RelationKind::References),
            "passing save as a value is a reference, not a second call"
        );
        assert!(
            from_run
                .iter()
                .all(|relation| relation.target == GraphEndpoint::Symbol(save.id)
                    || matches!(relation.target, GraphEndpoint::Symbol(_))),
            "targets are Symbols by stable id"
        );
        // Two `save()` calls are one edge with two proofs.
        assert_eq!(
            from_run
                .iter()
                .filter(|relation| relation.kind == RelationKind::Calls
                    && relation.target == GraphEndpoint::Symbol(save.id))
                .count(),
            1
        );

        // Another Resource's evidence for the same edge is its own.
        fixture.publish_calls("src/other.ts");
        let store = fixture.store();
        assert_eq!(
            store
                .relations_to(&GraphEndpoint::Symbol(save.id), Some(RelationKind::Calls))
                .expect("to")
                .len(),
            2,
            "two callers, each proven by its own file"
        );

        // Re-publishing app.ts with nothing to say leaves other.ts alone.
        fixture.write("src/app.ts", "export const nothing = 1\n");
        fixture.publish_calls("src/app.ts");
        let store = fixture.store();
        assert_eq!(
            store
                .relations_to(&GraphEndpoint::Symbol(save.id), Some(RelationKind::Calls))
                .expect("to")
                .len(),
            1,
            "other.ts still calls save"
        );
        assert!(
            store
                .relations_from(&GraphEndpoint::Resource(app.id), None)
                .expect("from")
                .is_empty()
        );
    }

    #[test]
    fn call_relations_store_no_source_text() {
        let fixture = Fixture::create("no-source");
        fixture.publish_calls("src/app.ts");

        let database = fs::read(fixture.db_path()).expect("index.db bytes");
        for body in ["register(save)", "return save()", "return handler()"] {
            assert!(
                !database
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db must not mirror source text ({body:?})"
            );
        }
    }
}
