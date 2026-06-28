//! Solidity builtin names and completion items.

use lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};

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
