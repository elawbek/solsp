//! Solidity type model and the **implicit conversion** table, used to decide whether a
//! call argument's type is accepted by a parameter without an explicit cast.
//!
//! The single source of truth is [`implicitly_convertible`]. The type checker flags an
//! argument only when its inferred type and the parameter type are **both concrete** and
//! the argument is **not** implicitly convertible to the parameter — so anything unknown
//! or fuzzy (literals, un-inferrable expressions) is treated as convertible and never
//! produces a false positive.
//!
//! Implicit conversions encoded here (Solidity ≥0.8, value/reference types):
//! - identity: `T` → `T`.
//! - integers: `uintN` → `uintM` and `intN` → `intM` when `M ≥ N` (widening only; no
//!   signedness change, no narrowing).
//! - `address payable` → `address`.
//! - a contract type → `address` (an instance is usable where an address is expected),
//!   and → a base contract / implemented interface (via the inheritance predicate).
//! - number literals → any integer type; the literal `0`-family is also accepted for
//!   `address`/`bytesN` (literal value fitting isn't tracked, so we allow it).
//! - string literals → `string` / `bytes` / `bytesN`.
//! - dynamic arrays / `bytes` / `string`: element-and-shape identity only.
//!
//! Deliberately NOT implicit (so a mismatch here is reportable): `uint` ↔ `int`,
//! integer narrowing, `bytesN` ↔ `bytesM` of a different size, unrelated user types,
//! cross-category (e.g. `uint` vs `bool`, a struct vs an address).

/// A parsed Solidity type, at the granularity the conversion rules need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    /// `uintN` (bits, 8..=256; `uint` == `uint256`).
    Uint(u16),
    /// `intN` (bits, 8..=256; `int` == `int256`).
    Int(u16),
    Address,
    AddressPayable,
    Bool,
    /// Dynamic `string`.
    StringT,
    /// Dynamic `bytes`.
    Bytes,
    /// `bytesN` (1..=32).
    BytesN(u8),
    /// `T[]` — dynamic array.
    Array(Box<Ty>),
    /// `T[N]` — fixed-size array (the size isn't tracked).
    FixedArray(Box<Ty>),
    /// A user-defined type by name (contract / interface / struct / enum / user value
    /// type), possibly qualified (`A.B`) — the last segment is kept.
    User(String),
    /// A number / string / bool literal whose precise type is left open.
    NumberLiteral,
    StringLiteral,
    BoolLiteral,
    /// Anything we could not classify — always treated as convertible.
    Unknown,
}

/// Parse a normalized type string (as produced by `type_text`) into a [`Ty`].
pub fn parse_ty(text: &str) -> Ty {
    let t = text.trim();
    // strip a trailing data location, if any leaked in.
    let t = t
        .trim_end_matches(" memory")
        .trim_end_matches(" storage")
        .trim_end_matches(" calldata")
        .trim();
    if let Some(inner) = t.strip_suffix("[]") {
        return Ty::Array(Box::new(parse_ty(inner)));
    }
    if t.ends_with(']') {
        if let Some(open) = t.rfind('[') {
            return Ty::FixedArray(Box::new(parse_ty(&t[..open])));
        }
    }
    match t {
        "address" => Ty::Address,
        "address payable" => Ty::AddressPayable,
        "bool" => Ty::Bool,
        "string" => Ty::StringT,
        "bytes" => Ty::Bytes,
        "uint" => Ty::Uint(256),
        "int" => Ty::Int(256),
        _ => {
            if let Some(bits) = int_bits(t, "uint") {
                return Ty::Uint(bits);
            }
            if let Some(bits) = int_bits(t, "int") {
                return Ty::Int(bits);
            }
            if let Some(n) = t.strip_prefix("bytes").and_then(|d| d.parse::<u8>().ok()) {
                if (1..=32).contains(&n) {
                    return Ty::BytesN(n);
                }
            }
            // a qualified user type keeps its last segment (`ICraftV2.Thing` -> `Thing`).
            Ty::User(t.rsplit('.').next().unwrap_or(t).to_string())
        }
    }
}

/// `uintN`/`intN` bit width, validated to a multiple of 8 in 8..=256.
fn int_bits(t: &str, prefix: &str) -> Option<u16> {
    let bits: u16 = t.strip_prefix(prefix)?.parse().ok()?;
    (bits.is_multiple_of(8) && (8..=256).contains(&bits)).then_some(bits)
}

/// Whether a value of type `from` is **implicitly** convertible to `to`. `is_base(a, b)`
/// reports whether user type `a` has user type `b` somewhere in its inheritance (bases /
/// implemented interfaces). When uncertain, returns `true` (never a false positive).
pub fn implicitly_convertible(from: &Ty, to: &Ty, is_base: &dyn Fn(&str, &str) -> bool) -> bool {
    use Ty::*;
    if from == to || matches!(from, Unknown) || matches!(to, Unknown) {
        return true;
    }
    match (from, to) {
        // integers widen within the same signedness.
        (Uint(a), Uint(b)) | (Int(a), Int(b)) => b >= a,

        // address payable is an address; a contract is usable as an address.
        (AddressPayable, Address) => true,
        (User(_), Address | AddressPayable) => true,

        // a contract implicitly converts to a base contract / implemented interface.
        (User(a), User(b)) => a == b || is_base(a, b),

        // literals.
        (NumberLiteral, Uint(_) | Int(_) | Address | AddressPayable) => true,
        (NumberLiteral, BytesN(_) | Bytes) => true, // `0`/hex literal, fit untracked
        (StringLiteral, StringT | Bytes | BytesN(_)) => true,
        (BoolLiteral, Bool) => true,

        // arrays / bytes / string convert only by exact identity (handled by `from == to`).
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never_base(_: &str, _: &str) -> bool {
        false
    }

    #[test]
    fn parses_types() {
        assert_eq!(parse_ty("uint256"), Ty::Uint(256));
        assert_eq!(parse_ty("uint"), Ty::Uint(256));
        assert_eq!(parse_ty("uint8"), Ty::Uint(8));
        assert_eq!(parse_ty("int128"), Ty::Int(128));
        assert_eq!(parse_ty("address payable"), Ty::AddressPayable);
        assert_eq!(parse_ty("bytes32"), Ty::BytesN(32));
        assert_eq!(parse_ty("uint256[]"), Ty::Array(Box::new(Ty::Uint(256))));
        assert_eq!(parse_ty("Foo.Bar"), Ty::User("Bar".into()));
        assert_eq!(parse_ty("Thing memory"), Ty::User("Thing".into()));
    }

    #[test]
    fn implicit_conversions_allowed() {
        let ok = |a: &str, b: &str| implicitly_convertible(&parse_ty(a), &parse_ty(b), &never_base);
        assert!(ok("uint8", "uint256")); // widening
        assert!(ok("uint256", "uint256")); // identity
        assert!(ok("address payable", "address"));
        assert!(ok("Roles", "Roles")); // same user type
    }

    #[test]
    fn user_type_inheritance_allowed() {
        let is_base = |a: &str, b: &str| a == "Derived" && b == "Base";
        assert!(implicitly_convertible(
            &parse_ty("Derived"),
            &parse_ty("Base"),
            &is_base
        ));
        // a contract is usable as an address.
        assert!(implicitly_convertible(
            &parse_ty("Roles"),
            &parse_ty("address"),
            &never_base
        ));
    }

    #[test]
    fn mismatches_rejected() {
        let bad =
            |a: &str, b: &str| !implicitly_convertible(&parse_ty(a), &parse_ty(b), &never_base);
        assert!(bad("uint256", "uint8")); // narrowing
        assert!(bad("uint256", "int256")); // signedness
        assert!(bad("uint256", "bool")); // category
        assert!(bad("bytes16", "bytes32")); // bytesN sizes
        assert!(bad("Roles", "Buildings")); // unrelated user types
        assert!(bad("string", "bytes")); // not implicit between dynamic types
    }

    #[test]
    fn unknown_and_literals_never_flagged() {
        let ok = |a: &Ty, b: &str| implicitly_convertible(a, &parse_ty(b), &never_base);
        assert!(ok(&Ty::Unknown, "uint256"));
        assert!(implicitly_convertible(
            &parse_ty("uint256"),
            &Ty::Unknown,
            &never_base
        ));
        assert!(ok(&Ty::NumberLiteral, "uint8"));
        assert!(ok(&Ty::StringLiteral, "bytes"));
    }
}
