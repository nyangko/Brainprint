//! #22 acceptance: grouping, boundary aggregation, cycles, coverage and
//! gaps, and the SQL access path, over an `index.db` built row by row.
//!
//! No source file exists anywhere in these fixtures: every summary here
//! is answered from `index.db` alone.

use std::{
    collections::hash_map::DefaultHasher,
    env, fs,
    hash::{Hash, Hasher},
    num::NonZeroUsize,
    path::PathBuf,
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use brainprint_core::{ResourceId, WorkspaceId};
use rusqlite::{Connection, params, params_from_iter};

use super::*;
use crate::{
    component::{self, ComponentRow, ProcessingState},
    structural::{self, StructuralState},
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn create() -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            env::temp_dir().join(format!("brainprint-boundary-{}-{sequence}", process::id()));
        fs::create_dir_all(&path).expect("temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn stable_bytes(key: &str) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    for (half, salt) in [(0, 0_u8), (8, 1_u8)] {
        let mut hasher = DefaultHasher::new();
        (key, salt).hash(&mut hasher);
        bytes[half..half + 8].copy_from_slice(&hasher.finish().to_be_bytes());
    }
    bytes
}

fn workspace() -> WorkspaceId {
    WorkspaceId::from_bytes(stable_bytes("workspace"))
}

/// An `index.db` written directly: Resources, Symbols, entities,
/// relations, Occurrences and gaps, with component rows in their real
/// stored format.
struct Fixture {
    dir: TempDir,
    connection: Connection,
    generation: i64,
    profile: i64,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::create();
        let connection = schema::index::open(&dir.0.join("index.db"))
            .expect("index.db")
            .connection;
        connection
            .execute(
                "UPDATE db_meta SET workspace_uid = ?1 WHERE id = 0",
                params![workspace().to_bytes().to_vec()],
            )
            .expect("bind workspace");
        connection
            .execute(
                "INSERT INTO generation (generation_no, basis_workspace_revision, state, created_at) \
                 VALUES (1, 'rev-1', 'STABLE', '0')",
                [],
            )
            .expect("generation");
        let generation = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO analysis_profile \
                 (profile_key, language, analysis_mode, structural_backend, \
                  structural_backend_version, extractor_semantics_version, \
                  adapter_semantics_version, backend_compatibility_class, \
                  capability_fingerprint, created_at) \
                 VALUES ('p', 'typescript', 'STRUCTURAL', 'tree-sitter', '1', '1', '1', '1', \
                         'fp', '0')",
                [],
            )
            .expect("profile");
        let profile = connection.last_insert_rowid();
        component::mark_current(&connection, "rev-1", generation).expect("resource index");
        Self {
            dir,
            connection,
            generation,
            profile,
        }
    }

    fn index(&self) -> StructuralSummaryIndex {
        StructuralSummaryIndex::open(&self.dir.0.join("index.db")).expect("open summary index")
    }

    fn summarize(&self, request: &StructuralSummaryRequest) -> StructuralSummary {
        self.index().summarize(request).expect("summary")
    }

    fn resource_with(&self, path: &str, role: &str, language: Option<&str>, kind: &str) -> Res {
        let id = ResourceId::from_bytes(stable_bytes(path));
        self.connection
            .execute(
                "INSERT INTO resource (uid, path_rel, path_key, kind, role, language, size, \
                 mtime_ns, fingerprint, state, resource_revision) \
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5, 0, 0, 'fp', 'ACTIVE', 'rev-1')",
                params![id.to_bytes().to_vec(), path, kind, role, language],
            )
            .expect("resource");
        let row = self.connection.last_insert_rowid();
        structural::write(
            &self.connection,
            id,
            StructuralState::Complete,
            "rev-1",
            Some(self.generation),
            None,
        )
        .expect("structure");
        let entity = self.entity("RESOURCE", "resource_id", row);
        Res { id, row, entity }
    }

    fn resource(&self, path: &str) -> Res {
        self.resource_with(path, "SOURCE", Some("TYPESCRIPT"), "FILE")
    }

    fn entity(&self, kind: &str, column: &str, payload: i64) -> i64 {
        self.connection
            .execute(
                &format!("INSERT INTO graph_entity (entity_kind, {column}) VALUES (?1, ?2)"),
                params![kind, payload],
            )
            .expect("graph entity");
        self.connection.last_insert_rowid()
    }

    fn symbol(&self, owner: &Res, name: &str) -> Sym {
        let key = format!("{}#{name}", owner.row);
        self.connection
            .execute(
                "INSERT INTO symbol (uid, resource_id, kind, name, qualified_name, visibility, \
                 exported, start_byte, end_byte, start_line, start_col, end_line, end_col, \
                 resource_revision, analysis_profile_id) \
                 VALUES (?1, ?2, 'FUNCTION', ?3, ?3, 'PUBLIC', 1, 0, 1, 1, 0, 1, 1, 'rev-1', ?4)",
                params![stable_bytes(&key).to_vec(), owner.row, name, self.profile],
            )
            .expect("symbol");
        let row = self.connection.last_insert_rowid();
        Sym {
            row,
            entity: self.entity("SYMBOL", "symbol_id", row),
        }
    }

    fn logical(&self, name: &str, declarations: &[&Sym]) -> i64 {
        self.connection
            .execute(
                "INSERT INTO logical_symbol (uid, identity_fingerprint, context_key, kind, \
                 display_name, created_generation) VALUES (?1, ?2, 'ctx', 'CLASS', ?2, ?3)",
                params![stable_bytes(name).to_vec(), name, self.generation],
            )
            .expect("logical symbol");
        let logical = self.connection.last_insert_rowid();
        for declaration in declarations {
            self.connection
                .execute(
                    "INSERT INTO logical_symbol_declaration \
                     (logical_symbol_id, symbol_id, context_key, generation_id) \
                     VALUES (?1, ?2, 'ctx', ?3)",
                    params![logical, declaration.row, self.generation],
                )
                .expect("declaration");
        }
        self.entity("LOGICAL", "logical_symbol_id", logical)
    }

    fn external(&self, package: &str) -> i64 {
        self.connection
            .execute(
                "INSERT INTO external_entity (package_identity, kind) VALUES (?1, 'PACKAGE')",
                params![package],
            )
            .expect("external");
        let row = self.connection.last_insert_rowid();
        self.entity("EXTERNAL", "external_entity_id", row)
    }

    fn domain(&self, key: &str) -> i64 {
        self.connection
            .execute(
                "INSERT INTO domain_entity (kind, normalized_identity, display_label) \
                 VALUES ('ENV_KEY', ?1, ?1)",
                params![key],
            )
            .expect("domain");
        let row = self.connection.last_insert_rowid();
        self.entity("DOMAIN", "domain_entity_id", row)
    }

    /// One canonical relation with `evidence` Occurrences in `owner`.
    fn relation(&self, kind: &str, source: i64, target: i64, owner: &Res, evidence: usize) -> i64 {
        self.connection
            .execute(
                "INSERT INTO relation (kind, source_entity_id, target_entity_id, dispatch, \
                 target_scope, created_generation) VALUES (?1, ?2, ?3, 'STATIC', 'INTERNAL', ?4)",
                params![kind, source, target, self.generation],
            )
            .expect("relation");
        let relation = self.connection.last_insert_rowid();
        for _ in 0..evidence {
            self.occurrence(owner, Some(relation));
        }
        relation
    }

    fn occurrence(&self, owner: &Res, relation: Option<i64>) -> i64 {
        self.connection
            .execute(
                "INSERT INTO occurrence (resource_id, kind, start_byte, end_byte, start_line, \
                 start_col, end_line, end_col, relation_id, analysis_profile_id, \
                 resource_revision, generation) \
                 VALUES (?1, 'CALL_SITE', 0, 1, 1, 0, 1, 1, ?2, ?3, 'rev-1', ?4)",
                params![owner.row, relation, self.profile, self.generation],
            )
            .expect("occurrence");
        self.connection.last_insert_rowid()
    }

    fn gap(&self, owner: &Res, intended: &str, reason: &str, candidates: &[i64], truncated: bool) {
        let occurrence = self.occurrence(owner, None);
        self.connection
            .execute(
                "INSERT INTO unresolved_reference (occurrence_id, intended_relation_kind, \
                 lookup_name, reason, candidate_truncated) VALUES (?1, ?2, 'shared', ?3, ?4)",
                params![occurrence, intended, reason, i64::from(truncated)],
            )
            .expect("unresolved reference");
        let unresolved = self.connection.last_insert_rowid();
        for (ordinal, candidate) in candidates.iter().enumerate() {
            self.connection
                .execute(
                    "INSERT INTO relation_candidate (unresolved_reference_id, target_entity_id, \
                     evidence_kind, ordinal) VALUES (?1, ?2, 'STRUCTURAL_RESOLVER', ?3)",
                    params![unresolved, candidate, i64::try_from(ordinal).unwrap()],
                )
                .expect("candidate");
        }
    }

    fn structure(&self, resource: &Res, state: StructuralState) {
        structural::write(
            &self.connection,
            resource.id,
            state,
            "rev-1",
            Some(self.generation),
            None,
        )
        .expect("structure");
    }

    fn relation_dirty(&self, resource: &Res) {
        component::write_scoped(
            &self.connection,
            component::RELATION_INDEX,
            component::RESOURCE_SCOPE_KIND,
            &structural::scope_key(resource.id),
            &ComponentRow {
                basis_workspace_revision: "rev-1".to_owned(),
                stable_generation_id: None,
                processing_state: ProcessingState::Queued,
                freshness_state: FreshnessState::Dirty,
                last_error_code: None,
                detail_state: None,
            },
        )
        .expect("relation state");
    }

    fn delete(&self, resource: &Res) {
        self.connection
            .execute(
                "UPDATE resource SET state = 'DELETED' WHERE id = ?1",
                params![resource.row],
            )
            .expect("delete");
    }
}

struct Res {
    id: ResourceId,
    row: i64,
    entity: i64,
}

struct Sym {
    row: i64,
    entity: i64,
}

fn rule(label: &str, prefix: &str) -> PathGroupRule {
    PathGroupRule {
        label: label.to_owned(),
        prefix: prefix.to_owned(),
    }
}

fn representative_rules() -> Vec<PathGroupRule> {
    vec![
        rule("routes", "src/routes/"),
        rule("auth", "src/features/auth/"),
        rule("billing", "src/features/billing/"),
        rule("shared", "src/shared/"),
        rule("tests", "tests/"),
    ]
}

fn request(grouping: GroupingSpec) -> StructuralSummaryRequest {
    StructuralSummaryRequest {
        workspace: workspace(),
        grouping,
        resource_scope: ResourceScope::default(),
        relation_kinds: Vec::new(),
        include_ungrouped: true,
        include_cycles: true,
        member_sample_limit: None,
    }
}

fn by_rules() -> StructuralSummaryRequest {
    request(GroupingSpec::PathPrefixes(representative_rules()))
}

fn g(label: &str) -> StructuralGroup {
    StructuralGroup::Group(label.to_owned())
}

fn group<'a>(summary: &'a StructuralSummary, which: &StructuralGroup) -> &'a GroupSummary {
    summary
        .groups
        .iter()
        .find(|group| &group.group == which)
        .unwrap_or_else(|| panic!("no group {which:?}"))
}

fn edges(
    summary: &StructuralSummary,
) -> Vec<(StructuralGroup, StructuralGroup, &'static str, u64)> {
    summary
        .boundary_edges
        .iter()
        .map(|edge| {
            (
                edge.source.clone(),
                edge.target.clone(),
                edge.kind.as_str(),
                edge.confirmed_edges,
            )
        })
        .collect()
}

/// #22 representative fixture. `reverse` inserts every row in the
/// opposite order, so nothing may depend on rowid.
fn representative(reverse: bool) -> Fixture {
    let fixture = Fixture::new();
    let mut paths = vec![
        "src/routes/index.ts",
        "src/features/auth/login.ts",
        "src/features/auth/session.ts",
        "src/features/billing/invoice.ts",
        "src/shared/util.ts",
        "tests/auth.test.ts",
        "scripts/build.ts",
    ];
    if reverse {
        paths.reverse();
    }
    let mut resources = std::collections::HashMap::new();
    for path in paths {
        let resource = if path.starts_with("tests/") {
            fixture.resource_with(path, "TEST", Some("TYPESCRIPT"), "FILE")
        } else {
            fixture.resource(path)
        };
        resources.insert(path, resource);
    }
    let res = |path: &str| &resources[path];
    let routes = res("src/routes/index.ts");
    let login = res("src/features/auth/login.ts");
    let session = res("src/features/auth/session.ts");
    let invoice = res("src/features/billing/invoice.ts");
    let util = res("src/shared/util.ts");
    let test = res("tests/auth.test.ts");
    let build = res("scripts/build.ts");

    let handle = fixture.symbol(routes, "handle");
    let login_fn = fixture.symbol(login, "login");
    let session_fn = fixture.symbol(session, "session");
    let invoice_ty = fixture.symbol(invoice, "Invoice");
    let charge = fixture.symbol(invoice, "charge");
    let hash = fixture.symbol(util, "hash");
    let main = fixture.symbol(build, "main");
    let lodash = fixture.external("lodash");
    let env_key = fixture.domain("AUTH_SECRET");

    let mut relations: Vec<Box<dyn Fn() + '_>> = vec![
        // routes -> auth, two kinds
        Box::new(|| {
            fixture.relation("IMPORTS", routes.entity, login.entity, routes, 1);
        }),
        Box::new(|| {
            fixture.relation("CALLS", handle.entity, login_fn.entity, routes, 1);
        }),
        // auth -> shared, one canonical edge with three evidence spans
        Box::new(|| {
            fixture.relation("CALLS", login_fn.entity, hash.entity, login, 3);
        }),
        // auth -> billing and billing -> auth: the cycle
        Box::new(|| {
            fixture.relation("USES_TYPE", login_fn.entity, invoice_ty.entity, login, 1);
        }),
        Box::new(|| {
            fixture.relation("CALLS", charge.entity, session_fn.entity, invoice, 2);
        }),
        // tests -> auth
        Box::new(|| {
            fixture.relation("IMPORTS", test.entity, login.entity, test, 1);
        }),
        // auth -> auth: internal
        Box::new(|| {
            fixture.relation("CALLS", login_fn.entity, session_fn.entity, login, 1);
        }),
        // auth -> External / Domain: no Workspace group
        Box::new(|| {
            fixture.relation("IMPORTS", login.entity, lodash, login, 1);
        }),
        Box::new(|| {
            fixture.relation("USES_ENV", login_fn.entity, env_key, login, 1);
        }),
        // Ungrouped -> shared
        Box::new(|| {
            fixture.relation("REFERENCES", main.entity, hash.entity, build, 1);
        }),
    ];
    if reverse {
        relations.reverse();
    }
    for insert in &relations {
        insert();
    }
    drop(relations);

    let mut gaps: Vec<Box<dyn Fn() + '_>> = vec![
        // unresolved, needs a type checker
        Box::new(|| fixture.gap(login, "CALLS", "RECEIVER_TYPE_REQUIRED", &[], false)),
        // candidate-only: two canonical candidates, one of them in billing
        Box::new(|| {
            fixture.gap(
                login,
                "CALLS",
                "AMBIGUOUS_CANDIDATES",
                &[charge.entity, hash.entity],
                false,
            );
        }),
        // candidate list cut
        Box::new(|| {
            fixture.gap(
                invoice,
                "CALLS",
                "AMBIGUOUS_CANDIDATES",
                &[hash.entity],
                true,
            );
        }),
        // unsupported construct
        Box::new(|| fixture.gap(util, "USES_ENV", "DYNAMIC_KEY_EXPRESSION", &[], false)),
        // unresolved import in routes
        Box::new(|| fixture.gap(routes, "IMPORTS", "MISSING_RELATIVE_TARGET", &[], false)),
    ];
    if reverse {
        gaps.reverse();
    }
    for insert in &gaps {
        insert();
    }
    drop(gaps);
    fixture
}

// ---------------------------------------------------------------------
// Representative fixture: exact results
// ---------------------------------------------------------------------

#[test]
fn representative_fixture_exact_summary() {
    let fixture = representative(false);
    let started = Instant::now();
    let index = fixture.index();
    let summary = index.summarize(&by_rules()).expect("summary");
    let elapsed = started.elapsed();

    assert_eq!(summary.basis, GroupBasis::PathPrefix);
    assert_eq!(summary.currentness, Currentness::Current);
    let order: Vec<_> = summary
        .groups
        .iter()
        .map(|group| group.group.clone())
        .collect();
    assert_eq!(
        order,
        [
            g("auth"),
            g("billing"),
            g("routes"),
            g("shared"),
            g("tests"),
            StructuralGroup::Ungrouped
        ]
    );
    let metrics = |which: &StructuralGroup| {
        let group = group(&summary, which);
        (
            group.resources,
            group.internal_edges,
            group.outgoing_edges,
            group.incoming_edges,
            group.fan_out_groups,
            group.fan_in_groups,
        )
    };
    assert_eq!(metrics(&g("auth")), (2, 1, 2, 4, 2, 3));
    assert_eq!(metrics(&g("billing")), (1, 0, 1, 1, 1, 1));
    assert_eq!(metrics(&g("routes")), (1, 0, 2, 0, 1, 0));
    assert_eq!(metrics(&g("shared")), (1, 0, 0, 2, 0, 2));
    assert_eq!(metrics(&g("tests")), (1, 0, 1, 0, 1, 0));
    assert_eq!(metrics(&StructuralGroup::Ungrouped), (1, 0, 1, 0, 1, 0));
    assert_eq!(group(&summary, &g("auth")).prefixes, ["src/features/auth/"]);

    assert_eq!(
        edges(&summary),
        [
            (g("auth"), g("billing"), "USES_TYPE", 1),
            (g("auth"), g("shared"), "CALLS", 1),
            (g("billing"), g("auth"), "CALLS", 1),
            (g("routes"), g("auth"), "CALLS", 1),
            (g("routes"), g("auth"), "IMPORTS", 1),
            (g("tests"), g("auth"), "IMPORTS", 1),
            (StructuralGroup::Ungrouped, g("shared"), "REFERENCES", 1),
        ]
    );
    assert_eq!(summary.boundary_edge_count(), 7);
    assert_eq!(summary.cycles, Some(vec![vec![g("auth"), g("billing")]]));

    let auth = group(&summary, &g("auth")).coverage;
    assert_eq!(
        (
            auth.unresolved_gaps,
            auth.candidate_gaps,
            auth.requires_semantics,
            auth.unsupported_construct,
            auth.candidate_truncated
        ),
        (1, 1, 1, 0, 0)
    );
    let billing = group(&summary, &g("billing")).coverage;
    assert_eq!(
        (billing.candidate_gaps, billing.candidate_truncated),
        (1, 1)
    );
    let shared = group(&summary, &g("shared")).coverage;
    assert_eq!(
        (shared.unresolved_gaps, shared.unsupported_construct),
        (1, 1)
    );
    let routes = group(&summary, &g("routes")).coverage;
    assert_eq!(routes.unresolved_gaps, 1);
    assert_eq!(summary.gaps.len(), 5);
    assert_eq!(
        summary.limits().limits(),
        [
            CoverageLimit::UnresolvedEvidence,
            CoverageLimit::AmbiguousCandidates,
            CoverageLimit::RequiresSemantics,
            CoverageLimit::UnsupportedConstruct,
            CoverageLimit::CandidateTruncated,
        ]
    );
    assert_eq!(summary.answer_state(), AnswerState::Confirmed);

    let stats = index.stats();
    let relation_rows: i64 = fixture
        .connection
        .query_row("SELECT COUNT(*) FROM relation", [], |row| row.get(0))
        .unwrap();
    let occurrence_rows: i64 = fixture
        .connection
        .query_row("SELECT COUNT(*) FROM occurrence", [], |row| row.get(0))
        .unwrap();
    println!(
        "TASK9 representative: active_resources=7 groups={} boundary_edge_rows={} \
         boundary_edges={} gap_rows={} statements={} aggregate_rows={} relation_rows={} \
         occurrence_rows={} wall_us={}",
        summary.groups.len(),
        summary.boundary_edges.len(),
        summary.boundary_edge_count(),
        summary.gaps.len(),
        stats.statements,
        stats.rows,
        relation_rows,
        occurrence_rows,
        elapsed.as_micros()
    );
}

// ---------------------------------------------------------------------
// Grouping
// ---------------------------------------------------------------------

#[test]
fn explicit_prefix_rules_group_deterministically() {
    let fixture = representative(false);
    let first = fixture.summarize(&by_rules());
    let second = fixture.summarize(&by_rules());
    assert_eq!(first, second);
    let mut shuffled = by_rules();
    if let GroupingSpec::PathPrefixes(rules) = &mut shuffled.grouping {
        rules.reverse();
    }
    assert_eq!(fixture.summarize(&shuffled), first);
}

#[test]
fn longest_matching_prefix_wins() {
    let fixture = representative(false);
    let mut rules = representative_rules();
    rules.push(rule("features", "src/features/"));
    rules.push(rule("src", "src/"));
    let summary = fixture.summarize(&request(GroupingSpec::PathPrefixes(rules)));
    assert_eq!(group(&summary, &g("auth")).resources, 2);
    assert_eq!(group(&summary, &g("billing")).resources, 1);
    assert_eq!(group(&summary, &g("features")).resources, 0);
    assert_eq!(group(&summary, &g("src")).resources, 0);
}

#[test]
fn a_prefix_with_no_resource_returns_zero() {
    let fixture = representative(false);
    let summary = fixture.summarize(&request(GroupingSpec::PathPrefixes(vec![rule(
        "docs", "docs/",
    )])));
    assert_eq!(group(&summary, &g("docs")).resources, 0);
    assert_eq!(group(&summary, &StructuralGroup::Ungrouped).resources, 7);
}

#[test]
fn duplicate_exact_prefix_with_different_labels_is_rejected() {
    let fixture = representative(false);
    let error = fixture
        .index()
        .summarize(&request(GroupingSpec::PathPrefixes(vec![
            rule("a", "src/shared/"),
            rule("b", "src/shared/"),
        ])))
        .expect_err("duplicate");
    assert!(matches!(
        error,
        SummaryError::Invalid(SummaryRequestError::DuplicatePrefix { .. })
    ));
    // The same rule twice is one rule.
    fixture.summarize(&request(GroupingSpec::PathPrefixes(vec![
        rule("a", "src/shared/"),
        rule("a", "src/shared/"),
    ])));
}

#[test]
fn ambiguous_equal_specificity_rules_are_rejected() {
    let fixture = representative(false);
    let error = fixture
        .index()
        .summarize(&request(GroupingSpec::PathPrefixes(vec![
            rule("a", "src/shared"),
            rule("b", "src\\shared/"),
        ])))
        .expect_err("ambiguous");
    assert!(matches!(
        error,
        SummaryError::Invalid(SummaryRequestError::AmbiguousPrefix { .. })
    ));
}

#[test]
fn path_rule_validation_and_bounds() {
    let invalid =
        |rules: Vec<PathGroupRule>| Plan::of(&request(GroupingSpec::PathPrefixes(rules))).err();
    assert_eq!(invalid(vec![]), Some(SummaryRequestError::NoPathRules));
    assert_eq!(
        invalid(vec![rule(" ", "src/")]),
        Some(SummaryRequestError::EmptyLabel)
    );
    assert!(matches!(
        invalid(vec![rule(&"x".repeat(MAX_LABEL_BYTES + 1), "src/")]),
        Some(SummaryRequestError::LabelTooLong { .. })
    ));
    assert_eq!(
        invalid(vec![rule("a", "./")]),
        Some(SummaryRequestError::EmptyPrefix)
    );
    assert_eq!(
        invalid(vec![rule("a", &"x/".repeat(MAX_PREFIX_BYTES))]),
        Some(SummaryRequestError::PrefixTooLong)
    );
    assert!(matches!(
        invalid(vec![rule("a", "/etc/")]),
        Some(SummaryRequestError::AbsolutePrefix { .. })
    ));
    assert!(matches!(
        invalid(vec![rule("a", "C:\\src")]),
        Some(SummaryRequestError::AbsolutePrefix { .. })
    ));
    assert!(matches!(
        invalid(vec![rule("a", "src/../../etc")]),
        Some(SummaryRequestError::EscapingPrefix { .. })
    ));
    let many = (0..=MAX_PATH_RULES)
        .map(|at| rule(&format!("g{at}"), &format!("d{at}/")))
        .collect();
    assert_eq!(
        invalid(many),
        Some(SummaryRequestError::TooManyPathRules {
            count: MAX_PATH_RULES + 1
        })
    );
    // Separators are normalized deterministically.
    assert_eq!(normalize_prefix(".\\src//a/").unwrap(), "src/a/");
    assert_eq!(normalize_prefix("src/a").unwrap(), "src/a/");

    let mut sample = by_rules();
    sample.member_sample_limit = NonZeroUsize::new(MAX_MEMBER_SAMPLE + 1);
    assert_eq!(
        Plan::of(&sample).err(),
        Some(SummaryRequestError::SampleTooLarge {
            limit: MAX_MEMBER_SAMPLE + 1
        })
    );
}

#[test]
fn unmatched_resources_are_ungrouped_and_can_be_excluded_alone() {
    let fixture = representative(false);
    let with = fixture.summarize(&by_rules());
    assert_eq!(group(&with, &StructuralGroup::Ungrouped).resources, 1);

    let mut request = by_rules();
    request.include_ungrouped = false;
    let without = fixture.summarize(&request);
    assert!(
        without
            .groups
            .iter()
            .all(|group| group.group != StructuralGroup::Ungrouped)
    );
    assert!(without.boundary_edges.iter().all(|edge| {
        edge.source != StructuralGroup::Ungrouped && edge.target != StructuralGroup::Ungrouped
    }));
    // Every other group is exactly what it was.
    let visible: Vec<_> = with
        .groups
        .iter()
        .filter(|group| group.group != StructuralGroup::Ungrouped)
        .cloned()
        .collect();
    assert_eq!(without.groups, visible);
    assert_eq!(group(&without, &g("shared")).incoming_edges, 2);
}

#[test]
fn classification_grouping_uses_stored_values_only() {
    let fixture = representative(false);
    // Stored classification disagrees with the folder name on purpose.
    fixture.resource_with("src/features/auth/notes.md", "DOCS", None, "FILE");
    fixture.resource_with("src/features", "UNKNOWN", None, "DIRECTORY");

    let roles = fixture.summarize(&request(GroupingSpec::ResourceRole));
    assert_eq!(roles.basis, GroupBasis::ResourceRole);
    let counts = |summary: &StructuralSummary| {
        summary
            .groups
            .iter()
            .map(|group| (group.group.clone(), group.resources))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        counts(&roles),
        [
            (g("DOCS"), 1),
            (g("SOURCE"), 6),
            (g("TEST"), 1),
            (g("UNKNOWN"), 1),
            (StructuralGroup::Ungrouped, 0),
        ]
    );
    assert_eq!(edges(&roles), [(g("TEST"), g("SOURCE"), "IMPORTS", 1)]);
    assert_eq!(group(&roles, &g("SOURCE")).internal_edges, 7);

    let languages = fixture.summarize(&request(GroupingSpec::ResourceLanguage));
    assert_eq!(
        counts(&languages),
        [(g("TYPESCRIPT"), 7), (StructuralGroup::Ungrouped, 2)]
    );

    let kinds = fixture.summarize(&request(GroupingSpec::ResourceKind));
    assert_eq!(
        counts(&kinds),
        [
            (g("DIRECTORY"), 1),
            (g("FILE"), 8),
            (StructuralGroup::Ungrouped, 0)
        ]
    );
}

#[test]
fn directory_depth_is_structural_and_deterministic() {
    let fixture = representative(false);
    fixture.resource("src/features/top.ts");
    fixture.resource("src/features/auth/deep/inner.ts");
    let summary = fixture.summarize(&request(GroupingSpec::DirectoryDepth {
        root: "src/features/".to_owned(),
        depth: 1,
    }));
    assert_eq!(summary.basis, GroupBasis::DirectoryDepth);
    let counts: Vec<_> = summary
        .groups
        .iter()
        .map(|group| (group.group.clone(), group.resources))
        .collect();
    // top.ts has too few segments; everything outside the root is Ungrouped.
    assert_eq!(
        counts,
        [
            (g("auth"), 3),
            (g("billing"), 1),
            (StructuralGroup::Ungrouped, 5)
        ]
    );
    // Outside the root is one bucket: auth -> Ungrouped -> auth closes.
    assert_eq!(
        summary.cycles,
        Some(vec![vec![
            g("auth"),
            g("billing"),
            StructuralGroup::Ungrouped
        ]])
    );

    let deeper = fixture.summarize(&request(GroupingSpec::DirectoryDepth {
        root: "src/features".to_owned(),
        depth: 2,
    }));
    assert_eq!(group(&deeper, &g("auth/deep")).resources, 1);

    let top = fixture.summarize(&request(GroupingSpec::DirectoryDepth {
        root: String::new(),
        depth: 1,
    }));
    let counts: Vec<_> = top
        .groups
        .iter()
        .map(|group| (group.group.clone(), group.resources))
        .collect();
    assert_eq!(
        counts,
        [
            (g("scripts"), 1),
            (g("src"), 7),
            (g("tests"), 1),
            (StructuralGroup::Ungrouped, 0)
        ]
    );
}

#[test]
fn directory_depth_zero_and_excess_are_rejected() {
    let depth = |depth| {
        Plan::of(&request(GroupingSpec::DirectoryDepth {
            root: "src/".to_owned(),
            depth,
        }))
        .err()
    };
    assert_eq!(depth(0), Some(SummaryRequestError::DepthZero));
    assert_eq!(
        depth(MAX_DIRECTORY_DEPTH + 1),
        Some(SummaryRequestError::DepthTooLarge {
            depth: MAX_DIRECTORY_DEPTH + 1
        })
    );
}

#[test]
fn folder_names_never_become_semantic_groups() {
    let fixture = Fixture::new();
    for path in [
        "src/features/a.ts",
        "src/domain/b.ts",
        "src/layer/c.ts",
        "src/routes/d.ts",
        "src/service/e.ts",
    ] {
        fixture.resource(path);
    }
    let summary = fixture.summarize(&request(GroupingSpec::PathPrefixes(vec![rule(
        "x", "lib/",
    )])));
    assert_eq!(
        summary
            .groups
            .iter()
            .map(|group| (group.group.clone(), group.resources))
            .collect::<Vec<_>>(),
        [(g("x"), 0), (StructuralGroup::Ungrouped, 5)]
    );
}

#[test]
fn only_active_resources_are_counted() {
    let fixture = Fixture::new();
    let kept = fixture.resource("src/shared/a.ts");
    let gone = fixture.resource("src/shared/b.ts");
    let caller = fixture.resource("src/routes/c.ts");
    fixture.relation("IMPORTS", caller.entity, gone.entity, &caller, 1);
    fixture.relation("IMPORTS", caller.entity, kept.entity, &caller, 1);
    fixture.delete(&gone);
    let summary = fixture.summarize(&by_rules());
    assert_eq!(group(&summary, &g("shared")).resources, 1);
    assert_eq!(group(&summary, &g("shared")).incoming_edges, 1);
}

#[test]
fn resource_scope_filters_are_respected() {
    let fixture = representative(false);
    let mut scoped = by_rules();
    scoped.resource_scope.path_prefix = Some("src/features".to_owned());
    let summary = fixture.summarize(&scoped);
    let total: u64 = summary.groups.iter().map(|group| group.resources).sum();
    assert_eq!(total, 3);
    // Edges leaving the scope have no group on the other side.
    assert_eq!(
        edges(&summary),
        [
            (g("auth"), g("billing"), "USES_TYPE", 1),
            (g("billing"), g("auth"), "CALLS", 1)
        ]
    );

    let mut tests_only = by_rules();
    tests_only.resource_scope.role = Some(ResourceRole::Test);
    let summary = fixture.summarize(&tests_only);
    assert_eq!(group(&summary, &g("tests")).resources, 1);
    assert_eq!(
        summary
            .groups
            .iter()
            .map(|group| group.resources)
            .sum::<u64>(),
        1
    );

    let mut none = by_rules();
    none.resource_scope.language = Some(ResourceLanguage::Rust);
    none.resource_scope.kind = Some(ResourceKind::File);
    let summary = fixture.summarize(&none);
    assert_eq!(
        summary
            .groups
            .iter()
            .map(|group| group.resources)
            .sum::<u64>(),
        0
    );
}

#[test]
fn member_sample_is_bounded_and_path_ordered() {
    let fixture = representative(false);
    fixture.resource("src/features/auth/a_first.ts");
    let mut sampled = by_rules();
    sampled.member_sample_limit = NonZeroUsize::new(2);
    let summary = fixture.summarize(&sampled);
    let auth = group(&summary, &g("auth"));
    assert_eq!(auth.resources, 3);
    assert_eq!(
        auth.members
            .iter()
            .map(|member| member.path.as_str())
            .collect::<Vec<_>>(),
        ["src/features/auth/a_first.ts", "src/features/auth/login.ts"]
    );
    assert_eq!(auth.members[1].role, ResourceRole::Source);
    assert_eq!(auth.members[1].language, Some(ResourceLanguage::TypeScript));
    assert_eq!(auth.members[1].kind, ResourceKind::File);
    assert_eq!(
        auth.members[1].resource,
        ResourceId::from_bytes(stable_bytes("src/features/auth/login.ts"))
    );
    // Without a sample, no member list.
    assert!(
        fixture
            .summarize(&by_rules())
            .groups
            .iter()
            .all(|group| group.members.is_empty())
    );
}

// ---------------------------------------------------------------------
// Boundary aggregation
// ---------------------------------------------------------------------

#[test]
fn evidence_spans_do_not_multiply_canonical_edges() {
    let fixture = representative(false);
    let before = fixture.summarize(&by_rules());
    let login = Res {
        id: ResourceId::from_bytes(stable_bytes("src/features/auth/login.ts")),
        row: 2,
        entity: 0,
    };
    // Twenty more spans on existing canonical edges.
    for _ in 0..20 {
        fixture
            .connection
            .execute(
                "INSERT INTO occurrence (resource_id, kind, start_byte, end_byte, start_line, \
                 start_col, end_line, end_col, relation_id, analysis_profile_id, \
                 resource_revision, generation) \
                 SELECT ?1, 'CALL_SITE', 5, 6, 2, 0, 2, 1, id, ?2, 'rev-1', ?3 FROM relation",
                params![login.row, fixture.profile, fixture.generation],
            )
            .unwrap();
    }
    let after = fixture.summarize(&by_rules());
    assert_eq!(before, after);
}

#[test]
fn relation_kind_filter_is_exact_and_empty_means_all() {
    let fixture = representative(false);
    let mut calls = by_rules();
    calls.relation_kinds = vec![RelationKind::Calls, RelationKind::Calls];
    let summary = fixture.summarize(&calls);
    assert_eq!(summary.relation_kinds, [RelationKind::Calls]);
    assert_eq!(
        edges(&summary),
        [
            (g("auth"), g("shared"), "CALLS", 1),
            (g("billing"), g("auth"), "CALLS", 1),
            (g("routes"), g("auth"), "CALLS", 1),
        ]
    );
    assert_eq!(group(&summary, &g("auth")).internal_edges, 1);
    // Gaps follow the same filter.
    assert!(
        summary
            .gaps
            .iter()
            .all(|gap| gap.intended == IntendedRelation::Known(RelationKind::Calls))
    );

    let mut imports = by_rules();
    imports.relation_kinds = vec![RelationKind::Imports];
    let summary = fixture.summarize(&imports);
    assert_eq!(
        edges(&summary),
        [
            (g("routes"), g("auth"), "IMPORTS", 1),
            (g("tests"), g("auth"), "IMPORTS", 1),
        ]
    );
    assert_eq!(summary.cycles, Some(Vec::new()));
    assert_eq!(fixture.summarize(&by_rules()).boundary_edge_count(), 7);
}

#[test]
fn endpoints_project_onto_their_owning_resource_group() {
    let fixture = Fixture::new();
    let a = fixture.resource("src/shared/a.ts");
    let b = fixture.resource("src/routes/b.ts");
    let a_fn = fixture.symbol(&a, "f");
    let b_fn = fixture.symbol(&b, "g");
    // Resource -> Resource, Symbol -> Symbol, Symbol -> Resource
    fixture.relation("IMPORTS", b.entity, a.entity, &b, 1);
    fixture.relation("CALLS", b_fn.entity, a_fn.entity, &b, 1);
    fixture.relation("REFERENCES", b_fn.entity, a.entity, &b, 1);
    let summary = fixture.summarize(&by_rules());
    assert_eq!(
        edges(&summary),
        [
            (g("routes"), g("shared"), "CALLS", 1),
            (g("routes"), g("shared"), "IMPORTS", 1),
            (g("routes"), g("shared"), "REFERENCES", 1),
        ]
    );
}

#[test]
fn logical_symbol_declarations_map_deterministically_and_dedupe() {
    let fixture = Fixture::new();
    let first = fixture.resource("src/features/auth/a.ts");
    let second = fixture.resource("src/features/auth/b.ts");
    let third = fixture.resource("src/features/billing/c.ts");
    let caller = fixture.resource("src/routes/r.ts");
    let decl_a = fixture.symbol(&first, "Runner");
    let decl_b = fixture.symbol(&second, "Runner");
    let decl_c = fixture.symbol(&third, "Runner");
    let logical = fixture.logical("Runner", &[&decl_a, &decl_b, &decl_c]);
    let call = fixture.symbol(&caller, "run");
    fixture.relation("CALLS", call.entity, logical, &caller, 4);
    let summary = fixture.summarize(&by_rules());
    // Two declarations in auth are one auth group-edge; billing gets its own.
    assert_eq!(
        edges(&summary),
        [
            (g("routes"), g("auth"), "CALLS", 1),
            (g("routes"), g("billing"), "CALLS", 1),
        ]
    );
    // Still one canonical relation leaving routes.
    let routes = group(&summary, &g("routes"));
    assert_eq!((routes.outgoing_edges, routes.fan_out_groups), (1, 2));
}

#[test]
fn external_and_domain_endpoints_get_no_workspace_group() {
    let fixture = Fixture::new();
    let a = fixture.resource("src/shared/a.ts");
    let a_fn = fixture.symbol(&a, "f");
    let external = fixture.external("react");
    let domain = fixture.domain("API_KEY");
    fixture.relation("IMPORTS", a.entity, external, &a, 1);
    fixture.relation("USES_ENV", a_fn.entity, domain, &a, 1);
    fixture.relation("USES_CONFIG", external, a.entity, &a, 1);
    let summary = fixture.summarize(&by_rules());
    assert!(summary.boundary_edges.is_empty());
    let shared = group(&summary, &g("shared"));
    assert_eq!(
        (
            shared.internal_edges,
            shared.outgoing_edges,
            shared.incoming_edges
        ),
        (0, 0, 0)
    );
    assert_eq!(
        group(&summary, &StructuralGroup::Ungrouped).resources,
        0,
        "no local bucket invented for external/domain"
    );
}

#[test]
fn insertion_order_does_not_change_the_summary() {
    let forward = representative(false);
    let backward = representative(true);
    let mut sampled = by_rules();
    sampled.member_sample_limit = NonZeroUsize::new(3);
    assert_eq!(forward.summarize(&sampled), backward.summarize(&sampled));
    assert_eq!(
        forward.summarize(&request(GroupingSpec::ResourceRole)),
        backward.summarize(&request(GroupingSpec::ResourceRole))
    );
}

// ---------------------------------------------------------------------
// Cycles
// ---------------------------------------------------------------------

fn chain(links: &[(&str, &str)]) -> StructuralSummary {
    let fixture = Fixture::new();
    let mut files = std::collections::BTreeMap::new();
    for (from, to) in links {
        for name in [*from, *to] {
            if !files.contains_key(name) {
                let resource = fixture.resource(&format!("{name}/x.ts"));
                files.insert(name, resource);
            }
        }
    }
    for (from, to) in links {
        let (from, to) = (&files[from], &files[to]);
        fixture.relation("IMPORTS", from.entity, to.entity, from, 2);
    }
    let rules = files
        .keys()
        .map(|name| rule(name, &format!("{name}/")))
        .collect();
    fixture.summarize(&request(GroupingSpec::PathPrefixes(rules)))
}

#[test]
fn acyclic_graph_has_no_component() {
    assert_eq!(
        chain(&[("a", "b"), ("b", "c"), ("a", "c")]).cycles,
        Some(Vec::new())
    );
}

#[test]
fn two_and_three_group_cycles_are_deterministic() {
    assert_eq!(
        chain(&[("b", "a"), ("a", "b")]).cycles,
        Some(vec![vec![g("a"), g("b")]])
    );
    assert_eq!(
        chain(&[("c", "a"), ("a", "b"), ("b", "c"), ("c", "d")]).cycles,
        Some(vec![vec![g("a"), g("b"), g("c")]])
    );
    assert_eq!(
        chain(&[("x", "y"), ("y", "x"), ("a", "b"), ("b", "a")]).cycles,
        Some(vec![vec![g("a"), g("b")], vec![g("x"), g("y")]])
    );
}

#[test]
fn internal_edges_are_not_a_boundary_cycle() {
    let summary = chain(&[("a", "a")]);
    assert_eq!(summary.cycles, Some(Vec::new()));
    assert_eq!(group(&summary, &g("a")).internal_edges, 1);
    // Not computed unless asked.
    let fixture = representative(false);
    let mut request = by_rules();
    request.include_cycles = false;
    assert_eq!(fixture.summarize(&request).cycles, None);
}

#[test]
fn cycles_come_from_the_aggregate_graph_not_evidence() {
    let fixture = representative(false);
    let before = fixture.summarize(&by_rules()).cycles;
    fixture
        .connection
        .execute(
            "INSERT INTO occurrence (resource_id, kind, start_byte, end_byte, start_line, \
             start_col, end_line, end_col, relation_id, analysis_profile_id, \
             resource_revision, generation) \
             SELECT resource_id, kind, start_byte + 10, end_byte + 10, start_line, start_col, \
                    end_line, end_col, relation_id, analysis_profile_id, resource_revision, \
                    generation FROM occurrence WHERE relation_id IS NOT NULL",
            [],
        )
        .unwrap();
    assert_eq!(fixture.summarize(&by_rules()).cycles, before);
    // Tarjan runs over boundary edges only: one row per group pair/kind.
    let summary = fixture.summarize(&by_rules());
    assert_eq!(strongly_connected(&summary.boundary_edges), before.unwrap());
}

// ---------------------------------------------------------------------
// Coverage and gaps
// ---------------------------------------------------------------------

#[test]
fn gaps_are_counted_under_their_owner_and_never_become_edges() {
    let fixture = representative(false);
    let summary = fixture.summarize(&by_rules());
    let gaps: Vec<_> = summary
        .gaps
        .iter()
        .map(|gap| {
            (
                gap.group.clone(),
                gap.intended.as_str(),
                gap.reason.as_str(),
                gap.unresolved,
                gap.candidate,
                gap.candidate_truncated,
            )
        })
        .collect();
    assert_eq!(
        gaps,
        [
            (g("auth"), "CALLS", "AMBIGUOUS_CANDIDATES", 0, 1, 0),
            (g("auth"), "CALLS", "RECEIVER_TYPE_REQUIRED", 1, 0, 0),
            (g("billing"), "CALLS", "AMBIGUOUS_CANDIDATES", 0, 1, 1),
            (g("routes"), "IMPORTS", "MISSING_RELATIVE_TARGET", 1, 0, 0),
            (g("shared"), "USES_ENV", "DYNAMIC_KEY_EXPRESSION", 1, 0, 0),
        ]
    );
    // auth's candidate points into billing and shared; no edge appears
    // for it, and lookup text ("shared") never names a group.
    assert!(!edges(&summary).contains(&(g("auth"), g("billing"), "CALLS", 1)));
    assert_eq!(group(&summary, &g("shared")).incoming_edges, 2);
}

#[test]
fn inheritance_gaps_answer_extends_and_implements() {
    let fixture = Fixture::new();
    let a = fixture.resource("src/shared/a.ts");
    fixture.gap(
        &a,
        "INHERITANCE",
        "RELATION_KIND_NOT_STRUCTURAL",
        &[],
        false,
    );
    let mut extends = by_rules();
    extends.relation_kinds = vec![RelationKind::Extends];
    assert_eq!(fixture.summarize(&extends).gaps.len(), 1);
    let mut calls = by_rules();
    calls.relation_kinds = vec![RelationKind::Calls];
    assert!(fixture.summarize(&calls).gaps.is_empty());
}

#[test]
fn stale_structure_and_dirty_relations_are_exposed() {
    let fixture = Fixture::new();
    let a = fixture.resource("src/shared/a.ts");
    let b = fixture.resource("src/shared/b.ts");
    let c = fixture.resource("src/shared/c.ts");
    let d = fixture.resource("src/shared/d.ts");
    fixture.structure(&a, StructuralState::Partial);
    fixture.structure(&b, StructuralState::Unsupported);
    fixture.relation_dirty(&c);
    fixture
        .connection
        .execute(
            "DELETE FROM component_state WHERE component_kind = 'STRUCTURAL_INDEX' \
             AND scope_key = ?1",
            params![structural::scope_key(d.id)],
        )
        .unwrap();
    let summary = fixture.summarize(&by_rules());
    let shared = group(&summary, &g("shared")).coverage;
    assert_eq!(
        (
            shared.supported,
            shared.partial,
            shared.unsupported,
            shared.structure_not_current,
            shared.relation_dirty
        ),
        (1, 1, 2, 2, 1)
    );
    assert_eq!(
        summary.limits().limits(),
        [
            CoverageLimit::PartialSupport,
            CoverageLimit::UnsupportedScope,
            CoverageLimit::DirtyRelationComponent,
        ]
    );
    assert_eq!(
        summary.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

#[test]
fn safe_zero_distinguishes_complete_from_incomplete() {
    let fixture = Fixture::new();
    let a = fixture.resource("src/shared/a.ts");
    let b = fixture.resource("src/routes/b.ts");
    fixture.relation("IMPORTS", a.entity, a.entity, &a, 1);
    let complete = fixture.summarize(&by_rules());
    assert_eq!(complete.boundary_edge_count(), 0);
    assert!(complete.limits().is_complete());
    assert_eq!(
        complete.answer_state(),
        AnswerState::NoneUnderCompleteCoverage
    );

    fixture.gap(&b, "IMPORTS", "MISSING_RELATIVE_TARGET", &[], false);
    let incomplete = fixture.summarize(&by_rules());
    assert_eq!(incomplete.boundary_edge_count(), 0);
    assert_eq!(
        incomplete.answer_state(),
        AnswerState::NoneWithIncompleteCoverage
    );
}

#[test]
fn dirty_index_does_not_masquerade_as_current() {
    let fixture = representative(false);
    component::mark_dirty(&fixture.connection, "rev-1", None).unwrap();
    let summary = fixture.summarize(&by_rules());
    assert_eq!(
        summary.currentness,
        Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty)
    );
    assert!(summary.limits().has(CoverageLimit::IndexNotCurrent));
    // Known facts are still returned.
    assert_eq!(summary.boundary_edge_count(), 7);

    let never = Fixture::new();
    never
        .connection
        .execute("DELETE FROM component_state", [])
        .unwrap();
    assert_eq!(
        never.summarize(&by_rules()).currentness,
        Currentness::NotCurrent(NotCurrentReason::ResourceIndexNeverPublished)
    );
}

#[test]
fn semantic_conflict_and_not_current_remain_visible() {
    let fixture = Fixture::new();
    let a = fixture.resource("src/shared/a.ts");
    let b = fixture.resource("src/routes/b.ts");
    let a_fn = fixture.symbol(&a, "f");
    let b_fn = fixture.symbol(&b, "g");
    // A semantically proven edge counts as confirmed.
    let relation = fixture.relation("CALLS", b_fn.entity, a_fn.entity, &b, 0);
    let site = fixture.occurrence(&b, Some(relation));
    fixture
        .connection
        .execute(
            "INSERT INTO semantic_evidence (context_key, occurrence_id, relation_id, capability, \
             proof_role, analysis_profile_id, generation_id) \
             VALUES ('ts', ?1, ?2, 'CALLS', 'SEMANTIC_ONLY', ?3, ?4)",
            params![site, relation, fixture.profile, fixture.generation],
        )
        .unwrap();
    let summary = fixture.summarize(&by_rules());
    assert_eq!(summary.boundary_edge_count(), 1);
    // No CURRENT owner row: the contribution is not current.
    assert_eq!(
        group(&summary, &g("routes")).coverage.semantic_not_current,
        1
    );

    component::write_scoped(
        &fixture.connection,
        component::SEMANTIC_INDEX,
        component::SEMANTIC_OWNER_SCOPE_KIND,
        &crate::semantic_index::owner_scope_key("ts", b.id),
        &ComponentRow {
            basis_workspace_revision: "rev-1".to_owned(),
            stable_generation_id: Some(fixture.generation),
            processing_state: ProcessingState::Ready,
            freshness_state: FreshnessState::Current,
            last_error_code: None,
            detail_state: Some("CURRENT".to_owned()),
        },
    )
    .unwrap();
    let current = fixture.summarize(&by_rules());
    assert_eq!(
        group(&current, &g("routes")).coverage.semantic_not_current,
        0
    );
    assert!(current.limits().is_complete());

    let conflict_site = fixture.occurrence(&b, None);
    fixture
        .connection
        .execute(
            "INSERT INTO semantic_conflict (context_key, occurrence_id, relation_kind, \
             source_entity_id, structural_target_entity_id, semantic_target_entity_id, \
             analysis_profile_id, generation_id) VALUES ('ts', ?1, 'CALLS', ?2, ?3, ?4, ?5, ?6)",
            params![
                conflict_site,
                b_fn.entity,
                a_fn.entity,
                b.entity,
                fixture.profile,
                fixture.generation
            ],
        )
        .unwrap();
    let conflicted = fixture.summarize(&by_rules());
    assert_eq!(
        group(&conflicted, &g("routes")).coverage.semantic_conflicts,
        1
    );
    assert!(conflicted.limits().has(CoverageLimit::SemanticConflict));
}

#[test]
fn workspace_binding_is_checked() {
    let fixture = representative(false);
    let mut other = by_rules();
    other.workspace = WorkspaceId::from_bytes(stable_bytes("other"));
    assert!(matches!(
        fixture.index().summarize(&other),
        Err(SummaryError::WorkspaceMismatch { .. })
    ));
}

// ---------------------------------------------------------------------
// SQL access path and boundaries
// ---------------------------------------------------------------------

fn plan_of(connection: &Connection, sql: &str, params: &[Value]) -> Vec<String> {
    let mut statement = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("explain");
    statement
        .query_map(params_from_iter(params.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("plan rows")
        .collect::<Result<_, _>>()
        .expect("plan")
}

/// Every statement, under every grouping shape and with a kind filter,
/// is index-driven on the large tables.
#[test]
fn query_plans_do_not_scan_relation_or_evidence_tables() {
    let fixture = representative(false);
    let mut filtered = by_rules();
    filtered.relation_kinds = vec![RelationKind::Calls, RelationKind::Imports];
    filtered.resource_scope.path_prefix = Some("src/".to_owned());
    let requests = [
        ("prefix", by_rules()),
        ("prefix+filter", filtered),
        (
            "depth",
            request(GroupingSpec::DirectoryDepth {
                root: "src/features/".to_owned(),
                depth: 1,
            }),
        ),
        ("role", request(GroupingSpec::ResourceRole)),
    ];
    let forbidden = [
        "SCAN rel", "SCAN o", "SCAN y", "SCAN ge", "SCAN u", "SCAN d", "SCAN e", "SCAN c",
    ];
    let mut violations = Vec::new();
    for (shape, request) in requests {
        let plan = Plan::of(&request).expect("valid");
        for (name, sql, params) in plan.statements() {
            for line in plan_of(&fixture.connection, &sql, &params) {
                println!("TASK9 EQP {shape}/{name}: {line}");
                if forbidden
                    .iter()
                    .any(|scan| line == *scan || line.starts_with(&format!("{scan} ")))
                {
                    violations.push(format!("{shape}/{name}: {line}"));
                }
            }
        }
    }
    assert!(violations.is_empty(), "large-table scans: {violations:#?}");
}

#[test]
fn statement_count_and_rows_scale_with_groups_not_evidence() {
    let fixture = representative(false);
    let index = fixture.index();
    index.summarize(&by_rules()).unwrap();
    let small = index.stats();
    // Grow Resources, relations and evidence tenfold inside auth.
    let login = Res {
        id: ResourceId::from_bytes(stable_bytes("src/features/auth/login.ts")),
        row: 2,
        entity: 0,
    };
    for at in 0..30 {
        let file = fixture.resource(&format!("src/features/auth/more{at:02}.ts"));
        let callee = fixture.symbol(&file, "f");
        let caller = fixture.symbol(&login, &format!("c{at}"));
        fixture.relation("CALLS", caller.entity, callee.entity, &login, 5);
    }
    let index = fixture.index();
    let summary = index.summarize(&by_rules()).unwrap();
    let large = index.stats();
    assert_eq!(group(&summary, &g("auth")).internal_edges, 31);
    assert_eq!(large.statements, small.statements);
    assert_eq!(large.rows, small.rows);
}

#[test]
fn the_module_reads_no_source_and_adds_no_structure() {
    let source = include_str!("../boundary.rs");
    for banned in [
        "SourceReader",
        "std::fs",
        "read_to_string",
        "lsp",
        "semantic_lifecycle",
        "runtime::",
        "DeliveryLedger",
        "economy",
        "lookup_name",
        "CREATE TABLE",
        "CREATE INDEX",
        "INSERT INTO",
        "UPDATE ",
        "recommend",
        "score",
    ] {
        assert!(!source.contains(banned), "boundary.rs mentions {banned}");
    }
}

#[test]
fn schema_versions_are_unchanged() {
    let fixture = Fixture::new();
    let version: i64 = fixture
        .connection
        .query_row("SELECT MAX(version) FROM schema_migration", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, 10);
    assert_eq!(crate::schema::index::INDEX_MIGRATIONS.len(), 10);
}
