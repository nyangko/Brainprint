//! Structural group / boundary summary (#20 task 9, #22).
//!
//! The view an Agent otherwise rebuilds with `find | grep | awk | sort |
//! uniq`: which Resources fall into which structural bucket, how many
//! confirmed relations stay inside a bucket, which cross between
//! buckets, and what the index could not see while counting them.
//!
//! ## Structural, not semantic
//!
//! A group is only ever what the caller asked for: an explicit
//! path-prefix rule, an explicit directory depth under an explicit root,
//! or a classification the Resource model already stored. Nothing here
//! reads a directory named `features` or `domain` as a Feature, and no
//! group is persisted -- a [`StructuralGroup`] is request-local identity
//! derived inside the query and forgotten with it.
//!
//! ## Counted in SQLite, not in Rust
//!
//! Every statement aggregates before anything crosses into Rust: the
//! request's grouping rule is a bound `CASE` (or a bounded recursive
//! split of the path) over the scoped Resource rows, endpoints are
//! projected onto their owning Resource by join, and relations are
//! counted by canonical `relation.id`, never by Occurrence. Core decodes
//! the aggregate rows, orders them, and computes fan counts and cycles
//! over the aggregated group graph. The statement count per request is
//! fixed; it does not grow with Resources or relations.
//!
//! ## What a zero is allowed to mean
//!
//! Each group carries its own coverage counters, folded through the
//! shared [`CoverageReport`] vocabulary, so "no boundary edges" is a
//! safe negative only when nothing -- unresolved sites, partial or
//! unsupported structure, dirty components, semantic conflicts, an index
//! that is not current -- stands in the way.
//!
//! Index-only: no source is read and no semantic backend is started.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    num::NonZeroUsize,
    path::Path,
};

use brainprint_core::{ResourceId, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, Row, params_from_iter, types::Value};

use crate::{
    component::{self, ComponentError, FreshnessState},
    coverage::{AnswerState, CoverageLimit, CoverageReport},
    db::DbOpenError,
    gaps::{GapError, IntendedRelation, UnresolvedReason},
    graph::{GraphError, RelationKind},
    query::{self, Currentness, NotCurrentReason},
    relations,
    resource::{ResourceError, ResourceKind, ResourceLanguage, ResourceRole},
    schema,
};

#[cfg(test)]
mod tests;

/// Implementation safety limits on caller-supplied rules. They bound the
/// generated SQL and the returned sample; they say nothing about the
/// Workspace.
pub const MAX_PATH_RULES: usize = 64;
pub const MAX_LABEL_BYTES: usize = 128;
pub const MAX_PREFIX_BYTES: usize = 512;
pub const MAX_DIRECTORY_DEPTH: usize = 16;
pub const MAX_MEMBER_SAMPLE: usize = 64;

/// One explicit path rule: Resources under `prefix` belong to `label`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathGroupRule {
    pub label: String,
    /// Workspace-relative directory prefix, e.g. `src/features/billing/`.
    pub prefix: String,
}

/// The one grouping dimension a request uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupingSpec {
    /// Longest matching prefix wins.
    PathPrefixes(Vec<PathGroupRule>),
    /// The first `depth` directory segments under `root`. A Resource with
    /// fewer segments than that, or outside `root`, is Ungrouped.
    DirectoryDepth {
        root: String,
        depth: usize,
    },
    ResourceRole,
    /// A Resource with no language is Ungrouped.
    ResourceLanguage,
    ResourceKind,
}

/// Which Resources the summary covers. ACTIVE Resources only, always.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceScope {
    pub path_prefix: Option<String>,
    pub role: Option<ResourceRole>,
    pub language: Option<ResourceLanguage>,
    pub kind: Option<ResourceKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralSummaryRequest {
    pub workspace: WorkspaceId,
    pub grouping: GroupingSpec,
    pub resource_scope: ResourceScope,
    /// Confirmed relation kinds to aggregate. Empty means every kind.
    pub relation_kinds: Vec<RelationKind>,
    /// When false, the Ungrouped bucket and the edges touching it are
    /// left out. Every other group's metrics are unchanged.
    pub include_ungrouped: bool,
    pub include_cycles: bool,
    pub member_sample_limit: Option<NonZeroUsize>,
}

/// Where a summary's groups came from. Always structural.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBasis {
    PathPrefix,
    DirectoryDepth,
    ResourceRole,
    ResourceLanguage,
    ResourceKind,
}

/// Request-local group identity: the value the request's one grouping
/// rule produced. Not a stable id and never stored.
///
/// Ordered by value, with Ungrouped last.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StructuralGroup {
    /// A path-rule label, a directory path under the root, or a stored
    /// classification value (`SOURCE`, `TYPESCRIPT`, `FILE`, ...).
    Group(String),
    Ungrouped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSample {
    pub resource: ResourceId,
    pub path: String,
    pub role: ResourceRole,
    pub language: Option<ResourceLanguage>,
    pub kind: ResourceKind,
}

/// What one group's counts do not cover.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupCoverage {
    /// Resources by derived [`crate::resolution::Support`].
    pub supported: u64,
    pub partial: u64,
    pub unsupported: u64,
    /// Resources whose structure is not current (or was never recorded).
    pub structure_not_current: u64,
    /// Resources whose relation publication is DIRTY.
    pub relation_dirty: u64,
    /// Owned use sites with no confirmed target and no candidates.
    pub unresolved_gaps: u64,
    /// Owned use sites with canonical candidates. Still not edges.
    pub candidate_gaps: u64,
    pub requires_semantics: u64,
    pub unsupported_construct: u64,
    pub candidate_truncated: u64,
    pub semantic_conflicts: u64,
    /// Resources whose semantic contribution is not CURRENT.
    pub semantic_not_current: u64,
}

impl GroupCoverage {
    #[must_use]
    pub fn limits(&self) -> CoverageReport {
        let mut report = CoverageReport::new();
        report.note_if(
            self.unresolved_gaps + self.candidate_gaps > 0,
            CoverageLimit::UnresolvedEvidence,
        );
        report.note_if(self.candidate_gaps > 0, CoverageLimit::AmbiguousCandidates);
        report.note_if(
            self.requires_semantics > 0,
            CoverageLimit::RequiresSemantics,
        );
        report.note_if(
            self.unsupported_construct > 0,
            CoverageLimit::UnsupportedConstruct,
        );
        report.note_if(
            self.candidate_truncated > 0,
            CoverageLimit::CandidateTruncated,
        );
        report.note_if(self.partial > 0, CoverageLimit::PartialSupport);
        report.note_if(self.unsupported > 0, CoverageLimit::UnsupportedScope);
        report.note_if(
            self.structure_not_current + self.relation_dirty > 0,
            CoverageLimit::DirtyRelationComponent,
        );
        report.note_if(self.semantic_conflicts > 0, CoverageLimit::SemanticConflict);
        report.note_if(
            self.semantic_not_current > 0,
            CoverageLimit::SemanticNotCurrent,
        );
        report
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSummary {
    pub group: StructuralGroup,
    /// The normalized prefixes behind a path-rule group; empty otherwise.
    pub prefixes: Vec<String>,
    pub resources: u64,
    /// Confirmed canonical relations with both ends in this group.
    pub internal_edges: u64,
    /// Distinct confirmed relations leaving / entering this group.
    pub outgoing_edges: u64,
    pub incoming_edges: u64,
    /// Distinct neighbor groups, not edges.
    pub fan_out_groups: usize,
    pub fan_in_groups: usize,
    pub coverage: GroupCoverage,
    /// First N by path, only when a sample was asked for.
    pub members: Vec<MemberSample>,
}

/// Confirmed canonical relations of one kind from one group to another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryEdge {
    pub source: StructuralGroup,
    pub target: StructuralGroup,
    pub kind: RelationKind,
    pub confirmed_edges: u64,
}

/// Unresolved use sites owned by one group, by intended kind and reason.
/// Never attributed to a target group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapAggregate {
    pub group: StructuralGroup,
    pub intended: IntendedRelation,
    pub reason: UnresolvedReason,
    pub unresolved: u64,
    pub candidate: u64,
    pub candidate_truncated: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralSummary {
    pub basis: GroupBasis,
    /// The kinds aggregated, sorted. Empty means every kind.
    pub relation_kinds: Vec<RelationKind>,
    pub currentness: Currentness,
    pub groups: Vec<GroupSummary>,
    pub boundary_edges: Vec<BoundaryEdge>,
    /// Strongly connected groups (size > 1) of the returned boundary
    /// edges. `None` when not asked for.
    pub cycles: Option<Vec<Vec<StructuralGroup>>>,
    pub gaps: Vec<GapAggregate>,
}

impl StructuralSummary {
    #[must_use]
    pub fn boundary_edge_count(&self) -> u64 {
        self.boundary_edges
            .iter()
            .map(|edge| edge.confirmed_edges)
            .sum()
    }

    /// Everything standing between these counts and a complete answer.
    #[must_use]
    pub fn limits(&self) -> CoverageReport {
        let mut report = CoverageReport::new();
        report.note_if(
            !self.currentness.is_current(),
            CoverageLimit::IndexNotCurrent,
        );
        for group in &self.groups {
            report.merge(&group.coverage.limits());
        }
        report
    }

    /// Whether zero boundary edges may be read as "none".
    #[must_use]
    pub fn answer_state(&self) -> AnswerState {
        self.limits()
            .state(usize::try_from(self.boundary_edge_count()).unwrap_or(usize::MAX))
    }
}

/// Statements run and aggregate rows read, for tests and benchmarks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SummaryStats {
    pub statements: u64,
    pub rows: u64,
}

/// A request that cannot be run as asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryRequestError {
    NoPathRules,
    TooManyPathRules {
        count: usize,
    },
    EmptyLabel,
    LabelTooLong {
        label: String,
    },
    EmptyPrefix,
    PrefixTooLong,
    AbsolutePrefix {
        prefix: String,
    },
    EscapingPrefix {
        prefix: String,
    },
    /// The same prefix under two labels.
    DuplicatePrefix {
        prefix: String,
    },
    /// Two spellings of one prefix under two labels: neither is more
    /// specific, so neither can win.
    AmbiguousPrefix {
        prefix: String,
    },
    DepthZero,
    DepthTooLarge {
        depth: usize,
    },
    SampleTooLarge {
        limit: usize,
    },
}

impl fmt::Display for SummaryRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPathRules => {
                formatter.write_str("path-prefix grouping needs at least one rule")
            }
            Self::TooManyPathRules { count } => {
                write!(formatter, "{count} path rules exceed {MAX_PATH_RULES}")
            }
            Self::EmptyLabel => formatter.write_str("a path rule label is empty"),
            Self::LabelTooLong { label } => {
                write!(formatter, "label {label:?} exceeds {MAX_LABEL_BYTES} bytes")
            }
            Self::EmptyPrefix => formatter.write_str("a path prefix is empty"),
            Self::PrefixTooLong => {
                write!(formatter, "a path prefix exceeds {MAX_PREFIX_BYTES} bytes")
            }
            Self::AbsolutePrefix { prefix } => write!(formatter, "prefix {prefix:?} is absolute"),
            Self::EscapingPrefix { prefix } => {
                write!(formatter, "prefix {prefix:?} escapes the Workspace")
            }
            Self::DuplicatePrefix { prefix } => {
                write!(formatter, "prefix {prefix:?} has two different labels")
            }
            Self::AmbiguousPrefix { prefix } => write!(
                formatter,
                "prefix {prefix:?} is spelled twice with two different labels"
            ),
            Self::DepthZero => formatter.write_str("directory depth 0 groups nothing"),
            Self::DepthTooLarge { depth } => {
                write!(formatter, "depth {depth} exceeds {MAX_DIRECTORY_DEPTH}")
            }
            Self::SampleTooLarge { limit } => {
                write!(
                    formatter,
                    "member sample {limit} exceeds {MAX_MEMBER_SAMPLE}"
                )
            }
        }
    }
}

impl Error for SummaryRequestError {}

#[derive(Debug)]
pub enum SummaryError {
    Invalid(SummaryRequestError),
    WorkspaceUnbound,
    WorkspaceMismatch {
        bound: WorkspaceId,
        requested: WorkspaceId,
    },
    Open(DbOpenError),
    Sqlite(rusqlite::Error),
    Component(ComponentError),
    Graph(GraphError),
    Gap(GapError),
    Resource(ResourceError),
}

impl fmt::Display for SummaryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => write!(formatter, "invalid summary request: {error}"),
            Self::WorkspaceUnbound => formatter.write_str("index.db is bound to no Workspace"),
            Self::WorkspaceMismatch { bound, requested } => write!(
                formatter,
                "index.db belongs to Workspace {bound}, not {requested}"
            ),
            Self::Open(error) => write!(formatter, "{error}"),
            Self::Sqlite(error) => write!(formatter, "summary sqlite error: {error}"),
            Self::Component(error) => write!(formatter, "{error}"),
            Self::Graph(error) => write!(formatter, "{error}"),
            Self::Gap(error) => write!(formatter, "{error}"),
            Self::Resource(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for SummaryError {}

impl From<SummaryRequestError> for SummaryError {
    fn from(error: SummaryRequestError) -> Self {
        Self::Invalid(error)
    }
}

impl From<DbOpenError> for SummaryError {
    fn from(error: DbOpenError) -> Self {
        Self::Open(error)
    }
}

impl From<rusqlite::Error> for SummaryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<ComponentError> for SummaryError {
    fn from(error: ComponentError) -> Self {
        Self::Component(error)
    }
}

impl From<GraphError> for SummaryError {
    fn from(error: GraphError) -> Self {
        Self::Graph(error)
    }
}

impl From<GapError> for SummaryError {
    fn from(error: GapError) -> Self {
        Self::Gap(error)
    }
}

impl From<ResourceError> for SummaryError {
    fn from(error: ResourceError) -> Self {
        Self::Resource(error)
    }
}

/// Read-only handle over one Workspace's `index.db`.
pub struct StructuralSummaryIndex {
    connection: Connection,
    stats: Cell<SummaryStats>,
}

impl StructuralSummaryIndex {
    pub fn open(path: &Path) -> Result<Self, SummaryError> {
        Ok(Self::from_connection(schema::index::open(path)?.connection))
    }

    #[must_use]
    pub fn from_connection(connection: Connection) -> Self {
        Self {
            connection,
            stats: Cell::new(SummaryStats::default()),
        }
    }

    #[must_use]
    pub fn stats(&self) -> SummaryStats {
        self.stats.get()
    }

    /// Summarize `request`. An invalid request fails before any query.
    pub fn summarize(
        &self,
        request: &StructuralSummaryRequest,
    ) -> Result<StructuralSummary, SummaryError> {
        let plan = Plan::of(request)?;
        // One snapshot for every statement, so the counts agree.
        let transaction = self.connection.unchecked_transaction()?;
        self.check_workspace(request.workspace)?;
        let currentness = match component::read(&self.connection)? {
            None => Currentness::NotCurrent(NotCurrentReason::ResourceIndexNeverPublished),
            Some(state) => match state.freshness_state {
                FreshnessState::Current => Currentness::Current,
                FreshnessState::Dirty => {
                    Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty)
                }
            },
        };

        let mut groups: BTreeMap<StructuralGroup, GroupSummary> = BTreeMap::new();
        for (label, prefix) in &plan.rules {
            group_entry(&mut groups, StructuralGroup::Group(label.clone()))
                .prefixes
                .push(prefix.clone());
        }
        if request.include_ungrouped {
            group_entry(&mut groups, StructuralGroup::Ungrouped);
        }

        let (sql, params) = plan.inventory_sql();
        for row in self.rows(&sql, params, |row| {
            Ok((
                group_of(row.get(0)?),
                count(row, 1)?,
                count(row, 2)?,
                count(row, 3)?,
                count(row, 4)?,
                count(row, 5)?,
            ))
        })? {
            let (group, resources, supported, partial, not_current, dirty) = row;
            let entry = group_entry(&mut groups, group);
            entry.resources = resources;
            entry.coverage.supported = supported;
            entry.coverage.partial = partial;
            entry.coverage.unsupported = resources - supported - partial;
            entry.coverage.structure_not_current = not_current;
            entry.coverage.relation_dirty = dirty;
        }

        let mut boundary_edges = Vec::new();
        let mut fan_out: BTreeMap<StructuralGroup, BTreeSet<StructuralGroup>> = BTreeMap::new();
        let mut fan_in: BTreeMap<StructuralGroup, BTreeSet<StructuralGroup>> = BTreeMap::new();
        let (sql, params) = plan.edge_sql();
        for row in self.rows(&sql, params, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                group_of(row.get(1)?),
                group_of(row.get(2)?),
                row.get::<_, Option<String>>(3)?,
                count(row, 4)?,
            ))
        })? {
            let (tag, source, target, kind, edges) = row;
            match tag {
                0 if source == target => group_entry(&mut groups, source).internal_edges += edges,
                0 => {
                    let kind = RelationKind::parse(kind.as_deref().unwrap_or_default())?;
                    fan_out
                        .entry(source.clone())
                        .or_default()
                        .insert(target.clone());
                    fan_in
                        .entry(target.clone())
                        .or_default()
                        .insert(source.clone());
                    boundary_edges.push(BoundaryEdge {
                        source,
                        target,
                        kind,
                        confirmed_edges: edges,
                    });
                }
                1 => group_entry(&mut groups, source).outgoing_edges = edges,
                _ => group_entry(&mut groups, target).incoming_edges = edges,
            }
        }
        for (group, neighbors) in fan_out {
            group_entry(&mut groups, group).fan_out_groups = neighbors.len();
        }
        for (group, neighbors) in fan_in {
            group_entry(&mut groups, group).fan_in_groups = neighbors.len();
        }

        let mut gaps = Vec::new();
        let (sql, params) = plan.gap_sql();
        for row in self.rows(&sql, params, |row| {
            Ok((
                group_of(row.get(0)?),
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                count(row, 3)?,
                count(row, 4)?,
                count(row, 5)?,
            ))
        })? {
            let (group, intended, reason, total, candidate, truncated) = row;
            let intended = IntendedRelation::parse(&intended)?;
            if !plan.kinds.is_empty() && !relations::intended_matches(intended, &plan.kinds) {
                continue;
            }
            let reason = UnresolvedReason::parse(&reason)?;
            let coverage = &mut group_entry(&mut groups, group.clone()).coverage;
            coverage.unresolved_gaps += total - candidate;
            coverage.candidate_gaps += candidate;
            coverage.candidate_truncated += truncated;
            if reason.requires_semantics() {
                coverage.requires_semantics += total;
            }
            if reason.is_unsupported_construct() {
                coverage.unsupported_construct += total;
            }
            gaps.push(GapAggregate {
                group,
                intended,
                reason,
                unresolved: total - candidate,
                candidate,
                candidate_truncated: truncated,
            });
        }

        let (sql, params) = plan.semantic_sql();
        for row in self.rows(&sql, params, |row| {
            Ok((row.get::<_, i64>(0)?, group_of(row.get(1)?), count(row, 2)?))
        })? {
            let (tag, group, value) = row;
            let coverage = &mut group_entry(&mut groups, group).coverage;
            if tag == 0 {
                coverage.semantic_not_current = value;
            } else {
                coverage.semantic_conflicts = value;
            }
        }

        if let Some(limit) = request.member_sample_limit {
            let (sql, params) = plan.sample_sql(limit);
            for row in self.rows(&sql, params, |row| {
                Ok((
                    group_of(row.get(0)?),
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })? {
                let (group, uid, path, role, language, kind) = row;
                let member = MemberSample {
                    resource: ResourceId::from_bytes(sixteen(&uid)),
                    path,
                    role: ResourceRole::parse_public(&role)?,
                    language: language
                        .as_deref()
                        .map(ResourceLanguage::parse)
                        .transpose()?,
                    kind: ResourceKind::parse(&kind)?,
                };
                group_entry(&mut groups, group).members.push(member);
            }
        }
        drop(transaction);

        if !request.include_ungrouped {
            groups.remove(&StructuralGroup::Ungrouped);
            boundary_edges.retain(|edge| {
                edge.source != StructuralGroup::Ungrouped
                    && edge.target != StructuralGroup::Ungrouped
            });
            gaps.retain(|gap| gap.group != StructuralGroup::Ungrouped);
        }
        boundary_edges.sort_by(|left, right| {
            (&left.source, &left.target, left.kind.as_str()).cmp(&(
                &right.source,
                &right.target,
                right.kind.as_str(),
            ))
        });
        gaps.sort_by(|left, right| {
            (&left.group, left.intended.as_str(), left.reason.as_str()).cmp(&(
                &right.group,
                right.intended.as_str(),
                right.reason.as_str(),
            ))
        });
        let cycles = request
            .include_cycles
            .then(|| strongly_connected(&boundary_edges));

        Ok(StructuralSummary {
            basis: plan.basis,
            relation_kinds: plan.kinds,
            currentness,
            groups: groups.into_values().collect(),
            boundary_edges,
            cycles,
            gaps,
        })
    }

    fn check_workspace(&self, requested: WorkspaceId) -> Result<(), SummaryError> {
        let bound: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT workspace_uid FROM db_meta WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(bound) = bound else {
            return Err(SummaryError::WorkspaceUnbound);
        };
        let bound = WorkspaceId::from_bytes(sixteen(&bound));
        if bound == requested {
            Ok(())
        } else {
            Err(SummaryError::WorkspaceMismatch { bound, requested })
        }
    }

    fn rows<T>(
        &self,
        sql: &str,
        params: Vec<Value>,
        map: impl FnMut(&Row<'_>) -> rusqlite::Result<T>,
    ) -> Result<Vec<T>, SummaryError> {
        let mut statement = self.connection.prepare(sql)?;
        let found = statement
            .query_map(params_from_iter(params), map)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut stats = self.stats.get();
        stats.statements += 1;
        stats.rows += found.len() as u64;
        self.stats.set(stats);
        Ok(found)
    }
}

fn group_entry(
    groups: &mut BTreeMap<StructuralGroup, GroupSummary>,
    group: StructuralGroup,
) -> &mut GroupSummary {
    groups.entry(group.clone()).or_insert_with(|| GroupSummary {
        group,
        prefixes: Vec::new(),
        resources: 0,
        internal_edges: 0,
        outgoing_edges: 0,
        incoming_edges: 0,
        fan_out_groups: 0,
        fan_in_groups: 0,
        coverage: GroupCoverage::default(),
        members: Vec::new(),
    })
}

fn group_of(raw: Option<String>) -> StructuralGroup {
    raw.map_or(StructuralGroup::Ungrouped, StructuralGroup::Group)
}

fn count(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    Ok(u64::try_from(row.get::<_, i64>(index)?).unwrap_or(0))
}

fn sixteen(raw: &[u8]) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    let take = raw.len().min(16);
    bytes[..take].copy_from_slice(&raw[..take]);
    bytes
}

/// Tarjan over the aggregated group graph. Groups and neighbors are
/// visited in order, members are sorted, components are ordered by
/// their smallest member: the same edges give the same answer.
fn strongly_connected(edges: &[BoundaryEdge]) -> Vec<Vec<StructuralGroup>> {
    let mut adjacency: BTreeMap<&StructuralGroup, BTreeSet<&StructuralGroup>> = BTreeMap::new();
    for edge in edges {
        adjacency
            .entry(&edge.source)
            .or_default()
            .insert(&edge.target);
        adjacency.entry(&edge.target).or_default();
    }
    let nodes: Vec<&StructuralGroup> = adjacency.keys().copied().collect();
    let position: BTreeMap<&StructuralGroup, usize> = nodes
        .iter()
        .enumerate()
        .map(|(at, node)| (*node, at))
        .collect();
    let neighbors: Vec<Vec<usize>> = nodes
        .iter()
        .map(|node| adjacency[node].iter().map(|next| position[next]).collect())
        .collect();

    struct Walk {
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        next: usize,
        components: Vec<Vec<usize>>,
    }
    fn visit(node: usize, neighbors: &[Vec<usize>], walk: &mut Walk) {
        walk.index[node] = Some(walk.next);
        walk.low[node] = walk.next;
        walk.next += 1;
        walk.stack.push(node);
        walk.on_stack[node] = true;
        for &next in &neighbors[node] {
            match walk.index[next] {
                None => {
                    visit(next, neighbors, walk);
                    walk.low[node] = walk.low[node].min(walk.low[next]);
                }
                Some(index) if walk.on_stack[next] => walk.low[node] = walk.low[node].min(index),
                Some(_) => {}
            }
        }
        if Some(walk.low[node]) == walk.index[node] {
            let mut component = Vec::new();
            while let Some(member) = walk.stack.pop() {
                walk.on_stack[member] = false;
                component.push(member);
                if member == node {
                    break;
                }
            }
            walk.components.push(component);
        }
    }

    let mut walk = Walk {
        index: vec![None; nodes.len()],
        low: vec![0; nodes.len()],
        on_stack: vec![false; nodes.len()],
        stack: Vec::new(),
        next: 0,
        components: Vec::new(),
    };
    for node in 0..nodes.len() {
        if walk.index[node].is_none() {
            visit(node, &neighbors, &mut walk);
        }
    }
    let mut cycles: Vec<Vec<StructuralGroup>> = walk
        .components
        .into_iter()
        .filter(|component| component.len() > 1)
        .map(|component| {
            let mut members: Vec<StructuralGroup> =
                component.into_iter().map(|at| nodes[at].clone()).collect();
            members.sort();
            members
        })
        .collect();
    cycles.sort();
    cycles
}

/// How the request's grouping becomes a SQL expression.
enum GroupKey {
    Column(&'static str),
    /// (label, normalized prefix), longest prefix first.
    Prefixes(Vec<(String, String)>),
    Depth {
        root: String,
        depth: usize,
    },
}

/// A validated request, ready to become SQL.
struct Plan {
    basis: GroupBasis,
    key: GroupKey,
    /// (label, normalized prefix) in label/prefix order, for reporting.
    rules: Vec<(String, String)>,
    scope_prefix: Option<String>,
    role: Option<ResourceRole>,
    language: Option<ResourceLanguage>,
    kind: Option<ResourceKind>,
    kinds: Vec<RelationKind>,
}

impl Plan {
    fn of(request: &StructuralSummaryRequest) -> Result<Self, SummaryRequestError> {
        if let Some(limit) = request.member_sample_limit
            && limit.get() > MAX_MEMBER_SAMPLE
        {
            return Err(SummaryRequestError::SampleTooLarge { limit: limit.get() });
        }
        let mut rules = Vec::new();
        let (basis, key) = match &request.grouping {
            GroupingSpec::PathPrefixes(raw) => {
                rules = validate_rules(raw)?;
                let mut arms = rules.clone();
                arms.sort_by(|(_, left), (_, right)| {
                    right
                        .chars()
                        .count()
                        .cmp(&left.chars().count())
                        .then_with(|| left.cmp(right))
                });
                (GroupBasis::PathPrefix, GroupKey::Prefixes(arms))
            }
            GroupingSpec::DirectoryDepth { root, depth } => {
                if *depth == 0 {
                    return Err(SummaryRequestError::DepthZero);
                }
                if *depth > MAX_DIRECTORY_DEPTH {
                    return Err(SummaryRequestError::DepthTooLarge { depth: *depth });
                }
                let root = match normalize_prefix(root) {
                    Ok(root) => root,
                    // The Workspace root itself is a valid root to split.
                    Err(SummaryRequestError::EmptyPrefix) => String::new(),
                    Err(error) => return Err(error),
                };
                (
                    GroupBasis::DirectoryDepth,
                    GroupKey::Depth {
                        root,
                        depth: *depth,
                    },
                )
            }
            GroupingSpec::ResourceRole => (GroupBasis::ResourceRole, GroupKey::Column("role")),
            GroupingSpec::ResourceLanguage => {
                (GroupBasis::ResourceLanguage, GroupKey::Column("language"))
            }
            GroupingSpec::ResourceKind => (GroupBasis::ResourceKind, GroupKey::Column("kind")),
        };
        let scope_prefix = request
            .resource_scope
            .path_prefix
            .as_deref()
            .map(normalize_prefix)
            .transpose()?;
        let mut kinds = request.relation_kinds.clone();
        kinds.sort_by_key(|kind| kind.as_str());
        kinds.dedup();
        Ok(Self {
            basis,
            key,
            rules,
            scope_prefix,
            role: request.resource_scope.role,
            language: request.resource_scope.language,
            kind: request.resource_scope.kind,
            kinds,
        })
    }

    /// `WITH RECURSIVE ... scoped(resource_id, uid, path_key, gkey)`,
    /// plus the parameters it binds, in text order.
    fn scoped(&self) -> (String, Vec<Value>) {
        let mut filter = String::from("r.state = 'ACTIVE'");
        let mut filter_params = Vec::new();
        if let Some(prefix) = &self.scope_prefix {
            if let Some(upper) = query::prefix_upper_bound(prefix) {
                // The path range is the narrower bound; `+` keeps the
                // planner from preferring the low-selectivity state index.
                filter = String::from("+r.state = 'ACTIVE'");
                filter.push_str(" AND r.path_key >= ? AND r.path_key < ?");
                filter_params.push(text(prefix));
                filter_params.push(text(&upper));
            }
            filter.push_str(" AND substr(r.path_key, 1, ?) = ?");
            filter_params.push(chars(prefix));
            filter_params.push(text(prefix));
        }
        for (column, value) in [
            ("role", self.role.map(|role| role.to_string())),
            (
                "language",
                self.language.map(|language| language.to_string()),
            ),
            ("kind", self.kind.map(|kind| kind.to_string())),
        ] {
            if let Some(value) = value {
                filter.push_str(&format!(" AND r.{column} = ?"));
                filter_params.push(Value::Text(value));
            }
        }

        let mut params = Vec::new();
        let sql = match &self.key {
            GroupKey::Column(column) => format!(
                "WITH RECURSIVE scoped(resource_id, uid, path_key, gkey) AS MATERIALIZED \
                 (SELECT r.id, r.uid, r.path_key, r.{column} FROM resource r WHERE {filter})"
            ),
            GroupKey::Prefixes(arms) => {
                let mut case = String::from("CASE");
                for (label, prefix) in arms {
                    case.push_str(" WHEN substr(r.path_key, 1, ?) = ? THEN ?");
                    params.push(chars(prefix));
                    params.push(text(prefix));
                    params.push(text(label));
                }
                case.push_str(" END");
                format!(
                    "WITH RECURSIVE scoped(resource_id, uid, path_key, gkey) AS MATERIALIZED \
                     (SELECT r.id, r.uid, r.path_key, {case} FROM resource r WHERE {filter})"
                )
            }
            GroupKey::Depth { root, depth } => {
                let sql = format!(
                    "WITH RECURSIVE base(resource_id, uid, path_key) AS MATERIALIZED \
                     (SELECT r.id, r.uid, r.path_key FROM resource r WHERE {filter}), \
                     seg(resource_id, rest, acc, n) AS ( \
                       SELECT resource_id, substr(path_key, ? + 1), '', 0 FROM base \
                       WHERE substr(path_key, 1, ?) = ? \
                       UNION ALL \
                       SELECT resource_id, substr(rest, instr(rest, '/') + 1), \
                              acc || substr(rest, 1, instr(rest, '/')), n + 1 \
                       FROM seg WHERE n < ? AND instr(rest, '/') > 0), \
                     scoped(resource_id, uid, path_key, gkey) AS MATERIALIZED \
                     (SELECT b.resource_id, b.uid, b.path_key, \
                             substr(s.acc, 1, length(s.acc) - 1) \
                      FROM base b LEFT JOIN seg s ON s.resource_id = b.resource_id AND s.n = ?)"
                );
                params.append(&mut filter_params);
                let depth = Value::Integer(i64::try_from(*depth).unwrap_or(i64::MAX));
                params.extend([chars(root), chars(root), text(root), depth.clone(), depth]);
                return (sql, params);
            }
        };
        params.append(&mut filter_params);
        (sql, params)
    }

    /// Q1: Resource count, support classes, and component freshness per
    /// group.
    fn inventory_sql(&self) -> (String, Vec<Value>) {
        let (with, params) = self.scoped();
        let uid = uuid_text("s.uid");
        (
            format!(
                "{with} \
                 SELECT s.gkey, COUNT(*), \
                        COUNT(CASE WHEN st.detail_state = 'COMPLETE' THEN 1 END), \
                        COUNT(CASE WHEN st.detail_state IN ('PARTIAL', 'CONTAINER_ONLY') \
                                   THEN 1 END), \
                        COUNT(CASE WHEN st.freshness_state IS NOT 'CURRENT' THEN 1 END), \
                        COUNT(CASE WHEN ri.freshness_state = 'DIRTY' THEN 1 END) \
                 FROM scoped s \
                 LEFT JOIN component_state st ON st.component_kind = '{structural}' \
                       AND st.scope_kind = '{resource}' AND st.scope_key = {uid} \
                 LEFT JOIN component_state ri ON ri.component_kind = '{relation}' \
                       AND ri.scope_kind = '{resource}' AND ri.scope_key = {uid} \
                 GROUP BY s.gkey ORDER BY s.gkey",
                structural = component::STRUCTURAL_INDEX,
                relation = component::RELATION_INDEX,
                resource = component::RESOURCE_SCOPE_KIND,
            ),
            params,
        )
    }

    /// Q2: confirmed canonical relations per (source group, target group,
    /// kind), then distinct cross-group totals per source and per target.
    fn edge_sql(&self) -> (String, Vec<Value>) {
        let (with, mut params) = self.scoped();
        let kinds = if self.kinds.is_empty() {
            String::new()
        } else {
            params.extend(self.kinds.iter().map(|kind| text(kind.as_str())));
            format!(
                " AND rel.kind IN ({})",
                vec!["?"; self.kinds.len()].join(", ")
            )
        };
        (
            format!(
                "{with}, \
                 ent(entity_id, gkey) AS MATERIALIZED ( \
                   SELECT ge.id, s.gkey FROM scoped s \
                   JOIN graph_entity ge ON ge.resource_id = s.resource_id \
                   UNION ALL \
                   SELECT ge.id, s.gkey FROM scoped s \
                   CROSS JOIN symbol y ON y.resource_id = s.resource_id \
                   CROSS JOIN graph_entity ge ON ge.symbol_id = y.id \
                   UNION ALL \
                   SELECT DISTINCT ge.id, s.gkey FROM scoped s \
                   CROSS JOIN symbol y ON y.resource_id = s.resource_id \
                   CROSS JOIN logical_symbol_declaration d ON d.symbol_id = y.id \
                   CROSS JOIN graph_entity ge ON ge.logical_symbol_id = d.logical_symbol_id), \
                 edge(rid, kind, sg, tg) AS MATERIALIZED ( \
                   SELECT DISTINCT rel.id, rel.kind, se.gkey, te.gkey FROM ent se \
                   CROSS JOIN relation rel ON rel.source_entity_id = se.entity_id{kinds} \
                   JOIN ent te ON te.entity_id = rel.target_entity_id) \
                 SELECT 0, sg, tg, kind, COUNT(*) FROM edge GROUP BY sg, tg, kind \
                 UNION ALL \
                 SELECT 1, sg, NULL, NULL, COUNT(DISTINCT rid) FROM edge \
                 WHERE sg IS NOT tg GROUP BY sg \
                 UNION ALL \
                 SELECT 2, NULL, tg, NULL, COUNT(DISTINCT rid) FROM edge \
                 WHERE sg IS NOT tg GROUP BY tg"
            ),
            params,
        )
    }

    /// Q3: unresolved use sites per owner group, intended kind, and
    /// reason. The lookup text is never selected.
    fn gap_sql(&self) -> (String, Vec<Value>) {
        let (with, params) = self.scoped();
        (
            format!(
                "{with} \
                 SELECT s.gkey, u.intended_relation_kind, u.reason, COUNT(*), \
                        COUNT(CASE WHEN EXISTS (SELECT 1 FROM relation_candidate c \
                                                WHERE c.unresolved_reference_id = u.id) \
                                   THEN 1 END), \
                        COUNT(CASE WHEN u.candidate_truncated <> 0 THEN 1 END) \
                 FROM scoped s \
                 JOIN occurrence o ON o.resource_id = s.resource_id \
                 JOIN unresolved_reference u ON u.occurrence_id = o.id \
                 GROUP BY s.gkey, u.intended_relation_kind, u.reason \
                 ORDER BY 1, 2, 3"
            ),
            params,
        )
    }

    /// Q4: per group, owners whose semantic contribution is not CURRENT
    /// (tag 0), and semantic conflicts (tag 1). The owner rule is
    /// [`crate::merge::semantic_scope`]'s.
    fn semantic_sql(&self) -> (String, Vec<Value>) {
        let (with, params) = self.scoped();
        let uid = uuid_text("contrib.uid");
        (
            format!(
                "{with}, \
                 contrib(resource_id, uid, gkey, context_key) AS ( \
                   SELECT s.resource_id, s.uid, s.gkey, e.context_key FROM scoped s \
                   CROSS JOIN occurrence o ON o.resource_id = s.resource_id \
                   CROSS JOIN semantic_evidence e ON e.occurrence_id = o.id \
                   UNION \
                   SELECT s.resource_id, s.uid, s.gkey, c.context_key FROM scoped s \
                   CROSS JOIN occurrence o ON o.resource_id = s.resource_id \
                   CROSS JOIN semantic_conflict c ON c.occurrence_id = o.id) \
                 SELECT 0, contrib.gkey, COUNT(DISTINCT contrib.resource_id) FROM contrib \
                 LEFT JOIN component_state cs ON cs.component_kind = '{semantic}' \
                       AND cs.scope_kind = '{owner}' \
                       AND cs.scope_key = contrib.context_key || char(31) || {uid} \
                 WHERE cs.detail_state IS NOT 'CURRENT' GROUP BY contrib.gkey \
                 UNION ALL \
                 SELECT 1, s.gkey, COUNT(*) FROM scoped s \
                 CROSS JOIN occurrence o ON o.resource_id = s.resource_id \
                 CROSS JOIN semantic_conflict c ON c.occurrence_id = o.id GROUP BY s.gkey",
                semantic = component::SEMANTIC_INDEX,
                owner = component::SEMANTIC_OWNER_SCOPE_KIND,
            ),
            params,
        )
    }

    /// Q5: the first `limit` members of each group by path.
    fn sample_sql(&self, limit: NonZeroUsize) -> (String, Vec<Value>) {
        let (with, mut params) = self.scoped();
        params.push(Value::Integer(
            i64::try_from(limit.get()).unwrap_or(i64::MAX),
        ));
        (
            format!(
                "{with} \
                 SELECT gkey, uid, path_rel, role, language, kind FROM ( \
                   SELECT s.gkey, r.uid, r.path_rel, r.role, r.language, r.kind, \
                          ROW_NUMBER() OVER (PARTITION BY s.gkey ORDER BY s.path_key) AS rn \
                   FROM scoped s JOIN resource r ON r.id = s.resource_id) \
                 WHERE rn <= ? ORDER BY gkey, rn"
            ),
            params,
        )
    }

    /// Every statement a request runs, for plan inspection.
    #[cfg(test)]
    fn statements(&self) -> Vec<(&'static str, String, Vec<Value>)> {
        let (inventory, inventory_params) = self.inventory_sql();
        let (edge, edge_params) = self.edge_sql();
        let (gap, gap_params) = self.gap_sql();
        let (semantic, semantic_params) = self.semantic_sql();
        let (sample, sample_params) = self.sample_sql(NonZeroUsize::MIN);
        vec![
            ("inventory", inventory, inventory_params),
            ("edge", edge, edge_params),
            ("gap", gap, gap_params),
            ("semantic", semantic, semantic_params),
            ("sample", sample, sample_params),
        ]
    }
}

/// Validate explicit path rules into (label, normalized prefix), deduped
/// and in (label, prefix) order.
fn validate_rules(raw: &[PathGroupRule]) -> Result<Vec<(String, String)>, SummaryRequestError> {
    if raw.is_empty() {
        return Err(SummaryRequestError::NoPathRules);
    }
    if raw.len() > MAX_PATH_RULES {
        return Err(SummaryRequestError::TooManyPathRules { count: raw.len() });
    }
    // normalized prefix -> (label, prefix as written)
    let mut seen: BTreeMap<String, (&str, &str)> = BTreeMap::new();
    for rule in raw {
        if rule.label.trim().is_empty() {
            return Err(SummaryRequestError::EmptyLabel);
        }
        if rule.label.len() > MAX_LABEL_BYTES {
            return Err(SummaryRequestError::LabelTooLong {
                label: rule.label.clone(),
            });
        }
        let prefix = normalize_prefix(&rule.prefix)?;
        match seen.get(&prefix) {
            Some((label, _)) if *label == rule.label => {}
            Some((_, written)) if *written == rule.prefix => {
                return Err(SummaryRequestError::DuplicatePrefix {
                    prefix: rule.prefix.clone(),
                });
            }
            Some(_) => return Err(SummaryRequestError::AmbiguousPrefix { prefix }),
            None => {
                seen.insert(prefix, (&rule.label, &rule.prefix));
            }
        }
    }
    let mut rules: Vec<(String, String)> = seen
        .into_iter()
        .map(|(prefix, (label, _))| (label.to_owned(), prefix))
        .collect();
    rules.sort();
    Ok(rules)
}

/// A Workspace-relative directory prefix with `/` separators, no empty
/// or `.` segments, and a trailing `/`. Never touches the filesystem.
fn normalize_prefix(raw: &str) -> Result<String, SummaryRequestError> {
    if raw.len() > MAX_PREFIX_BYTES {
        return Err(SummaryRequestError::PrefixTooLong);
    }
    let unified = raw.replace('\\', "/");
    let bytes = unified.as_bytes();
    if unified.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
    {
        return Err(SummaryRequestError::AbsolutePrefix {
            prefix: raw.to_owned(),
        });
    }
    let mut segments = Vec::new();
    for segment in unified.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                return Err(SummaryRequestError::EscapingPrefix {
                    prefix: raw.to_owned(),
                });
            }
            segment => segments.push(segment),
        }
    }
    if segments.is_empty() {
        return Err(SummaryRequestError::EmptyPrefix);
    }
    Ok(format!("{}/", segments.join("/")))
}

/// `structural::scope_key` in SQL: the uid as a lower-case hyphenated
/// UUID.
fn uuid_text(column: &str) -> String {
    format!(
        "lower(substr(hex({column}), 1, 8) || '-' || substr(hex({column}), 9, 4) || '-' || \
         substr(hex({column}), 13, 4) || '-' || substr(hex({column}), 17, 4) || '-' || \
         substr(hex({column}), 21, 12))"
    )
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

/// `substr` counts characters, so a prefix's length must too.
fn chars(value: &str) -> Value {
    Value::Integer(i64::try_from(value.chars().count()).unwrap_or(i64::MAX))
}
