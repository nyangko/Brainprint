//! Per-Resource structural state, and the one path that establishes it
//! (#16 task 14).
//!
//! Tasks 4 and 6 published a Resource inventory; tasks 7-9 built the
//! parser and the Symbol/Occurrence model; task 13 wired a single saved
//! file to a targeted publication. What was missing is the join: a fresh
//! Workspace had Resources but no structure, and a reconcile recovered
//! Resources without touching structure. This module is that join, and it
//! is used by all three publication paths so none of them grows its own
//! copy of "parse this file and write what came out".
//!
//! ## Why a per-Resource state exists
//!
//! One file that does not parse must not make the Workspace's structural
//! answer "refreshing" forever. So structural currentness is recorded per
//! Resource, in the generic `component_state` table
//! ([`component::STRUCTURAL_INDEX`] at scope
//! [`component::RESOURCE_SCOPE_KIND`] keyed by the stable `ResourceId`) --
//! no new table, no attribute bag.
//!
//! The two shared axes carry what they always carry: `processing_state`
//! is whether structural work is outstanding, `freshness_state` is
//! whether the stored Symbols describe the current revision.
//! `detail_state` carries what only this component can say --
//! [`StructuralState`] -- because PARTIAL and UNSUPPORTED are neither
//! errors nor freshness, and burying them in `last_error_code` would make
//! every reader guess.
//!
//! ## Current versus last-valid
//!
//! A `PARTIAL` result never overwrites the accepted Symbols. What was
//! last published stays exactly where it is, which is the only way to
//! answer "what does this file declare?" while somebody is halfway
//! through typing. But it is *not* promoted to current: those rows keep
//! the `resource_revision` they were extracted from, the Resource row
//! moves on to the new one, and every reader (#16 tasks 10-12) can see
//! the difference. `stable_generation_id` names the generation that
//! published the stored set, so "there is a last-valid structure" is a
//! fact on the row rather than an inference.

use std::{fs, path::Path};

use brainprint_core::ResourceId;
use rusqlite::Connection;

use crate::{
    component::{self, ComponentRow, FreshnessState, ProcessingState},
    extract,
    generation::PublicationGrant,
    identity,
    parser::{self, ParserRegistry, SourceBasis, StructuralCapability},
    resource::{Resource, ResourceKind, ResourceRole},
    scan::ScanError,
    symbol,
};

/// `last_error_code` written when a Resource's structure could not be
/// established because its bytes could not be read.
pub const STRUCTURE_UNREADABLE_CODE: &str = "STRUCTURE_UNREADABLE";

/// What the structural index can say about one Resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuralState {
    /// A whole-file parse succeeded and its Symbols/Occurrences are the
    /// current ones for the Resource's current revision.
    Complete,
    /// The current bytes do not parse cleanly. The Resource itself is
    /// published at its new revision; the stored structure is the last
    /// valid one and is not current.
    Partial,
    /// Only the container's own structure is indexed (a Svelte
    /// component). Zero Symbols is a statement about coverage, never
    /// about the file.
    ContainerOnly,
    /// No Tier-1 grammar covers this Resource. Also never a zero-result
    /// "complete".
    Unsupported,
    /// The Resource's bytes could not be read at all, so nothing is known
    /// about its structure right now.
    Unavailable,
    /// A GENERATED Resource with no mapping back to the source it was
    /// generated from. Its declarations are real, but they are not the
    /// original's, and this tier has nothing to relate them to -- so no
    /// Symbols are published for it rather than Symbols that would read
    /// as hand-written source. Explicit coverage, never a zero result.
    GeneratedUnmapped,
}

impl StructuralState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "COMPLETE",
            Self::Partial => "PARTIAL",
            Self::ContainerOnly => "CONTAINER_ONLY",
            Self::Unsupported => "UNSUPPORTED",
            Self::Unavailable => "UNAVAILABLE",
            Self::GeneratedUnmapped => "GENERATED_UNMAPPED",
        }
    }

    fn parse(raw: &str) -> Result<Self, ScanError> {
        match raw {
            "COMPLETE" => Ok(Self::Complete),
            "PARTIAL" => Ok(Self::Partial),
            "CONTAINER_ONLY" => Ok(Self::ContainerOnly),
            "UNSUPPORTED" => Ok(Self::Unsupported),
            "UNAVAILABLE" => Ok(Self::Unavailable),
            "GENERATED_UNMAPPED" => Ok(Self::GeneratedUnmapped),
            other => Err(ScanError::InvariantViolated {
                detail: format!("unknown structural state {other:?}"),
            }),
        }
    }

    /// Whether the stored Symbol set describes the Resource's current
    /// revision. Only a complete parse does.
    #[must_use]
    pub const fn is_current_structure(self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Whether zero Symbols here may be read as "this file declares
    /// nothing". Only a complete whole-file parse earns that.
    #[must_use]
    pub const fn may_claim_empty(self) -> bool {
        matches!(self, Self::Complete)
    }

    /// The two shared axes this state implies.
    const fn axes(self) -> (ProcessingState, FreshnessState) {
        match self {
            // Nothing outstanding: either the structure is current, or it
            // is as complete as this Resource's coverage allows.
            Self::Complete | Self::ContainerOnly | Self::Unsupported | Self::GeneratedUnmapped => {
                (ProcessingState::Ready, FreshnessState::Current)
            }
            // Something is owed: a clean parse, or a readable file.
            Self::Partial | Self::Unavailable => (ProcessingState::Queued, FreshnessState::Dirty),
        }
    }
}

impl std::fmt::Display for StructuralState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One Resource's structural state as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceStructure {
    pub state: StructuralState,
    /// The Workspace revision this state was established against.
    pub basis_workspace_revision: String,
    /// The generation that published the Symbols/Occurrences currently
    /// stored for this Resource -- the current ones under
    /// [`StructuralState::Complete`], the last valid ones otherwise.
    pub published_generation_id: Option<i64>,
    pub processing_state: ProcessingState,
    pub freshness_state: FreshnessState,
    pub last_error_code: Option<String>,
}

impl ResourceStructure {
    /// Whether a previously published structure is still stored. Under
    /// [`StructuralState::Partial`] this is what makes the difference
    /// between "we know what it used to declare" and "we know nothing".
    #[must_use]
    pub const fn has_last_valid(&self) -> bool {
        self.published_generation_id.is_some()
    }
}

/// What publishing one Resource's structure produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuralOutcome {
    pub state: StructuralState,
    /// Symbols written. Zero whenever nothing was replaced.
    pub symbols: usize,
    pub occurrences: usize,
}

/// One Resource's structural state, or `None` if none was ever recorded.
pub fn read(
    connection: &Connection,
    resource_id: ResourceId,
) -> Result<Option<ResourceStructure>, ScanError> {
    component::read_scoped(
        connection,
        component::STRUCTURAL_INDEX,
        component::RESOURCE_SCOPE_KIND,
        &scope_key(resource_id),
    )?
    .map(decode)
    .transpose()
}

/// Every recorded structural state, keyed by scope key (the `ResourceId`
/// rendered as text). One query for a whole query scope.
pub(crate) fn read_all(
    connection: &Connection,
) -> Result<Vec<(String, ResourceStructure)>, ScanError> {
    component::read_all_scoped(
        connection,
        component::STRUCTURAL_INDEX,
        component::RESOURCE_SCOPE_KIND,
    )?
    .into_iter()
    .map(|(key, row)| Ok((key, decode(row)?)))
    .collect()
}

/// The scope key a `ResourceId` is stored under.
#[must_use]
pub fn scope_key(resource_id: ResourceId) -> String {
    resource_id.to_string()
}

fn decode(row: ComponentRow) -> Result<ResourceStructure, ScanError> {
    let raw = row
        .detail_state
        .ok_or_else(|| ScanError::InvariantViolated {
            detail: "a STRUCTURAL_INDEX row has no detail_state".to_owned(),
        })?;
    Ok(ResourceStructure {
        state: StructuralState::parse(&raw)?,
        basis_workspace_revision: row.basis_workspace_revision,
        published_generation_id: row.stable_generation_id,
        processing_state: row.processing_state,
        freshness_state: row.freshness_state,
        last_error_code: row.last_error_code,
    })
}

/// Record one Resource's structural state.
pub(crate) fn write(
    connection: &Connection,
    resource_id: ResourceId,
    state: StructuralState,
    basis_workspace_revision: &str,
    published_generation_id: Option<i64>,
    last_error_code: Option<&str>,
) -> Result<(), ScanError> {
    let (processing_state, freshness_state) = state.axes();
    component::write_scoped(
        connection,
        component::STRUCTURAL_INDEX,
        component::RESOURCE_SCOPE_KIND,
        &scope_key(resource_id),
        &ComponentRow {
            basis_workspace_revision: basis_workspace_revision.to_owned(),
            stable_generation_id: published_generation_id,
            processing_state,
            freshness_state,
            last_error_code: last_error_code.map(ToOwned::to_owned),
            detail_state: Some(state.as_str().to_owned()),
        },
    )?;
    Ok(())
}

/// Forget a Resource's structure entirely: its Symbols, its Occurrences,
/// and its structural state.
///
/// For a DELETED Resource. Keeping the rows would mean a tombstone still
/// has a structure somewhere, and "last valid" is only meaningful for
/// something that still exists.
pub(crate) fn clear(connection: &Connection, resource_id: ResourceId) -> Result<(), ScanError> {
    let local: Option<i64> = rusqlite::OptionalExtension::optional(connection.query_row(
        "SELECT id FROM resource WHERE uid = ?1",
        rusqlite::params![resource_id.to_bytes().to_vec()],
        |row| row.get(0),
    ))
    .map_err(crate::resource::ResourceError::from)?;
    if let Some(local) = local {
        // Occurrences first: they are evidence about the Symbols, and the
        // foreign key would refuse the other order.
        connection
            .execute(
                "DELETE FROM occurrence WHERE resource_id = ?1",
                rusqlite::params![local],
            )
            .map_err(crate::resource::ResourceError::from)?;
        connection
            .execute(
                "DELETE FROM symbol WHERE resource_id = ?1",
                rusqlite::params![local],
            )
            .map_err(crate::resource::ResourceError::from)?;
    }
    component::delete_scoped(
        connection,
        component::STRUCTURAL_INDEX,
        component::RESOURCE_SCOPE_KIND,
        &scope_key(resource_id),
    )?;
    Ok(())
}

/// Establish one Resource's structure inside an open publication
/// transaction, and record what was established.
///
/// `resource` must be the row as this publication writes it -- its
/// `path_rel` is the path that gets read (so an identity-preserving move
/// is re-parsed at its *new* path) and its `resource_revision` is what the
/// Symbols are extracted against.
///
/// The bytes are read from the current filesystem and checked against the
/// Resource's own `content_hash` before anything is parsed, so a file that
/// moved on mid-publication fails the publication instead of producing
/// structure for bytes nobody verified.
pub(crate) fn publish_resource(
    connection: &Connection,
    grant: &PublicationGrant,
    publication_revision: &str,
    workspace_root: &Path,
    resource: &Resource,
) -> Result<StructuralOutcome, ScanError> {
    let generation_id = grant.generation_id();

    // A generated artifact nothing maps back to its original is not
    // indexed as source: publishing its declarations would put
    // machine-written Symbols in the same list as hand-written ones with
    // nothing to tell them apart (#16 task 14).
    if resource.role == ResourceRole::Generated && resource.container_resource_id.is_none() {
        return record(
            connection,
            resource,
            publication_revision,
            StructuralState::GeneratedUnmapped,
            None,
        );
    }

    // Coverage first, and without reading anything: a dialect the
    // registry does not have, or a container it only knows the outside
    // of, is a statement about capability rather than about the file.
    let dialect = match parser::dialect_for_resource(resource) {
        Ok(dialect) => dialect,
        Err(_) => {
            return record(
                connection,
                resource,
                publication_revision,
                StructuralState::Unsupported,
                None,
            );
        }
    };
    if !matches!(dialect.capability(), StructuralCapability::WholeFile) {
        return record(
            connection,
            resource,
            publication_revision,
            StructuralState::ContainerOnly,
            None,
        );
    }
    if resource.kind != ResourceKind::File {
        return record(
            connection,
            resource,
            publication_revision,
            StructuralState::Unsupported,
            None,
        );
    }

    let previous_generation =
        read(connection, resource.id)?.and_then(|state| state.published_generation_id);

    let path = workspace_root.join(&resource.path_rel);
    let Ok(bytes) = fs::read(&path) else {
        return record_with_error(
            connection,
            resource,
            publication_revision,
            StructuralState::Unavailable,
            previous_generation,
            Some(STRUCTURE_UNREADABLE_CODE),
        );
    };
    // The same verification task 11 makes for a read: the structure is
    // about these exact bytes, and the Resource row says which bytes
    // those are.
    if resource.content_hash.as_deref() != Some(identity::content_hash_of(&bytes).as_str()) {
        return Err(ScanError::InputChanged {
            detail: format!(
                "{} changed while its structure was being published",
                resource.path_rel
            ),
        });
    }

    let tree = ParserRegistry::new()
        .parse(dialect, &bytes, SourceBasis::of(resource))
        .map_err(ScanError::Parse)?;
    let extraction = extract::extract(&tree, &bytes);
    if !extraction.is_accepted() {
        // The previously accepted set is left exactly where it is: it is
        // the last valid answer, and it keeps its own revision so no
        // reader can mistake it for the current one.
        return record_with_error(
            connection,
            resource,
            publication_revision,
            StructuralState::Partial,
            previous_generation,
            None,
        );
    }

    let profile_id = symbol::ensure_profile(connection, &extraction.profile)?;
    let previous = symbol::list_for_resource(connection, resource.id)?;
    let symbols = extract::assign_ids(&previous, &extraction, resource, profile_id);
    let occurrences =
        extract::resolve_occurrences(&extraction, &symbols, resource, profile_id, generation_id);
    symbol::replace_structure_in_publication(
        connection,
        grant,
        publication_revision,
        resource.id,
        &resource.resource_revision,
        &symbols,
        &occurrences,
    )?;
    write(
        connection,
        resource.id,
        StructuralState::Complete,
        publication_revision,
        Some(generation_id),
        None,
    )?;
    Ok(StructuralOutcome {
        state: StructuralState::Complete,
        symbols: symbols.len(),
        occurrences: occurrences.len(),
    })
}

fn record(
    connection: &Connection,
    resource: &Resource,
    publication_revision: &str,
    state: StructuralState,
    published_generation_id: Option<i64>,
) -> Result<StructuralOutcome, ScanError> {
    record_with_error(
        connection,
        resource,
        publication_revision,
        state,
        published_generation_id,
        None,
    )
}

fn record_with_error(
    connection: &Connection,
    resource: &Resource,
    publication_revision: &str,
    state: StructuralState,
    published_generation_id: Option<i64>,
    last_error_code: Option<&str>,
) -> Result<StructuralOutcome, ScanError> {
    write(
        connection,
        resource.id,
        state,
        publication_revision,
        published_generation_id,
        last_error_code,
    )?;
    Ok(StructuralOutcome {
        state,
        symbols: 0,
        occurrences: 0,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        env,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use brainprint_core::SymbolId;

    use super::*;
    use crate::{
        component::FreshnessState,
        config::WorkspaceConfig,
        inspect::{ReadError, SourceReader},
        query::{
            Currentness, QueryIndex, ResourceScope, StructuralCoverage, SymbolQuery, SymbolSelector,
        },
        reconcile::Reconcile,
        resource::{ResourceState, ResourceStore},
        scan::BaselineScan,
        search::{QueryStatus, structured_status},
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

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-structural-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            std::fs::create_dir_all(root.join("src")).expect("src");
            std::fs::create_dir_all(root.join("ui")).expect("ui");
            std::fs::create_dir_all(root.join("docs")).expect("docs");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("lib.py", LIB_PY);
            fixture.write("ui/Widget.svelte", WIDGET_SVELTE);
            fixture.write("docs/readme.md", "# notes\n");
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            std::fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn baseline(&self) {
            let engine = BaselineScan::open(&self.db_path()).expect("index.db");
            engine
                .run_initial_scan(&self.root, &WorkspaceConfig::default(), "workspace-rev-1")
                .expect("baseline scan");
        }

        fn reconcile(&self) {
            let engine = Reconcile::open(&self.db_path()).expect("index.db");
            engine
                .run(&self.root, &WorkspaceConfig::default())
                .expect("reconcile");
        }

        fn ingest(&self, events: &[RawWatchEvent]) {
            let ingest = WatchIngest::open(&self.db_path()).expect("index.db");
            ingest
                .ingest_all(&self.root, &WorkspaceConfig::default(), events)
                .expect("ingestion");
        }

        fn resources(&self) -> ResourceStore {
            ResourceStore::open(&self.db_path()).expect("index.db")
        }

        fn resource(&self, rel: &str) -> Resource {
            self.resources()
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn structure(&self, rel: &str) -> ResourceStructure {
            let store = self.resources();
            read(store.connection(), self.resource(rel).id)
                .expect("structural state")
                .expect("recorded")
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

        fn occurrence_count(&self, rel: &str) -> usize {
            SymbolStore::open(&self.db_path())
                .expect("index.db")
                .list_occurrences_for_resource(self.resource(rel).id)
                .expect("occurrences")
                .len()
        }

        fn query(&self) -> QueryIndex {
            QueryIndex::open(&self.db_path()).expect("index.db")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_fresh_baseline_publishes_structure_for_every_supported_source() {
        let fixture = Fixture::create("baseline");
        fixture.baseline();

        // Supported whole-file source: real Symbols, real Occurrences,
        // and a state that says so.
        let symbols = fixture.symbols("src/app.ts");
        assert!(symbols.contains_key("App"));
        assert!(symbols.contains_key("App.run"));
        assert!(symbols.contains_key("helper"));
        assert!(fixture.occurrence_count("src/app.ts") > 0);
        let structure = fixture.structure("src/app.ts");
        assert_eq!(structure.state, StructuralState::Complete);
        assert_eq!(structure.freshness_state, FreshnessState::Current);
        assert!(structure.has_last_valid());
        assert!(!fixture.symbols("lib.py").is_empty());

        // Evidence belongs to the generation that published it, which is
        // the one that is now stable -- never a BUILDING one.
        let stable = crate::generation::current_stable(fixture.resources().connection())
            .expect("stable")
            .expect("published")
            .id;
        assert_eq!(structure.published_generation_id, Some(stable));

        // Coverage that is not complete says so instead of pretending.
        assert_eq!(
            fixture.structure("ui/Widget.svelte").state,
            StructuralState::ContainerOnly
        );
        assert!(fixture.symbols("ui/Widget.svelte").is_empty());
        assert_eq!(
            fixture.structure("docs/readme.md").state,
            StructuralState::Unsupported
        );

        // And none of it mirrored the source.
        let database = std::fs::read(fixture.db_path()).expect("index.db");
        for body in ["return helper()", "return 41", "    return 4"] {
            assert!(
                !database
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db must not mirror source text ({body:?})"
            );
        }
    }

    #[test]
    fn reconcile_reanalyzes_the_changed_resources_and_only_those() {
        let fixture = Fixture::create("reconcile-changed");
        fixture.baseline();
        let app_before = fixture.symbols("src/app.ts");

        // A sentinel in an *unchanged* Resource's structure. If reconcile
        // re-parsed it, this would be replaced by the real Symbol set.
        let untouched = fixture.resource("lib.py");
        {
            let store = SymbolStore::open(&fixture.db_path()).expect("index.db");
            let mut sentinel = store
                .list_for_resource(untouched.id)
                .expect("symbols")
                .remove(0);
            sentinel.name = "sentinel".to_owned();
            sentinel.qualified_name = "sentinel".to_owned();
            store
                .replace_for_resource(untouched.id, &untouched.resource_revision, &[sentinel])
                .expect("replace");
        }

        fixture.write(
            "src/app.ts",
            "export class App {\n  run(): number {\n    return 1\n  }\n}\n",
        );
        fixture.write(
            "src/created.ts",
            "export function created(): number {\n  return 7\n}\n",
        );
        std::fs::remove_file(fixture.path("docs/readme.md")).expect("remove");
        fixture.reconcile();

        // MODIFY: continuity kept, the removed declaration gone.
        let app_after = fixture.symbols("src/app.ts");
        assert_eq!(app_after.get("App.run"), app_before.get("App.run"));
        assert!(!app_after.contains_key("helper"));
        assert_eq!(
            fixture.structure("src/app.ts").state,
            StructuralState::Complete
        );

        // CREATE: a structure where there was none.
        assert!(fixture.symbols("src/created.ts").contains_key("created"));
        assert_eq!(
            fixture.structure("src/created.ts").state,
            StructuralState::Complete
        );

        // UNCHANGED: not re-analyzed, so the sentinel survives.
        assert_eq!(
            fixture.symbols("lib.py").keys().collect::<Vec<_>>(),
            vec!["sentinel"],
            "a bulk reconcile is not a reason to re-parse a file nothing happened to"
        );

        // DELETE: the tombstone has no structure at all.
        let readme = fixture
            .resources()
            .list()
            .expect("list")
            .into_iter()
            .find(|resource| resource.path_rel.contains("readme.md"))
            .expect("the tombstone row");
        assert_eq!(readme.state, ResourceState::Deleted);
        assert!(
            read(fixture.resources().connection(), readme.id)
                .expect("structural state")
                .is_none(),
            "a deleted Resource has no structural state to be current"
        );
    }

    #[test]
    fn an_identity_preserving_move_is_reparsed_at_its_new_path() {
        let fixture = Fixture::create("moved");
        fixture.baseline();
        let before = fixture.resource("src/app.ts");

        std::fs::rename(fixture.path("src/app.ts"), fixture.path("src/renamed.ts"))
            .expect("rename");
        std::fs::write(
            fixture.path("src/renamed.ts"),
            "export class App {\n  moved(): number {\n    return 3\n  }\n}\n",
        )
        .expect("write");
        fixture.ingest(&[RawWatchEvent::RenamedPair {
            from: fixture.path("src/app.ts"),
            to: fixture.path("src/renamed.ts"),
        }]);
        fixture.reconcile();

        let after = fixture.resource("src/renamed.ts");
        assert_eq!(after.id, before.id, "the identity survived the move");
        let symbols = fixture.symbols("src/renamed.ts");
        assert!(
            symbols.contains_key("App.moved"),
            "the structure came from the new path's current bytes"
        );
        assert!(!symbols.contains_key("App.run"));
        assert_eq!(
            fixture.structure("src/renamed.ts").state,
            StructuralState::Complete
        );
    }

    #[test]
    fn a_syntax_error_is_partial_rather_than_a_workspace_wide_refresh() {
        let fixture = Fixture::create("syntax-error");
        fixture.baseline();
        let before = fixture.symbols("src/app.ts");

        fixture.write("src/app.ts", "export class App {\n  run(): number {\n");
        fixture.reconcile();

        // The Resource itself is published from the current filesystem.
        let resource = fixture.resource("src/app.ts");
        assert_eq!(resource.resource_revision, "2");
        let structure = fixture.structure("src/app.ts");
        assert_eq!(structure.state, StructuralState::Partial);
        assert!(structure.has_last_valid());

        // The last valid structure is still there, at its own revision.
        assert_eq!(fixture.symbols("src/app.ts"), before);
        let store = SymbolStore::open(&fixture.db_path()).expect("index.db");
        assert!(
            store
                .list_for_resource(resource.id)
                .expect("symbols")
                .iter()
                .all(|symbol| symbol.resource_revision == "1")
        );

        // One unparseable file does not make the whole Workspace
        // refreshing: the Resource index is current, and a query about an
        // unaffected file gets a straight answer.
        let index = fixture.query();
        assert!(index.currentness().expect("currentness").is_current());
        let elsewhere = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::PathPrefix("lib.py")),
                ..SymbolQuery::new(SymbolSelector::Name("run"))
            })
            .expect("search");
        assert_eq!(structured_status(&elsewhere), QueryStatus::Found);
    }

    #[test]
    fn last_valid_symbols_are_labelled_evidence_and_never_current_candidates() {
        let fixture = Fixture::create("last-valid");
        fixture.baseline();
        fixture.write("src/app.ts", "export class App {\n  run(): number {\n");
        fixture.reconcile();

        let index = fixture.query();
        let located = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::Id(fixture.resource("src/app.ts").id)),
                ..SymbolQuery::new(SymbolSelector::Name("run"))
            })
            .expect("search");

        assert!(
            located.candidates.is_empty(),
            "a last-valid Symbol is not a current locator"
        );
        assert_eq!(
            located.last_valid.len(),
            1,
            "but it is kept, in its own list"
        );
        assert_eq!(located.last_valid[0].coverage, StructuralCoverage::Partial);
        assert_eq!(
            structured_status(&located),
            QueryStatus::Refreshing,
            "zero current candidates over a PARTIAL Resource is not a 'no'"
        );
        assert!(!structured_status(&located).is_negative_answer());

        // Its span must not be sliced out of the current file, and the
        // metadata still says the last-valid structure exists.
        let reader = SourceReader::open(&fixture.db_path(), &fixture.root).expect("reader");
        let stale = located.last_valid[0].symbol.id;
        assert!(matches!(
            reader
                .inspect_symbol(stale)
                .expect_err("a last-valid span is not current source"),
            ReadError::SymbolNotCurrent { .. }
        ));
        let metadata = reader.inspect_symbol_metadata(stale).expect("metadata");
        assert!(!metadata.is_current);
        assert_eq!(metadata.structural_state, Some(StructuralState::Partial));
    }

    #[test]
    fn coverage_decides_whether_zero_symbols_may_be_read_as_nothing() {
        let fixture = Fixture::create("coverage");
        fixture.baseline();
        let index = fixture.query();

        // COMPLETE and current: zero really is zero.
        let complete = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::Id(fixture.resource("src/app.ts").id)),
                ..SymbolQuery::new(SymbolSelector::Name("nothing_declared_here"))
            })
            .expect("search");
        assert_eq!(structured_status(&complete), QueryStatus::NotFound);

        // CONTAINER_ONLY: zero is a statement about coverage.
        let container = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::Id(fixture.resource("ui/Widget.svelte").id)),
                ..SymbolQuery::new(SymbolSelector::Name("mount"))
            })
            .expect("search");
        assert_eq!(structured_status(&container), QueryStatus::Unsupported);
        assert_eq!(
            container.incomplete_coverage[0].coverage,
            StructuralCoverage::ContainerOnly
        );
        assert!(!structured_status(&container).is_negative_answer());
    }

    #[test]
    fn a_generated_resource_with_no_original_mapping_publishes_no_source_symbols() {
        let fixture = Fixture::create("generated");
        fixture.baseline();
        let store = fixture.resources();

        // Discovery does not classify anything as GENERATED yet, so the
        // rule is exercised where it lives: a Resource that *is* marked
        // generated, with nothing mapping it back to an original.
        let mut resource = fixture.resource("src/app.ts");
        resource.role = crate::resource::ResourceRole::Generated;
        assert!(resource.container_resource_id.is_none());
        store.update_resource(&resource).expect("update");

        let revision = crate::generation::current_workspace_revision(store.connection())
            .expect("clock")
            .expect("bootstrapped");
        let building =
            crate::generation::begin_generation(store.connection(), &revision).expect("begin");
        let transaction = store.transaction().expect("transaction");
        let (record, grant) =
            crate::generation::grant_publication(&transaction, building.id).expect("grant");
        let outcome = publish_resource(&transaction, &grant, &revision, &fixture.root, &resource)
            .expect("publish");
        crate::generation::finish_publish_stable(&transaction, &record).expect("publish stable");
        transaction.commit().expect("commit");

        assert_eq!(outcome.state, StructuralState::GeneratedUnmapped);
        assert_eq!(outcome.symbols, 0, "no machine-written Symbol is published");
        assert_eq!(outcome.occurrences, 0);
        let structure = fixture.structure("src/app.ts");
        assert_eq!(structure.state, StructuralState::GeneratedUnmapped);
        assert_eq!(
            fixture
                .query()
                .search_symbols(&SymbolQuery {
                    scope: Some(ResourceScope::Id(resource.id)),
                    ..SymbolQuery::new(SymbolSelector::Name("nothing_here"))
                })
                .expect("search")
                .incomplete_coverage[0]
                .coverage,
            StructuralCoverage::GeneratedUnmapped,
            "and zero results there is explained rather than claimed"
        );
    }

    #[test]
    fn a_deleted_resource_keeps_no_structure_and_answers_nothing() {
        let fixture = Fixture::create("deleted");
        fixture.baseline();
        let deleted = fixture.resource("src/app.ts").id;
        std::fs::remove_file(fixture.path("src/app.ts")).expect("remove");
        fixture.reconcile();

        let index = fixture.query();
        let located = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::PathPrefix("src/")),
                ..SymbolQuery::new(SymbolSelector::QualifiedName("App.run"))
            })
            .expect("search");
        assert!(located.candidates.is_empty());
        assert!(
            located.last_valid.is_empty(),
            "a tombstone's structure is not last-valid evidence either"
        );
        assert_eq!(
            structured_status(&located),
            QueryStatus::NotFound,
            "the scope that held it is fully covered and current, so this really is a no"
        );
        assert!(
            read(fixture.resources().connection(), deleted)
                .expect("structural state")
                .is_none()
        );
        assert_eq!(
            index.currentness().expect("currentness"),
            Currentness::Current
        );
    }
}
