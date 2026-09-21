//! The query status contract and the bounded current-filesystem text
//! fallback (#16 task 12 / #5 search contract).
//!
//! ## Why a status vocabulary exists
//!
//! The one thing this module refuses to do is collapse different reasons
//! for an empty answer into the same shape. "Nothing declares `run`",
//! "this language has no structural parser", "the index is being
//! rebuilt", "the budget ran out after 40 files", and "that directory
//! could not be read" are five different facts, and an agent that reads
//! them all as *no such thing* deletes code that exists.
//! [`QueryStatus`] keeps them apart, and no path here turns a truncated
//! or unreadable scope into [`QueryStatus::NotFound`].
//!
//! ## Two search tiers, not a chain
//!
//! [`structured_status`] normalizes task 10's [`Located`] into that
//! vocabulary. The text fallback is *not* its continuation: an empty
//! Symbol query over a complete, current scope is an answer, and
//! re-running it as a text sweep would be a slower way to be less sure.
//! [`fallback_permitted`] encodes when a fallback is legitimate --
//! explicit text/regex search, targets the structural model does not
//! describe (comments, strings, error messages), or a scope the
//! structural index genuinely does not cover -- and
//! [`TextSearcher::search`] refuses to run otherwise.
//!
//! A text match is evidence about bytes, and stays that. Nothing here
//! turns one into a Symbol or a Relation.
//!
//! ## What is searched
//!
//! The current filesystem, walked with the same discovery and exclusion
//! rules as the index (#16 task 2), never a source mirror in `index.db`
//! (there is none, and this task does not add one). Searching the
//! persisted ACTIVE Resource list instead would make a file created a
//! second ago permanently unfindable, and would keep "finding" a file
//! that has been deleted.
//!
//! Out of scope: targeted structural refresh (task 13), the final
//! partial/last-valid publication policy (task 14), Relation/semantic
//! search, any source FTS table, the MCP/daemon surface (I5), and output
//! compression (I6).

use std::{
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use brainprint_core::ResourceId;
use regex::{Regex, RegexBuilder};

use crate::{
    config::WorkspaceConfig,
    discovery::{self, DiscoveryError},
    identity,
    parser::{SourcePoint, SourceSpan},
    query::{
        Currentness, Located, NotCurrentReason, QueryError, QueryIndex, ResourceLocator,
        StructuralCoverage,
    },
    resource::ResourceKind,
};

/// How an answer relates to the question, so that five different reasons
/// for "nothing" never arrive as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryStatus {
    /// Results exist that mean what was asked for.
    Found,
    /// Nothing matches -- and this may only be said after a *complete*
    /// search of a current, supported scope.
    NotFound,
    /// Several candidates, and picking one would be a guess.
    Ambiguous,
    /// No capability covers the requested meaning for (part of) the
    /// scope. An empty result says nothing about what is there.
    Unsupported,
    /// A budget stopped the search before the scope was finished.
    /// Whatever was found is still returned, and zero matches under this
    /// status is emphatically not a "no".
    Truncated,
    /// A component the answer depends on is dirty/queued, so no current
    /// structured answer can be confirmed right now.
    Refreshing,
    /// An I/O, backend, or state failure prevented the request from being
    /// carried out at all.
    Unavailable,
}

impl QueryStatus {
    /// Whether this status may be read as "there is no such thing".
    /// Only [`Self::NotFound`] may.
    #[must_use]
    pub const fn is_negative_answer(self) -> bool {
        matches!(self, Self::NotFound)
    }
}

impl fmt::Display for QueryStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Found => "FOUND",
            Self::NotFound => "NOT_FOUND",
            Self::Ambiguous => "AMBIGUOUS",
            Self::Unsupported => "UNSUPPORTED",
            Self::Truncated => "TRUNCATED",
            Self::Refreshing => "REFRESHING",
            Self::Unavailable => "UNAVAILABLE",
        })
    }
}

/// Normalize a task 10 structured result into the status contract.
///
/// The candidates are untouched: a `Refreshing` or `Unsupported` result
/// still carries whatever the index holds, as partial evidence rather
/// than a confirmed current answer.
///
/// Precedence, strongest claim last:
/// 1. The component was never published -- there is no index to answer
///    from, which is a state failure, not an empty answer.
/// 2. The component is DIRTY: nothing current can be confirmed.
/// 3. The candidate list was cut short: the scope was not finished.
/// 4. An exact selector matched more than once: ambiguous by definition.
///    (A *search* returning many results is simply found.)
/// 5. Any candidate at all: found.
/// 6. Zero current candidates, but a Resource in scope is PARTIAL or has
///    last-valid Symbols: its structure is owed a re-analysis, so this is
///    `Refreshing` -- and emphatically not a "no".
/// 7. Zero candidates with some of the scope structurally uncovered:
///    unsupported, never "not found".
/// 8. Zero candidates over a complete, current scope: the one honest
///    `NotFound`.
#[must_use]
pub fn structured_status<T>(located: &Located<T>) -> QueryStatus {
    match located.currentness {
        Currentness::NotCurrent(NotCurrentReason::ResourceIndexNeverPublished) => {
            return QueryStatus::Unavailable;
        }
        Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty) => {
            return QueryStatus::Refreshing;
        }
        Currentness::Current => {}
    }
    if located.truncated {
        return QueryStatus::Truncated;
    }
    if located.exact_selector && located.candidates.len() > 1 {
        return QueryStatus::Ambiguous;
    }
    if !located.candidates.is_empty() {
        return QueryStatus::Found;
    }
    // Last-valid Symbols are evidence that this scope *did* declare
    // something and that its current structure is pending (#16 task 14).
    // Answering "not found" here would be the false zero in its purest
    // form: the file is still there, and so are its declarations.
    if !located.last_valid.is_empty()
        || located
            .incomplete_coverage
            .iter()
            .any(|note| note.coverage == StructuralCoverage::Partial)
    {
        return QueryStatus::Refreshing;
    }
    if located.incomplete_coverage.is_empty() {
        QueryStatus::NotFound
    } else {
        QueryStatus::Unsupported
    }
}

/// Why a text fallback is being run. A fallback needs a reason; it is not
/// what happens automatically when a structured query comes back empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// The caller asked for a literal/regex search over source text.
    /// Always legitimate: it *is* the request.
    ExplicitTextSearch,
    /// The target is not something the structural model describes -- a
    /// comment, a string literal, an error message.
    NonStructuralTarget,
    /// The structural index does not cover (part of) the scope, so text
    /// candidates are the only evidence available there.
    IncompleteStructuralCoverage,
}

/// Whether a fallback with this reason may follow a structured result in
/// this state.
///
/// The two prohibitions this enforces: a complete, current, empty Symbol
/// query does not get re-run as a text sweep, and a structured result
/// that already found the thing does not get "confirmed" by searching the
/// source again.
#[must_use]
pub fn fallback_permitted(reason: FallbackReason, structured: Option<QueryStatus>) -> bool {
    match reason {
        // The user asked for text. There is no structured answer that
        // makes their question illegitimate.
        FallbackReason::ExplicitTextSearch | FallbackReason::NonStructuralTarget => true,
        FallbackReason::IncompleteStructuralCoverage => matches!(
            structured,
            Some(QueryStatus::Unsupported | QueryStatus::Truncated | QueryStatus::Refreshing)
        ),
    }
}

/// What to match. Both forms compile to the same linear-time engine; a
/// literal is an escaped pattern, not a second code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextPattern<'a> {
    /// Matched exactly, with every regex metacharacter escaped.
    Literal(&'a str),
    Regex(&'a str),
}

/// Where a match came from. A text hit is byte evidence -- never a
/// Symbol, never a Relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchSource {
    TextFallback,
}

/// The axis that stopped a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetAxis {
    Results,
    Files,
    Bytes,
    Deadline,
}

/// How much work a text search may do.
///
/// The numbers are defaults to be tuned against a benchmark, not
/// architectural constants: they live in a config struct precisely so
/// raising one is a configuration change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchBudget {
    pub max_results: usize,
    pub max_files: usize,
    pub max_bytes: u64,
    /// Wall-clock ceiling, if the caller wants one.
    pub deadline: Option<Duration>,
}

impl Default for SearchBudget {
    fn default() -> Self {
        Self {
            max_results: 200,
            max_files: 5_000,
            max_bytes: 64 * 1024 * 1024,
            deadline: None,
        }
    }
}

/// A file larger than this is not scanned. Like [`SearchBudget`], a
/// default to be tuned rather than a contract.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Hard cap on a match preview, so a "compact preview" can never become
/// the whole file by another name.
pub const PREVIEW_MAX_BYTES: usize = 200;

/// A bounded text search over the current filesystem.
#[derive(Debug, Clone, Copy)]
pub struct TextSearch<'a> {
    pub pattern: TextPattern<'a>,
    /// Case-sensitive unless this is set: a search for `Run` is a search
    /// for `Run`.
    pub case_insensitive: bool,
    /// Restrict the walk to this Workspace-relative path prefix.
    pub path_prefix: Option<&'a str>,
    pub reason: FallbackReason,
    /// The structured status this fallback follows, if any. Checked
    /// against [`fallback_permitted`] before anything is read.
    pub after_structured: Option<QueryStatus>,
    pub budget: SearchBudget,
    pub max_file_bytes: u64,
    pub with_preview: bool,
}

impl<'a> TextSearch<'a> {
    /// An explicit text search with the default budget.
    #[must_use]
    pub fn explicit(pattern: TextPattern<'a>) -> Self {
        Self {
            pattern,
            case_insensitive: false,
            path_prefix: None,
            reason: FallbackReason::ExplicitTextSearch,
            after_structured: None,
            budget: SearchBudget::default(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            with_preview: true,
        }
    }
}

/// One text hit, in the current bytes of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatch {
    pub path_rel: String,
    /// The current ACTIVE Resource these bytes belong to -- set only when
    /// the indexed Resource's `content_hash` matches the bytes actually
    /// scanned. A shared path alone is not identity.
    pub resource_id: Option<ResourceId>,
    /// Byte range plus line/column. Byte offsets are the truth; the
    /// line/column pair is a locator.
    pub span: SourceSpan,
    /// The matched line, capped at [`PREVIEW_MAX_BYTES`].
    pub preview: Option<String>,
    pub source: MatchSource,
}

/// What the search actually managed to cover.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeReport {
    pub files_scanned: usize,
    pub bytes_scanned: u64,
    /// Files skipped because they are not text. Not an incompleteness: a
    /// text search over a PNG has no answer to miss.
    pub binary_skipped: Vec<String>,
    /// Files skipped because they are larger than `max_file_bytes`.
    pub oversized_skipped: Vec<String>,
    /// Files that could not be read. These *are* an incompleteness.
    pub unreadable: Vec<String>,
    /// Files whose size/mtime moved while they were being read. Their
    /// matches are dropped rather than reported as current.
    pub changed_during_scan: Vec<String>,
    /// Which budget axis stopped the walk, if one did.
    pub budget_exhausted: Option<BudgetAxis>,
}

impl ScopeReport {
    /// Whether the requested scope was searched end to end. Only a
    /// complete scope may produce [`QueryStatus::NotFound`].
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.budget_exhausted.is_none()
            && self.unreadable.is_empty()
            && self.changed_during_scan.is_empty()
            && self.oversized_skipped.is_empty()
    }
}

/// A finished text fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSearchResult {
    pub status: QueryStatus,
    /// Ordered by path, then by byte offset. Stable across runs over the
    /// same files.
    pub matches: Vec<TextMatch>,
    pub scope: ScopeReport,
    /// How current the *structural index* is. Deliberately separate from
    /// the matches: these bytes were read from the filesystem just now,
    /// so they are current source evidence even when the structural index
    /// is not current. The two axes are never merged into one claim.
    pub structural_currentness: Currentness,
    pub reason: FallbackReason,
}

/// Failure running a text fallback.
#[derive(Debug)]
pub enum SearchError {
    /// The fallback was not legitimate in this situation -- typically an
    /// attempt to sweep the source after a complete, current structured
    /// answer.
    FallbackNotPermitted {
        reason: FallbackReason,
        after_structured: Option<QueryStatus>,
    },
    InvalidPattern {
        pattern: String,
        source: Box<regex::Error>,
    },
    /// The scope itself could not be walked.
    Discovery(DiscoveryError),
    Query(QueryError),
    Io {
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FallbackNotPermitted {
                reason,
                after_structured,
            } => write!(
                formatter,
                "a {reason:?} text fallback is not permitted after {after_structured:?}"
            ),
            Self::InvalidPattern { pattern, source } => {
                write!(formatter, "invalid search pattern {pattern:?}: {source}")
            }
            Self::Discovery(source) => write!(formatter, "failed to walk the scope: {source}"),
            Self::Query(source) => write!(formatter, "index query failed: {source}"),
            Self::Io { path, source } => {
                write!(formatter, "failed to read {}: {source}", path.display())
            }
        }
    }
}

impl Error for SearchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidPattern { source, .. } => Some(source),
            Self::Discovery(source) => Some(source),
            Self::Query(source) => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::FallbackNotPermitted { .. } => None,
        }
    }
}

impl From<DiscoveryError> for SearchError {
    fn from(source: DiscoveryError) -> Self {
        Self::Discovery(source)
    }
}

impl From<QueryError> for SearchError {
    fn from(source: QueryError) -> Self {
        Self::Query(source)
    }
}

/// Bounded text search over one Workspace's current files.
pub struct TextSearcher<'a> {
    workspace_root: &'a Path,
    config: &'a WorkspaceConfig,
    /// Used only to attach `ResourceId`s and to report how current the
    /// structural index is -- never as the source of the bytes.
    index: &'a QueryIndex,
}

impl<'a> TextSearcher<'a> {
    #[must_use]
    pub const fn new(
        workspace_root: &'a Path,
        config: &'a WorkspaceConfig,
        index: &'a QueryIndex,
    ) -> Self {
        Self {
            workspace_root,
            config,
            index,
        }
    }

    /// Run the search, or explain why it may not run.
    pub fn search(&self, request: &TextSearch<'_>) -> Result<TextSearchResult, SearchError> {
        if !fallback_permitted(request.reason, request.after_structured) {
            return Err(SearchError::FallbackNotPermitted {
                reason: request.reason,
                after_structured: request.after_structured,
            });
        }
        let matcher = compile(request)?;

        // The current filesystem, with the index's own exclusion rules --
        // so a file created a moment ago is searchable and a deleted one
        // is not, whatever the persisted Resource rows still say.
        let entries = discovery::enumerate_resources(self.workspace_root, self.config)?;
        let started = Instant::now();
        let mut scope = ScopeReport::default();
        let mut matches = Vec::new();

        for entry in entries {
            // Directories have no bytes of their own to search.
            if entry.kind != ResourceKind::File {
                continue;
            }
            if let Some(prefix) = request.path_prefix
                && !entry.path_rel.starts_with(prefix)
            {
                continue;
            }
            if let Some(axis) = self.exhausted(request, &scope, matches.len(), started) {
                scope.budget_exhausted = Some(axis);
                break;
            }

            self.scan_file(request, &matcher, &entry.path_rel, &mut scope, &mut matches)?;
        }

        // One last look, so a budget consumed by the final file is still
        // reported rather than rounded down to a complete search.
        if scope.budget_exhausted.is_none()
            && let Some(axis) = self.exhausted(request, &scope, matches.len(), started)
        {
            scope.budget_exhausted = Some(axis);
        }

        Ok(TextSearchResult {
            status: status_for(&matches, &scope),
            matches,
            scope,
            structural_currentness: self.index.currentness()?,
            reason: request.reason,
        })
    }

    /// Read one file's current bytes and collect its matches from that
    /// same buffer.
    fn scan_file(
        &self,
        request: &TextSearch<'_>,
        matcher: &Regex,
        path_rel: &str,
        scope: &mut ScopeReport,
        matches: &mut Vec<TextMatch>,
    ) -> Result<(), SearchError> {
        let path = self.workspace_root.join(path_rel);
        let Ok(before) = fs::metadata(&path) else {
            scope.unreadable.push(path_rel.to_owned());
            return Ok(());
        };
        if before.len() > request.max_file_bytes {
            scope.oversized_skipped.push(path_rel.to_owned());
            return Ok(());
        }
        let Ok(bytes) = fs::read(&path) else {
            // An unreadable file is recorded, never silently dropped: it
            // is the difference between "not there" and "not looked at".
            scope.unreadable.push(path_rel.to_owned());
            return Ok(());
        };
        // Whatever is matched comes out of this one buffer, and the file
        // is re-stat'ed afterwards: bytes that moved underneath the read
        // are not reported as the file's current content.
        let moved = fs::metadata(&path).is_ok_and(|after| {
            after.len() != before.len() || after.modified().ok() != before.modified().ok()
        });
        if moved {
            scope.changed_during_scan.push(path_rel.to_owned());
            return Ok(());
        }

        let Ok(text) = std::str::from_utf8(&bytes) else {
            scope.binary_skipped.push(path_rel.to_owned());
            return Ok(());
        };
        if text.as_bytes().contains(&0) {
            scope.binary_skipped.push(path_rel.to_owned());
            return Ok(());
        }

        scope.files_scanned += 1;
        scope.bytes_scanned += bytes.len() as u64;

        let mut found = Vec::new();
        let mut lines = LineIndex::new(text);
        for hit in matcher.find_iter(text) {
            if matches.len() + found.len() >= request.budget.max_results {
                scope.budget_exhausted = Some(BudgetAxis::Results);
                break;
            }
            found.push((hit.start(), hit.end()));
        }
        if found.is_empty() {
            return Ok(());
        }

        // A Resource id is attached only when the indexed Resource really
        // describes these bytes. A matching path with a stale hash is a
        // different file's identity.
        let resource_id = self.resource_id_for(path_rel, &bytes)?;
        for (start, end) in found {
            matches.push(TextMatch {
                path_rel: path_rel.to_owned(),
                resource_id,
                span: SourceSpan {
                    start_byte: start,
                    end_byte: end,
                    start: lines.point(start),
                    end: lines.point(end),
                },
                preview: request.with_preview.then(|| preview(text, start)),
                source: MatchSource::TextFallback,
            });
        }
        Ok(())
    }

    /// The current ACTIVE Resource for this path, if its persisted
    /// `content_hash` is the hash of the bytes just scanned.
    fn resource_id_for(
        &self,
        path_rel: &str,
        bytes: &[u8],
    ) -> Result<Option<ResourceId>, SearchError> {
        let located = self
            .index
            .locate_resource(ResourceLocator::Path(path_rel))?;
        let Some(resource) = located.exact() else {
            return Ok(None);
        };
        Ok(
            (resource.content_hash.as_deref() == Some(identity::content_hash_of(bytes).as_str()))
                .then_some(resource.id),
        )
    }

    /// The budget axis that is spent, if any.
    fn exhausted(
        &self,
        request: &TextSearch<'_>,
        scope: &ScopeReport,
        results: usize,
        started: Instant,
    ) -> Option<BudgetAxis> {
        if results >= request.budget.max_results {
            return Some(BudgetAxis::Results);
        }
        if scope.files_scanned >= request.budget.max_files {
            return Some(BudgetAxis::Files);
        }
        if scope.bytes_scanned >= request.budget.max_bytes {
            return Some(BudgetAxis::Bytes);
        }
        if request
            .budget
            .deadline
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            return Some(BudgetAxis::Deadline);
        }
        None
    }
}

/// The status a finished text search may claim.
///
/// The ordering is the whole point: a spent budget outranks the match
/// count, so zero matches over an unfinished scope is `Truncated`, and an
/// unreadable file keeps a fruitless search from claiming `NotFound`.
fn status_for(matches: &[TextMatch], scope: &ScopeReport) -> QueryStatus {
    if scope.budget_exhausted.is_some() {
        return QueryStatus::Truncated;
    }
    if !matches.is_empty() {
        return QueryStatus::Found;
    }
    if !scope.unreadable.is_empty() || !scope.changed_during_scan.is_empty() {
        // Nothing was found in the part that *was* searched, and part of
        // it was not searched at all. That is not a "no".
        return QueryStatus::Unavailable;
    }
    if !scope.oversized_skipped.is_empty() {
        return QueryStatus::Truncated;
    }
    QueryStatus::NotFound
}

fn compile(request: &TextSearch<'_>) -> Result<Regex, SearchError> {
    // One engine for both modes: a literal is an escaped pattern. The
    // `regex` crate matches in linear time, so no pattern -- from the
    // caller or from an escaped literal -- can blow up the search, and
    // nothing depends on an `rg` binary being installed.
    let (pattern, raw) = match request.pattern {
        TextPattern::Literal(literal) => (regex::escape(literal), literal),
        TextPattern::Regex(pattern) => (pattern.to_owned(), pattern),
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(request.case_insensitive)
        .build()
        .map_err(|source| SearchError::InvalidPattern {
            pattern: raw.to_owned(),
            source: Box::new(source),
        })
}

/// The matched line, capped and cut on a character boundary.
fn preview(text: &str, start: usize) -> String {
    let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |index| line_start + index);
    let line = &text[line_start..line_end];
    if line.len() <= PREVIEW_MAX_BYTES {
        return line.to_owned();
    }
    let mut cut = PREVIEW_MAX_BYTES;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    line[..cut].to_owned()
}

/// Byte offset → zero-based line and byte column, walked forward once per
/// file rather than re-counting from the start for every hit.
struct LineIndex<'a> {
    text: &'a str,
    cursor: usize,
    line: usize,
    line_start: usize,
}

impl<'a> LineIndex<'a> {
    const fn new(text: &'a str) -> Self {
        Self {
            text,
            cursor: 0,
            line: 0,
            line_start: 0,
        }
    }

    /// Offsets must be non-decreasing, which is how matches arrive.
    fn point(&mut self, offset: usize) -> SourcePoint {
        for (index, byte) in self.text.as_bytes()[self.cursor..offset].iter().enumerate() {
            if *byte == b'\n' {
                self.line += 1;
                self.line_start = self.cursor + index + 1;
            }
        }
        self.cursor = offset;
        SourcePoint::new(self.line, offset - self.line_start)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        extract::{assign_ids, extract},
        parser::{ParserRegistry, SourceBasis, dialect_for_resource},
        query::{ResourceScope, SymbolQuery, SymbolSelector},
        resource::{Resource, ResourceStore},
        scan::BaselineScan,
        symbol::SymbolStore,
        watch::{RawWatchEvent, WatchIngest},
    };

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    const APP_TS: &str = "\
export class App {
  // the retry budget is deliberate
  run(): number {
    return 41
  }
}
";

    const UTIL_TS: &str = "\
export function run(): number {
  // the retry budget is deliberate
  return 3
}
";

    const README_MD: &str = "\
# notes

the retry budget is deliberate, see App.run
";

    const WIDGET_SVELTE: &str = "\
<script lang=\"ts\">
  export function mount() {}
</script>
<div>hi</div>
";

    /// A Workspace with a published baseline, extracted Symbols, and
    /// enough shape to search: duplicate Symbol names, a container-only
    /// component, prose, and a binary file.
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn create(label: &str) -> Self {
            let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let base = env::temp_dir().join(format!(
                "brainprint-search-{label}-{}-{sequence}",
                std::process::id()
            ));
            let root = base.join("workspace");
            fs::create_dir_all(root.join("src").join("util")).expect("src/util");
            fs::create_dir_all(root.join("ui")).expect("ui");
            fs::create_dir_all(root.join("docs")).expect("docs");
            let fixture = Self { base, root };
            fixture.write("src/app.ts", APP_TS);
            fixture.write("src/util/app.ts", UTIL_TS);
            fixture.write("docs/readme.md", README_MD);
            fixture.write("ui/Widget.svelte", WIDGET_SVELTE);
            fs::write(
                fixture.root.join("docs/blob.bin"),
                b"\x00the retry budget\x00",
            )
            .expect("binary fixture");
            fixture
        }

        fn db_path(&self) -> PathBuf {
            self.base.join("data").join("index.db")
        }

        fn write(&self, rel: &str, contents: &str) {
            fs::write(self.root.join(rel), contents).expect("fixture file");
        }

        /// Publish the Resource baseline and the Symbol index.
        fn index(&self) {
            let engine = BaselineScan::open(&self.db_path()).expect("index.db");
            engine
                .run_initial_scan(&self.root, &WorkspaceConfig::default(), "workspace-rev-1")
                .expect("baseline scan");
            drop(engine);

            let store = SymbolStore::open(&self.db_path()).expect("index.db");
            for rel in ["src/app.ts", "src/util/app.ts"] {
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

        fn resource(&self, rel: &str) -> Resource {
            ResourceStore::open(&self.db_path())
                .expect("index.db")
                .get_active_by_path_key(rel)
                .expect("lookup")
                .expect("the fixture file is a Resource")
        }

        fn query(&self) -> QueryIndex {
            QueryIndex::open(&self.db_path()).expect("index.db")
        }

        fn ingest(&self, events: &[RawWatchEvent]) {
            let ingest = WatchIngest::open(&self.db_path()).expect("index.db");
            ingest
                .ingest_all(&self.root, &WorkspaceConfig::default(), events)
                .expect("ingestion");
        }

        fn search(&self, index: &QueryIndex, request: &TextSearch<'_>) -> TextSearchResult {
            TextSearcher::new(&self.root, &config(), index)
                .search(request)
                .expect("search should run")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    /// The discovery config the fallback reuses. Nothing extra is
    /// excluded: the point is that the default rules already apply.
    fn config() -> WorkspaceConfig {
        WorkspaceConfig::default()
    }

    fn hits(result: &TextSearchResult) -> Vec<(&str, usize)> {
        result
            .matches
            .iter()
            .map(|hit| (hit.path_rel.as_str(), hit.span.start_byte))
            .collect()
    }

    #[test]
    fn an_exact_literal_search_reads_the_current_filesystem_and_is_found() {
        let fixture = Fixture::create("literal");
        fixture.index();
        let index = fixture.query();

        let result = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal("retry budget")),
        );

        assert_eq!(result.status, QueryStatus::Found);
        assert_eq!(
            hits(&result)
                .iter()
                .map(|(path, _)| *path)
                .collect::<Vec<_>>(),
            vec!["docs/readme.md", "src/app.ts", "src/util/app.ts"],
        );
        let first = &result.matches[0];
        assert_eq!(first.source, MatchSource::TextFallback);
        assert_eq!(
            &README_MD[first.span.start_byte..first.span.end_byte],
            "retry budget"
        );
        assert_eq!(first.span.start.line, 2);
        assert_eq!(
            first.preview.as_deref(),
            Some("the retry budget is deliberate, see App.run")
        );
        assert_eq!(
            first.resource_id,
            Some(fixture.resource("docs/readme.md").id),
            "an unchanged indexed file's hash still matches, so the id is safe to attach"
        );
        // Case sensitivity is the default.
        let upper = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal("RETRY BUDGET")),
        );
        assert_eq!(upper.status, QueryStatus::NotFound);
        let insensitive = fixture.search(
            &index,
            &TextSearch {
                case_insensitive: true,
                ..TextSearch::explicit(TextPattern::Literal("RETRY BUDGET"))
            },
        );
        assert_eq!(insensitive.status, QueryStatus::Found);
    }

    #[test]
    fn a_literal_is_escaped_and_a_regex_is_not() {
        let fixture = Fixture::create("regex");
        fixture.index();
        let index = fixture.query();

        let regex = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Regex(r"return \d+")),
        );
        assert_eq!(regex.status, QueryStatus::Found);
        assert_eq!(
            hits(&regex)
                .iter()
                .map(|(path, _)| *path)
                .collect::<Vec<_>>(),
            vec!["src/app.ts", "src/util/app.ts"]
        );

        // The same text as a literal matches nothing: metacharacters are
        // escaped rather than interpreted.
        let literal = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal(r"return \d+")),
        );
        assert_eq!(literal.status, QueryStatus::NotFound);

        let broken = TextSearcher::new(&fixture.root, &config(), &index)
            .search(&TextSearch::explicit(TextPattern::Regex("return (")))
            .expect_err("an invalid pattern is reported, not ignored");
        assert!(matches!(broken, SearchError::InvalidPattern { .. }));
    }

    #[test]
    fn a_path_scope_limits_the_walk() {
        let fixture = Fixture::create("scope");
        fixture.index();
        let index = fixture.query();

        let result = fixture.search(
            &index,
            &TextSearch {
                path_prefix: Some("src/util/"),
                ..TextSearch::explicit(TextPattern::Literal("retry budget"))
            },
        );

        assert_eq!(result.status, QueryStatus::Found);
        assert_eq!(
            hits(&result)
                .iter()
                .map(|(path, _)| *path)
                .collect::<Vec<_>>(),
            vec!["src/util/app.ts"]
        );
        assert_eq!(result.scope.files_scanned, 1);
    }

    #[test]
    fn a_spent_result_budget_truncates_without_losing_the_matches_found() {
        let fixture = Fixture::create("result-budget");
        fixture.index();
        let index = fixture.query();

        let result = fixture.search(
            &index,
            &TextSearch {
                budget: SearchBudget {
                    max_results: 1,
                    ..SearchBudget::default()
                },
                ..TextSearch::explicit(TextPattern::Literal("retry budget"))
            },
        );

        assert_eq!(result.status, QueryStatus::Truncated);
        assert_eq!(result.matches.len(), 1, "what was found is still returned");
        assert_eq!(result.scope.budget_exhausted, Some(BudgetAxis::Results));
        assert!(!result.scope.is_complete());
    }

    #[test]
    fn a_spent_budget_with_no_matches_is_truncated_and_never_not_found() {
        let fixture = Fixture::create("empty-budget");
        fixture.index();
        let index = fixture.query();

        let by_files = fixture.search(
            &index,
            &TextSearch {
                budget: SearchBudget {
                    max_files: 1,
                    ..SearchBudget::default()
                },
                ..TextSearch::explicit(TextPattern::Literal("nothing matches this"))
            },
        );
        assert_eq!(by_files.status, QueryStatus::Truncated);
        assert!(by_files.matches.is_empty());
        assert_eq!(by_files.scope.budget_exhausted, Some(BudgetAxis::Files));
        assert!(
            !by_files.status.is_negative_answer(),
            "an unfinished scope may not be read as 'there is no such thing'"
        );

        let by_bytes = fixture.search(
            &index,
            &TextSearch {
                budget: SearchBudget {
                    max_bytes: 1,
                    ..SearchBudget::default()
                },
                ..TextSearch::explicit(TextPattern::Literal("nothing matches this"))
            },
        );
        assert_eq!(by_bytes.status, QueryStatus::Truncated);
        assert_eq!(by_bytes.scope.budget_exhausted, Some(BudgetAxis::Bytes));

        let by_deadline = fixture.search(
            &index,
            &TextSearch {
                budget: SearchBudget {
                    deadline: Some(Duration::ZERO),
                    ..SearchBudget::default()
                },
                ..TextSearch::explicit(TextPattern::Literal("nothing matches this"))
            },
        );
        assert_eq!(by_deadline.status, QueryStatus::Truncated);
        assert_eq!(
            by_deadline.scope.budget_exhausted,
            Some(BudgetAxis::Deadline)
        );
    }

    #[test]
    fn a_complete_current_scope_with_no_hit_is_the_one_honest_not_found() {
        let fixture = Fixture::create("not-found");
        fixture.index();
        let index = fixture.query();

        let result = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal("no such text anywhere")),
        );

        assert_eq!(result.status, QueryStatus::NotFound);
        assert!(result.scope.is_complete());
        assert!(
            !result.scope.binary_skipped.is_empty(),
            "the binary file was skipped, and said so"
        );
    }

    #[test]
    fn a_new_file_is_searchable_before_it_is_ever_indexed() {
        let fixture = Fixture::create("new-file");
        fixture.index();
        let index = fixture.query();
        // Never scanned, never a Resource row.
        fixture.write("src/brand_new.ts", "// retry budget, freshly written\n");

        let result = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal("freshly written")),
        );

        assert_eq!(result.status, QueryStatus::Found);
        assert_eq!(result.matches[0].path_rel, "src/brand_new.ts");
        assert_eq!(
            result.matches[0].resource_id, None,
            "no current ACTIVE row describes these bytes, so no identity is invented"
        );
    }

    #[test]
    fn a_deleted_file_is_not_searched_just_because_a_row_still_names_it() {
        let fixture = Fixture::create("deleted-file");
        fixture.index();
        let index = fixture.query();
        // The row survives; the bytes do not.
        fs::remove_file(fixture.root.join("docs/readme.md")).expect("remove");
        assert!(
            index
                .locate_resource(ResourceLocator::Path("docs/readme.md"))
                .expect("locate")
                .exact()
                .is_some(),
            "the stale ACTIVE row is still there -- and still not searched"
        );

        let result = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal("see App.run")),
        );

        assert_eq!(result.status, QueryStatus::NotFound);
        assert!(result.matches.is_empty());
    }

    #[test]
    fn an_edited_file_matches_on_its_current_bytes_without_borrowing_a_stale_identity() {
        let fixture = Fixture::create("stale-identity");
        fixture.index();
        let index = fixture.query();
        fixture.write("docs/readme.md", "# notes\n\nrewritten: retry budget\n");

        let result = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal("rewritten")),
        );

        assert_eq!(result.status, QueryStatus::Found);
        assert_eq!(result.matches[0].path_rel, "docs/readme.md");
        assert_eq!(
            result.matches[0].resource_id, None,
            "same path, different bytes: the indexed Resource does not describe this match"
        );
    }

    #[test]
    fn matches_are_ordered_by_path_then_byte_offset() {
        let fixture = Fixture::create("ordering");
        fixture.index();
        let index = fixture.query();
        fixture.write("src/a.ts", "// hit\n// hit\n");
        fixture.write("src/b.ts", "// hit\n");

        let expected = vec![("src/a.ts", 3), ("src/a.ts", 10), ("src/b.ts", 3)];
        for _ in 0..3 {
            let result = fixture.search(
                &index,
                &TextSearch {
                    path_prefix: Some("src/"),
                    ..TextSearch::explicit(TextPattern::Literal("hit"))
                },
            );
            assert_eq!(hits(&result), expected, "ordering is stable across runs");
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_keeps_a_fruitless_search_from_claiming_not_found() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::create("unreadable");
        fixture.index();
        let index = fixture.query();
        let path = fixture.root.join("src/app.ts");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");

        let result = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal("no such text anywhere")),
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("restore");

        assert_eq!(result.status, QueryStatus::Unavailable);
        assert_eq!(result.scope.unreadable, vec!["src/app.ts".to_owned()]);
        assert!(!result.scope.is_complete());
        assert!(!result.status.is_negative_answer());
    }

    #[test]
    fn a_text_fallback_never_stores_source_and_never_promotes_a_match() {
        let fixture = Fixture::create("no-promotion");
        fixture.index();
        let index = fixture.query();
        let symbols_before = count(&fixture, "symbol");

        let result = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal("retry budget")),
        );
        assert_eq!(result.status, QueryStatus::Found);
        assert!(
            result
                .matches
                .iter()
                .all(|hit| hit.source == MatchSource::TextFallback)
        );

        // A `run` hit in prose did not become a Symbol, a Relation, or
        // anything else the index would later report as structure.
        assert_eq!(
            count(&fixture, "symbol"),
            symbols_before,
            "the search published no Symbol of its own"
        );
        for table in [
            "occurrence",
            "relation",
            "unresolved_reference",
            "graph_entity",
        ] {
            assert_eq!(count(&fixture, table), 0, "{table} must stay empty");
        }

        let database = fs::read(fixture.db_path()).expect("index.db");
        for body in ["the retry budget is deliberate", "return 41"] {
            assert!(
                !database
                    .windows(body.len())
                    .any(|window| window == body.as_bytes()),
                "index.db must not mirror source text ({body:?})"
            );
        }
    }

    fn count(fixture: &Fixture, table: &str) -> i64 {
        rusqlite::Connection::open(fixture.db_path())
            .expect("index.db")
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count")
    }

    #[test]
    fn a_fallback_is_refused_after_a_complete_structured_answer() {
        let fixture = Fixture::create("routing");
        fixture.index();
        let index = fixture.query();
        let config = config();
        let searcher = TextSearcher::new(&fixture.root, &config, &index);

        for structured in [QueryStatus::Found, QueryStatus::NotFound] {
            let refused = searcher
                .search(&TextSearch {
                    reason: FallbackReason::IncompleteStructuralCoverage,
                    after_structured: Some(structured),
                    ..TextSearch::explicit(TextPattern::Literal("retry budget"))
                })
                .expect_err("a complete structured answer is not re-run as a text sweep");
            assert!(matches!(refused, SearchError::FallbackNotPermitted { .. }));
        }

        // Uncovered scope is exactly what the fallback is for.
        assert!(fallback_permitted(
            FallbackReason::IncompleteStructuralCoverage,
            Some(QueryStatus::Unsupported)
        ));
        // And an explicit text request stands on its own.
        assert!(fallback_permitted(
            FallbackReason::ExplicitTextSearch,
            Some(QueryStatus::Found)
        ));
        assert!(fallback_permitted(
            FallbackReason::NonStructuralTarget,
            None
        ));
        searcher
            .search(&TextSearch {
                after_structured: Some(QueryStatus::Found),
                ..TextSearch::explicit(TextPattern::Literal("retry budget"))
            })
            .expect("an explicit text search runs regardless");
    }

    #[test]
    fn a_binary_file_is_skipped_and_reported() {
        let fixture = Fixture::create("binary");
        fixture.index();
        let index = fixture.query();

        let result = fixture.search(
            &index,
            &TextSearch {
                path_prefix: Some("docs/blob.bin"),
                ..TextSearch::explicit(TextPattern::Literal("retry budget"))
            },
        );

        assert_eq!(result.status, QueryStatus::NotFound);
        assert_eq!(
            result.scope.binary_skipped,
            vec!["docs/blob.bin".to_owned()]
        );
        assert_eq!(result.scope.files_scanned, 0);
        assert!(
            result.scope.is_complete(),
            "a text search over a non-text file has no answer to miss"
        );
    }

    #[test]
    fn a_duplicate_exact_symbol_is_ambiguous_and_a_single_one_is_found() {
        let fixture = Fixture::create("ambiguous");
        fixture.index();
        let index = fixture.query();

        let duplicated = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::Name("run")))
            .expect("search");
        assert_eq!(duplicated.candidates.len(), 2);
        assert_eq!(structured_status(&duplicated), QueryStatus::Ambiguous);

        let single = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::QualifiedName("App.run")))
            .expect("search");
        assert_eq!(structured_status(&single), QueryStatus::Found);

        let missing = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::PathPrefix("src/")),
                ..SymbolQuery::new(SymbolSelector::QualifiedName("nothing.declared"))
            })
            .expect("search");
        assert_eq!(
            structured_status(&missing),
            QueryStatus::NotFound,
            "a complete, current, fully covered scope may say no"
        );
    }

    #[test]
    fn a_container_only_resource_with_no_symbols_is_unsupported_not_not_found() {
        let fixture = Fixture::create("container-only");
        fixture.index();
        let index = fixture.query();
        let widget = fixture.resource("ui/Widget.svelte").id;

        let located = index
            .search_symbols(&SymbolQuery {
                scope: Some(ResourceScope::Id(widget)),
                ..SymbolQuery::new(SymbolSelector::Name("mount"))
            })
            .expect("search");

        assert!(located.candidates.is_empty());
        let status = structured_status(&located);
        assert_eq!(status, QueryStatus::Unsupported);
        assert!(
            !status.is_negative_answer(),
            "zero Symbols in a container-only Resource is a coverage statement"
        );
    }

    #[test]
    fn a_dirty_structural_component_is_refreshing_rather_than_a_current_answer() {
        let fixture = Fixture::create("dirty");
        fixture.index();
        fixture.write("src/app.ts", "export class App {}\n");
        fixture.ingest(&[RawWatchEvent::Modified {
            path: fixture.root.join("src/app.ts"),
        }]);
        let index = fixture.query();

        let located = index
            .search_symbols(&SymbolQuery::new(SymbolSelector::QualifiedName("App.run")))
            .expect("search");
        assert_eq!(structured_status(&located), QueryStatus::Refreshing);

        // The text fallback reads the filesystem itself, so its own hit
        // is current evidence -- which is a separate claim from the
        // structural index being current, and is reported separately.
        let result = fixture.search(
            &index,
            &TextSearch::explicit(TextPattern::Literal("export class App")),
        );
        assert_eq!(result.status, QueryStatus::Found);
        assert_eq!(
            result.structural_currentness,
            Currentness::NotCurrent(NotCurrentReason::ResourceIndexDirty)
        );
    }
}
