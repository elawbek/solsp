//! `solsp-hir` — the item model. Lowers the typed AST into an id-based declaration
//! skeleton (contracts and their members, free items) that later plans resolve names
//! against (scopes, imports, inheritance). Built as a tracked query over `parse`
//! (base-db), so it memoizes and recomputes incrementally (design §7, M2).
//!
//! P2 covers item *signatures* only — no scopes, no bodies, no resolution yet.
//! P3 adds [`resolve`]: single-file lexical name resolution.

pub mod resolve;

use rowan::ast::SyntaxNodePtr;
use rowan::TextRange;
use solsp_base_db::{parse, Db, SourceFile};
use solsp_syntax::{
    ast::{AstNode, ContractDef, ContractKind},
    SolidityLanguage, SyntaxKind, SyntaxNode,
};

/// A stable pointer to a syntax node: its kind + byte range, re-resolvable against a
/// freshly built tree via [`AstPtr::to_node`]. This is how HIR refers back to syntax
/// without holding a (`!Send`) `SyntaxNode` in the salsa database.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AstPtr(SyntaxNodePtr<SolidityLanguage>);

impl AstPtr {
    fn new(node: &SyntaxNode) -> AstPtr {
        AstPtr(SyntaxNodePtr::new(node))
    }

    /// Recover the node this points at from a tree root (the same file's parse).
    pub fn to_node(&self, root: &SyntaxNode) -> SyntaxNode {
        self.0.to_node(root)
    }

    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    pub fn kind(&self) -> SyntaxKind {
        self.0.kind()
    }
}

// `AstPtr` is a fully-owned immutable `Eq` value, so salsa's "fallback" update applies:
// overwrite iff different. SAFETY: `old_pointer` is a valid owned `AstPtr`; we only read
// it via `Eq` and overwrite in place — no borrowed/`'db` data involved.
unsafe impl salsa::Update for AstPtr {
    unsafe fn maybe_update(old_pointer: *mut Self, new_value: Self) -> bool {
        let old = unsafe { &mut *old_pointer };
        if *old == new_value {
            false
        } else {
            *old = new_value;
            true
        }
    }
}

/// What kind of declaration an [`Item`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, salsa::Update)]
pub enum ItemKind {
    Contract,
    Interface,
    Library,
    Function,
    Constructor,
    Modifier,
    StateVariable,
    Struct,
    Enum,
    Event,
    Error,
    /// A user-defined value type (`type Price is uint128`).
    UserType,
}

/// One declaration in the item model. `name`/`name_ptr` are `None` for unnamed items
/// (constructor, `fallback`, `receive`). `children` is non-empty only for contracts
/// (their members, in source order).
#[derive(Debug, Clone, PartialEq, Eq, salsa::Update)]
pub struct Item {
    pub name: Option<String>,
    pub kind: ItemKind,
    /// Pointer to the whole declaration node (its full range).
    pub ptr: AstPtr,
    /// Pointer to the `NAME` node (the go-to-def target), when the item is named.
    pub name_ptr: Option<AstPtr>,
    pub children: Vec<Item>,
}

/// The item skeleton of one file: its top-level items, in source order.
#[derive(Debug, Clone, PartialEq, Eq, salsa::Update)]
pub struct ItemTree {
    pub items: Vec<Item>,
}

/// Lower a file into its item model. Tracked: recomputes only when the file's parse
/// changes, and downstream analysis over it memoizes.
#[salsa::tracked]
pub fn item_tree(db: &dyn Db, file: SourceFile) -> ItemTree {
    let root = parse(db, file).syntax();
    ItemTree {
        items: lower_items(&root),
    }
}

/// Lower every declaration child of `parent` (skipping non-decl nodes), in order.
fn lower_items(parent: &SyntaxNode) -> Vec<Item> {
    parent.children().filter_map(lower_item).collect()
}

/// Lower a single node into an [`Item`], or `None` if it is not a declaration we model.
fn lower_item(node: SyntaxNode) -> Option<Item> {
    use SyntaxKind::*;
    let (kind, children) = match node.kind() {
        CONTRACT_DEF => {
            let c = ContractDef::cast(node.clone())?;
            let kind = match c.kind() {
                ContractKind::Contract => ItemKind::Contract,
                ContractKind::Interface => ItemKind::Interface,
                ContractKind::Library => ItemKind::Library,
            };
            let members = c
                .body()
                .map(|b| lower_items(b.syntax()))
                .unwrap_or_default();
            (kind, members)
        }
        FUNCTION_DEF => (ItemKind::Function, Vec::new()),
        CONSTRUCTOR_DEF => (ItemKind::Constructor, Vec::new()),
        MODIFIER_DEF => (ItemKind::Modifier, Vec::new()),
        STATE_VAR_DEF => (ItemKind::StateVariable, Vec::new()),
        STRUCT_DEF => (ItemKind::Struct, Vec::new()),
        ENUM_DEF => (ItemKind::Enum, Vec::new()),
        EVENT_DEF => (ItemKind::Event, Vec::new()),
        ERROR_DEF => (ItemKind::Error, Vec::new()),
        USER_DEFINED_VALUE_TYPE => (ItemKind::UserType, Vec::new()),
        _ => return None,
    };
    let (name, name_ptr) = name_info(&node);
    Some(Item {
        name,
        kind,
        ptr: AstPtr::new(&node),
        name_ptr,
        children,
    })
}

/// `(name text, NAME-node pointer)` for a declaration: the first direct `NAME` child's
/// `IDENT` token text plus a pointer to that `NAME` node. `(None, None)` for unnamed
/// declarations (constructor / `fallback` / `receive`). We read the `IDENT` token's
/// text (not the `NAME` node's) because the M1 tree builder attaches leading trivia to
/// the `NAME` node.
fn name_info(node: &SyntaxNode) -> (Option<String>, Option<AstPtr>) {
    let Some(name) = node.children().find(|n| n.kind() == SyntaxKind::NAME) else {
        return (None, None);
    };
    let text = name
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == SyntaxKind::IDENT)
        .map(|t| t.text().to_string());
    (text, Some(AstPtr::new(&name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use salsa::Setter;
    use solsp_base_db::RootDatabase;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tree(db: &RootDatabase, src: &str) -> ItemTree {
        let f = SourceFile::new(db, "t.sol".to_string(), src.to_string());
        item_tree(db, f)
    }

    #[test]
    fn lowers_contract_members_in_source_order() {
        let db = RootDatabase::default();
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
        let t = tree(&db, src);
        assert_eq!(t.items.len(), 1);
        let c = &t.items[0];
        assert_eq!(c.kind, ItemKind::Contract);
        assert_eq!(c.name.as_deref(), Some("C"));

        let kinds: Vec<ItemKind> = c.children.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ItemKind::StateVariable,
                ItemKind::Function,
                ItemKind::Struct,
                ItemKind::Event,
                ItemKind::Error,
                ItemKind::Modifier,
                ItemKind::Constructor,
                ItemKind::Enum,
                ItemKind::UserType,
            ]
        );
        // constructor is unnamed
        let ctor = c
            .children
            .iter()
            .find(|i| i.kind == ItemKind::Constructor)
            .unwrap();
        assert_eq!(ctor.name, None);
        assert_eq!(ctor.name_ptr, None);
    }

    #[test]
    fn top_level_kinds_and_name_ptr_roundtrip() {
        let db = RootDatabase::default();
        let src = "interface I {}\nlibrary L {}\nfunction free() {}\ncontract C {}";
        let f = SourceFile::new(&db, "t.sol".to_string(), src.to_string());
        let t = item_tree(&db, f);
        let kinds: Vec<ItemKind> = t.items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ItemKind::Interface,
                ItemKind::Library,
                ItemKind::Function,
                ItemKind::Contract,
            ]
        );

        // name_ptr resolves back to the NAME node whose ident is the declared name.
        let root = parse(&db, f).syntax();
        let i = &t.items[0];
        let name_node = i.name_ptr.as_ref().unwrap().to_node(&root);
        assert_eq!(name_node.kind(), SyntaxKind::NAME);
        assert_eq!(name_node.text().to_string().trim(), "I");
        // the whole-item ptr covers the declaration.
        assert_eq!(i.ptr.to_node(&root).kind(), SyntaxKind::CONTRACT_DEF);
    }

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    #[salsa::tracked]
    fn item_count(db: &dyn Db, file: SourceFile) -> usize {
        RUNS.fetch_add(1, Ordering::SeqCst);
        item_tree(db, file).items.len()
    }

    #[test]
    fn item_tree_is_memoized_and_incremental() {
        let mut db = RootDatabase::default();
        let f1 = SourceFile::new(&db, "a.sol".to_string(), "contract C {}".to_string());
        let f2 = SourceFile::new(&db, "b.sol".to_string(), "contract D {}".to_string());

        RUNS.store(0, Ordering::SeqCst);
        assert_eq!(item_count(&db, f1), 1);
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);

        // unrelated edit → f1 memoized
        f2.set_text(&mut db).to("contract D2 {}".to_string());
        assert_eq!(item_count(&db, f1), 1);
        assert_eq!(RUNS.load(Ordering::SeqCst), 1);

        // f1's own edit (add an item) → recompute
        f1.set_text(&mut db)
            .to("contract C {}\ncontract C2 {}".to_string());
        assert_eq!(item_count(&db, f1), 2);
        assert_eq!(RUNS.load(Ordering::SeqCst), 2);
    }
}
