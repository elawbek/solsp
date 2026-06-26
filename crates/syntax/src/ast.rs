//! Typed AST: thin, hand-written wrappers over the untyped [`SyntaxNode`]. Each
//! wrapper is a newtype implementing [`AstNode`] plus accessors like `.name()` or
//! `.functions()` (design §3.5).

use crate::{SyntaxKind, SyntaxNode, SyntaxToken};

/// A typed view over a syntax node of a known kind.
pub trait AstNode {
    fn can_cast(kind: SyntaxKind) -> bool
    where
        Self: Sized;
    fn cast(node: SyntaxNode) -> Option<Self>
    where
        Self: Sized;
    fn syntax(&self) -> &SyntaxNode;
}

/// Generate a typed-AST newtype `pub struct $name(SyntaxNode)` over nodes of a
/// single [`SyntaxKind`], plus its [`AstNode`] impl. `can_cast` is a kind check,
/// `cast` wraps iff the kind matches, `syntax` borrows the inner node. This is the
/// one-line-per-wrapper boilerplate eliminator (design §3.5).
macro_rules! ast_node {
    ($name:ident, $kind:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(SyntaxNode);

        impl AstNode for $name {
            fn can_cast(kind: SyntaxKind) -> bool {
                kind == SyntaxKind::$kind
            }
            fn cast(node: SyntaxNode) -> Option<Self> {
                if Self::can_cast(node.kind()) {
                    Some(Self(node))
                } else {
                    None
                }
            }
            fn syntax(&self) -> &SyntaxNode {
                &self.0
            }
        }
    };
}

/// Generate a typed-AST sum type over several [`SyntaxKind`]s (e.g. `Item`,
/// `Type`). `can_cast` is the OR of the variant kinds; `cast` dispatches on
/// `node.kind()` and wraps the matching leaf wrapper. Each `$ty` must itself be an
/// [`AstNode`] (declared via `ast_node!`).
macro_rules! ast_enum {
    ($name:ident { $($variant:ident($ty:ty) = $kind:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant($ty)),+
        }

        impl AstNode for $name {
            fn can_cast(kind: SyntaxKind) -> bool {
                matches!(kind, $(SyntaxKind::$kind)|+)
            }
            fn cast(node: SyntaxNode) -> Option<Self> {
                let res = match node.kind() {
                    $(SyntaxKind::$kind => Self::$variant(<$ty>::cast(node)?),)+
                    _ => return None,
                };
                Some(res)
            }
            fn syntax(&self) -> &SyntaxNode {
                match self {
                    $(Self::$variant(it) => it.syntax()),+
                }
            }
        }
    };
}

/// Tiny accessor helpers over rowan's child iterators, shared by every wrapper.
/// Kept in a submodule so the call sites read `support::child(...)` etc. Grown
/// across tasks: `token` (Task 1), `children` (Task 2), `child` (Task 3) — each
/// introduced where first used (a `pub(super) fn` with no caller is a clippy
/// `dead_code` error under `-D warnings`).
mod support {
    use super::AstNode;
    use crate::{SyntaxKind, SyntaxNode, SyntaxToken};

    /// The first direct **token** child of `parent` with the given kind.
    pub(super) fn token(parent: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
        parent
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == kind)
    }

    /// All direct **node** children of `parent` castable to `N`, in tree order.
    pub(super) fn children<N: AstNode>(parent: &SyntaxNode) -> impl Iterator<Item = N> {
        parent.children().filter_map(N::cast)
    }

    /// The first direct **node** child of `parent` castable to `N`.
    pub(super) fn child<N: AstNode>(parent: &SyntaxNode) -> Option<N> {
        parent.children().find_map(N::cast)
    }
}

// ---- names -------------------------------------------------------------------

ast_node!(Name, NAME);
ast_node!(NameRef, NAME_REF);

impl Name {
    /// The single `IDENT` token this defining name wraps (grammar `name(p)` bumps
    /// exactly one `IDENT` inside the `NAME` marker).
    pub fn ident_token(&self) -> Option<SyntaxToken> {
        support::token(self.syntax(), SyntaxKind::IDENT)
    }
    /// The identifier text, owned (allocates; outline builds owned strings anyway).
    pub fn text(&self) -> Option<String> {
        self.ident_token().map(|t| t.text().to_string())
    }
}

impl NameRef {
    /// The single `IDENT` token this referencing name wraps (grammar `name_ref(p)`).
    pub fn ident_token(&self) -> Option<SyntaxToken> {
        support::token(self.syntax(), SyntaxKind::IDENT)
    }
    /// The identifier text, owned.
    pub fn text(&self) -> Option<String> {
        self.ident_token().map(|t| t.text().to_string())
    }
}

// ---- source file -------------------------------------------------------------

ast_node!(SourceFile, SOURCE_FILE);

// ---- file-level items --------------------------------------------------------

ast_node!(PragmaDirective, PRAGMA_DIRECTIVE);
ast_node!(ImportDirective, IMPORT_DIRECTIVE);
ast_node!(UsingDirective, USING_DIRECTIVE);
ast_node!(ContractDef, CONTRACT_DEF);
ast_node!(FunctionDef, FUNCTION_DEF);
ast_node!(StructDef, STRUCT_DEF);
ast_node!(EnumDef, ENUM_DEF);
ast_node!(EventDef, EVENT_DEF);
ast_node!(ErrorDef, ERROR_DEF);
ast_node!(UserDefinedValueType, USER_DEFINED_VALUE_TYPE);
ast_node!(StateVarDef, STATE_VAR_DEF);

// A top-level item of a source file. Mirrors `grammar.rs::item`'s dispatch: a
// file-level constant is a `STATE_VAR_DEF` (the `IDENT | MAPPING_KW` arm), and a
// free function is a `FUNCTION_DEF`. `MODIFIER_DEF`/`CONSTRUCTOR_DEF` are NOT here
// — they are contract-body-only members.
ast_enum!(Item {
    Pragma(PragmaDirective) = PRAGMA_DIRECTIVE,
    Import(ImportDirective) = IMPORT_DIRECTIVE,
    Using(UsingDirective) = USING_DIRECTIVE,
    Contract(ContractDef) = CONTRACT_DEF,
    Function(FunctionDef) = FUNCTION_DEF,
    Struct(StructDef) = STRUCT_DEF,
    Enum(EnumDef) = ENUM_DEF,
    Event(EventDef) = EVENT_DEF,
    Error(ErrorDef) = ERROR_DEF,
    Udvt(UserDefinedValueType) = USER_DEFINED_VALUE_TYPE,
    StateVar(StateVarDef) = STATE_VAR_DEF,
});

impl SourceFile {
    /// The file's top-level items, in source order (direct `Item`-castable children
    /// of `SOURCE_FILE`).
    pub fn items(&self) -> impl Iterator<Item = Item> {
        support::children(self.syntax())
    }
}

// ---- contract ----------------------------------------------------------------

ast_node!(ContractBody, CONTRACT_BODY);
ast_node!(InheritanceSpecifier, INHERITANCE_SPECIFIER);
ast_node!(ModifierDef, MODIFIER_DEF);
ast_node!(ConstructorDef, CONSTRUCTOR_DEF);

/// Which `contract`-family keyword introduced a `CONTRACT_DEF`. `abstract` is a
/// modifier on a contract (see `is_abstract`), not a distinct kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractKind {
    Contract,
    Interface,
    Library,
}

impl ContractDef {
    /// The contract's defining name (grammar `contract(p)` calls `name(p)` ⇒ a
    /// direct `NAME` child).
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }

    /// `contract` / `interface` / `library`. The introducer keyword is a direct
    /// token child (bumped by `contract(p)` before the marker completes).
    pub fn kind(&self) -> ContractKind {
        if support::token(self.syntax(), SyntaxKind::INTERFACE_KW).is_some() {
            ContractKind::Interface
        } else if support::token(self.syntax(), SyntaxKind::LIBRARY_KW).is_some() {
            ContractKind::Library
        } else {
            ContractKind::Contract
        }
    }

    /// Whether the contract carries the `abstract` modifier (`abstract contract …`).
    pub fn is_abstract(&self) -> bool {
        support::token(self.syntax(), SyntaxKind::ABSTRACT_KW).is_some()
    }

    /// The brace-delimited body, if present (a direct `CONTRACT_BODY` child).
    pub fn body(&self) -> Option<ContractBody> {
        support::child(self.syntax())
    }

    /// The `is A, B(args)` base specifiers (direct `INHERITANCE_SPECIFIER` children).
    pub fn inheritance_specifiers(&self) -> impl Iterator<Item = InheritanceSpecifier> {
        support::children(self.syntax())
    }

    /// Collect all body members of a given typed kind. A contract has at most one
    /// `CONTRACT_BODY`; returning an owned `Vec` keeps the signature borrow-free and
    /// sidesteps `Option`-of-iterator lifetime gymnastics (allocation is negligible —
    /// outline materializes these anyway).
    fn members<N: AstNode>(&self) -> Vec<N> {
        match self.body() {
            Some(body) => support::children(body.syntax()).collect(),
            None => Vec::new(),
        }
    }

    pub fn functions(&self) -> Vec<FunctionDef> {
        self.members()
    }
    pub fn state_vars(&self) -> Vec<StateVarDef> {
        self.members()
    }
    pub fn structs(&self) -> Vec<StructDef> {
        self.members()
    }
    pub fn enums(&self) -> Vec<EnumDef> {
        self.members()
    }
    pub fn events(&self) -> Vec<EventDef> {
        self.members()
    }
    pub fn errors(&self) -> Vec<ErrorDef> {
        self.members()
    }
    pub fn modifiers(&self) -> Vec<ModifierDef> {
        self.members()
    }
    pub fn constructors(&self) -> Vec<ConstructorDef> {
        self.members()
    }
    pub fn user_defined_value_types(&self) -> Vec<UserDefinedValueType> {
        self.members()
    }
}

// ---- types -------------------------------------------------------------------

ast_node!(PathType, PATH_TYPE);
ast_node!(MappingType, MAPPING_TYPE);
ast_node!(ArrayType, ARRAY_TYPE);
ast_node!(FunctionType, FUNCTION_TYPE);

// A type name node, as built by `grammar.rs::type_name`. Deep navigation (mapping
// key/value, array element/size, function-type params) is deferred to M2; outline
// renders a type via `self.syntax().text()`.
ast_enum!(Type {
    Path(PathType) = PATH_TYPE,
    Mapping(MappingType) = MAPPING_TYPE,
    Array(ArrayType) = ARRAY_TYPE,
    Function(FunctionType) = FUNCTION_TYPE,
});

// ---- params, fields, variants, blocks ----------------------------------------

ast_node!(ParamList, PARAM_LIST);
ast_node!(Param, PARAM);
ast_node!(StructField, STRUCT_FIELD);
ast_node!(EnumVariant, ENUM_VARIANT);
ast_node!(Block, BLOCK);

impl ParamList {
    /// The parameters, in order (direct `PARAM` children).
    pub fn params(&self) -> impl Iterator<Item = Param> {
        support::children(self.syntax())
    }
}

impl Param {
    /// The parameter's type (the leading `type_name` ⇒ a direct `Type` child).
    pub fn ty(&self) -> Option<Type> {
        support::child(self.syntax())
    }
    /// The parameter's bound name, if any. Soft modifiers like `indexed` are bumped
    /// as bare `IDENT` tokens (not `NAME` nodes) by `grammar.rs::param`, so the only
    /// `NAME` child is the real parameter name.
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
}

impl StructField {
    pub fn ty(&self) -> Option<Type> {
        support::child(self.syntax())
    }
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
}

impl EnumVariant {
    /// The variant's name (`grammar.rs::enum_def` wraps a single `NAME` in each
    /// `ENUM_VARIANT`).
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
}

// ---- member detail accessors -------------------------------------------------

impl FunctionDef {
    /// The function name. `None` for `fallback`/`receive` (grammar emits no `NAME`).
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
    /// The parameter list. Normally the **first** `PARAM_LIST` child. But on the
    /// error-recovery path where the parameter `(` is missing (`function f returns
    /// (uint) {}`, a common mid-edit state), the lone `PARAM_LIST` belongs to
    /// `returns`, not the params — so we use the `RETURNS_KW` token to disambiguate:
    /// with `returns` present and only one list, the params are absent (`None`).
    pub fn param_list(&self) -> Option<ParamList> {
        let mut lists = support::children::<ParamList>(self.syntax());
        let first = lists.next()?;
        let has_returns = support::token(self.syntax(), SyntaxKind::RETURNS_KW).is_some();
        if has_returns && lists.next().is_none() {
            None // the single list is the returns list; params are missing
        } else {
            Some(first)
        }
    }
    /// The `returns (...)` list, if present. Gated on the `RETURNS_KW` token: with
    /// two `PARAM_LIST`s the returns list is the second; with one (params `(`
    /// missing), the lone list IS the returns list.
    pub fn return_param_list(&self) -> Option<ParamList> {
        support::token(self.syntax(), SyntaxKind::RETURNS_KW)?; // no `returns` ⇒ no list
        let mut lists = support::children::<ParamList>(self.syntax());
        match (lists.next(), lists.next()) {
            (Some(_params), Some(ret)) => Some(ret),
            (Some(only), None) => Some(only),
            _ => None,
        }
    }
    /// The body block, if the function is defined (vs. a `;`-terminated declaration).
    pub fn body(&self) -> Option<Block> {
        support::child(self.syntax())
    }
}

impl ModifierDef {
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
    pub fn param_list(&self) -> Option<ParamList> {
        support::child(self.syntax())
    }
    pub fn body(&self) -> Option<Block> {
        support::child(self.syntax())
    }
}

impl ConstructorDef {
    pub fn param_list(&self) -> Option<ParamList> {
        support::child(self.syntax())
    }
    pub fn body(&self) -> Option<Block> {
        support::child(self.syntax())
    }
}

impl StateVarDef {
    /// The declared type — the leading `type_name`, a direct `Type` child (the
    /// initializer expression, if any, is a different child kind and is not a `Type`).
    pub fn ty(&self) -> Option<Type> {
        support::child(self.syntax())
    }
    /// The variable's name (a direct `NAME` child, after the type and modifiers).
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
}

impl StructDef {
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
    pub fn fields(&self) -> impl Iterator<Item = StructField> {
        support::children(self.syntax())
    }
}

impl EnumDef {
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
    pub fn variants(&self) -> impl Iterator<Item = EnumVariant> {
        support::children(self.syntax())
    }
}

impl EventDef {
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
    pub fn param_list(&self) -> Option<ParamList> {
        support::child(self.syntax())
    }
}

impl ErrorDef {
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
    pub fn param_list(&self) -> Option<ParamList> {
        support::child(self.syntax())
    }
}

impl UserDefinedValueType {
    pub fn name(&self) -> Option<Name> {
        support::child(self.syntax())
    }
    /// The underlying value type (after `is`); a direct `Type` child.
    pub fn ty(&self) -> Option<Type> {
        support::child(self.syntax())
    }
}

impl InheritanceSpecifier {
    /// The base contract's path (`grammar.rs::inheritance_specifier` emits a
    /// `PATH_TYPE` for the name; any `(args)` is a separate `ARG_LIST` child).
    pub fn path_type(&self) -> Option<PathType> {
        support::child(self.syntax())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, SyntaxKind};

    #[test]
    fn casts_source_file_and_reads_a_name() {
        let p = parse("contract C {}");
        // the root green tree casts to SourceFile
        let file = SourceFile::cast(p.syntax()).expect("root casts to SourceFile");
        assert_eq!(file.syntax().kind(), SyntaxKind::SOURCE_FILE);
        // a NAME node anywhere in the tree reads its identifier text through the wrapper
        let name = p
            .syntax()
            .descendants()
            .find_map(Name::cast)
            .expect("the contract has a NAME node");
        assert_eq!(name.text().as_deref(), Some("C"));
        assert_eq!(name.syntax().kind(), SyntaxKind::NAME);
    }

    #[test]
    fn walks_file_level_items() {
        // pragma, a contract, and a file-level struct — the order the grammar emits.
        let src = "pragma solidity ^0.8.20;\ncontract C {}\nstruct S { uint x; }\n";
        let p = parse(src);
        let file = SourceFile::cast(p.syntax()).unwrap();
        let kinds: Vec<SyntaxKind> = file.items().map(|it| it.syntax().kind()).collect();
        assert_eq!(
            kinds,
            vec![
                SyntaxKind::PRAGMA_DIRECTIVE,
                SyntaxKind::CONTRACT_DEF,
                SyntaxKind::STRUCT_DEF,
            ]
        );
        // the enum discriminates the contract variant
        assert!(matches!(file.items().nth(1), Some(Item::Contract(_))));
    }

    #[test]
    fn reads_contract_header_and_member_counts() {
        let src = "abstract contract C is A, B {\n  \
            uint x;\n  \
            function f() public {}\n  \
            function g() public {}\n  \
            struct P { uint a; }\n\
        }";
        let p = parse(src);
        let file = SourceFile::cast(p.syntax()).unwrap();
        let c = file
            .items()
            .find_map(|it| match it {
                Item::Contract(c) => Some(c),
                _ => None,
            })
            .unwrap();
        assert_eq!(c.name().and_then(|n| n.text()).as_deref(), Some("C"));
        assert!(matches!(c.kind(), ContractKind::Contract));
        assert!(c.is_abstract());
        assert_eq!(c.inheritance_specifiers().count(), 2);
        assert!(c.body().is_some());
        assert_eq!(c.functions().len(), 2);
        assert_eq!(c.state_vars().len(), 1);
        assert_eq!(c.structs().len(), 1);
        assert_eq!(c.enums().len(), 0);
        assert_eq!(c.events().len(), 0);
        assert_eq!(c.errors().len(), 0);
        assert_eq!(c.modifiers().len(), 0);
        assert_eq!(c.constructors().len(), 0);
        assert_eq!(c.user_defined_value_types().len(), 0);
    }

    #[test]
    fn reads_member_details() {
        let src = "contract C {\n  \
            mapping(address => uint256) public balances;\n  \
            uint256 constant FEE = 1;\n  \
            struct Account { uint256 balance; bool frozen; }\n  \
            enum Status { Open, Closed }\n  \
            event Deposit(address indexed who, uint256 amount);\n  \
            error Bad(uint256 x);\n  \
            type Price is uint128;\n  \
            modifier m(uint256 v) { _; }\n  \
            constructor(uint256 a) {}\n  \
            function f(uint256 p, bool q) public returns (uint256) {}\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let file = SourceFile::cast(p.syntax()).unwrap();
        let c = file
            .items()
            .find_map(|it| match it {
                Item::Contract(c) => Some(c),
                _ => None,
            })
            .unwrap();

        // state vars: names in order, and the first var's type is a mapping.
        let svs = c.state_vars();
        let sv_names: Vec<String> = svs
            .iter()
            .filter_map(|v| v.name().and_then(|n| n.text()))
            .collect();
        assert_eq!(sv_names, vec!["balances".to_string(), "FEE".to_string()]);
        assert!(matches!(svs[0].ty(), Some(Type::Mapping(_))));

        // function: name, parameter names, a returns list, and a body block.
        let fns = c.functions();
        let f = &fns[0];
        assert_eq!(f.name().and_then(|n| n.text()).as_deref(), Some("f"));
        let params: Vec<String> = f
            .param_list()
            .unwrap()
            .params()
            .filter_map(|prm| prm.name().and_then(|n| n.text()))
            .collect();
        assert_eq!(params, vec!["p".to_string(), "q".to_string()]);
        assert!(f.return_param_list().is_some());
        assert!(f.body().is_some());
        assert!(matches!(
            f.param_list().unwrap().params().next().unwrap().ty(),
            Some(Type::Path(_))
        ));

        // struct fields, enum variants.
        let structs = c.structs();
        let fields: Vec<String> = structs[0]
            .fields()
            .filter_map(|fl| fl.name().and_then(|n| n.text()))
            .collect();
        assert_eq!(fields, vec!["balance".to_string(), "frozen".to_string()]);
        let enums = c.enums();
        let variants: Vec<String> = enums[0]
            .variants()
            .filter_map(|v| v.name().and_then(|n| n.text()))
            .collect();
        assert_eq!(variants, vec!["Open".to_string(), "Closed".to_string()]);

        // event params — `indexed` is a soft modifier, so the bound name is `who`.
        let events = c.events();
        let ev_params: Vec<String> = events[0]
            .param_list()
            .unwrap()
            .params()
            .filter_map(|prm| prm.name().and_then(|n| n.text()))
            .collect();
        assert_eq!(ev_params, vec!["who".to_string(), "amount".to_string()]);

        // error param.
        let errors = c.errors();
        assert_eq!(
            errors[0].name().and_then(|n| n.text()).as_deref(),
            Some("Bad")
        );
        assert_eq!(errors[0].param_list().unwrap().params().count(), 1);

        // UDVT: name + underlying type.
        let udvts = c.user_defined_value_types();
        assert_eq!(
            udvts[0].name().and_then(|n| n.text()).as_deref(),
            Some("Price")
        );
        assert!(matches!(udvts[0].ty(), Some(Type::Path(_))));

        // modifier + constructor.
        let mods = c.modifiers();
        assert_eq!(mods[0].name().and_then(|n| n.text()).as_deref(), Some("m"));
        assert!(mods[0].param_list().is_some());
        assert!(mods[0].body().is_some());
        let ctors = c.constructors();
        assert!(ctors[0].param_list().is_some());
        assert!(ctors[0].body().is_some());
    }

    #[test]
    fn function_param_vs_returns_disambiguation() {
        // Well-formed: params present, returns present.
        let f = first_function("contract C { function g() public returns (uint) {} }");
        assert!(f.param_list().is_some());
        assert!(f.return_param_list().is_some());

        // No returns: param_list is the only list; return_param_list is None.
        let f = first_function("contract C { function h(uint a) public {} }");
        assert!(f.param_list().is_some());
        assert_eq!(f.param_list().unwrap().params().count(), 1);
        assert!(f.return_param_list().is_none());

        // Error recovery — the parameter `(` is missing (mid-edit). The lone
        // PARAM_LIST belongs to `returns`, so param_list() must be None (not the
        // returns list) and return_param_list() must be the returns list.
        let f = first_function("contract C { function k returns (uint) {} }");
        assert!(f.param_list().is_none());
        assert!(f.return_param_list().is_some());
    }

    #[test]
    fn walks_realistic_contract_via_typed_ast() {
        let src = "// SPDX-License-Identifier: MIT\n\
pragma solidity ^0.8.20;\n\
\n\
import {Ownable} from \"@openzeppelin/contracts/access/Ownable.sol\";\n\
\n\
contract Vault is Ownable {\n\
    mapping(address => uint256) public balances;\n\
    uint256 public constant FEE = 1_000;\n\
    address immutable owner;\n\
\n\
    event Deposit(address indexed who, uint256 amount);\n\
    error InsufficientBalance(uint256 have, uint256 want);\n\
\n\
    struct Account { uint256 balance; bool frozen; }\n\
    enum Status { Open, Closed }\n\
\n\
    modifier onlyPositive(uint256 v) { _; }\n\
\n\
    constructor() Ownable(msg.sender) {}\n\
\n\
    function deposit() external payable onlyPositive(msg.value) {}\n\
    function balanceOf(address a) external view returns (uint256) {}\n\
}\n";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let file = SourceFile::cast(p.syntax()).unwrap();

        // file-level items: pragma, import, contract (SPDX is a leading comment/trivia).
        let item_kinds: Vec<SyntaxKind> = file.items().map(|i| i.syntax().kind()).collect();
        assert!(item_kinds.contains(&SyntaxKind::PRAGMA_DIRECTIVE));
        assert!(item_kinds.contains(&SyntaxKind::IMPORT_DIRECTIVE));
        assert!(item_kinds.contains(&SyntaxKind::CONTRACT_DEF));

        let c = file
            .items()
            .find_map(|i| match i {
                Item::Contract(c) => Some(c),
                _ => None,
            })
            .unwrap();
        assert_eq!(c.name().and_then(|n| n.text()).as_deref(), Some("Vault"));
        assert!(matches!(c.kind(), ContractKind::Contract));
        assert!(!c.is_abstract());
        assert_eq!(c.inheritance_specifiers().count(), 1);

        // functions are FUNCTION_DEF members; the constructor is separate.
        let fn_names: Vec<String> = c
            .functions()
            .iter()
            .filter_map(|f| f.name().and_then(|n| n.text()))
            .collect();
        assert_eq!(
            fn_names,
            vec!["deposit".to_string(), "balanceOf".to_string()]
        );
        assert_eq!(c.constructors().len(), 1);

        // every other member-kind collection.
        let sv_names: Vec<String> = c
            .state_vars()
            .iter()
            .filter_map(|v| v.name().and_then(|n| n.text()))
            .collect();
        assert_eq!(
            sv_names,
            vec![
                "balances".to_string(),
                "FEE".to_string(),
                "owner".to_string()
            ]
        );
        assert_eq!(c.events().len(), 1);
        assert_eq!(c.errors().len(), 1);
        assert_eq!(c.structs().len(), 1);
        assert_eq!(c.enums().len(), 1);
        assert_eq!(c.modifiers().len(), 1);

        // a base specifier path resolves to its name text.
        let base = c.inheritance_specifiers().next().unwrap();
        assert_eq!(
            base.path_type()
                .and_then(|pt| pt.syntax().first_child().and_then(NameRef::cast))
                .and_then(|nr| nr.text())
                .as_deref(),
            Some("Ownable")
        );
    }

    fn first_function(src: &str) -> FunctionDef {
        let p = parse(src);
        SourceFile::cast(p.syntax())
            .unwrap()
            .items()
            .find_map(|it| match it {
                Item::Contract(c) => c.functions().into_iter().next(),
                _ => None,
            })
            .unwrap()
    }
}
