//! Canonical, transport-neutral encoding of projection payload (#20
//! task 7 §2, §22-24).
//!
//! One encoder, several sinks: counting gives a delivery unit's exact
//! canonical bytes, SHA-256 gives a streaming fingerprint. Nothing is
//! formatted: every field is a fixed-width integer, a one-byte variant
//! tag, a presence byte, or a u64 length prefix and its bytes, so no two
//! values encode alike. Not Rust heap size, not JSON, not Debug text.

use std::collections::BTreeSet;

use brainprint_core::{
    BlueprintApplicationId, BlueprintId, DecisionId, IndexIncarnationId, LogicalSymbolId, PolicyId,
    ProjectId, ProjectStateId, ResourceId, SymbolId, UserPreferenceId, WorkItemId, WorkspaceId,
};
use sha2::{Digest, Sha256};

use super::{
    ChangeKind, CoverageEvidence, CoverageSubject, DeliveryHint, EvidenceItem, GenerationBasis,
    PlannedSourceRange, PreparedProjection, ProjectionCorrelation, ProjectionGap, ProjectionIntent,
    ProjectionKnowledgeRefs, ProjectionRequest, ProjectionTarget, Relevance, ResourceTarget,
    SourceRequirement, SymbolName, SymbolTarget, TargetSelection,
};
use crate::{
    coverage::{CoverageLimit, CoverageReport},
    gaps::{IntendedRelation, UnresolvedReason},
    graph::{DomainEntity, ExternalEntity, GraphEndpoint, RelationKind},
    impact::ImpactIntent,
    inspect::SourceVerification,
    knowledge::{
        Blueprint, BlueprintApplication, BlueprintApplicationStatus, BlueprintComponent,
        BlueprintDefinition, BlueprintDefinitionState, BlueprintEvidence, BlueprintOwnerKind,
        BlueprintRef, BlueprintRelationship, BlueprintStatus, ConflictKind, Decision,
        DecisionStatus, DirectiveTarget, DirtyObservation, EvidenceCategory, EvidenceRef,
        GenerationReference, GenerationReferenceState, KnowledgeConflict, KnowledgeScope, Origin,
        Policy, PolicyStatus, PreferenceStatus, PriorityClass, ProjectState, ProjectStateStatus,
        ProtectionClass, Provenance, RequestDirective, ResolutionReason, Resolved, ScopeKind,
        ShadowedItem, SourceKind, Staleness, TypedValue, UserPreference, WorkHandoff, WorkItem,
        WorkItemSourceKind, WorkItemStatus, WorkOverlap, WorkResource, WorkResourceRole,
        WorkResult, WorkResultStatus, WorkingState,
    },
    parser::{SourcePoint, SourceSpan},
    prepare::{PreparedRange, RangeRole, SourceUnavailable},
    query::{
        CoverageNote, Currentness, Located, NotCurrentReason, ResultSource, StructuralCoverage,
        SymbolCandidate,
    },
    related_tests::{ProjectionBasis, RelatedTestCandidate, TestPath},
    relations::{Direction, EvidenceLocation, RelationGap, RelationResult},
    resolution::{Dispatch, Freshness, Resolution, Support, TargetScope},
    resource::{Resource, ResourceKind, ResourceLanguage, ResourceRole, ResourceState},
    symbol::{OccurrenceKind, Symbol, SymbolKind, Visibility},
};

// ------------------------------------------------------------------ sink

enum Sink {
    Count(usize),
    Hash(Sha256),
    #[cfg(test)]
    Bytes(Vec<u8>),
}

pub(crate) struct Canon {
    sink: Sink,
}

impl Canon {
    fn raw(&mut self, bytes: &[u8]) {
        match &mut self.sink {
            Sink::Count(count) => *count += bytes.len(),
            Sink::Hash(hasher) => hasher.update(bytes),
            #[cfg(test)]
            Sink::Bytes(out) => out.extend_from_slice(bytes),
        }
    }

    fn int(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }

    pub(crate) fn tag(&mut self, tag: u8) {
        self.raw(&[tag]);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.int(bytes.len() as u64);
        self.raw(bytes);
    }
}

pub(crate) trait Canonical {
    fn encode(&self, out: &mut Canon);
}

/// Exact canonical byte count of `value`.
pub(crate) fn size<T: Canonical + ?Sized>(value: &T) -> usize {
    let mut out = Canon {
        sink: Sink::Count(0),
    };
    value.encode(&mut out);
    match out.sink {
        Sink::Count(count) => count,
        _ => unreachable!("counting sink"),
    }
}

/// SHA-256 over a versioned domain label and the canonical encoding.
pub(crate) fn digest<T: Canonical + ?Sized>(domain: &str, value: &T) -> [u8; 32] {
    let mut out = Canon {
        sink: Sink::Hash(Sha256::new()),
    };
    out.bytes(domain.as_bytes());
    value.encode(&mut out);
    match out.sink {
        Sink::Hash(hasher) => hasher.finalize().into(),
        _ => unreachable!("hashing sink"),
    }
}

/// The encoded bytes themselves, to check [`size`] against.
#[cfg(test)]
pub(crate) fn encode<T: Canonical + ?Sized>(value: &T) -> Vec<u8> {
    let mut out = Canon {
        sink: Sink::Bytes(Vec::new()),
    };
    value.encode(&mut out);
    match out.sink {
        Sink::Bytes(bytes) => bytes,
        _ => unreachable!("byte sink"),
    }
}

// ------------------------------------------------------------ primitives

impl Canonical for str {
    fn encode(&self, out: &mut Canon) {
        out.bytes(self.as_bytes());
    }
}

impl Canonical for String {
    fn encode(&self, out: &mut Canon) {
        out.bytes(self.as_bytes());
    }
}

impl Canonical for bool {
    fn encode(&self, out: &mut Canon) {
        out.tag(u8::from(*self));
    }
}

impl Canonical for usize {
    fn encode(&self, out: &mut Canon) {
        out.int(*self as u64);
    }
}

impl Canonical for i64 {
    fn encode(&self, out: &mut Canon) {
        out.raw(&self.to_be_bytes());
    }
}

impl Canonical for [u8; 32] {
    fn encode(&self, out: &mut Canon) {
        out.raw(self);
    }
}

impl<T: Canonical + ?Sized> Canonical for &T {
    fn encode(&self, out: &mut Canon) {
        (**self).encode(out);
    }
}

impl<T: Canonical + ?Sized> Canonical for Box<T> {
    fn encode(&self, out: &mut Canon) {
        (**self).encode(out);
    }
}

impl<T: Canonical> Canonical for Option<T> {
    fn encode(&self, out: &mut Canon) {
        match self {
            Some(value) => {
                out.tag(1);
                value.encode(out);
            }
            None => out.tag(0),
        }
    }
}

impl<T: Canonical> Canonical for [T] {
    fn encode(&self, out: &mut Canon) {
        out.int(self.len() as u64);
        for value in self {
            value.encode(out);
        }
    }
}

impl<T: Canonical> Canonical for Vec<T> {
    fn encode(&self, out: &mut Canon) {
        self.as_slice().encode(out);
    }
}

impl<T: Canonical> Canonical for BTreeSet<T> {
    fn encode(&self, out: &mut Canon) {
        out.int(self.len() as u64);
        for value in self {
            value.encode(out);
        }
    }
}

impl Canonical for serde_json::Value {
    fn encode(&self, out: &mut Canon) {
        // Compact serde_json text; object keys are sorted (no
        // `preserve_order`), so equal values encode alike.
        out.bytes(self.to_string().as_bytes());
    }
}

macro_rules! ids {
    ($($ty:ty),* $(,)?) => {
        $(impl Canonical for $ty {
            fn encode(&self, out: &mut Canon) {
                out.raw(&self.to_bytes());
            }
        })*
    };
}

ids!(
    BlueprintApplicationId,
    BlueprintId,
    DecisionId,
    IndexIncarnationId,
    LogicalSymbolId,
    PolicyId,
    ProjectId,
    ProjectStateId,
    ResourceId,
    SymbolId,
    UserPreferenceId,
    WorkItemId,
    WorkspaceId,
);

/// A fieldless vocabulary: its variant tag.
macro_rules! tags {
    ($($ty:ty),* $(,)?) => {
        $(impl Canonical for $ty {
            fn encode(&self, out: &mut Canon) {
                out.tag(*self as u8);
            }
        })*
    };
}

tags!(
    ScopeKind,
    SourceKind,
    PolicyStatus,
    ProtectionClass,
    PriorityClass,
    DecisionStatus,
    BlueprintStatus,
    BlueprintOwnerKind,
    BlueprintApplicationStatus,
    ProjectStateStatus,
    PreferenceStatus,
    WorkItemSourceKind,
    WorkItemStatus,
    WorkResultStatus,
    WorkResourceRole,
    DirectiveTarget,
    Origin,
    ResolutionReason,
    ConflictKind,
    EvidenceCategory,
    GenerationReferenceState,
    Staleness,
    RelationKind,
    Direction,
    Dispatch,
    TargetScope,
    Resolution,
    Support,
    Freshness,
    OccurrenceKind,
    UnresolvedReason,
    ResourceKind,
    ResourceRole,
    ResourceLanguage,
    ResourceState,
    SymbolKind,
    Visibility,
    StructuralCoverage,
    ResultSource,
    CoverageLimit,
    NotCurrentReason,
    RangeRole,
    SourceRequirement,
    GenerationBasis,
    ImpactIntent,
    Relevance,
);

/// A struct: every field, in declaration order.
macro_rules! fields {
    ($($ty:ty { $($field:ident),* $(,)? })*) => {
        $(impl Canonical for $ty {
            fn encode(&self, out: &mut Canon) {
                $(self.$field.encode(out);)*
            }
        })*
    };
}

fields! {
    SourcePoint { line, column }
    SourceSpan { start_byte, end_byte, start, end }
    Provenance { source_kind, locator, revision }
    Policy {
        uid, scope, policy_key, title, rule_text, structured_rule, protection_class,
        priority_class, status, provenance, created_at, updated_at,
    }
    Decision {
        uid, scope, topic, chosen_summary, rationale, status, provenance, created_at, updated_at,
    }
    UserPreference {
        uid, scope, preference_key, value, status, provenance, created_at, updated_at,
    }
    ProjectState { uid, key, scope, value, status, provenance, updated_at }
    BlueprintComponent { name, description }
    BlueprintRelationship { from, to, kind, description }
    BlueprintDefinition { components, relationships, constraints }
    Blueprint {
        uid, scope, blueprint_key, title, intent, definition, status, version, provenance,
        created_at, updated_at,
    }
    BlueprintRef { owner, uid }
    BlueprintApplication {
        uid, blueprint, scope, status, application_summary, provenance, created_at, updated_at,
    }
    BlueprintEvidence { application, definition }
    WorkItem { uid, source_kind, source_ref, title, goal, status, created_at, closed_at }
    WorkingState {
        work_item, baseline_workspace_revision, baseline_index_incarnation,
        baseline_generation_no, baseline_head, baseline_dirty, current_step, progress_summary,
        remaining_summary, blocker_summary, owner_agent, last_observed_workspace_revision,
        updated_at,
    }
    WorkResult {
        work_item, result_status, result_summary, commit_id, change_set_fingerprint,
        verification_summary, result_workspace_revision, result_index_incarnation,
        result_generation_no, remaining_dirty, created_at,
    }
    WorkResource {
        work_item, resource, role, locator_hint, first_observed_revision, last_observed_revision,
    }
    WorkHandoff {
        work_item, handoff_summary, remaining_summary, blocker_summary, next_scope_hint,
        created_at,
    }
    RequestDirective { id, target, subject_key, scope, summary }
    EvidenceRef { category, id, origin, scope, layer, source_kind, status }
    KnowledgeConflict { kind, subject, involved }
    WorkOverlap { other, other_status, resource, this_roles, other_roles }
    GenerationReference { index_incarnation, generation_no, workspace_revision, state }
    ExternalEntity {
        package_identity, module_path, symbol_name, qualified_name, kind, resolved_version,
        declaration_locator,
    }
    DomainEntity { kind, normalized_identity, namespace, method, display_label }
    EvidenceLocation {
        resource, containing_symbol, occurrence_kind, span, basis_revision, support, freshness,
    }
    RelationResult {
        kind, source, target, direction, dispatch, target_scope, resolution, support, freshness,
        evidence,
    }
    RelationGap {
        location, intended, lookup_name, module_hint, reason, resolution, candidates,
        candidate_truncated, resolution_context_key,
    }
    RelatedTestCandidate {
        resource, path_rel, endpoints, distance, basis, paths, paths_truncated, support,
        freshness,
    }
    TestPath { hops }
    Resource {
        id, path_rel, path_key, kind, role, language, size_bytes, mtime_ns, fingerprint,
        content_hash, state, resource_revision, generated_kind, container_resource_id,
    }
    Symbol {
        id, resource_id, parent_id, kind, name, qualified_name, signature, visibility, exported,
        span, resource_revision, analysis_profile_id,
    }
    SymbolCandidate { symbol, path_rel, coverage }
    CoverageNote { resource_id, path_rel, coverage }
    SourceVerification { expected_content_hash, observed_content_hash, currentness }
    PreparedRange { resource, path_rel, resource_revision, span, source, role, verification }
    TargetSelection { selector, located }
    CoverageEvidence { subject, report, confirmed }
    PlannedSourceRange { resource, resource_revision, span, role, requirement }
    DeliveryHint { relevance, impact_depth }
    ProjectionKnowledgeRefs { decision_topics, preference_keys, state_keys, blueprint_applications }
    ProjectionCorrelation {
        client_id, session_id, external_task_id, external_subtask_id, role_hint, team_hint,
        persona_traits,
    }
    SymbolTarget { name, resource, kind, language }
}

// ------------------------------------------------------- the rest by hand

impl Canonical for KnowledgeScope {
    fn encode(&self, out: &mut Canon) {
        self.kind().encode(out);
        self.key().encode(out);
    }
}

impl Canonical for CoverageReport {
    fn encode(&self, out: &mut Canon) {
        self.limits().encode(out);
    }
}

impl<T: Canonical> Canonical for Resolved<T> {
    fn encode(&self, out: &mut Canon) {
        self.item.encode(out);
        self.origin.encode(out);
        self.layer.encode(out);
        self.reason.encode(out);
    }
}

impl<T: Canonical> Canonical for Located<T> {
    fn encode(&self, out: &mut Canon) {
        self.candidates.encode(out);
        self.last_valid.encode(out);
        self.exact_selector.encode(out);
        self.truncated.encode(out);
        self.currentness.encode(out);
        self.source.encode(out);
        self.incomplete_coverage.encode(out);
    }
}

impl Canonical for TypedValue {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Text(text) => {
                out.tag(0);
                text.encode(out);
            }
            Self::Integer(number) => {
                out.tag(1);
                number.encode(out);
            }
            Self::Boolean(flag) => {
                out.tag(2);
                flag.encode(out);
            }
            Self::Json(value) => {
                out.tag(3);
                value.encode(out);
            }
        }
    }
}

impl Canonical for ShadowedItem {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Policy(policy) => {
                out.tag(0);
                policy.encode(out);
            }
            Self::Decision(decision) => {
                out.tag(1);
                decision.encode(out);
            }
            Self::Preference(preference) => {
                out.tag(2);
                preference.encode(out);
            }
        }
    }
}

impl Canonical for DirtyObservation {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Unknown => out.tag(0),
            Self::Clean => out.tag(1),
            Self::Dirty { fingerprint } => {
                out.tag(2);
                fingerprint.encode(out);
            }
        }
    }
}

impl Canonical for BlueprintDefinitionState {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Available(blueprint) => {
                out.tag(0);
                blueprint.encode(out);
            }
            Self::Missing => out.tag(1),
            Self::Retired => out.tag(2),
        }
    }
}

impl Canonical for GraphEndpoint {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Resource(id) => {
                out.tag(0);
                id.encode(out);
            }
            Self::Symbol(id) => {
                out.tag(1);
                id.encode(out);
            }
            Self::External(entity) => {
                out.tag(2);
                entity.encode(out);
            }
            Self::Domain(entity) => {
                out.tag(3);
                entity.encode(out);
            }
            Self::Logical(id) => {
                out.tag(4);
                id.encode(out);
            }
        }
    }
}

impl Canonical for IntendedRelation {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Known(kind) => {
                out.tag(0);
                kind.encode(out);
            }
            Self::Inheritance => out.tag(1),
        }
    }
}

impl Canonical for ProjectionBasis {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::DirectRelation(kind) => {
                out.tag(0);
                kind.encode(out);
            }
            Self::RelationPath { hops, first } => {
                out.tag(1);
                hops.encode(out);
                first.encode(out);
            }
        }
    }
}

impl Canonical for Currentness {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Current => out.tag(0),
            Self::NotCurrent(reason) => {
                out.tag(1);
                reason.encode(out);
            }
        }
    }
}

impl Canonical for SourceUnavailable {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::StaleBasis {
                basis_revision,
                current_revision,
            } => {
                out.tag(0);
                basis_revision.encode(out);
                current_revision.encode(out);
            }
            Self::SourceChanged {
                expected_content_hash,
                observed_content_hash,
            } => {
                out.tag(1);
                expected_content_hash.encode(out);
                observed_content_hash.encode(out);
            }
            Self::SymbolNotCurrent {
                symbol_revision,
                resource_revision,
            } => {
                out.tag(2);
                symbol_revision.encode(out);
                resource_revision.encode(out);
            }
            Self::NoCurrentSource { detail } => {
                out.tag(3);
                detail.encode(out);
            }
            Self::SpanNotReadable { detail } => {
                out.tag(4);
                detail.encode(out);
            }
        }
    }
}

impl Canonical for CoverageSubject {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::TargetSelection(target) => {
                out.tag(0);
                target.encode(out);
            }
            Self::Relations {
                anchor,
                direction,
                kinds,
            } => {
                out.tag(1);
                anchor.encode(out);
                direction.encode(out);
                kinds.encode(out);
            }
            Self::Impact { root, intent } => {
                out.tag(2);
                root.encode(out);
                intent.encode(out);
            }
            Self::RelatedTests { target, intent } => {
                out.tag(3);
                target.encode(out);
                intent.encode(out);
            }
        }
    }
}

impl Canonical for ProjectionTarget {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Endpoint(endpoint) => {
                out.tag(0);
                endpoint.encode(out);
            }
            Self::Resource(target) => {
                out.tag(1);
                target.encode(out);
            }
            Self::Symbol(target) => {
                out.tag(2);
                target.encode(out);
            }
        }
    }
}

impl Canonical for ResourceTarget {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Id(id) => {
                out.tag(0);
                id.encode(out);
            }
            Self::Path(text) => {
                out.tag(1);
                text.encode(out);
            }
            Self::Basename(text) => {
                out.tag(2);
                text.encode(out);
            }
            Self::PathPrefix(text) => {
                out.tag(3);
                text.encode(out);
            }
        }
    }
}

impl Canonical for SymbolName {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Id(id) => {
                out.tag(0);
                id.encode(out);
            }
            Self::QualifiedName(text) => {
                out.tag(1);
                text.encode(out);
            }
            Self::Name(text) => {
                out.tag(2);
                text.encode(out);
            }
            Self::PartialName(text) => {
                out.tag(3);
                text.encode(out);
            }
        }
    }
}

impl Canonical for ChangeKind {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Structural(intent) => {
                out.tag(0);
                intent.encode(out);
            }
            Self::Delete => out.tag(1),
            Self::DomainContractChange => out.tag(2),
        }
    }
}

impl Canonical for ProjectionIntent {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Locate => out.tag(0),
            Self::Understand => out.tag(1),
            Self::Change(kind) => {
                out.tag(2);
                kind.encode(out);
            }
            Self::Impact(kind) => {
                out.tag(3);
                kind.encode(out);
            }
            Self::ResumeHandoff => out.tag(4),
        }
    }
}

impl Canonical for ProjectionGap {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::TargetAmbiguous => out.tag(0),
            Self::TargetNotExact => out.tag(1),
            Self::TargetNotFound => out.tag(2),
            Self::TargetNotFoundWithIncompleteCoverage => out.tag(3),
            Self::TargetNotCurrent => out.tag(4),
            Self::UnsupportedImpactProfile(kind) => {
                out.tag(5);
                kind.encode(out);
            }
            Self::DependencyExpansionUndefined => out.tag(6),
            Self::BlueprintApplicationNotApplied(id) => {
                out.tag(7);
                id.encode(out);
            }
            Self::RequiresSemantics => out.tag(8),
            Self::NotCurrent => out.tag(9),
        }
    }
}

impl Canonical for EvidenceItem {
    fn encode(&self, out: &mut Canon) {
        match self {
            Self::Resource(resource) => {
                out.tag(0);
                resource.encode(out);
            }
            Self::Symbol(candidate) => {
                out.tag(1);
                candidate.encode(out);
            }
            Self::Relation(relation) => {
                out.tag(2);
                relation.encode(out);
            }
            Self::RelationGap(gap) => {
                out.tag(3);
                gap.encode(out);
            }
            Self::Policy(entry) => {
                out.tag(4);
                entry.encode(out);
            }
            Self::Decision(entry) => {
                out.tag(5);
                entry.encode(out);
            }
            Self::Preference(entry) => {
                out.tag(6);
                entry.encode(out);
            }
            Self::Blueprint(entry) => {
                out.tag(7);
                entry.encode(out);
            }
            Self::ProjectState(entry) => {
                out.tag(8);
                entry.encode(out);
            }
            Self::WorkItem(item) => {
                out.tag(9);
                item.encode(out);
            }
            Self::WorkingState(state) => {
                out.tag(10);
                state.encode(out);
            }
            Self::WorkResult(result) => {
                out.tag(11);
                result.encode(out);
            }
            Self::Handoff(handoff) => {
                out.tag(12);
                handoff.encode(out);
            }
            Self::CurrentSource(range) => {
                out.tag(13);
                range.encode(out);
            }
            Self::Directive(entry) => {
                out.tag(14);
                entry.encode(out);
            }
            Self::RelatedTest { target, candidate } => {
                out.tag(15);
                target.encode(out);
                candidate.encode(out);
            }
            Self::KnowledgeConflict(conflict) => {
                out.tag(16);
                conflict.encode(out);
            }
            Self::WorkOverlap { work_item, overlap } => {
                out.tag(17);
                work_item.encode(out);
                overlap.encode(out);
            }
            Self::GenerationReference {
                work_item,
                basis,
                reference,
            } => {
                out.tag(18);
                work_item.encode(out);
                basis.encode(out);
                reference.encode(out);
            }
            Self::WorkStaleness {
                work_item,
                staleness,
            } => {
                out.tag(19);
                work_item.encode(out);
                staleness.encode(out);
            }
            Self::TargetSelection(selection) => {
                out.tag(20);
                selection.encode(out);
            }
            Self::Coverage(coverage) => {
                out.tag(21);
                coverage.encode(out);
            }
            Self::SourceUnavailable {
                resource,
                span,
                reason,
            } => {
                out.tag(22);
                resource.encode(out);
                span.encode(out);
                reason.encode(out);
            }
            Self::IndexCurrentness {
                workspace,
                currentness,
            } => {
                out.tag(23);
                workspace.encode(out);
                currentness.encode(out);
            }
        }
    }
}

// ------------------------------------------------------------ fingerprints

/// The request's logical identity. Set-like fields are canonical: a
/// layer's scopes and the directives are sorted; layer order is meaning.
impl Canonical for ProjectionRequest {
    fn encode(&self, out: &mut Canon) {
        self.workspace.encode(out);
        self.intent.encode(out);
        self.target.encode(out);
        self.work_item.encode(out);
        out.int(self.scope_layers.len() as u64);
        for layer in &self.scope_layers {
            let sorted: BTreeSet<&KnowledgeScope> = layer.iter().collect();
            sorted.encode(out);
        }
        let mut directives: Vec<&RequestDirective> = self.directives.iter().collect();
        directives.sort_by(|a, b| a.id.cmp(&b.id));
        directives.encode(out);
        self.knowledge.encode(out);
        self.correlation.encode(out);
    }
}

/// Everything a page sequence is cut from: the evidence (required source
/// bodies included), gaps, the source plan's metadata, and the sidecar.
/// Planner statistics and timings are not part of it.
impl Canonical for PreparedProjection {
    fn encode(&self, out: &mut Canon) {
        self.workspace.encode(out);
        self.project.encode(out);
        self.intent.encode(out);
        self.target.encode(out);
        self.evidence.encode(out);
        self.gaps.encode(out);
        self.source_plan.encode(out);
        self.delivery.encode(out);
    }
}
