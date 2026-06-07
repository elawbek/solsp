//! Syntax diagnostics: turn the parser's `SyntaxError`s into editor-ready ranges.
//! The "pulse" of the parser — if it breaks, this lights up (design §4, feature 1).

use rowan::TextRange;
use solsp_syntax::Parse;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A bare diagnostic: source range + message + severity. Mapped to LSP in the server.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub range: TextRange,
    pub message: String,
    pub severity: Severity,
}

/// Collect syntax diagnostics from a parse result.
pub fn diagnostics(parse: &Parse) -> Vec<Diagnostic> {
    parse
        .errors()
        .iter()
        .map(|e| Diagnostic {
            range: e.range,
            message: e.message.clone(),
            severity: Severity::Error,
        })
        .collect()
}
