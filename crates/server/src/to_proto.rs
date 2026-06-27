//! Mapping from `solsp-ide`'s "bare" data into LSP wire types. This is the *only*
//! place that knows both worlds: byte `TextRange`s + ide enums on one side, the
//! `lsp_types` protocol on the other. Every function is pure and unit-tested — the
//! main loop just calls them (design §4: features stay protocol-agnostic).

use lsp_types::{
    Diagnostic as LspDiagnostic, DiagnosticSeverity, DocumentSymbol as LspSymbol, Position, Range,
    SemanticToken as LspToken, SemanticTokenType, SemanticTokens as LspSemanticTokens,
    SemanticTokensLegend, SymbolKind as LspSymbolKind,
};
use rowan::{TextRange, TextSize};
use solsp_ide::diagnostics::{Diagnostic as IdeDiagnostic, Severity};
use solsp_ide::document_symbols::{DocumentSymbol as IdeSymbol, SymbolKind as IdeSymbolKind};
use solsp_ide::semantic_tokens::{SemanticToken as IdeToken, TokenType};
use solsp_ide::{LineCol, LineIndex};

// ---------------------------------------------------------------------------
// Positions
// ---------------------------------------------------------------------------

/// Byte offset → LSP `{line, character}` (character is a UTF-16 code-unit column).
fn position(li: &LineIndex, offset: TextSize) -> Position {
    let lc = li.line_col(offset);
    Position {
        line: lc.line,
        character: lc.col,
    }
}

/// LSP `{line, character}` → byte offset (`None` past EOF). The inverse of
/// [`position`]; used to turn a request cursor into a `TextSize`.
pub fn offset(li: &LineIndex, pos: Position) -> Option<TextSize> {
    li.offset(LineCol {
        line: pos.line,
        col: pos.character,
    })
}

/// Byte `TextRange` → LSP `Range`.
pub fn range(li: &LineIndex, r: TextRange) -> Range {
    Range {
        start: position(li, r.start()),
        end: position(li, r.end()),
    }
}

// ---------------------------------------------------------------------------
// Document symbols
// ---------------------------------------------------------------------------

/// Map a nested outline into LSP `DocumentSymbol`s (recursively).
pub fn document_symbols(syms: &[IdeSymbol], li: &LineIndex) -> Vec<LspSymbol> {
    syms.iter().map(|s| document_symbol(s, li)).collect()
}

// `DocumentSymbol.deprecated` is a `#[deprecated]` LSP field; we must name it to
// build the struct, so silence the lint at the one construction site.
#[allow(deprecated)]
fn document_symbol(s: &IdeSymbol, li: &LineIndex) -> LspSymbol {
    let children = if s.children.is_empty() {
        None
    } else {
        Some(document_symbols(&s.children, li))
    };
    LspSymbol {
        name: s.name.clone(),
        detail: None,
        kind: symbol_kind(s.kind),
        tags: None,
        deprecated: None,
        range: range(li, s.range),
        selection_range: range(li, s.selection_range),
        children,
    }
}

/// Map ide outline kinds onto the closest LSP `SymbolKind`. Solidity has no exact
/// LSP analogue for some kinds (modifier, error, library): contract → CLASS,
/// interface → INTERFACE, library → NAMESPACE, modifier → FUNCTION,
/// state variable → FIELD, error → OBJECT.
fn symbol_kind(kind: IdeSymbolKind) -> LspSymbolKind {
    match kind {
        IdeSymbolKind::Contract => LspSymbolKind::CLASS,
        IdeSymbolKind::Interface => LspSymbolKind::INTERFACE,
        IdeSymbolKind::Library => LspSymbolKind::NAMESPACE,
        IdeSymbolKind::Function => LspSymbolKind::FUNCTION,
        IdeSymbolKind::Constructor => LspSymbolKind::CONSTRUCTOR,
        IdeSymbolKind::Modifier => LspSymbolKind::FUNCTION,
        IdeSymbolKind::StateVariable => LspSymbolKind::FIELD,
        IdeSymbolKind::Struct => LspSymbolKind::STRUCT,
        IdeSymbolKind::Enum => LspSymbolKind::ENUM,
        IdeSymbolKind::Event => LspSymbolKind::EVENT,
        IdeSymbolKind::Error => LspSymbolKind::OBJECT,
    }
}

// ---------------------------------------------------------------------------
// Semantic tokens
// ---------------------------------------------------------------------------

/// The semantic-token legend, in a single fixed order. The index of a `TokenType`
/// in `TOKEN_TYPES` is exactly its `tokenType` index on the wire — `legend()` and
/// `token_index()` both read this array, so they can never drift apart.
const TOKEN_TYPES: [TokenType; 10] = [
    TokenType::Keyword,
    TokenType::Type,
    TokenType::Function,
    TokenType::Variable,
    TokenType::Parameter,
    TokenType::Property,
    TokenType::Number,
    TokenType::String,
    TokenType::Comment,
    TokenType::NatspecTag,
];

/// The LSP `SemanticTokenType` standard name for each ide `TokenType`.
fn semantic_token_type(t: TokenType) -> SemanticTokenType {
    match t {
        TokenType::Keyword => SemanticTokenType::KEYWORD,
        TokenType::Type => SemanticTokenType::TYPE,
        TokenType::Function => SemanticTokenType::FUNCTION,
        TokenType::Variable => SemanticTokenType::VARIABLE,
        TokenType::Parameter => SemanticTokenType::PARAMETER,
        TokenType::Property => SemanticTokenType::PROPERTY,
        TokenType::Number => SemanticTokenType::NUMBER,
        TokenType::String => SemanticTokenType::STRING,
        TokenType::Comment => SemanticTokenType::COMMENT,
        // a NatSpec `@tag` — `decorator` renders like an annotation, distinct from the
        // surrounding comment text.
        TokenType::NatspecTag => SemanticTokenType::DECORATOR,
    }
}

/// The legend advertised at `initialize` (no modifiers in M1).
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: TOKEN_TYPES
            .iter()
            .copied()
            .map(semantic_token_type)
            .collect(),
        token_modifiers: Vec::new(),
    }
}

/// The wire index of a token type (its position in `TOKEN_TYPES`).
fn token_index(t: TokenType) -> u32 {
    TOKEN_TYPES
        .iter()
        .position(|&x| x == t)
        .expect("every TokenType is in TOKEN_TYPES") as u32
}

/// One single-line slice of a classified token, ready for delta encoding.
struct Piece {
    line: u32,
    /// UTF-16 column where the slice starts.
    col: u32,
    /// UTF-16 length of the slice.
    len: u32,
    type_idx: u32,
}

/// Delta-encode classified tokens into LSP `SemanticTokens`. Tokens that span
/// multiple lines (a block comment is the only such token in M1) are split per line
/// — the LSP protocol forbids a single token from crossing a line boundary.
pub fn semantic_tokens(tokens: &[IdeToken], text: &str, li: &LineIndex) -> LspSemanticTokens {
    let mut pieces: Vec<Piece> = Vec::with_capacity(tokens.len());
    for tok in tokens {
        split_token(
            text,
            li,
            tok.range,
            token_index(tok.token_type),
            &mut pieces,
        );
    }

    // `pieces` is already in (line, col) ascending order: the bare tokens are
    // start-offset sorted, and each token's own slices ascend by line.
    let mut data = Vec::with_capacity(pieces.len());
    let (mut prev_line, mut prev_col) = (0u32, 0u32);
    for p in pieces {
        let delta_line = p.line - prev_line;
        let delta_start = if delta_line == 0 {
            p.col - prev_col
        } else {
            p.col
        };
        data.push(LspToken {
            delta_line,
            delta_start,
            length: p.len,
            token_type: p.type_idx,
            token_modifiers_bitset: 0,
        });
        prev_line = p.line;
        prev_col = p.col;
    }

    LspSemanticTokens {
        result_id: None,
        data,
    }
}

/// Slice one token range into per-line `Piece`s, pushing them onto `out`. Lengths
/// are UTF-16 code units; a `\r` (in a CRLF inside a block comment) is treated as
/// part of the line ending, not visible content, and dropped.
fn split_token(text: &str, li: &LineIndex, r: TextRange, type_idx: u32, out: &mut Vec<Piece>) {
    let start = li.line_col(r.start());
    let mut line = start.line;
    let mut col = start.col; // start column of the slice currently being built
    let mut len = 0u32;
    for ch in text[r].chars() {
        match ch {
            '\n' => {
                if len > 0 {
                    out.push(Piece {
                        line,
                        col,
                        len,
                        type_idx,
                    });
                }
                line += 1;
                col = 0;
                len = 0;
            }
            '\r' => {}
            _ => len += ch.len_utf16() as u32,
        }
    }
    if len > 0 {
        out.push(Piece {
            line,
            col,
            len,
            type_idx,
        });
    }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Map bare diagnostics to LSP, tagging the source as `solsp`.
pub fn diagnostics(diags: &[IdeDiagnostic], li: &LineIndex) -> Vec<LspDiagnostic> {
    diags
        .iter()
        .map(|d| LspDiagnostic {
            range: range(li, d.range),
            severity: Some(severity(d.severity)),
            source: Some("solsp".to_string()),
            message: d.message.clone(),
            ..Default::default()
        })
        .collect()
}

fn severity(s: Severity) -> DiagnosticSeverity {
    match s {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trange(start: u32, end: u32) -> TextRange {
        TextRange::new(start.into(), end.into())
    }

    #[test]
    fn range_maps_bytes_to_utf16_linecol() {
        // "ab\ncd€" — € is 3 bytes / 1 UTF-16 unit on line 1.
        let text = "ab\ncd€";
        let li = LineIndex::new(text);
        // "cd" occupies bytes 3..5 on line 1.
        let r = range(&li, trange(3, 5));
        assert_eq!(
            r.start,
            Position {
                line: 1,
                character: 0
            }
        );
        assert_eq!(
            r.end,
            Position {
                line: 1,
                character: 2
            }
        );
        // through the euro sign: bytes 5..8 → cols 2..3 (one UTF-16 unit).
        let r2 = range(&li, trange(5, 8));
        assert_eq!(
            r2.end,
            Position {
                line: 1,
                character: 3
            }
        );
    }

    #[test]
    fn legend_and_index_stay_in_sync() {
        let l = legend();
        assert_eq!(l.token_types.len(), TOKEN_TYPES.len());
        assert!(l.token_modifiers.is_empty());
        // index() agrees with the legend slot for every type.
        for (i, &t) in TOKEN_TYPES.iter().enumerate() {
            assert_eq!(token_index(t) as usize, i);
            assert_eq!(l.token_types[i], semantic_token_type(t));
        }
        assert_eq!(token_index(TokenType::Keyword), 0);
        assert_eq!(token_index(TokenType::Comment), 8);
    }

    #[test]
    fn symbol_kinds_and_nesting() {
        let li = LineIndex::new("contract C { function f() {} }");
        let inner = IdeSymbol {
            name: "f".into(),
            kind: IdeSymbolKind::Function,
            range: trange(13, 28),
            selection_range: trange(22, 23),
            children: Vec::new(),
        };
        let outer = IdeSymbol {
            name: "C".into(),
            kind: IdeSymbolKind::Contract,
            range: trange(0, 30),
            selection_range: trange(9, 10),
            children: vec![inner],
        };
        let mapped = document_symbols(&[outer], &li);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].kind, LspSymbolKind::CLASS);
        let kids = mapped[0].children.as_ref().unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].kind, LspSymbolKind::FUNCTION);
        assert_eq!(kids[0].name, "f");
        // a childless symbol gets `children: None`, not `Some(vec![])`.
        assert!(kids[0].children.is_none());
    }

    #[test]
    fn semantic_tokens_delta_encode_same_and_next_line() {
        // two tokens: "ab" at 0..2 (line0 col0) and "cd" at 6..8 (line1 col0).
        let text = "ab xx\ncd";
        let li = LineIndex::new(text);
        let toks = [
            IdeToken {
                range: trange(0, 2),
                token_type: TokenType::Type,
            },
            IdeToken {
                range: trange(6, 8),
                token_type: TokenType::Function,
            },
        ];
        let st = semantic_tokens(&toks, text, &li);
        assert_eq!(st.data.len(), 2);
        // first: absolute line0 char0 len2 type=Type(1)
        assert_eq!(st.data[0].delta_line, 0);
        assert_eq!(st.data[0].delta_start, 0);
        assert_eq!(st.data[0].length, 2);
        assert_eq!(st.data[0].token_type, token_index(TokenType::Type));
        // second: one line down, char resets to absolute 0
        assert_eq!(st.data[1].delta_line, 1);
        assert_eq!(st.data[1].delta_start, 0);
        assert_eq!(st.data[1].length, 2);
        assert_eq!(st.data[1].token_type, token_index(TokenType::Function));
    }

    #[test]
    fn multiline_block_comment_splits_per_line() {
        // a single Comment token spanning two lines must become two wire tokens.
        let text = "/* a\nbc */";
        let li = LineIndex::new(text);
        let toks = [IdeToken {
            range: trange(0, text.len() as u32),
            token_type: TokenType::Comment,
        }];
        let st = semantic_tokens(&toks, text, &li);
        assert_eq!(st.data.len(), 2);
        // line 0: "/* a" = 4 units
        assert_eq!(st.data[0].delta_line, 0);
        assert_eq!(st.data[0].delta_start, 0);
        assert_eq!(st.data[0].length, 4);
        // line 1: "bc */" = 5 units, one line down, col 0
        assert_eq!(st.data[1].delta_line, 1);
        assert_eq!(st.data[1].delta_start, 0);
        assert_eq!(st.data[1].length, 5);
    }

    #[test]
    fn diagnostics_map_severity_and_source() {
        let text = "contract";
        let li = LineIndex::new(text);
        let diags = [IdeDiagnostic {
            range: trange(0, 8),
            message: "boom".into(),
            severity: Severity::Error,
        }];
        let mapped = diagnostics(&diags, &li);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].severity, Some(DiagnosticSeverity::ERROR));
        assert_eq!(mapped[0].source.as_deref(), Some("solsp"));
        assert_eq!(mapped[0].message, "boom");
        assert_eq!(
            mapped[0].range.end,
            Position {
                line: 0,
                character: 8
            }
        );
    }
}
