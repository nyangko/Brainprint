//! Compact diagnostics (#53 step 1): structured facts read from command
//! output as it streams, in a format the caller names. No format is ever
//! guessed from the program or its output, and no argv is added.
//!
//! A [`DiagnosticParser`] takes the chunks of one stream, splits lines and
//! parses each on the fly; it never holds a whole line. Every line is a
//! diagnostic or a parse miss. [`summarize`] merges the streams, bounds
//! the result and normalizes paths against the Workspace.
//!
//! Memory: every captured field is held to [`MAX_MESSAGE_BYTES`]. A longer
//! message is cut (UTF-8 safe) and flagged; a longer path or code makes its
//! line a parse miss, never a cut-short identity or location.
//!
//! Arbitrary output never panics a parser: whatever is not a diagnostic of
//! the format is a parse miss.

use std::path::Path;

/// Diagnostics one command returns.
pub const MAX_DIAGNOSTICS: usize = 64;
/// UTF-8 bytes of one message; also the bound of every other field.
pub const MAX_MESSAGE_BYTES: usize = 2 * 1024;
/// A P0 defensive bound, not a schema claim: a line may have at most this
/// many JSON containers open at once; one more makes it a parse miss.
const MAX_JSON_DEPTH: usize = 1024;
/// The longest `:<line>:<column>: ` a PathLineColumn line can carry.
const MAX_SEPARATOR_BYTES: usize = ":4294967295:4294967295: ".len();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticFormat {
    /// Cargo's line-delimited JSON (`--message-format=json`, written into
    /// argv by the caller): only `reason: "compiler-message"` envelopes.
    /// A direct `rustc --error-format=json` diagnostic is a parse miss.
    CargoCompilerMessageJson,
    /// `<path>:<line>:<column>: <message>`.
    PathLineColumn,
}

/// In result order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Note,
    Help,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: Option<String>,
    pub message: String,
    pub path: DiagnosticPath,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub stream: Stream,
    pub message_truncated: bool,
}

/// What the path a diagnostic named is, as far as the filesystem shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticPath {
    /// The diagnostic named no path.
    Absent,
    /// An existing file inside the Workspace: its Workspace-relative,
    /// `/`-separated path.
    Workspace(String),
    /// An existing file outside the Workspace; its text is not kept.
    External,
    /// A path that does not resolve to an existing file (virtual, deleted,
    /// missing, a directory): neither inside nor outside is claimed, and
    /// its text is not kept.
    Unresolved,
}

/// `observed == items.len() + deduplicated + omitted`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiagnosticSummary {
    pub items: Vec<Diagnostic>,
    /// Diagnostics parsed, duplicates included.
    pub observed: u64,
    /// Occurrences found to be exact duplicates of an item kept at the
    /// time. Not a global unique count: memory stays bounded, so only the
    /// kept (at most [`MAX_DIAGNOSTICS`]) items are compared.
    pub deduplicated: u64,
    /// Occurrences beyond [`MAX_DIAGNOSTICS`], including repeats of an
    /// item already omitted (it is no longer compared).
    pub omitted: u64,
    /// Lines that are not a diagnostic of the format.
    pub parse_misses: u64,
}

/// A parsed diagnostic before its path is normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    severity: Severity,
    code: Option<String>,
    message: String,
    message_truncated: bool,
    path: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
}

/// The streaming parser of one stream.
#[derive(Debug)]
pub struct DiagnosticParser {
    stream: Stream,
    line: Line,
    /// Bytes arrived since the last newline.
    pending: bool,
    kept: Kept,
    parse_misses: u64,
}

#[derive(Debug)]
enum Line {
    Cargo(Box<CargoLine>),
    Plc(PlcLine),
}

impl DiagnosticParser {
    pub fn new(format: DiagnosticFormat, stream: Stream) -> Self {
        let line = match format {
            DiagnosticFormat::CargoCompilerMessageJson => Line::Cargo(Box::default()),
            DiagnosticFormat::PathLineColumn => Line::Plc(PlcLine::default()),
        };
        Self {
            stream,
            line,
            pending: false,
            kept: Kept::default(),
            parse_misses: 0,
        }
    }

    pub fn write(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\n' {
                self.end_line();
            } else {
                self.pending = true;
                match &mut self.line {
                    Line::Cargo(line) => line.byte(byte),
                    Line::Plc(line) => line.byte(byte),
                }
            }
        }
    }

    /// `complete`: the stream reached EOF. A last line cut off by the end
    /// of the run is a miss, never parsed.
    pub fn finish(mut self, complete: bool) -> StreamDiagnostics {
        if self.pending {
            if complete {
                self.end_line();
            } else {
                self.parse_misses += 1;
            }
        }
        StreamDiagnostics {
            stream: self.stream,
            kept: self.kept,
            parse_misses: self.parse_misses,
        }
    }

    fn end_line(&mut self) {
        self.pending = false;
        let found = match &mut self.line {
            Line::Cargo(line) => std::mem::take(&mut **line).end(),
            Line::Plc(line) => std::mem::take(line).end(),
        };
        match found {
            Some(found) => self.kept.push(found),
            None => self.parse_misses += 1,
        }
    }
}

/// One stream's parsed result, ready for [`summarize`].
#[derive(Debug)]
pub struct StreamDiagnostics {
    stream: Stream,
    kept: Kept,
    parse_misses: u64,
}

/// At most [`MAX_DIAGNOSTICS`] distinct items, in result order (severity,
/// then first seen), so the final bound never needs anything dropped.
#[derive(Debug, Default)]
struct Kept {
    items: Vec<Found>,
    observed: u64,
    deduplicated: u64,
    omitted: u64,
}

impl Kept {
    fn push(&mut self, found: Found) {
        self.observed += 1;
        if self.items.contains(&found) {
            self.deduplicated += 1;
            return;
        }
        if self.items.len() == MAX_DIAGNOSTICS {
            self.omitted += 1;
            // Full: only a better severity displaces the last kept item.
            if self.items.last().expect("full").severity <= found.severity {
                return;
            }
            self.items.pop();
        }
        let at = self
            .items
            .partition_point(|kept| kept.severity <= found.severity);
        self.items.insert(at, found);
    }
}

/// Merge the streams (stdout before stderr within a severity; their
/// relative timing is not kept), bound to [`MAX_DIAGNOSTICS`] and resolve
/// paths: relative ones against `base` (the command's cwd), then against
/// `root` (the Workspace). Both must be canonical.
pub fn summarize(
    streams: impl IntoIterator<Item = StreamDiagnostics>,
    root: &Path,
    base: &Path,
) -> DiagnosticSummary {
    let mut summary = DiagnosticSummary::default();
    let mut merged = Vec::new();
    for stream in streams {
        summary.observed += stream.kept.observed;
        summary.deduplicated += stream.kept.deduplicated;
        summary.omitted += stream.kept.omitted;
        summary.parse_misses += stream.parse_misses;
        merged.extend(
            stream
                .kept
                .items
                .into_iter()
                .map(|found| (stream.stream, found)),
        );
    }
    merged.sort_by_key(|(_, found)| found.severity);
    summary.omitted += merged.len().saturating_sub(MAX_DIAGNOSTICS) as u64;
    merged.truncate(MAX_DIAGNOSTICS);
    summary.items = merged
        .into_iter()
        .map(|(stream, found)| Diagnostic {
            severity: found.severity,
            code: found.code,
            message: found.message,
            path: resolve(found.path.as_deref(), root, base),
            line: found.line,
            column: found.column,
            stream,
            message_truncated: found.message_truncated,
        })
        .collect();
    summary
}

fn resolve(raw: Option<&str>, root: &Path, base: &Path) -> DiagnosticPath {
    let Some(raw) = raw else {
        return DiagnosticPath::Absent;
    };
    let Ok(resolved) = std::fs::canonicalize(base.join(raw)) else {
        return DiagnosticPath::Unresolved;
    };
    if !resolved.is_file() {
        return DiagnosticPath::Unresolved;
    }
    let Ok(relative) = resolved.strip_prefix(root) else {
        return DiagnosticPath::External;
    };
    let parts: Option<Vec<&str>> = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect();
    // A name that is not UTF-8 has no Workspace path text.
    parts.map_or(DiagnosticPath::Unresolved, |parts| {
        DiagnosticPath::Workspace(parts.join("/"))
    })
}

/// Captured bytes as text; a cut-off value may end mid-character, which is
/// dropped. Anything else that is not UTF-8 is `None`.
fn text(mut bytes: Vec<u8>, truncated: bool) -> Option<String> {
    match std::str::from_utf8(&bytes) {
        Ok(_) => {}
        Err(error) if truncated && error.error_len().is_none() => {
            bytes.truncate(error.valid_up_to());
        }
        Err(_) => return None,
    }
    String::from_utf8(bytes).ok()
}

/// Push onto a bounded field; `false` once it is over the bound.
fn capture(field: &mut Vec<u8>, byte: u8) -> bool {
    if field.len() == MAX_MESSAGE_BYTES {
        return false;
    }
    field.push(byte);
    true
}

// ------------------------------------------------------------ PathLineColumn

/// `<path>:<line>:<column>: <message>`. The first `:<line>:<column>: `
/// (1..=10 digits each, fitting `u32`) ends the path, so a drive colon
/// (`C:\`) stays in it. That is the whole grammar: the path is any
/// non-empty run without control bytes, up to [`MAX_MESSAGE_BYTES`]; its
/// name or extension is never judged. The message may open with
/// `error`/`warning`/`note`/`help`, optionally with a `[code]`, then `: `;
/// otherwise its severity is `Unknown`.
#[derive(Debug, Default)]
struct PlcLine {
    failed: bool,
    /// The path and the separator being matched, until one is.
    head: Vec<u8>,
    /// Set once the separator matched: the path's length in `head`.
    path_len: Option<usize>,
    line: u32,
    column: u32,
    message: Vec<u8>,
    truncated: bool,
}

impl PlcLine {
    fn byte(&mut self, byte: u8) {
        if self.failed {
            return;
        }
        if self.path_len.is_some() {
            if !capture(&mut self.message, byte) {
                self.truncated = true;
            }
            return;
        }
        // Past this, any separator would leave a path over the bound.
        if byte.is_ascii_control() || self.head.len() == MAX_MESSAGE_BYTES + MAX_SEPARATOR_BYTES {
            self.failed = true;
            return;
        }
        self.head.push(byte);
        if byte == b' '
            && let Some((path_len, line, column)) = separator(&self.head)
        {
            self.failed = path_len > MAX_MESSAGE_BYTES;
            (self.path_len, self.line, self.column) = (Some(path_len), line, column);
        }
    }

    fn end(mut self) -> Option<Found> {
        let path_len = self.path_len.filter(|_| !self.failed)?;
        if !self.truncated && self.message.last() == Some(&b'\r') {
            self.message.pop();
        }
        self.head.truncate(path_len);
        let path = text(self.head, false)?;
        let message = text(self.message, self.truncated)?;
        let (severity, code, message) = marked(&message);
        if message.is_empty() {
            return None;
        }
        Some(Found {
            severity,
            code,
            message: message.to_owned(),
            message_truncated: self.truncated,
            path: Some(path),
            line: Some(self.line),
            column: Some(self.column),
        })
    }
}

/// `head` ends in `<path>:<line>:<column>: ` with a non-empty path:
/// the path length, line and column.
fn separator(head: &[u8]) -> Option<(usize, u32, u32)> {
    let rest = head.strip_suffix(b": ")?;
    let (rest, column) = trailing_number(rest)?;
    let (rest, line) = trailing_number(rest.strip_suffix(b":")?)?;
    let path = rest.strip_suffix(b":")?;
    (!path.is_empty()).then_some((path.len(), line, column))
}

fn trailing_number(bytes: &[u8]) -> Option<(&[u8], u32)> {
    let digits = bytes
        .iter()
        .rev()
        .take_while(|b| b.is_ascii_digit())
        .count();
    if !(1..=10).contains(&digits) {
        return None;
    }
    let (rest, digits) = bytes.split_at(bytes.len() - digits);
    let value = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some((rest, value))
}

/// Split an explicit severity marker (and `[code]`) off the message.
fn marked(message: &str) -> (Severity, Option<String>, &str) {
    const MARKERS: [(&str, Severity); 4] = [
        ("error", Severity::Error),
        ("warning", Severity::Warning),
        ("note", Severity::Note),
        ("help", Severity::Help),
    ];
    for (marker, severity) in MARKERS {
        let Some(rest) = message.strip_prefix(marker) else {
            continue;
        };
        if let Some(rest) = rest.strip_prefix(": ") {
            return (severity, None, rest);
        }
        if let Some((code, rest)) = rest.strip_prefix('[').and_then(|r| r.split_once("]: "))
            && !code.is_empty()
            && code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.' | b'-'))
        {
            return (severity, Some(code.to_owned()), rest);
        }
    }
    (Severity::Unknown, None, message)
}

// -------------------------------------------------- CargoCompilerMessageJson

/// A validating push scanner over one JSON line. It follows the structure
/// and keeps only the fields it needs, each bounded; everything else
/// (`rendered`, `children`, span `text`, `expansion`, …) is scanned past,
/// never kept. Any JSON error makes the line a miss.
#[derive(Debug, Default)]
struct CargoLine {
    state: Json,
    failed: bool,
    /// Open containers: is-object and what the container is.
    stack: Vec<(bool, Slot)>,
    /// Where the value being read goes.
    target: Target,
    scratch: Vec<u8>,
    scratch_over: bool,
    high_surrogate: Option<u16>,
    number: Option<u32>,
    compiler_message: bool,
    level: Option<Severity>,
    message: Option<(Vec<u8>, bool)>,
    code: Option<Vec<u8>>,
    span: Span,
    primary: Option<Span>,
}

#[derive(Debug, Default, Clone)]
struct Span {
    file_name: Option<Vec<u8>>,
    line: Option<u32>,
    column: Option<u32>,
    primary: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Json {
    /// The root object must open.
    #[default]
    Start,
    Value,
    /// Right after `[`.
    ValueOrEnd,
    /// Right after `{`.
    KeyOrEnd,
    Key,
    Colon,
    After,
    Str {
        key: bool,
        escape: Escape,
    },
    Number(Num),
    Literal(&'static [u8], usize),
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escape {
    None,
    Backslash,
    Hex(u8, u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Num {
    Minus,
    Zero,
    Int,
    Dot,
    Frac,
    E,
    ESign,
    Exp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Root,
    Diagnostic,
    Code,
    Spans,
    Span,
    Other,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Target {
    #[default]
    Skip,
    Reason,
    Diagnostic,
    Level,
    Message,
    Code,
    CodeText,
    Spans,
    Span,
    FileName,
    LineStart,
    ColumnStart,
    IsPrimary,
}

impl CargoLine {
    fn byte(&mut self, byte: u8) {
        if !self.failed && !self.step(byte) {
            self.failed = true;
        }
    }

    fn step(&mut self, byte: u8) -> bool {
        let whitespace = matches!(byte, b' ' | b'\t' | b'\r');
        match self.state {
            Json::Str { key, escape } => self.string(key, escape, byte),
            Json::Number(num) => match next_num(num, byte) {
                Some(num) => {
                    self.state = Json::Number(num);
                    if num == Num::Int || num == Num::Zero {
                        self.digit(byte);
                    } else {
                        self.number = None;
                    }
                    true
                }
                None if matches!(num, Num::Zero | Num::Int | Num::Frac | Num::Exp) => {
                    if let Some(value) = self.number.take() {
                        match self.target {
                            Target::LineStart => self.span.line = Some(value),
                            Target::ColumnStart => self.span.column = Some(value),
                            _ => {}
                        }
                    }
                    self.value_done();
                    self.step(byte)
                }
                None => false,
            },
            Json::Literal(literal, at) => {
                if literal[at] != byte {
                    return false;
                }
                if at + 1 < literal.len() {
                    self.state = Json::Literal(literal, at + 1);
                } else {
                    if self.target == Target::IsPrimary {
                        self.span.primary = literal == b"true";
                    }
                    self.value_done();
                }
                true
            }
            _ if whitespace => true,
            Json::Start => byte == b'{' && self.open(true),
            Json::Value => self.value(byte),
            Json::ValueOrEnd if byte == b']' => self.close(false),
            Json::ValueOrEnd => self.value(byte),
            Json::KeyOrEnd if byte == b'}' => self.close(true),
            Json::KeyOrEnd | Json::Key => {
                self.begin_string(true);
                byte == b'"'
            }
            Json::Colon => {
                self.state = Json::Value;
                byte == b':'
            }
            Json::After => {
                let object = self.stack.last().expect("inside a container").0;
                match byte {
                    b',' if object => {
                        self.state = Json::Key;
                        true
                    }
                    b',' => {
                        self.element_target();
                        self.state = Json::Value;
                        true
                    }
                    b'}' if object => self.close(true),
                    b']' if !object => self.close(false),
                    _ => false,
                }
            }
            Json::Done => false,
        }
    }

    fn value(&mut self, byte: u8) -> bool {
        match byte {
            b'{' => self.open(true),
            b'[' => self.open(false),
            b'"' => {
                self.begin_string(false);
                true
            }
            b'-' => {
                self.number = None;
                self.state = Json::Number(Num::Minus);
                true
            }
            b'0'..=b'9' => {
                self.number = Some(0);
                self.digit(byte);
                self.state = Json::Number(if byte == b'0' { Num::Zero } else { Num::Int });
                true
            }
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            _ => false,
        }
    }

    fn literal(&mut self, literal: &'static [u8]) -> bool {
        self.state = Json::Literal(literal, 1);
        true
    }

    fn digit(&mut self, byte: u8) {
        self.number = self
            .number
            .and_then(|value| value.checked_mul(10))
            .and_then(|value| value.checked_add(u32::from(byte - b'0')));
    }

    fn open(&mut self, object: bool) -> bool {
        if self.stack.len() == MAX_JSON_DEPTH {
            return false;
        }
        let slot = match (self.stack.is_empty(), object, self.target) {
            (true, _, _) => Slot::Root,
            (false, true, Target::Diagnostic) => Slot::Diagnostic,
            (false, true, Target::Code) => Slot::Code,
            (false, true, Target::Span) => {
                self.span = Span::default();
                Slot::Span
            }
            (false, false, Target::Spans) => Slot::Spans,
            _ => Slot::Other,
        };
        self.stack.push((object, slot));
        if object {
            self.state = Json::KeyOrEnd;
        } else {
            self.element_target();
            self.state = Json::ValueOrEnd;
        }
        true
    }

    fn close(&mut self, object: bool) -> bool {
        let Some((opened, slot)) = self.stack.pop() else {
            return false;
        };
        if opened != object {
            return false;
        }
        if slot == Slot::Span && self.span.primary && self.primary.is_none() {
            self.primary = Some(std::mem::take(&mut self.span));
        }
        self.value_done();
        true
    }

    fn element_target(&mut self) {
        self.target = match self.stack.last() {
            Some((false, Slot::Spans)) => Target::Span,
            _ => Target::Skip,
        };
    }

    fn value_done(&mut self) {
        self.state = if self.stack.is_empty() {
            Json::Done
        } else {
            Json::After
        };
    }

    fn begin_string(&mut self, key: bool) {
        self.scratch.clear();
        self.scratch_over = false;
        self.state = Json::Str {
            key,
            escape: Escape::None,
        };
    }

    fn keep(&mut self, byte: u8) {
        if !capture(&mut self.scratch, byte) {
            self.scratch_over = true;
        }
    }

    fn keep_char(&mut self, ch: char) {
        for &byte in ch.encode_utf8(&mut [0; 4]).as_bytes() {
            self.keep(byte);
        }
    }

    fn string(&mut self, key: bool, escape: Escape, byte: u8) -> bool {
        let set = |this: &mut Self, escape| {
            this.state = Json::Str { key, escape };
            true
        };
        match escape {
            Escape::None if self.high_surrogate.is_some() && byte != b'\\' => false,
            Escape::None => match byte {
                b'"' => {
                    self.string_done(key);
                    true
                }
                b'\\' => set(self, Escape::Backslash),
                0..=0x1f => false,
                _ => {
                    self.keep(byte);
                    true
                }
            },
            Escape::Backslash if self.high_surrogate.is_some() && byte != b'u' => false,
            Escape::Backslash => {
                let decoded = match byte {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'u' => return set(self, Escape::Hex(0, 0)),
                    _ => return false,
                };
                self.keep(decoded);
                set(self, Escape::None)
            }
            Escape::Hex(count, value) => {
                let Some(digit) = (byte as char).to_digit(16) else {
                    return false;
                };
                let value = (value << 4) | digit as u16;
                if count < 3 {
                    return set(self, Escape::Hex(count + 1, value));
                }
                let ch = match (self.high_surrogate.take(), value) {
                    (Some(high), 0xDC00..=0xDFFF) => char::from_u32(
                        0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(value) - 0xDC00),
                    ),
                    (Some(_), _) => None,
                    (None, 0xD800..=0xDBFF) => {
                        self.high_surrogate = Some(value);
                        return set(self, Escape::None);
                    }
                    (None, value) => char::from_u32(value.into()),
                };
                let Some(ch) = ch else {
                    return false;
                };
                self.keep_char(ch);
                set(self, Escape::None)
            }
        }
    }

    fn string_done(&mut self, key: bool) {
        let over = self.scratch_over;
        if key {
            self.state = Json::Colon;
            let slot = self.stack.last().expect("inside an object").1;
            self.target = if over {
                Target::Skip
            } else {
                match (slot, self.scratch.as_slice()) {
                    (Slot::Root, b"reason") => Target::Reason,
                    (Slot::Root, b"message") => Target::Diagnostic,
                    (Slot::Diagnostic, b"level") => Target::Level,
                    (Slot::Diagnostic, b"message") => Target::Message,
                    (Slot::Diagnostic, b"code") => Target::Code,
                    (Slot::Diagnostic, b"spans") => Target::Spans,
                    (Slot::Code, b"code") => Target::CodeText,
                    (Slot::Span, b"file_name") => Target::FileName,
                    (Slot::Span, b"line_start") => Target::LineStart,
                    (Slot::Span, b"column_start") => Target::ColumnStart,
                    (Slot::Span, b"is_primary") => Target::IsPrimary,
                    _ => Target::Skip,
                }
            };
            return;
        }
        match self.target {
            Target::Reason => {
                self.compiler_message = !over && self.scratch == b"compiler-message";
            }
            Target::Level => {
                self.level = Some(match self.scratch.as_slice() {
                    b"error" => Severity::Error,
                    b"warning" => Severity::Warning,
                    b"note" => Severity::Note,
                    b"help" => Severity::Help,
                    _ => Severity::Unknown,
                });
            }
            Target::Message => self.message = Some((std::mem::take(&mut self.scratch), over)),
            // A code or path is never cut short.
            Target::CodeText | Target::FileName if over => self.failed = true,
            Target::CodeText => self.code = Some(std::mem::take(&mut self.scratch)),
            Target::FileName => self.span.file_name = Some(std::mem::take(&mut self.scratch)),
            _ => {}
        }
        self.value_done();
    }

    fn end(self) -> Option<Found> {
        if self.failed || self.state != Json::Done || !self.compiler_message {
            return None;
        }
        let (message, truncated) = self.message?;
        let primary = self.primary.unwrap_or_default();
        Some(Found {
            severity: self.level?,
            code: match self.code {
                Some(code) => Some(text(code, false)?),
                None => None,
            },
            message: text(message, truncated)?,
            message_truncated: truncated,
            path: match primary.file_name {
                Some(path) => Some(text(path, false)?),
                None => None,
            },
            line: primary.line,
            column: primary.column,
        })
    }
}

fn next_num(num: Num, byte: u8) -> Option<Num> {
    let digit = byte.is_ascii_digit();
    let exponent = matches!(byte, b'e' | b'E');
    match num {
        Num::Minus if byte == b'0' => Some(Num::Zero),
        Num::Minus | Num::Int if digit => Some(Num::Int),
        Num::Zero | Num::Int if byte == b'.' => Some(Num::Dot),
        Num::Zero | Num::Int | Num::Frac if exponent => Some(Num::E),
        Num::Dot | Num::Frac if digit => Some(Num::Frac),
        Num::E if matches!(byte, b'+' | b'-') => Some(Num::ESign),
        Num::E | Num::ESign | Num::Exp if digit => Some(Num::Exp),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
