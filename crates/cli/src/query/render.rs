//! Compact and `--json` output rendering (#24 §20, §21).
//!
//! Every writer takes an explicit `impl Write` and returns `io::Result`
//! rather than using `println!` (which panics on a write failure): #24
//! §20/§23 require the CLI to acknowledge only after output write/flush
//! actually succeeds, so a broken pipe must be a normal `Err`, not a
//! panic that skips the "no ack on output failure" rule by accident.

use std::io::Write;

use brainprint_core::{
    present::{self, Locale, Msg, text},
    protocol::query::*,
};

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
        range.span.start.line_1based(),
        range.span.start.column_1based(),
        range.span.end.line_1based(),
        range.span.end.column_1based(),
    )?;
    writeln!(out, "{}", range.source)
}

/// Prints one evidence item, returning whether it was `SourceUnavailable`
/// (batched into a single trailing marker by the caller).
fn print_one_evidence(out: &mut impl Write, item: &EvidenceWire) -> std::io::Result<bool> {
    match item {
        EvidenceWire::CurrentSource(range) => {
            print_current_source(out, range)?;
            Ok(false)
        }
        EvidenceWire::SourceUnavailable { .. } => Ok(true),
        // #58: one line per declaration; 1-based lines for a human reader.
        EvidenceWire::Outline(outline) => {
            writeln!(
                out,
                "--- outline {} ({:?}, {} symbols) ---",
                outline.path_rel,
                outline.coverage,
                outline.entries.len()
            )?;
            for entry in &outline.entries {
                let indent = if entry.parent.is_some() { "    " } else { "  " };
                writeln!(
                    out,
                    "{indent}{:?} {} L{}-{}",
                    entry.kind,
                    entry.name,
                    line_1based(entry.start_line),
                    line_1based(entry.end_line)
                )?;
            }
            Ok(false)
        }
        // #78: every variant that carries a source span is spelled out with
        // 1-based editor lines; a Debug dump would cite the canonical
        // 0-based `line` as if it were a locator.
        EvidenceWire::Symbol(candidate) => {
            let symbol = &candidate.symbol;
            writeln!(
                out,
                "{:?} {} {}:{}-{} ({:?})",
                symbol.kind,
                symbol.qualified_name,
                candidate.path_rel,
                symbol.span.start.line_1based(),
                symbol.span.end.line_1based(),
                candidate.coverage,
            )?;
            Ok(false)
        }
        EvidenceWire::Relation(projected) => {
            let relation = &projected.relation;
            writeln!(
                out,
                "{:?} {:?} {:?} -> {:?}",
                relation.kind, relation.direction, relation.source, relation.target
            )?;
            for (index, location) in relation.evidence.iter().enumerate() {
                // #73: a site's `line_1based` is already the editor line.
                match projected.sites.get(index) {
                    Some(site) => {
                        let path = site
                            .path_rel
                            .clone()
                            .unwrap_or_else(|| location.resource.to_string());
                        write!(out, "  at {path}:{}", site.line_1based)?;
                        match &site.owner {
                            Some(owner) => writeln!(out, " in {}", owner.qualified_name)?,
                            None => writeln!(out)?,
                        }
                    }
                    None => writeln!(
                        out,
                        "  at {}:{}",
                        location.resource,
                        location.span.start.line_1based()
                    )?,
                }
            }
            Ok(false)
        }
        EvidenceWire::RelationGap(gap) => {
            writeln!(
                out,
                "gap {:?} {:?} {} at {}:{}",
                gap.reason,
                gap.intended,
                gap.lookup_name,
                gap.location.resource,
                gap.location.span.start.line_1based()
            )?;
            Ok(false)
        }
        EvidenceWire::RelatedTest { candidate, .. } => {
            writeln!(
                out,
                "related test {} distance {} {:?} {:?}",
                candidate.path_rel, candidate.distance, candidate.basis, candidate.support
            )?;
            Ok(false)
        }
        // The remaining variants carry no source span, so their Debug form
        // is lossless and cites no line. ponytail: Debug-based; add
        // per-variant prose if a human compact reader needs it later.
        other => {
            writeln!(out, "{other:?}")?;
            Ok(false)
        }
    }
}

fn print_evidence(out: &mut impl Write, items: &[EvidenceWire]) -> std::io::Result<()> {
    let mut source_unavailable = false;
    for item in items {
        source_unavailable |= print_one_evidence(out, item)?;
    }
    if source_unavailable {
        marker(out, "SOURCE_UNAVAILABLE")?;
    }
    Ok(())
}

/// Same as [`print_evidence`], for a delivery page's slots: each is
/// either the full item or a Task 8 reuse reference in its place (#41).
fn print_delivered_items(out: &mut impl Write, items: &[DeliveredItemWire]) -> std::io::Result<()> {
    let mut source_unavailable = false;
    for item in items {
        match item {
            DeliveredItemWire::Full(evidence) => {
                source_unavailable |= print_one_evidence(out, evidence)?;
            }
            DeliveredItemWire::Reuse(reference) => writeln!(out, "REUSE {reference:?}")?,
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
    print_delivered_items(out, &answer.page.evidence)?;
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
                    text_match.span.start.line_1based(),
                    text_match.preview.as_deref().unwrap_or_default(),
                )?;
            }
            Ok(())
        }
    }
}

/// The English words for `msg` -- compact output is not localized.
fn say(msg: Msg) -> &'static str {
    text(Locale::En, msg)
}

/// `label count` for every non-zero count.
fn nonzero<N: Copy + Default + PartialEq + std::fmt::Display>(
    limits: &mut Vec<String>,
    counts: &[(&str, N)],
) {
    for (label, count) in counts {
        if *count != N::default() {
            limits.push(format!("{label} {count}"));
        }
    }
}

fn parenthesized(limits: &[String]) -> String {
    if limits.is_empty() {
        String::new()
    } else {
        format!(" ({})", limits.join(", "))
    }
}

/// What keeps a direct relation answer from complete coverage, as counted
/// by the daemon.
fn relation_limits(coverage: &CoverageWire) -> Vec<String> {
    let mut limits = Vec::new();
    if let Some(scope) = &coverage.scope
        && scope.support != SupportWire::Supported
    {
        limits.push(format!("scope {:?}", scope.support));
    }
    nonzero(
        &mut limits,
        &[
            ("gaps", coverage.gaps),
            ("ambiguous", coverage.ambiguous),
            ("requires semantics", coverage.requires_semantics),
            ("unsupported constructs", coverage.unsupported_construct),
            ("truncated", coverage.truncated),
            ("unattributed", coverage.unattributed),
            ("unconfirmed owners", coverage.unconfirmed_owners),
        ],
    );
    if coverage.semantic.not_current {
        limits.push("semantic not current".to_owned());
    }
    limits
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
        // #78: the confirmed rows are never the whole answer by omission --
        // each direction states its coverage the way `present` judges it.
        let summary = present::relation_summary(answer);
        let state = match summary.none {
            Some(none) => say(none).to_owned(),
            None => format!(
                "{} confirmed, coverage {}",
                summary.confirmed,
                say(summary.coverage)
            ),
        };
        writeln!(
            out,
            "{}: {state}{}",
            say(summary.direction),
            parenthesized(&relation_limits(&answer.coverage))
        )?;
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

/// What keeps a group's structure from complete coverage.
fn group_limits(coverage: &GroupCoverageWire) -> Vec<String> {
    let mut limits = Vec::new();
    nonzero(
        &mut limits,
        &[
            ("partial", coverage.partial),
            ("unsupported", coverage.unsupported),
            ("structure not current", coverage.structure_not_current),
            ("relation dirty", coverage.relation_dirty),
            ("unresolved gaps", coverage.unresolved_gaps),
            ("candidate gaps", coverage.candidate_gaps),
            ("requires semantics", coverage.requires_semantics),
            ("unsupported constructs", coverage.unsupported_construct),
            ("candidate truncated", coverage.candidate_truncated),
            ("semantic conflicts", coverage.semantic_conflicts),
            ("semantic not current", coverage.semantic_not_current),
        ],
    );
    limits
}

/// `label: N confirmed, coverage …` -- or, with none, which "none" it is.
fn print_structure_state(
    out: &mut impl Write,
    label: &str,
    confirmed: usize,
    complete: bool,
) -> std::io::Result<()> {
    let state = match (confirmed, complete) {
        (0, true) => say(Msg::AnswerNoneComplete).to_owned(),
        (0, false) => say(Msg::AnswerNoneIncomplete).to_owned(),
        (count, true) => format!("{count} confirmed, coverage {}", say(Msg::CoverageComplete)),
        (count, false) => format!("{count} confirmed, coverage {}", say(Msg::CoveragePartial)),
    };
    writeln!(out, "{label}: {state}")
}

fn print_structural_summary(
    out: &mut impl Write,
    summary: &StructuralSummaryWire,
) -> std::io::Result<()> {
    print_currentness(out, &summary.currentness)?;
    // #78: zero boundary edges or cycles is "none" only when nothing
    // limited the structure the daemon read.
    let mut complete = summary.currentness == CurrentnessWire::Current && summary.gaps.is_empty();
    for group in &summary.groups {
        let limits = group_limits(&group.coverage);
        complete &= limits.is_empty();
        writeln!(
            out,
            "{:?}: {} resources, {} internal edges{}",
            group.group,
            group.resources,
            group.internal_edges,
            parenthesized(&limits)
        )?;
    }
    for edge in &summary.boundary_edges {
        writeln!(
            out,
            "{:?} {:?} -> {:?}: {}",
            edge.kind, edge.source, edge.target, edge.confirmed_edges
        )?;
    }
    print_structure_state(
        out,
        "boundary edges",
        summary.boundary_edges.len(),
        complete,
    )?;
    if let Some(cycles) = &summary.cycles {
        for cycle in cycles {
            let groups: Vec<String> = cycle.iter().map(|group| format!("{group:?}")).collect();
            writeln!(out, "cycle: {}", groups.join(" -> "))?;
        }
        print_structure_state(out, "cycles", cycles.len(), complete)?;
    }
    if !summary.gaps.is_empty() {
        // One line per reason, summed over groups: the count and the why,
        // not every aggregate.
        let mut reasons: Vec<(UnresolvedReasonWire, u64)> = Vec::new();
        for gap in &summary.gaps {
            let count = gap.unresolved + gap.candidate;
            match reasons.iter_mut().find(|(reason, _)| *reason == gap.reason) {
                Some((_, seen)) => *seen += count,
                None => reasons.push((gap.reason, count)),
            }
        }
        let reasons: Vec<String> = reasons
            .iter()
            .map(|(reason, count)| format!("{reason:?} {count}"))
            .collect();
        writeln!(out, "gaps: {}", reasons.join(", "))?;
    }
    Ok(())
}

/// Renders `result` as compact text to stdout and flushes it. Returns an
/// error (never panics) on a write failure, so the caller can honor the
/// "no acknowledgement on output failure" rule.
pub fn print_compact(result: &QueryResultWire) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    write_compact(&mut stdout, result)?;
    stdout.flush()
}

/// The compact rendering of `result` into `out` -- also the TUI's
/// evidence body (#70), so both surfaces print the same facts.
pub fn write_compact(out: &mut impl Write, result: &QueryResultWire) -> std::io::Result<()> {
    match result {
        QueryResultWire::Find(result) => print_find_result(out, result)?,
        QueryResultWire::Inspect(answer)
        | QueryResultWire::Impact(answer)
        | QueryResultWire::Context(answer) => {
            print_projected_answer(out, answer)?;
        }
        QueryResultWire::Relations(result) => print_relations_result(out, result)?,
        QueryResultWire::Knowledge(result) => print_knowledge_result(out, result)?,
        QueryResultWire::Structure(summary) => print_structural_summary(out, summary)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use brainprint_core::{ResourceId, SymbolId};

    use super::*;

    fn span(start: usize, end: usize) -> SourceSpanWire {
        SourceSpanWire {
            start_byte: 0,
            end_byte: 0,
            start: SourcePointWire {
                line: start,
                column: 0,
            },
            end: SourcePointWire {
                line: end,
                column: 1,
            },
        }
    }

    fn compact(result: &QueryResultWire) -> String {
        let mut out = Vec::new();
        write_compact(&mut out, result).expect("render");
        String::from_utf8(out).expect("utf-8")
    }

    /// #76: the shared compact renderer (CLI, TUI, Web) cites 1-based editor lines for a
    /// canonical 0-based span: line 0 -> 1, and a multi-line range's start and end each.
    #[test]
    fn compact_locators_are_one_based() {
        let text = QueryResultWire::Find(FindResultWire::Text(TextSearchResultWire {
            status: QueryStatusWire::Found,
            matches: vec![TextMatchWire {
                path_rel: "src/a.rs".to_owned(),
                resource_id: None,
                span: span(0, 0),
                preview: Some("fn a() {}".to_owned()),
                source: MatchSourceWire::TextFallback,
            }],
            scope: ScopeReportWire::default(),
            structural_currentness: CurrentnessWire::Current,
            reason: FallbackReasonWire::ExplicitTextSearch,
        }));
        assert_eq!(compact(&text), "src/a.rs:1: fn a() {}\n");

        let range = PreparedRangeWire {
            resource: ResourceId::from_bytes([0; 16]),
            path_rel: "src/a.rs".to_owned(),
            resource_revision: "1".to_owned(),
            span: span(18, 20),
            source: "x".to_owned(),
            role: RangeRoleWire::AnchorDeclaration,
            verification: SourceVerificationWire {
                expected_content_hash: String::new(),
                observed_content_hash: String::new(),
                currentness: CurrentnessWire::Current,
            },
        };
        let mut out = Vec::new();
        print_current_source(&mut out, &range).expect("render");
        let header = String::from_utf8(out).expect("utf-8");
        assert!(header.contains("[19:1-21:2]"), "{header}");
    }

    fn coverage(unattributed: usize, unconfirmed_owners: usize) -> CoverageWire {
        CoverageWire {
            attribution: GapAttributionWire::TargetCandidateOnly,
            gaps: 0,
            ambiguous: 0,
            requires_semantics: 0,
            unsupported_construct: 0,
            truncated: 0,
            unattributed,
            unconfirmed_owners,
            scope: None,
            semantic: SemanticScopeWire {
                contexts: 0,
                conflicts: 0,
                not_current: false,
            },
        }
    }

    fn location(line: usize) -> EvidenceLocationWire {
        EvidenceLocationWire {
            resource: ResourceId::from_bytes([1; 16]),
            containing_symbol: None,
            occurrence_kind: OccurrenceKindWire::CallSite,
            span: span(line, line),
            basis_revision: "1".to_owned(),
            support: SupportWire::Supported,
            freshness: FreshnessWire::Fresh,
        }
    }

    fn call(line: usize) -> RelationResultWire {
        RelationResultWire {
            kind: RelationKindWire::Calls,
            source: GraphEndpointWire::Symbol(SymbolId::from_bytes([2; 16])),
            target: GraphEndpointWire::Symbol(SymbolId::from_bytes([3; 16])),
            direction: DirectionWire::Incoming,
            dispatch: DispatchWire::Static,
            target_scope: TargetScopeWire::Internal,
            resolution: ResolutionWire::Resolved,
            support: SupportWire::Supported,
            freshness: FreshnessWire::Fresh,
            evidence: vec![location(line)],
        }
    }

    fn relations(confirmed: Vec<RelationResultWire>, coverage: CoverageWire) -> String {
        compact(&QueryResultWire::Relations(RelationsResultWire {
            target: TargetResolutionWire::Resolved(GraphEndpointWire::Symbol(
                SymbolId::from_bytes([3; 16]),
            )),
            selection: Vec::new(),
            currentness: CurrentnessWire::Current,
            answers: vec![RelationAnswerWire {
                direction: DirectionWire::Incoming,
                kinds: vec![RelationKindWire::Calls],
                confirmed,
                gaps: Vec::new(),
                coverage,
            }],
        }))
    }

    /// #78: `load_workspace_config` incoming -- confirmed rows under incomplete coverage say so,
    /// and a zero under incomplete coverage is never the complete-coverage zero (nor empty).
    #[test]
    fn compact_relations_state_their_coverage() {
        let partial = relations(vec![call(3)], coverage(89317, 4));
        assert!(partial.contains("\nCalls Incoming Symbol("), "{partial}");
        assert!(
            partial.contains(
                "Incoming: 1 confirmed, coverage Partial (unattributed 89317, unconfirmed owners 4)"
            ),
            "{partial}"
        );

        let none_incomplete = relations(Vec::new(), coverage(89317, 4));
        assert!(
            none_incomplete.contains(
                "Incoming: None found -- coverage incomplete (unattributed 89317, unconfirmed owners 4)"
            ),
            "{none_incomplete}"
        );
        let none_complete = relations(Vec::new(), coverage(0, 0));
        assert!(
            none_complete.contains("Incoming: None (complete coverage)"),
            "{none_complete}"
        );
        assert_ne!(none_incomplete, none_complete);

        let mut unsupported = coverage(0, 0);
        unsupported.scope = Some(ScopeStateWire {
            resource: ResourceId::from_bytes([1; 16]),
            support: SupportWire::Unsupported,
            freshness: FreshnessWire::Fresh,
        });
        let unsupported = relations(Vec::new(), unsupported);
        assert!(
            unsupported.contains("Incoming: Unsupported (scope Unsupported)"),
            "{unsupported}"
        );
    }

    fn structure(coverage: GroupCoverageWire, gaps: Vec<GapAggregateWire>) -> String {
        let group = |name: &str| GroupSummaryWire {
            group: StructuralGroupWire::Group(name.to_owned()),
            prefixes: Vec::new(),
            resources: 19,
            internal_edges: 159,
            outgoing_edges: 0,
            incoming_edges: 0,
            fan_out_groups: 0,
            fan_in_groups: 0,
            coverage,
            members: Vec::new(),
        };
        compact(&QueryResultWire::Structure(StructuralSummaryWire {
            basis: GroupBasisWire::PathPrefix,
            relation_kinds: Vec::new(),
            currentness: CurrentnessWire::Current,
            groups: vec![group("agent")],
            boundary_edges: Vec::new(),
            cycles: Some(Vec::new()),
            gaps,
        }))
    }

    /// #78: zero boundary edges / cycles beside unresolved structure is incomplete, not "none".
    #[test]
    fn compact_structure_states_its_coverage() {
        let clean = GroupCoverageWire {
            supported: 19,
            partial: 0,
            unsupported: 0,
            structure_not_current: 0,
            relation_dirty: 0,
            unresolved_gaps: 0,
            candidate_gaps: 0,
            requires_semantics: 0,
            unsupported_construct: 0,
            candidate_truncated: 0,
            semantic_conflicts: 0,
            semantic_not_current: 0,
        };
        let complete = structure(clean, Vec::new());
        assert_eq!(
            complete,
            "Group(\"agent\"): 19 resources, 159 internal edges\n\
             boundary edges: None (complete coverage)\n\
             cycles: None (complete coverage)\n"
        );

        let limited = GroupCoverageWire {
            requires_semantics: 550,
            unresolved_gaps: 2260,
            ..clean
        };
        let gap = GapAggregateWire {
            group: StructuralGroupWire::Group("agent".to_owned()),
            intended: IntendedRelationWire::Known(RelationKindWire::Calls),
            reason: UnresolvedReasonWire::MacroCallRequiresSemantics,
            unresolved: 2000,
            candidate: 5,
            candidate_truncated: 0,
        };
        let incomplete = structure(limited, vec![gap]);
        assert_eq!(
            incomplete,
            "Group(\"agent\"): 19 resources, 159 internal edges \
             (unresolved gaps 2260, requires semantics 550)\n\
             boundary edges: None found -- coverage incomplete\n\
             cycles: None found -- coverage incomplete\n\
             gaps: MacroCallRequiresSemantics 2005\n"
        );
    }

    /// #78 / #76: `TelemetryEvent` (canonical line 24) and its members are cited at their editor
    /// lines; no Debug `line: 24` survives as a locator, and a site's `line_1based` is not
    /// shifted again.
    #[test]
    fn compact_evidence_cites_editor_lines_only() {
        let symbol = |name: &str, start: usize, end: usize| {
            EvidenceWire::Symbol(SymbolCandidateWire {
                symbol: SymbolWire {
                    id: SymbolId::from_bytes([4; 16]),
                    resource_id: ResourceId::from_bytes([1; 16]),
                    parent_id: None,
                    kind: SymbolKindWire::Struct,
                    name: name.to_owned(),
                    qualified_name: name.to_owned(),
                    signature: None,
                    visibility: VisibilityWire::Public,
                    exported: true,
                    span: span(start, end),
                    resource_revision: "1".to_owned(),
                    analysis_profile_id: 1,
                },
                path_rel: "crates/agent/src/telemetry.rs".to_owned(),
                coverage: StructuralCoverageWire::Complete,
            })
        };
        let relation = EvidenceWire::Relation(ProjectedRelationWire {
            relation: call(24),
            sites: vec![EvidenceSiteWire {
                path_rel: Some("crates/agent/src/telemetry.rs".to_owned()),
                line_1based: 25,
                owner: None,
            }],
        });
        let mut out = Vec::new();
        print_evidence(
            &mut out,
            &[
                symbol("TelemetryEvent", 24, 45),
                symbol("kind", 29, 29),
                relation,
            ],
        )
        .expect("render");
        let body = String::from_utf8(out).expect("utf-8");
        assert!(
            body.contains("Struct TelemetryEvent crates/agent/src/telemetry.rs:25-46 (Complete)"),
            "{body}"
        );
        assert!(body.contains("telemetry.rs:30-30"), "{body}");
        assert!(
            body.contains("  at crates/agent/src/telemetry.rs:25\n"),
            "{body}"
        );
        assert!(!body.contains("line:"), "no raw 0-based locator: {body}");
        assert!(!body.contains(":24") && !body.contains(":26"), "{body}");
    }
}
