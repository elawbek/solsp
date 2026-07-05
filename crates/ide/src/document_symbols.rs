//! Document symbols (outline): walk the tree, collect declarations into a
//! hierarchy `contract -> { functions, state vars, structs, events, ... }`. No name
//! resolution needed — pure tree shape (design §4, feature 2).

use rowan::TextRange;
use solsp_syntax::{
    ast::{
        AstNode, ContractDef, ContractKind, EnumDef, ErrorDef, EventDef, FunctionDef, ModifierDef,
        Name, SourceFile, StateVarDef, StructDef, StructField, UserDefinedValueType,
    },
    SyntaxKind, SyntaxNode, SyntaxToken,
};

/// Kind of an outline symbol (maps to LSP `SymbolKind` in the server).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Contract,
    Interface,
    Library,
    Function,
    Constructor,
    Modifier,
    StateVariable,
    Field,
    Struct,
    Enum,
    Event,
    Error,
}

/// A nested outline symbol.
#[derive(Debug, Clone)]
pub struct DocumentSymbol {
    pub name: String,
    pub kind: SymbolKind,
    /// Full range of the declaration.
    pub range: TextRange,
    /// Range of just the name (where the cursor lands).
    pub selection_range: TextRange,
    pub children: Vec<DocumentSymbol>,
}

/// Build the outline for a file. Top-level items become symbols; a contract nests
/// its members (in source order). Returns `[]` for a root that is not a SOURCE_FILE.
pub fn document_symbols(root: &SyntaxNode) -> Vec<DocumentSymbol> {
    let Some(file) = SourceFile::cast(root.clone()) else {
        return Vec::new();
    };
    // Walk the SOURCE_FILE's direct children in tree (source) order; non-decl items
    // (pragma/import/using) yield `None` and are skipped.
    file.syntax()
        .children()
        .filter_map(|node| symbol_for(&node))
        .collect()
}

/// Build a `DocumentSymbol` for a declaration node, or `None` if `node` is not a
/// declaration we surface (pragma/import/using, or any non-decl child). Shared by
/// the top level and by contract bodies, so members and file items map identically.
fn symbol_for(node: &SyntaxNode) -> Option<DocumentSymbol> {
    use SyntaxKind::*;
    let sym = match node.kind() {
        CONTRACT_DEF => contract_symbol(ContractDef::cast(node.clone())?),
        FUNCTION_DEF => function_symbol(FunctionDef::cast(node.clone())?),
        MODIFIER_DEF => leaf(
            node,
            SymbolKind::Modifier,
            ModifierDef::cast(node.clone())?.name(),
        ),
        CONSTRUCTOR_DEF => constructor_symbol(node),
        STATE_VAR_DEF => leaf(
            node,
            SymbolKind::StateVariable,
            StateVarDef::cast(node.clone())?.name(),
        ),
        STRUCT_DEF => struct_symbol(StructDef::cast(node.clone())?),
        ENUM_DEF => leaf(node, SymbolKind::Enum, EnumDef::cast(node.clone())?.name()),
        EVENT_DEF => leaf(
            node,
            SymbolKind::Event,
            EventDef::cast(node.clone())?.name(),
        ),
        ERROR_DEF => leaf(
            node,
            SymbolKind::Error,
            ErrorDef::cast(node.clone())?.name(),
        ),
        // A user-defined value type (`type Price is uint128`) has no clean LSP kind;
        // it is a user-defined type, so we approximate with `Struct`.
        USER_DEFINED_VALUE_TYPE => leaf(
            node,
            SymbolKind::Struct,
            UserDefinedValueType::cast(node.clone())?.name(),
        ),
        _ => return None,
    };
    Some(sym)
}

fn struct_symbol(s: StructDef) -> DocumentSymbol {
    let children = s.fields().map(struct_field_symbol).collect();
    let (name, selection_range) = name_and_selection(s.syntax(), s.name());
    DocumentSymbol {
        name,
        kind: SymbolKind::Struct,
        range: s.syntax().text_range(),
        selection_range,
        children,
    }
}

fn struct_field_symbol(field: StructField) -> DocumentSymbol {
    leaf(field.syntax(), SymbolKind::Field, field.name())
}

/// A contract/interface/library: kind from `ContractDef::kind`, children = its
/// members (CONTRACT_BODY direct children, in source order).
fn contract_symbol(c: ContractDef) -> DocumentSymbol {
    let kind = match c.kind() {
        ContractKind::Contract => SymbolKind::Contract,
        ContractKind::Interface => SymbolKind::Interface,
        ContractKind::Library => SymbolKind::Library,
    };
    let children = c
        .body()
        .map(|body| {
            body.syntax()
                .children()
                .filter_map(|node| symbol_for(&node))
                .collect()
        })
        .unwrap_or_default();
    let (name, selection_range) = name_and_selection(c.syntax(), c.name());
    DocumentSymbol {
        name,
        kind,
        range: c.syntax().text_range(),
        selection_range,
        children,
    }
}

/// A function. `fallback`/`receive` have no `NAME`; synthesize the label and the
/// selection range from their keyword token.
fn function_symbol(f: FunctionDef) -> DocumentSymbol {
    let (name, selection_range) = match f.name().and_then(|n| n.ident_token()) {
        Some(tok) => (tok.text().to_string(), tok.text_range()),
        None => {
            let kw = token_of(f.syntax(), SyntaxKind::FALLBACK_KW)
                .or_else(|| token_of(f.syntax(), SyntaxKind::RECEIVE_KW));
            match kw {
                Some(t) => (t.text().to_string(), t.text_range()),
                None => (String::new(), f.syntax().text_range()),
            }
        }
    };
    DocumentSymbol {
        name,
        kind: SymbolKind::Function,
        range: f.syntax().text_range(),
        selection_range,
        children: Vec::new(),
    }
}

/// A constructor — no `NAME`; synthesize `"constructor"` and point the cursor at
/// the `constructor` keyword (falling back to the whole node).
fn constructor_symbol(node: &SyntaxNode) -> DocumentSymbol {
    let selection_range = token_of(node, SyntaxKind::CONSTRUCTOR_KW)
        .map(|t| t.text_range())
        .unwrap_or_else(|| node.text_range());
    DocumentSymbol {
        name: "constructor".to_string(),
        kind: SymbolKind::Constructor,
        range: node.text_range(),
        selection_range,
        children: Vec::new(),
    }
}

/// A childless symbol whose name comes from a typed `Name` accessor.
fn leaf(node: &SyntaxNode, kind: SymbolKind, name: Option<Name>) -> DocumentSymbol {
    let (name, selection_range) = name_and_selection(node, name);
    DocumentSymbol {
        name,
        kind,
        range: node.text_range(),
        selection_range,
        children: Vec::new(),
    }
}

/// `(label, selection_range)` for a declaration: the `NAME`'s identifier text +
/// the `IDENT` **token** range when present, else an empty label + the whole-node
/// range (only on malformed input). We use the IDENT token's range, NOT the NAME
/// node's, because the M1 tree builder attaches leading trivia (whitespace) to the
/// NAME node — so `NAME.text_range()` would include the preceding space and the
/// editor cursor would land one column early.
fn name_and_selection(node: &SyntaxNode, name: Option<Name>) -> (String, TextRange) {
    if let Some(tok) = name.and_then(|n| n.ident_token()) {
        return (tok.text().to_string(), tok.text_range());
    }
    (String::new(), node.text_range())
}

/// The first direct token child of `node` with the given kind (a local copy of the
/// private `ast::support::token`, which is not exported).
fn token_of(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(|el| el.into_token())
        .find(|t| t.kind() == kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solsp_syntax::parse;

    fn outline(src: &str) -> Vec<DocumentSymbol> {
        document_symbols(&parse(src).syntax())
    }

    #[test]
    fn empty_and_non_source_root() {
        assert!(outline("").is_empty());
        // a non-SOURCE_FILE node yields nothing (defensive cast).
        let nested = parse("contract C {}");
        let name = nested
            .syntax()
            .descendants()
            .find(|n| n.kind() == solsp_syntax::SyntaxKind::NAME)
            .unwrap();
        assert!(document_symbols(&name).is_empty());
    }

    #[test]
    fn contract_members_in_source_order_with_kinds() {
        let src = "contract C {\n\
            uint x;\n\
            function f() public {}\n\
            struct S { uint a; }\n\
            event E();\n\
            error Er();\n\
            modifier m() { _; }\n\
            constructor() {}\n\
            enum En { A }\n\
            type T is uint;\n\
        }";
        let syms = outline(src);
        assert_eq!(syms.len(), 1);
        let c = &syms[0];
        assert_eq!(c.name, "C");
        assert_eq!(c.kind, SymbolKind::Contract);
        // selection_range (the name) is inside the full range (the whole def) and
        // is EXACTLY the identifier — no leading trivia (the NAME node would carry
        // the preceding whitespace; we use the IDENT token's range instead).
        assert!(c.range.contains_range(c.selection_range));
        assert_ne!(c.range, c.selection_range);
        assert_eq!(&src[c.selection_range], "C");
        // a function symbol's selection_range is likewise the bare identifier.
        let f = c
            .children
            .iter()
            .find(|s| s.kind == SymbolKind::Function)
            .unwrap();
        assert_eq!(&src[f.selection_range], "f");
        let kinds: Vec<SymbolKind> = c.children.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                SymbolKind::StateVariable, // uint x
                SymbolKind::Function,      // function f
                SymbolKind::Struct,        // struct S
                SymbolKind::Event,         // event E
                SymbolKind::Error,         // error Er
                SymbolKind::Modifier,      // modifier m
                SymbolKind::Constructor,   // constructor
                SymbolKind::Enum,          // enum En
                SymbolKind::Struct,        // type T  (UDVT -> Struct)
            ]
        );
        let ctor = c
            .children
            .iter()
            .find(|s| s.kind == SymbolKind::Constructor)
            .unwrap();
        assert_eq!(ctor.name, "constructor"); // synthesized label
        let s = c
            .children
            .iter()
            .find(|s| s.kind == SymbolKind::Struct)
            .unwrap();
        assert_eq!(s.children.len(), 1);
        assert_eq!(s.children[0].name, "a");
        assert_eq!(s.children[0].kind, SymbolKind::Field);
        assert_eq!(&src[s.children[0].selection_range], "a");
    }

    #[test]
    fn top_level_kinds_and_synthesized_function_labels() {
        let src = "interface I {}\n\
            library L {}\n\
            function free() {}\n\
            contract C { fallback() external {} receive() external payable {} }";
        let syms = outline(src);
        assert_eq!(syms[0].kind, SymbolKind::Interface);
        assert_eq!(syms[0].name, "I");
        assert_eq!(syms[1].kind, SymbolKind::Library);
        assert_eq!(syms[2].kind, SymbolKind::Function); // free function
        assert_eq!(syms[2].name, "free");
        let c = syms
            .iter()
            .find(|s| s.kind == SymbolKind::Contract)
            .unwrap();
        let labels: Vec<&str> = c.children.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(labels, vec!["fallback", "receive"]); // no NAME node ⇒ keyword labels
    }
}
