//! The single `SyntaxKind` enum tags every token *and* node in the tree — rowan
//! requires one `u16` enum for both (design §3.2). Token kinds below are complete
//! for M1; node kinds grow in Plan 2 as the parser lands.

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SyntaxKind {
    // -- trivia & special --
    WHITESPACE = 0,
    COMMENT,
    ERROR,
    EOF,

    // -- literals & names --
    IDENT,
    NUMBER,
    STRING,

    // -- punctuation --
    L_PAREN,   // (
    R_PAREN,   // )
    L_BRACK,   // [
    R_BRACK,   // ]
    L_BRACE,   // {
    R_BRACE,   // }
    SEMICOLON, // ;
    COMMA,     // ,
    DOT,       // .
    QUESTION,  // ?
    COLON,     // :

    // -- operators (longest-match in the lexer) --
    EQ,         // =
    EQ2,        // ==
    NEQ,        // !=
    LT,         // <
    GT,         // >
    LT_EQ,      // <=
    GT_EQ,      // >=
    PLUS,       // +
    MINUS,      // -
    STAR,       // *
    SLASH,      // /
    PERCENT,    // %
    STAR2,      // **
    BANG,       // !
    AMP2,       // &&
    PIPE2,      // ||
    TILDE,      // ~
    AMP,        // &
    PIPE,       // |
    CARET,      // ^
    SHL,        // <<
    SHR,        // >>
    PLUS_EQ,    // +=
    MINUS_EQ,   // -=
    STAR_EQ,    // *=
    SLASH_EQ,   // /=
    PERCENT_EQ, // %=
    AMP_EQ,     // &=
    PIPE_EQ,    // |=
    CARET_EQ,   // ^=
    SHL_EQ,     // <<=
    SHR_EQ,     // >>=
    PLUS2,      // ++
    MINUS2,     // --
    FAT_ARROW,  // =>
    THIN_ARROW, // ->

    // -- keywords --
    PRAGMA_KW,
    IMPORT_KW,
    AS_KW,
    FROM_KW,
    USING_KW,
    CONTRACT_KW,
    INTERFACE_KW,
    LIBRARY_KW,
    ABSTRACT_KW,
    IS_KW,
    FUNCTION_KW,
    MODIFIER_KW,
    CONSTRUCTOR_KW,
    FALLBACK_KW,
    RECEIVE_KW,
    RETURNS_KW,
    RETURN_KW,
    STRUCT_KW,
    ENUM_KW,
    EVENT_KW,
    ERROR_KW,
    MAPPING_KW,
    PUBLIC_KW,
    PRIVATE_KW,
    INTERNAL_KW,
    EXTERNAL_KW,
    PURE_KW,
    VIEW_KW,
    PAYABLE_KW,
    VIRTUAL_KW,
    OVERRIDE_KW,
    MEMORY_KW,
    STORAGE_KW,
    CALLDATA_KW,
    IMMUTABLE_KW,
    CONSTANT_KW,
    IF_KW,
    ELSE_KW,
    FOR_KW,
    WHILE_KW,
    DO_KW,
    BREAK_KW,
    CONTINUE_KW,
    TRY_KW,
    CATCH_KW,
    EMIT_KW,
    REVERT_KW,
    NEW_KW,
    DELETE_KW,
    ASSEMBLY_KW,
    UNCHECKED_KW,
    TYPE_KW,
    TRUE_KW,
    FALSE_KW,

    // -- nodes (grow in Plan 2) --
    SOURCE_FILE,
    PRAGMA_DIRECTIVE,
    IMPORT_DIRECTIVE,
    CONTRACT_DEF,
    FUNCTION_DEF,
    STATE_VAR_DEF,
    STRUCT_DEF,
    ENUM_DEF,
    EVENT_DEF,
    ERROR_DEF,
    NAME,
    PARAM_LIST,
    BLOCK,

    // Keep last: marks the valid discriminant range for `from_u16`.
    #[doc(hidden)]
    __LAST,
}

impl SyntaxKind {
    /// Convert a raw `u16` (from rowan) back into a `SyntaxKind`.
    pub fn from_u16(d: u16) -> SyntaxKind {
        assert!(d <= SyntaxKind::__LAST as u16, "invalid SyntaxKind: {d}");
        // Safe: discriminants are contiguous 0..=__LAST and the enum is repr(u16).
        unsafe { std::mem::transmute::<u16, SyntaxKind>(d) }
    }

    pub fn to_u16(self) -> u16 {
        self as u16
    }

    pub fn is_trivia(self) -> bool {
        matches!(self, SyntaxKind::WHITESPACE | SyntaxKind::COMMENT)
    }

    /// Map an identifier spelling to its keyword kind, or `None` if it's a plain
    /// identifier. Elementary type names (`uint256`, `bytes32`, `address`) are
    /// intentionally NOT keywords — they stay `IDENT` and are recognized by the
    /// parser/semantics later.
    pub fn from_keyword(text: &str) -> Option<SyntaxKind> {
        use SyntaxKind::*;
        let kw = match text {
            "pragma" => PRAGMA_KW,
            "import" => IMPORT_KW,
            "as" => AS_KW,
            "from" => FROM_KW,
            "using" => USING_KW,
            "contract" => CONTRACT_KW,
            "interface" => INTERFACE_KW,
            "library" => LIBRARY_KW,
            "abstract" => ABSTRACT_KW,
            "is" => IS_KW,
            "function" => FUNCTION_KW,
            "modifier" => MODIFIER_KW,
            "constructor" => CONSTRUCTOR_KW,
            "fallback" => FALLBACK_KW,
            "receive" => RECEIVE_KW,
            "returns" => RETURNS_KW,
            "return" => RETURN_KW,
            "struct" => STRUCT_KW,
            "enum" => ENUM_KW,
            "event" => EVENT_KW,
            "error" => ERROR_KW,
            "mapping" => MAPPING_KW,
            "public" => PUBLIC_KW,
            "private" => PRIVATE_KW,
            "internal" => INTERNAL_KW,
            "external" => EXTERNAL_KW,
            "pure" => PURE_KW,
            "view" => VIEW_KW,
            "payable" => PAYABLE_KW,
            "virtual" => VIRTUAL_KW,
            "override" => OVERRIDE_KW,
            "memory" => MEMORY_KW,
            "storage" => STORAGE_KW,
            "calldata" => CALLDATA_KW,
            "immutable" => IMMUTABLE_KW,
            "constant" => CONSTANT_KW,
            "if" => IF_KW,
            "else" => ELSE_KW,
            "for" => FOR_KW,
            "while" => WHILE_KW,
            "do" => DO_KW,
            "break" => BREAK_KW,
            "continue" => CONTINUE_KW,
            "try" => TRY_KW,
            "catch" => CATCH_KW,
            "emit" => EMIT_KW,
            "revert" => REVERT_KW,
            "new" => NEW_KW,
            "delete" => DELETE_KW,
            "assembly" => ASSEMBLY_KW,
            "unchecked" => UNCHECKED_KW,
            "type" => TYPE_KW,
            "true" => TRUE_KW,
            "false" => FALSE_KW,
            _ => return None,
        };
        Some(kw)
    }
}

#[cfg(test)]
mod tests {
    use super::SyntaxKind::*;
    use super::*;

    #[test]
    fn u16_roundtrip_covers_all_kinds() {
        // Every discriminant 0..=__LAST must survive a round-trip.
        for d in 0..=SyntaxKind::__LAST as u16 {
            assert_eq!(SyntaxKind::from_u16(d).to_u16(), d);
        }
    }

    #[test]
    fn keywords_map() {
        assert_eq!(SyntaxKind::from_keyword("contract"), Some(CONTRACT_KW));
        assert_eq!(SyntaxKind::from_keyword("mapping"), Some(MAPPING_KW));
        assert_eq!(SyntaxKind::from_keyword("returns"), Some(RETURNS_KW));
        assert_eq!(SyntaxKind::from_keyword("Foo"), None);
        assert_eq!(SyntaxKind::from_keyword("uint256"), None); // elementary types stay IDENT
    }

    #[test]
    fn trivia_classification() {
        assert!(WHITESPACE.is_trivia());
        assert!(COMMENT.is_trivia());
        assert!(!IDENT.is_trivia());
        assert!(!CONTRACT_KW.is_trivia());
    }
}
