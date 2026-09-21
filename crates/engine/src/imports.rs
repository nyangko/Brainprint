//! Structural IMPORTS extraction and deterministic target resolution
//! (#17 task 4).
//!
//! An import statement is the cheapest true thing a file says about its
//! dependencies: it names a module or a package in the source itself, in
//! a syntax the grammar already parses. This module reads that name and
//! -- only when the answer is structurally certain -- turns it into a
//! canonical IMPORTS edge.
//!
//! ## What "certain" means here
//!
//! An internal target is confirmed only when the specifier maps to
//! *exactly one* Resource in the current inventory. Two candidates is
//! not "probably the first"; zero candidates for a relative import is a
//! missing file, not a package. Nothing is ever resolved by name
//! similarity, and no semantic backend is consulted -- that is I4's, and
//! pretending otherwise here would produce edges that look confirmed and
//! are not.
//!
//! An external target is stored at the level the source actually states.
//! `import { x } from 'left-pad/sub'` proves a package called
//! `left-pad` and a module path inside it; it does not prove that a
//! symbol named `x` exists there, so no external symbol identity is
//! invented.
//!
//! ## What the edge means
//!
//! One IMPORTS edge per *module* named, from the importing Resource to
//! the imported Resource or package. `from p.q import r, s` names one
//! module, so it is one edge -- the imported identifiers are not inflated
//! into three. Two statements importing the same module are two pieces of
//! evidence for one canonical edge (#17 task 3).
//!
//! ## Evidence
//!
//! The evidence span is the module specifier exactly as written -- the
//! narrowest range that actually supports the edge -- and it is the same
//! span #16 task 9's walker already records as an `IMPORT_SITE`
//! Occurrence. Nothing here creates a new Occurrence or a new
//! `(kind, start, end)`: the edge binds to evidence that is already
//! published, which is what makes the binding deterministic.
//!
//! ## What this tier does not do
//!
//! CALLS/REFERENCES (task 5), inheritance and type relations (task 6),
//! persisting unresolved references and candidates (task 7) -- the
//! unresolved outcomes here are returned as runtime values for task 7 to
//! consume, and written nowhere. Lifecycle wiring is task 13.

use std::collections::HashMap;

use brainprint_core::ResourceId;
use tree_sitter::Node;

use crate::{
    extract::{children_by_field, span_of},
    graph::{ExternalEntity, GraphEndpoint, Relation, RelationKind},
    parser::{ParseTree, ParserDialect, SourceSpan},
    resolution::Dispatch,
    resource::{Resource, ResourceState},
};

/// `external_entity.kind` for a target known only as a package or module.
/// No symbol-level claim is made, so no symbol-level kind is written.
pub const EXTERNAL_MODULE_KIND: &str = "MODULE";

/// How a specifier is written, which decides how it may be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportForm {
    /// A dotted or slashed path that does not start from the importing
    /// file: `os`, `p.q`, `react`, `@scope/pkg`, `std::collections`.
    Absolute,
    /// Relative to the importing file. `levels` is how far up to go
    /// before applying the tail: 1 is the file's own directory.
    Relative { levels: usize },
    /// Rust's `crate::`/`self::`/`super::`, which name a position in the
    /// module tree rather than a path.
    ModuleTreeRelative,
}

/// One module specifier, as written, with the exact span that proves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportStatement {
    /// The module as the source names it, with quotes and leading dots
    /// removed: `p.q`, `./m`, `left-pad/sub`, `std::collections::HashMap`.
    pub specifier: String,
    pub form: ImportForm,
    /// The specifier's own span -- the same one the IMPORT_SITE
    /// Occurrence carries.
    pub span: SourceSpan,
}

/// What resolution could establish about one import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOutcome {
    /// Exactly one Resource in the current inventory.
    Internal(ResourceId),
    /// A package, and the module path inside it when the source states
    /// one. Never a symbol.
    External(ExternalEntity),
    /// More than one Resource matched. Picking one would be a guess, so
    /// the candidates are reported and nothing is confirmed.
    Ambiguous(Vec<ResourceId>),
    /// No target could be established, and why.
    Unresolved(UnresolvedImport),
}

impl ImportOutcome {
    /// Whether this outcome may become a canonical edge.
    #[must_use]
    pub const fn is_resolved(&self) -> bool {
        matches!(self, Self::Internal(_) | Self::External(_))
    }
}

/// Why an import has no confirmed target. Runtime only at this tier:
/// #17 task 7 owns persisting these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedImport {
    /// A relative specifier that matches no Resource. A missing file is
    /// not an external package.
    MissingRelativeTarget,
    /// A Rust `crate::`/`self::`/`super::` path. Mapping it to a file
    /// needs the module tree (`mod` declarations), which is language
    /// semantics rather than syntax.
    RustModuleTree,
    /// A C# `using` names a namespace. Which assembly or file provides
    /// it is not in the syntax, and guessing from the name is exactly
    /// what this tier refuses to do.
    CSharpNamespace,
    /// A specifier whose meaning depends on build configuration this
    /// tier does not read (a root-absolute path, a path alias).
    ConfigDependentSpecifier,
    /// A grouped or glob `use`/import whose module set is not one
    /// specifier.
    CompoundSpecifier,
}

/// One import statement and what resolution made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImport {
    pub statement: ImportStatement,
    pub outcome: ImportOutcome,
}

/// The current Resource inventory, indexed for module lookup.
///
/// Built once per analysis from the ACTIVE Resources; the resolver is
/// otherwise pure, so what it decides depends on nothing but the source
/// and this inventory.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceModules {
    by_path: HashMap<String, ResourceId>,
}

impl WorkspaceModules {
    #[must_use]
    pub fn from_resources(resources: &[Resource]) -> Self {
        Self {
            by_path: resources
                .iter()
                .filter(|resource| resource.state == ResourceState::Active)
                .map(|resource| (resource.path_key.clone(), resource.id))
                .collect(),
        }
    }

    fn get(&self, path: &str) -> Option<ResourceId> {
        self.by_path.get(path).copied()
    }

    /// Every Resource whose path ends with one of `suffixes`, at a path
    /// boundary. Used for an absolute module path whose source root is
    /// not stated: `p.q` may live at `p/q.py` or `src/p/q.py`, and two
    /// matches mean the answer is not structural.
    fn ending_with(&self, suffixes: &[String]) -> Vec<ResourceId> {
        let mut found: Vec<(&str, ResourceId)> = self
            .by_path
            .iter()
            .filter(|(path, _)| {
                suffixes.iter().any(|suffix| {
                    path.as_str() == suffix
                        || path.len() > suffix.len() && path.ends_with(&format!("/{suffix}"))
                })
            })
            .map(|(path, id)| (path.as_str(), *id))
            .collect();
        // Deterministic: the same inventory always reports the same
        // candidates in the same order.
        found.sort_by(|left, right| left.0.cmp(right.0));
        found.into_iter().map(|(_, id)| id).collect()
    }
}

/// Every module specifier one parse tree states, in source order.
///
/// Only the module part: the imported identifiers are evidence about
/// names, not about which module is imported (#17 "imported identifier
/// 각각을 무조건 별도 module relation으로 부풀리지 않는다").
#[must_use]
pub fn extract_imports(tree: &ParseTree, source: &[u8]) -> Vec<ImportStatement> {
    let dialect = tree.descriptor().dialect;
    if !tree.descriptor().capability.covers_whole_file() {
        // A container's embedded script is a different language that no
        // adapter maps yet (#16 task 8). Claiming it imports nothing
        // would be a false zero.
        return Vec::new();
    }
    let mut statements = Vec::new();
    collect(
        tree.syntax_tree().root_node(),
        dialect,
        source,
        &mut statements,
    );
    statements.sort_by_key(|statement| statement.span.start_byte);
    statements
}

fn collect(
    node: Node<'_>,
    dialect: ParserDialect,
    source: &[u8],
    statements: &mut Vec<ImportStatement>,
) {
    if let Some(statement) = statement_of(node, dialect, source) {
        statements.push(statement);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(child, dialect, source, statements);
    }
}

fn statement_of(node: Node<'_>, dialect: ParserDialect, source: &[u8]) -> Option<ImportStatement> {
    match (dialect, node.kind()) {
        (ParserDialect::Python, "import_statement" | "import_from_statement") => {
            // The module is what `module_name` names; for a plain
            // `import a.b` that is the same node. An aliased module
            // names itself first, and the alias is a local binding.
            // `from a.b import x` names the module under `module_name`;
            // a plain `import a.b` names it under `name`.
            let module = children_by_field(node, "module_name")
                .into_iter()
                .next()
                .or_else(|| children_by_field(node, "name").into_iter().next())?;
            let module = module
                .child_by_field_name("name")
                .filter(|_| module.kind() == "aliased_import")
                .unwrap_or(module);
            let text = text_of(module, source);
            let dots = text
                .chars()
                .take_while(|character| *character == '.')
                .count();
            Some(ImportStatement {
                specifier: text[dots..].to_owned(),
                form: if dots == 0 {
                    ImportForm::Absolute
                } else {
                    ImportForm::Relative { levels: dots }
                },
                span: span_of(module),
            })
        }
        (
            ParserDialect::JavaScript
            | ParserDialect::Jsx
            | ParserDialect::TypeScript
            | ParserDialect::Tsx,
            "import_statement",
        ) => {
            let source_node = node.child_by_field_name("source")?;
            let raw = text_of(source_node, source);
            let specifier = raw.trim_matches(['\'', '"', '`']).to_owned();
            let form = if specifier.starts_with("./") || specifier.starts_with("../") {
                ImportForm::Relative {
                    levels: relative_levels(&specifier),
                }
            } else {
                ImportForm::Absolute
            };
            Some(ImportStatement {
                specifier,
                form,
                span: span_of(source_node),
            })
        }
        (ParserDialect::Rust, "use_declaration") => {
            let argument = node.child_by_field_name("argument")?;
            let specifier = text_of(argument, source);
            let form = if specifier.starts_with("crate::")
                || specifier.starts_with("self::")
                || specifier.starts_with("super::")
            {
                ImportForm::ModuleTreeRelative
            } else {
                ImportForm::Absolute
            };
            Some(ImportStatement {
                specifier,
                form,
                span: span_of(argument),
            })
        }
        (ParserDialect::CSharp, "using_directive") => {
            // The last name is the namespace being used; an alias in
            // front of it is a local name.
            let mut cursor = node.walk();
            let names: Vec<Node<'_>> = node
                .named_children(&mut cursor)
                .filter(|child| {
                    matches!(
                        child.kind(),
                        "identifier" | "qualified_name" | "alias_qualified_name"
                    )
                })
                .collect();
            let used = *names.last()?;
            Some(ImportStatement {
                specifier: text_of(used, source),
                form: ImportForm::Absolute,
                span: span_of(used),
            })
        }
        _ => None,
    }
}

/// Resolve every extracted specifier against the current inventory.
///
/// `owner` is the importing Resource: a relative specifier is resolved
/// from its directory, which is why this needs no config and cannot
/// depend on where the analysis happens to run.
#[must_use]
pub fn resolve_imports(
    dialect: ParserDialect,
    owner: &Resource,
    modules: &WorkspaceModules,
    statements: Vec<ImportStatement>,
) -> Vec<ResolvedImport> {
    statements
        .into_iter()
        .map(|statement| ResolvedImport {
            outcome: resolve(dialect, owner, modules, &statement),
            statement,
        })
        .collect()
}

fn resolve(
    dialect: ParserDialect,
    owner: &Resource,
    modules: &WorkspaceModules,
    statement: &ImportStatement,
) -> ImportOutcome {
    match dialect {
        ParserDialect::Python => resolve_python(owner, modules, statement),
        ParserDialect::JavaScript
        | ParserDialect::Jsx
        | ParserDialect::TypeScript
        | ParserDialect::Tsx => resolve_js_ts(owner, modules, statement),
        ParserDialect::Rust => resolve_rust(statement),
        // A `using` names a namespace. Nothing in the syntax says which
        // file or assembly provides it.
        ParserDialect::CSharp => ImportOutcome::Unresolved(UnresolvedImport::CSharpNamespace),
        ParserDialect::Svelte => ImportOutcome::Unresolved(UnresolvedImport::CompoundSpecifier),
    }
}

const PYTHON_EXTENSIONS: &[&str] = &["py", "pyi"];

fn resolve_python(
    owner: &Resource,
    modules: &WorkspaceModules,
    statement: &ImportStatement,
) -> ImportOutcome {
    let segments: Vec<&str> = statement
        .specifier
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();
    match statement.form {
        ImportForm::Relative { levels } => {
            // `from . import x` is the file's own package; each extra
            // dot is one directory further up.
            let Some(base) = ancestor_directory(&owner.path_key, levels - 1) else {
                return ImportOutcome::Unresolved(UnresolvedImport::MissingRelativeTarget);
            };
            let joined = if segments.is_empty() {
                base.clone()
            } else {
                join(&base, &segments.join("/"))
            };
            let candidates = python_candidates(&joined)
                .into_iter()
                .filter_map(|path| modules.get(&path))
                .collect::<Vec<_>>();
            exact_or(candidates, UnresolvedImport::MissingRelativeTarget)
        }
        ImportForm::Absolute => {
            if segments.is_empty() {
                return ImportOutcome::Unresolved(UnresolvedImport::CompoundSpecifier);
            }
            let suffixes = python_candidates(&segments.join("/"));
            let candidates = modules.ending_with(&suffixes);
            match candidates.len() {
                1 => ImportOutcome::Internal(candidates[0]),
                0 => {
                    ImportOutcome::External(external_module(segments[0], &statement.specifier, "."))
                }
                _ => ImportOutcome::Ambiguous(candidates),
            }
        }
        ImportForm::ModuleTreeRelative => {
            ImportOutcome::Unresolved(UnresolvedImport::CompoundSpecifier)
        }
    }
}

/// `a/b` as Python may store it: the module file, or the package's
/// `__init__`.
fn python_candidates(path: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    for extension in PYTHON_EXTENSIONS {
        candidates.push(format!("{path}.{extension}"));
        candidates.push(format!("{path}/__init__.{extension}"));
    }
    candidates
}

const JS_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

fn resolve_js_ts(
    owner: &Resource,
    modules: &WorkspaceModules,
    statement: &ImportStatement,
) -> ImportOutcome {
    if matches!(statement.form, ImportForm::Relative { .. }) {
        let Some(base) = ancestor_directory(&owner.path_key, 0) else {
            return ImportOutcome::Unresolved(UnresolvedImport::MissingRelativeTarget);
        };
        let Some(joined) = join_relative(&base, &statement.specifier) else {
            return ImportOutcome::Unresolved(UnresolvedImport::MissingRelativeTarget);
        };
        let candidates: Vec<ResourceId> = js_candidates(&joined)
            .into_iter()
            .filter_map(|path| modules.get(&path))
            .collect();
        return exact_or(candidates, UnresolvedImport::MissingRelativeTarget);
    }
    if statement.specifier.starts_with('/') {
        // Root-absolute: what it means depends on build configuration
        // this tier does not read.
        return ImportOutcome::Unresolved(UnresolvedImport::ConfigDependentSpecifier);
    }
    // A bare specifier names a package. `@scope/name` is one package
    // name; anything after it is a module path inside it.
    let mut segments = statement.specifier.split('/');
    let first = segments.next().unwrap_or_default();
    let package = if first.starts_with('@') {
        match segments.next() {
            Some(second) => format!("{first}/{second}"),
            None => return ImportOutcome::Unresolved(UnresolvedImport::CompoundSpecifier),
        }
    } else {
        first.to_owned()
    };
    if package.is_empty() {
        return ImportOutcome::Unresolved(UnresolvedImport::CompoundSpecifier);
    }
    ImportOutcome::External(external_module(&package, &statement.specifier, "/"))
}

/// `./m` as a JS/TS resolver would look for it, minus anything that
/// needs configuration: the file itself, each extension, and the
/// directory's index.
fn js_candidates(path: &str) -> Vec<String> {
    let mut candidates = vec![path.to_owned()];
    for extension in JS_EXTENSIONS {
        candidates.push(format!("{path}.{extension}"));
        candidates.push(format!("{path}/index.{extension}"));
    }
    candidates
}

fn resolve_rust(statement: &ImportStatement) -> ImportOutcome {
    if statement.specifier.contains('{') || statement.specifier.contains('*') {
        return ImportOutcome::Unresolved(UnresolvedImport::CompoundSpecifier);
    }
    if statement.form == ImportForm::ModuleTreeRelative {
        return ImportOutcome::Unresolved(UnresolvedImport::RustModuleTree);
    }
    let crate_name = statement.specifier.split("::").next().unwrap_or_default();
    if crate_name.is_empty() {
        return ImportOutcome::Unresolved(UnresolvedImport::CompoundSpecifier);
    }
    // The first segment of a non-relative `use` is a crate. Which item
    // inside it the path ends at is not something this tier claims.
    ImportOutcome::External(external_module(crate_name, &statement.specifier, "::"))
}

/// A package-level external entity, plus the module path inside it when
/// the source states one. No symbol name is ever set: an import proves
/// the package, not what lives in it.
fn external_module(package: &str, specifier: &str, separator: &str) -> ExternalEntity {
    let module_path = specifier
        .strip_prefix(package)
        .and_then(|rest| rest.strip_prefix(separator))
        .filter(|rest| !rest.is_empty())
        .map(ToOwned::to_owned);
    ExternalEntity {
        package_identity: package.to_owned(),
        module_path,
        symbol_name: None,
        qualified_name: None,
        kind: EXTERNAL_MODULE_KIND.to_owned(),
        resolved_version: None,
        declaration_locator: None,
    }
}

/// Exactly one candidate is an answer; several is ambiguity; none is the
/// caller's stated reason.
fn exact_or(mut candidates: Vec<ResourceId>, reason: UnresolvedImport) -> ImportOutcome {
    candidates.dedup();
    match candidates.len() {
        1 => ImportOutcome::Internal(candidates[0]),
        0 => ImportOutcome::Unresolved(reason),
        _ => ImportOutcome::Ambiguous(candidates),
    }
}

/// The directory `levels` above the file's own directory.
fn ancestor_directory(path_key: &str, levels: usize) -> Option<String> {
    let mut directory = match path_key.rfind('/') {
        Some(index) => path_key[..index].to_owned(),
        None => String::new(),
    };
    for _ in 0..levels {
        directory = match directory.rfind('/') {
            Some(index) => directory[..index].to_owned(),
            None if directory.is_empty() => return None,
            None => String::new(),
        };
    }
    Some(directory)
}

fn join(base: &str, tail: &str) -> String {
    if base.is_empty() {
        tail.to_owned()
    } else {
        format!("{base}/{tail}")
    }
}

/// Apply a `./`-style specifier to a directory, resolving `..` segments.
/// `None` when it climbs above the Workspace root.
fn join_relative(base: &str, specifier: &str) -> Option<String> {
    let mut segments: Vec<&str> = if base.is_empty() {
        Vec::new()
    } else {
        base.split('/').collect()
    };
    for segment in specifier.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

fn relative_levels(specifier: &str) -> usize {
    specifier
        .split('/')
        .take_while(|segment| *segment == ".." || *segment == ".")
        .filter(|segment| *segment == "..")
        .count()
        + 1
}

fn text_of(node: Node<'_>, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]).into_owned()
}

/// Turn resolved imports into canonical edges from the importing
/// Resource, paired with the span that proves each one.
///
/// Unresolved and ambiguous outcomes are skipped rather than downgraded
/// into a guess -- they stay in the [`ResolvedImport`] list for #17 task
/// 7. Two statements naming the same module produce two evidence entries
/// for one edge, which is exactly what task 3's replacement expects.
#[must_use]
pub fn import_relations(
    owner: ResourceId,
    resolved: &[ResolvedImport],
    created_generation: i64,
) -> Vec<(SourceSpan, Relation)> {
    let source = GraphEndpoint::Resource(owner);
    resolved
        .iter()
        .filter_map(|import| {
            let target = match &import.outcome {
                ImportOutcome::Internal(resource) => GraphEndpoint::Resource(*resource),
                ImportOutcome::External(external) => GraphEndpoint::External(external.clone()),
                ImportOutcome::Ambiguous(_) | ImportOutcome::Unresolved(_) => return None,
            };
            Some((
                import.statement.span,
                Relation {
                    kind: RelationKind::Imports,
                    source: source.clone(),
                    target,
                    // An import binds at the module level as written:
                    // there is no dispatch to observe.
                    dispatch: Dispatch::Static,
                    created_generation,
                },
            ))
        })
        .collect()
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
        generation,
        graph::GraphStore,
        parser::{ParserRegistry, SourceBasis, dialect_for_path},
        resolution::EvidenceBasis,
        resource::ResourceStore,
        scan::BaselineScan,
        symbol::{OccurrenceKind, SymbolStore},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    /// Parse a snippet and read its module specifiers.
    fn imports_of(path_rel: &str, source: &str) -> Vec<ImportStatement> {
        let dialect = dialect_for_path(path_rel).expect("a supported dialect");
        let tree = ParserRegistry::new()
            .parse(dialect, source.as_bytes(), SourceBasis::default())
            .expect("parse");
        extract_imports(&tree, source.as_bytes())
    }

    fn specifiers(statements: &[ImportStatement]) -> Vec<&str> {
        statements
            .iter()
            .map(|statement| statement.specifier.as_str())
            .collect()
    }

    /// One inventory of Resources, built once so the ids a test
    /// compares against are the ids the resolver saw.
    struct Scenario {
        owner: Resource,
        modules: WorkspaceModules,
        ids: Vec<(String, ResourceId)>,
    }

    impl Scenario {
        fn new(owner_path: &str, inventory: &[&str]) -> Self {
            let mut paths: Vec<&str> = inventory.to_vec();
            if !paths.contains(&owner_path) {
                paths.push(owner_path);
            }
            let resources: Vec<Resource> = paths.iter().map(|path| resource_at(path)).collect();
            let owner = resources
                .iter()
                .find(|resource| resource.path_key == owner_path)
                .expect("the owner is in its own inventory")
                .clone();
            Self {
                owner,
                modules: WorkspaceModules::from_resources(&resources),
                ids: resources
                    .iter()
                    .map(|resource| (resource.path_key.clone(), resource.id))
                    .collect(),
            }
        }

        fn id(&self, path: &str) -> ResourceId {
            self.ids
                .iter()
                .find(|(candidate, _)| candidate == path)
                .expect("in the inventory")
                .1
        }

        fn resolve(&self, source: &str) -> Vec<(String, ImportOutcome)> {
            let dialect = dialect_for_path(&self.owner.path_key).expect("dialect");
            let statements = imports_of(&self.owner.path_key, source);
            resolve_imports(dialect, &self.owner, &self.modules, statements)
                .into_iter()
                .map(|import| (import.statement.specifier, import.outcome))
                .collect()
        }
    }

    fn resource_at(path: &str) -> Resource {
        Resource {
            id: ResourceId::generate(),
            path_rel: path.to_owned(),
            path_key: path.to_owned(),
            kind: crate::resource::ResourceKind::File,
            role: crate::resource::ResourceRole::Source,
            language: None,
            size_bytes: 0,
            mtime_ns: 0,
            fingerprint: "fp".to_owned(),
            content_hash: Some("sha256:test".to_owned()),
            state: ResourceState::Active,
            resource_revision: "1".to_owned(),
            generated_kind: None,
            container_resource_id: None,
        }
    }

    #[test]
    fn python_names_one_module_per_statement_with_its_exact_span() {
        let source = "import os\nfrom p.q import r, s as t\nfrom . import sibling\n";
        let statements = imports_of("app.py", source);

        assert_eq!(specifiers(&statements), vec!["os", "p.q", ""]);
        assert_eq!(
            statements[1].form,
            ImportForm::Absolute,
            "the imported identifiers r and s are not three more modules"
        );
        assert_eq!(statements[2].form, ImportForm::Relative { levels: 1 });

        // The span is the specifier as written, and nothing else.
        for statement in &statements {
            let text = &source[statement.span.start_byte..statement.span.end_byte];
            assert!(
                text == statement.specifier || text.trim_start_matches('.') == statement.specifier,
                "{text:?} is not the specifier {:?}",
                statement.specifier
            );
        }
        assert_eq!(
            &source[statements[0].span.start_byte..statements[0].span.end_byte],
            "os"
        );
    }

    #[test]
    fn a_python_import_resolves_inside_the_workspace_or_names_a_package() {
        // Internal: exactly one Resource is that module.
        let scenario = Scenario::new("app.py", &["src/p/q.py", "src/p/__init__.py"]);
        let resolved = scenario.resolve("from p.q import r\n");
        assert_eq!(
            resolved[0].1,
            ImportOutcome::Internal(scenario.id("src/p/q.py")),
            "one module file, one confirmed target"
        );

        // External: nothing in the Workspace is `os`, and the source
        // names the package explicitly.
        let bare = Scenario::new("app.py", &[]);
        let resolved = bare.resolve("import os\n");
        let ImportOutcome::External(external) = &resolved[0].1 else {
            panic!("expected a package: {:?}", resolved[0].1);
        };
        assert_eq!(external.package_identity, "os");
        assert_eq!(external.module_path, None);
        assert_eq!(
            external.symbol_name, None,
            "an import proves the package, never what lives in it"
        );

        // A dotted external keeps the module path, still no symbol.
        let resolved = bare.resolve("from django.db import models\n");
        let ImportOutcome::External(external) = &resolved[0].1 else {
            panic!("expected a package");
        };
        assert_eq!(external.package_identity, "django");
        assert_eq!(external.module_path.as_deref(), Some("db"));
        assert_eq!(external.symbol_name, None);
    }

    #[test]
    fn a_python_relative_import_resolves_from_the_importing_file() {
        let scenario = Scenario::new("pkg/app.py", &["pkg/config.py", "pkg/sub/__init__.py"]);
        let resolved = scenario.resolve("from .config import X\n");
        assert_eq!(
            resolved[0].1,
            ImportOutcome::Internal(scenario.id("pkg/config.py"))
        );

        let resolved = scenario.resolve("from .sub import Y\n");
        assert_eq!(
            resolved[0].1,
            ImportOutcome::Internal(scenario.id("pkg/sub/__init__.py"))
        );

        // A relative import with no file behind it is missing -- it is
        // emphatically not a package.
        let resolved = scenario.resolve("from .gone import Z\n");
        assert_eq!(
            resolved[0].1,
            ImportOutcome::Unresolved(UnresolvedImport::MissingRelativeTarget)
        );
    }

    #[test]
    fn typescript_relative_imports_resolve_and_bare_ones_name_packages() {
        let scenario = Scenario::new("src/app.ts", &["src/m.ts", "src/dir/index.tsx"]);
        let resolved = scenario.resolve("import { a } from './m'\n");
        assert_eq!(
            resolved[0].1,
            ImportOutcome::Internal(scenario.id("src/m.ts"))
        );

        let resolved = scenario.resolve("import x from './dir'\n");
        assert_eq!(
            resolved[0].1,
            ImportOutcome::Internal(scenario.id("src/dir/index.tsx")),
            "a directory resolves through its index file"
        );

        let resolved = scenario.resolve("import x from '../top'\n");
        assert_eq!(
            resolved[0].1,
            ImportOutcome::Unresolved(UnresolvedImport::MissingRelativeTarget)
        );

        for (specifier, package, module_path) in [
            ("'react'", "react", None),
            ("'left-pad/sub'", "left-pad", Some("sub")),
            ("'@scope/pkg'", "@scope/pkg", None),
            ("'@scope/pkg/deep'", "@scope/pkg", Some("deep")),
        ] {
            let resolved = scenario.resolve(&format!("import x from {specifier}\n"));
            let ImportOutcome::External(external) = &resolved[0].1 else {
                panic!("{specifier} should name a package: {:?}", resolved[0].1);
            };
            assert_eq!(external.package_identity, package);
            assert_eq!(external.module_path.as_deref(), module_path);
            assert_eq!(external.symbol_name, None);
        }

        // A root-absolute specifier means whatever the build config says
        // it means, and this tier does not read build config.
        let resolved = scenario.resolve("import x from '/aliased'\n");
        assert_eq!(
            resolved[0].1,
            ImportOutcome::Unresolved(UnresolvedImport::ConfigDependentSpecifier)
        );
    }

    #[test]
    fn rust_use_names_the_crate_and_refuses_the_module_tree() {
        let scenario = Scenario::new("src/lib.rs", &["src/app.rs"]);
        let resolved = scenario.resolve("use serde::Serialize;\n");
        let ImportOutcome::External(external) = &resolved[0].1 else {
            panic!("expected a crate: {:?}", resolved[0].1);
        };
        assert_eq!(external.package_identity, "serde");
        assert_eq!(external.module_path.as_deref(), Some("Serialize"));
        assert_eq!(
            external.symbol_name, None,
            "whether `Serialize` is a trait, a macro, or a module is not in the syntax"
        );

        let resolved = scenario.resolve("use std::collections::HashMap;\n");
        let ImportOutcome::External(external) = &resolved[0].1 else {
            panic!("expected a crate");
        };
        assert_eq!(external.package_identity, "std");
        assert_eq!(
            external.module_path.as_deref(),
            Some("collections::HashMap")
        );

        // `crate::` names a position in the module tree; mapping it to a
        // file needs `mod` declarations, which is semantics.
        for path in ["use crate::app::run;\n", "use super::thing;\n"] {
            let resolved = scenario.resolve(path);
            assert_eq!(
                resolved[0].1,
                ImportOutcome::Unresolved(UnresolvedImport::RustModuleTree),
                "{path} must not be guessed into src/app.rs"
            );
        }
    }

    #[test]
    fn a_csharp_using_is_a_namespace_and_stays_unresolved() {
        let scenario = Scenario::new("App.cs", &["Demo/Text.cs"]);
        let resolved = scenario.resolve("using System;\nusing Alias = Demo.Text;\n");

        assert_eq!(
            resolved
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            vec!["System", "Demo.Text"],
            "the alias is a local name; the namespace is what is used"
        );
        for (_, outcome) in &resolved {
            assert_eq!(
                *outcome,
                ImportOutcome::Unresolved(UnresolvedImport::CSharpNamespace),
                "a namespace is not a file, and Demo/Text.cs is not proof that it is"
            );
        }
    }

    #[test]
    fn an_alias_never_moves_the_canonical_target() {
        // Python, TypeScript and C# all let the local name differ from
        // the thing imported. The edge follows the thing.
        let python = Scenario::new("app.py", &["p/q.py"]);
        let plain = python.resolve("import p.q\n");
        let aliased = python.resolve("import p.q as shorthand\n");
        assert_eq!(plain, aliased, "the alias changes no target");
        assert_eq!(plain[0].1, ImportOutcome::Internal(python.id("p/q.py")));

        let typescript = Scenario::new("src/app.ts", &["src/m.ts"]);
        let plain = typescript.resolve("import { a } from './m'\n");
        let aliased = typescript.resolve("import { a as b } from './m'\n");
        assert_eq!(plain, aliased);

        let plain = imports_of("App.cs", "using Demo.Text;\n");
        let aliased = imports_of("App.cs", "using T = Demo.Text;\n");
        assert_eq!(specifiers(&plain), specifiers(&aliased));
    }

    #[test]
    fn two_candidates_are_never_promoted_to_one() {
        // The same module path exists under two source roots. Which one
        // the interpreter would pick depends on sys.path, which is not
        // in the source.
        let scenario = Scenario::new("app.py", &["src/p/q.py", "lib/p/q.py"]);
        let resolved = scenario.resolve("from p.q import r\n");

        let ImportOutcome::Ambiguous(candidates) = &resolved[0].1 else {
            panic!("expected ambiguity: {:?}", resolved[0].1);
        };
        assert_eq!(candidates.len(), 2);
        assert!(!resolved[0].1.is_resolved());

        let statements = imports_of("app.py", "from p.q import r\n");
        let ambiguous = resolve_imports(
            ParserDialect::Python,
            &scenario.owner,
            &scenario.modules,
            statements,
        );
        assert!(
            import_relations(scenario.owner.id, &ambiguous, 1).is_empty(),
            "an ambiguous outcome produces no edge at all"
        );
    }

    #[test]
    fn a_container_only_resource_states_no_imports() {
        let source = "<script lang=\"ts\">\n  import { a } from './m'\n</script>\n";
        let dialect = dialect_for_path("Widget.svelte").expect("dialect");
        let tree = ParserRegistry::new()
            .parse(dialect, source.as_bytes(), SourceBasis::default())
            .expect("parse");

        assert!(
            extract_imports(&tree, source.as_bytes()).is_empty(),
            "the embedded script is a language no adapter maps yet (#16 task 8)"
        );
    }

    /// A real Workspace, so the extracted spans can be checked against
    /// the Occurrences #16 task 9 published and bound through #17 task
    /// 3's replacement.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-imports-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            // app.ts imports the same module twice and a package once.
            fixture.write(
                "src/app.ts",
                "import { a } from './m'\nimport type { B } from './m'\nimport react from 'react'\n\nexport const used = a\n",
            );
            fixture.write(
                "src/other.ts",
                "import { a } from './m'\n\nexport const alsoUsed = a\n",
            );
            fixture.write("src/m.ts", "export const a = 1\nexport type B = number\n");
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

        fn source(&self, rel: &str) -> String {
            fs::read_to_string(self.root.join(rel)).expect("source")
        }

        /// Extract and resolve one file's imports against the real
        /// inventory.
        fn resolved(&self, rel: &str) -> Vec<ResolvedImport> {
            let resource = self.resource(rel);
            let source = self.source(rel);
            let dialect = dialect_for_path(rel).expect("dialect");
            let tree = ParserRegistry::new()
                .parse(dialect, source.as_bytes(), SourceBasis::of(&resource))
                .expect("parse");
            let statements = extract_imports(&tree, source.as_bytes());
            resolve_imports(
                dialect,
                &resource,
                &WorkspaceModules::from_resources(&self.resources()),
                statements,
            )
        }

        fn store(&self) -> GraphStore {
            GraphStore::open(&self.db_path()).expect("index.db")
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

        /// Publish one file's import evidence through task 3's
        /// Resource-owned replacement.
        fn publish_imports(&self, rel: &str) -> crate::evidence::EvidenceReplacement {
            let store = self.store();
            let connection = store.connection();
            let revision = generation::current_workspace_revision(connection)
                .expect("clock")
                .expect("bootstrapped");
            let building = generation::begin_generation(connection, &revision).expect("begin");
            let transaction = connection.unchecked_transaction().expect("transaction");
            let (record, grant) =
                generation::grant_publication(&transaction, building.id).expect("grant");

            let resource = self.resource(rel);
            let resolved = self.resolved(rel);
            let relations = import_relations(resource.id, &resolved, building.id);
            let evidence: Vec<RelationEvidence> = relations
                .into_iter()
                .map(|(span, relation)| {
                    for endpoint in [&relation.source, &relation.target] {
                        crate::graph::ensure_entity(&transaction, endpoint).expect("ensure");
                    }
                    RelationEvidence {
                        occurrence: OccurrenceRef {
                            kind: OccurrenceKind::ImportSite,
                            start_byte: span.start_byte,
                            end_byte: span.end_byte,
                        },
                        relation,
                    }
                })
                .collect();
            let report = replace_resource_evidence(
                &transaction,
                &grant,
                &self.basis(rel, building.id),
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
    fn extracted_spans_are_occurrences_the_index_already_published() {
        let fixture = Fixture::create("spans");
        let resource = fixture.resource("src/app.ts");
        let published: Vec<(usize, usize)> = SymbolStore::open(&fixture.db_path())
            .expect("index.db")
            .list_occurrences_for_resource(resource.id)
            .expect("occurrences")
            .into_iter()
            .filter(|occurrence| occurrence.kind == OccurrenceKind::ImportSite)
            .map(|occurrence| (occurrence.span.start_byte, occurrence.span.end_byte))
            .collect();

        let statements = fixture
            .resolved("src/app.ts")
            .into_iter()
            .map(|import| import.statement)
            .collect::<Vec<_>>();
        assert_eq!(statements.len(), 3);
        for statement in &statements {
            assert!(
                published.contains(&(statement.span.start_byte, statement.span.end_byte)),
                "{:?} is not an Occurrence the structural index published",
                statement.specifier
            );
        }

        // And the extractor invents no second evidence at the same
        // (kind, start, end).
        let mut spans: Vec<(usize, usize)> = statements
            .iter()
            .map(|statement| (statement.span.start_byte, statement.span.end_byte))
            .collect();
        let before = spans.len();
        spans.sort_unstable();
        spans.dedup();
        assert_eq!(spans.len(), before, "two statements share one span");
    }

    #[test]
    fn two_statements_naming_one_module_are_one_edge_with_two_proofs() {
        let fixture = Fixture::create("evidence");
        let app = fixture.resource("src/app.ts");
        let module = fixture.resource("src/m.ts");

        let report = fixture.publish_imports("src/app.ts");
        assert_eq!(
            report.occurrences_bound, 3,
            "two statements for ./m and one for react"
        );
        assert_eq!(
            report.relations_bound, 2,
            "which is one edge to the module and one to the package"
        );

        let store = fixture.store();
        let internal = store
            .relations_from(
                &GraphEndpoint::Resource(app.id),
                Some(RelationKind::Imports),
            )
            .expect("from");
        assert_eq!(internal.len(), 2);
        assert!(
            internal
                .iter()
                .any(|relation| relation.target == GraphEndpoint::Resource(module.id)),
            "the internal module is the Resource itself, by stable id"
        );
        assert!(internal.iter().any(|relation| matches!(
            &relation.target,
            GraphEndpoint::External(external) if external.package_identity == "react"
        )));

        // Another Resource's evidence for the same edge is its own.
        let other = fixture.resource("src/other.ts");
        fixture.publish_imports("src/other.ts");
        let store = fixture.store();
        assert_eq!(
            store
                .relations_to(
                    &GraphEndpoint::Resource(module.id),
                    Some(RelationKind::Imports)
                )
                .expect("to")
                .len(),
            2,
            "two importers, each proven by its own file"
        );
        assert_eq!(
            store
                .relations_from(
                    &GraphEndpoint::Resource(other.id),
                    Some(RelationKind::Imports)
                )
                .expect("from")
                .len(),
            1
        );

        // Re-publishing app.ts with nothing to say leaves other.ts alone.
        fixture.write("src/app.ts", "export const nothing = 1\n");
        let report = fixture.publish_imports("src/app.ts");
        assert_eq!(report.occurrences_bound, 0);
        let store = fixture.store();
        assert_eq!(
            store
                .relations_from(
                    &GraphEndpoint::Resource(other.id),
                    Some(RelationKind::Imports)
                )
                .expect("from")
                .len(),
            1,
            "other.ts still imports the module"
        );
        assert!(
            store
                .relations_from(
                    &GraphEndpoint::Resource(app.id),
                    Some(RelationKind::Imports)
                )
                .expect("from")
                .is_empty()
        );
    }

    #[test]
    fn import_relations_store_no_source_text() {
        let fixture = Fixture::create("no-source");
        fixture.publish_imports("src/app.ts");

        let database = fs::read(fixture.db_path()).expect("index.db bytes");
        for body in [
            "import { a } from",
            "export const a = 1",
            "import react from",
        ] {
            assert!(
                !database
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db must not mirror source text ({body:?})"
            );
        }
    }
}
