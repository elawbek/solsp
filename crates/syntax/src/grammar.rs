//! Solidity declaration & type grammar (Plan 3). The parser emits events; this
//! module is the recursive-descent grammar that drives it. Statements, Pratt
//! expressions and inline assembly are later plans — here, function/modifier
//! bodies are a single `BLOCK` whose interior is span-skipped, and state-variable
//! initializers are span-skipped to `;`.

use crate::parser::{CompletedMarker, Parser};
use crate::SyntaxKind::{self, *};

pub(crate) fn source_file(p: &mut Parser) {
    let m = p.start();
    while !p.at(EOF) {
        item(p);
    }
    m.complete(p, SOURCE_FILE);
}

fn item(p: &mut Parser) {
    match p.current() {
        PRAGMA_KW => pragma(p),
        IMPORT_KW => import_directive(p),
        USING_KW => using_directive(p),
        CONTRACT_KW | INTERFACE_KW | LIBRARY_KW | ABSTRACT_KW => contract(p),
        FUNCTION_KW => function_def(p), // free function
        STRUCT_KW => struct_def(p),
        ENUM_KW => enum_def(p),
        EVENT_KW => event_def(p),
        ERROR_KW => error_def(p),
        TYPE_KW => user_defined_value_type(p),
        IDENT | MAPPING_KW => state_var_def(p), // file-level constant
        _ => p.err_and_bump("expected an item (pragma, import, contract, …)"),
    }
}

/// `pragma <anything> ;` — the directive body isn't structured in M1; we span up
/// to and including the semicolon.
fn pragma(p: &mut Parser) {
    let m = p.start();
    p.bump(PRAGMA_KW);
    while !p.at(SEMICOLON) && !p.at(EOF) {
        p.bump_any();
    }
    p.expect(SEMICOLON);
    m.complete(p, PRAGMA_DIRECTIVE);
}

/// `'import' <anything> ';'` — structured into targets/paths in M2; here we span
/// to the semicolon (lossless), wrapped as one directive node.
fn import_directive(p: &mut Parser) {
    let m = p.start();
    p.bump(IMPORT_KW);
    while !p.at(SEMICOLON) && !p.at(EOF) {
        p.bump_any();
    }
    p.expect(SEMICOLON);
    m.complete(p, IMPORT_DIRECTIVE);
}

/// `'using' <anything> ';'` — same span-to-`;` treatment as imports for M1.
fn using_directive(p: &mut Parser) {
    let m = p.start();
    p.bump(USING_KW);
    while !p.at(SEMICOLON) && !p.at(EOF) {
        p.bump_any();
    }
    p.expect(SEMICOLON);
    m.complete(p, USING_DIRECTIVE);
}

/// `('contract'|'interface'|'library'|'abstract' 'contract'?) NAME contract_body`
fn contract(p: &mut Parser) {
    let m = p.start();
    p.bump_any(); // contract/interface/library/abstract keyword
    if p.at(CONTRACT_KW) {
        p.bump(CONTRACT_KW); // `abstract contract`
    }
    if p.at(IDENT) {
        name(p);
    } else {
        p.error("expected a contract name");
    }
    if p.eat(IS_KW) {
        inheritance_specifier(p);
        while p.eat(COMMA) {
            inheritance_specifier(p);
        }
    }
    if p.at(L_BRACE) {
        contract_body(p);
    } else {
        p.error("expected '{'");
    }
    m.complete(p, CONTRACT_DEF);
}

/// `path_type ('(' <constructor args> ')')?` — a base in an `is` list. The args
/// are expressions, span-skipped until a later plan.
fn inheritance_specifier(p: &mut Parser) {
    let m = p.start();
    if p.at(IDENT) {
        path_type(p);
    } else {
        // Error-only recovery (no bump): safe ONLY because the caller's base-list
        // loop is driven by `eat(COMMA)`, not by this function consuming a token.
        // Don't make any loop iterate on `inheritance_specifier` directly.
        p.error("expected a base contract name");
    }
    if p.at(L_PAREN) {
        skip_parens(p);
    }
    m.complete(p, INHERITANCE_SPECIFIER);
}

fn contract_body(p: &mut Parser) {
    let m = p.start();
    p.bump(L_BRACE);
    while !p.at(R_BRACE) && !p.at(EOF) {
        member(p);
    }
    p.expect(R_BRACE);
    m.complete(p, CONTRACT_BODY);
}

/// Dispatch one contract member. Every arm consumes at least one token (or
/// `err_and_bump` does), so the `contract_body` loop always makes progress.
fn member(p: &mut Parser) {
    match p.current() {
        FUNCTION_KW | FALLBACK_KW | RECEIVE_KW => function_def(p),
        MODIFIER_KW => modifier_def(p),
        CONSTRUCTOR_KW => constructor_def(p),
        STRUCT_KW => struct_def(p),
        ENUM_KW => enum_def(p),
        EVENT_KW => event_def(p),
        ERROR_KW => error_def(p),
        TYPE_KW => user_defined_value_type(p),
        USING_KW => using_directive(p),
        IDENT | MAPPING_KW => state_var_def(p),
        _ => p.err_and_bump("expected a contract member"),
    }
}

/// `type_name (visibility|'constant'|'immutable'|override_spec)* NAME? ('=' init)? ';'`
/// The initializer is a real (Plan 4) expression parsed by `expr`.
fn state_var_def(p: &mut Parser) {
    let m = p.start();
    type_name(p);
    loop {
        match p.current() {
            PUBLIC_KW | PRIVATE_KW | INTERNAL_KW | CONSTANT_KW | IMMUTABLE_KW => p.bump_any(),
            OVERRIDE_KW => override_spec(p),
            _ => break,
        }
    }
    if p.at(IDENT) {
        name(p);
    }
    if p.eat(EQ) {
        expr(p); // initializer is a real expression (Plan 4)
    }
    p.expect(SEMICOLON);
    m.complete(p, STATE_VAR_DEF);
}

/// `'override' ('(' <names> ')')?` — the override list is skipped for now.
fn override_spec(p: &mut Parser) {
    p.bump(OVERRIDE_KW);
    if p.at(L_PAREN) {
        skip_parens(p);
    }
}

// ---- functions ---------------------------------------------------------------

/// `('function' NAME? | 'fallback' | 'receive') param_list function_attribute*
///  ('returns' param_list)? (block | ';')`
fn function_def(p: &mut Parser) {
    let m = p.start();
    let named = p.at(FUNCTION_KW);
    p.bump_any(); // function / fallback / receive
    if named && p.at(IDENT) {
        name(p);
    }
    if p.at(L_PAREN) {
        param_list(p);
    } else {
        p.error("expected '(' after function");
    }
    function_attributes(p);
    if p.eat(RETURNS_KW) {
        if p.at(L_PAREN) {
            param_list(p);
        } else {
            p.error("expected '(' after returns");
        }
    }
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.expect(SEMICOLON);
    }
    m.complete(p, FUNCTION_DEF);
}

/// Visibility / mutability / `virtual` / `override(...)` / modifier invocations,
/// in any order, until the body or `returns`.
fn function_attributes(p: &mut Parser) {
    loop {
        match p.current() {
            PUBLIC_KW | PRIVATE_KW | INTERNAL_KW | EXTERNAL_KW | PURE_KW | VIEW_KW | PAYABLE_KW
            | VIRTUAL_KW => p.bump_any(),
            OVERRIDE_KW => override_spec(p),
            IDENT => modifier_invocation(p),
            _ => break,
        }
    }
}

/// `name_ref ('(' <args> ')')?` — a base-modifier/constructor call in a header.
fn modifier_invocation(p: &mut Parser) {
    let m = p.start();
    name_ref(p);
    if p.at(L_PAREN) {
        skip_parens(p);
    }
    m.complete(p, MODIFIER_INVOCATION);
}

/// A `{ … }` body. Statements land in a later plan, so the interior is span-
/// skipped to the matching brace; the node is still a real `BLOCK`.
fn block(p: &mut Parser) {
    let m = p.start();
    p.bump(L_BRACE);
    let mut depth = 1usize;
    while depth > 0 && !p.at(EOF) {
        match p.current() {
            L_BRACE => depth += 1,
            R_BRACE => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            break;
        }
        p.bump_any();
    }
    p.expect(R_BRACE);
    m.complete(p, BLOCK);
}

// ---- modifiers & constructors ------------------------------------------------

/// `'modifier' NAME? param_list? ('virtual'|override_spec)* (block | ';')`
fn modifier_def(p: &mut Parser) {
    let m = p.start();
    p.bump(MODIFIER_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.at(L_PAREN) {
        param_list(p);
    }
    loop {
        match p.current() {
            VIRTUAL_KW => p.bump_any(),
            OVERRIDE_KW => override_spec(p),
            _ => break,
        }
    }
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.expect(SEMICOLON);
    }
    m.complete(p, MODIFIER_DEF);
}

/// `'constructor' param_list? function_attribute* (block | ';')`
fn constructor_def(p: &mut Parser) {
    let m = p.start();
    p.bump(CONSTRUCTOR_KW);
    if p.at(L_PAREN) {
        param_list(p);
    }
    function_attributes(p); // visibility / payable / base-constructor invocations
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.expect(SEMICOLON);
    }
    m.complete(p, CONSTRUCTOR_DEF);
}

// ---- structs & enums ---------------------------------------------------------

/// `'struct' NAME? '{' struct_field* '}'`
fn struct_def(p: &mut Parser) {
    let m = p.start();
    p.bump(STRUCT_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.eat(L_BRACE) {
        while !p.at(R_BRACE) && !p.at(EOF) {
            if at_type_start(p) {
                struct_field(p);
            } else {
                p.err_and_bump("expected a struct field");
            }
        }
        p.expect(R_BRACE);
    } else {
        p.error("expected '{'");
    }
    m.complete(p, STRUCT_DEF);
}

/// `type_name NAME? ';'`
fn struct_field(p: &mut Parser) {
    let m = p.start();
    type_name(p);
    if p.at(IDENT) {
        name(p);
    }
    p.expect(SEMICOLON);
    m.complete(p, STRUCT_FIELD);
}

/// `'enum' NAME? '{' (enum_variant (',' enum_variant)*)? ','? '}'`
fn enum_def(p: &mut Parser) {
    let m = p.start();
    p.bump(ENUM_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.eat(L_BRACE) {
        while !p.at(R_BRACE) && !p.at(EOF) {
            if p.at(IDENT) {
                let v = p.start();
                name(p);
                v.complete(p, ENUM_VARIANT);
                if !p.eat(COMMA) {
                    break;
                }
            } else {
                p.err_and_bump("expected an enum variant");
            }
        }
        p.expect(R_BRACE);
    } else {
        p.error("expected '{'");
    }
    m.complete(p, ENUM_DEF);
}

// ---- events, errors & user-defined value types -------------------------------

/// `'event' NAME? param_list? IDENT? /*anonymous*/ ';'`
fn event_def(p: &mut Parser) {
    let m = p.start();
    p.bump(EVENT_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.at(L_PAREN) {
        param_list(p);
    }
    if p.at(IDENT) {
        p.bump_any(); // soft `anonymous`
    }
    p.expect(SEMICOLON);
    m.complete(p, EVENT_DEF);
}

/// `'error' NAME? param_list? ';'`
fn error_def(p: &mut Parser) {
    let m = p.start();
    p.bump(ERROR_KW);
    if p.at(IDENT) {
        name(p);
    }
    if p.at(L_PAREN) {
        param_list(p);
    }
    p.expect(SEMICOLON);
    m.complete(p, ERROR_DEF);
}

/// `'type' NAME? 'is' type_name ';'`
fn user_defined_value_type(p: &mut Parser) {
    let m = p.start();
    p.bump(TYPE_KW);
    if p.at(IDENT) {
        name(p);
    }
    p.expect(IS_KW);
    type_name(p);
    p.expect(SEMICOLON);
    m.complete(p, USER_DEFINED_VALUE_TYPE);
}

// ---- expressions -------------------------------------------------------------

/// Associativity of a binary operator level.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Assoc {
    Left,
    Right,
}

/// Binary-operator precedence level (1..=13) + associativity, per the Solidity
/// "Order of Precedence of Operators". `None` ⇒ the token is not a binary
/// operator and ends the operand. Assignment (level 1) and ternary (level 2) are
/// handled as special infixes in `expr_bp`, but they appear here too so the bp
/// comparison that decides whether to continue the loop is uniform.
fn bin_bp(kind: SyntaxKind) -> Option<(u8, Assoc)> {
    let r = match kind {
        EQ | PIPE_EQ | CARET_EQ | AMP_EQ | SHL_EQ | SHR_EQ | PLUS_EQ | MINUS_EQ | STAR_EQ
        | SLASH_EQ | PERCENT_EQ => (1, Assoc::Right),
        QUESTION => (2, Assoc::Right),
        PIPE2 => (3, Assoc::Left),
        AMP2 => (4, Assoc::Left),
        EQ2 | NEQ => (5, Assoc::Left),
        LT | GT | LT_EQ | GT_EQ => (6, Assoc::Left),
        PIPE => (7, Assoc::Left),
        CARET => (8, Assoc::Left),
        AMP => (9, Assoc::Left),
        SHL | SHR => (10, Assoc::Left),
        PLUS | MINUS => (11, Assoc::Left),
        STAR | SLASH | PERCENT => (12, Assoc::Left),
        STAR2 => (13, Assoc::Right),
        _ => return None,
    };
    Some(r)
}

/// Parse an expression. Entry point used by statements, initializers, args, etc.
fn expr(p: &mut Parser) {
    expr_bp(p, 0);
}

/// Precedence-climbing core. Parses an operand via `lhs`, then folds binary,
/// ternary and assignment operators whose binding power exceeds `min_bp`.
/// Returns the completed top expression, or `None` if nothing could start here
/// (the caller recovers). Never panics; always consumes ≥1 token when it returns
/// `Some` (via `lhs`), and breaks immediately on a non-operator.
fn expr_bp(p: &mut Parser, min_bp: u8) -> Option<CompletedMarker> {
    let mut lhs = lhs(p)?;
    loop {
        let Some((level, assoc)) = bin_bp(p.current()) else {
            break;
        };
        // Left binding power of this operator level. If it does not exceed the
        // caller's threshold, stop and let the caller fold it.
        let left_bp = level * 2;
        if left_bp <= min_bp {
            break;
        }
        match p.current() {
            QUESTION => {
                // ternary: `cond ? then : else` — right-assoc (level 2).
                let m = lhs.precede(p);
                p.bump(QUESTION);
                expr_bp(p, left_bp - 1); // then-branch
                p.expect(COLON);
                expr_bp(p, left_bp - 1); // else-branch
                lhs = m.complete(p, TERNARY_EXPR);
            }
            _ => {
                let node = if level == 1 { ASSIGN_EXPR } else { BIN_EXPR };
                let m = lhs.precede(p);
                p.bump_any(); // the operator token
                let rhs_min = if assoc == Assoc::Left {
                    left_bp
                } else {
                    left_bp - 1
                };
                expr_bp(p, rhs_min);
                lhs = m.complete(p, node);
            }
        }
    }
    Some(lhs)
}

/// Parse a unary-prefix-and-postfix operand: `prefix_op* primary postfix*`.
/// Returns `None` if no primary can start at `current()` (the caller recovers);
/// prefix and postfix handling come in Task 3 — for now `lhs` is just `primary`.
fn lhs(p: &mut Parser) -> Option<CompletedMarker> {
    primary(p)
}

/// Parse a primary expression. Returns `None` when `current()` cannot start one.
/// Task 3 extends this with `new`, `type(...)`, prefix ops and array literals.
fn primary(p: &mut Parser) -> Option<CompletedMarker> {
    let cm = match p.current() {
        NUMBER | STRING | TRUE_KW | FALSE_KW => {
            let m = p.start();
            p.bump_any();
            m.complete(p, LITERAL_EXPR)
        }
        IDENT => {
            let m = p.start();
            name_ref(p);
            m.complete(p, PATH_EXPR)
        }
        L_PAREN => paren_or_tuple_expr(p),
        _ => return None,
    };
    Some(cm)
}

/// `'(' expr ')'` ⇒ PAREN_EXPR; anything with a comma or a hole ⇒ TUPLE_EXPR.
/// Supports `()`, `(x)`, `(a, b)`, and holes like `(, x)` / `(a, , b)`.
fn paren_or_tuple_expr(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    p.bump(L_PAREN);
    let mut count = 0usize;
    let mut is_tuple = false;
    // Parse a comma-separated list where each element is optional (a hole).
    loop {
        if p.at(R_PAREN) || p.at(EOF) {
            break;
        }
        if p.at(COMMA) {
            // a hole: no element before this comma.
            is_tuple = true;
            p.bump(COMMA);
            continue;
        }
        if expr_bp(p, 0).is_none() {
            // not an expression and not `)`/`,` — recover one token to progress.
            p.err_and_bump("expected an expression");
            continue;
        }
        count += 1;
        if p.eat(COMMA) {
            is_tuple = true;
        } else {
            break;
        }
    }
    p.expect(R_PAREN);
    // `(x)` with exactly one element and no comma is a parenthesized expr;
    // everything else (commas, holes, `()`, multiple elems) is a tuple.
    let kind = if is_tuple || count != 1 {
        TUPLE_EXPR
    } else {
        PAREN_EXPR
    };
    m.complete(p, kind)
}

// ---- types -------------------------------------------------------------------

/// A type: a path/mapping/function base, then zero or more `[ size? ]` suffixes.
/// Array suffixes wrap left-associatively via `precede` (`uint[2][]` ⇒
/// ARRAY_TYPE(ARRAY_TYPE(uint, 2))).
fn type_name(p: &mut Parser) {
    let mut cm = match p.current() {
        MAPPING_KW => mapping_type(p),
        FUNCTION_KW => function_type(p),
        _ => path_type(p),
    };
    while p.at(L_BRACK) {
        let wrap = cm.precede(p);
        p.bump(L_BRACK);
        // array size is a constant expression — skipped until a later plan.
        while !p.at(R_BRACK) && !p.at(EOF) {
            p.bump_any();
        }
        p.expect(R_BRACK);
        cm = wrap.complete(p, ARRAY_TYPE);
    }
}

/// `name_ref ('.' name_ref)*` — covers elementary (`uint256`) and user (`A.B`)
/// type names alike; semantics tells them apart later. Returns the node so type
/// suffixes can wrap it (Task 3).
fn path_type(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    name_ref(p);
    while p.at(DOT) && p.nth(1) == IDENT {
        p.bump(DOT);
        name_ref(p);
    }
    m.complete(p, PATH_TYPE)
}

/// `'mapping' '(' type_name NAME? '=>' type_name NAME? ')'` — the optional names
/// are the 0.8.18+ named key/value syntax.
fn mapping_type(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    p.bump(MAPPING_KW);
    p.expect(L_PAREN);
    type_name(p);
    if p.at(IDENT) {
        name(p); // optional key name
    }
    p.expect(FAT_ARROW);
    type_name(p);
    if p.at(IDENT) {
        name(p); // optional value name
    }
    p.expect(R_PAREN);
    m.complete(p, MAPPING_TYPE)
}

/// `'function' param_list (visibility|mutability)* ('returns' param_list)?`
fn function_type(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    p.bump(FUNCTION_KW);
    if p.at(L_PAREN) {
        param_list(p);
    } else {
        p.error("expected '(' in function type");
    }
    while let INTERNAL_KW | EXTERNAL_KW | PUBLIC_KW | PRIVATE_KW | PURE_KW | VIEW_KW | PAYABLE_KW =
        p.current()
    {
        p.bump_any();
    }
    if p.eat(RETURNS_KW) {
        if p.at(L_PAREN) {
            param_list(p);
        } else {
            p.error("expected '(' after returns");
        }
    }
    m.complete(p, FUNCTION_TYPE)
}

/// `'(' (param (',' param)*)? ')'`
fn param_list(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    while !p.at(R_PAREN) && !p.at(EOF) {
        if !at_type_start(p) {
            // not the start of a parameter — recover one token and retry.
            p.err_and_bump("expected a parameter");
            continue;
        }
        param(p);
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    m.complete(p, PARAM_LIST);
}

/// `type_name (data_location | soft_modifier)* NAME?` — `soft_modifier` covers
/// `indexed`/`anonymous`-style words that lex as IDENT (a real name, if present,
/// is the final IDENT).
fn param(p: &mut Parser) {
    let m = p.start();
    type_name(p);
    loop {
        match p.current() {
            MEMORY_KW | STORAGE_KW | CALLDATA_KW => p.bump_any(),
            IDENT if p.nth(1) == IDENT => p.bump_any(),
            _ => break,
        }
    }
    if p.at(IDENT) {
        name(p);
    }
    m.complete(p, PARAM);
}

/// True when the current token can begin a `type_name`.
fn at_type_start(p: &Parser) -> bool {
    matches!(p.current(), IDENT | MAPPING_KW | FUNCTION_KW)
}

// ---- names -------------------------------------------------------------------

/// A defining name (binding occurrence).
fn name(p: &mut Parser) {
    if p.at(IDENT) {
        let m = p.start();
        p.bump(IDENT);
        m.complete(p, NAME);
    } else {
        p.error("expected a name");
    }
}

/// A referencing name (use occurrence) — used in type paths, base lists, etc.
fn name_ref(p: &mut Parser) {
    if p.at(IDENT) {
        let m = p.start();
        p.bump(IDENT);
        m.complete(p, NAME_REF);
    } else {
        p.error("expected a name");
    }
}

// ---- shared skips ------------------------------------------------------------

/// Balanced `'(' … ')'` whose interior is span-skipped (it holds expressions,
/// parsed in a later plan).
fn skip_parens(p: &mut Parser) {
    p.bump(L_PAREN);
    let mut depth = 1usize;
    while depth > 0 && !p.at(EOF) {
        match p.current() {
            L_PAREN => depth += 1,
            R_PAREN => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            break;
        }
        p.bump_any();
    }
    p.expect(R_PAREN);
}
