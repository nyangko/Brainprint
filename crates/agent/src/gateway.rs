//! The common adoption/substitution logic (#26 "Common flow"). Input is
//! a normalized [`IntegrationEvent`]; nothing here reads a native payload
//! or branches on a client brand -- only on capabilities (#26
//! acceptance 46-49).
//!
//! Safety rule, everywhere: equivalence not proven => allow native.
//! Suppression requires (a) guard mode, (b) a Brainprint delivery of the
//! same fact in the same agent context and reset epoch, (c) a
//! deterministic request mapping, and (d) a fresh cheap probe proving
//! the delivered basis is still the current index truth.

use std::path::{Path, PathBuf};

use crate::{
    delivery::{self, DeliveredFact},
    event::{
        ActionClass, Decision, DecisionKind, EventKind, FallbackReason, IntegrationEvent, Mode,
        NativeAction, SearchPattern,
    },
    probe::{FilesProbe, Probe},
    state::{
        ListingDescriptor, RouteState, SearchDescriptor, SessionRecord, SourceDescriptor,
        StateStore,
    },
};

/// #26 "Thin bootstrap": an operational pointer, not Policy/Working
/// State/Skill/schemas.
pub const BOOTSTRAP: &str = "Brainprint is available for this project: use it first for current/complete project facts. \
brainprint.find = location/files/text; brainprint.inspect = exact source/understanding; \
brainprint.relations = dependency/impact; brainprint.context = change/resume/rules/history/structure/state. \
Do not rediscover facts Brainprint already returned current/complete. Native tools remain valid when \
Brainprint is partial/stale/unsupported/ambiguous/truncated or raw verification is explicitly needed.";

/// Bound on the scope listing that pins a text search's basis: Task 11's
/// own `Find::Files` maximum (`DEFAULT_CANDIDATE_LIMIT`). A larger scope
/// comes back truncated and is simply never a suppression basis.
pub const SCOPE_PROBE_LIMIT: usize = 200;
/// Bound on a single-path revision probe.
const PATH_PROBE_LIMIT: usize = 16;
/// Bound on facts recorded (and probes spent) per delivery.
const MAX_FACTS_PER_DELIVERY: usize = 8;

/// Everything a bridge and the telemetry sink need from one event.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub decision: Decision,
    pub route: Option<&'static str>,
    pub exact_substitute_proven: bool,
    pub native_kind: Option<&'static str>,
    pub suggested: Option<&'static str>,
    pub state_bytes: usize,
    /// Whether this event is an adoption fact worth a telemetry line
    /// (session boundaries, exploration, Brainprint calls).
    pub observed: bool,
}

pub fn handle(
    event: &IntegrationEvent,
    mode: Mode,
    store: &StateStore,
    probe: &mut dyn Probe,
) -> Outcome {
    let key = state_key(event);
    let mut record = store.load(&key);
    record.mode = Some(mode);
    let mut outcome = Outcome {
        decision: Decision::allow(),
        route: None,
        exact_substitute_proven: false,
        native_kind: None,
        suggested: None,
        state_bytes: 0,
        observed: false,
    };

    match event.kind {
        EventKind::SessionStarted => session_started(event, &mut record, &mut outcome),
        EventKind::SessionReset => session_reset(event, &mut record, &mut outcome),
        EventKind::PreTool | EventKind::PostTool => {
            if let Some(action) = &event.action {
                tool_event(event, action, mode, &mut record, probe, &mut outcome);
            }
            if record.pending_bootstrap && event.can_inject_context {
                record.pending_bootstrap = false;
                record.bootstrapped_epoch = Some(record.epoch);
                outcome.decision.bootstrap = Some(BOOTSTRAP);
                outcome.observed = true;
            }
        }
        EventKind::ContextUsage => outcome.observed = true,
    }

    outcome.route = record.route.classification();
    // A state write failure only loses an optimization: fail open.
    outcome.state_bytes = store.save(&key, &mut record).unwrap_or(0);
    outcome
}

/// A subagent's context is not its parent's: separate records.
fn state_key(event: &IntegrationEvent) -> String {
    match (&event.agent_id, event.kind) {
        (Some(agent), EventKind::PreTool | EventKind::PostTool) => {
            format!("{}#{agent}", event.session_id)
        }
        _ => event.session_id.clone(),
    }
}

fn session_started(event: &IntegrationEvent, record: &mut SessionRecord, outcome: &mut Outcome) {
    outcome.observed = true;
    if let Some(agent) = &event.agent_id {
        // Subagent start: bootstrap once per agent id (#26 acceptance 23).
        if record.note_subagent(agent) && event.can_inject_context {
            outcome.decision.bootstrap = Some(BOOTSTRAP);
        }
        return;
    }
    // Duplicate startup events are idempotent (#29 "Fail-open").
    if record.bootstrapped_epoch == Some(record.epoch) {
        return;
    }
    if event.can_inject_context {
        record.bootstrapped_epoch = Some(record.epoch);
        outcome.decision.bootstrap = Some(BOOTSTRAP);
    } else {
        record.pending_bootstrap = true;
    }
}

fn session_reset(event: &IntegrationEvent, record: &mut SessionRecord, outcome: &mut Outcome) {
    outcome.observed = true;
    record.start_epoch();
    if event.can_inject_context {
        record.pending_bootstrap = false;
        record.bootstrapped_epoch = Some(record.epoch);
        outcome.decision.bootstrap = Some(BOOTSTRAP);
    } else {
        // e.g. an advisory-only pre-compaction signal: one later
        // bootstrap at the next event that can carry it (#26 acc. 22).
        record.pending_bootstrap = true;
    }
}

fn tool_event(
    event: &IntegrationEvent,
    action: &NativeAction,
    mode: Mode,
    record: &mut SessionRecord,
    probe: &mut dyn Probe,
    outcome: &mut Outcome,
) {
    let is_pre = event.kind == EventKind::PreTool;
    match action {
        NativeAction::Brainprint {
            tool,
            input,
            result,
        } => {
            outcome.observed = true;
            outcome.native_kind = Some("brainprint");
            record.route = match record.route {
                RouteState::Unset => RouteState::BrainprintFirst,
                RouteState::NativeFirst => RouteState::NativeFirstThenBrainprint,
                other => other,
            };
            if let (false, Some(result), true) =
                (is_pre, result, event.capabilities.post_tool_result)
                && let Some(delivered) = delivery::observe(*tool, input, result)
            {
                record.route = match (record.route, delivered.complete) {
                    (RouteState::BrainprintFirst, false) => RouteState::BrainprintGap,
                    (RouteState::BrainprintGap, true) => RouteState::BrainprintFirst,
                    (other, _) => other,
                };
                // Observe never suppresses, so it never spends probes
                // pinning a suppression basis.
                if mode != Mode::Observe {
                    record_facts(event, &delivered, record, probe);
                }
            }
        }
        NativeAction::Write { path } => record.forget_path(path),
        NativeAction::Opaque => record.forget_deliveries(),
        NativeAction::Inert => {}
        NativeAction::SourceRead { .. }
        | NativeAction::FileDiscovery { .. }
        | NativeAction::TextSearch { .. }
        | NativeAction::UnprovenExploration { .. } => {
            let class = class_of(action);
            outcome.observed = true;
            outcome.native_kind = Some(class_label(class));
            outcome.suggested = Some(suggestion(class));
            if is_pre {
                record.route = match record.route {
                    RouteState::Unset => RouteState::NativeFirst,
                    RouteState::BrainprintGap => RouteState::FallbackRequired,
                    other => other,
                };
            }
            if mode == Mode::Observe {
                return;
            }
            if is_pre {
                pre_exploration(event, action, class, mode, record, probe, outcome);
            } else if !event.capabilities.pre_tool_context && event.capabilities.post_tool_context {
                // No pre-tool channel: the advice rides on the result.
                maybe_advise(event, class, record, probe, outcome);
            }
        }
    }
}

fn pre_exploration(
    event: &IntegrationEvent,
    action: &NativeAction,
    class: ActionClass,
    mode: Mode,
    record: &mut SessionRecord,
    probe: &mut dyn Probe,
    outcome: &mut Outcome,
) {
    if mode == Mode::Guard && record.bypass {
        record.bypass = false;
        outcome.decision = Decision::fallback(FallbackReason::UserBypass);
        return;
    }

    let verdict = match action {
        NativeAction::UnprovenExploration { reason, .. } => Err(Some(*reason)),
        NativeAction::SourceRead { path, lines } => {
            prove_source(event, path, *lines, record, probe)
        }
        NativeAction::FileDiscovery { scope } => {
            prove_listing(event, scope.as_deref(), record, probe)
        }
        NativeAction::TextSearch {
            pattern,
            case_insensitive,
            scope,
        } => prove_search(
            event,
            pattern,
            *case_insensitive,
            scope.as_deref(),
            record,
            probe,
        ),
        _ => Err(None),
    };

    match verdict {
        Ok(fact) => {
            outcome.exact_substitute_proven = true;
            let suppress = mode == Mode::Guard && event.capabilities.agent_scoped_tool_events;
            let message = if suppress {
                format!(
                    "Brainprint already delivered {fact} in this session and it is still current; \
                     this native {} is redundant -- use the delivered result. For raw verification, \
                     run `brainprint-agent bypass-once --session {}` and retry.",
                    class_label(class),
                    event.session_id
                )
            } else {
                format!(
                    "Brainprint already delivered {fact} in this session and it is still current; \
                     re-running it natively is redundant."
                )
            };
            outcome.decision = Decision {
                kind: if suppress {
                    DecisionKind::SuppressRedirect
                } else {
                    DecisionKind::Advise
                },
                message: (event.capabilities.pre_tool_context || suppress).then_some(message),
                bootstrap: None,
                // Guard without agent attribution degrades to advice.
                fallback: (mode == Mode::Guard && !suppress)
                    .then_some(FallbackReason::UnprovenEquivalence),
            };
        }
        Err(reason) => {
            outcome.decision = Decision {
                fallback: reason,
                ..Decision::allow()
            };
            if event.capabilities.pre_tool_context {
                maybe_advise(event, class, record, probe, outcome);
            }
        }
    }
}

/// First-route advice (#26: "Before any Brainprint result has been
/// delivered, prefer mode may advise a Brainprint route using cheap
/// no-source Task 11 probes"). At most once per action class per epoch,
/// and never once Brainprint is already in use this epoch.
fn maybe_advise(
    event: &IntegrationEvent,
    class: ActionClass,
    record: &mut SessionRecord,
    probe: &mut dyn Probe,
    outcome: &mut Outcome,
) {
    let brainprint_in_use = matches!(
        record.route,
        RouteState::BrainprintFirst
            | RouteState::BrainprintGap
            | RouteState::NativeFirstThenBrainprint
            | RouteState::FallbackRequired
    );
    if brainprint_in_use || record.advised.contains(&class) {
        return;
    }
    let Some(root) = event.cwd.as_deref().and_then(|cwd| canonical(None, cwd)) else {
        return;
    };
    let initialized = probe.files(&FilesProbe {
        root: path_text(&root),
        directory: None,
        recursive: false,
        path_prefix: None,
        limit: 1,
    });
    match initialized {
        Ok(_) => {
            record.advised.push(class);
            outcome.decision.kind = DecisionKind::Advise;
            outcome.decision.message = Some(format!(
                "Brainprint has current indexed facts for this workspace. For {}, prefer {} \
                 first; native tools stay valid when Brainprint reports a gap.",
                class_label(class),
                suggestion(class)
            ));
        }
        Err(reason) => {
            outcome.decision.fallback.get_or_insert(reason);
        }
    }
}

type Proof = Result<String, Option<FallbackReason>>;

fn prove_source(
    event: &IntegrationEvent,
    path: &str,
    lines: Option<(usize, usize)>,
    record: &SessionRecord,
    probe: &mut dyn Probe,
) -> Proof {
    let absolute = canonical(event.cwd.as_deref(), path).ok_or(None)?;
    let candidates: Vec<&SourceDescriptor> = record
        .sources
        .iter()
        .filter(|source| Path::new(&source.root).join(&source.path_rel) == absolute)
        .collect();
    let first = candidates.first().ok_or(None)?;
    let (start, end) = lines.ok_or(Some(FallbackReason::RequestExceedsDeliveredRange))?;
    let basis = (&first.root, &first.workspace_id, &first.revision);
    let mut covered: Vec<(usize, usize)> = candidates
        .iter()
        .filter(|source| (&source.root, &source.workspace_id, &source.revision) == basis)
        .map(|source| source.lines)
        .collect();
    if !union_covers(&mut covered, (start, end)) {
        return Err(Some(FallbackReason::RequestExceedsDeliveredRange));
    }

    let listing = probe
        .files(&FilesProbe {
            root: first.root.clone(),
            directory: None,
            recursive: true,
            path_prefix: Some(first.path_rel.clone()),
            limit: PATH_PROBE_LIMIT,
        })
        .map_err(Some)?;
    if listing.workspace_id.to_string() != first.workspace_id || !listing.current {
        return Err(Some(FallbackReason::StaleOrNotCurrent));
    }
    match listing
        .entries
        .iter()
        .find(|entry| entry.path_rel == first.path_rel)
    {
        Some(entry) if entry.revision == first.revision => Ok(format!(
            "the exact current source of {} lines {}-{}",
            first.path_rel,
            start + 1,
            end
        )),
        Some(_) => Err(Some(FallbackReason::StaleOrNotCurrent)),
        None if listing.truncated => Err(Some(FallbackReason::Truncated)),
        None => Err(Some(FallbackReason::StaleOrNotCurrent)),
    }
}

fn prove_listing(
    event: &IntegrationEvent,
    scope: Option<&str>,
    record: &SessionRecord,
    probe: &mut dyn Probe,
) -> Proof {
    let absolute = canonical(event.cwd.as_deref(), scope.unwrap_or(".")).ok_or(None)?;
    if !absolute.is_dir() {
        return Err(None);
    }
    let descriptor = record
        .listings
        .iter()
        .find(|listing| {
            relative_directory(&listing.root, &absolute)
                .is_some_and(|directory| directory == listing.directory)
        })
        .ok_or(None)?;
    let listing = probe
        .files(&FilesProbe {
            root: descriptor.root.clone(),
            directory: descriptor.directory.clone(),
            recursive: true,
            path_prefix: None,
            limit: descriptor.limit,
        })
        .map_err(Some)?;
    if listing.truncated {
        return Err(Some(FallbackReason::Truncated));
    }
    if listing.workspace_id.to_string() != descriptor.workspace_id
        || !listing.current
        || listing.fingerprint() != descriptor.fingerprint
    {
        return Err(Some(FallbackReason::StaleOrNotCurrent));
    }
    Ok(format!(
        "the complete current file listing of {}",
        descriptor
            .directory
            .as_deref()
            .unwrap_or("the workspace root")
    ))
}

fn prove_search(
    event: &IntegrationEvent,
    pattern: &SearchPattern,
    case_insensitive: bool,
    scope: Option<&str>,
    record: &SessionRecord,
    probe: &mut dyn Probe,
) -> Proof {
    if let SearchPattern::Regex(regex) = pattern
        && delivery::regex_may_span_lines(regex)
    {
        return Err(Some(FallbackReason::UnprovenEquivalence));
    }
    let absolute = canonical(event.cwd.as_deref(), scope.unwrap_or(".")).ok_or(None)?;
    if !absolute.is_dir() {
        return Err(None);
    }
    let descriptor = record
        .searches
        .iter()
        .find(|search| {
            &search.pattern == pattern
                && search.case_insensitive == case_insensitive
                && relative_directory(&search.root, &absolute).is_some_and(|directory| {
                    directory_prefix(directory.as_deref()) == search.path_prefix
                })
        })
        .ok_or(None)?;
    let listing = probe
        .files(&FilesProbe {
            root: descriptor.root.clone(),
            directory: None,
            recursive: true,
            path_prefix: descriptor.path_prefix.clone(),
            limit: descriptor.scope_limit,
        })
        .map_err(Some)?;
    if listing.truncated {
        return Err(Some(FallbackReason::Truncated));
    }
    if listing.workspace_id.to_string() != descriptor.workspace_id
        || !listing.current
        || listing.fingerprint() != descriptor.scope_fingerprint
    {
        return Err(Some(FallbackReason::StaleOrNotCurrent));
    }
    Ok("this exact complete current text search".to_owned())
}

/// Pin each delivered fact to one Workspace root + identity with a cheap
/// probe before it may ever justify a suppression.
fn record_facts(
    event: &IntegrationEvent,
    delivered: &delivery::Delivery,
    record: &mut SessionRecord,
    probe: &mut dyn Probe,
) {
    let root = match &delivered.workspace_path {
        Some(path) => canonical(event.cwd.as_deref(), path),
        None => event.cwd.as_deref().and_then(|cwd| canonical(None, cwd)),
    };
    let Some(root) = root.map(|root| path_text(&root)) else {
        return;
    };

    for fact in delivered.facts.iter().take(MAX_FACTS_PER_DELIVERY) {
        match fact {
            DeliveredFact::Source {
                resource_id,
                path_rel,
                revision,
                lines,
            } => {
                let Ok(listing) = probe.files(&FilesProbe {
                    root: root.clone(),
                    directory: None,
                    recursive: true,
                    path_prefix: Some(path_rel.clone()),
                    limit: PATH_PROBE_LIMIT,
                }) else {
                    continue;
                };
                let tied = listing.current
                    && listing.entries.iter().any(|entry| {
                        &entry.path_rel == path_rel
                            && &entry.id == resource_id
                            && &entry.revision == revision
                    });
                if tied {
                    record.push_source(SourceDescriptor {
                        root: root.clone(),
                        workspace_id: listing.workspace_id.to_string(),
                        path_rel: path_rel.clone(),
                        revision: revision.clone(),
                        lines: *lines,
                    });
                }
            }
            DeliveredFact::Listing {
                directory,
                limit,
                fingerprint,
            } => {
                let Ok(listing) = probe.files(&FilesProbe {
                    root: root.clone(),
                    directory: directory.clone(),
                    recursive: true,
                    path_prefix: None,
                    limit: *limit,
                }) else {
                    continue;
                };
                // Identical ids+paths+revisions: the delivery came from
                // this Workspace and nothing changed since.
                if listing.current && !listing.truncated && &listing.fingerprint() == fingerprint {
                    record.push_listing(ListingDescriptor {
                        root: root.clone(),
                        workspace_id: listing.workspace_id.to_string(),
                        directory: directory.clone(),
                        limit: *limit,
                        fingerprint: fingerprint.clone(),
                    });
                }
            }
            DeliveredFact::Search {
                pattern,
                case_insensitive,
                path_prefix,
                match_resource_ids,
            } => {
                // Without an explicit root, only matches (globally unique
                // ResourceIds) can tie the result to this Workspace.
                if delivered.workspace_path.is_none() && match_resource_ids.is_empty() {
                    continue;
                }
                let Ok(listing) = probe.files(&FilesProbe {
                    root: root.clone(),
                    directory: None,
                    recursive: true,
                    path_prefix: path_prefix.clone(),
                    limit: SCOPE_PROBE_LIMIT,
                }) else {
                    continue;
                };
                let tied = match_resource_ids
                    .iter()
                    .all(|id| listing.entries.iter().any(|entry| &entry.id == id));
                if listing.current && !listing.truncated && tied {
                    record.push_search(SearchDescriptor {
                        root: root.clone(),
                        workspace_id: listing.workspace_id.to_string(),
                        pattern: pattern.clone(),
                        case_insensitive: *case_insensitive,
                        path_prefix: path_prefix.clone(),
                        scope_limit: SCOPE_PROBE_LIMIT,
                        scope_fingerprint: listing.fingerprint(),
                    });
                }
            }
        }
    }
}

/// Resolve against `cwd` and canonicalize (symlinks, `..`). This is a
/// metadata lookup (`realpath`), never a content read -- the same
/// normalization `brainprint-mcp` applies to its Locator.
fn canonical(cwd: Option<&str>, path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(cwd?).join(path)
    };
    absolute.canonicalize().ok()
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `Some(None)` for the root itself, `Some(Some("a/b"))` below it,
/// `None` outside it.
fn relative_directory(root: &str, absolute: &Path) -> Option<Option<String>> {
    let relative = absolute.strip_prefix(root).ok()?;
    let text = relative.to_str()?.trim_end_matches('/');
    Some((!text.is_empty()).then(|| text.to_owned()))
}

fn directory_prefix(directory: Option<&str>) -> Option<String> {
    directory.map(|directory| format!("{directory}/"))
}

/// Whether the union of `ranges` covers `wanted` (end-exclusive).
fn union_covers(ranges: &mut [(usize, usize)], wanted: (usize, usize)) -> bool {
    ranges.sort_unstable();
    let mut reach = wanted.0;
    for &(start, end) in ranges.iter() {
        if start > reach {
            break;
        }
        reach = reach.max(end);
        if reach >= wanted.1 {
            return true;
        }
    }
    reach >= wanted.1
}

const fn class_of(action: &NativeAction) -> ActionClass {
    match action {
        NativeAction::FileDiscovery { .. } => ActionClass::ProjectTreeDiscovery,
        NativeAction::TextSearch { .. } => ActionClass::TextSearch,
        NativeAction::UnprovenExploration { class, .. } => *class,
        _ => ActionClass::SourceRead,
    }
}

pub const fn class_label(class: ActionClass) -> &'static str {
    match class {
        ActionClass::ProjectTreeDiscovery => "file discovery",
        ActionClass::TextSearch => "text search",
        ActionClass::SourceRead => "source read",
    }
}

pub const fn suggestion(class: ActionClass) -> &'static str {
    match class {
        ActionClass::ProjectTreeDiscovery => "brainprint.find (mode: files)",
        ActionClass::TextSearch => "brainprint.find (mode: text)",
        ActionClass::SourceRead => "brainprint.inspect",
    }
}

#[cfg(test)]
mod tests {
    use super::union_covers;

    #[test]
    fn union_coverage_is_exact() {
        assert!(union_covers(&mut [(10, 20)], (12, 18)));
        assert!(union_covers(&mut [(10, 20)], (10, 20)));
        assert!(!union_covers(&mut [(10, 20)], (9, 20)));
        assert!(!union_covers(&mut [(10, 20)], (10, 21)));
        assert!(union_covers(&mut [(15, 30), (10, 15)], (10, 30)));
        assert!(!union_covers(&mut [(10, 14), (15, 30)], (10, 30)));
    }
}
