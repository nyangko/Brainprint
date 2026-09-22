//! What a `.svelte` container declares, in its own coordinates.
//!
//! #19 task 11 needed exactly two things I3 did not have, and this adds
//! exactly those two:
//!
//! * **Script declarations.** A template reference has to resolve *to*
//!   something, and before this a `.svelte` file declared no Symbol at
//!   all -- so `{count}` had no possible target but the file itself,
//!   which is not an answer.
//! * **Template use sites.** `{increment}` and `<Child …>` are uses
//!   written in the original source, and a semantic answer has to anchor
//!   on an Occurrence that already exists (#19 task 4 refuses to invent
//!   one).
//!
//! Everything else about Svelte is left alone. This does not model
//! reactivity, does not resolve anything, and does not try to understand
//! Svelte expressions: it finds the embedded script, hands it to the
//! JavaScript/TypeScript extractor that already exists, and shifts the
//! result back into `.svelte` byte offsets.
//!
//! ## Why the offsets are recomputed rather than added
//!
//! A span carries a line and a **byte column within that line**, so a
//! flat `+offset` on the byte range would leave the points describing
//! the script fragment rather than the component. Both ends are
//! recomputed against the original bytes instead, which is also what
//! makes "every Agent-facing span is a current `.svelte` span" checkable
//! rather than hoped for.
//!
//! ## Which template names become evidence
//!
//! Only names the script block binds. That rule is doing real work:
//! `{item}` introduced by `{#each items as item}` is a local the graph
//! has no Symbol model for, `<button>` is a DOM element, and neither
//! becomes a use site -- while `{count}`, `{increment}` and `<Child>`
//! do, because the script declares or imports them. It is binding
//! evidence from the same file, not a naming convention, and it is why
//! there is no "capitalised means component" rule anywhere here.

use std::collections::BTreeSet;

use tree_sitter::Node;

use crate::{
    extract::{ExtractedOccurrence, ExtractedSymbol, Extraction, ExtractionStatus, extract},
    parser::{
        ParseStatus, ParseTree, ParserDialect, ParserRegistry, SourceBasis, SourcePoint, SourceSpan,
    },
    symbol::{AnalysisProfile, OccurrenceKind},
};

/// Extract a Svelte component: its embedded script, and the template
/// uses of what that script binds.
///
/// Returns an [`Extraction`] in the *container's* dialect and profile,
/// because that is what the Resource is. A script that does not parse
/// makes the whole extraction [`ExtractionStatus::Partial`], so a broken
/// keystroke cannot blank a component's Symbol set.
#[must_use]
pub fn extract_component(tree: &ParseTree, source: &[u8], profile: AnalysisProfile) -> Extraction {
    let root = tree.syntax_tree().root_node();
    let mut registry = ParserRegistry::new();
    let mut symbols: Vec<ExtractedSymbol> = Vec::new();
    let mut occurrences: Vec<ExtractedOccurrence> = Vec::new();
    let mut bound: BTreeSet<String> = BTreeSet::new();
    let mut status = match tree.status() {
        ParseStatus::Complete => ExtractionStatus::Complete,
        ParseStatus::Partial => ExtractionStatus::Partial,
    };

    for region in script_regions(root, source) {
        let Ok(inner) = registry.parse(
            region.dialect,
            region.bytes(source),
            SourceBasis {
                path_rel: None,
                resource_revision: None,
                content_hash: None,
            },
        ) else {
            // A script this build cannot parse at all is a partial
            // component, never a component that declares nothing.
            status = ExtractionStatus::Partial;
            continue;
        };
        // A *default* import binds a name the JS/TS extractor records no
        // occurrence for -- `import Child from './lib/Child.svelte'`
        // leaves only the specifier. In a plain module that costs
        // nothing; in a component it is the name the markup writes, so
        // the binding is read straight off the import clause. Only the
        // name enters scope: no occurrence is invented for it, and the
        // specifier keeps the one it already has.
        bound.extend(default_import_names(
            inner.syntax_tree().root_node(),
            region.bytes(source),
        ));
        let produced = extract(&inner, region.bytes(source));
        if produced.status != ExtractionStatus::Complete {
            status = ExtractionStatus::Partial;
        }

        let base = symbols.len();
        for mut symbol in produced.symbols {
            symbol.span = shift(symbol.span, region.offset, source);
            symbol.parent = symbol.parent.map(|index| index + base);
            if symbol.parent.is_none() {
                // Only a top-level binding is in template scope. A class
                // member is not a name the markup can write.
                bound.insert(symbol.name.clone());
            }
            symbols.push(symbol);
        }
        for mut occurrence in produced.occurrences {
            occurrence.span = shift(occurrence.span, region.offset, source);
            occurrence.containing = occurrence.containing.map(|index| index + base);
            if occurrence.kind == OccurrenceKind::ImportSite
                && let Some(name) = identifier_text(source, occurrence.span)
            {
                // `import Child from './lib/Child.svelte'` binds `Child`
                // for the markup below. The specifier itself is a quoted
                // string and never matches.
                bound.insert(name);
            }
            occurrences.push(occurrence);
        }
    }

    for span in template_uses(root, source, &bound, &mut registry) {
        occurrences.push(ExtractedOccurrence {
            kind: OccurrenceKind::ReferenceSite,
            span,
            // A template use sits outside every script declaration. The
            // component Resource owns it, which is what an endpoint of
            // `GraphEndpoint::Resource` means.
            containing: None,
        });
    }

    occurrences.sort_by(|left, right| {
        (left.span.start_byte, left.span.end_byte, left.kind.as_str()).cmp(&(
            right.span.start_byte,
            right.span.end_byte,
            right.kind.as_str(),
        ))
    });
    occurrences.dedup_by(|left, right| {
        left.kind == right.kind
            && left.span.start_byte == right.span.start_byte
            && left.span.end_byte == right.span.end_byte
    });

    Extraction {
        status,
        dialect: ParserDialect::Svelte,
        profile,
        symbols,
        occurrences,
    }
}

// ---------------------------------------------------------------------
// The embedded script
// ---------------------------------------------------------------------

/// One `<script>` block, as bytes and where they start.
struct ScriptRegion {
    offset: usize,
    end: usize,
    dialect: ParserDialect,
}

impl ScriptRegion {
    fn bytes<'a>(&self, source: &'a [u8]) -> &'a [u8] {
        &source[self.offset..self.end]
    }
}

/// Every `<script>` block in the component, in source order.
///
/// Both of them, when a component has both: `<script module>` and the
/// instance script are two scopes, and flattening them by name is
/// exactly what this must not do. They are extracted separately and
/// their declarations keep their own spans.
fn script_regions(root: Node<'_>, source: &[u8]) -> Vec<ScriptRegion> {
    let mut found = Vec::new();
    walk(root, &mut |node| {
        if node.kind() != "script_element" {
            return;
        }
        let dialect = script_dialect(node, source);
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "raw_text" {
                found.push(ScriptRegion {
                    offset: child.start_byte(),
                    end: child.end_byte(),
                    dialect,
                });
            }
        }
    });
    found
}

/// `lang="ts"` means TypeScript; anything else, including no attribute
/// at all, means JavaScript.
///
/// Read from the attribute rather than guessed from the file, because a
/// component may write a plain `<script>` next to a `<script lang="ts">`
/// and the two are different grammars.
fn script_dialect(script: Node<'_>, source: &[u8]) -> ParserDialect {
    let mut dialect = ParserDialect::JavaScript;
    walk(script, &mut |node| {
        if node.kind() != "attribute" {
            return;
        }
        let mut name = None;
        let mut value = None;
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "attribute_name" => name = text_of(source, child),
                "quoted_attribute_value" | "attribute_value" => {
                    let mut inner = child.walk();
                    value = child
                        .children(&mut inner)
                        .find(|part| part.kind() == "attribute_value")
                        .and_then(|part| text_of(source, part))
                        .or_else(|| text_of(source, child));
                }
                _ => {}
            }
        }
        if name.as_deref() == Some("lang")
            && matches!(value.as_deref(), Some("ts") | Some("\"ts\"") | Some("'ts'"))
        {
            dialect = ParserDialect::TypeScript;
        }
    });
    dialect
}

/// The local names a script's default and namespace imports bind.
///
/// `import Child from './lib/Child.svelte'` and
/// `import * as helpers from './helpers'`. A named import
/// (`import { x }`) already has its own occurrence and is picked up from
/// that, so it is deliberately absent here.
fn default_import_names(root: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    walk(root, &mut |node| {
        if node.kind() != "import_clause" {
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "identifier" => found.extend(text_of(source, child)),
                "namespace_import" => {
                    let mut inner = child.walk();
                    for part in child.children(&mut inner) {
                        if part.kind() == "identifier" {
                            found.extend(text_of(source, part));
                        }
                    }
                }
                _ => {}
            }
        }
    });
    found
}

// ---------------------------------------------------------------------
// The template
// ---------------------------------------------------------------------

/// Every template site that uses a name the script bound.
///
/// Two shapes, and nothing else: an identifier inside a `{…}` expression
/// and a component tag name. Both are spans of the original `.svelte`
/// source.
fn template_uses(
    root: Node<'_>,
    source: &[u8],
    bound: &BTreeSet<String>,
    registry: &mut ParserRegistry,
) -> Vec<SourceSpan> {
    let mut found = Vec::new();
    walk(root, &mut |node| match node.kind() {
        // `{count}`, `onclick={increment}`, `{#if count > 0}` -- the
        // grammar hands the expression back as raw text, which is
        // JavaScript, so the JavaScript grammar is what reads it.
        "svelte_raw_text" => {
            found.extend(expression_identifiers(node, source, bound, registry));
        }
        // `<Child …>` and `</Child>`. A tag the script did not bind is a
        // DOM element and is not evidence about anything.
        "tag_name" => {
            if let Some(name) = text_of(source, node)
                && bound.contains(&name)
            {
                found.push(span_at(source, node.start_byte(), node.end_byte()));
            }
        }
        _ => {}
    });
    found
}

/// The identifiers a template expression reads, as original spans.
///
/// Parsed with the JavaScript grammar rather than scanned for word
/// characters: `model.name` must contribute `model` and not `name`, and
/// `{ name: 'js' }` must contribute neither. `property_identifier` is a
/// different node kind in that grammar, so a plain `identifier` is
/// already only a value position.
fn expression_identifiers(
    node: Node<'_>,
    source: &[u8],
    bound: &BTreeSet<String>,
    registry: &mut ParserRegistry,
) -> Vec<SourceSpan> {
    let offset = node.start_byte();
    let bytes = &source[offset..node.end_byte()];
    // An expression fragment is not a statement, and the grammar wants
    // one. Wrapping keeps the offsets predictable: the prefix is a fixed
    // width this subtracts back off.
    const PREFIX: &str = "(";
    let mut wrapped = Vec::with_capacity(bytes.len() + 2);
    wrapped.extend_from_slice(PREFIX.as_bytes());
    wrapped.extend_from_slice(bytes);
    wrapped.push(b')');
    let Ok(tree) = registry.parse(
        ParserDialect::JavaScript,
        &wrapped,
        SourceBasis {
            path_rel: None,
            resource_revision: None,
            content_hash: None,
        },
    ) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    walk(tree.syntax_tree().root_node(), &mut |inner| {
        if inner.kind() != "identifier" {
            return;
        }
        let Some(name) = text_of(&wrapped, inner) else {
            return;
        };
        if !bound.contains(&name) {
            // A local the markup introduced (`{#each items as item}`) or
            // a global. Neither has a Symbol, and inventing one with
            // borrowed coordinates is the failure this refuses.
            return;
        }
        let start = offset + inner.start_byte() - PREFIX.len();
        let end = offset + inner.end_byte() - PREFIX.len();
        found.push(span_at(source, start, end));
    });
    found
}

// ---------------------------------------------------------------------
// Spans
// ---------------------------------------------------------------------

/// Move a span from script-fragment coordinates into component ones.
fn shift(span: SourceSpan, offset: usize, source: &[u8]) -> SourceSpan {
    span_at(source, span.start_byte + offset, span.end_byte + offset)
}

/// A span over `source`, with both points recomputed from the bytes.
fn span_at(source: &[u8], start: usize, end: usize) -> SourceSpan {
    SourceSpan {
        start_byte: start,
        end_byte: end,
        start: point_at(source, start),
        end: point_at(source, end),
    }
}

/// The zero-based line, and byte column within it, of one byte offset.
fn point_at(source: &[u8], byte: usize) -> SourcePoint {
    let byte = byte.min(source.len());
    let line = source[..byte].iter().filter(|&&b| b == b'\n').count();
    let line_start = source[..byte]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |index| index + 1);
    SourcePoint::new(line, byte - line_start)
}

fn text_of(source: &[u8], node: Node<'_>) -> Option<String> {
    std::str::from_utf8(source.get(node.start_byte()..node.end_byte())?)
        .ok()
        .map(str::to_owned)
}

fn identifier_text(source: &[u8], span: SourceSpan) -> Option<String> {
    let text = std::str::from_utf8(source.get(span.start_byte..span.end_byte)?).ok()?;
    text.chars()
        .all(|character| character.is_alphanumeric() || character == '_' || character == '$')
        .then(|| text.to_owned())
}

fn walk(node: Node<'_>, visit: &mut dyn FnMut(Node<'_>)) {
    visit(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, visit);
    }
}
