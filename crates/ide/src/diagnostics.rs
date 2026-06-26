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

#[cfg(test)]
mod tests {
    use super::*;
    use solsp_syntax::parse;

    #[test]
    fn clean_parse_yields_no_diagnostics() {
        let p = parse("contract C {}");
        assert!(p.errors().is_empty());
        assert!(diagnostics(&p).is_empty());
    }

    #[test]
    fn each_syntax_error_becomes_an_error_diagnostic() {
        // Leading garbage `@@@` reliably yields ≥1 syntax error (the parser
        // err_and_bumps each unexpected token at the file level).
        let p = parse("@@@ contract C {}");
        let diags = diagnostics(&p);
        assert_eq!(diags.len(), p.errors().len());
        assert!(!diags.is_empty(), "expected at least one diagnostic");
        for (d, e) in diags.iter().zip(p.errors()) {
            assert_eq!(d.severity, Severity::Error); // M1: every syntax error is Error
            assert_eq!(d.range, e.range); // ranges map 1:1
            assert_eq!(d.message, e.message); // messages map 1:1
            assert!(!d.message.is_empty());
        }
    }
}
