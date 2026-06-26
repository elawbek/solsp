//! Solidity declaration, type, expression & statement grammar. The parser emits
//! events; this module is the recursive-descent grammar that drives it.
//! Expressions use a precedence-climbing (Pratt) core; statements are a flat
//! dispatch on the current token (Plan 4). The inline-assembly (Yul) interior is
//! the remaining span-skip, parsed in a later plan.

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
/// are a real ARG_LIST of expressions (Plan 4).
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
        arg_list(p);
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
        arg_list(p);
    }
    m.complete(p, MODIFIER_INVOCATION);
}

/// A `{ … }` statement block: zero or more statements between the braces.
/// Each `stmt` consumes ≥1 token, so the loop always makes progress.
fn block(p: &mut Parser) {
    let m = p.start();
    p.bump(L_BRACE);
    while !p.at(R_BRACE) && !p.at(EOF) {
        stmt(p); // each `stmt` consumes ≥1 token, so this loop always progresses
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

/// Binding power above every binary level (level 13 ⇒ left bp 26). Unary prefix
/// binds tighter than `**`, so prefix operands are parsed at this bound.
const UNARY_BP: u8 = 27;

/// `prefix_op* primary postfix*`. A prefix operator wraps a recursively-parsed
/// operand (at unary binding power) in a PREFIX_EXPR; otherwise we parse a
/// primary and then apply the postfix cluster. Returns `None` only when no
/// prefix op is present AND no primary can start here.
fn lhs(p: &mut Parser) -> Option<CompletedMarker> {
    let cm = match p.current() {
        BANG | TILDE | MINUS | PLUS | PLUS2 | MINUS2 | DELETE_KW => {
            let m = p.start();
            p.bump_any(); // the prefix operator
                          // Operand binds at unary power (tighter than `**`). If the operand is
                          // missing, record an error but still complete the PREFIX_EXPR.
            if expr_bp(p, UNARY_BP).is_none() {
                p.error("expected an operand after unary operator");
            }
            m.complete(p, PREFIX_EXPR)
        }
        _ => primary(p)?,
    };
    Some(postfix(p, cm))
}

/// Apply zero or more postfix operators to an already-parsed operand:
/// call `(...)` / call-options `{...}(...)` / index `[...]` (with optional
/// calldata slice) / member `.x` / postfix `++` `--`. Each wraps via `precede`,
/// so they nest left-to-right. Loops until the next token is not a postfix.
fn postfix(p: &mut Parser, mut cm: CompletedMarker) -> CompletedMarker {
    loop {
        cm = match p.current() {
            L_PAREN => call_expr(p, cm),
            L_BRACE => {
                // call-options `{value: …}` must be followed by an arg list to be
                // a call; bare `{` is not a postfix (it begins a block elsewhere).
                if !is_call_options(p) {
                    break;
                }
                let m = cm.precede(p);
                call_options(p);
                arg_list(p);
                m.complete(p, CALL_EXPR)
            }
            L_BRACK => {
                let m = cm.precede(p);
                p.bump(L_BRACK);
                index_or_slice(p);
                p.expect(R_BRACK);
                m.complete(p, INDEX_EXPR)
            }
            DOT => {
                let m = cm.precede(p);
                p.bump(DOT);
                if p.at(IDENT) {
                    name_ref(p);
                } else {
                    p.error("expected a member name after '.'");
                }
                m.complete(p, MEMBER_EXPR)
            }
            PLUS2 | MINUS2 => {
                let m = cm.precede(p);
                p.bump_any(); // `++` or `--`
                m.complete(p, POSTFIX_EXPR)
            }
            _ => break,
        };
    }
    cm
}

/// `'(' (expr (',' expr)*)? ')'` ⇒ CALL_EXPR wrapping an ARG_LIST or, for the
/// `({a: 1, b: 2})` form, a NAMED_ARG_LIST.
fn call_expr(p: &mut Parser, cm: CompletedMarker) -> CompletedMarker {
    let m = cm.precede(p);
    if is_named_args(p) {
        named_arg_list(p);
    } else {
        arg_list(p);
    }
    m.complete(p, CALL_EXPR)
}

/// Lookahead: `(` immediately followed by `{` is the `({name: expr, …})` named-
/// argument call form.
fn is_named_args(p: &Parser) -> bool {
    p.at(L_PAREN) && p.nth(1) == L_BRACE
}

/// Lookahead for call-options: `{ IDENT :` (an options block), vs a bare `{`.
fn is_call_options(p: &Parser) -> bool {
    p.at(L_BRACE) && p.nth(1) == IDENT && p.nth(2) == COLON
}

/// `'(' (expr (',' expr)*)? ')'` ⇒ ARG_LIST.
fn arg_list(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    while !p.at(R_PAREN) && !p.at(EOF) {
        if expr_bp(p, 0).is_none() {
            p.err_and_bump("expected an argument expression");
            continue;
        }
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    m.complete(p, ARG_LIST);
}

/// `'(' '{' (NAME ':' expr (',' …)*)? '}' ')'` ⇒ NAMED_ARG_LIST. Called only
/// when `is_named_args` held, so we are positioned at `(`.
fn named_arg_list(p: &mut Parser) {
    let m = p.start();
    p.bump(L_PAREN);
    named_arg_fields(p);
    p.expect(R_PAREN);
    m.complete(p, NAMED_ARG_LIST);
}

/// `'{' (NAME ':' expr (',' …)*)? '}'` ⇒ CALL_OPTIONS (the `{value: …, gas: …}`
/// block between a callee and its argument list).
fn call_options(p: &mut Parser) {
    let m = p.start();
    named_arg_fields(p);
    m.complete(p, CALL_OPTIONS);
}

/// Shared `'{' (NAME ':' expr (',' …)*)? '}'` body used by both NAMED_ARG_LIST
/// and CALL_OPTIONS. Each name is a defining-position-free key; we model it as a
/// NAME node (a label) followed by `:` and an expression.
fn named_arg_fields(p: &mut Parser) {
    p.expect(L_BRACE);
    while !p.at(R_BRACE) && !p.at(EOF) {
        if p.at(IDENT) {
            name(p);
            p.expect(COLON);
            if expr_bp(p, 0).is_none() {
                p.error("expected a value expression");
            }
        } else {
            p.err_and_bump("expected a named argument");
            continue;
        }
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_BRACE);
}

/// Index or calldata slice inside `[ … ]`: `expr`, `expr : expr?`, `: expr?`, or
/// empty. The caller has already bumped `[` and will expect `]`.
fn index_or_slice(p: &mut Parser) {
    if p.at(R_BRACK) {
        return; // empty `[]` (rare, but keep it lossless rather than erroring)
    }
    if p.at(COLON) {
        // slice with no start: `[:end]` or `[:]`
        p.bump(COLON);
        if !p.at(R_BRACK) {
            expr(p);
        }
        return;
    }
    expr(p);
    if p.eat(COLON) {
        // slice `[start:end]` / `[start:]`
        if !p.at(R_BRACK) {
            expr(p);
        }
    }
}

/// Parse a primary expression. Returns `None` when `current()` cannot start one.
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
        NEW_KW => {
            // `new T` — the constructed type; the following `(args)` is attached
            // by the postfix loop as a CALL_EXPR.
            let m = p.start();
            p.bump(NEW_KW);
            type_name(p);
            m.complete(p, NEW_EXPR)
        }
        TYPE_KW => {
            // `type(T)` meta-expression; `.max`/`.min` attach via postfix MEMBER.
            let m = p.start();
            p.bump(TYPE_KW);
            p.expect(L_PAREN);
            type_name(p);
            p.expect(R_PAREN);
            m.complete(p, TYPE_EXPR)
        }
        PAYABLE_KW => {
            // `payable(x)` — a keyword-leaf path-shaped expr; the postfix `(`
            // makes it a CALL_EXPR. (No NAME_REF child — it's a leaf keyword.)
            let m = p.start();
            p.bump(PAYABLE_KW);
            m.complete(p, PATH_EXPR)
        }
        L_PAREN => paren_or_tuple_expr(p),
        L_BRACK => array_expr(p),
        _ => return None,
    };
    Some(cm)
}

/// `'[' (expr (',' expr)*)? ']'` ⇒ ARRAY_EXPR (an inline array literal).
fn array_expr(p: &mut Parser) -> CompletedMarker {
    let m = p.start();
    p.bump(L_BRACK);
    while !p.at(R_BRACK) && !p.at(EOF) {
        if expr_bp(p, 0).is_none() {
            p.err_and_bump("expected an array element");
            continue;
        }
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_BRACK);
    m.complete(p, ARRAY_EXPR)
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

// ---- statements --------------------------------------------------------------

/// Parse one statement. Dispatches on `current()`. CRITICAL: this must consume
/// ≥1 token on every input (including recovery) so the `block` loop can't spin —
/// the `_ =>` arm routes to `simple_statement`, which either parses a real
/// statement (consuming the trailing `;`) or recovers via `err_and_bump`. The
/// modifier placeholder `_;` flows through `_ =>` → `simple_statement` →
/// `expr_statement` (it's a PATH_EXPR `_` followed by `;`).
fn stmt(p: &mut Parser) {
    match p.current() {
        L_BRACE => block(p),
        IF_KW => if_stmt(p),
        FOR_KW => for_stmt(p),
        WHILE_KW => while_stmt(p),
        DO_KW => do_while_stmt(p),
        RETURN_KW => return_stmt(p),
        BREAK_KW => break_stmt(p),
        CONTINUE_KW => continue_stmt(p),
        EMIT_KW => emit_stmt(p),
        REVERT_KW => revert_stmt(p),
        TRY_KW => try_stmt(p),
        UNCHECKED_KW => unchecked_block(p),
        ASSEMBLY_KW => assembly_stmt(p),
        _ => simple_statement(p),
    }
}

/// `'if' '(' expr ')' stmt ('else' stmt)?` — `else` binds to the nearest `if`,
/// and `else if` is just an `else` whose statement is another `if_stmt`.
fn if_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(IF_KW);
    p.expect(L_PAREN);
    expr(p);
    p.expect(R_PAREN);
    stmt(p);
    if p.eat(ELSE_KW) {
        stmt(p);
    }
    m.complete(p, IF_STMT);
}

/// `'for' '(' (simple_statement | ';') expr? ';' expr? ')' stmt`. The init slot
/// is either a simple statement (which consumes its own `;`) or a bare `;`.
fn for_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(FOR_KW);
    p.expect(L_PAREN);
    // init
    if p.eat(SEMICOLON) {
        // empty init
    } else {
        simple_statement(p); // consumes the trailing `;`
    }
    // condition (optional)
    if !p.at(SEMICOLON) {
        expr(p);
    }
    p.expect(SEMICOLON);
    // update (optional)
    if !p.at(R_PAREN) {
        expr(p);
    }
    p.expect(R_PAREN);
    stmt(p);
    m.complete(p, FOR_STMT);
}

/// `'while' '(' expr ')' stmt`
fn while_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(WHILE_KW);
    p.expect(L_PAREN);
    expr(p);
    p.expect(R_PAREN);
    stmt(p);
    m.complete(p, WHILE_STMT);
}

/// `'do' stmt 'while' '(' expr ')' ';'`
fn do_while_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(DO_KW);
    stmt(p);
    p.expect(WHILE_KW);
    p.expect(L_PAREN);
    expr(p);
    p.expect(R_PAREN);
    p.expect(SEMICOLON);
    m.complete(p, DO_WHILE_STMT);
}

/// `'return' expr? ';'`
fn return_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(RETURN_KW);
    if !p.at(SEMICOLON) && !p.at(R_BRACE) && !p.at(EOF) {
        expr(p);
    }
    p.expect(SEMICOLON);
    m.complete(p, RETURN_STMT);
}

/// `'break' ';'`
fn break_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(BREAK_KW);
    p.expect(SEMICOLON);
    m.complete(p, BREAK_STMT);
}

/// `'continue' ';'`
fn continue_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(CONTINUE_KW);
    p.expect(SEMICOLON);
    m.complete(p, CONTINUE_STMT);
}

/// A simple statement: a local variable declaration or an expression statement,
/// each consuming through the trailing `;`.
fn simple_statement(p: &mut Parser) {
    if is_var_decl_start(p) || is_tuple_var_decl(p) {
        var_decl_statement(p);
    } else {
        expr_statement(p);
    }
}

/// `expr ';'` ⇒ EXPR_STMT. On a token that can't start an expression, recover by
/// bumping one token so the `block` loop progresses (and we never emit an empty
/// EXPR_STMT that would re-loop forever).
fn expr_statement(p: &mut Parser) {
    let m = p.start();
    if expr_bp(p, 0).is_none() {
        // Nothing parseable here: consume one token as an error and bail, still
        // completing an EXPR_STMT so the tree is well-formed and lossless.
        p.err_and_bump("expected a statement");
        m.complete(p, EXPR_STMT);
        return;
    }
    p.expect(SEMICOLON);
    m.complete(p, EXPR_STMT);
}

/// Speculative predicate: does a single-variable declaration start here? We
/// tentatively parse a `type_name` and check that an IDENT (the variable name)
/// or a data-location keyword follows; then rewind. Never consumes on return.
fn is_var_decl_start(p: &mut Parser) -> bool {
    let cp = p.checkpoint();
    type_name(p);
    let looks_like_decl =
        p.at(IDENT) || matches!(p.current(), MEMORY_KW | STORAGE_KW | CALLDATA_KW);
    p.rewind(cp);
    looks_like_decl
}

/// Speculative predicate for the leading-`(` tuple form `(uint a, bool b) = …`.
/// We only treat a `(` as a tuple var-decl when at least one element parses as
/// `type_name (loc)? IDENT` and the closing `)` is immediately followed by `=`.
/// Otherwise it is an expression statement (e.g. a tuple-assignment `(a, b) =`).
fn is_tuple_var_decl(p: &mut Parser) -> bool {
    if !p.at(L_PAREN) {
        return false;
    }
    let cp = p.checkpoint();
    let mut saw_typed_element = false;
    p.bump(L_PAREN);
    loop {
        if p.at(R_PAREN) || p.at(EOF) {
            break;
        }
        if p.at(COMMA) {
            p.bump(COMMA); // empty tuple slot
            continue;
        }
        // Try to parse `type_name (loc)? IDENT`. A bare expression element (e.g.
        // `a` in `(a, b)`) will parse `a` as a path type with no trailing IDENT.
        type_name(p);
        if matches!(p.current(), MEMORY_KW | STORAGE_KW | CALLDATA_KW) {
            p.bump_any();
        }
        if p.at(IDENT) {
            saw_typed_element = true;
            p.bump(IDENT);
        }
        if !p.eat(COMMA) {
            break;
        }
    }
    let closes_then_assigns = p.at(R_PAREN) && p.nth(1) == EQ;
    p.rewind(cp);
    saw_typed_element && closes_then_assigns
}

/// `var_decl ('=' expr)? ';'` (single) or
/// `'(' var_decl? (',' var_decl?)* ')' '=' expr ';'` (tuple) ⇒ VAR_DECL_STMT.
fn var_decl_statement(p: &mut Parser) {
    let m = p.start();
    if p.at(L_PAREN) {
        // tuple form
        p.bump(L_PAREN);
        loop {
            if p.at(R_PAREN) || p.at(EOF) {
                break;
            }
            if p.at(COMMA) {
                p.bump(COMMA); // empty slot
                continue;
            }
            var_decl(p);
            if !p.eat(COMMA) {
                break;
            }
        }
        p.expect(R_PAREN);
        p.expect(EQ);
        expr(p);
    } else {
        // single form
        var_decl(p);
        if p.eat(EQ) {
            expr(p);
        }
    }
    p.expect(SEMICOLON);
    m.complete(p, VAR_DECL_STMT);
}

/// `type_name (data_location)? NAME` ⇒ VAR_DECL (one declared variable).
fn var_decl(p: &mut Parser) {
    let m = p.start();
    type_name(p);
    if matches!(p.current(), MEMORY_KW | STORAGE_KW | CALLDATA_KW) {
        p.bump_any();
    }
    if p.at(IDENT) {
        name(p);
    } else {
        p.error("expected a variable name");
    }
    m.complete(p, VAR_DECL);
}

/// `'emit' expr ';'` — the expression is the event call (`Name(args)`).
fn emit_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(EMIT_KW);
    expr(p);
    p.expect(SEMICOLON);
    m.complete(p, EMIT_STMT);
}

/// `'revert' expr? ';'` — `revert;` is legal, as is `revert Err(args);` and the
/// string form `revert("msg");` (a call expression).
fn revert_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(REVERT_KW);
    if !p.at(SEMICOLON) && !p.at(EOF) {
        expr(p);
    }
    p.expect(SEMICOLON);
    m.complete(p, REVERT_STMT);
}

/// `'unchecked' block` ⇒ UNCHECKED_BLOCK (a block whose arithmetic does not
/// revert on over/underflow).
fn unchecked_block(p: &mut Parser) {
    let m = p.start();
    p.bump(UNCHECKED_KW);
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.error("expected '{' after 'unchecked'");
    }
    m.complete(p, UNCHECKED_BLOCK);
}

/// `'try' expr ('returns' param_list)? block catch_clause+`
fn try_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(TRY_KW);
    expr(p);
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
        p.error("expected '{' for try body");
    }
    while p.at(CATCH_KW) {
        catch_clause(p);
    }
    m.complete(p, TRY_STMT);
}

/// `'catch' (IDENT? param_list)? block` — the optional IDENT is the error name
/// (`Error`, `Panic`) and `param_list` binds the destructured values.
fn catch_clause(p: &mut Parser) {
    let m = p.start();
    p.bump(CATCH_KW);
    if p.at(IDENT) {
        name_ref(p); // catch Error(...) / catch Panic(...)
    }
    if p.at(L_PAREN) {
        param_list(p);
    }
    if p.at(L_BRACE) {
        block(p);
    } else {
        p.error("expected '{' for catch body");
    }
    m.complete(p, CATCH_CLAUSE);
}

/// `'assembly' STRING? ('(' <flags> ')')? '{' yul_block '}'` — a real ASSEMBLY_STMT
/// whose `{ … }` body is now a parsed YUL_BLOCK (Plan 5). The optional `"evmasm"`
/// dialect string and the `("memory-safe")` flag parens are still consumed as
/// opaque leaves (their internal structure is out of M1 scope).
fn assembly_stmt(p: &mut Parser) {
    let m = p.start();
    p.bump(ASSEMBLY_KW);
    if p.at(STRING) {
        p.bump(STRING); // dialect, e.g. "evmasm"
    }
    if p.at(L_PAREN) {
        skip_parens(p); // memory-safe flag — kept as a leaf-skip in M1
    }
    if p.at(L_BRACE) {
        yul_block(p); // real Yul interior (Plan 5)
    } else {
        p.error("expected '{' for assembly body");
    }
    m.complete(p, ASSEMBLY_STMT);
}

// ---- Yul (inline assembly) ---------------------------------------------------

/// `'{' yul_statement* '}'` ⇒ YUL_BLOCK. The caller guarantees we're at `{`. Each
/// `yul_statement` consumes ≥1 token (or `err_and_bump` does), so the loop always
/// makes progress.
fn yul_block(p: &mut Parser) {
    let m = p.start();
    p.bump(L_BRACE);
    while !p.at(R_BRACE) && !p.at(EOF) {
        yul_statement(p);
    }
    p.expect(R_BRACE);
    m.complete(p, YUL_BLOCK);
}

/// Dispatch one Yul statement on `current()`. CRITICAL: every arm consumes ≥1
/// token (keyword-led statements bump their keyword first; the identifier arm
/// bumps the identifier; `_ =>` recovers via `err_and_bump`), so the `yul_block`
/// loop can't spin. Final dispatch: nested blocks, `let` declarations, control
/// flow (`if`/`for`/`switch`), function definitions, `leave`/`break`/`continue`,
/// and identifier-led statements (assignment / call).
fn yul_statement(p: &mut Parser) {
    match p.current() {
        L_BRACE => yul_block(p),
        LET_KW => yul_var_decl(p),
        IF_KW => yul_if(p),
        FOR_KW => yul_for(p),
        SWITCH_KW => yul_switch(p),
        FUNCTION_KW => yul_function_def(p),
        LEAVE_KW => yul_leaf_stmt(p, LEAVE_KW, YUL_LEAVE),
        BREAK_KW => yul_leaf_stmt(p, BREAK_KW, YUL_BREAK),
        CONTINUE_KW => yul_leaf_stmt(p, CONTINUE_KW, YUL_CONTINUE),
        IDENT | RETURN_KW | REVERT_KW => yul_ident_statement(p),
        _ => p.err_and_bump("expected a Yul statement"),
    }
}

/// `'let' NAME (',' NAME)* (':=' yul_expr)?` ⇒ YUL_VAR_DECL. Binding names are
/// NAME nodes (defining occurrences). The optional initializer is a Yul
/// expression (call / path / literal — there are no binary operators in Yul).
fn yul_var_decl(p: &mut Parser) {
    let m = p.start();
    p.bump(LET_KW);
    name(p); // first binding name
    while p.eat(COMMA) {
        name(p);
    }
    if p.eat(COLON_EQ) && !yul_expr(p) {
        p.error("expected a Yul expression after ':='");
    }
    m.complete(p, YUL_VAR_DECL);
}

/// An identifier-led Yul statement. Yul is LL(1) here: a `(` immediately after the
/// leading identifier means a **function-call statement** (`mstore(p, v)`); any
/// other follow-set means an **assignment** whose targets are a path list
/// (`x := …`, `x, y := …`, `x.slot := …`). No speculation/`checkpoint` is needed —
/// one token of lookahead (`nth(1)`) disambiguates, because Yul has no dotted
/// callees and no postfix chains.
fn yul_ident_statement(p: &mut Parser) {
    if p.nth(1) == L_PAREN {
        yul_function_call(p); // a bare call statement ⇒ a YUL_FUNCTION_CALL node
        return;
    }
    let m = p.start();
    yul_path(p);
    while p.eat(COMMA) {
        yul_path(p);
    }
    p.expect(COLON_EQ);
    if !yul_expr(p) {
        p.error("expected a Yul expression after ':='");
    }
    m.complete(p, YUL_ASSIGNMENT);
}

/// `yul_function_call | yul_path | yul_literal` ⇒ one Yul expression. Returns
/// `true` if an expression was parsed (consuming ≥1 token), `false` if `current()`
/// can't begin one (the caller recovers). There are **no binary operators** in
/// Yul, so this is a flat choice — never reuse the Solidity `expr`/`expr_bp`.
fn yul_expr(p: &mut Parser) -> bool {
    match p.current() {
        NUMBER | STRING | TRUE_KW | FALSE_KW => {
            let m = p.start();
            p.bump_any();
            m.complete(p, YUL_LITERAL);
            true
        }
        IDENT | RETURN_KW | REVERT_KW => {
            if p.nth(1) == L_PAREN {
                yul_function_call(p);
            } else {
                yul_path(p);
            }
            true
        }
        _ => false,
    }
}

/// `yul_ident '(' (yul_expr (',' yul_expr)*)? ')'` ⇒ YUL_FUNCTION_CALL. The callee
/// (`mstore`, `add`, a user-defined Yul function, …) is a plain NAME_REF — opcodes
/// are NOT special-cased here (the IDE highlights them later via the builtins
/// registry). The arg loop advances on every branch, so it can't spin.
fn yul_function_call(p: &mut Parser) {
    let m = p.start();
    yul_name_ref(p); // callee identifier (or the `return`/`revert` builtin)
    p.expect(L_PAREN);
    while !p.at(R_PAREN) && !p.at(EOF) {
        if !yul_expr(p) {
            p.err_and_bump("expected a Yul argument expression");
            continue;
        }
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    m.complete(p, YUL_FUNCTION_CALL);
}

/// `yul_ident ('.' IDENT)*` ⇒ YUL_PATH (a dotted Yul identifier, e.g. `x` or
/// `x.slot`). Mirrors the Solidity `path_type` dotted-segment loop; dotted
/// segments are plain IDENTs.
fn yul_path(p: &mut Parser) {
    let m = p.start();
    yul_name_ref(p);
    while p.at(DOT) && p.nth(1) == IDENT {
        p.bump(DOT);
        name_ref(p);
    }
    m.complete(p, YUL_PATH);
}

/// A token that can begin a Yul identifier: a normal `IDENT`, plus the two
/// builtins that collide with Solidity keywords because the lexer is
/// context-free — `return` (RETURN_KW) and `revert` (REVERT_KW), as in the
/// ubiquitous `return(0, 0x20)` / `revert(p, s)`. They are the only two
/// collisions among Yul builtins.
fn at_yul_ident(p: &Parser) -> bool {
    matches!(p.current(), IDENT | RETURN_KW | REVERT_KW)
}

/// Consume a Yul identifier as a NAME_REF (a use occurrence). Accepts the
/// contextual `return`/`revert` builtin spellings in addition to `IDENT`.
fn yul_name_ref(p: &mut Parser) {
    if at_yul_ident(p) {
        let m = p.start();
        p.bump_any(); // IDENT, or the contextual `return`/`revert` keyword
        m.complete(p, NAME_REF);
    } else {
        p.error("expected a Yul identifier");
    }
}

/// `'if' yul_expr yul_block` ⇒ YUL_IF. NOTE the Yul shape: **no parentheses**
/// around the condition and **no `else`** branch (unlike Solidity `if`).
fn yul_if(p: &mut Parser) {
    let m = p.start();
    p.bump(IF_KW);
    if !yul_expr(p) {
        p.error("expected a Yul condition expression");
    }
    if p.at(L_BRACE) {
        yul_block(p);
    } else {
        p.error("expected '{' for if body");
    }
    m.complete(p, YUL_IF);
}

/// `'for' yul_block yul_expr yul_block yul_block` ⇒ YUL_FOR — the
/// `for { init } cond { post } { body }` shape. init/post/body are blocks; the
/// condition is a bare Yul expression (no parens).
fn yul_for(p: &mut Parser) {
    let m = p.start();
    p.bump(FOR_KW);
    // init block
    if p.at(L_BRACE) {
        yul_block(p);
    } else {
        p.error("expected '{' for for-init");
    }
    // condition
    if !yul_expr(p) {
        p.error("expected a Yul condition expression");
    }
    // post block
    if p.at(L_BRACE) {
        yul_block(p);
    } else {
        p.error("expected '{' for for-post");
    }
    // body block
    if p.at(L_BRACE) {
        yul_block(p);
    } else {
        p.error("expected '{' for for-body");
    }
    m.complete(p, YUL_FOR);
}

/// `'switch' yul_expr ( yul_case+ yul_default? | yul_default )` ⇒ YUL_SWITCH. The
/// `case`/`default` parse is keyword-driven; a `switch` with neither a case nor a
/// default is semantically illegal but still parses losslessly (semantics flags
/// it later).
fn yul_switch(p: &mut Parser) {
    let m = p.start();
    p.bump(SWITCH_KW);
    if !yul_expr(p) {
        p.error("expected a Yul switch expression");
    }
    while p.at(CASE_KW) {
        yul_case(p);
    }
    if p.at(DEFAULT_KW) {
        yul_default(p);
    }
    m.complete(p, YUL_SWITCH);
}

/// `'case' yul_literal yul_block` ⇒ YUL_CASE. The case label is a Yul literal.
fn yul_case(p: &mut Parser) {
    let m = p.start();
    p.bump(CASE_KW);
    yul_literal(p);
    if p.at(L_BRACE) {
        yul_block(p);
    } else {
        p.error("expected '{' for case body");
    }
    m.complete(p, YUL_CASE);
}

/// `'default' yul_block` ⇒ YUL_DEFAULT.
fn yul_default(p: &mut Parser) {
    let m = p.start();
    p.bump(DEFAULT_KW);
    if p.at(L_BRACE) {
        yul_block(p);
    } else {
        p.error("expected '{' for default body");
    }
    m.complete(p, YUL_DEFAULT);
}

/// `NUMBER | STRING | 'true' | 'false'` ⇒ YUL_LITERAL (hex/decimal numbers and
/// strings already lex as NUMBER/STRING). Used for `case` labels; the
/// expression-position literal is handled inline by `yul_expr`.
fn yul_literal(p: &mut Parser) {
    if matches!(p.current(), NUMBER | STRING | TRUE_KW | FALSE_KW) {
        let m = p.start();
        p.bump_any();
        m.complete(p, YUL_LITERAL);
    } else {
        p.error("expected a Yul literal");
    }
}

/// `'function' NAME yul_param_list ('->' NAME (',' NAME)*)? yul_block` ⇒
/// YUL_FUNCTION_DEF. Parameters are wrapped in a YUL_PARAM_LIST (they're
/// parenthesized, so the `(`/`)` give natural boundaries). The optional return
/// identifiers after `->` are inline NAME children of the YUL_FUNCTION_DEF — they
/// have no delimiters of their own, and the THIN_ARROW token marks where they
/// begin (mirroring how `yul_var_decl` lists its binding names inline).
fn yul_function_def(p: &mut Parser) {
    let m = p.start();
    p.bump(FUNCTION_KW);
    if p.at(IDENT) {
        name(p);
    } else {
        p.error("expected a Yul function name");
    }
    yul_param_list(p);
    if p.eat(THIN_ARROW) {
        name(p); // first return name
        while p.eat(COMMA) {
            name(p);
        }
    }
    if p.at(L_BRACE) {
        yul_block(p);
    } else {
        p.error("expected '{' for function body");
    }
    m.complete(p, YUL_FUNCTION_DEF);
}

/// `'(' (NAME (',' NAME)*)? ')'` ⇒ YUL_PARAM_LIST (a function definition's
/// parameter list — bare binding names, no types). The loop advances on every
/// branch (`name` bumps an IDENT, else `err_and_bump`), so it can't spin.
fn yul_param_list(p: &mut Parser) {
    let m = p.start();
    p.expect(L_PAREN);
    // Stop on `}` as well as `)`/EOF: a misplaced brace in an unterminated param
    // list (`function f( }`) should re-sync the enclosing `yul_block` rather than
    // be swallowed by `err_and_bump`.
    while !p.at(R_PAREN) && !p.at(R_BRACE) && !p.at(EOF) {
        if p.at(IDENT) {
            name(p);
        } else {
            p.err_and_bump("expected a parameter name");
            continue;
        }
        if !p.eat(COMMA) {
            break;
        }
    }
    p.expect(R_PAREN);
    m.complete(p, YUL_PARAM_LIST);
}

/// A single-keyword Yul flow statement (`leave` / `break` / `continue`). Each gets
/// its own node kind so the later AST layer distinguishes them without reading
/// token text (consistent with the project's text-free-parser stance). The caller
/// matched `current() == kw`, so `bump(kw)` is guarded.
fn yul_leaf_stmt(p: &mut Parser, kw: SyntaxKind, node: SyntaxKind) {
    let m = p.start();
    p.bump(kw);
    m.complete(p, node);
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
        if !p.at(R_BRACK) {
            expr(p); // array size is a constant expression (optional: `T[]` vs `T[N]`)
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

/// Balanced `'(' … ')'` whose interior is span-skipped. Callers: `override_spec`
/// (a list of contract/type paths, not expressions — structured in M2) and
/// `assembly_stmt` (the memory-safe flags — structured in Plan 5).
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
