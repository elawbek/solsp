//! `LineIndex`: convert between rowan byte offsets and LSP `{line, character}`
//! positions. LSP columns are **UTF-16** code units, so non-ASCII text (emoji in
//! comments, unicode string literals) shifts `character` — a classic source of
//! off-by-N bugs. Built once per document (design §4).

use rowan::{TextSize};

/// A zero-based line/column. `col` is a UTF-16 code-unit offset within the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// Maps byte offsets <-> line/UTF-16-column for one document.
#[derive(Debug, Clone)]
pub struct LineIndex {
    // TODO(M1 §4): newline byte offsets + per-line table of multi-byte/non-BMP
    // chars to make UTF-8 byte <-> UTF-16 column conversion exact.
    _newlines: Vec<TextSize>,
}

impl LineIndex {
    pub fn new(_text: &str) -> LineIndex {
        // TODO(M1 §4): scan for '\n', record line starts, note UTF-16 widths.
        LineIndex {
            _newlines: Vec::new(),
        }
    }

    /// Byte offset -> line/UTF-16-column.
    pub fn line_col(&self, _offset: TextSize) -> LineCol {
        // TODO(M1 §4)
        LineCol { line: 0, col: 0 }
    }

    /// Line/UTF-16-column -> byte offset.
    pub fn offset(&self, _line_col: LineCol) -> Option<TextSize> {
        // TODO(M1 §4)
        None
    }
}
