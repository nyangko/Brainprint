//! Brainprint delivery observation (#26 "Delivery observation").
//!
//! Reads the Task 12 envelope a client reports for one of the four
//! Brainprint tools, decodes its `payload` as the typed Task 11
//! `QueryResultWire` (the exact type `brainprint-mcp` serialized -- no
//! second interpretation), and keeps only compact descriptors. The
//! payload itself -- including any source body -- is dropped here and
//! never stored.

use brainprint_core::protocol::query::{
    CurrentnessWire, EvidenceWire, FindResultWire, ProjectedAnswerWire, QueryResultWire,
    QueryStatusWire, TargetResolutionWire,
};
use serde_json::Value;

use crate::{
    event::{BrainprintTool, SearchPattern},
    util,
};

/// `brainprint.find` `files` default, mirroring `brainprint-mcp`.
const DEFAULT_FILES_LIMIT: usize = 100;
const MAX_ENVELOPE_DEPTH: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// A current/complete answer (first-route: `brainprint_first`), as
    /// opposed to a gap (partial/stale/ambiguous/truncated/error).
    pub complete: bool,
    /// `Some(path)` when the request named `workspace_path` explicitly;
    /// `None` when it relied on the MCP server's startup directory.
    /// Requests by `workspace_id` never yield facts.
    pub workspace_path: Option<String>,
    pub facts: Vec<DeliveredFact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveredFact {
    /// A verified-current source range; `lines` are the 0-based,
    /// end-exclusive lines it covered completely.
    Source {
        resource_id: String,
        path_rel: String,
        revision: String,
        lines: (usize, usize),
    },
    /// A complete/current unfiltered recursive listing.
    Listing {
        directory: Option<String>,
        limit: usize,
        fingerprint: String,
    },
    /// A complete text search over current bytes of indexed Resources.
    Search {
        pattern: SearchPattern,
        case_insensitive: bool,
        path_prefix: Option<String>,
        match_resource_ids: Vec<String>,
    },
}

/// `None` when the client's result carries no recognizable Task 12
/// envelope -- then nothing is claimed delivered.
pub fn observe(tool: BrainprintTool, input: &Value, result: &Value) -> Option<Delivery> {
    let envelope = find_envelope(result, 0)?;
    let workspace_path = input
        .get("workspace_path")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let by_id = input.get("workspace_id").is_some_and(|id| !id.is_null());

    let mut delivery = Delivery {
        complete: false,
        workspace_path,
        facts: Vec::new(),
    };
    if envelope.get("outcome").and_then(Value::as_str) != Some("ok") {
        return Some(delivery);
    }
    let mode = envelope
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if tool == BrainprintTool::Context && mode == "status" {
        delivery.complete = true;
        return Some(delivery);
    }
    let Some(payload) = envelope.get("payload") else {
        return Some(delivery);
    };
    let Ok(result) = serde_json::from_value::<QueryResultWire>(payload.clone()) else {
        return Some(delivery);
    };

    delivery.complete = is_complete(&result);
    if !by_id {
        delivery.facts = facts(&result, input);
    }
    Some(delivery)
}

/// Accept the envelope as a JSON object, or as JSON text inside any
/// client wrapper (content blocks, `llmContent`, ...).
fn find_envelope(value: &Value, depth: usize) -> Option<Value> {
    if depth > MAX_ENVELOPE_DEPTH {
        return None;
    }
    match value {
        Value::Object(map) => {
            let is_envelope = map
                .get("tool")
                .and_then(Value::as_str)
                .is_some_and(|tool| tool.starts_with("brainprint."))
                && map.contains_key("outcome")
                && map.contains_key("payload");
            if is_envelope {
                return Some(value.clone());
            }
            map.values()
                .find_map(|inner| find_envelope(inner, depth + 1))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|inner| find_envelope(inner, depth + 1)),
        Value::String(text) => {
            let trimmed = text.trim();
            if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
                return None;
            }
            serde_json::from_str::<Value>(trimmed)
                .ok()
                .and_then(|parsed| find_envelope(&parsed, depth + 1))
        }
        _ => None,
    }
}

fn projected_complete(answer: &ProjectedAnswerWire) -> bool {
    answer.currentness == CurrentnessWire::Current
        && !answer.more_available
        && answer.page.gaps.is_empty()
        && matches!(
            answer.target_resolution,
            TargetResolutionWire::Resolved(_) | TargetResolutionWire::NoTarget
        )
}

fn is_complete(result: &QueryResultWire) -> bool {
    match result {
        QueryResultWire::Find(FindResultWire::Files(listing)) => {
            !listing.truncated && listing.currentness == CurrentnessWire::Current
        }
        QueryResultWire::Find(FindResultWire::Text(text)) => text_complete(text),
        QueryResultWire::Find(FindResultWire::Target(answer))
        | QueryResultWire::Inspect(answer)
        | QueryResultWire::Impact(answer)
        | QueryResultWire::Context(answer) => projected_complete(answer),
        QueryResultWire::Relations(relations) => {
            relations.currentness == CurrentnessWire::Current
                && matches!(relations.target, TargetResolutionWire::Resolved(_))
        }
        QueryResultWire::Structure(summary) => summary.currentness == CurrentnessWire::Current,
        QueryResultWire::Knowledge(_) => true,
    }
}

fn text_complete(text: &brainprint_core::protocol::query::TextSearchResultWire) -> bool {
    matches!(text.status, QueryStatusWire::Found | QueryStatusWire::NotFound)
        && text.structural_currentness == CurrentnessWire::Current
        && text.scope.budget_exhausted.is_none()
        && text.scope.oversized_skipped.is_empty()
        && text.scope.unreadable.is_empty()
        && text.scope.changed_during_scan.is_empty()
        // A native search does not share Brainprint's binary detection;
        // any skipped binary makes the two answers not provably equal.
        && text.scope.binary_skipped.is_empty()
}

fn facts(result: &QueryResultWire, input: &Value) -> Vec<DeliveredFact> {
    match result {
        QueryResultWire::Find(FindResultWire::Files(listing)) => {
            let unfiltered = input.get("recursive").and_then(Value::as_bool) == Some(true)
                && ["path_prefix", "role", "language", "kind"]
                    .iter()
                    .all(|key| input.get(*key).is_none_or(Value::is_null));
            if !unfiltered
                || listing.truncated
                || listing.currentness != CurrentnessWire::Current
                || listing.entries.is_empty()
            {
                return Vec::new();
            }
            let limit = input
                .get("limit")
                .and_then(Value::as_u64)
                .and_then(|limit| usize::try_from(limit).ok())
                .unwrap_or(DEFAULT_FILES_LIMIT);
            let ids: Vec<String> = listing
                .entries
                .iter()
                .map(|entry| entry.id.to_string())
                .collect();
            let fingerprint =
                util::fingerprint(listing.entries.iter().zip(&ids).flat_map(|(entry, id)| {
                    [
                        id.as_str(),
                        entry.path_rel.as_str(),
                        entry.resource_revision.as_str(),
                    ]
                }));
            vec![DeliveredFact::Listing {
                directory: normalize_directory(input.get("directory").and_then(Value::as_str)),
                limit,
                fingerprint,
            }]
        }
        QueryResultWire::Find(FindResultWire::Text(text)) => {
            let Some(pattern_text) = input.get("pattern").and_then(Value::as_str) else {
                return Vec::new();
            };
            let regex = input.get("regex").and_then(Value::as_bool).unwrap_or(false);
            if !text_complete(text) || (regex && regex_may_span_lines(pattern_text)) {
                return Vec::new();
            }
            let Some(match_resource_ids) = text
                .matches
                .iter()
                .map(|hit| hit.resource_id.map(|id| id.to_string()))
                .collect::<Option<Vec<_>>>()
            else {
                // A match on bytes the index does not describe: not
                // tied to a current revision.
                return Vec::new();
            };
            vec![DeliveredFact::Search {
                pattern: if regex {
                    SearchPattern::Regex(pattern_text.to_owned())
                } else {
                    SearchPattern::Literal(pattern_text.to_owned())
                },
                case_insensitive: input
                    .get("case_insensitive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                path_prefix: input
                    .get("path_prefix")
                    .and_then(Value::as_str)
                    .filter(|prefix| !prefix.is_empty())
                    .map(str::to_owned),
                match_resource_ids,
            }]
        }
        QueryResultWire::Find(FindResultWire::Target(answer))
        | QueryResultWire::Inspect(answer)
        | QueryResultWire::Impact(answer)
        | QueryResultWire::Context(answer) => source_facts(answer),
        _ => Vec::new(),
    }
}

fn source_facts(answer: &ProjectedAnswerWire) -> Vec<DeliveredFact> {
    if answer.currentness != CurrentnessWire::Current {
        return Vec::new();
    }
    answer
        .page
        .evidence
        .iter()
        .zip(
            answer
                .page
                .references
                .iter()
                .map(Option::is_some)
                .chain(std::iter::repeat(true)),
        )
        .filter_map(|(evidence, by_reference)| match evidence {
            // A reference means the body was *not* in this response.
            EvidenceWire::CurrentSource(range)
                if !by_reference && range.verification.currentness == CurrentnessWire::Current =>
            {
                fully_covered_lines(range.span.start, range.span.end).map(|lines| {
                    DeliveredFact::Source {
                        resource_id: range.resource.to_string(),
                        path_rel: range.path_rel.clone(),
                        revision: range.resource_revision.clone(),
                        lines,
                    }
                })
            }
            _ => None,
        })
        .collect()
}

/// Lines a span covers *entirely* (0-based, end-exclusive). A span that
/// starts mid-line leaves that line partial; the end point is exclusive,
/// so line `end.line` is never fully covered (and when `end.column == 0`
/// it is not covered at all).
pub fn fully_covered_lines(
    start: brainprint_core::protocol::query::SourcePointWire,
    end: brainprint_core::protocol::query::SourcePointWire,
) -> Option<(usize, usize)> {
    let first = if start.column == 0 {
        start.line
    } else {
        start.line + 1
    };
    (first < end.line).then_some((first, end.line))
}

pub fn normalize_directory(directory: Option<&str>) -> Option<String> {
    directory
        .map(|directory| directory.trim_end_matches('/'))
        .filter(|directory| !directory.is_empty())
        .map(str::to_owned)
}

/// Native searches are line-oriented; Brainprint's matcher is not. A
/// regex that could match across a line break has no proven native
/// equivalent, so it never becomes a suppression basis.
pub fn regex_may_span_lines(pattern: &str) -> bool {
    [
        "\\s", "\\S", "\\W", "\\D", "\\n", "\\r", "\\p", "\\P", "\\x", "\\u", "\\0", "[^", "[[:",
        "(?",
    ]
    .iter()
    .any(|construct| pattern.contains(construct))
        || pattern.contains('\n')
}

#[cfg(test)]
mod tests {
    use brainprint_core::protocol::query::SourcePointWire;

    use super::*;

    fn point(line: usize, column: usize) -> SourcePointWire {
        SourcePointWire { line, column }
    }

    #[test]
    fn only_whole_lines_count_as_covered() {
        // Declaration at col 0 through the start of line 20.
        assert_eq!(
            fully_covered_lines(point(10, 0), point(20, 0)),
            Some((10, 20))
        );
        // Starts mid-line, ends mid-line: both edge lines are partial.
        assert_eq!(
            fully_covered_lines(point(10, 4), point(20, 1)),
            Some((11, 20))
        );
        // A one-line span covers no whole line.
        assert_eq!(fully_covered_lines(point(3, 0), point(3, 9)), None);
    }

    #[test]
    fn envelope_is_found_inside_text_content_blocks() {
        let envelope = serde_json::json!({
            "tool": "brainprint.find", "mode": "files", "outcome": "transport_error",
            "payload": {"message": "down"}
        });
        let wrapped = serde_json::json!([{"type": "text", "text": envelope.to_string()}]);
        let delivery = observe(BrainprintTool::Find, &serde_json::json!({}), &wrapped)
            .expect("envelope inside a text block is recognized");
        assert!(!delivery.complete);
        assert!(delivery.facts.is_empty());
        assert_eq!(
            observe(
                BrainprintTool::Find,
                &serde_json::json!({}),
                &serde_json::json!("plain text")
            ),
            None
        );
    }

    #[test]
    fn multi_line_capable_regexes_are_rejected() {
        assert!(regex_may_span_lines("foo\\s+bar"));
        assert!(regex_may_span_lines("[^x]"));
        assert!(!regex_may_span_lines("fn \\w+_budget"));
    }
}
