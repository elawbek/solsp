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

/// Hover information: a markdown string plus the range of the hovered identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hover {
    pub contents: String,
    pub range: TextRange,
}

/// Hover for the identifier at `offset`: the declaration's signature line plus a
/// `(kind) name` caption. `None` if nothing resolves.
pub fn hover(root: &SyntaxNode, offset: TextSize) -> Option<Hover> {
    let def = definition_at(root, offset)?;
    let decl = def.full_ptr.to_node(root);
    let decl_text = decl.text().to_string();
    let signature = first_line(&decl_text);
    let contents = format!(
        "```solidity\n{signature}\n```\n\n*({label})* `{name}`",
        label = def_label(def.kind),
        name = def.name,
    );
    let range = ident_range_at(root, offset).unwrap_or_else(|| name_range(root, &def));
    Some(Hover { contents, range })
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

/// The first non-blank line of a declaration's text (its signature). The decl node
/// can carry leading trivia (newline + indent), so trim that before taking the line.
fn first_line(text: &str) -> &str {
    text.trim_start().lines().next().unwrap_or("").trim_end()
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
    fn unresolved_identifier_has_no_goto_or_hover() {
        let src = "contract C { function f() public { mystery(); } }";
        let root = parse(src).syntax();
        assert!(goto_definition(&root, at(src, "mystery")).is_none());
        assert!(hover(&root, at(src, "mystery")).is_none());
    }
}
