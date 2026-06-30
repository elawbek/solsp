//! Solidity builtin names and completion items.

use lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};
use std::sync::OnceLock;

use crate::protocol::trigger_signature_help;

/// Build completion items from `(name, detail, is_method)` triples — synthetic builtin
/// members. Methods insert call parens.
pub(super) fn synthetic_members(items: &[(&str, &str, bool)]) -> Vec<CompletionItem> {
    items
        .iter()
        .map(|&(name, detail, method)| {
            let (insert_text, insert_text_format) = if method {
                (Some(format!("{name}($0)")), Some(InsertTextFormat::SNIPPET))
            } else {
                (None, None)
            };
            CompletionItem {
                kind: Some(if method {
                    CompletionItemKind::METHOD
                } else {
                    CompletionItemKind::FIELD
                }),
                detail: Some(if detail.is_empty() {
                    "builtin".to_string()
                } else {
                    detail.to_string()
                }),
                insert_text,
                insert_text_format,
                label: name.to_string(),
                ..Default::default()
            }
        })
        .collect()
}

pub(super) fn is_integer_type_name(n: &str) -> bool {
    let rest = n.strip_prefix("uint").or_else(|| n.strip_prefix("int"));
    matches!(rest, Some(d) if d.is_empty() || d.parse::<u16>().is_ok())
}

pub(super) fn is_fixed_bytes(n: &str) -> bool {
    matches!(n.strip_prefix("bytes").map(str::parse::<u8>), Some(Ok(w)) if (1..=32).contains(&w))
}

/// Members of a builtin global object when the receiver is `block`/`tx`/`msg`/`abi`.
pub(super) fn builtin_member_items(
    receiver: &solsp_syntax::SyntaxNode,
) -> Option<Vec<CompletionItem>> {
    use solsp_syntax::SyntaxKind::{NAME_REF, PATH_EXPR};
    if !matches!(receiver.kind(), PATH_EXPR | NAME_REF) {
        return None; // only a bare global, not a chain/call
    }
    let name = solsp_hir::resolve::receiver_name(receiver)?;
    // `(member, type, is_method)` — real types so hover and completion show them.
    let members: &[(&str, &str, bool)] = match name.as_str() {
        "block" => &[
            ("basefee", "uint256", false),
            ("blobbasefee", "uint256", false),
            ("chainid", "uint256", false),
            ("coinbase", "address payable", false),
            ("difficulty", "uint256", false),
            ("gaslimit", "uint256", false),
            ("number", "uint256", false),
            ("prevrandao", "uint256", false),
            ("timestamp", "uint256", false),
        ],
        "tx" => &[("gasprice", "uint256", false), ("origin", "address", false)],
        "msg" => &[
            ("data", "bytes calldata", false),
            ("sender", "address", false),
            ("sig", "bytes4", false),
            ("value", "uint256", false),
        ],
        "abi" => &[
            ("decode", "", true),
            ("encode", "bytes memory", true),
            ("encodeCall", "bytes memory", true),
            ("encodePacked", "bytes memory", true),
            ("encodeWithSelector", "bytes memory", true),
            ("encodeWithSignature", "bytes memory", true),
        ],
        _ => return None,
    };
    Some(synthetic_members(members))
}

/// Solidity keywords, elementary types, and global builtins — always available as
/// bare-identifier completions.
pub(super) fn builtin_items() -> Vec<CompletionItem> {
    use CompletionItemKind as K;
    const KEYWORDS: &[&str] = &[
        "if",
        "else",
        "for",
        "while",
        "do",
        "return",
        "break",
        "continue",
        "emit",
        "try",
        "catch",
        "new",
        "delete",
        "using",
        "unchecked",
        "assembly",
        "is",
        "virtual",
        "override",
        "public",
        "private",
        "internal",
        "external",
        "view",
        "pure",
        "payable",
        "memory",
        "storage",
        "calldata",
        "constant",
        "immutable",
        "returns",
        "function",
        "modifier",
        "struct",
        "enum",
        "event",
        "error",
        "mapping",
        "contract",
        "interface",
        "library",
        "import",
        "pragma",
        "abstract",
        "indexed",
        "anonymous",
    ];
    const TYPES: &[&str] = &[
        "address", "bool", "string", "bytes", "uint", "uint8", "uint16", "uint32", "uint64",
        "uint128", "uint256", "int", "int128", "int256", "bytes1", "bytes4", "bytes20", "bytes32",
    ];
    const GLOBALS: &[&str] = &["msg", "block", "tx", "abi", "this", "super", "type", "now"];
    const FUNCS: &[&str] = &[
        "require",
        "assert",
        "revert",
        "keccak256",
        "sha256",
        "ripemd160",
        "ecrecover",
        "addmod",
        "mulmod",
        "selfdestruct",
        "blockhash",
        "gasleft",
    ];
    let item = |label: &str, kind: CompletionItemKind, detail: &str| CompletionItem {
        kind: Some(kind),
        detail: Some(detail.to_string()),
        label: label.to_string(),
        ..Default::default()
    };
    // A builtin function inserts `name()` with the cursor between the parens, and asks the
    // client to pop signature help there.
    let func = |label: &str| CompletionItem {
        insert_text: Some(format!("{label}($0)")),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        command: Some(trigger_signature_help()),
        ..item(label, K::FUNCTION, "builtin")
    };
    let mut out = Vec::with_capacity(KEYWORDS.len() + TYPES.len() + GLOBALS.len() + FUNCS.len());
    out.extend(KEYWORDS.iter().map(|&k| item(k, K::KEYWORD, "keyword")));
    out.extend(TYPES.iter().map(|&t| item(t, K::TYPE_PARAMETER, "type")));
    out.extend(GLOBALS.iter().map(|&g| item(g, K::VARIABLE, "builtin")));
    out.extend(FUNCS.iter().map(|&f| func(f)));
    out
}

/// Whether `name` is a Solidity builtin usable as a value: a global object, a builtin
/// function, an elementary type (also a cast callee), `payable`, or a unit literal.
pub(super) fn is_builtin_name(name: &str) -> bool {
    const NAMES: &[&str] = &[
        // the modifier-body placeholder `_;`
        "_",
        // globals
        "msg",
        "block",
        "tx",
        "abi",
        "this",
        "super",
        "type",
        "now",
        "blobhash",
        // builtin functions
        "require",
        "assert",
        "revert",
        "keccak256",
        "sha256",
        "ripemd160",
        "ecrecover",
        "addmod",
        "mulmod",
        "selfdestruct",
        "blockhash",
        "gasleft",
        "payable",
        "sha3",
        // elementary type names (cast callees)
        "address",
        "bool",
        "string",
        "bytes",
        "byte",
        // unit suffixes (lexed as identifiers after a literal)
        "wei",
        "gwei",
        "ether",
        "seconds",
        "minutes",
        "hours",
        "days",
        "weeks",
        "years",
        "finney",
        "szabo",
    ];
    NAMES.contains(&name)
        || is_integer_type_name(name)
        || is_fixed_bytes(name)
        || name.starts_with("ufixed")
        || name.starts_with("fixed")
}

#[derive(Clone, Copy)]
pub(super) struct YulBuiltin {
    pub name: &'static str,
    pub signature: &'static str,
    pub detail: &'static str,
}

pub(super) const YUL_BUILTINS: &[YulBuiltin] = &[
    YulBuiltin {
        name: "stop",
        signature: "stop()",
        detail: "stop execution",
    },
    YulBuiltin {
        name: "add",
        signature: "add(x, y)",
        detail: "wrapping addition",
    },
    YulBuiltin {
        name: "sub",
        signature: "sub(x, y)",
        detail: "wrapping subtraction",
    },
    YulBuiltin {
        name: "mul",
        signature: "mul(x, y)",
        detail: "wrapping multiplication",
    },
    YulBuiltin {
        name: "div",
        signature: "div(x, y)",
        detail: "unsigned division",
    },
    YulBuiltin {
        name: "sdiv",
        signature: "sdiv(x, y)",
        detail: "signed division",
    },
    YulBuiltin {
        name: "mod",
        signature: "mod(x, y)",
        detail: "unsigned modulo",
    },
    YulBuiltin {
        name: "smod",
        signature: "smod(x, y)",
        detail: "signed modulo",
    },
    YulBuiltin {
        name: "exp",
        signature: "exp(x, y)",
        detail: "exponentiation",
    },
    YulBuiltin {
        name: "not",
        signature: "not(x)",
        detail: "bitwise not",
    },
    YulBuiltin {
        name: "lt",
        signature: "lt(x, y)",
        detail: "unsigned less-than",
    },
    YulBuiltin {
        name: "gt",
        signature: "gt(x, y)",
        detail: "unsigned greater-than",
    },
    YulBuiltin {
        name: "slt",
        signature: "slt(x, y)",
        detail: "signed less-than",
    },
    YulBuiltin {
        name: "sgt",
        signature: "sgt(x, y)",
        detail: "signed greater-than",
    },
    YulBuiltin {
        name: "eq",
        signature: "eq(x, y)",
        detail: "equality comparison",
    },
    YulBuiltin {
        name: "iszero",
        signature: "iszero(x)",
        detail: "1 if x is zero, otherwise 0",
    },
    YulBuiltin {
        name: "and",
        signature: "and(x, y)",
        detail: "bitwise and",
    },
    YulBuiltin {
        name: "or",
        signature: "or(x, y)",
        detail: "bitwise or",
    },
    YulBuiltin {
        name: "xor",
        signature: "xor(x, y)",
        detail: "bitwise xor",
    },
    YulBuiltin {
        name: "byte",
        signature: "byte(n, x)",
        detail: "nth byte of x",
    },
    YulBuiltin {
        name: "shl",
        signature: "shl(x, y)",
        detail: "logical left shift",
    },
    YulBuiltin {
        name: "shr",
        signature: "shr(x, y)",
        detail: "logical right shift",
    },
    YulBuiltin {
        name: "sar",
        signature: "sar(x, y)",
        detail: "arithmetic right shift",
    },
    YulBuiltin {
        name: "addmod",
        signature: "addmod(x, y, m)",
        detail: "modular addition",
    },
    YulBuiltin {
        name: "mulmod",
        signature: "mulmod(x, y, m)",
        detail: "modular multiplication",
    },
    YulBuiltin {
        name: "signextend",
        signature: "signextend(i, x)",
        detail: "sign extend from byte i",
    },
    YulBuiltin {
        name: "keccak256",
        signature: "keccak256(p, n)",
        detail: "hash memory region",
    },
    YulBuiltin {
        name: "pc",
        signature: "pc()",
        detail: "program counter",
    },
    YulBuiltin {
        name: "pop",
        signature: "pop(x)",
        detail: "discard value",
    },
    YulBuiltin {
        name: "mload",
        signature: "mload(p)",
        detail: "load word from memory",
    },
    YulBuiltin {
        name: "mstore",
        signature: "mstore(p, v)",
        detail: "store word to memory",
    },
    YulBuiltin {
        name: "mstore8",
        signature: "mstore8(p, v)",
        detail: "store byte to memory",
    },
    YulBuiltin {
        name: "msize",
        signature: "msize()",
        detail: "current memory size",
    },
    YulBuiltin {
        name: "mcopy",
        signature: "mcopy(t, f, s)",
        detail: "copy memory",
    },
    YulBuiltin {
        name: "sload",
        signature: "sload(p)",
        detail: "load word from storage",
    },
    YulBuiltin {
        name: "sstore",
        signature: "sstore(p, v)",
        detail: "store word to storage",
    },
    YulBuiltin {
        name: "tload",
        signature: "tload(p)",
        detail: "load word from transient storage",
    },
    YulBuiltin {
        name: "tstore",
        signature: "tstore(p, v)",
        detail: "store word to transient storage",
    },
    YulBuiltin {
        name: "calldataload",
        signature: "calldataload(p)",
        detail: "load word from calldata",
    },
    YulBuiltin {
        name: "calldatasize",
        signature: "calldatasize()",
        detail: "calldata size",
    },
    YulBuiltin {
        name: "calldatacopy",
        signature: "calldatacopy(t, f, s)",
        detail: "copy calldata to memory",
    },
    YulBuiltin {
        name: "codesize",
        signature: "codesize()",
        detail: "current code size",
    },
    YulBuiltin {
        name: "codecopy",
        signature: "codecopy(t, f, s)",
        detail: "copy current code to memory",
    },
    YulBuiltin {
        name: "extcodesize",
        signature: "extcodesize(a)",
        detail: "external account code size",
    },
    YulBuiltin {
        name: "extcodecopy",
        signature: "extcodecopy(a, t, f, s)",
        detail: "copy external account code",
    },
    YulBuiltin {
        name: "extcodehash",
        signature: "extcodehash(a)",
        detail: "external account code hash",
    },
    YulBuiltin {
        name: "returndatasize",
        signature: "returndatasize()",
        detail: "last return data size",
    },
    YulBuiltin {
        name: "returndatacopy",
        signature: "returndatacopy(t, f, s)",
        detail: "copy return data to memory",
    },
    YulBuiltin {
        name: "create",
        signature: "create(v, p, n)",
        detail: "create contract",
    },
    YulBuiltin {
        name: "create2",
        signature: "create2(v, p, n, s)",
        detail: "create contract with salt",
    },
    YulBuiltin {
        name: "call",
        signature: "call(g, a, v, in, insize, out, outsize)",
        detail: "message call",
    },
    YulBuiltin {
        name: "callcode",
        signature: "callcode(g, a, v, in, insize, out, outsize)",
        detail: "message call with current storage",
    },
    YulBuiltin {
        name: "delegatecall",
        signature: "delegatecall(g, a, in, insize, out, outsize)",
        detail: "delegate call",
    },
    YulBuiltin {
        name: "staticcall",
        signature: "staticcall(g, a, in, insize, out, outsize)",
        detail: "static call",
    },
    YulBuiltin {
        name: "return",
        signature: "return(p, s)",
        detail: "return memory region",
    },
    YulBuiltin {
        name: "revert",
        signature: "revert(p, s)",
        detail: "revert with memory region",
    },
    YulBuiltin {
        name: "selfdestruct",
        signature: "selfdestruct(a)",
        detail: "destroy current contract",
    },
    YulBuiltin {
        name: "invalid",
        signature: "invalid()",
        detail: "end execution with invalid instruction",
    },
    YulBuiltin {
        name: "log0",
        signature: "log0(p, s)",
        detail: "emit log with no topics",
    },
    YulBuiltin {
        name: "log1",
        signature: "log1(p, s, t1)",
        detail: "emit log with one topic",
    },
    YulBuiltin {
        name: "log2",
        signature: "log2(p, s, t1, t2)",
        detail: "emit log with two topics",
    },
    YulBuiltin {
        name: "log3",
        signature: "log3(p, s, t1, t2, t3)",
        detail: "emit log with three topics",
    },
    YulBuiltin {
        name: "log4",
        signature: "log4(p, s, t1, t2, t3, t4)",
        detail: "emit log with four topics",
    },
    YulBuiltin {
        name: "chainid",
        signature: "chainid()",
        detail: "current chain id",
    },
    YulBuiltin {
        name: "basefee",
        signature: "basefee()",
        detail: "current block base fee",
    },
    YulBuiltin {
        name: "blobbasefee",
        signature: "blobbasefee()",
        detail: "current blob base fee",
    },
    YulBuiltin {
        name: "origin",
        signature: "origin()",
        detail: "transaction origin",
    },
    YulBuiltin {
        name: "gasprice",
        signature: "gasprice()",
        detail: "transaction gas price",
    },
    YulBuiltin {
        name: "blockhash",
        signature: "blockhash(b)",
        detail: "hash of block b",
    },
    YulBuiltin {
        name: "coinbase",
        signature: "coinbase()",
        detail: "current block beneficiary",
    },
    YulBuiltin {
        name: "timestamp",
        signature: "timestamp()",
        detail: "current block timestamp",
    },
    YulBuiltin {
        name: "number",
        signature: "number()",
        detail: "current block number",
    },
    YulBuiltin {
        name: "difficulty",
        signature: "difficulty()",
        detail: "current block difficulty",
    },
    YulBuiltin {
        name: "prevrandao",
        signature: "prevrandao()",
        detail: "current block randomness",
    },
    YulBuiltin {
        name: "gaslimit",
        signature: "gaslimit()",
        detail: "current block gas limit",
    },
    YulBuiltin {
        name: "caller",
        signature: "caller()",
        detail: "message caller",
    },
    YulBuiltin {
        name: "callvalue",
        signature: "callvalue()",
        detail: "call value",
    },
    YulBuiltin {
        name: "gas",
        signature: "gas()",
        detail: "remaining gas",
    },
    YulBuiltin {
        name: "balance",
        signature: "balance(a)",
        detail: "account balance",
    },
    YulBuiltin {
        name: "selfbalance",
        signature: "selfbalance()",
        detail: "current contract balance",
    },
    YulBuiltin {
        name: "blobhash",
        signature: "blobhash(i)",
        detail: "versioned hash of blob i",
    },
];

pub(super) fn yul_builtin_items() -> Vec<CompletionItem> {
    static ITEMS: OnceLock<Vec<CompletionItem>> = OnceLock::new();
    ITEMS
        .get_or_init(|| {
            YUL_BUILTINS
                .iter()
                .map(|builtin| CompletionItem {
                    kind: Some(CompletionItemKind::FUNCTION),
                    detail: Some(format!("Yul builtin: {}", builtin.detail)),
                    insert_text: Some(format!("{}($0)", builtin.name)),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    label: builtin.name.to_string(),
                    ..Default::default()
                })
                .collect()
        })
        .clone()
}

pub(super) fn yul_builtin(name: &str) -> Option<YulBuiltin> {
    YUL_BUILTINS
        .iter()
        .copied()
        .find(|builtin| builtin.name == name)
}
