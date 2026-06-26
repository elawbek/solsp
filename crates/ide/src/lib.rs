//! `solsp-ide` — IDE features over the syntax tree.
//!
//! Every feature is a pure function `(&SyntaxNode, &LineIndex) -> bare data`. The
//! data is **not** in LSP types — `solsp-server` maps it to the protocol. This
//! keeps features trivially testable and decoupled from the wire format (design §4).

pub mod diagnostics;
pub mod document_symbols;
pub mod line_index;
pub mod semantic_tokens;

pub use line_index::{LineCol, LineIndex};

#[cfg(test)]
mod tests {
    use crate::diagnostics::diagnostics;
    use crate::document_symbols::{document_symbols, SymbolKind};
    use crate::line_index::{LineCol, LineIndex};
    use crate::semantic_tokens::{semantic_tokens, TokenType};
    use solsp_syntax::parse;

    const VAULT: &str = "// SPDX-License-Identifier: MIT\n\
pragma solidity ^0.8.20;\n\
\n\
contract Vault is Ownable {\n\
    mapping(address => uint256) public balances;\n\
    uint256 public constant FEE = 1_000;\n\
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
    function deposit() external payable onlyPositive(msg.value) {\n\
        balances[msg.sender] += msg.value;\n\
        emit Deposit(msg.sender, msg.value);\n\
    }\n\
    function balanceOf(address a) external view returns (uint256) {\n\
        return balances[a];\n\
    }\n\
}\n";

    #[test]
    fn all_features_compose_over_a_realistic_contract() {
        let p = parse(VAULT);
        assert!(p.errors().is_empty(), "unexpected errors: {:?}", p.errors());
        let root = p.syntax();

        // Feature 1 — diagnostics: a clean parse ⇒ none.
        assert!(diagnostics(&p).is_empty());

        // Infra — LineIndex round-trips on a UTF-16-shifting comment line.
        let li = LineIndex::new(VAULT);
        for el in root.descendants_with_tokens() {
            let r = el.text_range();
            let lc = li.line_col(r.start());
            assert_eq!(li.offset(lc), Some(r.start()));
        }
        assert_eq!(li.line_col(0.into()), LineCol { line: 0, col: 0 });

        // Feature 2 — document symbols.
        let syms = document_symbols(&root);
        assert_eq!(syms.len(), 1);
        let c = &syms[0];
        assert_eq!(c.name, "Vault");
        assert_eq!(c.kind, SymbolKind::Contract);
        let kinds: Vec<SymbolKind> = c.children.iter().map(|s| s.kind).collect();
        for k in [
            SymbolKind::StateVariable,
            SymbolKind::Event,
            SymbolKind::Error,
            SymbolKind::Struct,
            SymbolKind::Enum,
            SymbolKind::Modifier,
            SymbolKind::Constructor,
            SymbolKind::Function,
        ] {
            assert!(kinds.contains(&k), "outline missing {k:?}");
        }
        let labels: Vec<&str> = c.children.iter().map(|s| s.name.as_str()).collect();
        assert!(labels.contains(&"deposit"));
        assert!(labels.contains(&"balanceOf"));
        assert!(labels.contains(&"constructor"));

        // Feature 3 — semantic tokens.
        let toks = semantic_tokens(&root);
        let pick = |tt: TokenType| -> Vec<&str> {
            toks.iter()
                .filter(|t| t.token_type == tt)
                .map(|t| &VAULT[t.range])
                .collect()
        };
        assert!(pick(TokenType::Type).contains(&"Vault")); // contract name
        assert!(pick(TokenType::Type).contains(&"Ownable")); // base (PATH_TYPE)
        assert!(pick(TokenType::Function).contains(&"deposit")); // function decl
        assert!(!pick(TokenType::Function).contains(&"emit")); // `emit` is a keyword
        assert!(pick(TokenType::Keyword).contains(&"emit"));
        assert!(pick(TokenType::Function).contains(&"Deposit")); // emit callee
        assert!(pick(TokenType::Property).contains(&"sender")); // msg.sender member
        assert!(pick(TokenType::Comment).contains(&"// SPDX-License-Identifier: MIT"));
    }
}
