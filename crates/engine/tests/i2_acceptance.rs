//! I2 (#16) end-to-end acceptance tests.
//!
//! This is acceptance *infrastructure* (#16 task 15), not new product
//! behavior. Unlike I1, whose surface was the `brainprint`/`brainprintd`
//! binaries, I2's deliverable is the engine itself -- the MCP/daemon
//! surface belongs to I5 -- so the whole stack under test here is the
//! public engine API driven against a real filesystem, from outside the
//! crate. The per-module unit tests from tasks 1-14 are not re-listed;
//! this file exists to prove the scenarios #16's completion criteria
//! name actually hold together end to end.
//!
//! Every scenario below answers from `index.db` plus verified current
//! bytes. Nothing here shells out to `ls`, `find`, `rg`, `cat` or `sed`,
//! and nothing may: the point of I2 is that those loops are gone.

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
};

use brainprint_core::{ResourceId, SymbolId};
use brainprint_engine::{
    component::FreshnessState,
    config::WorkspaceConfig,
    generation::GenerationState,
    inspect::{ReadError, SourceReader},
    query::{
        Currentness, FileQuery, QueryIndex, ResourceLocator, ResourceScope, StructuralCoverage,
        SymbolQuery, SymbolSelector,
    },
    reconcile::Reconcile,
    refresh::TargetedRefresh,
    resource::{ResourceState, ResourceStore},
    scan::BaselineScan,
    search::{
        FallbackReason, QueryStatus, TextPattern, TextSearch, TextSearcher, structured_status,
    },
    structural::{self, StructuralState},
    symbol::SymbolStore,
    watch::{RawWatchEvent, WatchIngest},
};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

const APP_TS: &str = "\
export class App {
  run(): number {
    return helper()
  }
}

function helper(): number {
  return 41
}
";

const LIB_PY: &str = "\
def run():
    return 4
";

const WIDGET_SVELTE: &str = "\
<script lang=\"ts\">
  export function mount() {}
</script>
<div>hi</div>
";

/// One Workspace with its `index.db` outside it, driven through the
/// public engine API only.
struct Workspace {
    base: PathBuf,
    root: PathBuf,
}

impl Workspace {
    fn create(label: &str) -> Self {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let base = env::temp_dir().join(format!(
            "brainprint-i2-{label}-{}-{sequence}",
            process::id()
        ));
        let root = base.join("workspace");
        fs::create_dir_all(root.join("src")).expect("src");
        fs::create_dir_all(root.join("ui")).expect("ui");
        fs::create_dir_all(root.join("docs")).expect("docs");
        let workspace = Self { base, root };
        workspace.write("src/app.ts", APP_TS);
        workspace.write("lib.py", LIB_PY);
        workspace.write("ui/Widget.svelte", WIDGET_SVELTE);
        workspace.write("docs/readme.md", "# notes\n");
        workspace
    }

    fn db_path(&self) -> PathBuf {
        self.base.join("data").join("index.db")
    }

    fn write(&self, rel: &str, contents: &str) {
        fs::write(self.root.join(rel), contents).expect("fixture file");
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn config(&self) -> WorkspaceConfig {
        WorkspaceConfig::default()
    }

    /// The initial structural scan: Resources *and* structure, in one
    /// published generation (#16 task 4 + 14).
    fn initial_scan(&self) {
        BaselineScan::open(&self.db_path())
            .expect("index.db")
            .run_initial_scan(&self.root, &self.config(), "workspace-rev-1")
            .expect("initial scan");
    }

    fn save(&self, rel: &str, contents: &str) {
        self.write(rel, contents);
        WatchIngest::open(&self.db_path())
            .expect("index.db")
            .ingest_all(
                &self.root,
                &self.config(),
                &[RawWatchEvent::Modified {
                    path: self.path(rel),
                }],
            )
            .expect("watcher ingestion");
    }

    fn refresh(&self) -> brainprint_engine::refresh::RefreshOutcome {
        TargetedRefresh::open(&self.db_path())
            .expect("index.db")
            .run(&self.root, &self.config())
            .expect("targeted refresh")
    }

    fn reconcile(&self) -> brainprint_engine::reconcile::ReconcileReport {
        Reconcile::open(&self.db_path())
            .expect("index.db")
            .run(&self.root, &self.config())
            .expect("reconcile")
    }

    fn index(&self) -> QueryIndex {
        QueryIndex::open(&self.db_path()).expect("index.db")
    }

    fn reader(&self) -> SourceReader {
        SourceReader::open(&self.db_path(), &self.root).expect("index.db")
    }

    fn resources(&self) -> ResourceStore {
        ResourceStore::open(&self.db_path()).expect("index.db")
    }

    fn resource(&self, rel: &str) -> brainprint_engine::resource::Resource {
        self.resources()
            .get_active_by_path_key(rel)
            .expect("lookup")
            .expect("an ACTIVE Resource at this path")
    }

    /// A raw connection to this Workspace's `index.db`, for the handful
    /// of assertions that read the database directly.
    fn store(&self) -> SymbolStore {
        SymbolStore::open(&self.db_path()).expect("index.db")
    }

    fn structure(&self, rel: &str) -> StructuralState {
        let store = self.store();
        structural::read(store.connection(), self.resource(rel).id)
            .expect("structural state")
            .expect("recorded")
            .state
    }

    fn symbols(&self, rel: &str) -> BTreeMap<String, SymbolId> {
        SymbolStore::open(&self.db_path())
            .expect("index.db")
            .list_for_resource(self.resource(rel).id)
            .expect("symbols")
            .into_iter()
            .map(|symbol| (symbol.qualified_name, symbol.id))
            .collect()
    }

    fn stable_generation(&self) -> i64 {
        Reconcile::open(&self.db_path())
            .expect("index.db")
            .current_stable()
            .expect("stable generation")
            .expect("one has been published")
            .id
    }

    fn resource_index_freshness(&self) -> FreshnessState {
        Reconcile::open(&self.db_path())
            .expect("index.db")
            .resource_index_state()
            .expect("component state")
            .expect("published")
            .freshness_state
    }

    /// The single current Symbol with this qualified name, or `None`.
    fn locate(&self, qualified_name: &str) -> Option<(SymbolId, ResourceId)> {
        let located = self
            .index()
            .search_symbols(&SymbolQuery::new(SymbolSelector::QualifiedName(
                qualified_name,
            )))
            .expect("symbol search");
        located
            .exact()
            .map(|candidate| (candidate.symbol.id, candidate.symbol.resource_id))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn generation_states(workspace: &Workspace) -> Vec<GenerationState> {
    let store = workspace.store();
    let mut statement = store
        .connection()
        .prepare("SELECT state FROM generation ORDER BY id")
        .expect("prepare");
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query");
    rows.map(|row| match row.expect("row").as_str() {
        "BUILDING" => GenerationState::Building,
        "STABLE" => GenerationState::Stable,
        "ABORTED" => GenerationState::Aborted,
        other => panic!("unexpected generation state {other}"),
    })
    .collect()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// Fresh Workspace → initial structural scan → files/tree → symbol
/// locate → inspect definition source, with no shell exploration
/// anywhere in the flow.
#[test]
fn fresh_workspace_answers_tree_locate_and_inspect_from_the_index() {
    let workspace = Workspace::create("fresh");
    workspace.initial_scan();

    // files/tree, from the Resource inventory.
    let listing = workspace
        .index()
        .list_files(&FileQuery {
            directory: Some(""),
            ..FileQuery::default()
        })
        .expect("list files");
    let names: Vec<&str> = listing
        .entries
        .iter()
        .map(|entry| entry.path_rel.as_str())
        .collect();
    assert_eq!(names, vec!["docs", "lib.py", "src", "ui"]);
    assert!(listing.currentness.is_current());

    // Symbol locate, from the structural index.
    let (symbol_id, resource_id) = workspace.locate("App.run").expect("App.run is locatable");

    // Inspect: one call, locator *and* current definition source.
    let inspection = workspace
        .reader()
        .inspect_symbol(symbol_id)
        .expect("inspect");
    assert_eq!(inspection.path_rel, "src/app.ts");
    assert_eq!(inspection.symbol.resource_id, resource_id);
    assert!(inspection.symbol.signature.is_some());
    assert_eq!(
        inspection.source, "run(): number {\n    return helper()\n  }",
        "the definition's current source arrives with its locator"
    );
    assert!(inspection.verification.currentness.is_current());

    // The structure is real, published, and attributed to the generation
    // that is stable now -- never to a BUILDING one.
    let stable = workspace.stable_generation();
    let occurrences = SymbolStore::open(&workspace.db_path())
        .expect("index.db")
        .list_occurrences_for_resource(resource_id)
        .expect("occurrences");
    assert!(!occurrences.is_empty());
    assert!(
        occurrences
            .iter()
            .all(|occurrence| occurrence.generation_id == stable)
    );
    assert_eq!(workspace.structure("src/app.ts"), StructuralState::Complete);
    assert!(
        generation_states(&workspace)
            .iter()
            .all(|state| *state != GenerationState::Building)
    );

    // And no source body was mirrored into the database.
    let database = fs::read(workspace.db_path()).expect("index.db bytes");
    for body in ["return helper()", "return 41", "    return 4"] {
        assert!(
            !contains(&database, body),
            "index.db must not mirror source text ({body:?})"
        );
    }
}

/// Single-file save → watcher DIRTY → targeted refresh → new STABLE
/// generation → locate/inspect immediately return the new span/source,
/// with no full Workspace rescan and no other Resource touched.
#[test]
fn a_single_save_is_recovered_without_rescanning_the_workspace() {
    let workspace = Workspace::create("save");
    workspace.initial_scan();
    let before_run = workspace.locate("App.run").expect("App.run").0;
    let before_resource = workspace.resource("src/app.ts");
    let before_lib = workspace.symbols("lib.py");

    workspace.save(
        "src/app.ts",
        "export class App {\n  // a comment\n  run(): number {\n    return helper()\n  }\n}\n\n\
         function helper(): number {\n  return 42\n}\n",
    );
    assert_eq!(
        workspace.resource_index_freshness(),
        FreshnessState::Dirty,
        "the watcher marks the index dirty as its fast path"
    );

    // The proof that no full rescan happens: every other file is gone by
    // the time the refresh runs. A Workspace-wide pass would see two
    // deletions; the targeted path never looks.
    fs::remove_file(workspace.path("ui/Widget.svelte")).expect("remove");
    fs::remove_file(workspace.path("docs/readme.md")).expect("remove");

    let outcome = workspace.refresh();
    let published = outcome.published().expect("a targeted publication");
    assert_eq!(published.path_rel, "src/app.ts");
    assert_eq!(published.structure.state, StructuralState::Complete);

    // Resource and Symbol continuity.
    let after_resource = workspace.resource("src/app.ts");
    assert_eq!(after_resource.id, before_resource.id);
    assert_ne!(
        after_resource.resource_revision,
        before_resource.resource_revision
    );
    let after_run = workspace.locate("App.run").expect("App.run").0;
    assert_eq!(after_run, before_run, "the same declaration, the same id");
    assert_eq!(
        workspace.symbols("lib.py"),
        before_lib,
        "no other Resource was parsed, hashed, or replaced"
    );

    // A new STABLE generation, and the new source straight away.
    assert_eq!(published.generation.state, GenerationState::Stable);
    assert_eq!(workspace.stable_generation(), published.generation.id);
    assert_eq!(
        workspace.resource_index_freshness(),
        FreshnessState::Current
    );
    let inspection = workspace
        .reader()
        .inspect_symbol(after_run)
        .expect("inspect");
    assert_eq!(inspection.symbol.span.start.line, 2, "the span moved");
    assert_eq!(
        inspection.source,
        "run(): number {\n    return helper()\n  }"
    );
}

/// A syntax error advances the Resource but not the structure: PARTIAL
/// plus last-valid, no false NOT_FOUND, and every other Resource still
/// answers normally.
#[test]
fn a_syntax_error_degrades_to_partial_without_a_false_not_found() {
    let workspace = Workspace::create("syntax-error");
    workspace.initial_scan();
    let before_symbols = workspace.symbols("src/app.ts");

    workspace.save("src/app.ts", "export class App {\n  run(): number {\n");
    let published = workspace.refresh();
    let publication = published.published().expect("the Resource change is real");
    assert_eq!(publication.structure.state, StructuralState::Partial);

    let resource = workspace.resource("src/app.ts");
    assert_eq!(
        resource.resource_revision, "2",
        "the current Resource revision moves on with the filesystem"
    );
    assert_eq!(
        workspace.symbols("src/app.ts"),
        before_symbols,
        "the last valid structure is kept, not blanked"
    );
    assert_eq!(workspace.structure("src/app.ts"), StructuralState::Partial);

    // The query layer refuses to call that a "no".
    let index = workspace.index();
    let located = index
        .search_symbols(&SymbolQuery {
            scope: Some(ResourceScope::Id(resource.id)),
            ..SymbolQuery::new(SymbolSelector::Name("run"))
        })
        .expect("search");
    assert!(located.candidates.is_empty());
    assert_eq!(located.last_valid.len(), 1);
    let status = structured_status(&located);
    assert_eq!(status, QueryStatus::Refreshing);
    assert!(!status.is_negative_answer());

    // A last-valid span is never sliced out of the current file, but its
    // metadata is still answerable.
    let reader = workspace.reader();
    let stale = located.last_valid[0].symbol.id;
    assert!(matches!(
        reader.inspect_symbol(stale).expect_err("not current"),
        ReadError::SymbolNotCurrent { .. }
    ));
    assert!(
        !reader
            .inspect_symbol_metadata(stale)
            .expect("metadata")
            .is_current
    );

    // One broken file does not take the Workspace with it.
    assert_eq!(
        workspace.resource_index_freshness(),
        FreshnessState::Current
    );
    let elsewhere = index
        .search_symbols(&SymbolQuery {
            scope: Some(ResourceScope::PathPrefix("lib.py")),
            ..SymbolQuery::new(SymbolSelector::Name("run"))
        })
        .expect("search");
    assert_eq!(structured_status(&elsewhere), QueryStatus::Found);
}

/// A lost event stream, a bulk change behind it, and a reconcile that
/// recovers create/modify/delete/move correctness -- with no stale path
/// or stale Symbol left current.
#[test]
fn reconcile_recovers_correctness_after_the_watcher_loses_events() {
    let workspace = Workspace::create("recovery");
    workspace.initial_scan();
    let moved_id = workspace.resource("lib.py").id;
    let before_app = workspace.symbols("src/app.ts");

    // Changes the watcher never reports, plus an explicit continuity
    // loss: a create, a modify, a delete, and an evidenced move.
    workspace.write(
        "src/created.ts",
        "export function created(): number {\n  return 7\n}\n",
    );
    workspace.write(
        "src/app.ts",
        "export class App {\n  run(): number {\n    return 1\n  }\n}\n",
    );
    fs::remove_file(workspace.path("docs/readme.md")).expect("remove");
    fs::rename(workspace.path("lib.py"), workspace.path("src/lib.py")).expect("rename");
    WatchIngest::open(&workspace.db_path())
        .expect("index.db")
        .ingest_all(
            &workspace.root,
            &workspace.config(),
            &[
                RawWatchEvent::RenamedPair {
                    from: workspace.path("lib.py"),
                    to: workspace.path("src/lib.py"),
                },
                RawWatchEvent::ContinuityLost {
                    detail: "queue overflow".to_owned(),
                },
            ],
        )
        .expect("ingestion");
    assert_eq!(workspace.resource_index_freshness(), FreshnessState::Dirty);

    // The targeted fast path refuses this shape outright.
    assert!(
        workspace.refresh().published().is_none(),
        "a continuity loss is reconcile's problem, not the fast path's"
    );

    let report = workspace.reconcile();
    assert!(report.published(), "a real change set was published");
    assert_eq!(
        workspace.resource_index_freshness(),
        FreshnessState::Current
    );

    // CREATE.
    assert!(workspace.symbols("src/created.ts").contains_key("created"));
    // MODIFY, with continuity.
    let after_app = workspace.symbols("src/app.ts");
    assert_eq!(after_app.get("App.run"), before_app.get("App.run"));
    assert!(!after_app.contains_key("helper"));
    // MOVE: same identity, new path, structure re-parsed there.
    let moved = workspace.resource("src/lib.py");
    assert_eq!(moved.id, moved_id);
    assert_eq!(workspace.structure("src/lib.py"), StructuralState::Complete);
    assert!(
        workspace
            .index()
            .locate_resource(ResourceLocator::Path("lib.py"))
            .expect("locate")
            .candidates
            .is_empty(),
        "the old path is not a current locator any more"
    );
    // DELETE: the tombstone answers nothing, and keeps no structure.
    let readme = workspace
        .resources()
        .list()
        .expect("list")
        .into_iter()
        .find(|resource| resource.path_rel.contains("readme.md"))
        .expect("the tombstone row");
    assert_eq!(readme.state, ResourceState::Deleted);
    assert!(
        structural::read(workspace.store().connection(), readme.id)
            .expect("structural state")
            .is_none()
    );

    // No stale Symbol is presented as current anywhere.
    let all = workspace
        .index()
        .search_symbols(&SymbolQuery::new(SymbolSelector::PartialName("")))
        .expect("search");
    assert!(
        all.last_valid.is_empty(),
        "everything current parses cleanly after recovery"
    );
    for candidate in &all.candidates {
        assert!(!candidate.path_rel.starts_with("/tombstone/"));
    }
}

/// Unsupported, container-only, and generated-unmapped coverage are all
/// stated, never passed off as a complete zero result.
#[test]
fn incomplete_coverage_is_never_a_complete_zero_result() {
    let workspace = Workspace::create("coverage");
    workspace.initial_scan();
    let index = workspace.index();

    // Container-only (Svelte).
    let container = index
        .search_symbols(&SymbolQuery {
            scope: Some(ResourceScope::Id(workspace.resource("ui/Widget.svelte").id)),
            ..SymbolQuery::new(SymbolSelector::Name("mount"))
        })
        .expect("search");
    assert!(container.candidates.is_empty());
    assert_eq!(structured_status(&container), QueryStatus::Unsupported);
    assert_eq!(
        container.incomplete_coverage[0].coverage,
        StructuralCoverage::ContainerOnly
    );
    assert_eq!(
        workspace.structure("ui/Widget.svelte"),
        StructuralState::ContainerOnly
    );

    // Unsupported (Markdown is not code, and says so).
    assert_eq!(
        workspace.structure("docs/readme.md"),
        StructuralState::Unsupported
    );

    // Generated with no mapping back to an original. Discovery does not
    // classify anything as GENERATED yet (#16 tasks 1-2 assign the role
    // vocabulary but no rule produces it), so the publication rule for
    // it is covered by the engine's own `structural` tests, which drive
    // `publish_resource` directly. What is observable from out here is
    // the guard in front of it: a Resource whose classification no
    // longer matches what is on disk is never republished as if it did.
    let store = workspace.resources();
    let mut generated = workspace.resource("lib.py");
    generated.role = brainprint_engine::resource::ResourceRole::Generated;
    store.update_resource(&generated).expect("update");
    workspace.save("lib.py", "def run():\n    return 5\n");
    assert!(
        workspace.refresh().published().is_none(),
        "a changed classification defers instead of publishing a guess"
    );
}

/// Two Workspaces of the same Project share nothing: not Resource ids,
/// not revisions, not generations, not structural results.
#[test]
fn separate_worktrees_do_not_share_resources_revisions_or_generations() {
    let first = Workspace::create("worktree-a");
    let second = Workspace::create("worktree-b");
    first.initial_scan();
    second.initial_scan();

    // Identical content at identical paths, and still distinct identity.
    let first_app = first.resource("src/app.ts");
    let second_app = second.resource("src/app.ts");
    assert_eq!(first_app.path_rel, second_app.path_rel);
    assert_eq!(first_app.content_hash, second_app.content_hash);
    assert_ne!(
        first_app.id, second_app.id,
        "a stable id belongs to one Workspace's inventory"
    );
    assert_ne!(first.locate("App.run"), second.locate("App.run"));

    // A change in one moves nothing in the other.
    let second_before_revision = second_app.resource_revision.clone();
    let second_before_generation = second.stable_generation();
    let second_before_symbols = second.symbols("src/app.ts");
    first.save(
        "src/app.ts",
        "export class App {\n  run(): number {\n    return 99\n  }\n}\n",
    );
    assert!(first.refresh().published().is_some());

    assert_eq!(
        second.resource("src/app.ts").resource_revision,
        second_before_revision
    );
    assert_eq!(second.stable_generation(), second_before_generation);
    assert_eq!(second.symbols("src/app.ts"), second_before_symbols);
    assert_eq!(
        second
            .reader()
            .inspect_symbol(second.locate("App.run").expect("App.run").0)
            .expect("inspect")
            .source,
        "run(): number {\n    return helper()\n  }",
        "the untouched Workspace still describes its own current source"
    );
    assert_eq!(second.resource_index_freshness(), FreshnessState::Current);
    assert_eq!(
        second.index().currentness().expect("currentness"),
        Currentness::Current
    );
}

/// The representative I0 scenario, run the way I2 owns it: the
/// definition is located and read structurally, and the remaining
/// evidence comes from one bounded text fallback -- with no repeated
/// broad search and no duplicate read of the definition file.
#[test]
fn the_representative_scenario_needs_no_repeated_broad_search() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/workspaces/python-signature-impact");
    let workspace = Workspace::create("scenario");
    // The same snapshot the I0 baseline used, copied into a scratch
    // Workspace so the run may write its own index.db.
    fs::remove_dir_all(&workspace.root).expect("clear scratch workspace");
    copy_tree(&fixture, &workspace.root);
    workspace.initial_scan();

    // 1. Structured locate: no text search at all.
    let located = workspace
        .index()
        .search_symbols(&SymbolQuery::new(SymbolSelector::Name("build_profile")))
        .expect("search");
    assert_eq!(structured_status(&located), QueryStatus::Found);
    let definition = located
        .candidates
        .iter()
        .find(|candidate| candidate.path_rel == "src/profile_app/profile.py")
        .expect("the definition is in the structural index");

    // 2. Inspect: the definition's current source in the same packet.
    let inspection = workspace
        .reader()
        .inspect_symbol(definition.symbol.id)
        .expect("inspect");
    assert!(inspection.source.starts_with("def build_profile("));
    assert!(
        inspection.source.contains("return {\"user_id\": user_id"),
        "the body is included, so nothing has to read the file again"
    );

    // 3. One bounded text fallback for the evidence I2 does not model:
    //    call sites and tests are textual evidence here, and I3 is what
    //    turns them into Relations.
    let index = workspace.index();
    let config = workspace.config();
    let searcher = TextSearcher::new(&workspace.root, &config, &index);
    let hits = searcher
        .search(&TextSearch {
            reason: FallbackReason::NonStructuralTarget,
            ..TextSearch::explicit(TextPattern::Literal("build_profile"))
        })
        .expect("text fallback");
    assert_eq!(hits.status, QueryStatus::Found);
    let files: Vec<&str> = {
        let mut files: Vec<&str> = hits
            .matches
            .iter()
            .map(|hit| hit.path_rel.as_str())
            .collect();
        files.dedup();
        files
    };
    for expected in [
        "src/profile_app/admin.py",
        "src/profile_app/profile.py",
        "src/profile_app/service.py",
        "tests/test_profile.py",
    ] {
        assert!(files.contains(&expected), "missing evidence in {expected}");
    }
    assert!(
        hits.scope.is_complete(),
        "the whole scope was searched once"
    );
    // Textual evidence stays textual: no Relation row is invented.
    let store = workspace.store();
    for table in ["relation", "unresolved_reference", "graph_entity"] {
        let count: i64 = store
            .connection()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(count, 0, "{table} belongs to I3, not to this tier");
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create directory");
    for entry in fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}
