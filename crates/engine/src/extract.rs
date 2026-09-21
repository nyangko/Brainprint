//! Structural Symbol/Scope/span extraction, the structural Occurrence
//! evidence found in the same walk, and the SymbolID continuity rules that
//! survive an edit (#16 task 8-9).
//!
//! This is the only place a `tree_sitter` tree is walked. What comes out is
//! [`ExtractedSymbol`] -- Brainprint types with byte + line/column spans --
//! and what goes into `index.db` is [`crate::symbol::Symbol`]. No backend
//! type crosses either boundary.
//!
//! ## What is extracted
//!
//! The [`SymbolKind`] taxonomy, and nothing else. A local variable, a
//! temporary, an expression, an enum member, a parameter: none of them
//! becomes a persistent Symbol. Declarations *inside* a function body are
//! still declarations -- a nested `def` or `class` is extracted -- but a
//! binding inside one is not, which is the whole of the "no locals" rule
//! expressed as one piece of walker state.
//!
//! ## Scope
//!
//! A block, an `if`, a loop, a C# `namespace`, a Rust `mod`, a Rust `impl`,
//! a TS `namespace`: none of these is a Symbol, and none of them gets a
//! synthetic row. The ones that name something contribute a segment to
//! [`ExtractedSymbol::qualified_name`]; the rest are pure walker state.
//! `parent` points only at a real enclosing Symbol.
//!
//! A Rust `impl` block deliberately does **not** become its type's
//! declaration: joining `impl Thing` to `struct Thing` is type resolution,
//! which I2 does not do. The method's lexical identity still reads
//! `Thing::value`, and its `parent` is `None`.
//!
//! ## qualified_name
//!
//! The chain of enclosing declarations and named lexical segments inside
//! one Resource, joined with the language's own separator. It is lexical
//! identity, not a module path: no import graph, package layout, or
//! `__init__.py` is consulted, because guessing one would be semantic
//! resolution (I4).
//!
//! ## SymbolID continuity
//!
//! A span is never identity -- if it were, adding a line at the top of a
//! file would renumber every Symbol in it. Continuity is decided by
//! structural identity instead ([`assign_ids`]):
//!
//! 1. `(qualified_name, kind)` -- which already encodes the parent chain
//!    and the name. Unique on both sides → the previous [`SymbolId`] is
//!    kept, whatever moved.
//! 2. Ambiguous (overloads, two same-named declarations) → narrowed by
//!    `signature`.
//! 3. Still ambiguous → a **new** id. A false split can be repaired by a
//!    later edit; a false merge silently attributes one declaration's
//!    history to another.
//!
//! A rename produces a different `qualified_name`, so it matches nothing
//! and gets a new id, and the old Symbol simply ceases to exist in the
//! replacement. Nothing tries to argue that a renamed declaration "is" the
//! old one.
//!
//! ## Occurrence evidence
//!
//! The same walk collects [`ExtractedOccurrence`]s: a declaration's own
//! name token, the module or name an import statement writes, and a call's
//! callee. Nothing else. Every other identifier is left alone rather than
//! asserted to be a reference to something, because what it refers to is
//! resolution (I3/I4) -- and for the same reason none of this evidence
//! carries a target. An occurrence's containing Symbol is whatever
//! lexically encloses it, which the walk already knows; no semantic owner
//! is worked out.
//!
//! ## Partial parses
//!
//! A [`ParseStatus::Partial`] tree still yields the candidates it can see,
//! but the extraction is **not accepted**: publishing it would let one
//! broken keystroke replace a file's whole Symbol set with a fragment, and
//! a file that fails to parse is not a file with no symbols. The
//! partial/last-valid publication policy is #16 task 14. A container
//! dialect ([`ExtractionStatus::ContainerOnly`]) is refused for the same
//! reason it is honest: Svelte's embedded script is not extracted here, so
//! an empty result must never read as "this component declares nothing".

use std::collections::HashMap;

use brainprint_core::SymbolId;
use tree_sitter::Node;

use crate::{
    parser::{ParseStatus, ParseTree, ParserDialect, SourcePoint, SourceSpan},
    resource::Resource,
    symbol::{AnalysisProfile, Occurrence, OccurrenceKind, Symbol, SymbolKind, Visibility},
};

/// Upper bound on a stored `signature`, in characters.
///
/// A signature is a declaration header, not source. The bound is the hard
/// guarantee behind that: whatever a declaration's syntax turns out to
/// look like, no body can reach the database through this column.
pub const SIGNATURE_MAX_CHARS: usize = 200;

/// Marker appended to a signature the bound cut short.
const TRUNCATION_MARK: char = '…';

/// How much of a Resource an extraction actually accounts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionStatus {
    /// A complete parse of a whole-file dialect: this is the file's full
    /// Symbol set, and it may replace what is stored.
    Complete,
    /// The parse had syntax errors. The candidates are real as far as they
    /// go, but they are not the file's Symbol set.
    Partial,
    /// A container dialect. Its own boundary is understood; the embedded
    /// language inside it is not extracted at this stage.
    ContainerOnly,
}

impl ExtractionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "COMPLETE",
            Self::Partial => "PARTIAL",
            Self::ContainerOnly => "CONTAINER_ONLY",
        }
    }
}

impl std::fmt::Display for ExtractionStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One declaration, before it has an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedSymbol {
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub visibility: Visibility,
    pub exported: bool,
    pub span: SourceSpan,
    /// Index of the enclosing Symbol in the same list, which always
    /// precedes this one.
    pub parent: Option<usize>,
}

/// One piece of structural source evidence, before it has a Resource,
/// a profile, or a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtractedOccurrence {
    pub kind: OccurrenceKind,
    /// The evidence's own narrow span: a declaration's name token, the
    /// module/name an import writes, a call's callee.
    pub span: SourceSpan,
    /// Index of the smallest enclosing Symbol in the same extraction, or
    /// `None` at file level. Lexical containment only.
    pub containing: Option<usize>,
}

/// What one parse yielded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extraction {
    pub status: ExtractionStatus,
    pub dialect: ParserDialect,
    /// The profile these candidates were produced under, dialect included.
    pub profile: AnalysisProfile,
    pub symbols: Vec<ExtractedSymbol>,
    /// Structural evidence found in the same walk. Never a Relation: no
    /// entry here names a target.
    pub occurrences: Vec<ExtractedOccurrence>,
}

impl Extraction {
    /// Whether this result may replace a Resource's stored Symbol set.
    ///
    /// Only a complete parse of a whole-file dialect may. Everything else
    /// is returned to the caller but never written, so a broken edit or an
    /// unsupported container can never blank a file's symbols.
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        self.status == ExtractionStatus::Complete
    }
}

/// Walk a parse tree into structural candidates.
///
/// `source` must be the bytes the tree was parsed from -- spans index
/// straight into it.
#[must_use]
pub fn extract(tree: &ParseTree, source: &[u8]) -> Extraction {
    let descriptor = tree.descriptor();
    let dialect = descriptor.dialect;
    let profile = AnalysisProfile::of(descriptor);

    if !descriptor.capability.covers_whole_file() {
        // A container's embedded script is a different language that a
        // later adapter must map (I4). Returning an empty COMPLETE result
        // here would be a false zero.
        return Extraction {
            status: ExtractionStatus::ContainerOnly,
            dialect,
            profile,
            symbols: Vec::new(),
            // An unmapped container yields no evidence either. Claiming
            // occurrences for an embedded script this stage does not read
            // would be inventing them.
            occurrences: Vec::new(),
        };
    }

    let mut walker = Walker {
        source,
        dialect,
        separator: separator(dialect),
        symbols: Vec::new(),
        occurrences: Vec::new(),
    };
    walker.walk(
        tree.syntax_tree().root_node(),
        &Frame {
            segments: Vec::new(),
            parent: None,
            in_callable: false,
            member_context: false,
        },
    );

    // Source order, which is also the order the evidence reads back in.
    walker.occurrences.sort_by(|left, right| {
        (left.span.start_byte, left.span.end_byte, left.kind.as_str()).cmp(&(
            right.span.start_byte,
            right.span.end_byte,
            right.kind.as_str(),
        ))
    });

    Extraction {
        status: match tree.status() {
            ParseStatus::Complete => ExtractionStatus::Complete,
            ParseStatus::Partial => ExtractionStatus::Partial,
        },
        dialect,
        profile,
        symbols: walker.symbols,
        occurrences: walker.occurrences,
    }
}

/// Bind an extraction's evidence to a Resource, a profile, and the
/// generation that publishes it.
///
/// `symbols` must be [`assign_ids`]'s output for the same extraction: it
/// is index-aligned with `extraction.symbols`, which is how a candidate's
/// enclosing-Symbol index becomes a [`SymbolId`].
#[must_use]
pub fn resolve_occurrences(
    extraction: &Extraction,
    symbols: &[Symbol],
    resource: &Resource,
    analysis_profile_id: i64,
    generation_id: i64,
) -> Vec<Occurrence> {
    extraction
        .occurrences
        .iter()
        .map(|evidence| Occurrence {
            resource_id: resource.id,
            containing_symbol_id: evidence
                .containing
                .and_then(|index| symbols.get(index))
                .map(|symbol| symbol.id),
            kind: evidence.kind,
            span: evidence.span,
            // A structural Occurrence resolves nothing: both stay NULL
            // until I3/I4 has something true to put in them.
            relation_id: None,
            resolution_context_id: None,
            analysis_profile_id,
            resource_revision: resource.resource_revision.clone(),
            generation_id,
        })
        .collect()
}

/// Give every candidate a [`SymbolId`], reusing a previous one wherever the
/// structural evidence is unambiguous.
///
/// See the module docs for the rules. `previous` is the Resource's stored
/// Symbol set; anything in it that nothing matches simply does not appear
/// in the result, which is what a whole-set replacement means.
#[must_use]
pub fn assign_ids(
    previous: &[Symbol],
    extraction: &Extraction,
    resource: &Resource,
    analysis_profile_id: i64,
) -> Vec<Symbol> {
    let previous_by_structure = index_unique(previous.iter().map(structural_key));
    let previous_by_signature = index_unique(previous.iter().map(signature_key));
    let extracted_by_structure =
        index_unique(extraction.symbols.iter().map(extracted_structural_key));
    let extracted_by_signature =
        index_unique(extraction.symbols.iter().map(extracted_signature_key));

    let mut assigned: Vec<SymbolId> = Vec::with_capacity(extraction.symbols.len());
    let mut taken: Vec<SymbolId> = Vec::new();
    for candidate in &extraction.symbols {
        // Unique on both sides, or it is not evidence of continuity.
        let carried = unique_match(
            &previous_by_structure,
            &extracted_by_structure,
            &extracted_structural_key(candidate),
        )
        .or_else(|| {
            unique_match(
                &previous_by_signature,
                &extracted_by_signature,
                &extracted_signature_key(candidate),
            )
        })
        .map(|index| previous[index].id)
        // Belt and braces: one previous id is never handed to two
        // candidates, whatever the keys say.
        .filter(|id| !taken.contains(id));

        let id = carried.unwrap_or_else(SymbolId::generate);
        taken.push(id);
        assigned.push(id);
    }

    extraction
        .symbols
        .iter()
        .zip(&assigned)
        .map(|(candidate, id)| Symbol {
            id: *id,
            resource_id: resource.id,
            parent_id: candidate.parent.map(|index| assigned[index]),
            kind: candidate.kind,
            name: candidate.name.clone(),
            qualified_name: candidate.qualified_name.clone(),
            signature: candidate.signature.clone(),
            visibility: candidate.visibility,
            exported: candidate.exported,
            span: candidate.span,
            resource_revision: resource.resource_revision.clone(),
            analysis_profile_id,
        })
        .collect()
}

fn structural_key(symbol: &Symbol) -> String {
    format!("{}\u{1}{}", symbol.qualified_name, symbol.kind)
}

fn extracted_structural_key(symbol: &ExtractedSymbol) -> String {
    format!("{}\u{1}{}", symbol.qualified_name, symbol.kind)
}

fn signature_key(symbol: &Symbol) -> String {
    format!(
        "{}\u{1}{}\u{1}{}",
        symbol.qualified_name,
        symbol.kind,
        symbol.signature.as_deref().unwrap_or("")
    )
}

fn extracted_signature_key(symbol: &ExtractedSymbol) -> String {
    format!(
        "{}\u{1}{}\u{1}{}",
        symbol.qualified_name,
        symbol.kind,
        symbol.signature.as_deref().unwrap_or("")
    )
}

/// Map each key to its single owner's index, dropping every key that more
/// than one item claims -- an ambiguous key is not evidence.
fn index_unique(keys: impl Iterator<Item = String>) -> HashMap<String, Option<usize>> {
    let mut index: HashMap<String, Option<usize>> = HashMap::new();
    for (position, key) in keys.enumerate() {
        index
            .entry(key)
            .and_modify(|slot| *slot = None)
            .or_insert(Some(position));
    }
    index
}

fn unique_match(
    previous: &HashMap<String, Option<usize>>,
    extracted: &HashMap<String, Option<usize>>,
    key: &str,
) -> Option<usize> {
    extracted.get(key)?.and_then(|_| *previous.get(key)?)
}

fn separator(dialect: ParserDialect) -> &'static str {
    match dialect {
        ParserDialect::Rust => "::",
        _ => ".",
    }
}

/// One node's contribution to the walk.
enum Declared {
    /// Real declarations. More than one when a single statement declares
    /// several names.
    Symbols(Vec<Candidate>),
    /// A named lexical scope that is not itself a Symbol: a namespace, a
    /// Rust `mod`, a Rust `impl`. `member_context` says whether what it
    /// contains are members rather than free functions.
    Segment { name: String, member_context: bool },
    /// Nothing of its own; keep walking.
    Nothing,
}

struct Candidate {
    kind: SymbolKind,
    name: String,
    /// The name token itself, which is the DEFINITION evidence span.
    name_span: SourceSpan,
    span: SourceSpan,
    signature: Option<String>,
    visibility: Visibility,
    exported: bool,
}

struct Frame {
    segments: Vec<String>,
    parent: Option<usize>,
    /// Inside a function/method body: bindings here are locals.
    in_callable: bool,
    /// Inside a type or an `impl`: a function here is a method.
    member_context: bool,
}

struct Walker<'a> {
    source: &'a [u8],
    dialect: ParserDialect,
    separator: &'static str,
    symbols: Vec<ExtractedSymbol>,
    occurrences: Vec<ExtractedOccurrence>,
}

impl Walker<'_> {
    fn walk(&mut self, node: Node<'_>, frame: &Frame) {
        self.collect_evidence(node, frame);
        let declared = self.classify(node, frame);
        let child_frame = match declared {
            Declared::Nothing => None,
            Declared::Segment {
                name,
                member_context,
            } => Some(Frame {
                segments: extend(&frame.segments, &name),
                parent: frame.parent,
                in_callable: frame.in_callable,
                member_context,
            }),
            Declared::Symbols(candidates) => self.emit(candidates, frame),
        };

        let frame = child_frame.as_ref().unwrap_or(frame);
        // A C# file-scoped namespace has no body: it names the scope of
        // everything *after* it, so it changes the sibling frame rather
        // than a child frame.
        let mut siblings = Frame {
            segments: frame.segments.clone(),
            parent: frame.parent,
            in_callable: frame.in_callable,
            member_context: frame.member_context,
        };
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match self.trailing_segment(child) {
                Some(name) => siblings.segments.push(name),
                None => self.walk(child, &siblings),
            }
        }
    }

    /// A declaration that scopes its following siblings instead of its own
    /// children. Only C#'s file-scoped namespace works this way.
    fn trailing_segment(&self, node: Node<'_>) -> Option<String> {
        (self.dialect == ParserDialect::CSharp
            && node.kind() == "file_scoped_namespace_declaration")
            .then(|| node.child_by_field_name("name"))
            .flatten()
            .and_then(|name| self.plain_name(name))
    }

    /// Push the candidates this node declares, and produce the frame its
    /// children belong in -- but only when exactly one Symbol came out, so
    /// a multi-name statement never invents a parent for its own subtree.
    fn emit(&mut self, candidates: Vec<Candidate>, frame: &Frame) -> Option<Frame> {
        let mut emitted: Vec<usize> = Vec::new();
        for candidate in candidates {
            if is_binding(candidate.kind) && frame.in_callable {
                // A binding inside a function body is a local.
                continue;
            }
            let qualified_name = extend(&frame.segments, &candidate.name).join(self.separator);
            // A declaration is evidence of itself, at its name token --
            // not across the whole body it opens.
            self.occurrences.push(ExtractedOccurrence {
                kind: OccurrenceKind::Definition,
                span: candidate.name_span,
                containing: Some(self.symbols.len()),
            });
            self.symbols.push(ExtractedSymbol {
                kind: candidate.kind,
                name: candidate.name,
                qualified_name,
                signature: candidate.signature,
                visibility: candidate.visibility,
                exported: candidate.exported,
                span: candidate.span,
                parent: frame.parent,
            });
            emitted.push(self.symbols.len() - 1);
        }

        let [index] = emitted[..] else {
            return None;
        };
        let symbol = &self.symbols[index];
        Some(Frame {
            segments: extend(&frame.segments, &symbol.name),
            parent: Some(index),
            in_callable: frame.in_callable || is_callable(symbol.kind),
            member_context: symbol.kind.is_type_like(),
        })
    }

    /// Import and call evidence at `node`, if any.
    ///
    /// `frame.parent` is already the smallest Symbol that lexically
    /// contains this node, which is exactly the containment rule -- no
    /// semantic owner is worked out.
    fn collect_evidence(&mut self, node: Node<'_>, frame: &Frame) {
        let containing = frame.parent;
        let mut push = |kind: OccurrenceKind, span: SourceSpan| {
            self.occurrences.push(ExtractedOccurrence {
                kind,
                span,
                containing,
            });
        };

        match (self.dialect, node.kind()) {
            // A call is evidence that a call is written here. Which
            // function it reaches is resolution (I3/I4), so the callee
            // expression's span is all that is recorded.
            (ParserDialect::Python, "call")
            | (
                ParserDialect::JavaScript
                | ParserDialect::Jsx
                | ParserDialect::TypeScript
                | ParserDialect::Tsx
                | ParserDialect::Rust,
                "call_expression",
            )
            | (ParserDialect::CSharp, "invocation_expression") => {
                if let Some(callee) = node.child_by_field_name("function") {
                    push(OccurrenceKind::CallSite, span_of(callee));
                }
                // A name handed to a call as a value is the one
                // reference shape that is structurally unambiguous
                // evidence (#17 task 5's `register(save)`). Every other
                // identifier stays unrecorded: an index of all of them
                // would be a different product.
                for argument in bare_identifier_arguments(node) {
                    push(OccurrenceKind::ReferenceSite, span_of(argument));
                }
            }
            (ParserDialect::Python, "import_statement" | "import_from_statement") => {
                for field in ["module_name", "name"] {
                    for named in children_by_field(node, field) {
                        // `import x as y` names x; the alias is a local
                        // binding, not the thing imported.
                        let evidence = named
                            .child_by_field_name("name")
                            .filter(|_| named.kind() == "aliased_import")
                            .unwrap_or(named);
                        push(OccurrenceKind::ImportSite, span_of(evidence));
                    }
                }
            }
            (
                ParserDialect::JavaScript
                | ParserDialect::Jsx
                | ParserDialect::TypeScript
                | ParserDialect::Tsx,
                "import_statement",
            ) => {
                if let Some(source) = node.child_by_field_name("source") {
                    push(OccurrenceKind::ImportSite, span_of(source));
                }
                for specifier in descendants(node, "import_specifier") {
                    if let Some(name) = specifier.child_by_field_name("name") {
                        push(OccurrenceKind::ImportSite, span_of(name));
                    }
                }
            }
            (ParserDialect::CSharp, "using_directive") => {
                // The last name in the directive is the namespace being
                // used; an alias in front of it is a local name.
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
                if let Some(used) = names.last() {
                    push(OccurrenceKind::ImportSite, span_of(*used));
                }
            }
            (ParserDialect::Rust, "use_declaration") => {
                if let Some(argument) = node.child_by_field_name("argument") {
                    push(OccurrenceKind::ImportSite, span_of(argument));
                }
            }
            _ => {}
        }

        // Type positions whose relation the syntax settles (#17 task 6).
        // The set and the spans are that module's, so evidence and
        // relations cannot disagree about what counts as a type
        // reference or where it sits. An `override` marker is not one:
        // it states that something is overridden, never what, so it
        // gets no Occurrence to bind to.
        for reference in crate::types::type_references_at(node, self.dialect, self.source) {
            if reference.evidence != crate::types::TypeEvidence::OverrideMarker {
                push(OccurrenceKind::TypeSite, reference.span);
            }
        }

        // Environment and configuration keys the syntax settles (#17
        // task 12). Same arrangement: the set and the spans belong to
        // that module. A dynamic key has no literal to point at, so it
        // gets no Occurrence -- there would be nothing to bind.
        for access in crate::domain::key_accesses_at(node, self.dialect, self.source) {
            if matches!(access.key, crate::domain::KeyLiteral::Static(_)) {
                push(OccurrenceKind::KeySite, access.span);
            }
        }
    }

    fn classify(&self, node: Node<'_>, frame: &Frame) -> Declared {
        match self.dialect {
            ParserDialect::Python => self.classify_python(node, frame),
            ParserDialect::JavaScript
            | ParserDialect::Jsx
            | ParserDialect::TypeScript
            | ParserDialect::Tsx => self.classify_js_ts(node, frame),
            ParserDialect::CSharp => self.classify_csharp(node, frame),
            ParserDialect::Rust => self.classify_rust(node, frame),
            // Handled before the walk ever starts.
            ParserDialect::Svelte => Declared::Nothing,
        }
    }

    fn classify_python(&self, node: Node<'_>, frame: &Frame) -> Declared {
        let kind = match node.kind() {
            "class_definition" => SymbolKind::Class,
            "function_definition" => {
                if frame.member_context {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                }
            }
            "type_alias_statement" => SymbolKind::TypeAlias,
            "assignment" => {
                if frame.member_context {
                    SymbolKind::Field
                } else {
                    SymbolKind::Constant
                }
            }
            _ => return Declared::Nothing,
        };

        // `type X = int` names itself through a `type` wrapper; an
        // assignment to anything but a bare name (an attribute, a
        // subscript, a tuple) declares nothing this taxonomy covers.
        let name_node = match node.kind() {
            "assignment" | "type_alias_statement" => node.child_by_field_name("left"),
            _ => node.child_by_field_name("name"),
        };
        let Some((name, name_span)) = name_node.and_then(|node| self.name_of(node)) else {
            return Declared::Nothing;
        };

        Declared::Symbols(vec![Candidate {
            kind,
            name,
            name_span,
            span: span_of(node),
            signature: self.signature(node, self.signature_end(node)),
            // Python writes no visibility keyword, and a leading
            // underscore is a convention, not a declaration.
            visibility: Visibility::Unspecified,
            exported: false,
        }])
    }

    fn classify_js_ts(&self, node: Node<'_>, frame: &Frame) -> Declared {
        let exported = node
            .parent()
            .is_some_and(|parent| parent.kind() == "export_statement");
        let visibility = self.js_visibility(node);

        let simple = |kind: SymbolKind| Some(kind);
        let kind = match node.kind() {
            "class_declaration" | "abstract_class_declaration" | "class" => {
                simple(SymbolKind::Class)
            }
            "interface_declaration" => simple(SymbolKind::Interface),
            "enum_declaration" => simple(SymbolKind::Enum),
            "type_alias_declaration" => simple(SymbolKind::TypeAlias),
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                simple(SymbolKind::Function)
            }
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                simple(SymbolKind::Method)
            }
            "public_field_definition" | "field_definition" => simple(SymbolKind::Field),
            "property_signature" => simple(SymbolKind::Property),
            "internal_module" | "module" => {
                return match node
                    .child_by_field_name("name")
                    .and_then(|node| self.plain_name(node))
                {
                    // A TS namespace is a lexical scope, not a Symbol row.
                    Some(name) => Declared::Segment {
                        name,
                        member_context: false,
                    },
                    None => Declared::Nothing,
                };
            }
            "lexical_declaration" | "variable_declaration" => {
                return self.js_bindings(node, exported);
            }
            _ => None,
        };

        let Some(kind) = kind else {
            return Declared::Nothing;
        };
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("property"));
        let Some((name, name_span)) = name_node.and_then(|node| self.name_of(node)) else {
            return Declared::Nothing;
        };
        let _ = frame;

        Declared::Symbols(vec![Candidate {
            kind,
            name,
            name_span,
            span: span_of(node),
            signature: self.signature(node, self.signature_end(node)),
            visibility,
            exported,
        }])
    }

    /// `const`/`let`/`var`. A declarator whose value is a function is the
    /// idiomatic way these languages declare one, so it is recorded as a
    /// FUNCTION rather than as a constant that happens to be callable.
    fn js_bindings(&self, node: Node<'_>, exported: bool) -> Declared {
        // `const`/`let`/`var`, so each name's header reads as a
        // declaration even when one statement declares several of them.
        let keyword = node
            .child(0)
            .map(|child| self.text(child))
            .unwrap_or_default();
        let mut cursor = node.walk();
        let candidates: Vec<Candidate> = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "variable_declarator")
            .filter_map(|declarator| {
                let (name, name_span) = declarator
                    .child_by_field_name("name")
                    .and_then(|node| self.name_of(node))?;
                let value = declarator.child_by_field_name("value");
                let kind = match value.map(|node| node.kind()) {
                    Some("arrow_function" | "function_expression" | "function") => {
                        SymbolKind::Function
                    }
                    Some("class" | "class_declaration") => SymbolKind::Class,
                    _ => SymbolKind::Constant,
                };
                let header_end = value.map_or_else(|| declarator.end_byte(), |v| v.start_byte());
                let header = self.header(declarator.start_byte(), header_end)?;
                Some(Candidate {
                    kind,
                    name,
                    name_span,
                    span: span_of(node),
                    signature: Some(truncate(&format!("{keyword} {header}"))),
                    visibility: Visibility::Unspecified,
                    exported,
                })
            })
            .collect();
        Declared::Symbols(candidates)
    }

    fn classify_csharp(&self, node: Node<'_>, frame: &Frame) -> Declared {
        let (visibility, exported) = self.csharp_modifiers(node);
        let kind = match node.kind() {
            "class_declaration" => SymbolKind::Class,
            "interface_declaration" => SymbolKind::Interface,
            "struct_declaration" => SymbolKind::Struct,
            "enum_declaration" => SymbolKind::Enum,
            "record_declaration" | "record_struct_declaration" => SymbolKind::Record,
            "method_declaration" | "constructor_declaration" | "destructor_declaration" => {
                SymbolKind::Method
            }
            "property_declaration" => SymbolKind::Property,
            "delegate_declaration" => return Declared::Nothing,
            "namespace_declaration" => {
                return match node
                    .child_by_field_name("name")
                    .and_then(|node| self.plain_name(node))
                {
                    // A namespace names a scope; it declares no member.
                    Some(name) => Declared::Segment {
                        name,
                        member_context: false,
                    },
                    None => Declared::Nothing,
                };
            }
            "field_declaration" => return self.csharp_fields(node, visibility, exported),
            _ => return Declared::Nothing,
        };

        let Some((name, name_span)) = node
            .child_by_field_name("name")
            .and_then(|node| self.name_of(node))
        else {
            return Declared::Nothing;
        };
        let _ = frame;

        Declared::Symbols(vec![Candidate {
            kind,
            name,
            name_span,
            span: span_of(node),
            signature: self.signature(node, self.signature_end(node)),
            visibility,
            exported,
        }])
    }

    /// `public int a = 1, b = 2;` declares two fields in one statement, so
    /// both names share that statement's span -- it is the source that
    /// declares them.
    fn csharp_fields(&self, node: Node<'_>, visibility: Visibility, exported: bool) -> Declared {
        let constant = self
            .modifier_texts(node)
            .iter()
            .any(|modifier| modifier == "const");
        // The header is everything up to and including the declared type,
        // so a field's initializer never becomes its signature.
        let prefix = self
            .header(node.start_byte(), self.declared_type_end(node))
            .unwrap_or_default();
        let candidates = self
            .declarator_names(node)
            .into_iter()
            .map(|(name, name_span)| Candidate {
                kind: if constant {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Field
                },
                signature: Some(truncate(format!("{prefix} {name}").trim())),
                name,
                name_span,
                span: span_of(node),
                visibility,
                exported,
            })
            .collect();
        Declared::Symbols(candidates)
    }

    fn classify_rust(&self, node: Node<'_>, frame: &Frame) -> Declared {
        let (visibility, exported) = self.rust_visibility(node);
        let kind = match node.kind() {
            "struct_item" => SymbolKind::Struct,
            "enum_item" => SymbolKind::Enum,
            "trait_item" => SymbolKind::Trait,
            "type_item" => SymbolKind::TypeAlias,
            "const_item" | "static_item" => SymbolKind::Constant,
            "field_declaration" => SymbolKind::Field,
            "function_item" | "function_signature_item" => {
                if frame.member_context {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                }
            }
            "mod_item" => {
                return match node
                    .child_by_field_name("name")
                    .and_then(|node| self.plain_name(node))
                {
                    // A module names a scope. Binding it to a file or a
                    // crate path is module resolution, which is I4's.
                    Some(name) => Declared::Segment {
                        name,
                        member_context: false,
                    },
                    None => Declared::Nothing,
                };
            }
            "impl_item" => {
                return match node
                    .child_by_field_name("type")
                    .and_then(|node| self.plain_name(node))
                {
                    // `impl Thing` is not `struct Thing`: saying so would
                    // be type resolution. It only names the scope its
                    // methods are written in.
                    Some(name) => Declared::Segment {
                        name,
                        member_context: true,
                    },
                    None => Declared::Nothing,
                };
            }
            _ => return Declared::Nothing,
        };

        let Some((name, name_span)) = node
            .child_by_field_name("name")
            .and_then(|node| self.name_of(node))
        else {
            return Declared::Nothing;
        };

        Declared::Symbols(vec![Candidate {
            kind,
            name,
            name_span,
            span: span_of(node),
            signature: self.signature(node, self.signature_end(node)),
            visibility,
            exported,
        }])
    }

    /// The text of a node that is expected to be a bare name, unwrapping
    /// the one-child wrappers some grammars use (Python's `type`).
    fn plain_name(&self, node: Node<'_>) -> Option<String> {
        self.name_of(node).map(|(name, _)| name)
    }

    /// A bare name plus the span of the token it actually came from --
    /// which is the DEFINITION evidence span, never the declaration's.
    fn name_of(&self, node: Node<'_>) -> Option<(String, SourceSpan)> {
        const NAMES: &[&str] = &[
            "identifier",
            "type_identifier",
            "property_identifier",
            "private_property_identifier",
            "field_identifier",
            "dotted_name",
            "qualified_name",
            "shorthand_property_identifier",
        ];
        if NAMES.contains(&node.kind()) {
            return Some((self.text(node), span_of(node)));
        }
        if node.kind() == "type" || node.kind() == "generic_type" {
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            return children.into_iter().find_map(|child| self.name_of(child));
        }
        None
    }

    /// Where a C# field's declared type ends, so its header can stop
    /// there rather than swallowing every initializer in the statement.
    fn declared_type_end(&self, node: Node<'_>) -> usize {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|child| child.kind() == "variable_declaration")
            .and_then(|declaration| declaration.child_by_field_name("type"))
            .map_or_else(|| node.start_byte(), |declared| declared.end_byte())
    }

    fn declarator_names(&self, node: Node<'_>) -> Vec<(String, SourceSpan)> {
        let mut names = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "variable_declarator" {
                if let Some(named) = child
                    .child_by_field_name("name")
                    .and_then(|node| self.name_of(node))
                {
                    names.push(named);
                }
            } else if child.kind() == "variable_declaration" {
                names.extend(self.declarator_names(child));
            }
        }
        names
    }

    fn modifier_texts(&self, node: Node<'_>) -> Vec<String> {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| child.kind() == "modifier")
            .map(|child| self.text(child))
            .collect()
    }

    fn csharp_modifiers(&self, node: Node<'_>) -> (Visibility, bool) {
        let modifiers = self.modifier_texts(node);
        let visibility = if modifiers.iter().any(|value| value == "public") {
            Visibility::Public
        } else if modifiers.iter().any(|value| value == "private") {
            Visibility::Private
        } else if modifiers.iter().any(|value| value == "protected") {
            Visibility::Protected
        } else if modifiers.iter().any(|value| value == "internal") {
            Visibility::Internal
        } else {
            // C#'s default depends on the declaration's context; it is not
            // written here, so it is not claimed.
            Visibility::Unspecified
        };
        (visibility, visibility == Visibility::Public)
    }

    fn rust_visibility(&self, node: Node<'_>) -> (Visibility, bool) {
        let mut cursor = node.walk();
        let modifier = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "visibility_modifier")
            .map(|child| self.text(child));
        match modifier.as_deref() {
            Some("pub") => (Visibility::Public, true),
            // `pub(crate)`, `pub(super)`, `pub(in path)`: visible, but not
            // exported out of the crate.
            Some(_) => (Visibility::Internal, false),
            None => (Visibility::Unspecified, false),
        }
    }

    fn js_visibility(&self, node: Node<'_>) -> Visibility {
        let mut cursor = node.walk();
        let modifier = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "accessibility_modifier")
            .map(|child| self.text(child));
        match modifier.as_deref() {
            Some("public") => Visibility::Public,
            Some("private") => Visibility::Private,
            Some("protected") => Visibility::Protected,
            _ => Visibility::Unspecified,
        }
    }

    /// Where a declaration's header stops and its body or initializer
    /// begins.
    fn signature_end(&self, node: Node<'_>) -> usize {
        for field in ["body", "value", "right", "accessors"] {
            if let Some(child) = node.child_by_field_name(field) {
                return child.start_byte();
            }
        }
        // A declaration statement keeps its initializer on its declarator.
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if matches!(child.kind(), "variable_declarator" | "variable_declaration") {
                if let Some(value) = child.child_by_field_name("value") {
                    return value.start_byte();
                }
                let mut inner = child.walk();
                for grandchild in child.named_children(&mut inner) {
                    if grandchild.kind() == "variable_declarator" {
                        if let Some(value) = grandchild.child_by_field_name("value") {
                            return value.start_byte();
                        }
                    }
                }
            }
        }
        node.end_byte()
    }

    fn signature(&self, node: Node<'_>, end: usize) -> Option<String> {
        self.header(node.start_byte(), end)
    }

    /// The declaration header as one compact line: whitespace collapsed,
    /// trailing punctuation dropped, and hard-capped so no body can ever
    /// reach the database through this column.
    fn header(&self, start: usize, end: usize) -> Option<String> {
        let end = end.max(start).min(self.source.len());
        let raw = String::from_utf8_lossy(&self.source[start..end]);
        let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        let trimmed = collapsed
            .trim_end_matches([' ', '=', '{', ':', ';', ','])
            .trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(truncate(trimmed))
    }

    fn text(&self, node: Node<'_>) -> String {
        String::from_utf8_lossy(&self.source[node.byte_range()]).into_owned()
    }
}

fn truncate(value: &str) -> String {
    match value.char_indices().nth(SIGNATURE_MAX_CHARS) {
        None => value.to_owned(),
        Some((offset, _)) => {
            let mut short = value[..offset].to_owned();
            short.push(TRUNCATION_MARK);
            short
        }
    }
}

fn extend(segments: &[String], name: &str) -> Vec<String> {
    let mut extended = segments.to_vec();
    extended.push(name.to_owned());
    extended
}

/// Kinds whose body makes everything inside it local.
fn is_callable(kind: SymbolKind) -> bool {
    matches!(kind, SymbolKind::Function | SymbolKind::Method)
}

/// Kinds that are bindings, and therefore locals when they appear inside a
/// callable.
fn is_binding(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Constant | SymbolKind::Field | SymbolKind::Property
    )
}

/// Every named child of `node` under `field`, since a Python import can
/// name several things in one statement.
/// The identifiers a call receives as plain arguments -- `save` in
/// `register(save)`, and nothing else. A nested call, a literal, an
/// attribute access or any other expression is not a bare name, so it
/// produces no reference evidence here.
pub(crate) fn bare_identifier_arguments<'tree>(call: Node<'tree>) -> Vec<Node<'tree>> {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    let mut found = Vec::new();
    for child in arguments.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => found.push(child),
            // C# wraps each argument in an `argument` node.
            "argument" => {
                let mut inner = child.walk();
                found.extend(
                    child
                        .named_children(&mut inner)
                        .filter(|node| node.kind() == "identifier"),
                );
            }
            _ => {}
        }
    }
    found
}

pub(crate) fn children_by_field<'tree>(node: Node<'tree>, field: &str) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    let children: Vec<Node<'tree>> = node
        .children_by_field_name(field, &mut cursor)
        .filter(|child| child.is_named())
        .collect();
    children
}

/// Named descendants of `node` with the given kind.
fn descendants<'tree>(node: Node<'tree>, kind: &str) -> Vec<Node<'tree>> {
    let mut found = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            found.push(child);
        }
        found.extend(descendants(child, kind));
    }
    found
}

pub(crate) fn span_of(node: Node<'_>) -> SourceSpan {
    SourceSpan {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start: SourcePoint::new(node.start_position().row, node.start_position().column),
        end: SourcePoint::new(node.end_position().row, node.end_position().column),
    }
}

#[cfg(test)]
mod tests {
    use brainprint_core::ResourceId;

    use super::*;
    use crate::{
        parser::{ParserRegistry, SourceBasis},
        resource::{Resource, ResourceKind, ResourceLanguage, ResourceRole, ResourceState},
    };

    const PYTHON: &str = "\
import os

CONSTANT = 1
ANNOTATED: int = 2

type Alias = int


class Thing:
    field: int = 1

    def method(self) -> int:
        local = 1

        def inner() -> int:
            return local

        return inner()


def top(a, b=2):
    helper = a
    return helper
";

    const TYPESCRIPT: &str = "\
export interface Shape {
  value: number
  measure(): number
}

export type Alias = Shape | null

export enum Level { Low, High }

export const LIMIT = 10

export class Thing implements Shape {
  private hidden = 1
  public value = 2

  measure(): number {
    const scratch = this.value
    return scratch
  }
}

export function top(a: number): number {
  const scratch = a
  return scratch
}
";

    const RUST: &str = "\
pub mod inner {
    pub const LIMIT: u32 = 10;

    pub struct Thing {
        pub field: u32,
    }

    pub trait Shape {
        fn measure(&self) -> u32;
    }

    impl Shape for Thing {
        fn measure(&self) -> u32 {
            let scratch = self.field;
            scratch
        }
    }

    pub(crate) type Alias = u32;

    pub enum Level { Low, High }

    pub fn top() -> u32 { 1 }
}
";

    const CSHARP: &str = "\
namespace Demo;

public interface IShape { int Value { get; } }

public record Pair(int Left, int Right);

public struct Point { public int X; }

public enum Level { Low, High }

public class Thing : IShape
{
    private int hidden = 1;
    public const int Limit = 10;
    public int Value => hidden;
    public int Measure(int a) { var scratch = a; return scratch; }
}
";

    fn resource(revision: &str) -> Resource {
        Resource {
            id: ResourceId::generate(),
            path_rel: "src/thing".to_owned(),
            path_key: "src/thing".to_owned(),
            kind: ResourceKind::File,
            role: ResourceRole::Source,
            language: Some(ResourceLanguage::Rust),
            size_bytes: 0,
            mtime_ns: 0,
            fingerprint: "sha256-fp1:test".to_owned(),
            content_hash: Some("sha256:test".to_owned()),
            state: ResourceState::Active,
            resource_revision: revision.to_owned(),
            generated_kind: None,
            container_resource_id: None,
        }
    }

    fn run(dialect: ParserDialect, source: &str) -> Extraction {
        let mut registry = ParserRegistry::new();
        let tree = registry
            .parse(dialect, source.as_bytes(), SourceBasis::default())
            .expect("fixture parses");
        extract(&tree, source.as_bytes())
    }

    fn named(extraction: &Extraction) -> Vec<(SymbolKind, String)> {
        extraction
            .symbols
            .iter()
            .map(|symbol| (symbol.kind, symbol.qualified_name.clone()))
            .collect()
    }

    fn find<'a>(extraction: &'a Extraction, qualified_name: &str) -> &'a ExtractedSymbol {
        extraction
            .symbols
            .iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
            .unwrap_or_else(|| panic!("{qualified_name} should be extracted"))
    }

    fn ids(symbols: &[Symbol]) -> Vec<(String, SymbolId)> {
        symbols
            .iter()
            .map(|symbol| (symbol.qualified_name.clone(), symbol.id))
            .collect()
    }

    /// Extract, then give the candidates identities against `previous`.
    fn identify(previous: &[Symbol], dialect: ParserDialect, source: &str) -> Vec<Symbol> {
        let extraction = run(dialect, source);
        assert!(extraction.is_accepted(), "fixture must parse cleanly");
        assign_ids(previous, &extraction, &resource("1"), 1)
    }

    #[test]
    fn python_declarations_are_extracted_with_their_hierarchy() {
        let extraction = run(ParserDialect::Python, PYTHON);

        assert_eq!(
            named(&extraction),
            vec![
                (SymbolKind::Constant, "CONSTANT".to_owned()),
                (SymbolKind::Constant, "ANNOTATED".to_owned()),
                (SymbolKind::TypeAlias, "Alias".to_owned()),
                (SymbolKind::Class, "Thing".to_owned()),
                (SymbolKind::Field, "Thing.field".to_owned()),
                (SymbolKind::Method, "Thing.method".to_owned()),
                (SymbolKind::Function, "Thing.method.inner".to_owned()),
                (SymbolKind::Function, "top".to_owned()),
            ]
        );

        let class = find(&extraction, "Thing");
        let method = find(&extraction, "Thing.method");
        let inner = find(&extraction, "Thing.method.inner");
        assert_eq!(class.parent, None);
        assert_eq!(method.parent, Some(3), "a method's parent is its class");
        assert_eq!(inner.parent, Some(5), "a nested def's parent is its method");
    }

    #[test]
    fn typescript_declarations_are_extracted_with_their_hierarchy() {
        let extraction = run(ParserDialect::TypeScript, TYPESCRIPT);

        assert_eq!(
            named(&extraction),
            vec![
                (SymbolKind::Interface, "Shape".to_owned()),
                (SymbolKind::Property, "Shape.value".to_owned()),
                (SymbolKind::Method, "Shape.measure".to_owned()),
                (SymbolKind::TypeAlias, "Alias".to_owned()),
                (SymbolKind::Enum, "Level".to_owned()),
                (SymbolKind::Constant, "LIMIT".to_owned()),
                (SymbolKind::Class, "Thing".to_owned()),
                (SymbolKind::Field, "Thing.hidden".to_owned()),
                (SymbolKind::Field, "Thing.value".to_owned()),
                (SymbolKind::Method, "Thing.measure".to_owned()),
                (SymbolKind::Function, "top".to_owned()),
            ]
        );
        assert!(find(&extraction, "Shape").exported);
        assert_eq!(
            find(&extraction, "Thing.hidden").visibility,
            Visibility::Private
        );
        assert_eq!(
            find(&extraction, "Thing.value").visibility,
            Visibility::Public
        );
        assert!(
            !find(&extraction, "Thing.value").exported,
            "a class member is not itself an export"
        );
    }

    #[test]
    fn a_jsx_arrow_component_is_a_function_not_a_constant() {
        let source = "export const View = ({ label }) => <div>{label}</div>\n";
        let extraction = run(ParserDialect::Jsx, source);

        assert_eq!(
            named(&extraction),
            vec![(SymbolKind::Function, "View".to_owned())],
            "the idiomatic component declaration is a function"
        );
        assert!(find(&extraction, "View").exported);
    }

    #[test]
    fn csharp_declarations_keep_their_own_kinds() {
        let extraction = run(ParserDialect::CSharp, CSHARP);

        assert_eq!(
            named(&extraction),
            vec![
                (SymbolKind::Interface, "Demo.IShape".to_owned()),
                (SymbolKind::Property, "Demo.IShape.Value".to_owned()),
                // A record is not a class, and is not mapped onto one.
                (SymbolKind::Record, "Demo.Pair".to_owned()),
                (SymbolKind::Struct, "Demo.Point".to_owned()),
                (SymbolKind::Field, "Demo.Point.X".to_owned()),
                (SymbolKind::Enum, "Demo.Level".to_owned()),
                (SymbolKind::Class, "Demo.Thing".to_owned()),
                (SymbolKind::Field, "Demo.Thing.hidden".to_owned()),
                (SymbolKind::Constant, "Demo.Thing.Limit".to_owned()),
                (SymbolKind::Property, "Demo.Thing.Value".to_owned()),
                (SymbolKind::Method, "Demo.Thing.Measure".to_owned()),
            ]
        );
        assert_eq!(
            find(&extraction, "Demo.Thing").parent,
            None,
            "a namespace is a lexical scope, not a Symbol row"
        );
        assert_eq!(
            find(&extraction, "Demo.Thing.hidden").visibility,
            Visibility::Private
        );
        assert!(find(&extraction, "Demo.Thing.Limit").exported);
    }

    #[test]
    fn rust_declarations_keep_trait_and_impl_apart() {
        let extraction = run(ParserDialect::Rust, RUST);

        assert_eq!(
            named(&extraction),
            vec![
                (SymbolKind::Constant, "inner::LIMIT".to_owned()),
                (SymbolKind::Struct, "inner::Thing".to_owned()),
                (SymbolKind::Field, "inner::Thing::field".to_owned()),
                // A trait is its own kind, not an interface row.
                (SymbolKind::Trait, "inner::Shape".to_owned()),
                (SymbolKind::Method, "inner::Shape::measure".to_owned()),
                (SymbolKind::Method, "inner::Thing::measure".to_owned()),
                (SymbolKind::TypeAlias, "inner::Alias".to_owned()),
                (SymbolKind::Enum, "inner::Level".to_owned()),
                (SymbolKind::Function, "inner::top".to_owned()),
            ]
        );

        // `impl Thing` names the scope the method is written in. Binding
        // that to `struct Thing` would be type resolution, so the method
        // has no structural parent.
        assert_eq!(find(&extraction, "inner::Thing::measure").parent, None);
        assert_ne!(
            find(&extraction, "inner::Thing::measure").qualified_name,
            find(&extraction, "inner::Shape::measure").qualified_name
        );
        assert_eq!(
            find(&extraction, "inner::Alias").visibility,
            Visibility::Internal,
            "pub(crate) is visible but not exported"
        );
        assert!(!find(&extraction, "inner::Alias").exported);
        assert!(find(&extraction, "inner::top").exported);
    }

    #[test]
    fn a_module_is_a_lexical_scope_and_never_a_synthetic_row() {
        for (dialect, source, scope) in [
            (ParserDialect::Rust, RUST, "inner"),
            (ParserDialect::CSharp, CSHARP, "Demo"),
        ] {
            let extraction = run(dialect, source);
            assert!(
                !extraction
                    .symbols
                    .iter()
                    .any(|symbol| symbol.qualified_name == scope),
                "{scope} must contribute a name, not a row"
            );
            assert!(
                extraction
                    .symbols
                    .iter()
                    .all(|symbol| symbol.qualified_name.starts_with(scope)),
                "but every declaration inside it carries that name"
            );
        }
    }

    #[test]
    fn local_bindings_are_never_persistent_symbols() {
        for (dialect, source) in [
            (ParserDialect::Python, PYTHON),
            (ParserDialect::TypeScript, TYPESCRIPT),
            (ParserDialect::Rust, RUST),
            (ParserDialect::CSharp, CSHARP),
        ] {
            let extraction = run(dialect, source);
            for local in ["local", "helper", "scratch"] {
                assert!(
                    !extraction.symbols.iter().any(|symbol| symbol.name == local),
                    "{dialect} stored the local {local:?}"
                );
            }
        }
    }

    #[test]
    fn a_span_is_the_exact_declaration_source() {
        let extraction = run(ParserDialect::Python, PYTHON);
        let method = find(&extraction, "Thing.method");
        let text = &PYTHON[method.span.start_byte..method.span.end_byte];

        assert!(text.starts_with("def method(self) -> int:"));
        assert!(text.ends_with("return inner()"));
        assert_eq!(
            PYTHON[..method.span.start_byte].lines().count() - 1,
            method.span.start.line,
            "the line/column endpoints agree with the byte offsets"
        );

        // And a declaration read back by span is the declaration itself.
        let class = find(&extraction, "Thing");
        assert!(PYTHON[class.span.start_byte..class.span.end_byte].starts_with("class Thing:"));
    }

    #[test]
    fn a_signature_is_a_declaration_header_never_a_body() {
        let extraction = run(ParserDialect::TypeScript, TYPESCRIPT);
        assert_eq!(
            find(&extraction, "Thing.measure").signature.as_deref(),
            Some("measure(): number")
        );
        assert_eq!(
            find(&extraction, "Thing").signature.as_deref(),
            // `export` sits outside the declaration node, exactly as it
            // sits outside the declaration's span; `exported` carries it.
            Some("class Thing implements Shape")
        );

        // Even a declaration whose initializer is enormous keeps a header.
        let huge = format!("export const DATA = \"{}\"\n", "x".repeat(10_000));
        let bulky = run(ParserDialect::TypeScript, &huge);
        let signature = find(&bulky, "DATA").signature.clone().expect("a header");
        assert_eq!(signature, "const DATA");
        assert!(signature.chars().count() <= SIGNATURE_MAX_CHARS + 1);

        for symbol in &extraction.symbols {
            let signature = symbol.signature.as_deref().unwrap_or_default();
            assert!(!signature.contains("return"), "{signature:?} holds a body");
            assert!(signature.chars().count() <= SIGNATURE_MAX_CHARS + 1);
        }
    }

    #[test]
    fn an_unchanged_reextraction_keeps_every_symbol_id() {
        let first = identify(&[], ParserDialect::TypeScript, TYPESCRIPT);
        let second = identify(&first, ParserDialect::TypeScript, TYPESCRIPT);

        assert_eq!(ids(&first), ids(&second));
        assert_eq!(
            first[0].parent_id, second[0].parent_id,
            "the hierarchy is carried too"
        );
    }

    #[test]
    fn a_body_only_edit_and_a_line_shift_keep_symbol_ids() {
        let first = identify(&[], ParserDialect::TypeScript, TYPESCRIPT);

        // Every declaration moves down three lines, and one body changes.
        let edited = format!(
            "// a new comment\n// and another\n\n{}",
            TYPESCRIPT.replace(
                "const scratch = this.value",
                "const scratch = this.value + 1"
            )
        );
        let second = identify(&first, ParserDialect::TypeScript, &edited);

        assert_eq!(
            ids(&first),
            ids(&second),
            "neither a body nor a line is identity"
        );
        assert_ne!(
            first
                .iter()
                .find(|symbol| symbol.qualified_name == "Thing.measure")
                .expect("measure")
                .span
                .start_byte,
            second
                .iter()
                .find(|symbol| symbol.qualified_name == "Thing.measure")
                .expect("measure")
                .span
                .start_byte,
            "the span really did move"
        );
    }

    #[test]
    fn a_rename_drops_the_old_symbol_without_claiming_continuity() {
        let first = identify(&[], ParserDialect::TypeScript, TYPESCRIPT);
        let renamed = TYPESCRIPT.replace("measure(): number {", "gauge(): number {");
        let second = identify(&first, ParserDialect::TypeScript, &renamed);

        let before = first
            .iter()
            .find(|symbol| symbol.qualified_name == "Thing.measure")
            .expect("the original method");
        let after = second
            .iter()
            .find(|symbol| symbol.qualified_name == "Thing.gauge")
            .expect("the renamed method");

        assert_ne!(before.id, after.id, "a rename is not evidence of identity");
        assert!(
            !second
                .iter()
                .any(|symbol| symbol.qualified_name == "Thing.measure"),
            "and the old declaration is simply gone from the replacement"
        );
        // Everything the rename did not touch is untouched.
        assert_eq!(
            first
                .iter()
                .find(|symbol| symbol.qualified_name == "Thing")
                .map(|symbol| symbol.id),
            second
                .iter()
                .find(|symbol| symbol.qualified_name == "Thing")
                .map(|symbol| symbol.id)
        );
    }

    #[test]
    fn overloads_are_kept_apart_by_signature_instead_of_merged() {
        let source = "\
public class Thing
{
    public int Measure(int a) { return a; }
    public int Measure(string a) { return 1; }
}
";
        let first = identify(&[], ParserDialect::CSharp, source);
        let overloads: Vec<&Symbol> = first
            .iter()
            .filter(|symbol| symbol.qualified_name == "Thing.Measure")
            .collect();
        assert_eq!(overloads.len(), 2);
        assert_ne!(overloads[0].id, overloads[1].id, "no false merge");

        // A body-only edit to one overload keeps both identities.
        let edited = source.replace("{ return a; }", "{ return a + 1; }");
        let second = identify(&first, ParserDialect::CSharp, &edited);
        assert_eq!(ids(&first), ids(&second));
    }

    #[test]
    fn indistinguishable_duplicates_get_new_ids_rather_than_a_false_merge() {
        let source = "\
public class Thing
{
    public int Measure(int a) { return 1; }
    public int Measure(int a) { return 2; }
}
";
        let first = identify(&[], ParserDialect::CSharp, source);
        let second = identify(&first, ParserDialect::CSharp, source);

        let before: Vec<SymbolId> = first
            .iter()
            .filter(|symbol| symbol.qualified_name == "Thing.Measure")
            .map(|symbol| symbol.id)
            .collect();
        let after: Vec<SymbolId> = second
            .iter()
            .filter(|symbol| symbol.qualified_name == "Thing.Measure")
            .map(|symbol| symbol.id)
            .collect();

        assert_eq!(before.len(), 2);
        assert_eq!(after.len(), 2);
        for id in &after {
            assert!(
                !before.contains(id),
                "nothing distinguishes these two, so neither inherits an identity"
            );
        }
        // The enclosing class is still unambiguous, so it keeps its id.
        assert_eq!(
            first
                .iter()
                .find(|symbol| symbol.qualified_name == "Thing")
                .map(|symbol| symbol.id),
            second
                .iter()
                .find(|symbol| symbol.qualified_name == "Thing")
                .map(|symbol| symbol.id)
        );
    }

    #[test]
    fn one_previous_identity_is_never_handed_to_two_candidates() {
        let first = identify(&[], ParserDialect::TypeScript, TYPESCRIPT);
        let second = identify(&first, ParserDialect::TypeScript, TYPESCRIPT);

        let mut seen: Vec<SymbolId> = Vec::new();
        for symbol in &second {
            assert!(!seen.contains(&symbol.id), "{} reused an id", symbol.name);
            seen.push(symbol.id);
        }
    }

    #[test]
    fn a_partial_parse_is_extracted_but_never_accepted() {
        let broken = "\
export class Thing {
  measure(): number {
    return
}

export function top(
";
        let extraction = run(ParserDialect::TypeScript, broken);

        assert_eq!(extraction.status, ExtractionStatus::Partial);
        assert!(
            !extraction.is_accepted(),
            "a broken keystroke must not be allowed to blank a file's symbols"
        );
        assert!(
            !extraction.symbols.is_empty(),
            "what is still structurally clear is still extracted"
        );
    }

    /// Every occurrence of `kind`, as `(text, containing qualified_name)`.
    fn evidence(
        extraction: &Extraction,
        source: &str,
        kind: OccurrenceKind,
    ) -> Vec<(String, Option<String>)> {
        extraction
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.kind == kind)
            .map(|occurrence| {
                (
                    source[occurrence.span.start_byte..occurrence.span.end_byte].to_owned(),
                    occurrence
                        .containing
                        .map(|index| extraction.symbols[index].qualified_name.clone()),
                )
            })
            .collect()
    }

    #[test]
    fn every_declaration_is_evidence_of_itself_at_its_name_token() {
        for (dialect, source) in [
            (ParserDialect::Python, PYTHON),
            (ParserDialect::TypeScript, TYPESCRIPT),
            (ParserDialect::Rust, RUST),
            (ParserDialect::CSharp, CSHARP),
        ] {
            let extraction = run(dialect, source);
            let definitions: Vec<(String, Option<String>)> =
                evidence(&extraction, source, OccurrenceKind::Definition);

            assert_eq!(
                definitions.len(),
                extraction.symbols.len(),
                "{dialect}: one DEFINITION per declaration"
            );
            for symbol in &extraction.symbols {
                let (text, containing) = definitions
                    .iter()
                    .find(|(_, containing)| {
                        containing.as_deref() == Some(symbol.qualified_name.as_str())
                    })
                    .unwrap_or_else(|| panic!("{dialect}: {} has no DEFINITION", symbol.name));
                assert_eq!(
                    text, &symbol.name,
                    "{dialect}: the span must be the name token, not the declaration"
                );
                assert_eq!(containing.as_deref(), Some(symbol.qualified_name.as_str()));
            }
        }
    }

    #[test]
    fn a_definition_span_is_narrower_than_its_declaration() {
        let extraction = run(ParserDialect::Python, PYTHON);
        let method = find(&extraction, "Thing.method");
        let definition = extraction
            .occurrences
            .iter()
            .find(|occurrence| {
                occurrence.kind == OccurrenceKind::Definition
                    && occurrence.containing
                        == Some(
                            extraction
                                .symbols
                                .iter()
                                .position(|symbol| symbol.qualified_name == "Thing.method")
                                .expect("method"),
                        )
            })
            .expect("a definition");

        assert_eq!(
            &PYTHON[definition.span.start_byte..definition.span.end_byte],
            "method"
        );
        assert!(definition.span.start_byte > method.span.start_byte);
        assert!(definition.span.end_byte < method.span.end_byte);
        assert_eq!(definition.span.start.line, method.span.start.line);
        assert!(definition.span.start.column > method.span.start.column);
    }

    #[test]
    fn imports_are_evidence_of_what_the_statement_names() {
        let python = "import os\nfrom p.q import r, s as t\n";
        assert_eq!(
            evidence(
                &run(ParserDialect::Python, python),
                python,
                OccurrenceKind::ImportSite
            ),
            vec![
                ("os".to_owned(), None),
                ("p.q".to_owned(), None),
                ("r".to_owned(), None),
                // `s as t` imports s; the alias is a local binding.
                ("s".to_owned(), None),
            ]
        );

        let typescript = "import { a, b as c } from './m'\n";
        assert_eq!(
            evidence(
                &run(ParserDialect::TypeScript, typescript),
                typescript,
                OccurrenceKind::ImportSite
            ),
            vec![
                ("a".to_owned(), None),
                ("b".to_owned(), None),
                ("'./m'".to_owned(), None),
            ]
        );

        let csharp = "using System;\nusing Alias = System.Text;\n";
        assert_eq!(
            evidence(
                &run(ParserDialect::CSharp, csharp),
                csharp,
                OccurrenceKind::ImportSite
            ),
            vec![
                ("System".to_owned(), None),
                ("System.Text".to_owned(), None),
            ]
        );

        let rust = "use std::collections::HashMap;\n";
        assert_eq!(
            evidence(
                &run(ParserDialect::Rust, rust),
                rust,
                OccurrenceKind::ImportSite
            ),
            vec![("std::collections::HashMap".to_owned(), None)]
        );
    }

    #[test]
    fn a_call_site_records_its_callee_and_its_smallest_enclosing_symbol() {
        let source = "\
def free():
    return helper()


class Thing:
    def method(self):
        def nested():
            return deep()

        return nested()


outer()
";
        let extraction = run(ParserDialect::Python, source);

        assert_eq!(
            evidence(&extraction, source, OccurrenceKind::CallSite),
            vec![
                ("helper".to_owned(), Some("free".to_owned())),
                // The innermost function owns the call, not its method.
                ("deep".to_owned(), Some("Thing.method.nested".to_owned())),
                ("nested".to_owned(), Some("Thing.method".to_owned())),
                // Nothing lexically contains a file-level call.
                ("outer".to_owned(), None),
            ]
        );
    }

    #[test]
    fn a_call_site_resolves_nothing() {
        // Two calls written the same way, reaching different things, are
        // indistinguishable here -- which is the point: this is evidence
        // that a call is written, not a resolved target.
        let source = "\
import lib

def one():
    return lib.run()

def two():
    return lib.run()
";
        let extraction = run(ParserDialect::Python, source);
        let calls = evidence(&extraction, source, OccurrenceKind::CallSite);

        assert_eq!(
            calls,
            vec![
                ("lib.run".to_owned(), Some("one".to_owned())),
                ("lib.run".to_owned(), Some("two".to_owned())),
            ]
        );
    }

    #[test]
    fn a_container_dialect_invents_no_evidence() {
        let source = "<script lang=\"ts\">\n  export function go() { run() }\n</script>\n";
        let extraction = run(ParserDialect::Svelte, source);

        assert_eq!(extraction.status, ExtractionStatus::ContainerOnly);
        assert!(
            extraction.occurrences.is_empty(),
            "an unread embedded script yields no occurrences, true or false"
        );
    }

    #[test]
    fn svelte_is_container_only_and_not_an_empty_success() {
        let source =
            "<script lang=\"ts\">\n  export let label: string;\n</script>\n<p>{label}</p>\n";
        let extraction = run(ParserDialect::Svelte, source);

        assert_eq!(extraction.status, ExtractionStatus::ContainerOnly);
        assert!(
            !extraction.is_accepted(),
            "an unextracted container must never read as a component that declares nothing"
        );
        assert!(extraction.symbols.is_empty());
        assert_eq!(
            extraction.profile.capability_fingerprint,
            "container:TYPESCRIPT,JAVASCRIPT"
        );
    }
}
