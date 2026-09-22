//! The negative-answer contract shared by every relation surface
//! (#17 task 14).
//!
//! Tasks 8-13 each learned to say what they could not see: unresolved
//! sites, candidate truncation, unsupported constructs, degraded
//! support and freshness, exhausted traversal budgets, unknown test
//! roles. Each also grew its own `is_complete()`. This module is the
//! one place that decides what those facts mean, so that
//!
//! ```text
//! confirmed_count() == 0
//! ```
//!
//! can never on its own be read as "none exist".
//!
//! ## Three states, not a score
//!
//! [`AnswerState`] is the whole contract: results were found, or none
//! were found *and the scope was completely covered*, or none were
//! found and something specific stopped the scope from being complete.
//! The third case names its reasons -- [`CoverageLimit`] is a closed
//! vocabulary, one variant per dimension the tiers below already know
//! about, so a caller is never handed a generic `unknown` when the
//! index knows exactly which thing it could not see.
//!
//! There is deliberately no confidence number here. A limit is present
//! or it is not; a negative answer is safe or it is not.
//!
//! ## Where the limits come from
//!
//! Each surface builds its own [`CoverageReport`] from the counters it
//! already keeps -- [`crate::relations::Coverage`],
//! [`crate::impact::ImpactCoverage`], [`crate::related_tests::
//! TestCoverage`] -- through the helpers here, rather than inventing a
//! slightly different completeness rule of its own.
//!
//! Source preparation (#17 task 9) is *not* one of these limits: a
//! confirmed relation whose current source cannot be read is still a
//! confirmed relation, and a successfully read file does not make an
//! incomplete graph complete. That axis stays on
//! [`crate::prepare::PreparedInspection::source_complete`].

use std::fmt;

use crate::resolution::{Freshness, Support};

/// One specific reason a scope is not completely covered.
///
/// Closed vocabulary. Every variant corresponds to state the tiers
/// below already record -- nothing here is inferred, and nothing is
/// collapsed into a generic "unknown" when the specific reason is
/// known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CoverageLimit {
    /// Use sites of a matching kind exist with no confirmed target.
    UnresolvedEvidence,
    /// Some of those had canonical candidates: picking one would be a
    /// guess.
    AmbiguousCandidates,
    /// Some of those need a semantic backend (I4) to answer at all.
    RequiresSemantics,
    /// Some of those are constructs this tier does not model.
    UnsupportedConstruct,
    /// A candidate list was cut (#17 task 7), so the candidates shown
    /// are not all of them.
    CandidateTruncated,
    /// Unresolved sites exist that cannot be attributed to the queried
    /// target without matching by name, which is never done.
    UnattributedGaps,
    /// A reverse query: an unresolved site names no target, so the gap
    /// list is only what could be attributed by stored candidate
    /// identity. Workspace-global completeness is not claimable from
    /// this side.
    ReverseScopeNotEnumerable,
    /// The owning Resource is only partially covered structurally, so
    /// evidence may be missing because extraction was partial.
    PartialSupport,
    /// The owning Resource's language or construct coverage is not
    /// modelled at all.
    UnsupportedScope,
    /// Evidence describes a revision the Resource has moved past.
    StaleEvidence,
    /// The relation component for the scope is DIRTY: what is returned
    /// is the last valid publication, not current truth.
    DirtyRelationComponent,
    /// A traversal budget stopped the walk (#17 task 10). Separate
    /// from candidate truncation and from relation coverage.
    TraversalTruncated,
    /// A related-test candidate's supporting-path list was cut.
    SupportingPathTruncated,
    /// A Resource in scope has no known role, so a test among them
    /// would not have been recognised.
    UnknownResourceRole,
    /// A Resource in scope could not be read at all.
    UnreadableResourceOwner,
    /// The index as a whole is not current.
    IndexNotCurrent,
}

impl CoverageLimit {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnresolvedEvidence => "UNRESOLVED_EVIDENCE",
            Self::AmbiguousCandidates => "AMBIGUOUS_CANDIDATES",
            Self::RequiresSemantics => "REQUIRES_SEMANTICS",
            Self::UnsupportedConstruct => "UNSUPPORTED_CONSTRUCT",
            Self::CandidateTruncated => "CANDIDATE_TRUNCATED",
            Self::UnattributedGaps => "UNATTRIBUTED_GAPS",
            Self::ReverseScopeNotEnumerable => "REVERSE_SCOPE_NOT_ENUMERABLE",
            Self::PartialSupport => "PARTIAL_SUPPORT",
            Self::UnsupportedScope => "UNSUPPORTED_SCOPE",
            Self::StaleEvidence => "STALE_EVIDENCE",
            Self::DirtyRelationComponent => "DIRTY_RELATION_COMPONENT",
            Self::TraversalTruncated => "TRAVERSAL_TRUNCATED",
            Self::SupportingPathTruncated => "SUPPORTING_PATH_TRUNCATED",
            Self::UnknownResourceRole => "UNKNOWN_RESOURCE_ROLE",
            Self::UnreadableResourceOwner => "UNREADABLE_RESOURCE_OWNER",
            Self::IndexNotCurrent => "INDEX_NOT_CURRENT",
        }
    }
}

impl fmt::Display for CoverageLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a result set is allowed to claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerState {
    /// One or more confirmed results.
    Confirmed,
    /// Zero confirmed results, and the scope really was completely
    /// covered: reading this as "none exist in this scope" is safe.
    NoneUnderCompleteCoverage,
    /// Zero confirmed results, and at least one [`CoverageLimit`]
    /// prevents concluding anything from that.
    NoneWithIncompleteCoverage,
}

impl AnswerState {
    /// Whether an empty result may be reported as "there are none".
    #[must_use]
    pub const fn is_safe_negative(self) -> bool {
        matches!(self, Self::NoneUnderCompleteCoverage)
    }
}

/// The limits standing between a result set and a complete-negative
/// conclusion.
///
/// Sorted and deduplicated, so the same scope always reports the same
/// list in the same order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageReport {
    limits: Vec<CoverageLimit>,
}

impl CoverageReport {
    #[must_use]
    pub const fn new() -> Self {
        Self { limits: Vec::new() }
    }

    /// Record a limit. Recording the same one twice changes nothing --
    /// two unresolved sites are one reason, not two.
    pub fn note(&mut self, limit: CoverageLimit) {
        if let Err(at) = self.limits.binary_search(&limit) {
            self.limits.insert(at, limit);
        }
    }

    /// Record a limit when `present`.
    pub fn note_if(&mut self, present: bool, limit: CoverageLimit) {
        if present {
            self.note(limit);
        }
    }

    /// Record what a [`Support`] weaker than SUPPORTED means.
    pub fn note_support(&mut self, support: Support) {
        match support {
            Support::Supported => {}
            Support::Partial => self.note(CoverageLimit::PartialSupport),
            Support::Unsupported => self.note(CoverageLimit::UnsupportedScope),
        }
    }

    /// Record what a [`Freshness`] weaker than FRESH means. DIRTY is
    /// the relation component not being current; STALE is evidence
    /// describing a revision that has moved.
    pub fn note_freshness(&mut self, freshness: Freshness) {
        match freshness {
            Freshness::Fresh => {}
            Freshness::Dirty => self.note(CoverageLimit::DirtyRelationComponent),
            Freshness::Stale => self.note(CoverageLimit::StaleEvidence),
        }
    }

    /// Fold another report's limits in.
    pub fn merge(&mut self, other: &Self) {
        for limit in &other.limits {
            self.note(*limit);
        }
    }

    #[must_use]
    pub fn limits(&self) -> &[CoverageLimit] {
        &self.limits
    }

    #[must_use]
    pub fn has(&self, limit: CoverageLimit) -> bool {
        self.limits.binary_search(&limit).is_ok()
    }

    /// Whether the scope is covered completely enough for zero to mean
    /// none.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.limits.is_empty()
    }

    /// The answer state for a result set of this size.
    #[must_use]
    pub fn state(&self, confirmed: usize) -> AnswerState {
        if confirmed > 0 {
            AnswerState::Confirmed
        } else if self.is_complete() {
            AnswerState::NoneUnderCompleteCoverage
        } else {
            AnswerState::NoneWithIncompleteCoverage
        }
    }
}

impl FromIterator<CoverageLimit> for CoverageReport {
    fn from_iter<I: IntoIterator<Item = CoverageLimit>>(iterator: I) -> Self {
        let mut report = Self::new();
        for limit in iterator {
            report.note(limit);
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_with_no_limits_makes_zero_a_safe_negative() {
        let report = CoverageReport::new();
        assert!(report.is_complete());
        assert_eq!(report.state(0), AnswerState::NoneUnderCompleteCoverage);
        assert!(report.state(0).is_safe_negative());
        assert_eq!(report.state(3), AnswerState::Confirmed);
    }

    #[test]
    fn one_limit_is_enough_to_block_a_negative_conclusion() {
        let mut report = CoverageReport::new();
        report.note(CoverageLimit::UnresolvedEvidence);
        assert_eq!(report.state(0), AnswerState::NoneWithIncompleteCoverage);
        assert!(!report.state(0).is_safe_negative());
        // Confirmed results stay confirmed: a gap elsewhere does not
        // unconfirm an edge that is stored.
        assert_eq!(report.state(1), AnswerState::Confirmed);
    }

    #[test]
    fn limits_are_deduplicated_and_deterministically_ordered() {
        let mut left = CoverageReport::new();
        left.note(CoverageLimit::TraversalTruncated);
        left.note(CoverageLimit::UnresolvedEvidence);
        left.note(CoverageLimit::UnresolvedEvidence);
        let mut right = CoverageReport::new();
        right.note(CoverageLimit::UnresolvedEvidence);
        right.note(CoverageLimit::TraversalTruncated);
        assert_eq!(left, right);
        assert_eq!(left.limits().len(), 2);
        assert!(left.has(CoverageLimit::TraversalTruncated));
        assert!(!left.has(CoverageLimit::StaleEvidence));
    }

    #[test]
    fn support_and_freshness_map_to_their_own_reasons() {
        let mut report = CoverageReport::new();
        report.note_support(Support::Supported);
        report.note_freshness(Freshness::Fresh);
        assert!(report.is_complete());
        report.note_support(Support::Partial);
        report.note_freshness(Freshness::Dirty);
        report.note_freshness(Freshness::Stale);
        report.note_support(Support::Unsupported);
        assert_eq!(
            report.limits(),
            [
                CoverageLimit::PartialSupport,
                CoverageLimit::UnsupportedScope,
                CoverageLimit::StaleEvidence,
                CoverageLimit::DirtyRelationComponent,
            ]
        );
    }

    #[test]
    fn merge_unions_without_duplicating() {
        let mut left: CoverageReport = [CoverageLimit::UnresolvedEvidence].into_iter().collect();
        let right: CoverageReport = [
            CoverageLimit::UnresolvedEvidence,
            CoverageLimit::TraversalTruncated,
        ]
        .into_iter()
        .collect();
        left.merge(&right);
        assert_eq!(left, right);
    }
}
