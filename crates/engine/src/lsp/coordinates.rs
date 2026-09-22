//! Exact conversion between Brainprint byte offsets and LSP positions.
//!
//! Brainprint's [`SourceSpan`](crate::parser::SourceSpan) is byte-based,
//! because tree-sitter is. LSP counts a `character` in code units of
//! whatever [`PositionEncoding`] the `initialize` handshake settled on,
//! and the two backends settled on different ones: Pyright answers no
//! `positionEncoding` at all, which under LSP means UTF-16 and nothing
//! else (#19 task 5 tried negotiating `utf-8` and the server declined),
//! while the TypeScript 7 native server accepts `utf-8` when the client
//! offers it first (#19 task 10 measured it choosing `utf-8`). So the
//! encoding is a parameter here, never an assumption -- and it is the
//! *negotiated* one, read back out of the handshake, not the one the
//! client asked for.
//!
//! Getting this wrong is not a visible failure. On the task 5 fixture
//! `pkg/wide.py`, one identifier sits at UTF-16 offset 18, codepoint
//! offset 16, and byte offset 26 on the same line. Sending the byte
//! offset does not miss -- it lands on a *different real symbol* and the
//! backend answers confidently about that one. A confident wrong target
//! is worse than no target, so every conversion here is exact and every
//! failure is an error: nothing is clamped to a line end, rounded to the
//! nearest boundary, or guessed.

use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

/// The code unit an LSP `character` counts, as `initialize` settled it.
///
/// LSP names three (`utf-8`, `utf-16`, `utf-32`); a server picks one
/// from the client's offered list and reports it back. UTF-16 is the
/// protocol default and the only one a server that answers nothing may
/// be assumed to mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PositionEncoding {
    /// Bytes. Brainprint's own unit, so the mapping is the identity on
    /// every character -- which is why the TS/JS backend offers it first.
    Utf8,
    /// The LSP default, and what a server that reports no encoding
    /// means.
    #[default]
    Utf16,
    /// Codepoints.
    Utf32,
}

impl PositionEncoding {
    /// The wire name, as it appears in `initialize`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }

    /// The encoding a wire name means, or `None` for one LSP does not
    /// define. An unknown name is never rounded to the default: a
    /// server naming an encoding this code cannot count in must not
    /// have its positions read as if it had named UTF-16.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "utf-8" => Some(Self::Utf8),
            "utf-16" => Some(Self::Utf16),
            "utf-32" => Some(Self::Utf32),
            _ => None,
        }
    }

    /// How many code units one character occupies.
    const fn units(self, character: char) -> usize {
        match self {
            Self::Utf8 => character.len_utf8(),
            Self::Utf16 => character.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

/// A zero-based LSP position: a line, and an offset into it counted in
/// UTF-16 code units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    #[must_use]
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

/// A zero-based LSP range, end-exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    #[must_use]
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// Whether the range names no text at all. Pyright uses `0:0-0:0`
    /// for "the whole file", which is a module target, not a symbol one.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Why a coordinate could not be converted.
///
/// Every variant is a refusal. None of them carries a "closest" answer,
/// because a closest answer is the failure mode this module exists to
/// prevent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinateError {
    /// The line does not exist in this text.
    LineOutOfRange {
        line: u32,
        lines: usize,
    },
    /// The character offset runs past the end of its line.
    CharacterOutOfRange {
        line: u32,
        character: u32,
        line_units: u32,
    },
    /// The offset falls inside one character rather than between two
    /// -- between the halves of a surrogate pair under UTF-16, or
    /// between the bytes of a multi-byte character under UTF-8. Not a
    /// position either way.
    SplitCodeUnit {
        line: u32,
        character: u32,
    },
    ByteOutOfRange {
        byte: usize,
        len: usize,
    },
    /// The byte is inside a multi-byte character.
    NotCharBoundary {
        byte: usize,
    },
    /// A range whose end precedes its start.
    InvertedRange {
        start: Position,
        end: Position,
    },
}

impl fmt::Display for CoordinateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineOutOfRange { line, lines } => {
                write!(formatter, "line {line} is past the last line ({lines})")
            }
            Self::CharacterOutOfRange {
                line,
                character,
                line_units,
            } => write!(
                formatter,
                "character {character} is past the end of line {line} ({line_units} units)"
            ),
            Self::SplitCodeUnit { line, character } => write!(
                formatter,
                "character {character} on line {line} splits a character"
            ),
            Self::ByteOutOfRange { byte, len } => {
                write!(formatter, "byte {byte} is past the end of source ({len})")
            }
            Self::NotCharBoundary { byte } => {
                write!(formatter, "byte {byte} is not a character boundary")
            }
            Self::InvertedRange { start, end } => {
                write!(formatter, "range end {end:?} precedes its start {start:?}")
            }
        }
    }
}

impl Error for CoordinateError {}

/// One source text, indexed by line, for converting positions both ways.
///
/// Built once per Resource per batch. The text is borrowed: this never
/// owns or stores source, and nothing here is persisted.
#[derive(Debug)]
pub struct LineMap<'a> {
    text: &'a str,
    /// Byte offset where each line begins. Always at least one entry.
    starts: Vec<usize>,
    encoding: PositionEncoding,
}

impl<'a> LineMap<'a> {
    /// A map over `text` in the LSP default encoding, UTF-16.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        Self::with_encoding(text, PositionEncoding::Utf16)
    }

    /// A map over `text` in the encoding the handshake settled on.
    #[must_use]
    pub fn with_encoding(text: &'a str, encoding: PositionEncoding) -> Self {
        let mut starts = vec![0];
        starts.extend(
            text.match_indices('\n')
                .map(|(index, separator)| index + separator.len()),
        );
        Self {
            text,
            starts,
            encoding,
        }
    }

    /// The encoding this map counts `character` offsets in.
    #[must_use]
    pub const fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    #[must_use]
    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// The line's text without its terminator.
    ///
    /// A `\r\n` line ends at the `\r`: an LSP character offset counts
    /// the line's content, and the carriage return is part of the
    /// separator, not of the content. A lone `\r` is left alone -- LSP
    /// allows it as a separator but both backends report lines split on
    /// `\n` (#19 tasks 5 and 10 measured it), and inventing a second
    /// line here would move every offset after it.
    fn line_text(&self, line: usize) -> Option<&'a str> {
        let start = *self.starts.get(line)?;
        let end = self
            .starts
            .get(line + 1)
            .map_or(self.text.len(), |next| next - 1);
        let raw = &self.text[start..end];
        Some(raw.strip_suffix('\r').unwrap_or(raw))
    }

    /// The LSP position of a byte offset.
    pub fn position(&self, byte: usize) -> Result<Position, CoordinateError> {
        if byte > self.text.len() {
            return Err(CoordinateError::ByteOutOfRange {
                byte,
                len: self.text.len(),
            });
        }
        if !self.text.is_char_boundary(byte) {
            return Err(CoordinateError::NotCharBoundary { byte });
        }
        // The last line whose start is at or before `byte`.
        let line = self.starts.partition_point(|start| *start <= byte) - 1;
        let start = self.starts[line];
        let character: usize = self.text[start..byte]
            .chars()
            .map(|character| self.encoding.units(character))
            .sum();
        Ok(Position {
            line: u32::try_from(line).unwrap_or(u32::MAX),
            character: u32::try_from(character).unwrap_or(u32::MAX),
        })
    }

    /// The byte offset of an LSP position.
    pub fn byte(&self, position: Position) -> Result<usize, CoordinateError> {
        let line = usize::try_from(position.line).unwrap_or(usize::MAX);
        let Some(text) = self.line_text(line) else {
            return Err(CoordinateError::LineOutOfRange {
                line: position.line,
                lines: self.starts.len(),
            });
        };
        let start = self.starts[line];
        let wanted = usize::try_from(position.character).unwrap_or(usize::MAX);

        let mut units = 0_usize;
        for (offset, character) in text.char_indices() {
            if units == wanted {
                return Ok(start + offset);
            }
            let next = units + self.encoding.units(character);
            if next > wanted {
                // `wanted` points inside `character` rather than at a
                // boundary between two.
                return Err(CoordinateError::SplitCodeUnit {
                    line: position.line,
                    character: position.character,
                });
            }
            units = next;
        }
        if units == wanted {
            return Ok(start + text.len());
        }
        Err(CoordinateError::CharacterOutOfRange {
            line: position.line,
            character: position.character,
            line_units: u32::try_from(units).unwrap_or(u32::MAX),
        })
    }

    /// The byte span of an LSP range.
    pub fn span(&self, range: Range) -> Result<(usize, usize), CoordinateError> {
        let start = self.byte(range.start)?;
        let end = self.byte(range.end)?;
        if end < start {
            return Err(CoordinateError::InvertedRange {
                start: range.start,
                end: range.end,
            });
        }
        Ok((start, end))
    }

    /// The LSP range of a byte span.
    pub fn range(&self, start: usize, end: usize) -> Result<Range, CoordinateError> {
        if end < start {
            let start = self.position(start)?;
            let end = self.position(end)?;
            return Err(CoordinateError::InvertedRange { start, end });
        }
        Ok(Range {
            start: self.position(start)?,
            end: self.position(end)?,
        })
    }

    /// The position *inside* the last character of a byte span.
    ///
    /// Not cosmetic. I3 records a call site at the whole callee
    /// expression, so `x.run(1)` gives the span of `x.run`; asking
    /// Pyright at its start returns the definition of `x`, the receiver
    /// -- a real, wrong answer. The last character is always inside the
    /// final identifier of a dotted name, for `x.run`, for `.base`, and
    /// for a bare `Base` alike.
    pub fn last_character_position(
        &self,
        start: usize,
        end: usize,
    ) -> Result<Position, CoordinateError> {
        if end <= start {
            return self.position(start);
        }
        if end > self.text.len() {
            return Err(CoordinateError::ByteOutOfRange {
                byte: end,
                len: self.text.len(),
            });
        }
        let last = self.text[start..end]
            .char_indices()
            .next_back()
            .map_or(start, |(offset, _)| start + offset);
        self.position(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_round_trips_at_every_boundary() {
        let text = "class Base:\n    def run(self):\n        pass\n";
        let map = LineMap::new(text);
        for (byte, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            let position = map.position(byte).expect("position");
            assert_eq!(map.byte(position), Ok(byte), "byte {byte}");
        }
    }

    #[test]
    fn korean_round_trips_and_is_not_a_byte_offset() {
        // Every Hangul syllable is 3 UTF-8 bytes and 1 UTF-16 unit.
        let text = "변수 = \"한글\"\n결과 = 변수\n";
        let map = LineMap::new(text);
        for (byte, _) in text.char_indices() {
            let position = map.position(byte).expect("position");
            assert_eq!(map.byte(position), Ok(byte));
        }

        let second_use = text.rfind("변수").expect("second 변수");
        let position = map.position(second_use).expect("position");
        assert_eq!(position, Position::new(1, 5));
        assert_ne!(
            usize::try_from(position.character).expect("fits"),
            second_use - text.find('\n').expect("newline") - 1,
            "the UTF-16 offset is not the byte offset into the line"
        );
    }

    #[test]
    fn supplementary_plane_costs_two_code_units() {
        let text = "값 = \"🐍🐍\" + 파라미터\n";
        let map = LineMap::new(text);
        let identifier = text.find("파라미터").expect("identifier");

        let position = map.position(identifier).expect("position");
        // 값(1) ␣(1) =(1) ␣(1) "(1) 🐍(2) 🐍(2) "(1) ␣(1) +(1) ␣(1)
        assert_eq!(position.character, 13);
        assert_eq!(map.byte(position), Ok(identifier));

        // Codepoint counting would say 11; byte counting would say 21.
        let codepoints = text[..identifier].chars().count();
        assert_eq!(codepoints, 11);
        assert_eq!(identifier, 19, "and the byte offset is a third value again");
    }

    #[test]
    fn splitting_a_surrogate_pair_is_rejected_not_rounded() {
        let text = "🐍x\n";
        let map = LineMap::new(text);
        assert_eq!(map.byte(Position::new(0, 0)), Ok(0));
        assert_eq!(
            map.byte(Position::new(0, 1)),
            Err(CoordinateError::SplitCodeUnit {
                line: 0,
                character: 1
            })
        );
        assert_eq!(map.byte(Position::new(0, 2)), Ok(4));
    }

    #[test]
    fn the_task_five_wrong_offset_case_cannot_reach_the_backend() {
        // pkg/wide.py line 7 verbatim. The three conventions disagree,
        // and task 5 proved the byte offset resolves to a different real
        // symbol. Feeding a byte offset in as a character offset must
        // be caught here rather than answered.
        let line = "    결과 = \"🐍🐍\" + 파라미터.run(4)";
        let text = format!("{line}\n");
        let map = LineMap::new(&text);
        let identifier = line.find("파라미터").expect("identifier");

        let correct = map.position(identifier).expect("position");
        assert_eq!(correct.character, 18);
        assert_eq!(line[..identifier].chars().count(), 16);
        assert_eq!(identifier, 26);

        // Each wrong convention maps to a different byte, so it would
        // have asked about a different token.
        let by_codepoint = map.byte(Position::new(0, 16)).expect("in range");
        let by_byte = map.byte(Position::new(0, 26)).expect("in range");
        assert_ne!(by_codepoint, identifier);
        assert_ne!(by_byte, identifier);
        assert_eq!(map.byte(correct), Ok(identifier));

        // And the returned range only decodes to the identifier under
        // UTF-16.
        let range = Range::new(correct, Position::new(0, 22));
        assert_eq!(
            map.span(range).map(|(start, end)| &line[start..end]),
            Ok("파라미터")
        );
    }

    #[test]
    fn out_of_range_is_rejected_never_clamped() {
        let text = "ab\ncd\n";
        let map = LineMap::new(text);
        assert_eq!(
            map.byte(Position::new(0, 3)),
            Err(CoordinateError::CharacterOutOfRange {
                line: 0,
                character: 3,
                line_units: 2
            })
        );
        assert_eq!(
            map.byte(Position::new(9, 0)),
            Err(CoordinateError::LineOutOfRange { line: 9, lines: 3 })
        );
        assert_eq!(
            map.position(99),
            Err(CoordinateError::ByteOutOfRange { byte: 99, len: 6 })
        );
        let korean = LineMap::new("변수\n");
        assert_eq!(
            korean.position(1),
            Err(CoordinateError::NotCharBoundary { byte: 1 }),
            "a byte inside a character is not a position"
        );
        assert_eq!(korean.position(3), Ok(Position::new(0, 1)));
    }

    #[test]
    fn end_of_line_and_end_of_file_are_positions() {
        let text = "ab\ncd";
        let map = LineMap::new(text);
        assert_eq!(map.byte(Position::new(0, 2)), Ok(2));
        assert_eq!(map.byte(Position::new(1, 2)), Ok(5));
        assert_eq!(map.position(5), Ok(Position::new(1, 2)));
        // A trailing newline opens a real, empty last line.
        let trailing = LineMap::new("ab\n");
        assert_eq!(trailing.line_count(), 2);
        assert_eq!(trailing.position(3), Ok(Position::new(1, 0)));
    }

    #[test]
    fn crlf_line_endings_keep_byte_and_character_in_step() {
        let text = "ab\r\ncd\r\n";
        let map = LineMap::new(text);
        assert_eq!(map.position(0), Ok(Position::new(0, 0)));
        // The carriage return is a separator, so the line is two units
        // wide and its end is at the `\r`.
        assert_eq!(map.byte(Position::new(0, 2)), Ok(2));
        assert_eq!(
            map.byte(Position::new(0, 3)),
            Err(CoordinateError::CharacterOutOfRange {
                line: 0,
                character: 3,
                line_units: 2
            })
        );
        assert_eq!(map.byte(Position::new(1, 0)), Ok(4));
        assert_eq!(map.position(4), Ok(Position::new(1, 0)));
    }

    #[test]
    fn last_character_of_a_dotted_callee_is_inside_its_final_name() {
        let text = "    return x.run(1)\n";
        let map = LineMap::new(text);
        let start = text.find("x.run").expect("callee");
        let end = start + "x.run".len();

        let position = map.last_character_position(start, end).expect("position");
        let byte = map.byte(position).expect("byte");
        assert_eq!(&text[byte..byte + 1], "n");
        assert!(byte > start + 2, "inside `run`, not on the receiver");
    }

    #[test]
    fn last_character_handles_non_ascii_and_empty_spans() {
        let text = "파라미터.run\n";
        let map = LineMap::new(text);
        let end = text.find('\n').expect("newline");
        let position = map.last_character_position(0, end).expect("position");
        assert_eq!(map.byte(position), Ok(end - 1));

        assert_eq!(map.last_character_position(3, 3), map.position(3));
    }

    #[test]
    fn inverted_ranges_are_refused() {
        let map = LineMap::new("abcd\n");
        let range = Range::new(Position::new(0, 3), Position::new(0, 1));
        assert!(matches!(
            map.span(range),
            Err(CoordinateError::InvertedRange { .. })
        ));
    }

    #[test]
    fn utf8_positions_are_byte_offsets_on_every_script() {
        // The TS/JS backend negotiates utf-8 (#19 task 10), so a
        // `character` is a byte offset into the line and the mapping
        // must be the identity -- including across Hangul and an
        // astral-plane emoji, where the UTF-16 answer differs.
        let text = "const 한글 = \"🎈\";\nconst after = 1;\n";
        let map = LineMap::with_encoding(text, PositionEncoding::Utf8);
        assert_eq!(map.encoding(), PositionEncoding::Utf8);
        for (byte, _) in text.char_indices() {
            let position = map.position(byte).expect("position");
            assert_eq!(map.byte(position), Ok(byte), "byte {byte}");
            let line_start = text[..byte].rfind('\n').map_or(0, |index| index + 1);
            assert_eq!(
                usize::try_from(position.character).expect("fits"),
                byte - line_start,
                "utf-8 character must be a byte offset at {byte}"
            );
        }
    }

    #[test]
    fn the_three_encodings_disagree_on_the_same_identifier() {
        // The whole reason the encoding is a parameter: one identifier,
        // three different `character` offsets. Reading a position in
        // the wrong one lands on a different real symbol.
        let text = "const 한글 = \"🎈\"; const target = 1;\n";
        let byte = text.find("target").expect("needle");
        let at = |encoding| {
            LineMap::with_encoding(text, encoding)
                .position(byte)
                .expect("position")
                .character
        };
        let (utf8, utf16, utf32) = (
            at(PositionEncoding::Utf8),
            at(PositionEncoding::Utf16),
            at(PositionEncoding::Utf32),
        );
        assert_eq!(utf8, 29);
        assert_eq!(utf16, 23);
        assert_eq!(utf32, 22);
    }

    #[test]
    fn a_utf8_offset_inside_a_multibyte_character_is_refused() {
        let text = "const 한 = 1;\n";
        let map = LineMap::with_encoding(text, PositionEncoding::Utf8);
        // `한` starts at byte 6 and is three bytes wide; 7 is inside it.
        assert_eq!(
            map.byte(Position::new(0, 7)),
            Err(CoordinateError::SplitCodeUnit {
                line: 0,
                character: 7,
            })
        );
    }

    #[test]
    fn an_encoding_lsp_does_not_define_is_not_rounded_to_the_default() {
        assert_eq!(
            PositionEncoding::parse("utf-8"),
            Some(PositionEncoding::Utf8)
        );
        assert_eq!(
            PositionEncoding::parse("utf-16"),
            Some(PositionEncoding::Utf16)
        );
        assert_eq!(
            PositionEncoding::parse("utf-32"),
            Some(PositionEncoding::Utf32)
        );
        assert_eq!(PositionEncoding::parse("UTF-8"), None);
        assert_eq!(PositionEncoding::parse("latin-1"), None);
        assert_eq!(PositionEncoding::default(), PositionEncoding::Utf16);
    }
}
