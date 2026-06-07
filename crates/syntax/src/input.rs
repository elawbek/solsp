//! `Input` — the parser's view of the token stream with trivia removed. The
//! parser reasons only about meaningful tokens; trivia is re-attached later by the
//! tree builder (design §3.3).

use crate::lexer::Token;
use crate::SyntaxKind;

#[allow(dead_code)] // consumed by Parser, which lands in a later task of this plan
pub(crate) struct Input {
    kinds: Vec<SyntaxKind>,
}

#[allow(dead_code)] // consumed by Parser, which lands in a later task of this plan
impl Input {
    pub(crate) fn new(tokens: &[Token]) -> Input {
        let kinds = tokens
            .iter()
            .map(|t| t.kind)
            .filter(|k| !k.is_trivia())
            .collect();
        Input { kinds }
    }

    /// Kind of the `i`-th non-trivia token, or `EOF` past the end.
    pub(crate) fn kind(&self, i: usize) -> SyntaxKind {
        self.kinds.get(i).copied().unwrap_or(SyntaxKind::EOF)
    }

    pub(crate) fn len(&self) -> usize {
        self.kinds.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;
    use crate::SyntaxKind::*;

    #[test]
    fn input_hides_trivia() {
        // "contract C" -> tokens: CONTRACT_KW, WS, IDENT  =>  non-trivia: CONTRACT_KW, IDENT
        let toks = tokenize("contract C");
        let input = Input::new(&toks);
        assert_eq!(input.kind(0), CONTRACT_KW);
        assert_eq!(input.kind(1), IDENT);
        assert_eq!(input.kind(2), EOF); // past the end reads as EOF
        assert_eq!(input.len(), 2);
    }
}
