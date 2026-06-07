//! The single `SyntaxKind` enum tags every token *and* node in the tree — rowan
//! requires one `u16` enum for both. This is the canonical list to grow per the
//! official Solidity grammar (design §3.2). The subset below is enough to compile
//! the pipeline; fill it out as the parser lands.

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SyntaxKind {
    // -- trivia & special --
    WHITESPACE = 0,
    COMMENT,
    ERROR,
    EOF,

    // -- tokens: punctuation --
    L_PAREN,   // (
    R_PAREN,   // )
    L_BRACE,   // {
    R_BRACE,   // }
    SEMICOLON, // ;

    // -- tokens: literals & names --
    IDENT,
    INT_NUMBER,
    STRING,

    // -- tokens: keywords (subset; complete per grammar) --
    PRAGMA_KW,
    IMPORT_KW,
    CONTRACT_KW,
    INTERFACE_KW,
    LIBRARY_KW,
    FUNCTION_KW,

    // -- nodes: top level --
    SOURCE_FILE,
    PRAGMA_DIRECTIVE,
    IMPORT_DIRECTIVE,
    CONTRACT_DEF,

    // -- nodes: members --
    FUNCTION_DEF,
    STATE_VAR_DEF,
    STRUCT_DEF,
    ENUM_DEF,
    EVENT_DEF,
    ERROR_DEF,

    // -- nodes: misc --
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
}
