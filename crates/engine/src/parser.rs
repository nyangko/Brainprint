//! Tree-sitter structural adapter foundation and the parser/language
//! registry (#16 task 7).
//!
//! Tree-sitter is Brainprint's **Tier-1 structural backend** (#3 task 3
//! §1-2): fast, error-tolerant syntax structure. It is not a semantic
//! resolver, and nothing here claims cross-file binding, types, or
//! dispatch -- that is Tier 2 (I4).
//!
//! ## What this task does and does not do
//!
//! This is the adapter and the registry only. No Symbol is extracted (#16
//! task 8), no Occurrence (task 9), no Relation (I3), no locate/query
//! surface, and not one row is written to `index.db`. A parse tree is a
//! runtime artifact: it lives in memory, it is never serialized, and
//! neither it nor the source it was parsed from is persisted (#3 task 3
//! §2: "full Tree-sitter AST를 영속 DB에 기본 저장하지 않는다").
//!
//! ## The adapter boundary
//!
//! `tree_sitter`'s own types stop at this module. [`ParseTree`] owns a
//! `tree_sitter::Tree` privately and exposes Brainprint types --
//! [`ParserDescriptor`], [`ParseStatus`], [`SourceSpan`] -- so the
//! canonical model never depends on the backend. A future extractor (task
//! 8) reaches the raw tree through the crate-internal
//! [`ParseTree::syntax_tree`], not through the public API.
//!
//! ## Dialects
//!
//! [`ResourceLanguage`] stays exactly as task 1 defined it; no enum gains a
//! variant for React or for a file extension. The finer distinction lives
//! in [`ParserDialect`], which the **path extension** decides:
//! `.js`/`.jsx` and `.ts`/`.tsx` are different dialects of the same
//! language, and a JSX file is never parsed by a grammar that cannot see
//! its markup.
//!
//! `.svelte` is not disguised as a TS/JS file. Its grammar parses the
//! component's own structure -- template, `<script>`, `<style>` boundaries
//! -- and that is all this stage claims: [`StructuralCapability::Container`]
//! names the embedded languages that a later adapter (I4) must map before
//! any declaration inside those blocks may be asserted.
//!
//! ## Parse results
//!
//! - valid source → [`ParseStatus::Complete`] and a root span covering the
//!   whole byte range.
//! - broken or half-typed source → **not** a failure. Tree-sitter still
//!   produces a tree, so the result is [`ParseStatus::Partial`] and
//!   [`ParseTree::error_spans`] reports where the damage is, leaving the
//!   extractor to decide what it can still claim.
//! - no grammar for the path → an explicit [`ParseError`]. Never an empty
//!   success: an unsupported file must not look like a file with no
//!   structure (#16 "false-zero 금지").
//!
//! ## Incremental
//!
//! [`ParserRegistry::reparse`] is the boundary Tree-sitter's incremental
//! parsing needs: it takes the previous [`ParseTree`] plus the
//! [`SourceEdit`]s that produced the new source. Computing those edits from
//! a file save, and orchestrating when to reparse, is #16 task 13 -- this
//! task only refuses to design that possibility away.
//!
//! ## No global state
//!
//! A [`ParserRegistry`] is an ordinary value its caller owns. There is no
//! static, no process-wide cache, and no shared parser, so one Workspace or
//! worktree can never contaminate another's parsing.

use std::{
    collections::{HashMap, hash_map::Entry},
    error::Error,
    fmt,
    path::Path,
};

use tree_sitter::{InputEdit, Language, Parser, Point, Tree};

use crate::resource::{Resource, ResourceKind, ResourceLanguage};

/// The Tier-1 structural backend behind every dialect here (#3 task 3).
/// A name, not a version: the version is a Cargo dependency, deliberately
/// not an architecture constant.
pub const STRUCTURAL_BACKEND: &str = "tree-sitter";

/// A byte range plus its line/column endpoints, so a span can be handed
/// straight to a current-source read.
///
/// Lines and columns are zero-based, and a column is a **byte** offset
/// within its line -- which is what the backend reports, and what a byte
/// range read needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start: SourcePoint,
    pub end: SourcePoint,
}

/// A zero-based line/column position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourcePoint {
    pub line: usize,
    pub column: usize,
}

impl SourcePoint {
    #[must_use]
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }

    fn from_backend(point: Point) -> Self {
        Self {
            line: point.row,
            column: point.column,
        }
    }

    fn to_backend(self) -> Point {
        Point {
            row: self.line,
            column: self.column,
        }
    }
}

/// One contiguous replacement in a source file, as Tree-sitter needs it to
/// reuse the unchanged parts of a previous tree.
///
/// Producing these from an actual file save is #16 task 13's job; this type
/// only exists so that path stays open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start: SourcePoint,
    pub old_end: SourcePoint,
    pub new_end: SourcePoint,
}

impl SourceEdit {
    fn to_backend(self) -> InputEdit {
        InputEdit {
            start_byte: self.start_byte,
            old_end_byte: self.old_end_byte,
            new_end_byte: self.new_end_byte,
            start_position: self.start.to_backend(),
            old_end_position: self.old_end.to_backend(),
            new_end_position: self.new_end.to_backend(),
        }
    }
}

/// Which grammar parses a file. Finer than [`ResourceLanguage`] on purpose:
/// the language says what the file *is*, the dialect says what can read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParserDialect {
    Python,
    /// Plain JavaScript (`.js`, `.mjs`, `.cjs`).
    JavaScript,
    /// JavaScript with JSX markup (`.jsx`). React is not a language --
    /// this is a JavaScript dialect.
    Jsx,
    /// TypeScript without JSX (`.ts`, `.mts`, `.cts`). A separate grammar
    /// from [`Self::Tsx`], and not interchangeable with it.
    TypeScript,
    /// TypeScript with JSX markup (`.tsx`).
    Tsx,
    CSharp,
    Rust,
    /// A Svelte component file: a container, not a script file.
    Svelte,
}

impl ParserDialect {
    /// The task 1 [`ResourceLanguage`] this dialect belongs to. Several
    /// dialects share one language; no new language exists.
    #[must_use]
    pub const fn language(self) -> ResourceLanguage {
        match self {
            Self::Python => ResourceLanguage::Python,
            Self::JavaScript | Self::Jsx => ResourceLanguage::JavaScript,
            Self::TypeScript | Self::Tsx => ResourceLanguage::TypeScript,
            Self::CSharp => ResourceLanguage::CSharp,
            Self::Rust => ResourceLanguage::Rust,
            Self::Svelte => ResourceLanguage::Svelte,
        }
    }

    /// The grammar package behind this dialect, named so a profile can
    /// record which one produced a result.
    #[must_use]
    pub const fn grammar(self) -> &'static str {
        match self {
            Self::Python => "tree-sitter-python",
            // One grammar covers both: tree-sitter-javascript parses JSX
            // natively. The dialects stay distinct anyway, because what a
            // `.jsx` file may contain is a different question from which
            // grammar happens to accept it.
            Self::JavaScript | Self::Jsx => "tree-sitter-javascript",
            Self::TypeScript => "tree-sitter-typescript:typescript",
            Self::Tsx => "tree-sitter-typescript:tsx",
            Self::CSharp => "tree-sitter-c-sharp",
            Self::Rust => "tree-sitter-rust",
            Self::Svelte => "tree-sitter-svelte-ng",
        }
    }

    /// What Tier-1 structure this dialect can be trusted for.
    #[must_use]
    pub const fn capability(self) -> StructuralCapability {
        match self {
            Self::Svelte => StructuralCapability::Container {
                embedded: &[ResourceLanguage::TypeScript, ResourceLanguage::JavaScript],
            },
            _ => StructuralCapability::WholeFile,
        }
    }

    /// Whether this build reads the container's embedded regions.
    ///
    /// A capability statement says what the *grammar* covers; this says
    /// what Brainprint currently does with it. #19 task 11 made a Svelte
    /// component's `<script>` real -- its declarations are extracted in
    /// the component's own byte offsets -- while the component stays a
    /// container, because its `<style>` block and the template locals it
    /// does not bind are still outside the index.
    #[must_use]
    pub const fn extracts_embedded(self) -> bool {
        matches!(self, Self::Svelte)
    }

    /// Whether the grammar covers embedded markup (JSX/TSX, Svelte
    /// template) as well as code.
    #[must_use]
    pub const fn has_markup(self) -> bool {
        matches!(self, Self::Jsx | Self::Tsx | Self::Svelte)
    }

    fn backend_language(self) -> Language {
        match self {
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript | Self::Jsx => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Svelte => tree_sitter_svelte_ng::LANGUAGE.into(),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Python => "PYTHON",
            Self::JavaScript => "JAVASCRIPT",
            Self::Jsx => "JSX",
            Self::TypeScript => "TYPESCRIPT",
            Self::Tsx => "TSX",
            Self::CSharp => "CSHARP",
            Self::Rust => "RUST",
            Self::Svelte => "SVELTE",
        }
    }
}

impl fmt::Display for ParserDialect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How much of a file this backend's structure covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuralCapability {
    /// Declarations and spans for the whole file come from this one
    /// grammar.
    WholeFile,
    /// The grammar parses the file's own structure -- its template and the
    /// boundaries of its embedded blocks -- but the code *inside* those
    /// blocks belongs to `embedded` and needs a further adapter before any
    /// declaration in it may be claimed (I4). Pretending otherwise is how
    /// a Svelte component ends up indexed as a broken TypeScript file.
    Container {
        embedded: &'static [ResourceLanguage],
    },
}

impl StructuralCapability {
    /// Whether whole-file declarations may be claimed from this parse.
    #[must_use]
    pub const fn covers_whole_file(self) -> bool {
        matches!(self, Self::WholeFile)
    }
}

/// Everything a later stage needs to record *what produced* a structural
/// result -- dialect and language identity, backend and grammar identity,
/// and the declared capability.
///
/// This is deliberately the exact shape an `analysis_profile` row wants
/// (#13 task 6: language, structural backend, backend version, capability
/// fingerprint). No profile row is written here; task 8 builds one from
/// this descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParserDescriptor {
    pub dialect: ParserDialect,
    pub language: ResourceLanguage,
    /// Always [`STRUCTURAL_BACKEND`] at this tier.
    pub backend: &'static str,
    /// The backend's language ABI version -- the version identity that is
    /// actually observable at runtime, and the one that decides whether a
    /// grammar can be loaded at all.
    pub backend_abi_version: usize,
    pub grammar: &'static str,
    /// The grammar's own ABI version, which moves when the grammar is
    /// regenerated.
    pub grammar_abi_version: usize,
    pub capability: StructuralCapability,
}

impl ParserDescriptor {
    fn of(dialect: ParserDialect, language: &Language) -> Self {
        Self {
            dialect,
            language: dialect.language(),
            backend: STRUCTURAL_BACKEND,
            backend_abi_version: tree_sitter::LANGUAGE_VERSION,
            grammar: dialect.grammar(),
            grammar_abi_version: language.abi_version(),
            capability: dialect.capability(),
        }
    }
}

/// What a parse tree was produced from, so a retained tree can later be
/// checked against a Resource's current source instead of being assumed
/// fresh (#16 task 11's revision compatibility).
///
/// Every field is optional because a parse does not require any of them --
/// a fixture or an ad-hoc parse has no Resource behind it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceBasis {
    pub path_rel: Option<String>,
    pub resource_revision: Option<String>,
    pub content_hash: Option<String>,
}

impl SourceBasis {
    /// The basis of a Resource's current persisted evidence.
    #[must_use]
    pub fn of(resource: &Resource) -> Self {
        Self {
            path_rel: Some(resource.path_rel.clone()),
            resource_revision: Some(resource.resource_revision.clone()),
            content_hash: resource.content_hash.clone(),
        }
    }
}

/// Whether the grammar accepted the whole source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseStatus {
    /// No error or missing node anywhere in the tree.
    Complete,
    /// A usable tree with at least one error or missing node. Structure
    /// outside the damaged region is still real -- this is the normal state
    /// of a file being edited, not a failure.
    Partial,
}

impl ParseStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "COMPLETE",
            Self::Partial => "PARTIAL",
        }
    }
}

impl fmt::Display for ParseStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One parsed source, as Brainprint sees it.
///
/// Holds the backend tree privately: this is the wrapper that keeps
/// `tree_sitter::Tree`/`Node` an implementation detail. It is a runtime
/// value -- never serialized, never persisted.
pub struct ParseTree {
    descriptor: ParserDescriptor,
    basis: SourceBasis,
    source_len: usize,
    tree: Tree,
}

impl ParseTree {
    /// What parsed this, and what it may be trusted for.
    #[must_use]
    pub fn descriptor(&self) -> &ParserDescriptor {
        &self.descriptor
    }

    /// What this tree was parsed from.
    #[must_use]
    pub fn basis(&self) -> &SourceBasis {
        &self.basis
    }

    /// The length in bytes of the source this tree describes.
    #[must_use]
    pub fn source_len(&self) -> usize {
        self.source_len
    }

    #[must_use]
    pub fn status(&self) -> ParseStatus {
        if self.tree.root_node().has_error() {
            ParseStatus::Partial
        } else {
            ParseStatus::Complete
        }
    }

    /// The root node's span, which covers the parsed byte range.
    #[must_use]
    pub fn root_span(&self) -> SourceSpan {
        span_of(&self.tree.root_node())
    }

    /// Every error or missing node, in source order.
    ///
    /// Computed on demand rather than cached: a caller that only needs to
    /// know *whether* the source is broken asks [`Self::status`], and a
    /// badly broken file should not pay for a span list nobody reads.
    #[must_use]
    pub fn error_spans(&self) -> Vec<SourceSpan> {
        let mut cursor = self.tree.walk();
        let mut spans = Vec::new();
        let mut descend = true;
        loop {
            let node = cursor.node();
            if node.is_error() || node.is_missing() {
                spans.push(span_of(&node));
                // The damage is reported once, at its outermost node.
                descend = false;
            }
            // A subtree with no error anywhere cannot contain one.
            if descend && node.has_error() && cursor.goto_first_child() {
                continue;
            }
            descend = true;
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return spans;
                }
            }
        }
    }

    /// The backend tree, for an extractor inside this crate
    /// ([`crate::extract`]).
    ///
    /// Crate-internal on purpose: this is the *only* door to the backend,
    /// and the public model never exposes `tree_sitter` types.
    pub(crate) fn syntax_tree(&self) -> &Tree {
        &self.tree
    }
}

impl fmt::Debug for ParseTree {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The backend tree is not part of the contract, so it is described
        // rather than dumped.
        formatter
            .debug_struct("ParseTree")
            .field("dialect", &self.descriptor.dialect)
            .field("status", &self.status())
            .field("source_len", &self.source_len)
            .field("basis", &self.basis)
            .finish()
    }
}

/// Failure selecting a dialect or producing a tree.
#[derive(Debug)]
pub enum ParseError {
    /// No Tier-1 grammar covers this path. An explicit refusal, never an
    /// empty success -- "unsupported" and "no structure" are different
    /// answers (#16 "false-zero 금지").
    UnsupportedPath { path_rel: String },
    /// Only a FILE has source to parse.
    UnsupportedKind {
        path_rel: String,
        kind: ResourceKind,
    },
    /// The Resource's classified language and the dialect its extension
    /// selects disagree. Reported rather than guessed away.
    LanguageMismatch {
        path_rel: String,
        classified: ResourceLanguage,
        dialect: ParserDialect,
    },
    /// The grammar could not be installed into a parser -- an ABI
    /// mismatch between the runtime and the grammar package.
    Grammar {
        dialect: ParserDialect,
        source: tree_sitter::LanguageError,
    },
    /// The backend returned no tree at all. This is *not* a syntax error:
    /// broken source still yields a [`ParseStatus::Partial`] tree.
    NoTree { dialect: ParserDialect },
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPath { path_rel } => write!(
                formatter,
                "no structural grammar supports {path_rel:?}; it is unsupported, not empty"
            ),
            Self::UnsupportedKind { path_rel, kind } => {
                write!(formatter, "{path_rel:?} is a {kind}, which has no source")
            }
            Self::LanguageMismatch {
                path_rel,
                classified,
                dialect,
            } => write!(
                formatter,
                "{path_rel:?} is classified {classified} but its extension selects the \
                 {dialect} dialect"
            ),
            Self::Grammar { dialect, source } => {
                write!(
                    formatter,
                    "the {dialect} grammar could not be loaded: {source}"
                )
            }
            Self::NoTree { dialect } => {
                write!(formatter, "the {dialect} parser produced no tree at all")
            }
        }
    }
}

impl Error for ParseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Grammar { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The dialect a path's extension selects, or an explicit refusal.
///
/// The extension decides, because that is the only thing that separates
/// `.ts` from `.tsx` or `.js` from `.jsx` -- a distinction
/// [`ResourceLanguage`] deliberately does not carry.
pub fn dialect_for_path(path_rel: &str) -> Result<ParserDialect, ParseError> {
    let extension = Path::new(path_rel)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);

    match extension.as_deref() {
        Some("py" | "pyi") => Ok(ParserDialect::Python),
        Some("js" | "mjs" | "cjs") => Ok(ParserDialect::JavaScript),
        Some("jsx") => Ok(ParserDialect::Jsx),
        Some("ts" | "mts" | "cts") => Ok(ParserDialect::TypeScript),
        Some("tsx") => Ok(ParserDialect::Tsx),
        Some("cs") => Ok(ParserDialect::CSharp),
        Some("rs") => Ok(ParserDialect::Rust),
        Some("svelte") => Ok(ParserDialect::Svelte),
        _ => Err(ParseError::UnsupportedPath {
            path_rel: path_rel.to_owned(),
        }),
    }
}

/// The dialect for a persisted Resource: its kind must be a FILE, its
/// extension selects the dialect, and its classified language must agree
/// with that dialect's language.
pub fn dialect_for_resource(resource: &Resource) -> Result<ParserDialect, ParseError> {
    if resource.kind != ResourceKind::File {
        return Err(ParseError::UnsupportedKind {
            path_rel: resource.path_rel.clone(),
            kind: resource.kind,
        });
    }
    let dialect = dialect_for_path(&resource.path_rel)?;
    match resource.language {
        Some(language) if language != dialect.language() => Err(ParseError::LanguageMismatch {
            path_rel: resource.path_rel.clone(),
            classified: language,
            dialect,
        }),
        _ => Ok(dialect),
    }
}

/// One caller's set of grammars and parsers.
///
/// Plain owned state, created with [`Self::new`] and dropped with its
/// owner. Deliberately not a global: parsers are stateful, and a process
/// that indexes several Workspaces must not let them share one.
#[derive(Default)]
pub struct ParserRegistry {
    // Created on first use and kept, because installing a grammar is the
    // expensive part of parsing one small file.
    parsers: HashMap<ParserDialect, Parser>,
}

impl ParserRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The descriptor for `dialect`, loading its grammar if this registry
    /// has not used it yet.
    pub fn describe(&mut self, dialect: ParserDialect) -> Result<ParserDescriptor, ParseError> {
        self.parser(dialect)?;
        Ok(ParserDescriptor::of(dialect, &dialect.backend_language()))
    }

    /// Parse `source` from scratch.
    pub fn parse(
        &mut self,
        dialect: ParserDialect,
        source: &[u8],
        basis: SourceBasis,
    ) -> Result<ParseTree, ParseError> {
        self.run(dialect, source, basis, None)
    }

    /// Reparse `source`, reusing whatever `previous` still describes.
    ///
    /// `edits` must be the changes that turned `previous`'s source into
    /// `source`; the backend needs them to know which parts of the old tree
    /// survive. Computing them from a file save is #16 task 13 -- an empty
    /// slice here would make the old tree a lie, so this refuses to guess
    /// and simply passes through what the caller vouches for.
    pub fn reparse(
        &mut self,
        previous: &ParseTree,
        edits: &[SourceEdit],
        source: &[u8],
        basis: SourceBasis,
    ) -> Result<ParseTree, ParseError> {
        let dialect = previous.descriptor.dialect;
        // Cloning a tree is a refcount bump; the caller keeps theirs valid.
        let mut edited = previous.tree.clone();
        for edit in edits {
            edited.edit(&edit.to_backend());
        }
        self.run(dialect, source, basis, Some(&edited))
    }

    fn run(
        &mut self,
        dialect: ParserDialect,
        source: &[u8],
        basis: SourceBasis,
        previous: Option<&Tree>,
    ) -> Result<ParseTree, ParseError> {
        let descriptor = ParserDescriptor::of(dialect, &dialect.backend_language());
        let parser = self.parser(dialect)?;
        let tree = parser
            .parse(source, previous)
            .ok_or(ParseError::NoTree { dialect })?;
        Ok(ParseTree {
            descriptor,
            basis,
            source_len: source.len(),
            tree,
        })
    }

    fn parser(&mut self, dialect: ParserDialect) -> Result<&mut Parser, ParseError> {
        match self.parsers.entry(dialect) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let mut parser = Parser::new();
                parser
                    .set_language(&dialect.backend_language())
                    .map_err(|source| ParseError::Grammar { dialect, source })?;
                Ok(entry.insert(parser))
            }
        }
    }
}

fn span_of(node: &tree_sitter::Node<'_>) -> SourceSpan {
    SourceSpan {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start: SourcePoint::from_backend(node.start_position()),
        end: SourcePoint::from_backend(node.end_position()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
        thread,
    };

    use brainprint_core::ResourceId;

    use super::*;
    use crate::{
        config::WorkspaceConfig,
        resource::{ResourceRole, ResourceState},
        scan::BaselineScan,
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    /// One minimal but representative fixture per dialect: the declaration
    /// shapes #16 names as P0 structure, not a "hello world".
    const FIXTURES: &[(ParserDialect, &str, &str)] = &[
        (
            ParserDialect::Python,
            "mod.py",
            "import os\n\n\nclass Thing:\n    value: int = 1\n\n    def method(self) -> int:\n \
             return self.value\n\n\ndef top(a, b=2):\n    return a + b\n",
        ),
        (
            ParserDialect::Python,
            "mod.pyi",
            "class Thing:\n    value: int\n    def method(self) -> int: ...\n",
        ),
        (
            ParserDialect::JavaScript,
            "mod.js",
            "import { x } from './x.js'\n\nexport class Thing {\n  method() { return 1 }\n}\n\n\
             export const top = (a, b) => a + b\n",
        ),
        (
            ParserDialect::Jsx,
            "view.jsx",
            "export const View = ({ label }) => <div className=\"row\">{label}</div>\n",
        ),
        (
            ParserDialect::TypeScript,
            "mod.ts",
            "export interface Shape {\n  value: number\n}\n\nexport type Alias = Shape | null\n\n\
             export class Thing implements Shape {\n  value = 1\n  method(): number { return \
             this.value }\n}\n",
        ),
        (
            ParserDialect::Tsx,
            "view.tsx",
            "export const View = ({ label }: { label: string }) => <div>{label}</div>\n",
        ),
        (
            ParserDialect::CSharp,
            "Thing.cs",
            "namespace Demo;\n\npublic interface IShape { int Value { get; } }\n\n\
             public class Thing : IShape\n{\n    public int Value => 1;\n    public int \
             Method() => Value;\n}\n",
        ),
        (
            ParserDialect::Rust,
            "thing.rs",
            "pub trait Shape {\n    fn value(&self) -> u32;\n}\n\npub struct Thing;\n\n\
             impl Shape for Thing {\n    fn value(&self) -> u32 { 1 }\n}\n",
        ),
        (
            ParserDialect::Svelte,
            "View.svelte",
            "<script lang=\"ts\">\n  export let label: string;\n</script>\n\n<div>{label}</div>\n",
        ),
    ];

    fn file_resource(path_rel: &str, language: Option<ResourceLanguage>) -> Resource {
        Resource {
            id: ResourceId::generate(),
            path_rel: path_rel.to_owned(),
            path_key: path_rel.to_owned(),
            kind: ResourceKind::File,
            role: ResourceRole::Source,
            language,
            size_bytes: 0,
            mtime_ns: 0,
            fingerprint: "sha256-fp1:test".to_owned(),
            content_hash: Some("sha256:test".to_owned()),
            state: ResourceState::Active,
            resource_revision: "1".to_owned(),
            generated_kind: None,
            container_resource_id: None,
        }
    }

    #[test]
    fn the_path_extension_selects_the_dialect() {
        let expectations: &[(&str, ParserDialect)] = &[
            ("pkg/mod.py", ParserDialect::Python),
            ("pkg/mod.pyi", ParserDialect::Python),
            ("src/mod.js", ParserDialect::JavaScript),
            ("src/mod.mjs", ParserDialect::JavaScript),
            ("src/mod.cjs", ParserDialect::JavaScript),
            ("src/view.jsx", ParserDialect::Jsx),
            ("src/mod.ts", ParserDialect::TypeScript),
            ("src/mod.mts", ParserDialect::TypeScript),
            ("src/mod.cts", ParserDialect::TypeScript),
            ("src/view.tsx", ParserDialect::Tsx),
            ("src/Thing.cs", ParserDialect::CSharp),
            ("src/thing.rs", ParserDialect::Rust),
            ("src/View.svelte", ParserDialect::Svelte),
            // The extension decides, whatever the case on disk.
            ("src/View.SVELTE", ParserDialect::Svelte),
        ];

        for (path_rel, expected) in expectations {
            assert_eq!(
                dialect_for_path(path_rel).expect("supported"),
                *expected,
                "{path_rel}"
            );
        }
    }

    #[test]
    fn an_unsupported_path_is_refused_explicitly_rather_than_answered_emptily() {
        for path_rel in ["README.md", "data.json", "Makefile", "src/lib.go", "noext"] {
            let error = dialect_for_path(path_rel).expect_err("must be refused");
            assert!(
                matches!(error, ParseError::UnsupportedPath { .. }),
                "{path_rel} produced {error:?}"
            );
        }
    }

    #[test]
    fn jsx_and_tsx_are_never_folded_into_their_plain_dialects() {
        assert_ne!(ParserDialect::Jsx, ParserDialect::JavaScript);
        assert_ne!(ParserDialect::Tsx, ParserDialect::TypeScript);
        assert_ne!(
            ParserDialect::Tsx.grammar(),
            ParserDialect::TypeScript.grammar(),
            "TSX is a different grammar, not a flag"
        );
        assert!(ParserDialect::Jsx.has_markup() && ParserDialect::Tsx.has_markup());
        assert!(!ParserDialect::JavaScript.has_markup() && !ParserDialect::TypeScript.has_markup());
    }

    #[test]
    fn a_dialect_maps_onto_the_existing_resource_language_vocabulary() {
        // React is not a language, and no dialect invents one.
        assert_eq!(
            ParserDialect::Jsx.language(),
            ResourceLanguage::JavaScript,
            "JSX is a JavaScript dialect"
        );
        assert_eq!(ParserDialect::Tsx.language(), ResourceLanguage::TypeScript);
        assert_eq!(ParserDialect::Python.language(), ResourceLanguage::Python);
        assert_eq!(ParserDialect::CSharp.language(), ResourceLanguage::CSharp);
        assert_eq!(ParserDialect::Rust.language(), ResourceLanguage::Rust);
        assert_eq!(ParserDialect::Svelte.language(), ResourceLanguage::Svelte);
    }

    #[test]
    fn svelte_is_a_container_and_does_not_pretend_to_be_a_script_file() {
        let capability = ParserDialect::Svelte.capability();
        assert!(!capability.covers_whole_file());
        let StructuralCapability::Container { embedded } = capability else {
            panic!("a Svelte component is a container");
        };
        assert_eq!(
            embedded,
            &[ResourceLanguage::TypeScript, ResourceLanguage::JavaScript],
            "the embedded languages are named, not silently indexed"
        );
        assert!(
            ParserDialect::TypeScript.capability().covers_whole_file(),
            "a real TS file is not a container"
        );
        assert_ne!(
            ParserDialect::Svelte.grammar(),
            ParserDialect::TypeScript.grammar()
        );
    }

    #[test]
    fn a_directory_or_a_misclassified_resource_is_an_explicit_error() {
        let mut directory = file_resource("src", None);
        directory.kind = ResourceKind::Directory;
        assert!(matches!(
            dialect_for_resource(&directory).expect_err("directories have no source"),
            ParseError::UnsupportedKind { .. }
        ));

        let mismatched = file_resource("src/mod.ts", Some(ResourceLanguage::Python));
        assert!(matches!(
            dialect_for_resource(&mismatched).expect_err("a disagreement is reported"),
            ParseError::LanguageMismatch { .. }
        ));

        // The normal case: a TSX Resource is classified TypeScript, and the
        // finer dialect is not a disagreement.
        assert_eq!(
            dialect_for_resource(&file_resource(
                "src/view.tsx",
                Some(ResourceLanguage::TypeScript)
            ))
            .expect("tsx is a TypeScript dialect"),
            ParserDialect::Tsx
        );
    }

    #[test]
    fn every_supported_dialect_parses_its_minimal_declaration_fixture() {
        let mut registry = ParserRegistry::new();
        for (dialect, path_rel, source) in FIXTURES {
            assert_eq!(
                dialect_for_path(path_rel).expect("supported"),
                *dialect,
                "{path_rel} must route to its own dialect"
            );
            let tree = registry
                .parse(*dialect, source.as_bytes(), SourceBasis::default())
                .expect("a valid fixture parses");
            assert_eq!(
                tree.status(),
                ParseStatus::Complete,
                "{path_rel} should parse cleanly, errors at {:?}",
                tree.error_spans()
            );
            assert!(tree.error_spans().is_empty());

            let root = tree.root_span();
            assert_eq!(root.start_byte, 0);
            assert_eq!(
                root.end_byte,
                source.len(),
                "{path_rel} root span must cover the whole source"
            );
            assert_eq!(root.start, SourcePoint::new(0, 0));
            assert_eq!(tree.source_len(), source.len());
        }
    }

    #[test]
    fn a_descriptor_carries_the_backend_and_grammar_identity_a_profile_needs() {
        let mut registry = ParserRegistry::new();
        let descriptor = registry
            .describe(ParserDialect::Tsx)
            .expect("the grammar loads");

        assert_eq!(descriptor.backend, STRUCTURAL_BACKEND);
        assert_eq!(descriptor.dialect, ParserDialect::Tsx);
        assert_eq!(descriptor.language, ResourceLanguage::TypeScript);
        assert_eq!(descriptor.grammar, "tree-sitter-typescript:tsx");
        assert!(descriptor.backend_abi_version > 0);
        assert!(descriptor.grammar_abi_version > 0);
        assert!(descriptor.capability.covers_whole_file());
    }

    #[test]
    fn tsx_markup_is_not_parsed_by_the_plain_typescript_grammar() {
        let source = b"const View = <div className=\"row\">text</div>;\n";
        let mut registry = ParserRegistry::new();

        let tsx = registry
            .parse(ParserDialect::Tsx, source, SourceBasis::default())
            .expect("parse");
        assert_eq!(tsx.status(), ParseStatus::Complete);

        let plain = registry
            .parse(ParserDialect::TypeScript, source, SourceBasis::default())
            .expect("parse");
        assert_eq!(
            plain.status(),
            ParseStatus::Partial,
            "picking the wrong dialect must be visible, not silently wrong"
        );
    }

    #[test]
    fn malformed_source_still_yields_a_tree_and_says_where_it_broke() {
        let source = b"def top(a, b:\n    return a +\n\nclass Thing:\n    pass\n";
        let mut registry = ParserRegistry::new();
        let tree = registry
            .parse(ParserDialect::Python, source, SourceBasis::default())
            .expect("broken source is not a parse failure");

        assert_eq!(tree.status(), ParseStatus::Partial);
        let spans = tree.error_spans();
        assert!(!spans.is_empty(), "the damage must be locatable");
        for span in &spans {
            assert!(span.start_byte <= span.end_byte);
            assert!(span.end_byte <= source.len());
        }
        assert_eq!(
            tree.root_span().end_byte,
            source.len(),
            "the tree still covers the whole file"
        );
    }

    #[test]
    fn an_unsupported_file_is_never_a_zero_result_success() {
        let mut registry = ParserRegistry::new();
        let error = dialect_for_path("docs/guide.md").expect_err("unsupported");

        assert!(matches!(error, ParseError::UnsupportedPath { .. }));
        assert!(
            error.to_string().contains("unsupported"),
            "the refusal must read as unsupported, not as empty: {error}"
        );
        // There is no way to ask for a parse without naming a dialect, so
        // an unsupported file cannot produce an empty successful tree.
        let supported = registry
            .parse(ParserDialect::Rust, b"fn a() {}\n", SourceBasis::default())
            .expect("parse");
        assert_eq!(supported.status(), ParseStatus::Complete);
    }

    #[test]
    fn a_reparse_reuses_the_previous_tree() {
        let before = "fn a() {}\n";
        let after = "fn a() {}\nfn b() {}\n";
        let mut registry = ParserRegistry::new();
        let first = registry
            .parse(
                ParserDialect::Rust,
                before.as_bytes(),
                SourceBasis::default(),
            )
            .expect("parse");

        let edit = SourceEdit {
            start_byte: before.len(),
            old_end_byte: before.len(),
            new_end_byte: after.len(),
            start: SourcePoint::new(1, 0),
            old_end: SourcePoint::new(1, 0),
            new_end: SourcePoint::new(2, 0),
        };
        let second = registry
            .reparse(&first, &[edit], after.as_bytes(), SourceBasis::default())
            .expect("reparse");

        assert_eq!(second.status(), ParseStatus::Complete);
        assert_eq!(second.root_span().end_byte, after.len());
        assert_eq!(second.source_len(), after.len());
        // The caller's tree is untouched: reparsing edits a clone.
        assert_eq!(first.root_span().end_byte, before.len());
    }

    #[test]
    fn a_parse_carries_the_basis_that_a_revision_check_will_need() {
        let resource = file_resource("src/thing.rs", Some(ResourceLanguage::Rust));
        let mut registry = ParserRegistry::new();
        let tree = registry
            .parse(
                dialect_for_resource(&resource).expect("supported"),
                b"fn a() {}\n",
                SourceBasis::of(&resource),
            )
            .expect("parse");

        assert_eq!(tree.basis().path_rel.as_deref(), Some("src/thing.rs"));
        assert_eq!(tree.basis().resource_revision.as_deref(), Some("1"));
        assert_eq!(tree.basis().content_hash, resource.content_hash);
    }

    #[test]
    fn the_backend_tree_stays_behind_the_adapter() {
        let mut registry = ParserRegistry::new();
        let tree = registry
            .parse(ParserDialect::Rust, b"fn a() {}\n", SourceBasis::default())
            .expect("parse");

        // Reachable inside the crate, for the task 8 extractor only.
        assert_eq!(tree.syntax_tree().root_node().kind(), "source_file");
        // And never leaked by the public representation.
        assert!(!format!("{tree:?}").contains("source_file"));
    }

    #[test]
    fn registries_share_no_state_across_workspaces() {
        let workers: Vec<_> = (0..4)
            .map(|worker| {
                thread::spawn(move || {
                    // Each thread stands in for one Workspace: its own
                    // registry, its own parsers, no shared global.
                    let mut registry = ParserRegistry::new();
                    for _ in 0..8 {
                        for (dialect, _, source) in FIXTURES {
                            let tree = registry
                                .parse(*dialect, source.as_bytes(), SourceBasis::default())
                                .expect("parse");
                            assert_eq!(
                                tree.status(),
                                ParseStatus::Complete,
                                "worker {worker} on {dialect}"
                            );
                        }
                    }
                })
            })
            .collect();

        for worker in workers {
            worker.join().expect("no worker may fail");
        }
    }

    #[test]
    fn parsing_writes_nothing_to_index_db() {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let base: PathBuf = env::temp_dir().join(format!(
            "brainprint-parser-persistence-{}-{sequence}",
            process::id()
        ));
        let root = base.join("workspace");
        fs::create_dir_all(&root).expect("workspace root");
        for (_, path_rel, source) in FIXTURES {
            fs::write(root.join(path_rel), source).expect("fixture");
        }
        let db_path = base.join("data").join("index.db");

        let engine = BaselineScan::open(&db_path).expect("index.db");
        engine
            .run_initial_scan(&root, &WorkspaceConfig::default(), "workspace-rev-1")
            .expect("baseline scan");
        let resources = engine.resources().list_active().expect("list");
        drop(engine);
        let before = fs::read(&db_path).expect("index.db bytes");

        let mut registry = ParserRegistry::new();
        for resource in &resources {
            let Ok(dialect) = dialect_for_resource(resource) else {
                continue;
            };
            let source = fs::read(root.join(&resource.path_rel)).expect("source");
            registry
                .parse(dialect, &source, SourceBasis::of(resource))
                .expect("parse");
        }

        let after = fs::read(&db_path).expect("index.db bytes");
        assert_eq!(after, before, "parsing must not write a single byte");
        // And the source it parsed is still nowhere in the database.
        // Declaration *headers* are stored deliberately (#16 task 8's
        // `Symbol.signature`), so the needles here are bodies -- the
        // thing no table may mirror.
        for needle in [
            "return self.value",
            "return a + b",
            "public int Value => 1;",
        ] {
            assert!(
                !after
                    .windows(needle.len())
                    .any(|window| window == needle.as_bytes()),
                "index.db must not contain {needle:?}"
            );
        }

        let _ = fs::remove_dir_all(&base);
    }
}
