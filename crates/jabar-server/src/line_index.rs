//! Converting between LSP positions and byte offsets.
//!
//! LSP addresses text as a line number plus a character offset, where
//! "character" means whatever encoding was negotiated at startup — UTF-16 by
//! default, because that is what the protocol was originally specified against.
//! Everything below this layer counts bytes.
//!
//! On ASCII the three agree and any conversion looks correct. They come apart
//! exactly where the fixture's `Messages.java` lives: `🔁` is one codepoint, two
//! UTF-16 code units, and four UTF-8 bytes, so a line holding one is 63 bytes,
//! 59 UTF-16 units and 58 codepoints. A server that conflates them returns
//! ranges that are plausible, off by a character, and wrong in a way no type
//! checker catches.
//!
//! The representation follows rust-analyzer's: line starts, plus a per-line list
//! of the non-ASCII characters. Lines that are pure ASCII — nearly all of them —
//! carry no extra data and convert by identity.

use rustc_hash::FxHashMap;

/// Which unit an LSP `character` offset counts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    /// Byte offsets. Requires the client to have opted in during `initialize`.
    Utf8,
    /// UTF-16 code units. The protocol default, so the fallback.
    Utf16,
}

/// A zero-based line and character offset, as LSP sends them.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LinePosition {
    pub line: u32,
    pub character: u32,
}

impl LinePosition {
    pub fn new(line: u32, character: u32) -> LinePosition {
        LinePosition { line, character }
    }
}

/// A non-ASCII character, positioned within its line.
#[derive(Copy, Clone, Debug)]
struct WideChar {
    /// Byte offset from the start of the line.
    start: u32,
    /// Length in bytes: 2, 3 or 4.
    len: u32,
    /// Length in UTF-16 code units: 1, or 2 for anything outside the BMP.
    utf16_len: u32,
}

/// Line starts and wide-character positions for one file.
#[derive(Debug)]
pub struct LineIndex {
    /// Byte offset of the start of each line. Always begins with 0.
    line_starts: Vec<u32>,
    /// Non-ASCII characters, keyed by line. Absent for pure-ASCII lines.
    wide_chars: FxHashMap<u32, Vec<WideChar>>,
    len: u32,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut line_starts = vec![0u32];
        let mut wide_chars: FxHashMap<u32, Vec<WideChar>> = FxHashMap::default();

        let mut line = 0u32;
        let mut line_start = 0u32;
        let mut current_wide: Vec<WideChar> = Vec::new();

        for (offset, ch) in text.char_indices() {
            let offset = offset as u32;
            if ch == '\n' {
                if !current_wide.is_empty() {
                    wide_chars.insert(line, std::mem::take(&mut current_wide));
                }
                line += 1;
                line_start = offset + 1;
                line_starts.push(line_start);
                continue;
            }
            if !ch.is_ascii() {
                current_wide.push(WideChar {
                    start: offset - line_start,
                    len: ch.len_utf8() as u32,
                    utf16_len: ch.len_utf16() as u32,
                });
            }
        }
        if !current_wide.is_empty() {
            wide_chars.insert(line, current_wide);
        }

        LineIndex { line_starts, wide_chars, len: text.len() as u32 }
    }

    /// Number of lines. A file with no trailing newline still has a last line;
    /// a file ending in `\n` has an empty one after it.
    pub fn line_count(&self) -> u32 {
        self.line_starts.len() as u32
    }

    /// Byte offset for `position`, or `None` if it is past the end of the file.
    ///
    /// A character offset past the end of its line is clamped to the line's end
    /// rather than rejected: clients routinely send a column derived from a
    /// stale buffer, and refusing the whole request over it is worse than
    /// answering about the end of the line.
    pub fn offset(&self, position: LinePosition, encoding: PositionEncoding) -> Option<u32> {
        let line_start = *self.line_starts.get(position.line as usize)?;
        let line_end = self.line_end(position.line);
        let max_col = line_end - line_start;

        let col_bytes = match encoding {
            PositionEncoding::Utf8 => position.character,
            PositionEncoding::Utf16 => self.utf16_to_byte_col(position.line, position.character),
        };
        let col_bytes = self.snap_to_boundary(position.line, col_bytes.min(max_col));
        Some(line_start + col_bytes)
    }

    /// Moves a byte column back to the start of the character containing it.
    ///
    /// A client can address the inside of a character: under UTF-8 encoding the
    /// column *is* a byte offset, so nothing stops it naming byte 11 of a line
    /// whose `é` occupies bytes 10 and 11. Passing that through yields an offset
    /// that splits a character, and every column derived from it afterwards is
    /// wrong. Snapping backwards is the conservative repair — it never reaches
    /// past text the client meant to address.
    fn snap_to_boundary(&self, line: u32, col_bytes: u32) -> u32 {
        let Some(wide) = self.wide_chars.get(&line) else {
            // A pure-ASCII line has a boundary at every byte.
            return col_bytes;
        };
        match wide.iter().find(|ch| col_bytes > ch.start && col_bytes < ch.start + ch.len) {
            Some(ch) => ch.start,
            None => col_bytes,
        }
    }

    /// LSP position for a byte offset. Offsets past the end clamp to the end.
    pub fn position(&self, offset: u32, encoding: PositionEncoding) -> LinePosition {
        let offset = offset.min(self.len);
        // `partition_point` gives the first line starting after `offset`, so the
        // line containing it is one before.
        let line = self.line_starts.partition_point(|&start| start <= offset) as u32 - 1;
        let col_bytes = offset - self.line_starts[line as usize];

        let character = match encoding {
            PositionEncoding::Utf8 => col_bytes,
            PositionEncoding::Utf16 => self.byte_to_utf16_col(line, col_bytes),
        };
        LinePosition { line, character }
    }

    /// Byte offset just past the last character of `line`, excluding the newline.
    fn line_end(&self, line: u32) -> u32 {
        match self.line_starts.get(line as usize + 1) {
            // The next line starts after this one's `\n`, which is not part of it.
            Some(&next) => next - 1,
            None => self.len,
        }
    }

    fn byte_to_utf16_col(&self, line: u32, col_bytes: u32) -> u32 {
        let Some(wide) = self.wide_chars.get(&line) else {
            return col_bytes;
        };
        let mut col = col_bytes;
        for ch in wide {
            if ch.start >= col_bytes {
                break;
            }
            // Each wide char occupies `len` bytes but only `utf16_len` units.
            col -= ch.len - ch.utf16_len;
        }
        col
    }

    fn utf16_to_byte_col(&self, line: u32, col_utf16: u32) -> u32 {
        let Some(wide) = self.wide_chars.get(&line) else {
            return col_utf16;
        };
        let mut col = col_utf16;
        for ch in wide {
            // `ch.start` is a byte offset, and `col` is being converted into one
            // as we go, so the comparison is consistent.
            if ch.start >= col {
                break;
            }
            col += ch.len - ch.utf16_len;
        }
        col
    }
}

#[cfg(test)]
mod tests {
    use super::PositionEncoding::{Utf8, Utf16};
    use super::*;

    fn pos(line: u32, character: u32) -> LinePosition {
        LinePosition::new(line, character)
    }

    #[test]
    fn ascii_offsets_are_identical_in_both_encodings() {
        let index = LineIndex::new("class A {}\nclass B {}\n");
        for encoding in [Utf8, Utf16] {
            assert_eq!(index.offset(pos(0, 0), encoding), Some(0));
            assert_eq!(index.offset(pos(0, 6), encoding), Some(6));
            assert_eq!(index.offset(pos(1, 6), encoding), Some(17));
            assert_eq!(index.position(17, encoding), pos(1, 6));
        }
    }

    #[test]
    fn line_count_handles_trailing_newlines() {
        assert_eq!(LineIndex::new("").line_count(), 1, "an empty file is one empty line");
        assert_eq!(LineIndex::new("a").line_count(), 1);
        assert_eq!(LineIndex::new("a\n").line_count(), 2, "a trailing newline opens a line");
        assert_eq!(LineIndex::new("a\nb").line_count(), 2);
    }

    #[test]
    fn the_emoji_line_from_the_fixture() {
        // Verbatim from fixtures/megarepo Messages.java, the line that made the
        // three encodings disagree: 63 bytes, 59 UTF-16 units, 58 codepoints.
        let line = r#"  public static final String RETRY_BANNER = "🔁 retrying…";"#;
        assert_eq!(line.len(), 63);
        assert_eq!(line.encode_utf16().count(), 59);
        assert_eq!(line.chars().count(), 58);

        let index = LineIndex::new(line);
        let emoji_start = line.find('🔁').unwrap() as u32;

        // Before the emoji the encodings still agree.
        assert_eq!(index.position(emoji_start, Utf8), pos(0, emoji_start));
        assert_eq!(index.position(emoji_start, Utf16), pos(0, emoji_start));

        // Immediately after it they diverge by exactly the two extra bytes.
        let after = emoji_start + '🔁'.len_utf8() as u32;
        assert_eq!(index.position(after, Utf8), pos(0, after));
        assert_eq!(index.position(after, Utf16), pos(0, emoji_start + 2));

        // And the ellipsis, three bytes for one UTF-16 unit, adds two more.
        assert_eq!(index.position(line.len() as u32, Utf16), pos(0, 59));
        assert_eq!(index.position(line.len() as u32, Utf8), pos(0, 63));
    }

    #[test]
    fn conversions_round_trip_on_every_boundary() {
        // The property that matters: for every character boundary in a file full
        // of awkward text, offset(position(o)) == o. If this holds, no range the
        // server returns can be off by a character.
        let text = "class Grüße {\n  String s = \"🔁 完了\";\n  // Γειά σου\n}\n";
        let index = LineIndex::new(text);

        for (offset, _) in text.char_indices() {
            let offset = offset as u32;
            for encoding in [Utf8, Utf16] {
                let position = index.position(offset, encoding);
                assert_eq!(
                    index.offset(position, encoding),
                    Some(offset),
                    "round trip failed at byte {offset} under {encoding:?}"
                );
            }
        }
    }

    #[test]
    fn offsets_always_land_on_character_boundaries() {
        // The concrete failure this guards: a UTF-16 column used as a byte
        // offset splits a multi-byte character, and every column after is wrong.
        let text = "let x = \"héllo 🔁 wörld\";\n";
        let index = LineIndex::new(text);

        for character in 0..40 {
            for encoding in [Utf8, Utf16] {
                if let Some(offset) = index.offset(pos(0, character), encoding) {
                    assert!(
                        text.is_char_boundary(offset as usize),
                        "byte {offset} splits a character (col {character}, {encoding:?})"
                    );
                }
            }
        }
    }

    #[test]
    fn a_column_inside_a_character_snaps_back_to_its_start() {
        // Under UTF-8 encoding the column is a byte offset, so a client can name
        // the second byte of a two-byte character. `é` occupies bytes 1 and 2.
        let index = LineIndex::new("aébc");
        assert_eq!(index.offset(pos(0, 1), Utf8), Some(1), "the start of é");
        assert_eq!(index.offset(pos(0, 2), Utf8), Some(1), "inside é, snapped back");
        assert_eq!(index.offset(pos(0, 3), Utf8), Some(3), "past é, untouched");

        // Same for a surrogate pair addressed halfway through under UTF-16.
        let index = LineIndex::new("a🔁b");
        assert_eq!(index.offset(pos(0, 1), Utf16), Some(1), "the start of the emoji");
        assert_eq!(index.offset(pos(0, 2), Utf16), Some(1), "mid-pair, snapped back");
        assert_eq!(index.offset(pos(0, 3), Utf16), Some(5), "past it, untouched");
    }

    #[test]
    fn a_column_past_the_line_end_clamps() {
        // Clients send stale columns routinely. Clamping to the line end beats
        // refusing the request or running off into the next line.
        let index = LineIndex::new("ab\ncdef\n");
        assert_eq!(index.offset(pos(0, 999), Utf8), Some(2), "end of line 0, not into line 1");
        assert_eq!(index.offset(pos(1, 999), Utf8), Some(7), "end of line 1");
    }

    #[test]
    fn a_line_past_the_end_is_rejected() {
        // A missing line is a different kind of wrong from a long column: there
        // is no sensible offset to clamp to, so the caller has to know.
        let index = LineIndex::new("ab\n");
        assert_eq!(index.offset(pos(99, 0), Utf8), None);
        assert_eq!(index.offset(pos(1, 0), Utf8), Some(3), "the empty final line exists");
    }

    #[test]
    fn an_offset_past_the_end_clamps_to_the_end() {
        let text = "abc\n";
        let index = LineIndex::new(text);
        assert_eq!(index.position(9999, Utf8), index.position(text.len() as u32, Utf8));
    }

    #[test]
    fn empty_files_and_empty_lines() {
        let index = LineIndex::new("");
        assert_eq!(index.offset(pos(0, 0), Utf16), Some(0));
        assert_eq!(index.position(0, Utf16), pos(0, 0));

        let index = LineIndex::new("\n\n\n");
        assert_eq!(index.line_count(), 4);
        assert_eq!(index.offset(pos(2, 0), Utf8), Some(2));
        assert_eq!(index.position(2, Utf8), pos(2, 0));
    }

    #[test]
    fn wide_characters_on_later_lines_do_not_affect_earlier_ones() {
        // Wide-char data is per line; leaking it across lines would shift every
        // position in the file after the first non-ASCII character.
        let text = "plain ascii\nGrüße\nplain again\n";
        let index = LineIndex::new(text);
        assert_eq!(index.position(6, Utf16), pos(0, 6), "line 0 is untouched");

        let line2_start = text.find("plain again").unwrap() as u32;
        assert_eq!(index.position(line2_start + 5, Utf16), pos(2, 5), "line 2 is untouched");
    }

    #[test]
    fn crlf_line_endings_do_not_shift_columns() {
        // Text reaching the index is expected to be normalized, but a stray \r
        // must not be mistaken for part of the next line.
        let index = LineIndex::new("ab\r\ncd\r\n");
        assert_eq!(index.line_count(), 3);
        assert_eq!(index.offset(pos(1, 0), Utf8), Some(4), "line 1 starts after the \\n");
    }

    #[test]
    fn a_line_of_only_wide_characters() {
        let text = "🔁🔁🔁";
        let index = LineIndex::new(text);
        assert_eq!(index.position(text.len() as u32, Utf16), pos(0, 6), "three surrogate pairs");
        assert_eq!(index.position(text.len() as u32, Utf8), pos(0, 12));
        assert_eq!(index.offset(pos(0, 4), Utf16), Some(8), "two emoji in");
        assert_eq!(index.offset(pos(0, 6), Utf16), Some(12));
    }
}
