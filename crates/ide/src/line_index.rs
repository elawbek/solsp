//! `LineIndex`: convert between rowan byte offsets and LSP `{line, character}`
//! positions. LSP columns are **UTF-16** code units, so non-ASCII text (emoji in
//! comments, unicode string literals) shifts `character` — a classic source of
//! off-by-N bugs. Built once per document (design §4).

use rowan::TextSize;

/// A zero-based line/column. `col` is a UTF-16 code-unit offset within the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// Maps byte offsets <-> line/UTF-16-column for one document. Built once per parse.
///
/// We keep the line-start byte offsets plus an owned copy of the text; conversions
/// walk a single line counting UTF-16 code units (exact, `O(line-length)`). The
/// owned copy is the cost of the fixed `(&self, offset)` signature; if it ever
/// matters, swap for a per-line non-ASCII delta table (rust-analyzer style) — the
/// public API does not change.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// Byte offset of the start of each line. `line_starts[0] == 0`; one entry per
    /// line, in ascending order. A trailing `\n` yields a final empty-line entry
    /// equal to `text.len()`.
    line_starts: Vec<TextSize>,
    /// The full source text — needed to count UTF-16 units within a line.
    text: String,
}

impl LineIndex {
    pub fn new(text: &str) -> LineIndex {
        let mut line_starts = vec![TextSize::from(0)];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(TextSize::from(i as u32 + 1));
            }
        }
        LineIndex {
            line_starts,
            text: text.to_owned(),
        }
    }

    /// Byte offset -> line/UTF-16-column. Offsets past EOF clamp to the text end.
    pub fn line_col(&self, offset: TextSize) -> LineCol {
        let offset = (u32::from(offset) as usize).min(self.text.len());
        // The line is the index of the greatest line-start <= offset. `partition_point`
        // counts the line-starts that are <= offset; since line_starts[0] == 0 <= offset,
        // that count is >= 1, so the subtraction never underflows.
        let line = self
            .line_starts
            .partition_point(|&start| u32::from(start) as usize <= offset)
            - 1;
        let line_start = u32::from(self.line_starts[line]) as usize;
        let col: usize = self.text[line_start..offset]
            .chars()
            .map(char::len_utf16)
            .sum();
        LineCol {
            line: line as u32,
            col: col as u32,
        }
    }

    /// Line/UTF-16-column -> byte offset. `None` if the line is past EOF; a `col`
    /// past the line's content clamps to the line's end (LSP tolerates over-range
    /// positions); a `col` that lands mid-surrogate clamps forward to the next char.
    pub fn offset(&self, line_col: LineCol) -> Option<TextSize> {
        let line = line_col.line as usize;
        let line_start = u32::from(*self.line_starts.get(line)?) as usize;
        let line_end = self
            .line_starts
            .get(line + 1)
            .map(|&s| u32::from(s) as usize)
            .unwrap_or(self.text.len());
        let mut utf16: u32 = 0;
        let mut byte = line_start;
        for c in self.text[line_start..line_end].chars() {
            if utf16 >= line_col.col {
                break;
            }
            utf16 += c.len_utf16() as u32;
            byte += c.len_utf8();
        }
        Some(TextSize::from(byte.min(line_end) as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solsp_syntax::parse;

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
