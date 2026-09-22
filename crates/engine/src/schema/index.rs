//! `index.db` I1 schema: rebuildable code-intelligence index.
//!
//! Canonical tables per #13 task 6 §5-16 / task 7 §2, §4, §6, §8-10:
//! `generation`, `workspace_clock`, `component_state`, `change_journal`,
//! `analysis_profile`, `resolution_context`, `resource`, `symbol`,
//! `external_entity`, `domain_entity`, `graph_entity`, `relation`,
//! `occurrence`, `unresolved_reference`, `relation_candidate`.
//! `db_meta`/`schema_migration` are provided by the shared runner
//! ([`crate::db`]) and are not redefined here.
//!
//! This is schema only: no Resource discovery, Tree-sitter parsing,
//! relation extraction, or semantic resolution runs against these tables
//! in I1. `scope` (mentioned only as "if persisted" in #13 task 3 §7,
//! with no confirmed column list) and `symbol_search_fts` (explicitly
//! derived/optional per #13 task 6 §17) are not created here.

use std::path::Path;

use crate::db::{self, DbKind, DbOpenError, Migration, OpenedDb};

pub const INDEX_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "create_index_foundation_tables",
        sql: "
        CREATE TABLE generation (
            id INTEGER PRIMARY KEY,
            generation_no INTEGER NOT NULL UNIQUE,
            basis_workspace_revision TEXT NOT NULL,
            state TEXT NOT NULL,
            created_at TEXT NOT NULL,
            published_at TEXT,
            aborted_reason TEXT
        );

        CREATE TABLE workspace_clock (
            id INTEGER PRIMARY KEY CHECK (id = 0),
            current_workspace_revision TEXT NOT NULL,
            stable_generation_id INTEGER REFERENCES generation (id),
            last_change_seq INTEGER NOT NULL,
            last_reconcile_seq INTEGER NOT NULL,
            last_full_reconcile_at TEXT,
            watcher_continuity_state TEXT NOT NULL
        );

        CREATE TABLE component_state (
            id INTEGER PRIMARY KEY,
            component_kind TEXT NOT NULL,
            scope_kind TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            basis_workspace_revision TEXT NOT NULL,
            stable_generation_id INTEGER REFERENCES generation (id),
            processing_state TEXT NOT NULL,
            freshness_state TEXT NOT NULL,
            last_error_code TEXT,
            updated_at TEXT NOT NULL,
            UNIQUE (component_kind, scope_kind, scope_key)
        );

        CREATE TABLE change_journal (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            workspace_revision TEXT NOT NULL,
            event_kind TEXT NOT NULL,
            resource_uid BLOB,
            path_before TEXT,
            path_after TEXT,
            observed_size INTEGER,
            observed_mtime_ns INTEGER,
            candidate_fingerprint TEXT,
            processing_state TEXT NOT NULL,
            coalesced_into_seq INTEGER REFERENCES change_journal (seq),
            observed_at TEXT NOT NULL,
            applied_generation_id INTEGER REFERENCES generation (id)
        );
        CREATE INDEX idx_change_journal_processing_state ON change_journal (processing_state);

        CREATE TABLE analysis_profile (
            id INTEGER PRIMARY KEY,
            profile_key TEXT NOT NULL UNIQUE,
            language TEXT NOT NULL,
            analysis_mode TEXT NOT NULL,
            structural_backend TEXT NOT NULL,
            structural_backend_version TEXT NOT NULL,
            semantic_backend TEXT,
            semantic_backend_version TEXT,
            extractor_semantics_version TEXT NOT NULL,
            adapter_semantics_version TEXT NOT NULL,
            backend_compatibility_class TEXT NOT NULL,
            capability_fingerprint TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE resolution_context (
            id INTEGER PRIMARY KEY,
            context_key TEXT NOT NULL UNIQUE,
            language TEXT NOT NULL,
            scope_key TEXT NOT NULL,
            config_fingerprint TEXT NOT NULL,
            dependency_fingerprint TEXT NOT NULL,
            environment_fingerprint TEXT NOT NULL,
            module_resolution_fingerprint TEXT NOT NULL,
            backend_snapshot_token TEXT,
            created_generation INTEGER NOT NULL REFERENCES generation (id)
        );

        CREATE TABLE resource (
            id INTEGER PRIMARY KEY,
            uid BLOB NOT NULL UNIQUE,
            path_rel TEXT NOT NULL,
            path_key TEXT NOT NULL UNIQUE,
            kind TEXT NOT NULL,
            role TEXT NOT NULL,
            language TEXT,
            size INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL,
            fingerprint TEXT NOT NULL,
            content_hash TEXT,
            state TEXT NOT NULL,
            resource_revision TEXT NOT NULL,
            generated_kind TEXT,
            container_resource_id INTEGER REFERENCES resource (id)
        );
        CREATE INDEX idx_resource_role ON resource (role);
        CREATE INDEX idx_resource_language ON resource (language);
        CREATE INDEX idx_resource_state ON resource (state);

        CREATE TABLE symbol (
            id INTEGER PRIMARY KEY,
            uid BLOB NOT NULL UNIQUE,
            resource_id INTEGER NOT NULL REFERENCES resource (id),
            parent_symbol_id INTEGER REFERENCES symbol (id),
            kind TEXT NOT NULL,
            name TEXT NOT NULL,
            qualified_name TEXT NOT NULL,
            signature TEXT,
            visibility TEXT NOT NULL,
            exported INTEGER NOT NULL,
            start_byte INTEGER NOT NULL,
            end_byte INTEGER NOT NULL,
            start_line INTEGER NOT NULL,
            start_col INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            end_col INTEGER NOT NULL,
            resource_revision TEXT NOT NULL,
            analysis_profile_id INTEGER NOT NULL REFERENCES analysis_profile (id)
        );
        CREATE INDEX idx_symbol_resource_start ON symbol (resource_id, start_byte);
        CREATE INDEX idx_symbol_name ON symbol (name);
        CREATE INDEX idx_symbol_qualified_name ON symbol (qualified_name);
        CREATE INDEX idx_symbol_parent ON symbol (parent_symbol_id);
        CREATE INDEX idx_symbol_kind_name ON symbol (kind, name);

        CREATE TABLE external_entity (
            id INTEGER PRIMARY KEY,
            package_identity TEXT NOT NULL,
            module_path TEXT,
            symbol_name TEXT,
            qualified_name TEXT,
            kind TEXT NOT NULL,
            resolved_version TEXT,
            declaration_locator TEXT,
            UNIQUE (package_identity, module_path, symbol_name)
        );

        CREATE TABLE domain_entity (
            id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL,
            normalized_identity TEXT NOT NULL,
            namespace TEXT,
            method TEXT,
            display_label TEXT NOT NULL,
            UNIQUE (kind, normalized_identity, namespace, method)
        );

        CREATE TABLE graph_entity (
            id INTEGER PRIMARY KEY,
            entity_kind TEXT NOT NULL,
            resource_id INTEGER REFERENCES resource (id),
            symbol_id INTEGER REFERENCES symbol (id),
            external_entity_id INTEGER REFERENCES external_entity (id),
            domain_entity_id INTEGER REFERENCES domain_entity (id),
            CHECK (
                (CASE WHEN resource_id IS NOT NULL THEN 1 ELSE 0 END +
                 CASE WHEN symbol_id IS NOT NULL THEN 1 ELSE 0 END +
                 CASE WHEN external_entity_id IS NOT NULL THEN 1 ELSE 0 END +
                 CASE WHEN domain_entity_id IS NOT NULL THEN 1 ELSE 0 END) = 1
            )
        );
        CREATE UNIQUE INDEX idx_graph_entity_resource ON graph_entity (resource_id) WHERE resource_id IS NOT NULL;
        CREATE UNIQUE INDEX idx_graph_entity_symbol ON graph_entity (symbol_id) WHERE symbol_id IS NOT NULL;
        CREATE UNIQUE INDEX idx_graph_entity_external ON graph_entity (external_entity_id) WHERE external_entity_id IS NOT NULL;
        CREATE UNIQUE INDEX idx_graph_entity_domain ON graph_entity (domain_entity_id) WHERE domain_entity_id IS NOT NULL;

        CREATE TABLE relation (
            id INTEGER PRIMARY KEY,
            kind TEXT NOT NULL,
            source_entity_id INTEGER NOT NULL REFERENCES graph_entity (id),
            target_entity_id INTEGER NOT NULL REFERENCES graph_entity (id),
            dispatch TEXT,
            target_scope TEXT,
            created_generation INTEGER NOT NULL REFERENCES generation (id),
            UNIQUE (kind, source_entity_id, target_entity_id, dispatch)
        );
        CREATE INDEX idx_relation_source_kind ON relation (source_entity_id, kind);
        CREATE INDEX idx_relation_target_kind ON relation (target_entity_id, kind);
        CREATE INDEX idx_relation_kind_source_target ON relation (kind, source_entity_id, target_entity_id);

        CREATE TABLE occurrence (
            id INTEGER PRIMARY KEY,
            resource_id INTEGER NOT NULL REFERENCES resource (id),
            containing_symbol_id INTEGER REFERENCES symbol (id),
            kind TEXT NOT NULL,
            start_byte INTEGER NOT NULL,
            end_byte INTEGER NOT NULL,
            start_line INTEGER NOT NULL,
            start_col INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            end_col INTEGER NOT NULL,
            relation_id INTEGER REFERENCES relation (id),
            analysis_profile_id INTEGER NOT NULL REFERENCES analysis_profile (id),
            resolution_context_id INTEGER REFERENCES resolution_context (id),
            resource_revision TEXT NOT NULL,
            generation INTEGER NOT NULL REFERENCES generation (id)
        );
        CREATE INDEX idx_occurrence_resource_start ON occurrence (resource_id, start_byte);
        CREATE INDEX idx_occurrence_relation ON occurrence (relation_id);
        CREATE INDEX idx_occurrence_containing_symbol ON occurrence (containing_symbol_id);
        CREATE INDEX idx_occurrence_kind_resource ON occurrence (kind, resource_id);

        CREATE TABLE unresolved_reference (
            id INTEGER PRIMARY KEY,
            occurrence_id INTEGER NOT NULL UNIQUE REFERENCES occurrence (id),
            intended_relation_kind TEXT NOT NULL,
            lookup_name TEXT NOT NULL,
            module_hint TEXT,
            reason TEXT NOT NULL,
            resolution_context_id INTEGER REFERENCES resolution_context (id),
            candidate_truncated INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX idx_unresolved_reference_lookup_name ON unresolved_reference (lookup_name);
        CREATE INDEX idx_unresolved_reference_kind ON unresolved_reference (intended_relation_kind);
        CREATE INDEX idx_unresolved_reference_context ON unresolved_reference (resolution_context_id);

        CREATE TABLE relation_candidate (
            id INTEGER PRIMARY KEY,
            unresolved_reference_id INTEGER NOT NULL REFERENCES unresolved_reference (id),
            target_entity_id INTEGER NOT NULL REFERENCES graph_entity (id),
            evidence_kind TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            UNIQUE (unresolved_reference_id, target_entity_id)
        );
    ",
    },
    Migration {
        version: 2,
        name: "add_workspace_identity_binding",
        sql: "ALTER TABLE db_meta ADD COLUMN project_uid BLOB; \
              ALTER TABLE db_meta ADD COLUMN workspace_uid BLOB;",
    },
    Migration {
        version: 3,
        name: "add_component_detail_state",
        // A component's own state, alongside the two axes every component
        // shares (#16 task 14). `processing_state`/`freshness_state` say
        // whether work is outstanding and whether the rows are current;
        // they cannot say that a Resource's structure is PARTIAL because
        // the file does not parse, or UNSUPPORTED because no grammar
        // covers it. Those are not errors, so hiding them in
        // `last_error_code` would make every reader guess. NULL for a
        // component that has no such distinction to draw.
        sql: "ALTER TABLE component_state ADD COLUMN detail_state TEXT;",
    },
    Migration {
        version: 4,
        name: "add_relation_null_dispatch_uniqueness",
        // `relation`'s UNIQUE (kind, source, target, dispatch) is the
        // intended identity of an edge, but SQLite treats NULLs as
        // distinct, so it does not constrain the rows that matter most:
        // an edge whose dispatch is not yet known (#17 task 2 owns that
        // vocabulary). Without this, the same CALLS edge could be stored
        // twice. A partial unique index covers exactly that case and
        // changes nothing for rows that do carry a dispatch.
        sql: "CREATE UNIQUE INDEX idx_relation_unique_null_dispatch \
              ON relation (kind, source_entity_id, target_entity_id) \
              WHERE dispatch IS NULL;",
    },
    Migration {
        version: 5,
        name: "canonical_relation_identity",
        // #17 task 2 settles what identifies an edge: `kind`, `source`,
        // `target`. Not `dispatch` -- "A calls B" does not become two
        // facts because one call site is dynamic; per-site dispatch is
        // Occurrence evidence. Not `target_scope` either, which is a
        // function of the target entity and so cannot discriminate.
        //
        // The original UNIQUE (kind, source, target, dispatch) stays: it
        // is strictly weaker than the index below, so it constrains
        // nothing new and rejects nothing this contract allows. What
        // makes new writes correct is `idx_relation_identity`, not the
        // NULL-only partial index migration 4 added for the same reason.
        //
        // Before it can exist, the rows have to mean one thing. The
        // order matters: rows that were only distinct because NULL
        // never collides are collapsed to the first *before* dispatch
        // is filled in, or the backfill would make them collide under
        // the original UNIQUE mid-migration. Then NULL dispatch becomes
        // UNKNOWN, and NULL target_scope is derived from what the
        // target entity actually is.
        sql: "DELETE FROM relation WHERE id NOT IN ( \
                  SELECT MIN(id) FROM relation \
                  GROUP BY kind, source_entity_id, target_entity_id \
              ); \
              UPDATE relation SET dispatch = 'UNKNOWN' WHERE dispatch IS NULL; \
              UPDATE relation SET target_scope = ( \
                  SELECT CASE WHEN graph_entity.external_entity_id IS NOT NULL \
                              THEN 'EXTERNAL' ELSE 'INTERNAL' END \
                  FROM graph_entity WHERE graph_entity.id = relation.target_entity_id \
              ) WHERE target_scope IS NULL; \
              CREATE UNIQUE INDEX idx_relation_identity \
              ON relation (kind, source_entity_id, target_entity_id);",
    },
    Migration {
        version: 6,
        name: "add_semantic_publication",
        // #19 task 3. A semantic result is only current for the inputs
        // it was computed from, and those inputs are not the Workspace
        // revision alone: an interpreter change, a tsconfig edit, or a
        // different capability set can all make the same source mean
        // something else. `component_state` already carries the two
        // shared axes for the SEMANTIC_INDEX component; what it cannot
        // carry is *which* inputs a publication was bound to, which is
        // what makes obsolescence detectable rather than guessed.
        //
        // One row per AnalysisContext: a publication supersedes the
        // previous one for that context, and the dependency set is
        // replaced with it in the same transaction, so metadata and
        // result set can never disagree.
        sql: "CREATE TABLE semantic_publication ( \
                  id INTEGER PRIMARY KEY, \
                  context_key TEXT NOT NULL UNIQUE, \
                  workspace_uid BLOB NOT NULL, \
                  analysis_profile_id INTEGER NOT NULL REFERENCES analysis_profile (id), \
                  generation_id INTEGER NOT NULL REFERENCES generation (id), \
                  basis_workspace_revision TEXT NOT NULL, \
                  basis_fingerprint TEXT NOT NULL, \
                  config_fingerprint TEXT NOT NULL, \
                  environment_fingerprint TEXT NOT NULL, \
                  inventory_fingerprint TEXT, \
                  support TEXT NOT NULL, \
                  published_at TEXT NOT NULL \
              ); \
              CREATE TABLE semantic_publication_source ( \
                  publication_id INTEGER NOT NULL \
                      REFERENCES semantic_publication (id) ON DELETE CASCADE, \
                  resource_id INTEGER NOT NULL REFERENCES resource (id), \
                  resource_revision TEXT NOT NULL, \
                  PRIMARY KEY (publication_id, resource_id) \
              ); \
              CREATE INDEX idx_semantic_publication_source_resource \
              ON semantic_publication_source (resource_id);",
    },
    Migration {
        version: 7,
        name: "add_semantic_merge_contribution",
        // #19 task 4. Semantic results enrich the one canonical graph:
        // an edge a backend proves is a `relation` row like any other,
        // and the site that states it is the `occurrence` the
        // structural tier already recorded. What has nowhere to live in
        // those tables is *which tier proved what*, and that is exactly
        // the thing a downgrade needs to know: a relation that exists
        // only because a backend said so must stop being claimed when
        // the backend's answer stops being current, while one the
        // parser proved must survive the backend disappearing
        // entirely.
        //
        // `proof_role` is that distinction, and `displaced_*` is what
        // the structural gap said before semantic evidence replaced it,
        // so withdrawing a semantic proof restores an honest gap rather
        // than a silence.
        //
        // Both tables cascade from `occurrence`: a structural
        // re-extraction replaces a Resource's occurrences outright, and
        // a semantic contribution anchored to spans that no longer
        // exist is not a contribution.
        sql: "CREATE TABLE semantic_evidence ( \
                  id INTEGER PRIMARY KEY, \
                  context_key TEXT NOT NULL, \
                  occurrence_id INTEGER NOT NULL \
                      REFERENCES occurrence (id) ON DELETE CASCADE, \
                  relation_id INTEGER NOT NULL REFERENCES relation (id), \
                  capability TEXT NOT NULL, \
                  proof_role TEXT NOT NULL, \
                  analysis_profile_id INTEGER NOT NULL REFERENCES analysis_profile (id), \
                  generation_id INTEGER NOT NULL REFERENCES generation (id), \
                  displaced_intended_kind TEXT, \
                  displaced_lookup_name TEXT, \
                  displaced_module_hint TEXT, \
                  displaced_reason TEXT, \
                  UNIQUE (context_key, occurrence_id) \
              ); \
              CREATE INDEX idx_semantic_evidence_relation ON semantic_evidence (relation_id); \
              CREATE INDEX idx_semantic_evidence_occurrence ON semantic_evidence (occurrence_id); \
              CREATE TABLE semantic_conflict ( \
                  id INTEGER PRIMARY KEY, \
                  context_key TEXT NOT NULL, \
                  occurrence_id INTEGER NOT NULL \
                      REFERENCES occurrence (id) ON DELETE CASCADE, \
                  relation_kind TEXT NOT NULL, \
                  source_entity_id INTEGER NOT NULL REFERENCES graph_entity (id), \
                  structural_target_entity_id INTEGER NOT NULL REFERENCES graph_entity (id), \
                  semantic_target_entity_id INTEGER NOT NULL REFERENCES graph_entity (id), \
                  analysis_profile_id INTEGER NOT NULL REFERENCES analysis_profile (id), \
                  generation_id INTEGER NOT NULL REFERENCES generation (id), \
                  UNIQUE (context_key, occurrence_id, relation_kind) \
              ); \
              CREATE INDEX idx_semantic_conflict_occurrence ON semantic_conflict (occurrence_id);",
    },
    Migration {
        version: 8,
        name: "scope_semantic_publication_to_its_owner",
        // #19 task 8. Task 3 keyed a publication by AnalysisContext,
        // which is the right key for the *runtime* -- one Pyright
        // serves the whole project -- but the wrong one for
        // currentness. Task 4 already replaces a contribution per
        // (context, owner Resource), and task 6 refreshes one Resource
        // at a time, so a context-wide CURRENT flag lets one owner's
        // publication vouch for another's:
        //
        //   B published at generation 10, A changes, A republishes at
        //   11 and marks the context CURRENT -- and B's untouched
        //   generation-10 contribution starts reading as current too.
        //
        // The key becomes (context_key, owner_resource_id), which is
        // the unit that was always being replaced. The runtime stays
        // shared: nothing here is per-Resource backend state.
        //
        // The old rows are dropped rather than migrated. A publication
        // is a claim about which inputs a result was computed from, and
        // there is no honest way to split a context-wide claim into
        // per-owner ones after the fact -- so this fails toward
        // "needs revalidation", never toward CURRENT. The semantic
        // evidence rows stay exactly where they are: they are anchored
        // to Occurrences and owned by task 4's replacement, and with no
        // publication vouching for them every contributing scope reads
        // NOT CURRENT until its owner is refreshed. That is the honest
        // state, and it is what `semantic_scope` reports.
        sql: "DROP TABLE semantic_publication_source; \
              DROP TABLE semantic_publication; \
              CREATE TABLE semantic_publication ( \
                  id INTEGER PRIMARY KEY, \
                  context_key TEXT NOT NULL, \
                  owner_resource_id INTEGER NOT NULL REFERENCES resource (id), \
                  workspace_uid BLOB NOT NULL, \
                  analysis_profile_id INTEGER NOT NULL REFERENCES analysis_profile (id), \
                  generation_id INTEGER NOT NULL REFERENCES generation (id), \
                  basis_workspace_revision TEXT NOT NULL, \
                  basis_fingerprint TEXT NOT NULL, \
                  config_fingerprint TEXT NOT NULL, \
                  environment_fingerprint TEXT NOT NULL, \
                  inventory_fingerprint TEXT, \
                  support TEXT NOT NULL, \
                  published_at TEXT NOT NULL, \
                  UNIQUE (context_key, owner_resource_id) \
              ); \
              CREATE INDEX idx_semantic_publication_owner \
              ON semantic_publication (owner_resource_id); \
              CREATE TABLE semantic_publication_source ( \
                  publication_id INTEGER NOT NULL \
                      REFERENCES semantic_publication (id) ON DELETE CASCADE, \
                  resource_id INTEGER NOT NULL REFERENCES resource (id), \
                  resource_revision TEXT NOT NULL, \
                  PRIMARY KEY (publication_id, resource_id) \
              ); \
              CREATE INDEX idx_semantic_publication_source_resource \
              ON semantic_publication_source (resource_id); \
              DELETE FROM component_state \
              WHERE component_kind = 'SEMANTIC_INDEX' AND scope_kind = 'ANALYSIS_CONTEXT';",
    },
    Migration {
        version: 9,
        name: "logical_symbol_identity",
        // #19 task 12. A C# `partial class` is one semantic type with
        // several declarations, and Roslyn answers a reference to it with
        // *all* of them. The canonical model had nowhere to put that: a
        // `Symbol` is one Resource and one span by design, and task 4
        // binds one Occurrence to exactly one relation
        // (`UNIQUE (context_key, occurrence_id)`), which is a rule worth
        // keeping -- a reference does bind to one thing.
        //
        // So the missing piece was never a second binding. It was the
        // *thing* being bound to: one semantic symbol that owns several
        // source declarations. That is what `logical_symbol` is, and it
        // is deliberately language-neutral -- nothing here says C#, and a
        // later language that merges declarations uses the same table.
        //
        // Source Symbols are untouched. They remain the exact editable
        // declarations, and the group only points at them.
        //
        // `graph_entity` is rebuilt rather than altered because its
        // exactly-one-payload CHECK has to learn a fifth column, and
        // SQLite cannot alter a CHECK in place. Foreign keys are deferred
        // to COMMIT for the swap, which is SQLite's own documented
        // procedure -- `PRAGMA foreign_keys` is a no-op inside a
        // transaction, `defer_foreign_keys` is not.
        sql: "PRAGMA defer_foreign_keys = ON; \
              CREATE TABLE logical_symbol ( \
                  id INTEGER PRIMARY KEY, \
                  uid BLOB NOT NULL UNIQUE, \
                  identity_fingerprint TEXT NOT NULL UNIQUE, \
                  context_key TEXT NOT NULL, \
                  kind TEXT NOT NULL, \
                  display_name TEXT NOT NULL, \
                  created_generation INTEGER NOT NULL REFERENCES generation (id) \
              ); \
              CREATE INDEX idx_logical_symbol_context ON logical_symbol (context_key); \
              CREATE TABLE logical_symbol_declaration ( \
                  logical_symbol_id INTEGER NOT NULL \
                      REFERENCES logical_symbol (id) ON DELETE CASCADE, \
                  symbol_id INTEGER NOT NULL REFERENCES symbol (id) ON DELETE CASCADE, \
                  context_key TEXT NOT NULL, \
                  generation_id INTEGER NOT NULL REFERENCES generation (id), \
                  PRIMARY KEY (logical_symbol_id, symbol_id) \
              ); \
              CREATE INDEX idx_logical_declaration_symbol \
              ON logical_symbol_declaration (symbol_id); \
              CREATE TABLE graph_entity_next ( \
                  id INTEGER PRIMARY KEY, \
                  entity_kind TEXT NOT NULL, \
                  resource_id INTEGER REFERENCES resource (id), \
                  symbol_id INTEGER REFERENCES symbol (id), \
                  external_entity_id INTEGER REFERENCES external_entity (id), \
                  domain_entity_id INTEGER REFERENCES domain_entity (id), \
                  logical_symbol_id INTEGER REFERENCES logical_symbol (id), \
                  CHECK ( \
                      (CASE WHEN resource_id IS NOT NULL THEN 1 ELSE 0 END + \
                       CASE WHEN symbol_id IS NOT NULL THEN 1 ELSE 0 END + \
                       CASE WHEN external_entity_id IS NOT NULL THEN 1 ELSE 0 END + \
                       CASE WHEN domain_entity_id IS NOT NULL THEN 1 ELSE 0 END + \
                       CASE WHEN logical_symbol_id IS NOT NULL THEN 1 ELSE 0 END) = 1 \
                  ) \
              ); \
              INSERT INTO graph_entity_next \
                  (id, entity_kind, resource_id, symbol_id, external_entity_id, \
                   domain_entity_id) \
              SELECT id, entity_kind, resource_id, symbol_id, external_entity_id, \
                     domain_entity_id FROM graph_entity; \
              DROP TABLE graph_entity; \
              ALTER TABLE graph_entity_next RENAME TO graph_entity; \
              CREATE UNIQUE INDEX idx_graph_entity_resource \
              ON graph_entity (resource_id) WHERE resource_id IS NOT NULL; \
              CREATE UNIQUE INDEX idx_graph_entity_symbol \
              ON graph_entity (symbol_id) WHERE symbol_id IS NOT NULL; \
              CREATE UNIQUE INDEX idx_graph_entity_external \
              ON graph_entity (external_entity_id) WHERE external_entity_id IS NOT NULL; \
              CREATE UNIQUE INDEX idx_graph_entity_domain \
              ON graph_entity (domain_entity_id) WHERE domain_entity_id IS NOT NULL; \
              CREATE UNIQUE INDEX idx_graph_entity_logical \
              ON graph_entity (logical_symbol_id) WHERE logical_symbol_id IS NOT NULL;",
    },
];

/// Open (creating and migrating if needed) an `index.db` at `path`.
pub fn open(path: &Path) -> Result<OpenedDb, DbOpenError> {
    db::open(path, DbKind::Index, INDEX_MIGRATIONS)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use rusqlite::{OptionalExtension, params};

    use super::*;
    use crate::db::DbOpenError;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "brainprint-schema-index-{label}-{}-{sequence}",
                process::id()
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn db_path(&self) -> PathBuf {
            self.0.join("data").join("index.db")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn table_exists(opened: &OpenedDb, table: &str) -> bool {
        opened
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |_| Ok(()),
            )
            .optional()
            .expect("table existence query should not fail")
            .is_some()
    }

    const ALL_CANONICAL_TABLES: &[&str] = &[
        "generation",
        "workspace_clock",
        "component_state",
        "change_journal",
        "analysis_profile",
        "resolution_context",
        "resource",
        "symbol",
        "external_entity",
        "domain_entity",
        "graph_entity",
        "relation",
        "occurrence",
        "unresolved_reference",
        "relation_candidate",
        "semantic_publication",
        "semantic_publication_source",
        "semantic_evidence",
        "semantic_conflict",
    ];

    #[test]
    fn fresh_rebuildable_index_db_can_be_created() {
        let dir = TestDir::create("fresh");
        let opened = open(&dir.db_path()).expect("fresh index.db should migrate");
        assert_eq!(opened.schema_version, 9);
    }

    #[test]
    fn canonical_i1_tables_exist() {
        let dir = TestDir::create("canonical-tables");
        let opened = open(&dir.db_path()).expect("index.db should migrate");

        for table in ALL_CANONICAL_TABLES {
            assert!(
                table_exists(&opened, table),
                "missing canonical table {table}"
            );
        }
    }

    #[test]
    fn generation_workspace_clock_component_state_change_journal_schema_exists() {
        let dir = TestDir::create("revision-generation-schema");
        let opened = open(&dir.db_path()).expect("index.db should migrate");

        for table in [
            "generation",
            "workspace_clock",
            "component_state",
            "change_journal",
        ] {
            assert!(
                table_exists(&opened, table),
                "missing revision/generation table {table}"
            );
        }
    }

    fn insert_generation(opened: &OpenedDb, generation_no: i64) -> i64 {
        opened
            .connection
            .execute(
                "INSERT INTO generation (generation_no, basis_workspace_revision, state, created_at) \
                 VALUES (?1, 'rev-1', 'STABLE', '0')",
                params![generation_no],
            )
            .expect("generation insert should succeed");
        opened.connection.last_insert_rowid()
    }

    fn insert_analysis_profile(opened: &OpenedDb, key: &str) -> i64 {
        opened
            .connection
            .execute(
                "INSERT INTO analysis_profile \
                 (profile_key, language, analysis_mode, structural_backend, structural_backend_version, \
                  extractor_semantics_version, adapter_semantics_version, backend_compatibility_class, \
                  capability_fingerprint, created_at) \
                 VALUES (?1, 'rust', 'STRUCTURAL', 'tree-sitter', '1', '1', '1', '1', 'fp', '0')",
                params![key],
            )
            .expect("analysis_profile insert should succeed");
        opened.connection.last_insert_rowid()
    }

    fn insert_resource(opened: &OpenedDb, uid: u8, path_key: &str) -> i64 {
        opened
            .connection
            .execute(
                "INSERT INTO resource \
                 (uid, path_rel, path_key, kind, role, size, mtime_ns, fingerprint, state, resource_revision) \
                 VALUES (?1, ?2, ?2, 'FILE', 'SOURCE', 0, 0, 'fp', 'ACTIVE', 'rev-1')",
                params![vec![uid; 16], path_key],
            )
            .expect("resource insert should succeed");
        opened.connection.last_insert_rowid()
    }

    fn insert_graph_entity_for_resource(opened: &OpenedDb, resource_id: i64) -> i64 {
        opened
            .connection
            .execute(
                "INSERT INTO graph_entity (entity_kind, resource_id) VALUES ('RESOURCE', ?1)",
                params![resource_id],
            )
            .expect("graph_entity insert should succeed");
        opened.connection.last_insert_rowid()
    }

    #[test]
    fn relation_fk_is_only_valid_within_this_index_db() {
        let dir = TestDir::create("relation-fk");
        let opened = open(&dir.db_path()).expect("index.db should migrate");
        let generation_id = insert_generation(&opened, 1);
        let resource_id = insert_resource(&opened, 1, "a.rs");
        let entity_id = insert_graph_entity_for_resource(&opened, resource_id);

        opened
            .connection
            .execute(
                "INSERT INTO relation (kind, source_entity_id, target_entity_id, created_generation) \
                 VALUES ('IMPORTS', ?1, ?1, ?2)",
                params![entity_id, generation_id],
            )
            .expect("relation between existing graph_entity rows should succeed");

        let error = opened
            .connection
            .execute(
                "INSERT INTO relation (kind, source_entity_id, target_entity_id, created_generation) \
                 VALUES ('IMPORTS', ?1, ?2, ?3)",
                params![entity_id, 999_999_i64, generation_id],
            )
            .expect_err("relation targeting a nonexistent graph_entity must be rejected");

        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(inner, _) if inner.code == rusqlite::ErrorCode::ConstraintViolation
        ));
    }

    #[test]
    fn occurrence_fk_is_only_valid_within_this_index_db() {
        let dir = TestDir::create("occurrence-fk");
        let opened = open(&dir.db_path()).expect("index.db should migrate");
        let generation_id = insert_generation(&opened, 1);
        let profile_id = insert_analysis_profile(&opened, "profile-1");
        let resource_id = insert_resource(&opened, 2, "b.rs");

        opened
            .connection
            .execute(
                "INSERT INTO occurrence \
                 (resource_id, kind, start_byte, end_byte, start_line, start_col, end_line, end_col, \
                  analysis_profile_id, resource_revision, generation) \
                 VALUES (?1, 'DEFINITION', 0, 1, 1, 0, 1, 1, ?2, 'rev-1', ?3)",
                params![resource_id, profile_id, generation_id],
            )
            .expect("occurrence for an existing resource should succeed");

        let error = opened
            .connection
            .execute(
                "INSERT INTO occurrence \
                 (resource_id, kind, start_byte, end_byte, start_line, start_col, end_line, end_col, \
                  analysis_profile_id, resource_revision, generation) \
                 VALUES (?1, 'DEFINITION', 0, 1, 1, 0, 1, 1, ?2, 'rev-1', ?3)",
                params![999_999_i64, profile_id, generation_id],
            )
            .expect_err("occurrence for a nonexistent resource must be rejected");

        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(inner, _) if inner.code == rusqlite::ErrorCode::ConstraintViolation
        ));
    }

    #[test]
    fn graph_entity_requires_exactly_one_target() {
        let dir = TestDir::create("graph-entity-check");
        let opened = open(&dir.db_path()).expect("index.db should migrate");

        let error = opened
            .connection
            .execute(
                "INSERT INTO graph_entity (entity_kind) VALUES ('RESOURCE')",
                [],
            )
            .expect_err("graph_entity with no target must be rejected");
        assert!(matches!(
            error,
            rusqlite::Error::SqliteFailure(inner, _) if inner.code == rusqlite::ErrorCode::ConstraintViolation
        ));

        let resource_id = insert_resource(&opened, 3, "c.rs");
        opened
            .connection
            .execute(
                "INSERT INTO graph_entity (entity_kind, resource_id, symbol_id) VALUES ('RESOURCE', ?1, NULL)",
                params![resource_id],
            )
            .expect("single-target graph_entity should succeed");

        // A second graph_entity for the same resource_id must be rejected
        // by the partial unique index (not exactly-one-target, but the
        // 1:1 entity-spine constraint from #13 task 6 §8).
        let second = opened.connection.execute(
            "INSERT INTO graph_entity (entity_kind, resource_id) VALUES ('RESOURCE', ?1)",
            params![resource_id],
        );
        assert!(
            second.is_err(),
            "duplicate graph_entity for one resource must be rejected"
        );
    }

    #[test]
    fn stable_uid_and_local_row_id_are_distinct() {
        let dir = TestDir::create("uid-vs-rowid");
        let opened = open(&dir.db_path()).expect("index.db should migrate");
        let stable_uid = vec![0x42u8; 16];
        let resource_id = insert_resource(&opened, 0x42, "d.rs");

        // The local row id is a small sequential SQLite rowid; the stable
        // uid is the full 16-byte external identity. They must not be
        // conflated: looking a row up by uid must return the same local
        // id, and the two representations must differ in width.
        let (found_id, found_uid): (i64, Vec<u8>) = opened
            .connection
            .query_row(
                "SELECT id, uid FROM resource WHERE path_key = 'd.rs'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("resource should be queryable by its filesystem-scoped path_key");

        assert_eq!(found_id, resource_id);
        assert_eq!(found_uid, stable_uid);
        assert_eq!(
            found_uid.len(),
            16,
            "stable uid must be the full 16-byte identity"
        );
    }

    #[test]
    fn reopen_does_not_reapply_index_migrations() {
        let dir = TestDir::create("reopen");
        open(&dir.db_path()).expect("first open should migrate");
        let reopened = open(&dir.db_path()).expect("reopen should be a no-op");

        assert_eq!(reopened.schema_version, 9);
        let ledger_count: u32 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM schema_migration", [], |row| {
                row.get(0)
            })
            .expect("ledger should be queryable");
        assert_eq!(ledger_count, 9, "migration must not reapply on reopen");
    }

    #[test]
    fn opening_index_db_as_a_different_kind_is_rejected() {
        let dir = TestDir::create("kind-mismatch");
        open(&dir.db_path()).expect("index.db should migrate");

        let error = db::open(&dir.db_path(), DbKind::Global, &[])
            .expect_err("opening an index.db as global must be rejected");

        assert!(matches!(
            error,
            DbOpenError::KindMismatch {
                expected: DbKind::Global,
                ..
            }
        ));
    }

    #[test]
    fn foreign_keys_are_enabled_for_index_db() {
        let dir = TestDir::create("foreign-keys");
        let opened = open(&dir.db_path()).expect("index.db should migrate");

        let enabled: i64 = opened
            .connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("pragma should be queryable");
        assert_eq!(enabled, 1);
    }
}
