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

    #[test]
    fn parses_expression_precedence_in_initializer() {
        // Precedence: `*` binds tighter than `+`; `**` is right-assoc and binds
        // tightest; parentheses group. Wired through a state-var initializer.
        let src = "contract C { uint x = 1 + 2 * 3 ** 4 - (a || b); }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("STATE_VAR_DEF@"));
        assert!(dump.contains("BIN_EXPR@"));
        assert!(dump.contains("LITERAL_EXPR@")); // the numeric literals
        assert!(dump.contains("PATH_EXPR@")); // `a`, `b`
        assert!(dump.contains("PAREN_EXPR@")); // `( … )`
        assert_eq!(p.syntax().text().to_string(), src); // lossless
    }

    #[test]
    fn parses_tuple_expression_initializer() {
        let src = "contract C { uint x = (1, 2, foo); }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("TUPLE_EXPR@"));
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_rich_expression() {
        // Calls (positional + named + options), index + slice, member access,
        // prefix/postfix, ternary, new, type(), array literal — all inside one
        // state-var initializer so we don't need statements yet.
        let src = "contract C { uint x = \
            a.b.c{value: 1, gas: g}(p, q) \
            + f({to: x, amount: y}) \
            + arr[0] + data[1:n] \
            + (cond ? -m++ : ~k) \
            + uint8(z) + new Token(1) + type(uint).max \
            + [1, 2, 3][i]; }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        for kind in [
            "CALL_EXPR@",
            "ARG_LIST@",
            "NAMED_ARG_LIST@",
            "CALL_OPTIONS@",
            "INDEX_EXPR@",
            "MEMBER_EXPR@",
            "PREFIX_EXPR@",
            "POSTFIX_EXPR@",
            "TERNARY_EXPR@",
            "NEW_EXPR@",
            "TYPE_EXPR@",
            "ARRAY_EXPR@",
        ] {
            assert!(dump.contains(kind), "missing {kind} in:\n{dump}");
        }
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_array_size_and_real_call_args() {
        // Array size is now a real expression; modifier/inheritance args are a
        // real ARG_LIST, not a span-skip.
        let src = "contract C is Base(1 + 2) {\n  \
            uint256[2 * N] grid;\n  \
            function f() public mod(a, b) {}\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("ARRAY_TYPE@"));
        assert!(dump.contains("ARG_LIST@")); // both Base(...) and mod(...)
        assert!(dump.contains("MODIFIER_INVOCATION@"));
        assert!(dump.contains("INHERITANCE_SPECIFIER@"));
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_function_body_with_locals_and_return() {
        let src = "contract C {\n  \
            function f(uint a) public returns (uint) {\n    \
                uint x = a + 1;\n    \
                uint[] memory ys;\n    \
                (uint p, bool q) = g();\n    \
                x = x * 2;\n    \
                x[0] = 1;\n    \
                a.b.c(x);\n    \
                return x;\n  \
            }\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        for kind in [
            "VAR_DECL_STMT@",
            "VAR_DECL@",
            "EXPR_STMT@",
            "RETURN_STMT@",
            "ASSIGN_EXPR@",
            "CALL_EXPR@",
            "INDEX_EXPR@",
        ] {
            assert!(dump.contains(kind), "missing {kind} in:\n{dump}");
        }
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_break_continue_and_underscore() {
        // `_;` in a modifier body parses as an ordinary EXPR_STMT (PATH_EXPR `_`);
        // there is no dedicated PLACEHOLDER_STMT.
        let src = "contract C {\n  \
            modifier m() { _; }\n  \
            function f() public { break; continue; return; }\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        assert!(dump.contains("BREAK_STMT@"));
        assert!(dump.contains("CONTINUE_STMT@"));
        assert!(dump.contains("RETURN_STMT@"));
        assert!(dump.contains("EXPR_STMT@")); // the `_;`
        assert!(dump.contains("PATH_EXPR@")); // the `_`
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_nested_control_flow() {
        let src = "contract C {\n  \
            function f(uint n) public {\n    \
                for (uint i = 0; i < n; i++) {\n      \
                    if (i == 0) { continue; }\n      \
                    else if (i == 1) { break; }\n      \
                    else { x = i; }\n    \
                }\n    \
                while (n > 0) { n--; }\n    \
                do { n++; } while (n < 10);\n  \
            }\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        for kind in ["IF_STMT@", "FOR_STMT@", "WHILE_STMT@", "DO_WHILE_STMT@"] {
            assert!(dump.contains(kind), "missing {kind} in:\n{dump}");
        }
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_solidity_statements() {
        let src = "contract C {\n  \
            function f() public {\n    \
                emit Transfer(a, b, 1);\n    \
                revert Unauthorized(msg.sender);\n    \
                revert(\"nope\");\n    \
                unchecked { x = x + 1; }\n    \
                try g(x) returns (uint v) { y = v; }\n    \
                catch Error(string memory reason) { z = 0; }\n    \
                catch { z = 1; }\n    \
                assembly { let p := mload(0x40) }\n  \
            }\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        for kind in [
            "EMIT_STMT@",
            "REVERT_STMT@",
            "UNCHECKED_BLOCK@",
            "TRY_STMT@",
            "CATCH_CLAUSE@",
            "ASSEMBLY_STMT@",
        ] {
            assert!(dump.contains(kind), "missing {kind} in:\n{dump}");
        }
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_realistic_contract_with_bodies_losslessly() {
        // The Plan-3 Vault, now with real statement bodies in every function.
        let src = "// SPDX-License-Identifier: MIT\n\
pragma solidity ^0.8.20;\n\
\n\
import {Ownable} from \"@openzeppelin/contracts/access/Ownable.sol\";\n\
\n\
contract Vault is Ownable {\n\
    mapping(address => uint256) public balances;\n\
    uint256 public constant FEE = 1_000;\n\
    uint256 public total = 0;\n\
\n\
    event Deposit(address indexed who, uint256 amount);\n\
    error InsufficientBalance(uint256 have, uint256 want);\n\
\n\
    modifier onlyPositive(uint256 v) {\n\
        require(v > 0, \"non-positive\");\n\
        _;\n\
    }\n\
\n\
    constructor() Ownable(msg.sender) {}\n\
\n\
    function deposit() external payable onlyPositive(msg.value) {\n\
        balances[msg.sender] += msg.value;\n\
        total += msg.value;\n\
        emit Deposit(msg.sender, msg.value);\n\
    }\n\
\n\
    function withdraw(uint256 amount) external {\n\
        uint256 bal = balances[msg.sender];\n\
        if (bal < amount) {\n\
            revert InsufficientBalance(bal, amount);\n\
        }\n\
        unchecked { balances[msg.sender] = bal - amount; }\n\
        for (uint256 i = 0; i < amount; i++) {\n\
            total = total - 1;\n\
        }\n\
        (bool ok, ) = msg.sender.call{value: amount}(\"\");\n\
        require(ok, \"transfer failed\");\n\
    }\n\
\n\
    function balanceOf(address a) external view returns (uint256) {\n\
        return balances[a];\n\
    }\n\
}\n";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        // Lossless on a full, realistic contract WITH bodies.
        assert_eq!(p.syntax().text().to_string(), src);
        let dump = debug_tree(src);
        for kind in [
            "CONTRACT_DEF@",
            "FUNCTION_DEF@",
            "BLOCK@",
            "VAR_DECL_STMT@",
            "VAR_DECL@",
            "EXPR_STMT@",
            "IF_STMT@",
            "FOR_STMT@",
            "RETURN_STMT@",
            "EMIT_STMT@",
            "REVERT_STMT@",
            "UNCHECKED_BLOCK@",
            "ASSIGN_EXPR@",
            "CALL_EXPR@",
            "CALL_OPTIONS@",
            "INDEX_EXPR@",
            "MEMBER_EXPR@",
            "BIN_EXPR@",
        ] {
            assert!(dump.contains(kind), "missing {kind} in:\n{dump}");
        }
    }

    #[test]
    fn statement_bodies_are_total() {
        // Adversarial bodies must never panic and must round-trip losslessly.
        for body in [
            "",                               // empty body
            "if (x) {",                       // unterminated if + block
            "for (;;) {}",                    // empty for header
            "return",                         // return, no expr, no `;`
            "x = ;",                          // assignment missing rhs
            "uint",                           // dangling type, no name/`;`
            "{ { { { } } }",                  // deeply nested, one `}` short
            "a.b.c.d.e.f.g;",                 // long member chain
            "x = a ? b ? c : d : e;",         // nested ternary
            "1 + + + + 2;",                   // operator pile-up
            "delete delete x;",               // stacked prefix
            "assembly { let x := add(1, 2) ", // unterminated assembly
            "try f() {",                      // try with no catch + unterminated
            "[1,2,3",                         // unterminated array literal
            "f({a: 1, b: });",                // named arg missing value
            "(uint a, , bool b) = t;",        // tuple var-decl with a hole
            "_;",                             // modifier placeholder ⇒ EXPR_STMT
        ] {
            let src = format!("contract C {{ function f() public {{ {body} }} }}");
            let p = parse(&src);
            // Totality + losslessness; errors are allowed (these are malformed).
            assert_eq!(
                p.syntax().text().to_string(),
                src,
                "lossy on body: {body:?}"
            );
        }
    }

    #[test]
    fn parses_yul_block_let_assign_call() {
        // `let`, single + multi assignment, function-call statement, dotted path
        // target, and the `return` builtin (a Solidity keyword reused as a Yul
        // callee). No binary operators anywhere — every computation is a call.
        let src = "contract C {\n  \
            function f() public {\n    \
                assembly {\n      \
                    let x := add(1, 2)\n      \
                    let y, z := mload(0x40)\n      \
                    x := mul(x, 3)\n      \
                    x, y := calldataload(0)\n      \
                    sstore(0, x)\n      \
                    return(0, 0x20)\n    \
                }\n  \
            }\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        for kind in [
            "ASSEMBLY_STMT@",
            "YUL_BLOCK@",
            "YUL_VAR_DECL@",
            "YUL_ASSIGNMENT@",
            "YUL_FUNCTION_CALL@",
            "YUL_PATH@",
            "YUL_LITERAL@",
        ] {
            assert!(dump.contains(kind), "missing {kind} in:\n{dump}");
        }
        assert_eq!(p.syntax().text().to_string(), src); // lossless
    }

    #[test]
    fn parses_yul_control_flow() {
        // `if` has no parens/else; `for { init } cond { post } { body }`; `switch`
        // with case literals + default.
        let src = "contract C {\n  \
            function f() public {\n    \
                assembly {\n      \
                    if lt(x, 3) { x := 0 }\n      \
                    for { let i := 0 } lt(i, 10) { i := add(i, 1) } { mstore(i, i) }\n      \
                    switch x\n      \
                    case 0 { y := 1 }\n      \
                    case 1 { y := 2 }\n      \
                    default { y := 0 }\n    \
                }\n  \
            }\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        for kind in [
            "YUL_IF@",
            "YUL_FOR@",
            "YUL_SWITCH@",
            "YUL_CASE@",
            "YUL_DEFAULT@",
        ] {
            assert!(dump.contains(kind), "missing {kind} in:\n{dump}");
        }
        assert_eq!(p.syntax().text().to_string(), src);
    }

    #[test]
    fn parses_yul_function_def_and_flow() {
        // Yul function definitions (params + `-> r` returns), nested control flow,
        // and `leave`/`break`/`continue`.
        let src = "contract C {\n  \
            function f() public {\n    \
                assembly {\n      \
                    function add2(a, b) -> r {\n        \
                        r := add(a, b)\n        \
                        leave\n      \
                    }\n      \
                    function loop() {\n        \
                        for { let i := 0 } lt(i, 10) { i := add(i, 1) } {\n          \
                            if eq(i, 5) { break }\n          \
                            if eq(i, 3) { continue }\n        \
                        }\n      \
                    }\n      \
                    let x := add2(1, 2)\n    \
                }\n  \
            }\n\
        }";
        let p = parse(src);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let dump = debug_tree(src);
        for kind in [
            "YUL_FUNCTION_DEF@",
            "YUL_PARAM_LIST@",
            "YUL_LEAVE@",
            "YUL_BREAK@",
            "YUL_CONTINUE@",
        ] {
            assert!(dump.contains(kind), "missing {kind} in:\n{dump}");
        }
        assert_eq!(p.syntax().text().to_string(), src);
    }
}
