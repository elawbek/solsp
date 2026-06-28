//! Go-to-definition and hover (M2 P5). Thin feature layer over `solsp-hir`'s
//! resolver: turn a cursor offset into a target range (goto) or a documentation
//! string (hover), as bare data the server maps to LSP (design §4).

use rowan::{TextRange, TextSize};
use solsp_hir::resolve::{definition_at, DefKind, Definition};
use solsp_syntax::{SyntaxKind, SyntaxNode};

/// The definition target for the identifier at `offset`: the byte range of the
/// declaration's name (where the editor should jump). `None` if nothing resolves.
pub fn goto_definition(root: &SyntaxNode, offset: TextSize) -> Option<TextRange> {
    let def = definition_at(root, offset)?;
    Some(name_range(root, &def))
}

/// The identifier text at `offset`, if any (used to look a name up across files).
pub fn name_at(root: &SyntaxNode, offset: TextSize) -> Option<String> {
    root.token_at_offset(offset)
        .find(|t| t.kind() == SyntaxKind::IDENT)
        .map(|t| t.text().to_string())
}

/// Hover information: a markdown string plus the range of the hovered identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    pub contents: String,
    pub range: TextRange,
}

/// Hover for the identifier at `offset`: the declaration's signature line plus a
/// `(kind) name` caption. `None` if nothing resolves.
pub fn hover(root: &SyntaxNode, offset: TextSize) -> Option<Hover> {
    if let Some(def) = definition_at(root, offset) {
        let contents = hover_markdown(root, &def);
        let range = ident_range_at(root, offset).unwrap_or_else(|| name_range(root, &def));
        return Some(Hover { contents, range });
    }
    // a literal (`5`, `"hi"`, `true`, `1 ether`) → its inferred type.
    literal_hover(root, offset)
}

/// Hover for a literal at `offset`: its inferred type and the literal text.
fn literal_hover(root: &SyntaxNode, offset: TextSize) -> Option<Hover> {
    use SyntaxKind::{FALSE_KW, LITERAL_EXPR, NUMBER, STRING, TRUE_KW};
    let tok = root
        .token_at_offset(offset)
        .find(|t| matches!(t.kind(), NUMBER | STRING | TRUE_KW | FALSE_KW))?;
    let lit = tok
        .parent()
        .filter(|n| n.kind() == LITERAL_EXPR)
        .unwrap_or_else(|| tok.parent().expect("token has a parent"));
    let text = lit.text().to_string();
    let trimmed = text.trim();
    let (ty, label) = match tok.kind() {
        TRUE_KW | FALSE_KW => ("bool", "boolean literal"),
        STRING => {
            if trimmed.starts_with("hex") {
                ("bytes", "hex literal")
            } else {
                ("string", "string literal")
            }
        }
        // NUMBER: an `0x`-prefixed 40-hex-digit value is an address; otherwise an integer.
        _ => {
            let hex = tok
                .text()
                .strip_prefix("0x")
                .or_else(|| tok.text().strip_prefix("0X"));
            if hex.is_some_and(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())) {
                ("address", "address literal")
            } else {
                ("uint256", "number literal")
            }
        }
    };
    Some(Hover {
        contents: format!("```solidity\n{ty}\n```\n\n*({label})* `{trimmed}`"),
        range: lit.text_range(),
    })
}

/// The hover markdown for a definition (public so the server can render a hover for a
/// member it resolved itself): a `solidity` code block with the signature line plus a
/// `(kind) name` caption.
pub fn hover_text(root: &SyntaxNode, def: &Definition) -> String {
    hover_markdown(root, def)
}

/// The hover markdown for a definition: a `solidity` code block with its signature
/// line plus a `(kind) name` caption.
fn hover_markdown(root: &SyntaxNode, def: &Definition) -> String {
    let decl = def.full_ptr.to_node(root);
    let sig = signature(&decl);
    format!(
        "```solidity\n{sig}\n```\n\n*({label})* `{name}`",
        label = def_label(def.kind),
        name = def.name,
    )
}

/// A declaration's signature for hover: its text up to the body block (or the whole
/// declaration if it has none), with comments dropped and whitespace collapsed — so a
/// multi-line function header renders on one line. A trailing `;` is removed.
fn signature(decl: &SyntaxNode) -> String {
    use SyntaxKind::{BLOCK, COMMENT, CONTRACT_BODY, WHITESPACE};
    // stop at the body: a function `BLOCK` or a contract/interface/library `CONTRACT_BODY`
    // (otherwise a contract-like declaration's whole body is dumped into the hover).
    let end = decl
        .children()
        .find(|n| matches!(n.kind(), BLOCK | CONTRACT_BODY))
        .map(|b| b.text_range().start())
        .unwrap_or_else(|| decl.text_range().end());
    let mut out = String::new();
    for tok in decl
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
    {
        if tok.text_range().start() >= end {
            break;
        }
        match tok.kind() {
            COMMENT => {}
            WHITESPACE => {
                if !out.is_empty() && !out.ends_with(' ') {
                    out.push(' ');
                }
            }
            _ => out.push_str(tok.text()),
        }
    }
    // tighten spacing introduced by collapsing newlines around punctuation.
    let out = out
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(" ,", ",")
        .replace(" ;", ";");
    out.trim().trim_end_matches(';').trim_end().to_string()
}

/// The precise identifier range of a definition's name (the `IDENT` token, not the
/// `NAME` node, which carries leading trivia).
fn name_range(root: &SyntaxNode, def: &Definition) -> TextRange {
    let name_node = def.name_ptr.to_node(root);
    ident_range(&name_node).unwrap_or_else(|| name_node.text_range())
}

/// Range of the first `IDENT` token within `node`.
fn ident_range(node: &SyntaxNode) -> Option<TextRange> {
    node.children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT)
        .map(|t| t.text_range())
}

/// Range of the `IDENT` token at `offset`.
fn ident_range_at(root: &SyntaxNode, offset: TextSize) -> Option<TextRange> {
    root.token_at_offset(offset)
        .find(|t| t.kind() == SyntaxKind::IDENT)
        .map(|t| t.text_range())
}

/// Human label for a definition kind (used in hover captions).
fn def_label(kind: DefKind) -> &'static str {
    match kind {
        DefKind::Contract => "contract",
        DefKind::Interface => "interface",
        DefKind::Library => "library",
        DefKind::Function => "function",
        DefKind::Modifier => "modifier",
        DefKind::StateVariable => "state variable",
        DefKind::Struct => "struct",
        DefKind::Enum => "enum",
        DefKind::Event => "event",
        DefKind::Error => "error",
        DefKind::UserType => "type",
        DefKind::Parameter => "parameter",
        DefKind::Local => "local",
        DefKind::Field => "field",
        DefKind::Variant => "enum variant",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solsp_syntax::parse;

    fn at(src: &str, needle: &str) -> TextSize {
        TextSize::from(src.find(needle).expect("needle") as u32)
    }

    #[test]
    fn goto_jumps_to_the_member_declaration_name() {
        let src = "contract C {\n\
            uint256 stored;\n\
            function f() public { stored = 1; }\n\
        }";
        let root = parse(src).syntax();
        let target = goto_definition(&root, at(src, "stored = 1")).unwrap();
        // points exactly at the `stored` in the state-var declaration
        assert_eq!(&src[target], "stored");
        let decl_off = src.find("uint256 stored").unwrap() + "uint256 ".len();
        assert_eq!(usize::from(target.start()), decl_off);
    }

    #[test]
    fn hover_shows_kind_and_signature() {
        let src = "contract C {\n\
            function helper(uint256 n) internal returns (uint256) { return n; }\n\
            function g() public { helper(1); }\n\
        }";
        let root = parse(src).syntax();
        let h = hover(&root, at(src, "helper(1)")).unwrap();
        assert!(h.contents.contains("(function)"));
        assert!(h.contents.contains("`helper`"));
        assert!(h.contents.contains("function helper(uint256 n)")); // signature line
        assert_eq!(&src[h.range], "helper"); // hovered identifier range
    }

    #[test]
    fn hover_signature_collapses_multiline_and_drops_body() {
        let src = "contract C {\n\
            function big(\n\
                uint256 amount,\n\
                address to\n\
            ) internal returns (bool ok) {\n\
                return true;\n\
            }\n\
            function g() public { big(1, address(0)); }\n\
        }";
        let root = parse(src).syntax();
        let h = hover(&root, at(src, "big(1")).unwrap();
        assert!(
            h.contents
                .contains("function big(uint256 amount, address to) internal returns (bool ok)"),
            "signature: {}",
            h.contents
        );
        assert!(!h.contents.contains("return true"), "body must be excluded");
    }

    #[test]
    fn hover_on_contract_like_shows_header_not_body() {
        let src = "interface IFoo is IBar {\n\
                function a() external;\n\
                function b() external view returns (uint256);\n\
            }\n\
            contract C { IFoo foo; }";
        let root = parse(src).syntax();
        let h = hover(&root, at(src, "IFoo foo")).unwrap();
        assert!(
            h.contents.contains("interface IFoo is IBar"),
            "{}",
            h.contents
        );
        assert!(
            !h.contents.contains("function a()"),
            "body must be excluded"
        );
    }

    #[test]
    fn hover_on_literals_shows_inferred_type() {
        let src = "contract C { function f() public { \
                   uint256 n = 5; string memory s = \"hi\"; bool b = true; uint256 e = 1 ether; \
                   address a = 0x1111111111111111111111111111111111111111; } }";
        let root = parse(src).syntax();
        let ty = |needle: &str| hover(&root, at(src, needle)).unwrap().contents;
        assert!(ty("5;").contains("uint256") && ty("5;").contains("number literal"));
        assert!(ty("\"hi\"").contains("string"));
        assert!(ty("true").contains("bool"));
        assert!(ty("1 ether").contains("uint256"));
        assert!(ty("0x1111").contains("address"));
    }

    #[test]
    fn hover_shows_type_not_comments() {
        let src = "contract C {\n\
            /// @notice the running balance\n\
            uint256 balance; // storage slot 0\n\
            function f() public { balance = 1; }\n\
        }";
        let root = parse(src).syntax();
        let h = hover(&root, at(src, "balance = 1")).unwrap();
        assert!(
            h.contents.contains("uint256 balance"),
            "type shown: {}",
            h.contents
        );
        assert!(!h.contents.contains("slot 0"), "trailing comment stripped");
        assert!(!h.contents.contains("@notice"), "leading doc skipped");
    }

    #[test]
    fn unresolved_identifier_has_no_goto_or_hover() {
        let src = "contract C { function f() public { mystery(); } }";
        let root = parse(src).syntax();
        assert!(goto_definition(&root, at(src, "mystery")).is_none());
        assert!(hover(&root, at(src, "mystery")).is_none());
    }
}
