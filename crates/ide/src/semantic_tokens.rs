//! Semantic tokens: walk the tree and classify each identifier token by its
//! **syntactic** position (contract name -> type, callee -> function, type in
//! `mapping(...)` -> type, ...). No name resolution in M1 — that sharpens this in
//! M2 (e.g. state var vs local) (design §4, feature 3).

use rowan::{TextRange, TextSize};
use solsp_syntax::{SyntaxKind, SyntaxNode, SyntaxToken};

/// Token classification (maps to the LSP semantic-tokens legend in the server).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    Keyword,
    Type,
    Function,
    Variable,
    Parameter,
    Property,
    Number,
    String,
    Comment,
    /// A NatSpec tag (`@param`, `@dev`, …) inside a doc comment.
    NatspecTag,
}

/// One classified token span.
#[derive(Debug, Clone, Copy)]
pub struct SemanticToken {
    pub range: TextRange,
    pub token_type: TokenType,
}

/// Classify every relevant token, in document order. Whitespace, operators, and
/// bare (non-`NAME`-wrapped) identifiers are skipped; everything else is colored by
/// syntactic position. The result is start-offset sorted (the server delta-encodes
/// it). No name resolution — that sharpens this in M2.
pub fn semantic_tokens(root: &SyntaxNode) -> Vec<SemanticToken> {
    root.descendants_with_tokens()
        .filter_map(|el| el.into_token())
        .flat_map(|tok| -> Vec<SemanticToken> {
            // A doc comment expands into NatSpec sub-tokens (`@tag`, the name after
            // `@param`/`@inheritdoc`, and the surrounding comment text); every other
            // token is a single classified span (or nothing).
            if tok.kind() == SyntaxKind::COMMENT {
                natspec_tokens(&tok)
            } else {
                classify(&tok)
                    .map(|token_type| SemanticToken {
                        range: tok.text_range(),
                        token_type,
                    })
                    .into_iter()
                    .collect()
            }
        })
        .collect()
}

/// Split a comment into NatSpec sub-tokens. Only `///` and `/** … */` doc comments are
/// scanned; any other comment stays a single `Comment` span. The result tiles the whole
/// comment (gaps emitted as `Comment`) so nothing overlaps.
fn natspec_tokens(tok: &SyntaxToken) -> Vec<SemanticToken> {
    let text = tok.text();
    let base = tok.text_range().start();
    let whole = || {
        vec![SemanticToken {
            range: tok.text_range(),
            token_type: TokenType::Comment,
        }]
    };
    if !(text.starts_with("///") || text.starts_with("/**")) {
        return whole();
    }
    let specials = natspec_spans(text);
    if specials.is_empty() {
        return whole();
    }
    let span = |s: usize, e: usize, ty: TokenType| SemanticToken {
        range: TextRange::new(
            base + TextSize::from(s as u32),
            base + TextSize::from(e as u32),
        ),
        token_type: ty,
    };
    let mut out = Vec::with_capacity(specials.len() * 2 + 1);
    let mut cursor = 0usize;
    for (s, e, ty) in specials {
        if s > cursor {
            out.push(span(cursor, s, TokenType::Comment));
        }
        out.push(span(s, e, ty));
        cursor = e;
    }
    if cursor < text.len() {
        out.push(span(cursor, text.len(), TokenType::Comment));
    }
    out
}

/// Find the special spans in a doc comment: each `@tag`, plus the identifier following
/// `@param` (a parameter) or `@inheritdoc` (a type). Byte offsets into `text`, ordered
/// and non-overlapping.
fn natspec_spans(text: &str) -> Vec<(usize, usize, TokenType)> {
    let b = text.as_bytes();
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut spans = Vec::new();
    let mut i = 0;
    while i < b.len() {
        // a tag is `@word` at a word boundary (so `foo@bar` in prose is not a tag).
        if b[i] == b'@' && (i == 0 || !is_ident(b[i - 1])) {
            let start = i;
            i += 1;
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            if i == start + 1 {
                continue; // a lone `@`
            }
            spans.push((start, i, TokenType::NatspecTag));
            let name_ty = match &text[start + 1..i] {
                "param" => TokenType::Parameter,
                "inheritdoc" => TokenType::Type,
                _ => continue,
            };
            // the name on the same line after the tag (skip spaces/tabs).
            let mut j = i;
            while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            let nstart = j;
            while j < b.len() && is_ident(b[j]) {
                j += 1;
            }
            if j > nstart {
                spans.push((nstart, j, name_ty));
                i = j;
            }
        } else {
            i += 1;
        }
    }
    spans
}

/// The token type for one token, or `None` if it should not be highlighted.
fn classify(tok: &SyntaxToken) -> Option<TokenType> {
    use SyntaxKind::*;
    match tok.kind() {
        WHITESPACE => return None,
        COMMENT => return Some(TokenType::Comment),
        NUMBER => return Some(TokenType::Number),
        STRING => return Some(TokenType::String),
        _ => {}
    }
    // An identifier — or a contextual keyword (`return`/`revert`) used as a Yul
    // callee — is wrapped in a NAME/NAME_REF; classify by that node's position.
    // This runs BEFORE the keyword fallback so Yul `return`/`revert` color Function.
    if let Some(parent) = tok.parent() {
        match parent.kind() {
            NAME => return Some(classify_name(&parent)),
            NAME_REF => return Some(classify_name_ref(&parent)),
            _ => {}
        }
    }
    if is_keyword(tok.kind()) {
        return Some(TokenType::Keyword);
    }
    None
}

/// A defining name (`NAME`), classified by its parent declaration node.
fn classify_name(name: &SyntaxNode) -> TokenType {
    use SyntaxKind::*;
    let Some(parent) = name.parent() else {
        return TokenType::Variable;
    };
    match parent.kind() {
        CONTRACT_DEF | STRUCT_DEF | ENUM_DEF | USER_DEFINED_VALUE_TYPE | EVENT_DEF | ERROR_DEF => {
            TokenType::Type
        }
        FUNCTION_DEF | MODIFIER_DEF => TokenType::Function,
        // A Yul function definition holds BOTH its own name and its `-> r, s` return
        // names as bare `NAME` children (no wrapper). The function name is the first
        // such child (it precedes the param list); the return names come later and
        // are local bindings ⇒ Variable.
        YUL_FUNCTION_DEF => {
            if parent.children().find(|n| n.kind() == NAME).as_ref() == Some(name) {
                TokenType::Function
            } else {
                TokenType::Variable
            }
        }
        PARAM | MAPPING_TYPE | YUL_PARAM_LIST => TokenType::Parameter,
        VAR_DECL | STATE_VAR_DEF => TokenType::Variable,
        STRUCT_FIELD | ENUM_VARIANT | NAMED_ARG_LIST | CALL_OPTIONS => TokenType::Property,
        _ => TokenType::Variable,
    }
}

/// A referencing name (`NAME_REF`), classified by its parent use-site node.
fn classify_name_ref(name_ref: &SyntaxNode) -> TokenType {
    use SyntaxKind::*;
    let Some(parent) = name_ref.parent() else {
        return TokenType::Variable;
    };
    match parent.kind() {
        // a base-contract path, an elementary/user type name, or a `catch Error(…)`
        // / `catch Panic(…)` error name — all type-position references.
        PATH_TYPE | CATCH_CLAUSE => TokenType::Type,
        MODIFIER_INVOCATION | YUL_FUNCTION_CALL => TokenType::Function,
        YUL_PATH => TokenType::Variable,
        PATH_EXPR => {
            if is_callee(&parent) {
                TokenType::Function
            } else {
                TokenType::Variable
            }
        }
        MEMBER_EXPR => {
            if is_callee(&parent) {
                TokenType::Function
            } else {
                TokenType::Property
            }
        }
        _ => TokenType::Variable,
    }
}

/// Is `node` (a `PATH_EXPR`/`MEMBER_EXPR`) the callee — i.e. the first child — of an
/// enclosing `CALL_EXPR`? (`first_child` skips tokens, so it is the callee subtree.)
fn is_callee(node: &SyntaxNode) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != SyntaxKind::CALL_EXPR {
        return false;
    }
    parent.first_child().as_ref() == Some(node)
}

/// Is `kind` a keyword? The keyword discriminants form one contiguous block in
/// `SyntaxKind` (`PRAGMA_KW ..= FALSE_KW`, syntax_kind.rs); a range check over the
/// `u16` reprs stays correct as keywords are added *within* that block.
fn is_keyword(kind: SyntaxKind) -> bool {
    (SyntaxKind::PRAGMA_KW.to_u16()..=SyntaxKind::FALSE_KW.to_u16()).contains(&kind.to_u16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solsp_syntax::parse;

    /// All token texts classified as `tt`, in document order.
    fn pick<'a>(src: &'a str, toks: &[SemanticToken], tt: TokenType) -> Vec<&'a str> {
        toks.iter()
            .filter(|t| t.token_type == tt)
            .map(|t| &src[t.range])
            .collect()
    }

    #[test]
    fn natspec_tags_names_and_text() {
        let src = "/// @notice sets it\n\
                   /// @param owner_ the new owner\n\
                   /// @inheritdoc IBase\n\
                   contract C {}";
        let toks = semantic_tokens(&parse(src).syntax());
        assert_eq!(
            pick(src, &toks, TokenType::NatspecTag),
            ["@notice", "@param", "@inheritdoc"]
        );
        // `@param`'s name is a parameter; `@inheritdoc`'s is a type.
        assert!(pick(src, &toks, TokenType::Parameter).contains(&"owner_"));
        assert!(pick(src, &toks, TokenType::Type).contains(&"IBase"));
        // the descriptions stay plain comment text (distinct from tag/name).
        let comments = pick(src, &toks, TokenType::Comment);
        assert!(comments.iter().any(|c| c.contains("sets it")));
        assert!(comments.iter().any(|c| c.contains("the new owner")));
        // a non-doc comment is left as a single comment span (no tag scanning).
        let plain = semantic_tokens(&parse("// @param x\ncontract C {}").syntax());
        assert!(pick("// @param x\ncontract C {}", &plain, TokenType::NatspecTag).is_empty());
    }

    #[test]
    fn classifies_decls_types_params_and_keywords() {
        let src = "contract C {\n\
            uint256 balance;\n\
            function f(uint256 amount) public returns (uint256) {\n\
                g(amount);\n\
                balance = amount;\n\
            }\n\
        }";
        let toks = semantic_tokens(&parse(src).syntax());
        assert!(pick(src, &toks, TokenType::Type).contains(&"C")); // contract name
        assert!(pick(src, &toks, TokenType::Type).contains(&"uint256")); // type position
        assert!(pick(src, &toks, TokenType::Function).contains(&"f")); // function decl
        assert!(pick(src, &toks, TokenType::Function).contains(&"g")); // callee
        assert!(pick(src, &toks, TokenType::Parameter).contains(&"amount")); // param decl
        assert!(pick(src, &toks, TokenType::Variable).contains(&"balance")); // state var name + use
        assert!(pick(src, &toks, TokenType::Keyword).contains(&"function"));
        assert!(pick(src, &toks, TokenType::Keyword).contains(&"public"));
        // output is start-offset sorted (delta encoder relies on it)
        assert!(toks
            .windows(2)
            .all(|w| w[0].range.start() <= w[1].range.start()));
    }

    #[test]
    fn classifies_members_calls_comments_and_yul() {
        let src = "// note\n\
            contract C {\n\
                function f() public {\n\
                    a.b.c(x);\n\
                    assembly { let v := add(1, 2) sstore(0, v) }\n\
                }\n\
            }";
        let toks = semantic_tokens(&parse(src).syntax());
        assert!(pick(src, &toks, TokenType::Variable).contains(&"a")); // receiver (PATH_EXPR)
        assert!(pick(src, &toks, TokenType::Property).contains(&"b")); // member, not called
        assert!(pick(src, &toks, TokenType::Function).contains(&"c")); // member, is callee
        assert!(pick(src, &toks, TokenType::Function).contains(&"add")); // yul callee
        assert!(pick(src, &toks, TokenType::Function).contains(&"sstore")); // yul callee
        assert!(pick(src, &toks, TokenType::Variable).contains(&"v")); // yul path var
        assert!(pick(src, &toks, TokenType::Keyword).contains(&"let")); // yul keyword
        assert!(pick(src, &toks, TokenType::Comment).contains(&"// note"));
        assert!(pick(src, &toks, TokenType::Number).contains(&"1"));
    }

    #[test]
    fn classifies_yul_function_def_and_catch_error_name() {
        let src = "contract C {\n\
            function f() public {\n\
                assembly { function sum(a, b) -> r { r := add(a, b) } }\n\
                try this.g() {} catch Error(string memory reason) {}\n\
            }\n\
        }";
        let toks = semantic_tokens(&parse(src).syntax());
        assert!(pick(src, &toks, TokenType::Function).contains(&"sum")); // yul fn name
        assert!(pick(src, &toks, TokenType::Parameter).contains(&"a")); // yul param
        assert!(pick(src, &toks, TokenType::Parameter).contains(&"b")); // yul param
        assert!(pick(src, &toks, TokenType::Variable).contains(&"r")); // yul return binding
        assert!(pick(src, &toks, TokenType::Type).contains(&"Error")); // catch error name
    }
}
