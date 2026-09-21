//! Verified current-source reads for a known Resource span or Symbol
//! (#16 task 11 / #4 freshness-revision / #5 inspect contract).
//!
//! What this removes is the second round-trip: task 10 can already say
//! *where* a declaration is, and an agent handed a path and a line range
//! still has to go and read the file. [`SourceReader::inspect_symbol`]
//! returns the Symbol's metadata *and* the current source of its
//! definition span in one call, and [`SourceReader::read_range`] does the
//! same for an already-known span.
//!
//! ## Where source comes from, and why the span is not trusted
//!
//! `index.db` owns Resource and Symbol identity; it does not mirror source
//! text, and this module does not start. The bytes come from the current
//! filesystem. But a persisted span describes the Resource *as indexed*,
//! so slicing it out of whatever the file happens to contain now would
//! hand back convincing, wrong source. Every read therefore runs:
//!
//! ```text
//! read bytes → hash those same bytes → compare to the persisted
//! content_hash → slice those same bytes
//! ```
//!
//! One buffer throughout: what was verified is what is sliced, so no
//! write can land between the check and the read. mtime never decides
//! compatibility -- an editor that rewrites a file to the same length
//! within the same timestamp tick is exactly the case a content hash is
//! for.
//!
//! ## When the file has moved on
//!
//! A hash mismatch is [`ReadError::SourceChanged`], never a best-effort
//! slice. It also marks `RESOURCE_INDEX` DIRTY through the ordinary
//! component API, so the next structured query stops describing itself as
//! current; the raw watcher journal is not touched and no Workspace
//! revision is invented. Re-indexing the Resource is task 13's targeted
//! refresh, and this module deliberately does not start one.
//!
//! "Source bytes are current" and "the structural index is current" are
//! separate claims. A DIRTY component does not block a verified read --
//! the hash either matches or it does not -- but the result still carries
//! [`Currentness::NotCurrent`], because the *structure* being described
//! is what may have moved on.
//!
//! Out of scope: text/regex fallback and the wire status contract (task
//! 12), targeted refresh (task 13), partial/last-valid policy (task 14),
//! Relation/semantic (I3/I4), and any kind of edit. Nothing here caches a
//! file body either: the canonical current source is the filesystem, once
//! per read.

use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    str::Utf8Error,
};

use brainprint_core::{ResourceId, SymbolId};
use rusqlite::{Connection, OptionalExtension};

use crate::{
    component, identity,
    parser::SourceSpan,
    query::{Currentness, QueryError, QueryIndex, ResultSource, StructuralCoverage, coverage_of},
    resource::{self, Resource, ResourceKind, ResourceState},
    symbol::{self, Symbol},
};

/// Why a returned slice may be believed: the hash the bytes actually had
/// when they were read, and how current the structural index around them
/// is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceVerification {
    /// The persisted `resource.content_hash` the read matched against.
    pub expected_content_hash: String,
    /// The hash of the exact bytes that were sliced. Equal to
    /// `expected_content_hash` by construction -- a read that found
    /// otherwise returned [`ReadError::SourceChanged`] instead.
    pub observed_content_hash: String,
    /// Whether the *structural* index may be claimed current. Separate
    /// from the byte-level verification above.
    pub currentness: Currentness,
}

/// One verified range read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeRead {
    pub resource_id: ResourceId,
    /// The Resource's *current* Workspace-relative path, which is the path
    /// that was read. A move keeps the id and changes this.
    pub path_rel: String,
    pub resource_revision: String,
    pub requested_span: SourceSpan,
    /// The span actually sliced. Always equal to `requested_span`: an
    /// out-of-bounds or mis-aligned span is an error, never a range
    /// quietly shrunk until it succeeds.
    pub effective_span: SourceSpan,
    pub source: String,
    pub verification: SourceVerification,
    pub result_source: ResultSource,
}

/// Everything a caller needs about one Symbol, including its current
/// source, so that nothing has to go back and read the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolInspection {
    /// Identity and metadata: `SymbolId`, name, kind, `qualified_name`,
    /// signature, parent `SymbolId`, `ResourceId`, the exact declaration
    /// span, and the `resource_revision` it was extracted from.
    pub symbol: Symbol,
    /// The owning Resource's current Workspace-relative path.
    pub path_rel: String,
    /// The current source of the declaration span -- that span only, never
    /// the whole file.
    pub source: String,
    pub coverage: StructuralCoverage,
    pub verification: SourceVerification,
    pub result_source: ResultSource,
}

/// Failure of a verified current-source read.
#[derive(Debug)]
pub enum ReadError {
    Query(QueryError),
    Sqlite(rusqlite::Error),
    /// This `index.db` has no Resource with that id at all.
    UnknownResource {
        resource_id: ResourceId,
    },
    /// The Resource is a DELETED tombstone: it has no current source, and
    /// an empty body would be a lie about a file that is gone.
    ResourceDeleted {
        resource_id: ResourceId,
        path_rel: String,
    },
    /// The Resource has no content of its own to read (a directory).
    NotAFile {
        resource_id: ResourceId,
        path_rel: String,
    },
    /// No `content_hash` is persisted, so no read can be verified. Better
    /// than returning source nothing vouches for.
    UnverifiableResource {
        resource_id: ResourceId,
        path_rel: String,
    },
    /// This `index.db` has no Symbol with that id.
    UnknownSymbol {
        symbol_id: SymbolId,
    },
    /// The Symbol row was extracted from an older revision of its
    /// Resource, so its span describes source that is no longer there.
    SymbolNotCurrent {
        symbol_id: SymbolId,
        symbol_revision: String,
        resource_revision: String,
    },
    /// The caller's expected `resource_revision` is not the Resource's.
    /// Their span came from a different revision, so it is not sliced.
    RevisionMismatch {
        resource_id: ResourceId,
        expected: String,
        actual: String,
    },
    /// The file's current bytes do not hash to the persisted Resource's
    /// `content_hash`. No source is returned, and `RESOURCE_INDEX` has
    /// been marked DIRTY.
    SourceChanged {
        resource_id: ResourceId,
        path_rel: String,
        expected_content_hash: String,
        observed_content_hash: String,
    },
    /// `start_byte > end_byte`.
    InvalidSpan {
        start_byte: usize,
        end_byte: usize,
    },
    /// The span reaches past the end of the current file.
    SpanOutOfBounds {
        start_byte: usize,
        end_byte: usize,
        length: usize,
    },
    /// The span's bytes are not valid UTF-8 -- either the range splits a
    /// character or the file is not text. Reported rather than truncated
    /// or lossily converted.
    SpanNotUtf8 {
        start_byte: usize,
        end_byte: usize,
        source: Utf8Error,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(source) => write!(formatter, "index query failed: {source}"),
            Self::Sqlite(source) => write!(formatter, "read sqlite error: {source}"),
            Self::UnknownResource { resource_id } => {
                write!(formatter, "no resource row for {resource_id}")
            }
            Self::ResourceDeleted {
                resource_id,
                path_rel,
            } => write!(
                formatter,
                "resource {resource_id} ({path_rel}) is deleted and has no current source"
            ),
            Self::NotAFile {
                resource_id,
                path_rel,
            } => write!(
                formatter,
                "resource {resource_id} ({path_rel}) is not a file and has no source"
            ),
            Self::UnverifiableResource {
                resource_id,
                path_rel,
            } => write!(
                formatter,
                "resource {resource_id} ({path_rel}) has no persisted content hash to verify against"
            ),
            Self::UnknownSymbol { symbol_id } => {
                write!(formatter, "no symbol row for {symbol_id}")
            }
            Self::SymbolNotCurrent {
                symbol_id,
                symbol_revision,
                resource_revision,
            } => write!(
                formatter,
                "symbol {symbol_id} was extracted from revision {symbol_revision}, \
                 but its resource is at {resource_revision}"
            ),
            Self::RevisionMismatch {
                resource_id,
                expected,
                actual,
            } => write!(
                formatter,
                "resource {resource_id} is at revision {actual}, not the expected {expected}"
            ),
            Self::SourceChanged {
                resource_id,
                path_rel,
                expected_content_hash,
                observed_content_hash,
            } => write!(
                formatter,
                "resource {resource_id} ({path_rel}) now hashes to {observed_content_hash}, \
                 not the indexed {expected_content_hash}"
            ),
            Self::InvalidSpan {
                start_byte,
                end_byte,
            } => write!(
                formatter,
                "span start {start_byte} is past its end {end_byte}"
            ),
            Self::SpanOutOfBounds {
                start_byte,
                end_byte,
                length,
            } => write!(
                formatter,
                "span {start_byte}..{end_byte} reaches past the {length}-byte source"
            ),
            Self::SpanNotUtf8 {
                start_byte,
                end_byte,
                source,
            } => write!(
                formatter,
                "span {start_byte}..{end_byte} is not valid UTF-8: {source}"
            ),
            Self::Io { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
        }
    }
}

impl Error for ReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query(source) => Some(source),
            Self::Sqlite(source) => Some(source),
            Self::SpanNotUtf8 { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<QueryError> for ReadError {
    fn from(source: QueryError) -> Self {
        Self::Query(source)
    }
}

impl From<rusqlite::Error> for ReadError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<component::ComponentError> for ReadError {
    fn from(source: component::ComponentError) -> Self {
        Self::Query(QueryError::Component(source))
    }
}

impl From<resource::ResourceError> for ReadError {
    fn from(source: resource::ResourceError) -> Self {
        Self::Query(QueryError::Resource(source))
    }
}

impl From<symbol::SymbolError> for ReadError {
    fn from(source: symbol::SymbolError) -> Self {
        Self::Query(QueryError::Symbol(source))
    }
}

/// Verified reads of one Workspace's current source, against its
/// `index.db`.
pub struct SourceReader {
    index: QueryIndex,
    workspace_root: PathBuf,
}

impl SourceReader {
    /// Open the `index.db` at `index_path` for the Workspace rooted at
    /// `workspace_root`.
    pub fn open(index_path: &Path, workspace_root: &Path) -> Result<Self, ReadError> {
        Ok(Self::new(QueryIndex::open(index_path)?, workspace_root))
    }

    /// Read against an already-opened index.
    #[must_use]
    pub fn new(index: QueryIndex, workspace_root: &Path) -> Self {
        Self {
            index,
            workspace_root: workspace_root.to_path_buf(),
        }
    }

    /// The structured index these reads are verified against.
    #[must_use]
    pub const fn index(&self) -> &QueryIndex {
        &self.index
    }

    /// The current source of `span` in `resource_id`, if the Resource is
    /// still at `expected_revision` and its bytes still hash to what was
    /// indexed.
    pub fn read_range(
        &self,
        resource_id: ResourceId,
        expected_revision: &str,
        span: SourceSpan,
    ) -> Result<RangeRead, ReadError> {
        let resource = self.readable_resource(resource_id)?;
        if resource.resource_revision != expected_revision {
            return Err(ReadError::RevisionMismatch {
                resource_id,
                expected: expected_revision.to_owned(),
                actual: resource.resource_revision,
            });
        }

        let (bytes, verification) = self.verified_bytes(&resource)?;
        let source = slice(&bytes, span)?;

        Ok(RangeRead {
            resource_id,
            path_rel: resource.path_rel,
            resource_revision: resource.resource_revision,
            requested_span: span,
            effective_span: span,
            source,
            verification,
            result_source: ResultSource::StructuralIndex,
        })
    }

    /// One Symbol's current metadata *and* the current source of its
    /// definition span.
    ///
    /// The span is the declaration's, body included -- which is what makes
    /// a second read unnecessary -- and nothing beyond it is returned.
    pub fn inspect_symbol(&self, symbol_id: SymbolId) -> Result<SymbolInspection, ReadError> {
        let Some(symbol) = self.symbol_row(symbol_id)? else {
            return Err(ReadError::UnknownSymbol { symbol_id });
        };
        let resource = self.readable_resource(symbol.resource_id)?;
        if symbol.resource_revision != resource.resource_revision {
            return Err(ReadError::SymbolNotCurrent {
                symbol_id,
                symbol_revision: symbol.resource_revision,
                resource_revision: resource.resource_revision,
            });
        }

        let (bytes, verification) = self.verified_bytes(&resource)?;
        let source = slice(&bytes, symbol.span)?;

        Ok(SymbolInspection {
            symbol,
            coverage: coverage_of(&resource.path_rel, resource.kind.as_str()),
            path_rel: resource.path_rel,
            source,
            verification,
            result_source: ResultSource::StructuralIndex,
        })
    }

    /// The Resource behind an id, refusing the states that have no current
    /// source rather than reporting them as empty.
    fn readable_resource(&self, resource_id: ResourceId) -> Result<Resource, ReadError> {
        let raw = self
            .index
            .connection()
            .query_row(
                &format!("{} WHERE r.uid = ?1", resource::SELECT_RESOURCE_SQL),
                rusqlite::params![resource_id.to_bytes().to_vec()],
                resource::raw_resource_from_row,
            )
            .optional()?;
        let Some(resource) = raw.map(resource::decode_resource).transpose()? else {
            return Err(ReadError::UnknownResource { resource_id });
        };
        match resource.state {
            ResourceState::Deleted => Err(ReadError::ResourceDeleted {
                resource_id,
                path_rel: resource.path_rel,
            }),
            ResourceState::Active => Ok(resource),
        }
    }

    fn symbol_row(&self, symbol_id: SymbolId) -> Result<Option<Symbol>, ReadError> {
        let raw = self
            .index
            .connection()
            .query_row(
                &format!(
                    "SELECT {} {} WHERE s.uid = ?1",
                    symbol::SYMBOL_COLUMNS,
                    symbol::SYMBOL_FROM
                ),
                rusqlite::params![symbol_id.to_bytes().to_vec()],
                symbol::raw_symbol_row,
            )
            .optional()?;
        Ok(raw.map(symbol::decode_symbol).transpose()?)
    }

    /// Read the Resource's current file and prove those exact bytes are
    /// the ones the index describes.
    ///
    /// The path comes from the Resource row, so a moved Resource is read
    /// at its new path and a stale locator's old path is never opened.
    fn verified_bytes(
        &self,
        resource: &Resource,
    ) -> Result<(Vec<u8>, SourceVerification), ReadError> {
        let Some(expected) = resource.content_hash.clone() else {
            return Err(if resource.kind == ResourceKind::File {
                ReadError::UnverifiableResource {
                    resource_id: resource.id,
                    path_rel: resource.path_rel.clone(),
                }
            } else {
                ReadError::NotAFile {
                    resource_id: resource.id,
                    path_rel: resource.path_rel.clone(),
                }
            });
        };

        let path = self.workspace_root.join(&resource.path_rel);
        // A vanished path is an I/O failure, not an empty file: claiming
        // "the source is ''" about a file that is not there is the false
        // zero #16 forbids.
        let bytes = fs::read(&path).map_err(|source| ReadError::Io {
            path: path.clone(),
            source,
        })?;
        let observed = identity::content_hash_of(&bytes);
        if observed != expected {
            // The index describes bytes that are gone. Say so, and stop
            // later queries claiming currentness -- without touching the
            // watcher journal or inventing a Workspace revision.
            self.mark_resource_index_dirty()?;
            return Err(ReadError::SourceChanged {
                resource_id: resource.id,
                path_rel: resource.path_rel.clone(),
                expected_content_hash: expected,
                observed_content_hash: observed,
            });
        }

        let verification = SourceVerification {
            expected_content_hash: expected,
            observed_content_hash: observed,
            currentness: self.index.currentness()?,
        };
        Ok((bytes, verification))
    }

    /// Flip `RESOURCE_INDEX` to DIRTY through the ordinary component API.
    ///
    /// A component that was never published has nothing to demote: there
    /// is no claim of currentness to withdraw, and writing a row here
    /// would invent a basis revision.
    fn mark_resource_index_dirty(&self) -> Result<(), ReadError> {
        let connection: &Connection = self.index.connection();
        let Some(state) = component::read(connection)? else {
            return Ok(());
        };
        component::mark_dirty(
            connection,
            &state.basis_workspace_revision,
            Some(component::SOURCE_HASH_MISMATCH_CODE),
        )?;
        Ok(())
    }
}

/// Slice the verified bytes, or explain why the span does not describe
/// them. Byte offsets are the slicing truth; line/column are locator
/// metadata that never decide what is returned.
fn slice(bytes: &[u8], span: SourceSpan) -> Result<String, ReadError> {
    if span.start_byte > span.end_byte {
        return Err(ReadError::InvalidSpan {
            start_byte: span.start_byte,
            end_byte: span.end_byte,
        });
    }
    if span.end_byte > bytes.len() {
        return Err(ReadError::SpanOutOfBounds {
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            length: bytes.len(),
        });
    }
    // A range that splits a character starts or ends on a continuation
    // byte, which is not valid UTF-8 on its own -- so this one check
    // covers both a mis-aligned boundary and a file that is not text. No
    // lossy conversion, no shrinking the range until it decodes.
    std::str::from_utf8(&bytes[span.start_byte..span.end_byte])
        .map(ToOwned::to_owned)
        .map_err(|source| ReadError::SpanNotUtf8 {
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            source,
        })
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        path::PathBuf,
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        component::{FreshnessState, ProcessingState},
        config::WorkspaceConfig,
        extract::{assign_ids, extract},
        parser::{ParserRegistry, SourceBasis, SourcePoint, dialect_for_resource},
        query::{NotCurrentReason, SymbolSelector},
        reconcile::Reconcile,
        resource::ResourceStore,
        scan::BaselineScan,
        symbol::SymbolStore,
        watch::{RawWatchEvent, WatchIngest},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const BASELINE_REVISION: &str = "workspace-rev-1";

    const APP_TS: &str = "\
export class App {
  run(): number {
    return 41
  }
}
";

    const LIB_PY: &str = "\
def run():
    return 4
";

    /// A Workspace with a published baseline and extracted Symbols.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-inspect-{label}-{}-{sequence}",
                process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src")).expect("src");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("lib.py", LIB_PY);
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        /// An equal-length rewrite whose mtime is restored afterwards, so
        /// nothing but a content hash can notice it.
        fn overwrite_preserving_metadata(&self, rel: &str, contents: &str) {
            let path = self.root.join(rel);
            let before = fs::metadata(&path).expect("metadata");
            let mtime = before.modified().expect("mtime");
            assert_eq!(
                before.len(),
                contents.len() as u64,
                "this helper only makes sense for an equal-length rewrite"
            );
            fs::write(&path, contents).expect("rewrite");
            fs::File::options()
                .write(true)
                .open(&path)
                .expect("open")
                .set_modified(mtime)
                .expect("mtime should be restorable");
        }

        /// Publish the Resource baseline and extract every supported
        /// file's Symbols from its current content.
        fn index(&self) {
            let engine = BaselineScan::open(&self.db_path()).expect("index.db");
            engine
                .run_initial_scan(&self.root, &WorkspaceConfig::default(), BASELINE_REVISION)
                .expect("baseline scan");
            drop(engine);
            self.index_symbols();
        }

        fn index_symbols(&self) {
            let store = SymbolStore::open(&self.db_path()).expect("index.db");
            for rel in ["src/app.ts", "lib.py"] {
                let resource = self.resource(rel);
                let source = fs::read(self.root.join(rel)).expect("current source");
                let dialect = dialect_for_resource(&resource).expect("a supported dialect");
                let mut registry = ParserRegistry::new();
                let tree = registry
                    .parse(dialect, &source, SourceBasis::of(&resource))
                    .expect("parse");
                let extraction = extract(&tree, &source);
                let profile_id = store.ensure_profile(&extraction.profile).expect("profile");
                let symbols = assign_ids(&[], &extraction, &resource, profile_id);
                store
                    .replace_for_resource(resource.id, &resource.resource_revision, &symbols)
                    .expect("replace");
            }
        }

        fn reader(&self) -> SourceReader {
            SourceReader::open(&self.db_path(), &self.root).expect("index.db")
        }

        fn resource(&self, rel: &str) -> Resource {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn symbol(&self, qualified_name: &str) -> Symbol {
            let index = QueryIndex::open(&self.db_path()).expect("index.db");
            let located = index
                .search_symbols(&crate::query::SymbolQuery::new(
                    SymbolSelector::QualifiedName(qualified_name),
                ))
                .expect("search");
            located
                .exact()
                .expect("exactly one current symbol")
                .symbol
                .clone()
        }

        fn ingest(&self, events: &[RawWatchEvent]) {
            let ingest = WatchIngest::open(&self.db_path()).expect("index.db");
            ingest
                .ingest_all(&self.root, &WorkspaceConfig::default(), events)
                .expect("ingestion");
        }

        fn reconcile(&self) {
            let engine = Reconcile::open(&self.db_path()).expect("index.db");
            engine
                .run(&self.root, &WorkspaceConfig::default())
                .expect("reconcile");
        }

        fn resource_index_state(&self) -> FreshnessState {
            let engine = Reconcile::open(&self.db_path()).expect("index.db");
            engine
                .resource_index_state()
                .expect("component state")
                .expect("published")
                .freshness_state
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// A byte range with placeholder line/column, which is the point:
    /// line/column are locator metadata, and the byte range alone decides
    /// what is sliced.
    const fn byte_span(start_byte: usize, end_byte: usize) -> SourceSpan {
        SourceSpan {
            start_byte,
            end_byte,
            start: SourcePoint::new(0, 0),
            end: SourcePoint::new(0, 0),
        }
    }

    #[test]
    fn inspecting_a_symbol_returns_its_metadata_and_current_definition_source_in_one_call() {
        let fixture = Fixture::create("inspect-symbol");
        fixture.index();
        let symbol = fixture.symbol("App.run");

        let inspection = fixture
            .reader()
            .inspect_symbol(symbol.id)
            .expect("inspect should succeed");

        assert_eq!(inspection.symbol.id, symbol.id);
        assert_eq!(inspection.symbol.name, "run");
        assert_eq!(inspection.symbol.qualified_name, "App.run");
        assert!(inspection.symbol.signature.is_some());
        assert_eq!(
            inspection.symbol.parent_id,
            Some(fixture.symbol("App").id),
            "the containing declaration's identity comes back with it"
        );
        assert_eq!(
            inspection.symbol.resource_id,
            fixture.resource("src/app.ts").id
        );
        assert_eq!(inspection.path_rel, "src/app.ts");
        assert_eq!(
            inspection.symbol.resource_revision,
            fixture.resource("src/app.ts").resource_revision
        );
        assert_eq!(inspection.symbol.span, symbol.span);
        assert_eq!(inspection.coverage, StructuralCoverage::Complete);
        assert!(inspection.verification.currentness.is_current());
        // The method *body* is there: nothing has to go back and read the
        // file to find out what `run` does.
        assert_eq!(
            inspection.source, "run(): number {\n    return 41\n  }",
            "the definition span's current source, body included"
        );
    }

    #[test]
    fn a_range_read_returns_exactly_the_requested_bytes_and_nothing_else() {
        let fixture = Fixture::create("range-read");
        fixture.index();
        let resource = fixture.resource("lib.py");

        let read = fixture
            .reader()
            .read_range(
                resource.id,
                &resource.resource_revision,
                byte_span(0, "def run():".len()),
            )
            .expect("read should succeed");

        assert_eq!(read.source, "def run():");
        assert_eq!(read.path_rel, "lib.py");
        assert_eq!(read.resource_id, resource.id);
        assert_eq!(read.resource_revision, resource.resource_revision);
        assert_eq!(read.requested_span, read.effective_span);
        assert_eq!(
            read.verification.observed_content_hash,
            resource.content_hash.expect("a file has a content hash"),
            "the verification basis is the hash of the bytes that were sliced"
        );
        assert!(read.verification.currentness.is_current());
    }

    #[test]
    fn a_body_edit_that_shifts_lines_is_reflected_in_the_next_span_and_source() {
        let fixture = Fixture::create("body-edit");
        fixture.index();
        let before = fixture
            .reader()
            .inspect_symbol(fixture.symbol("App.run").id)
            .expect("inspect");

        fixture.write(
            "src/app.ts",
            "// a new leading comment\nexport class App {\n  run(): number {\n    return 42\n  }\n}\n",
        );
        fixture.ingest(&[RawWatchEvent::Modified {
            path: fixture.root.join("src/app.ts"),
        }]);
        fixture.reconcile();
        fixture.index_symbols();

        let after = fixture
            .reader()
            .inspect_symbol(fixture.symbol("App.run").id)
            .expect("inspect the re-extracted symbol");

        assert!(
            after.symbol.span.start_byte > before.symbol.span.start_byte,
            "the declaration moved down the file"
        );
        assert_eq!(after.symbol.span.start.line, 2, "one line further down");
        assert_eq!(after.source, "run(): number {\n    return 42\n  }");
        assert!(after.verification.currentness.is_current());
    }

    #[test]
    fn an_expected_revision_that_is_not_the_resources_refuses_to_return_source() {
        let fixture = Fixture::create("revision-mismatch");
        fixture.index();
        let resource = fixture.resource("lib.py");

        let failure = fixture
            .reader()
            .read_range(resource.id, "not-the-current-revision", byte_span(0, 3))
            .expect_err("a stale locator must not be sliced");

        assert!(
            matches!(
                failure,
                ReadError::RevisionMismatch { ref expected, ref actual, .. }
                    if expected == "not-the-current-revision"
                        && *actual == resource.resource_revision
            ),
            "unexpected error: {failure}"
        );
    }

    #[test]
    fn an_equal_length_edit_under_the_same_mtime_is_caught_by_the_content_hash() {
        let fixture = Fixture::create("same-size-edit");
        fixture.index();
        let resource = fixture.resource("lib.py");
        // Same byte length, same mtime: only hashing the bytes can tell.
        fixture.overwrite_preserving_metadata("lib.py", "def run():\n    return 9\n");

        let failure = fixture
            .reader()
            .read_range(
                resource.id,
                &resource.resource_revision,
                byte_span(0, "def run():".len()),
            )
            .expect_err("changed source must not be sliced");

        let ReadError::SourceChanged {
            expected_content_hash,
            observed_content_hash,
            ..
        } = &failure
        else {
            panic!("unexpected error: {failure}");
        };
        assert_eq!(
            Some(expected_content_hash.as_str()),
            resource.content_hash.as_deref()
        );
        assert_ne!(expected_content_hash, observed_content_hash);
    }

    #[test]
    fn a_hash_mismatch_leaves_the_resource_index_unable_to_claim_current() {
        let fixture = Fixture::create("mismatch-dirties");
        fixture.index();
        let resource = fixture.resource("lib.py");
        assert_eq!(fixture.resource_index_state(), FreshnessState::Current);
        // No watcher event at all: the read itself is what notices.
        fixture.overwrite_preserving_metadata("lib.py", "def run():\n    return 9\n");

        let reader = fixture.reader();
        reader
            .read_range(resource.id, &resource.resource_revision, byte_span(0, 3))
            .expect_err("changed source must not be sliced");

        assert_eq!(fixture.resource_index_state(), FreshnessState::Dirty);
        let engine = Reconcile::open(&fixture.db_path()).expect("index.db");
        let state = engine
            .resource_index_state()
            .expect("component state")
            .expect("published");
        assert_eq!(state.processing_state, ProcessingState::Queued);
        assert_eq!(
            state.last_error_code.as_deref(),
            Some(component::SOURCE_HASH_MISMATCH_CODE)
        );
        assert!(
            state.stable_generation_id.is_some(),
            "the last published generation stays available as the last valid snapshot"
        );
        assert!(
            engine.journal().expect("journal").is_empty(),
            "a read never fabricates watcher journal entries"
        );
        // And the next query says so rather than repeating "current".
        assert_eq!(
            reader.index().currentness().expect("currentness"),
            Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty)
        );
    }

    #[test]
    fn a_span_past_the_end_of_the_current_file_is_an_error_not_a_truncated_read() {
        let fixture = Fixture::create("out-of-bounds");
        fixture.index();
        let resource = fixture.resource("lib.py");
        let length = LIB_PY.len();

        let failure = fixture
            .reader()
            .read_range(
                resource.id,
                &resource.resource_revision,
                byte_span(0, length + 10),
            )
            .expect_err("an out-of-bounds span must not succeed");

        assert!(
            matches!(
                failure,
                ReadError::SpanOutOfBounds { end_byte, length: actual, .. }
                    if end_byte == length + 10 && actual == length
            ),
            "unexpected error: {failure}"
        );

        let inverted = fixture
            .reader()
            .read_range(resource.id, &resource.resource_revision, byte_span(5, 2))
            .expect_err("an inverted span must not succeed");
        assert!(
            matches!(
                inverted,
                ReadError::InvalidSpan {
                    start_byte: 5,
                    end_byte: 2
                }
            ),
            "unexpected error: {inverted}"
        );
    }

    #[test]
    fn a_span_that_splits_a_character_is_rejected_rather_than_silently_truncated() {
        let fixture = Fixture::create("utf8-boundary");
        // `가` is three bytes; the comment puts it at a known offset.
        fixture.write("lib.py", "# 가\ndef run():\n    return 4\n");
        fixture.index();
        let resource = fixture.resource("lib.py");
        let split = "# ".len() + 1;

        let failure = fixture
            .reader()
            .read_range(
                resource.id,
                &resource.resource_revision,
                byte_span(0, split),
            )
            .expect_err("a mid-character boundary must not succeed");

        assert!(
            matches!(failure, ReadError::SpanNotUtf8 { end_byte, .. } if end_byte == split),
            "unexpected error: {failure}"
        );

        // The whole character, on the other hand, reads fine.
        let read = fixture
            .reader()
            .read_range(
                resource.id,
                &resource.resource_revision,
                byte_span(0, "# 가".len()),
            )
            .expect("an aligned span reads");
        assert_eq!(read.source, "# 가");
    }

    #[test]
    fn a_moved_resource_is_read_at_its_new_path_under_the_same_id() {
        let fixture = Fixture::create("moved");
        fixture.index();
        let original = fixture.resource("lib.py").id;

        fs::rename(fixture.root.join("lib.py"), fixture.root.join("src/lib.py"))
            .expect("rename should succeed");
        fixture.ingest(&[RawWatchEvent::RenamedPair {
            from: fixture.root.join("lib.py"),
            to: fixture.root.join("src/lib.py"),
        }]);
        fixture.reconcile();

        let moved = fixture.resource("src/lib.py");
        assert_eq!(moved.id, original, "a move keeps the stable id");

        let read = fixture
            .reader()
            .read_range(
                moved.id,
                &moved.resource_revision,
                byte_span(0, "def run():".len()),
            )
            .expect("the read follows the Resource, not the stale path");

        assert_eq!(read.path_rel, "src/lib.py");
        assert_eq!(read.source, "def run():");
    }

    #[test]
    fn a_deleted_resource_is_explicitly_unavailable_rather_than_empty_source() {
        let fixture = Fixture::create("deleted");
        fixture.index();
        let resource = fixture.resource("lib.py");

        fs::remove_file(fixture.root.join("lib.py")).expect("remove");
        fixture.ingest(&[RawWatchEvent::Removed {
            path: fixture.root.join("lib.py"),
        }]);
        fixture.reconcile();

        let failure = fixture
            .reader()
            .read_range(resource.id, &resource.resource_revision, byte_span(0, 3))
            .expect_err("a tombstone has no current source");

        assert!(
            matches!(failure, ReadError::ResourceDeleted { ref path_rel, .. } if path_rel == "lib.py"),
            "unexpected error: {failure}"
        );
    }

    #[test]
    fn a_dirty_structural_index_still_allows_a_verified_read_but_stays_not_current() {
        let fixture = Fixture::create("dirty-but-readable");
        fixture.index();
        let resource = fixture.resource("src/app.ts");
        // Something else moved, so the structural index is DIRTY -- but
        // this Resource's own bytes are untouched.
        fixture.write("lib.py", "def run():\n    return 5\n");
        fixture.ingest(&[RawWatchEvent::Modified {
            path: fixture.root.join("lib.py"),
        }]);
        assert_eq!(fixture.resource_index_state(), FreshnessState::Dirty);

        let inspection = fixture
            .reader()
            .inspect_symbol(fixture.symbol("App.run").id)
            .expect("content-compatible source is still readable");

        assert_eq!(inspection.source, "run(): number {\n    return 41\n  }");
        assert_eq!(
            inspection.verification.expected_content_hash,
            inspection.verification.observed_content_hash
        );
        assert_eq!(
            inspection.verification.currentness,
            Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty),
            "verified bytes and a current structural index are separate claims"
        );
        assert_eq!(
            resource.content_hash.as_deref(),
            Some(inspection.verification.expected_content_hash.as_str())
        );
    }

    #[test]
    fn reading_source_never_stores_it_in_the_index() {
        let fixture = Fixture::create("no-source-mirror");
        fixture.index();
        let resource = fixture.resource("src/app.ts");

        let read = fixture
            .reader()
            .read_range(
                resource.id,
                &resource.resource_revision,
                byte_span(0, APP_TS.len()),
            )
            .expect("read");
        assert_eq!(read.source, APP_TS);
        fixture
            .reader()
            .inspect_symbol(fixture.symbol("App.run").id)
            .expect("inspect");

        let database = fs::read(fixture.db_path()).expect("index.db should be readable");
        for body in ["return 41", "def run():\n    return 4"] {
            assert!(
                !contains(&database, body.as_bytes()),
                "index.db must not mirror source text ({body:?})"
            );
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
