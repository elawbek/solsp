//! `LineIndex`: convert between rowan byte offsets and LSP `{line, character}`
//! positions. LSP columns are **UTF-16** code units, so non-ASCII text (emoji in
//! comments, unicode string literals) shifts `character` — a classic source of
//! off-by-N bugs. Built once per document (design §4).

use rowan::{TextRange, TextSize};

/// A zero-based line/column. `col` is a UTF-16 code-unit offset within the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// Maps byte offsets <-> line/UTF-16-column for one document.
///
/// Construction scans the text once. Queries use binary searches over line starts
/// and non-ASCII character boundaries, with no source-text copy or line rescanning.
#[derive(Debug, Clone)]
pub struct LineIndex {
    lines: Vec<Line>,
    non_ascii: Vec<NonAsciiChar>,
}

/// Global byte/UTF-16 starts and content ends (excluding LF/CRLF).
#[derive(Debug, Clone, Copy)]
struct Line {
    byte_start: u32,
    utf16_start: u32,
    byte_end: u32,
    utf16_end: u32,
}

/// At each non-ASCII boundary, byte_end - utf16_end is the cumulative difference
/// between byte offsets and UTF-16 offsets. ASCII runs need no entries.
#[derive(Debug, Clone, Copy)]
struct NonAsciiChar {
    byte_end: u32,
    utf16_end: u32,
    byte_len: u8,
    utf16_len: u8,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut lines = Vec::new();
        let mut non_ascii = Vec::new();
        let mut byte_start = 0;
        let mut utf16_start = 0;
        let mut utf16 = 0;
        for (byte, ch) in text.char_indices() {
            let byte = byte as u32;
            if ch == '\n' {
                let crlf = byte > 0 && text.as_bytes()[byte as usize - 1] == b'\r';
                lines.push(Line {
                    byte_start,
                    utf16_start,
                    byte_end: byte - u32::from(crlf),
                    utf16_end: utf16 - u32::from(crlf),
                });
                byte_start = byte + 1;
                utf16_start = utf16 + 1;
            }
            utf16 += ch.len_utf16() as u32;
            if !ch.is_ascii() {
                non_ascii.push(NonAsciiChar {
                    byte_end: byte + ch.len_utf8() as u32,
                    utf16_end: utf16,
                    byte_len: ch.len_utf8() as u8,
                    utf16_len: ch.len_utf16() as u8,
                });
            }
        }
        // Always retain the final line, including an empty document/trailing LF.
        lines.push(Line {
            byte_start,
            utf16_start,
            byte_end: text.len() as u32,
            utf16_end: utf16,
        });
        LineIndex { lines, non_ascii }
    }

    /// Byte offset -> line/UTF-16-column. Offsets past EOF clamp to the text end.
    /// In-range offsets must be UTF-8 character boundaries.
    pub fn line_col(&self, offset: TextSize) -> LineCol {
        let offset = u32::from(offset).min(self.lines.last().unwrap().byte_end);
        let line = self
            .lines
            .partition_point(|entry| entry.byte_start <= offset)
            - 1;
        LineCol {
            line: line as u32,
            col: self.utf16_offset(offset) - self.lines[line].utf16_start,
        }
    }

    fn utf16_offset(&self, offset: u32) -> u32 {
        let completed = self.non_ascii.partition_point(|ch| ch.byte_end <= offset);
        if let Some(next) = self.non_ascii.get(completed) {
            assert!(
                offset <= next.byte_end - u32::from(next.byte_len),
                "offset is inside a UTF-8 character"
            );
        }
        let correction = completed
            .checked_sub(1)
            .map(|i| self.non_ascii[i].byte_end - self.non_ascii[i].utf16_end)
            .unwrap_or(0);
        offset - correction
    }

    /// Line/UTF-16-column -> byte offset. `None` if the line is past EOF; a `col`
    /// past the line's content clamps to the line's end (LSP tolerates over-range
    /// positions); a `col` that lands mid-surrogate clamps forward to the next char.
    pub fn offset(&self, line_col: LineCol) -> Option<TextSize> {
        let line = self.lines.get(line_col.line as usize)?;
        let utf16 = line.utf16_start + line_col.col.min(line.utf16_end - line.utf16_start);
        let completed = self.non_ascii.partition_point(|ch| ch.utf16_end <= utf16);
        if let Some(next) = self.non_ascii.get(completed) {
            if utf16 > next.utf16_end - u32::from(next.utf16_len) {
                return Some(TextSize::from(next.byte_end));
            }
        }
        let correction = completed
            .checked_sub(1)
            .map(|i| self.non_ascii[i].byte_end - self.non_ascii[i].utf16_end)
            .unwrap_or(0);
        Some(TextSize::from(utf16 + correction))
    }

    /// Update after replacing an old byte range with `replacement`. The range
    /// must be on old UTF-8 boundaries and `new_text` must be the resulting text.
    /// Scan only the inserted text; shift existing suffix metadata without
    /// rescanning unchanged lines. Refresh adjacent content ends for CRLF joins.
    pub fn apply_edit(&mut self, range: TextRange, replacement: &str, new_text: &str) {
        let start = u32::from(range.start());
        let end = u32::from(range.end());
        assert!(end <= self.lines.last().unwrap().byte_end);
        let start_utf16 = self.utf16_offset(start);
        let end_utf16 = self.utf16_offset(end);
        let inserted = Self::new(replacement);
        let byte_delta = replacement.len() as i64 - i64::from(end - start);
        let utf16_delta = i64::from(inserted.lines.last().unwrap().utf16_end)
            - i64::from(end_utf16 - start_utf16);
        let total_utf16 = shift(self.lines.last().unwrap().utf16_end, utf16_delta);
        assert_eq!(
            new_text.len() as i64,
            i64::from(self.lines.last().unwrap().byte_end) + byte_delta
        );

        let first_line = self.lines.partition_point(|line| line.byte_start <= start) - 1;
        let suffix_line = self.lines.partition_point(|line| line.byte_start <= end);
        if byte_delta != 0 || utf16_delta != 0 {
            for line in &mut self.lines[suffix_line..] {
                line.byte_start = shift(line.byte_start, byte_delta);
                line.byte_end = shift(line.byte_end, byte_delta);
                line.utf16_start = shift(line.utf16_start, utf16_delta);
                line.utf16_end = shift(line.utf16_end, utf16_delta);
            }
        }
        let added_lines = inserted.lines.len() - 1;
        self.lines.splice(
            first_line + 1..suffix_line,
            inserted.lines.into_iter().skip(1).map(|line| Line {
                byte_start: start + line.byte_start,
                utf16_start: start_utf16 + line.utf16_start,
                byte_end: start + line.byte_end,
                utf16_end: start_utf16 + line.utf16_end,
            }),
        );

        let first_char = self.non_ascii.partition_point(|ch| ch.byte_end <= start);
        let suffix_char = self.non_ascii.partition_point(|ch| ch.byte_end <= end);
        if byte_delta != 0 || utf16_delta != 0 {
            for ch in &mut self.non_ascii[suffix_char..] {
                ch.byte_end = shift(ch.byte_end, byte_delta);
                ch.utf16_end = shift(ch.utf16_end, utf16_delta);
            }
        }
        self.non_ascii.splice(
            first_char..suffix_char,
            inserted.non_ascii.into_iter().map(|ch| NonAsciiChar {
                byte_end: start + ch.byte_end,
                utf16_end: start_utf16 + ch.utf16_end,
                ..ch
            }),
        );

        for line in first_line..=first_line + added_lines {
            let (byte_end, utf16_end) = if let Some(next) = self.lines.get(line + 1) {
                let lf = next.byte_start - 1;
                let crlf = lf > 0 && new_text.as_bytes()[lf as usize - 1] == b'\r';
                (lf - u32::from(crlf), next.utf16_start - 1 - u32::from(crlf))
            } else {
                (new_text.len() as u32, total_utf16)
            };
            self.lines[line].byte_end = byte_end;
            self.lines[line].utf16_end = utf16_end;
        }
    }
}

fn shift(value: u32, delta: i64) -> u32 {
    u32::try_from(i64::from(value) + delta).expect("coordinate outside u32 range")
}

#[cfg(test)]
mod tests {
    use super::*;
    use solsp_syntax::parse;

    fn assert_same_coordinates(index: &LineIndex, text: &str) {
        let rebuilt = LineIndex::new(text);
        for byte in text
            .char_indices()
            .map(|(byte, _)| byte)
            .chain([text.len()])
        {
            assert_eq!(
                index.line_col(TextSize::from(byte as u32)),
                rebuilt.line_col(TextSize::from(byte as u32)),
                "{text:?} at byte {byte}"
            );
        }
        for (line, entry) in rebuilt.lines.iter().enumerate() {
            for col in (0..=entry.utf16_end - entry.utf16_start + 2).chain([u32::MAX]) {
                let position = LineCol {
                    line: line as u32,
                    col,
                };
                assert_eq!(
                    index.offset(position),
                    rebuilt.offset(position),
                    "{text:?} at {position:?}"
                );
            }
        }
        assert_eq!(
            index.offset(LineCol {
                line: rebuilt.lines.len() as u32,
                col: 0
            }),
            None
        );
    }

    #[test]
    fn incremental_index_matches_rebuild_for_all_small_edits() {
        for original in [
            "",
            "abc",
            "\n",
            "\r",
            "\r\n",
            "a\r\nb\n",
            "é😀\r\nx🌍",
            "a\r\r\nb",
        ] {
            let boundaries: Vec<_> = original
                .char_indices()
                .map(|(byte, _)| byte)
                .chain([original.len()])
                .collect();
            for (i, &start) in boundaries.iter().enumerate() {
                for &end in &boundaries[i..] {
                    for replacement in ["", "x", "\n", "\r", "\r\n", "😀", "\n🌍\r\n"] {
                        let mut index = LineIndex::new(original);
                        let mut text = original.to_string();
                        text.replace_range(start..end, replacement);
                        index.apply_edit(
                            TextRange::new((start as u32).into(), (end as u32).into()),
                            replacement,
                            &text,
                        );
                        assert_same_coordinates(&index, &text);
                    }
                }
            }
        }
    }

    #[test]
    fn incremental_index_remains_consistent_across_edit_sequences() {
        let mut text = "é\r\n😀 next\nlast".to_string();
        let mut index = LineIndex::new(&text);
        let replacements = ["", "abc", "\n", "\r", "\r\n", "🌍é", "x\n😀\r\ny"];
        for i in 0..200 {
            let boundaries: Vec<_> = text
                .char_indices()
                .map(|(byte, _)| byte)
                .chain([text.len()])
                .collect();
            let a = (i * 17) % boundaries.len();
            let b = (i * 31 + 1) % boundaries.len();
            let (start, end) = (boundaries[a.min(b)], boundaries[a.max(b)]);
            let replacement = replacements[i % replacements.len()];
            text.replace_range(start..end, replacement);
            index.apply_edit(
                TextRange::new((start as u32).into(), (end as u32).into()),
                replacement,
                &text,
            );
            assert_same_coordinates(&index, &text);
        }
    }

    #[test]
    fn ascii_crlf_and_trailing_newline() {
        // bytes: a0 b1 \r2 \n3 c4 d5 \n6   ⇒ line_starts = [0, 4, 7]
        let li = LineIndex::new("ab\r\ncd\n");
        assert_eq!(li.line_col(TextSize::from(0)), LineCol { line: 0, col: 0 });
        assert_eq!(li.line_col(TextSize::from(2)), LineCol { line: 0, col: 2 }); // the '\r'
        assert_eq!(li.line_col(TextSize::from(4)), LineCol { line: 1, col: 0 });
        assert_eq!(li.line_col(TextSize::from(6)), LineCol { line: 1, col: 2 });
        assert_eq!(li.line_col(TextSize::from(7)), LineCol { line: 2, col: 0 }); // empty final line @ EOF
        assert_eq!(
            li.offset(LineCol { line: 1, col: 0 }),
            Some(TextSize::from(4))
        );
        assert_eq!(li.offset(LineCol { line: 5, col: 0 }), None); // line past EOF
    }

    #[test]
    fn oversized_columns_stop_before_line_endings() {
        for ending in ["\n", "\r\n"] {
            for content in ["", "abc", "é🌍"] {
                let text = format!("{content}{ending}next{ending}");
                let index = LineIndex::new(&text);
                let utf16_len = content.encode_utf16().count() as u32;
                for col in [utf16_len, utf16_len + 1, u32::MAX] {
                    assert_eq!(
                        index.offset(LineCol { line: 0, col }),
                        Some(TextSize::from(content.len() as u32)),
                        "{text:?}, column {col}"
                    );
                }
                assert_eq!(
                    index.offset(LineCol {
                        line: 1,
                        col: u32::MAX
                    }),
                    Some(TextSize::from((content.len() + ending.len() + 4) as u32))
                );
                assert_eq!(
                    index.offset(LineCol {
                        line: 2,
                        col: u32::MAX
                    }),
                    Some(TextSize::from(text.len() as u32))
                );
                assert_eq!(index.offset(LineCol { line: 3, col: 0 }), None);
            }
        }
        for text in ["", "plain", "é🌍"] {
            assert_eq!(
                LineIndex::new(text).offset(LineCol {
                    line: 0,
                    col: u32::MAX
                }),
                Some(TextSize::from(text.len() as u32))
            );
        }
        // Preserve the existing forward clamp inside a UTF-16 surrogate pair.
        assert_eq!(
            LineIndex::new("🌍\r\nnext").offset(LineCol { line: 0, col: 1 }),
            Some(TextSize::from(4))
        );
    }

    #[test]
    fn indexed_coordinates_match_character_walks() {
        for text in [
            "",
            "ascii",
            "\n\r\n\n",
            "a\r\nb\nlast",
            "last\r",
            "é€🌍𐐀a\r\n🌍é\n\n終😀",
            "a🌍b🌍c",
            "\u{0000}é\r\nx",
        ] {
            let index = LineIndex::new(text);
            for offset in text
                .char_indices()
                .map(|(offset, _)| offset)
                .chain([text.len()])
            {
                let before = &text[..offset];
                let expected = LineCol {
                    line: before.bytes().filter(|&b| b == b'\n').count() as u32,
                    col: before.rsplit('\n').next().unwrap().encode_utf16().count() as u32,
                };
                assert_eq!(
                    index.line_col(TextSize::from(offset as u32)),
                    expected,
                    "{text:?} at {offset}"
                );
            }
            assert_eq!(
                index.line_col(TextSize::from(u32::MAX)),
                index.line_col(TextSize::from(text.len() as u32))
            );
            let mut start = 0;
            let mut line = 0;
            loop {
                let next = text[start..].find('\n').map(|i| start + i + 1);
                let raw = &text[start..next.unwrap_or(text.len())];
                let content = if let Some(s) = raw.strip_suffix('\n') {
                    s.strip_suffix('\r').unwrap_or(s)
                } else {
                    raw
                };
                let width = content.encode_utf16().count() as u32;
                for col in (0..=width + 2).chain([u32::MAX]) {
                    let mut expected = start;
                    let mut units = 0;
                    for ch in content.chars() {
                        if units >= col {
                            break;
                        }
                        units += ch.len_utf16() as u32;
                        expected += ch.len_utf8();
                    }
                    assert_eq!(
                        index.offset(LineCol { line, col }),
                        Some(TextSize::from(expected as u32)),
                        "{text:?} at {line}:{col}"
                    );
                }
                let Some(next) = next else {
                    break;
                };
                start = next;
                line += 1;
            }
            assert_eq!(
                index.offset(LineCol {
                    line: line + 1,
                    col: 0
                }),
                None
            );
        }
    }

    #[test]
    fn counts_utf16_for_multibyte_and_non_bmp() {
        // "x🌍y": x = 1 byte / 1 u16, 🌍 = 4 bytes / 2 u16 (surrogate pair), y = 1 / 1
        let li = LineIndex::new("x🌍y");
        assert_eq!(li.line_col(TextSize::from(1)), LineCol { line: 0, col: 1 }); // before emoji
        assert_eq!(li.line_col(TextSize::from(5)), LineCol { line: 0, col: 3 }); // after emoji: 1 + 2
        assert_eq!(
            li.offset(LineCol { line: 0, col: 3 }),
            Some(TextSize::from(5))
        );
        // BMP multibyte: € = U+20AC = 3 bytes / 1 u16
        let li2 = LineIndex::new("a€b"); // a0 €(1,2,3) b4
        assert_eq!(li2.line_col(TextSize::from(4)), LineCol { line: 0, col: 2 });
        // a(1) + €(1)
    }

    #[test]
    fn roundtrips_every_token_boundary_on_unicode_source() {
        // Unicode in a comment AND a unicode string literal — both shift `character`.
        let src = "// héllo 🌍\ncontract C { string s = unicode\"αβγ 😀\"; }\n";
        let li = LineIndex::new(src);
        for el in parse(src).syntax().descendants_with_tokens() {
            let r = el.text_range();
            for off in [r.start(), r.end()] {
                let lc = li.line_col(off);
                assert_eq!(
                    li.offset(lc),
                    Some(off),
                    "roundtrip failed at {off:?} -> {lc:?}"
                );
            }
        }
        // EOF offset maps to the (empty) final line, col 0.
        let eof = TextSize::from(src.len() as u32);
        assert_eq!(li.line_col(eof).col, 0);
    }
}
