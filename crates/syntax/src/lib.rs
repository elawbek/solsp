//! `solsp-syntax` — lossless parser for Solidity (rust-analyzer style).
//!
//! Pipeline (see design §3):
//! ```text
//! text -> lexer -> [tokens incl. trivia] -> parser -> [events] -> tree builder -> rowan green tree
//! ```
//! This crate is **pure**: it knows nothing about LSP or salsa. The single entry
//! point is [`parse`], a total function that never panics and never fails — errors
//! are reported in [`Parse::errors`] and the tree always spans the whole input.

mod event;
mod input;
mod syntax_kind;

pub mod ast;
mod grammar;
pub mod lexer;
pub mod parser;

pub use syntax_kind::SyntaxKind;

use rowan::{GreenNode, TextRange};

/// The rowan language marker for Solidity. Ties our [`SyntaxKind`] to rowan trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SolidityLanguage {}

impl rowan::Language for SolidityLanguage {
    type Kind = SyntaxKind;
    fn kind_from_raw(raw: rowan::SyntaxKind) -> SyntaxKind {
        SyntaxKind::from_u16(raw.0)
    }
    fn kind_to_raw(kind: SyntaxKind) -> rowan::SyntaxKind {
        rowan::SyntaxKind(kind.to_u16())
    }
}

pub type SyntaxNode = rowan::SyntaxNode<SolidityLanguage>;
pub type SyntaxToken = rowan::SyntaxToken<SolidityLanguage>;
pub type SyntaxElement = rowan::SyntaxElement<SolidityLanguage>;

/// A syntax error with the source range it covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SyntaxError {
    pub message: String,
    pub range: TextRange,
}

/// The result of parsing: an immutable green tree plus any syntax errors.
#[derive(Debug, Clone)]
pub struct Parse {
    green: GreenNode,
    errors: Vec<SyntaxError>,
}

impl Parse {
    /// The typed-untyped root node of the tree.
    pub fn syntax(&self) -> SyntaxNode {
        SyntaxNode::new_root(self.green.clone())
    }
    pub fn errors(&self) -> &[SyntaxError] {
        &self.errors
    }
}

/// Parse Solidity source into a lossless syntax tree. Total: never panics, never
/// fails; problems are surfaced via [`Parse::errors`] and the tree spans the whole
/// input byte-for-byte.
pub fn parse(text: &str) -> Parse {
    let tokens = lexer::tokenize(text);
    let input = input::Input::new(&tokens);
    let mut p = parser::Parser::new(&input);
    grammar::source_file(&mut p);
    let events = p.finish();
    let (green, errors) = event::build_tree(text, &tokens, events);
    Parse { green, errors }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic tree dump for assertions: `KIND@start..end` per line, tokens
    /// show their text. Independent of rowan's own Debug formatting.
    fn debug_tree(text: &str) -> String {
        use std::fmt::Write;
        let node = parse(text).syntax();
        let mut out = String::new();
        fn go(out: &mut String, el: SyntaxElement, indent: usize) {
            for _ in 0..indent {
                out.push_str("  ");
            }
            match el {
                rowan::NodeOrToken::Node(n) => {
                    let r = n.text_range();
                    let _ = writeln!(
                        out,
                        "{:?}@{}..{}",
                        n.kind(),
                        u32::from(r.start()),
                        u32::from(r.end())
                    );
                    for c in n.children_with_tokens() {
                        go(out, c, indent + 1);
                    }
                }
                rowan::NodeOrToken::Token(t) => {
                    let r = t.text_range();
                    let _ = writeln!(
                        out,
                        "{:?}@{}..{} {:?}",
                        t.kind(),
                        u32::from(r.start()),
                        u32::from(r.end()),
                        t.text()
                    );
                }
            }
        }
        go(&mut out, rowan::NodeOrToken::Node(node), 0);
        out
    }

    #[test]
    fn parse_is_total_and_lossless() {
        for src in [
            "",
            "contract C {}",
            "this is not solidity !!!",
            "  \n// leading + trailing\ncontract C {}  \n",
            "// only a comment",
            "contract C { string s = unicode\"héllo 🌍\"; }",
            "contract C {",
            "contract C is A, , B {}", // empty base between commas → zero-width specifier
            "contract C is A(((",      // unbalanced inheritance args run to EOF
            "contract C is",           // EOF right after `is`
            "import",                  // bare directive keyword at EOF
            "import \"x\"",            // unterminated import (no `;`)
            "using",                   // bare `using` keyword at EOF
            "foo",                     // file-level stray IDENT → state_var_def path
            "function",                // free-function keyword then EOF
        ] {
            assert_eq!(parse(src).syntax().text().to_string(), src);
        }
    }

    #[test]
    fn parses_pragma_and_contract() {
        let src = "pragma solidity ^0.8.20;\ncontract C {}";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        // Sanity on structure (not a brittle full snapshot): both items present.
        assert!(dump.contains("PRAGMA_DIRECTIVE@"));
        assert!(dump.contains("CONTRACT_DEF@"));
        assert!(dump.contains("NAME@"));
    }

    #[test]
    fn parses_state_vars_and_paths() {
        let src = "contract C {\n    uint256 x;\n    A.B y = z;\n}";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("CONTRACT_BODY@"));
        assert!(dump.contains("STATE_VAR_DEF@"));
        assert!(dump.contains("PATH_TYPE@"));
        assert!(dump.contains("NAME_REF@")); // type segments are name refs
        assert!(dump.contains("NAME@")); // the variable's own name
        assert_eq!(p.syntax().text().to_string(), src); // lossless
    }

    #[test]
    fn parses_array_mapping_function_types() {
        let src = "contract C {\n  \
            uint256[] a;\n  \
            mapping(address => uint256) bal;\n  \
            mapping(address owner => uint256 amount) named;\n  \
            uint8[2][] grid;\n  \
            mapping(uint256 => function (uint) external returns (bool)) cbs;\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("ARRAY_TYPE@"));
        assert!(dump.contains("MAPPING_TYPE@"));
        assert!(dump.contains("FUNCTION_TYPE@"));
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_functions() {
        let src = "contract C {\n  \
            function f(uint a, uint b) public pure returns (uint) {}\n  \
            function g() external onlyOwner(msg.sender) {}\n  \
            function h(uint x) external view returns (uint);\n  \
            receive() external payable {}\n  \
            fallback() external {}\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("FUNCTION_DEF@"));
        assert!(dump.contains("PARAM_LIST@"));
        assert!(dump.contains("PARAM@"));
        assert!(dump.contains("BLOCK@"));
        assert!(dump.contains("MODIFIER_INVOCATION@"));
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_modifiers_and_constructors() {
        let src = "contract C {\n  \
            modifier onlyOwner() virtual { _; }\n  \
            modifier nonReentrant;\n  \
            constructor(uint x) Ownable(msg.sender) payable {}\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("MODIFIER_DEF@"));
        assert!(dump.contains("CONSTRUCTOR_DEF@"));
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_structs_and_enums() {
        let src = "contract C {\n  \
            struct Point { uint256 x; uint256 y; }\n  \
            enum State { Idle, Running, Done }\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("STRUCT_DEF@"));
        assert!(dump.contains("STRUCT_FIELD@"));
        assert!(dump.contains("ENUM_DEF@"));
        assert!(dump.contains("ENUM_VARIANT@"));
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_events_errors_udvt() {
        let src = "contract C {\n  \
            event Transfer(address indexed from, address indexed to, uint256 value);\n  \
            event Flag() anonymous;\n  \
            error Unauthorized(address caller);\n  \
            type Price is uint128;\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("EVENT_DEF@"));
        assert!(dump.contains("ERROR_DEF@"));
        assert!(dump.contains("USER_DEFINED_VALUE_TYPE@"));
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_inheritance() {
        let src = "abstract contract C is Ownable, IERC20, Base(1, 2) {}";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("INHERITANCE_SPECIFIER@"));
        assert!(dump.matches("INHERITANCE_SPECIFIER@").count() == 3);
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_file_level_items() {
        let src = "// SPDX-License-Identifier: MIT\n\
pragma solidity ^0.8.20;\n\
import {ERC20} from \"@openzeppelin/contracts/token/ERC20/ERC20.sol\";\n\
using SafeMath for uint256;\n\
type Wad is uint256;\n\
uint256 constant CHAIN_ID = 1;\n\
function freeAdd(uint a, uint b) pure returns (uint) {}\n\
contract C is ERC20 {}\n";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("IMPORT_DIRECTIVE@"));
        assert!(dump.contains("USING_DIRECTIVE@"));
        assert!(dump.contains("USER_DEFINED_VALUE_TYPE@"));
        assert!(dump.contains("STATE_VAR_DEF@")); // file-level constant
        assert!(dump.contains("FUNCTION_DEF@")); // free function
        assert!(dump.contains("CONTRACT_DEF@"));
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_realistic_contract_losslessly_without_errors() {
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
        assert_eq!(p.syntax().text().to_string(), src);
        let dump = debug_tree(src);
        for kind in [
            "PRAGMA_DIRECTIVE@",
            "IMPORT_DIRECTIVE@",
            "CONTRACT_DEF@",
            "INHERITANCE_SPECIFIER@",
            "STATE_VAR_DEF@",
            "MAPPING_TYPE@",
            "EVENT_DEF@",
            "ERROR_DEF@",
            "STRUCT_DEF@",
            "ENUM_DEF@",
            "MODIFIER_DEF@",
            "CONSTRUCTOR_DEF@",
            "FUNCTION_DEF@",
            "BLOCK@",
        ] {
            assert!(dump.contains(kind), "missing {kind} in:\n{dump}");
        }
    }

    #[test]
    fn recovers_on_garbage_then_continues() {
        // Leading junk becomes ERROR nodes; the contract after it still parses.
        let src = "@@@ contract C {}";
        let p = parse(src);
        let dump = debug_tree(src);
        assert!(dump.contains("ERROR@"));
        assert!(dump.contains("CONTRACT_DEF@"));
        assert_eq!(p.syntax().text().to_string(), src); // still lossless
    }
}
