//! Compact and `--json` output rendering (#24 §20, §21).
//!
//! Every writer takes an explicit `impl Write` and returns `io::Result`
//! rather than using `println!` (which panics on a write failure): #24
//! §20/§23 require the CLI to acknowledge only after output write/flush
//! actually succeeds, so a broken pipe must be a normal `Err`, not a
//! panic that skips the "no ack on output failure" rule by accident.

use std::io::Write;

use brainprint_core::protocol::query::*;

use super::delivery::encode_continuation;

/// `--json`: exactly one `QueryResponse` JSON on stdout, nothing else.
pub fn print_json(response: &QueryResponse) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, response)?;
    writeln!(stdout)?;
    stdout.flush()
}

fn marker(out: &mut impl Write, line: &str) -> std::io::Result<()> {
    writeln!(out, "{line}")
}

fn print_target_resolution(
    out: &mut impl Write,
    resolution: &TargetResolutionWire,
) -> std::io::Result<()> {
    match resolution {
        TargetResolutionWire::Resolved(endpoint) => writeln!(out, "target: {endpoint:?}"),
        TargetResolutionWire::MultipleCandidates => marker(out, "AMBIGUOUS"),
        TargetResolutionWire::SingleNonExactCandidate => marker(out, "SINGLE_NON_EXACT_CANDIDATE"),
        TargetResolutionWire::NotFound => marker(out, "NOT_FOUND"),
        TargetResolutionWire::NotFoundIncompleteCoverage => marker(out, "NOT_FOUND_INCOMPLETE"),
        TargetResolutionWire::NotCurrent => marker(out, "NOT_CURRENT"),
        TargetResolutionWire::NoTarget => Ok(()),
    }
}

fn print_currentness(out: &mut impl Write, currentness: &CurrentnessWire) -> std::io::Result<()> {
    if matches!(currentness, CurrentnessWire::NotCurrent(_)) {
        marker(out, "NOT_CURRENT")?;
    }
    Ok(())
}

/// #24 §21: every delivered `CurrentSource` is printed with its identity,
/// span and exact source text -- never a location-only replacement.
fn print_current_source(out: &mut impl Write, range: &PreparedRangeWire) -> std::io::Result<()> {
    writeln!(
        out,
        "--- {} ({}) [{}:{}-{}:{}] ---",
        range.path_rel,
        range.resource,
        range.span.start.line,
        range.span.start.column,
        range.span.end.line,
        range.span.end.column,
    )?;
    writeln!(out, "{}", range.source)
}

fn print_evidence(out: &mut impl Write, items: &[EvidenceWire]) -> std::io::Result<()> {
    let mut source_unavailable = false;
    for item in items {
        match item {
            EvidenceWire::CurrentSource(range) => print_current_source(out, range)?,
            EvidenceWire::SourceUnavailable { .. } => source_unavailable = true,
            // A full bespoke one-line format per evidence variant is a
            // larger surface than this compact renderer covers; the debug
            // form is lossless (every field survives) even though it is
            // not hand-formatted prose. ponytail: compact rendering for
            // non-source evidence is Debug-based; add per-variant
            // formatting if a human compact reader needs it later.
            other => writeln!(out, "{other:?}")?,
        }
    }
    if source_unavailable {
        marker(out, "SOURCE_UNAVAILABLE")?;
    }
    Ok(())
}

fn print_delivery_tail(
    out: &mut impl Write,
    page: &DeliveryPageWire,
    economy: &DeliveryEconomyWire,
    continuation: &Option<DeliveryContinuationWire>,
) -> std::io::Result<()> {
    if !economy.limiting.is_empty() || economy.more_available {
        marker(out, "PARTIAL")?;
    }
    if !page.gaps.is_empty() {
        marker(out, "TRUNCATED")?;
    }
    if economy.more_available {
        marker(out, "MORE_AVAILABLE")?;
        if let Some(continuation) = continuation {
            writeln!(out, "CONTINUATION {}", encode_continuation(continuation))?;
        }
    }
    Ok(())
}

fn print_projected_answer(
    out: &mut impl Write,
    answer: &ProjectedAnswerWire,
) -> std::io::Result<()> {
    print_target_resolution(out, &answer.target_resolution)?;
    print_currentness(out, &answer.currentness)?;
    print_evidence(out, &answer.page.evidence)?;
    print_delivery_tail(out, &answer.page, &answer.economy, &answer.continuation)
}

fn print_find_result(out: &mut impl Write, result: &FindResultWire) -> std::io::Result<()> {
    match result {
        FindResultWire::Target(answer) => print_projected_answer(out, answer),
        FindResultWire::Files(listing) => {
            for entry in &listing.entries {
                writeln!(out, "{}", entry.path_rel)?;
            }
            if listing.truncated {
                marker(out, "TRUNCATED")?;
            }
            print_currentness(out, &listing.currentness)
        }
        FindResultWire::Text(result) => {
            match result.status {
                QueryStatusWire::NotFound => writeln!(out, "no matches")?,
                QueryStatusWire::Ambiguous => marker(out, "AMBIGUOUS")?,
                QueryStatusWire::Unsupported => marker(out, "UNSUPPORTED")?,
                QueryStatusWire::Truncated => marker(out, "TRUNCATED")?,
                QueryStatusWire::Refreshing
                | QueryStatusWire::Unavailable
                | QueryStatusWire::Found => {}
            }
            for text_match in &result.matches {
                writeln!(
                    out,
                    "{}:{}: {}",
                    text_match.path_rel,
                    text_match.span.start.line,
                    text_match.preview.as_deref().unwrap_or_default(),
                )?;
            }
            Ok(())
        }
    }
}

fn print_relations_result(
    out: &mut impl Write,
    result: &RelationsResultWire,
) -> std::io::Result<()> {
    print_target_resolution(out, &result.target)?;
    print_currentness(out, &result.currentness)?;
    for answer in &result.answers {
        for relation in &answer.confirmed {
            writeln!(
                out,
                "{:?} {:?} {:?} -> {:?}",
                relation.kind, relation.direction, relation.source, relation.target
            )?;
        }
        if !answer.gaps.is_empty() {
            marker(out, "TRUNCATED")?;
        }
    }
    Ok(())
}

fn print_knowledge_result(
    out: &mut impl Write,
    result: &KnowledgeResultWire,
) -> std::io::Result<()> {
    match result {
        KnowledgeResultWire::Rules { evidence, .. } => print_evidence(out, evidence),
        KnowledgeResultWire::WorkItems { items, truncated } => {
            for item in items {
                writeln!(out, "{:?} {} {}", item.status, item.uid, item.goal)?;
            }
            if *truncated {
                marker(out, "TRUNCATED")?;
            }
            Ok(())
        }
        KnowledgeResultWire::PolicyLineage(lineage) => writeln!(out, "{lineage:?}"),
        KnowledgeResultWire::DecisionLineage(lineage) => writeln!(out, "{lineage:?}"),
        KnowledgeResultWire::Handoffs {
            handoffs,
            truncated,
            ..
        } => {
            for handoff in handoffs {
                writeln!(out, "{} {}", handoff.created_at, handoff.handoff_summary)?;
            }
            if *truncated {
                marker(out, "TRUNCATED")?;
            }
            Ok(())
        }
    }
}

fn print_structural_summary(
    out: &mut impl Write,
    summary: &StructuralSummaryWire,
) -> std::io::Result<()> {
    print_currentness(out, &summary.currentness)?;
    for group in &summary.groups {
        writeln!(
            out,
            "{:?}: {} resources, {} internal edges",
            group.group, group.resources, group.internal_edges
        )?;
    }
    for edge in &summary.boundary_edges {
        writeln!(
            out,
            "{:?} {:?} -> {:?}: {}",
            edge.kind, edge.source, edge.target, edge.confirmed_edges
        )?;
    }
    Ok(())
}

/// Renders `result` as compact text to stdout and flushes it. Returns an
/// error (never panics) on a write failure, so the caller can honor the
/// "no acknowledgement on output failure" rule.
pub fn print_compact(result: &QueryResultWire) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    match result {
        QueryResultWire::Find(result) => print_find_result(&mut stdout, result)?,
        QueryResultWire::Inspect(answer)
        | QueryResultWire::Impact(answer)
        | QueryResultWire::Context(answer) => {
            print_projected_answer(&mut stdout, answer)?;
        }
        QueryResultWire::Relations(result) => print_relations_result(&mut stdout, result)?,
        QueryResultWire::Knowledge(result) => print_knowledge_result(&mut stdout, result)?,
        QueryResultWire::Structure(summary) => print_structural_summary(&mut stdout, summary)?,
    }
    stdout.flush()
}
