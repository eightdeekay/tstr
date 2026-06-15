use winnow::prelude::*;
use winnow::error::ModalResult;
use winnow::combinator::{alt, opt, delimited};
use winnow::token::take_while;
use winnow::ascii::digit1;

use crate::ast::*;
use super::primitives::{identifier, quoted_string, escaped_string, property_key, var_list, brace_matched_content, regex_literal, ws};

// ---------------------------------------------------------------------------
// Atoms — the smallest expression units
// ---------------------------------------------------------------------------

/// Parse a number literal: integer or float.
fn number(input: &mut &str) -> ModalResult<Expr> {
    let negative = opt('-').parse_next(input)?;
    let digits = digit1.parse_next(input)?;
    let decimal = opt(('.', digit1)).parse_next(input)?;

    let mut s = String::new();
    if negative.is_some() {
        s.push('-');
    }
    s.push_str(digits);
    if let Some((_, frac)) = decimal {
        s.push('.');
        s.push_str(frac);
    }
    let n: f64 = s.parse().unwrap();
    Ok(Expr::Number(n))
}

/// Parse a string literal: "content" (with escape support)
fn string_literal(input: &mut &str) -> ModalResult<Expr> {
    escaped_string.map(Expr::StringLiteral).parse_next(input)
}

/// Parse `null`
fn null_literal(input: &mut &str) -> ModalResult<Expr> {
    "null".map(|_| Expr::Null).parse_next(input)
}

/// Parse `true` or `false`
fn bool_literal(input: &mut &str) -> ModalResult<Expr> {
    alt((
        "true".map(|_| Expr::Bool(true)),
        "false".map(|_| Expr::Bool(false)),
    )).parse_next(input)
}

/// Parse `@path/to/file`
fn file_ref(input: &mut &str) -> ModalResult<Expr> {
    '@'.parse_next(input)?;
    let path = take_while(1.., |c: char| !c.is_whitespace() && c != ')' && c != ',' && c != ';')
        .parse_next(input)?;
    Ok(Expr::FileRef(path.to_string()))
}

/// Parse `{{varName}}`
fn interpolated(input: &mut &str) -> ModalResult<Expr> {
    "{{".parse_next(input)?;
    let name = take_while(1.., |c: char| c != '}').parse_next(input)?;
    "}}".parse_next(input)?;
    Ok(Expr::Interpolated(name.to_string()))
}

/// Parse `${name}` or `${name.sub.field}` — constant reference.
/// The dotted path is stored as one string; the evaluator splits and walks it.
fn constant_ref(input: &mut &str) -> ModalResult<Expr> {
    "${".parse_next(input)?;
    let path = take_while(1.., |c: char| {
        c.is_alphanumeric() || c == '_' || c == '.'
    }).parse_next(input)?;
    '}'.parse_next(input)?;
    Ok(Expr::ConstantRef(path.to_string()))
}

/// Parse `name(args, ...)` — library call.
/// Requires `(` immediately after the identifier; backtracks otherwise so
/// bare identifiers still parse via the trailing `identifier` atom.
/// HTTP verb names are excluded — those are handled by HTTP-call parsing.
fn lib_call(input: &mut &str) -> ModalResult<Expr> {
    let saved = *input;
    let name = match identifier.parse_next(input) {
        Ok(n) => n.to_string(),
        Err(e) => { *input = saved; return Err(e); }
    };
    // Must be followed by `(` to qualify as a call; no whitespace allowed
    // (so that `r == foo(...)` doesn't accidentally split).
    if !input.starts_with('(') {
        *input = saved;
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::new(),
        ));
    }
    // HTTP verbs are reserved at the statement level and have their own
    // call parsing path. Don't shadow them.
    if matches!(name.as_str(),
        "get" | "post" | "put" | "patch" | "delete" | "head" | "options")
    {
        *input = saved;
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::new(),
        ));
    }
    '('.parse_next(input)?;
    ws.parse_next(input)?;
    let mut args = Vec::new();
    if !input.starts_with(')') {
        args.push(expr.parse_next(input)?);
        ws.parse_next(input)?;
        while input.starts_with(',') {
            *input = &input[1..];
            ws.parse_next(input)?;
            args.push(expr.parse_next(input)?);
            ws.parse_next(input)?;
        }
    }
    ')'.parse_next(input)?;
    Ok(Expr::LibCall { name, args })
}

/// Parse a `js:{ ... }` block — brace-matched, content is opaque.
fn js_block(input: &mut &str) -> ModalResult<Expr> {
    "js:".parse_next(input)?;
    let code = brace_matched_content.parse_next(input)?;
    Ok(Expr::JsExpr(code))
}

/// Parse a built-in function call: `$.uuid()`, `$.string(10)`, `$.randEmail("doug")`
fn builtin_call(input: &mut &str) -> ModalResult<Expr> {
    "$.".parse_next(input)?;
    let name = identifier.map(String::from).parse_next(input)?;
    ws.parse_next(input)?;
    '('.parse_next(input)?;
    ws.parse_next(input)?;

    let mut args = Vec::new();
    if !input.starts_with(')') {
        args.push(expr.parse_next(input)?);
        ws.parse_next(input)?;
        while input.starts_with(',') {
            *input = &input[1..];
            ws.parse_next(input)?;
            args.push(expr.parse_next(input)?);
            ws.parse_next(input)?;
        }
    }
    ')'.parse_next(input)?;

    Ok(Expr::BuiltinCall { name, args })
}

/// Parse a brace expression: disambiguates between JSON objects and tstr blocks.
/// - `{}` → empty JSON object
/// - `{ key: value }` → JSON object
/// - `{ "key": value }` → JSON object
/// - `{ item --> ... <-- ... }` → tstr block
/// - `{ --> ... }` → tstr block with no inputs
fn brace_expr(input: &mut &str) -> ModalResult<Expr> {
    '{'.parse_next(input)?;
    ws.parse_next(input)?;

    // Empty object: {}
    if input.starts_with('}') {
        *input = &input[1..];
        return Ok(Expr::JsonObject(Vec::new()));
    }

    // Peek ahead to determine if this is a tstr block or JSON.
    // tstr block: starts with `-->` or `identifier -->` or `identifier, identifier -->`
    if input.starts_with("-->") {
        "-->".parse_next(input)?;
        return parse_tstr_block_body(Vec::new(), input);
    }

    // Peek: scan ahead for `-->` before any `:` to distinguish block from JSON.
    let is_block = {
        let peek = *input;
        // Look for `-->` that appears before any `:` (JSON key-value separator)
        let arrow_pos = peek.find("-->");
        let colon_pos = peek.find(':');
        match (arrow_pos, colon_pos) {
            (Some(a), Some(c)) => a < c,
            (Some(_), None) => true,
            _ => false,
        }
    };

    if is_block {
        // Parse block inputs: `item` or `item, other`
        let inputs = var_list.parse_next(input)?;
        ws.parse_next(input)?;
        "-->".parse_next(input)?;
        parse_tstr_block_body(inputs, input)
    } else {
        parse_json_object_body(input)
    }
}

/// Parse the body of a tstr block after `-->` has been consumed.
/// Supports two forms:
///   - Predicate/expression block: `{ item --> item.active == true }` (no semicolons)
///   - Statement block: `{ item --> x = item.id; <-- x; }` (with semicolons)
fn parse_tstr_block_body(inputs: Vec<String>, input: &mut &str) -> ModalResult<Expr> {
    use super::statement::statements;

    ws.parse_next(input)?;

    // Try parsing as a single expression (predicate block).
    // Save position so we can backtrack if it fails.
    let saved = *input;
    if let Ok(predicate_expr) = expr(input) {
        ws.parse_next(input)?;
        // If we hit `}` right after, it's a predicate block
        if input.starts_with('}') {
            *input = &input[1..];
            // Wrap the expression as an assertion statement so the evaluator can use it
            let body = vec![Statement::Assertion {
                expr: predicate_expr.clone(),
                message: format!("block predicate failed"),
            }];
            return Ok(Expr::Block { inputs, body, outputs: Vec::new() });
        }
        // Didn't hit `}` — backtrack and try as statements
        *input = saved;
    } else {
        *input = saved;
    }

    // Parse as statements
    let body = statements.parse_next(input)?;

    ws.parse_next(input)?;
    let outputs = if input.starts_with("<--") {
        "<--".parse_next(input)?;
        ws.parse_next(input)?;
        let vars = var_list.parse_next(input)?;
        ws.parse_next(input)?;
        opt(';').parse_next(input)?;
        vars
    } else {
        Vec::new()
    };

    ws.parse_next(input)?;
    '}'.parse_next(input)?;

    Ok(Expr::Block { inputs, body, outputs })
}

/// Parse the body of a JSON object after `{` and whitespace have been consumed.
///
/// Supports ES6-style shorthand: `{ name }` is sugar for `{ name: name }`,
/// and mixes freely with explicit pairs (`{ x, y: 2 }`). Shorthand only
/// applies to bare-identifier keys — a quoted key with no value is an error.
fn parse_json_object_body(input: &mut &str) -> ModalResult<Expr> {
    let mut entries = Vec::new();

    loop {
        ws.parse_next(input)?;
        // Track whether the key was a bare identifier (eligible for shorthand).
        let (key, was_ident) = alt((
            quoted_string.map(|s| (s.to_string(), false)),
            identifier.map(|s| (s.to_string(), true)),
        )).parse_next(input)?;

        ws.parse_next(input)?;

        let value = if input.starts_with(':') {
            ':'.parse_next(input)?;
            ws.parse_next(input)?;
            expr.parse_next(input)?
        } else if was_ident && (input.starts_with(',') || input.starts_with('}')) {
            // Shorthand: `{ name }` == `{ name: name }`.
            Expr::Identifier(key.clone())
        } else {
            // Quoted key with no value, or a key followed by neither `:` nor
            // a terminator — not a valid object entry.
            return Err(winnow::error::ErrMode::Backtrack(
                winnow::error::ContextError::new(),
            ));
        };
        entries.push((key, value));

        ws.parse_next(input)?;
        if input.starts_with('}') {
            *input = &input[1..];
            break;
        }
        ','.parse_next(input)?;
    }

    Ok(Expr::JsonObject(entries))
}

/// Parse a JSON array: `[expr, expr, expr]`
fn json_array(input: &mut &str) -> ModalResult<Expr> {
    '['.parse_next(input)?;
    ws.parse_next(input)?;

    let mut items = Vec::new();

    // Empty array
    if input.starts_with(']') {
        *input = &input[1..];
        return Ok(Expr::JsonArray(items));
    }

    loop {
        ws.parse_next(input)?;
        let value = expr.parse_next(input)?;
        items.push(value);

        ws.parse_next(input)?;
        if input.starts_with(']') {
            *input = &input[1..];
            break;
        }
        ','.parse_next(input)?;
    }

    Ok(Expr::JsonArray(items))
}

/// Parse an atom — the smallest expression unit.
fn atom(input: &mut &str) -> ModalResult<Expr> {
    alt((
        // Parenthesized expression
        delimited(('(', ws), expr, (ws, ')')),
        // Built-in function: $.uuid(), $.string(10)
        builtin_call,
        // Constant reference: ${name}, ${name.sub.field}
        constant_ref,
        // Library call: foo(args) — must come before bare identifier
        lib_call,
        // js:{ block }
        js_block,
        // JSON object or tstr block: { ... }
        brace_expr,
        // JSON array: [value, value]
        json_array,
        // Literals
        null_literal,
        bool_literal,
        string_literal,
        file_ref,
        interpolated,
        number,
        // Identifier (must be last — it's greedy)
        identifier.map(|s| Expr::Identifier(s.to_string())),
    )).parse_next(input)
}

// ---------------------------------------------------------------------------
// Postfix — property access, indexing, optional chaining
// ---------------------------------------------------------------------------

/// Parse an index operation: `0`, `-1`, `0:3`
fn parse_index_op(input: &mut &str) -> ModalResult<IndexOp> {
    let neg = opt('-').parse_next(input)?;
    let digits = digit1.parse_next(input)?;
    let mut idx: i64 = digits.parse().unwrap();
    if neg.is_some() {
        idx = -idx;
    }

    if let Some(_) = opt(':').parse_next(input)? {
        let end_neg = opt('-').parse_next(input)?;
        let end_digits = opt(digit1).parse_next(input)?;
        let end = end_digits.map(|d: &str| {
            let mut n: i64 = d.parse().unwrap();
            if end_neg.is_some() {
                n = -n;
            }
            n
        });
        Ok(IndexOp::Slice(Some(idx), end))
    } else {
        Ok(IndexOp::Single(idx))
    }
}

/// Parse postfix operations: .field, ?.field, [index], [].field
fn postfix(input: &mut &str) -> ModalResult<Expr> {
    let mut result = atom.parse_next(input)?;

    loop {
        // Try optional chaining: ?.field
        if input.starts_with("?.") {
            *input = &input[2..];
            let key = property_key.parse_next(input)?;
            result = Expr::OptionalAccess {
                object: Box::new(result),
                key,
            };
            continue;
        }

        // Try property access or method call: .field, ."quoted", .method(args)
        if input.starts_with('.') && !input.starts_with("..") {
            *input = &input[1..];

            // Check if this is a method call: identifier followed by (
            let saved = *input;
            if let Ok(name) = identifier.parse_next(input) {
                if input.starts_with('(') {
                    // Method call: .filter({ ... }), .find({ ... }), etc.
                    *input = &input[1..]; // consume (
                    ws.parse_next(input)?;
                    let mut args = Vec::new();
                    if !input.starts_with(')') {
                        args.push(expr.parse_next(input)?);
                        ws.parse_next(input)?;
                        while input.starts_with(',') {
                            *input = &input[1..];
                            ws.parse_next(input)?;
                            args.push(expr.parse_next(input)?);
                            ws.parse_next(input)?;
                        }
                    }
                    ')'.parse_next(input)?;
                    result = Expr::MethodCall {
                        object: Box::new(result),
                        method: name.to_string(),
                        args,
                    };
                    continue;
                } else {
                    // Not a method call — backtrack and parse as property access
                    *input = saved;
                }
            } else {
                *input = saved;
            }

            let key = property_key.parse_next(input)?;
            result = Expr::PropertyAccess {
                object: Box::new(result),
                key,
            };
            continue;
        }

        // Try index access: [expr]
        if input.starts_with('[') {
            *input = &input[1..];

            // Check for collect: []
            if input.starts_with(']') {
                *input = &input[1..];
                '.'.parse_next(input)?;
                let key = property_key.parse_next(input)?;
                result = Expr::CollectAccess {
                    object: Box::new(result),
                    key,
                };
                continue;
            }

            // Parse index or slice
            let idx = parse_index_op.parse_next(input)?;
            ']'.parse_next(input)?;
            result = Expr::IndexAccess {
                object: Box::new(result),
                index: Box::new(idx),
            };
            continue;
        }

        break;
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Operators — precedence climbing
// ---------------------------------------------------------------------------

/// Parse unary: `!expr`
fn unary(input: &mut &str) -> ModalResult<Expr> {
    // Peek: `!` but NOT `!=` or `!~`
    if input.starts_with('!') && !input.starts_with("!=") && !input.starts_with("!~") {
        *input = &input[1..];
        ws.parse_next(input)?;
        let operand = unary.parse_next(input)?;
        Ok(Expr::Not(Box::new(operand)))
    } else {
        postfix.parse_next(input)
    }
}

/// Parse multiplicative: `*`, `/`, `%`
fn multiplicative(input: &mut &str) -> ModalResult<Expr> {
    let mut left = unary.parse_next(input)?;

    loop {
        ws.parse_next(input)?;
        let op = opt(alt((
            '*'.map(|_| BinOp::Mul),
            '/'.map(|_| BinOp::Div),
            '%'.map(|_| BinOp::Mod),
        ))).parse_next(input)?;

        match op {
            Some(op) => {
                ws.parse_next(input)?;
                let right = unary.parse_next(input)?;
                left = Expr::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                };
            }
            None => break,
        }
    }
    Ok(left)
}

/// Parse additive: `+`, `-`
fn additive(input: &mut &str) -> ModalResult<Expr> {
    let mut left = multiplicative.parse_next(input)?;

    loop {
        ws.parse_next(input)?;
        let op = opt(alt((
            '+'.map(|_| BinOp::Add),
            '-'.map(|_| BinOp::Sub),
        ))).parse_next(input)?;

        match op {
            Some(op) => {
                ws.parse_next(input)?;
                let right = multiplicative.parse_next(input)?;
                left = Expr::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                };
            }
            None => break,
        }
    }
    Ok(left)
}

/// Parse comparison: `==`, `!=`, `>`, `<`, `>=`, `<=`, `~`, `~?`, `!~`
fn comparison(input: &mut &str) -> ModalResult<Expr> {
    let left = additive.parse_next(input)?;

    ws.parse_next(input)?;
    let op = opt(alt((
        "==".map(|_| BinOp::Eq),
        "!=".map(|_| BinOp::NotEq),
        ">=".map(|_| BinOp::Gte),
        "<=".map(|_| BinOp::Lte),
        ">".map(|_| BinOp::Gt),
        "<".map(|_| BinOp::Lt),
        "~?".map(|_| BinOp::RegexTest),
        "!~".map(|_| BinOp::RegexNoMatch),
        "~".map(|_| BinOp::RegexExtract),
    ))).parse_next(input)?;

    match op {
        Some(op) => {
            ws.parse_next(input)?;
            let right = match op {
                BinOp::RegexExtract | BinOp::RegexTest | BinOp::RegexNoMatch => {
                    regex_literal.map(|s| Expr::StringLiteral(s)).parse_next(input)?
                }
                _ => additive.parse_next(input)?,
            };
            Ok(Expr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            })
        }
        None => Ok(left),
    }
}

/// Parse logical AND: `&&`
fn logical_and(input: &mut &str) -> ModalResult<Expr> {
    let mut left = comparison.parse_next(input)?;

    loop {
        ws.parse_next(input)?;
        if let Some(_) = opt("&&").parse_next(input)? {
            ws.parse_next(input)?;
            let right = comparison.parse_next(input)?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::And,
                right: Box::new(right),
            };
        } else {
            break;
        }
    }
    Ok(left)
}

/// Parse logical OR: `||`
fn logical_or(input: &mut &str) -> ModalResult<Expr> {
    let mut left = logical_and.parse_next(input)?;

    loop {
        ws.parse_next(input)?;
        if let Some(_) = opt("||").parse_next(input)? {
            ws.parse_next(input)?;
            let right = logical_and.parse_next(input)?;
            left = Expr::BinaryOp {
                left: Box::new(left),
                op: BinOp::Or,
                right: Box::new(right),
            };
        } else {
            break;
        }
    }
    Ok(left)
}

/// Parse pipe operations: `expr | any(predicate)`, `expr | all(predicate)`
fn pipe_op(input: &mut &str) -> ModalResult<Expr> {
    let mut result = logical_or.parse_next(input)?;

    loop {
        let saved = *input;
        ws.parse_next(input)?;
        if input.starts_with('|') && !input.starts_with("||") {
            let after_pipe = &input[1..];
            let trimmed = after_pipe.trim_start();
            if trimmed.starts_with("any(") || trimmed.starts_with("all(") {
                *input = &input[1..]; // consume |
                ws.parse_next(input)?;

                let func_name = identifier.parse_next(input)?;
                '('.parse_next(input)?;
                ws.parse_next(input)?;

                // Parse the predicate block
                let predicate = expr.parse_next(input)?;
                ws.parse_next(input)?;
                ')'.parse_next(input)?;

                let pipe_func = match func_name {
                    "any" => PipeFunc::Any(Box::new(predicate)),
                    "all" => PipeFunc::All(Box::new(predicate)),
                    _ => {
                        *input = saved;
                        break;
                    }
                };

                result = Expr::PipeOp {
                    left: Box::new(result),
                    op: pipe_func,
                };
                continue;
            }
        }
        *input = saved;
        break;
    }

    Ok(result)
}

/// Parse a full expression — entry point for expression parsing.
/// Does NOT consume `| "message"` — that's handled at the statement level
/// for assertions, and by `expr_with_guard` for inline guards in assignments.
pub fn expr(input: &mut &str) -> ModalResult<Expr> {
    pipe_op.parse_next(input)
}

/// Parse an expression with an optional guard: `expr | "message"`.
/// Used in assignment context where `| "message"` means "value must not be null."
pub fn expr_with_guard(input: &mut &str) -> ModalResult<Expr> {
    let result = pipe_op.parse_next(input)?;

    let saved = *input;
    ws.parse_next(input)?;
    if input.starts_with('|') && !input.starts_with("||") {
        *input = &input[1..];
        ws.parse_next(input)?;
        if input.starts_with('"') {
            let message = escaped_string.parse_next(input)?;
            Ok(Expr::Guard {
                expr: Box::new(result),
                message,
            })
        } else {
            *input = saved;
            Ok(result)
        }
    } else {
        *input = saved;
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Atom tests ---

    #[test]
    fn test_number() {
        let mut input = "42";
        assert_eq!(expr(&mut input).unwrap(), Expr::Number(42.0));
    }

    #[test]
    fn test_float() {
        let mut input = "3.14";
        assert_eq!(expr(&mut input).unwrap(), Expr::Number(3.14));
    }

    #[test]
    fn test_negative_number() {
        let mut input = "-5";
        assert_eq!(expr(&mut input).unwrap(), Expr::Number(-5.0));
    }

    #[test]
    fn test_string() {
        let mut input = "\"hello\"";
        assert_eq!(expr(&mut input).unwrap(), Expr::StringLiteral("hello".to_string()));
    }

    #[test]
    fn test_null() {
        let mut input = "null";
        assert_eq!(expr(&mut input).unwrap(), Expr::Null);
    }

    #[test]
    fn test_constant_ref_simple() {
        let mut input = "${apiVersion}";
        assert_eq!(expr(&mut input).unwrap(), Expr::ConstantRef("apiVersion".to_string()));
    }

    #[test]
    fn test_constant_ref_dotted() {
        let mut input = "${orgService.baseUrl}";
        assert_eq!(
            expr(&mut input).unwrap(),
            Expr::ConstantRef("orgService.baseUrl".to_string()),
        );
    }

    #[test]
    fn test_lib_call_no_args() {
        let mut input = "foo()";
        match expr(&mut input).unwrap() {
            Expr::LibCall { name, args } => {
                assert_eq!(name, "foo");
                assert!(args.is_empty());
            }
            other => panic!("expected LibCall, got {:?}", other),
        }
    }

    #[test]
    fn test_lib_call_with_args() {
        let mut input = "createTag(\"name\", 42)";
        match expr(&mut input).unwrap() {
            Expr::LibCall { name, args } => {
                assert_eq!(name, "createTag");
                assert_eq!(args.len(), 2);
            }
            other => panic!("expected LibCall, got {:?}", other),
        }
    }

    #[test]
    fn test_http_verb_does_not_parse_as_lib() {
        // `get(req, "/path")` would be a top-level expression in some
        // contexts — but `get` is a reserved HTTP verb and must NOT parse
        // as a lib call. It should backtrack and fall through.
        let mut input = "get(x)";
        let result = expr(&mut input);
        // Either it doesn't parse at all (no get() expression form) or it
        // parses as something other than LibCall. Either way, NOT a LibCall.
        if let Ok(Expr::LibCall { name, .. }) = result {
            panic!("HTTP verb '{}' must not become a LibCall", name);
        }
    }

    #[test]
    fn test_bare_identifier_still_parses() {
        // After adding lib_call, bare `foo` (no parens) must still parse
        // as Identifier — backtracking has to work.
        let mut input = "foo";
        assert_eq!(expr(&mut input).unwrap(), Expr::Identifier("foo".to_string()));
    }

    #[test]
    fn test_constant_ref_distinct_from_builtin() {
        // `$.uuid()` is a builtin call; `${name}` is a constant ref.
        // They must not collide in parsing.
        let mut input = "${x}";
        assert_eq!(expr(&mut input).unwrap(), Expr::ConstantRef("x".to_string()));
        let mut input2 = "$.uuid()";
        match expr(&mut input2).unwrap() {
            Expr::BuiltinCall { name, .. } => assert_eq!(name, "uuid"),
            other => panic!("expected BuiltinCall, got {:?}", other),
        }
    }

    #[test]
    fn test_bool() {
        let mut input = "true";
        assert_eq!(expr(&mut input).unwrap(), Expr::Bool(true));
    }

    #[test]
    fn test_identifier() {
        let mut input = "groupId";
        assert_eq!(expr(&mut input).unwrap(), Expr::Identifier("groupId".to_string()));
    }

    #[test]
    fn test_file_ref() {
        let mut input = "@fixtures/group.json";
        assert_eq!(expr(&mut input).unwrap(), Expr::FileRef("fixtures/group.json".to_string()));
    }

    #[test]
    fn test_interpolated() {
        let mut input = "{{profile}}";
        assert_eq!(expr(&mut input).unwrap(), Expr::Interpolated("profile".to_string()));
    }

    // --- Postfix tests ---

    #[test]
    fn test_property_access() {
        let mut input = "r.id";
        assert_eq!(expr(&mut input).unwrap(), Expr::PropertyAccess {
            object: Box::new(Expr::Identifier("r".to_string())),
            key: PropertyKey::Name("id".to_string()),
        });
    }

    #[test]
    fn test_nested_property() {
        let mut input = "r.group.name";
        assert_eq!(expr(&mut input).unwrap(), Expr::PropertyAccess {
            object: Box::new(Expr::PropertyAccess {
                object: Box::new(Expr::Identifier("r".to_string())),
                key: PropertyKey::Name("group".to_string()),
            }),
            key: PropertyKey::Name("name".to_string()),
        });
    }

    #[test]
    fn test_quoted_property() {
        let mut input = "r.\"content-type\"";
        assert_eq!(expr(&mut input).unwrap(), Expr::PropertyAccess {
            object: Box::new(Expr::Identifier("r".to_string())),
            key: PropertyKey::Quoted("content-type".to_string()),
        });
    }

    #[test]
    fn test_optional_chaining() {
        let mut input = "r?.name";
        assert_eq!(expr(&mut input).unwrap(), Expr::OptionalAccess {
            object: Box::new(Expr::Identifier("r".to_string())),
            key: PropertyKey::Name("name".to_string()),
        });
    }

    #[test]
    fn test_index_access() {
        let mut input = "r.items[0]";
        assert_eq!(expr(&mut input).unwrap(), Expr::IndexAccess {
            object: Box::new(Expr::PropertyAccess {
                object: Box::new(Expr::Identifier("r".to_string())),
                key: PropertyKey::Name("items".to_string()),
            }),
            index: Box::new(IndexOp::Single(0)),
        });
    }

    #[test]
    fn test_negative_index() {
        let mut input = "r.items[-1]";
        assert_eq!(expr(&mut input).unwrap(), Expr::IndexAccess {
            object: Box::new(Expr::PropertyAccess {
                object: Box::new(Expr::Identifier("r".to_string())),
                key: PropertyKey::Name("items".to_string()),
            }),
            index: Box::new(IndexOp::Single(-1)),
        });
    }

    #[test]
    fn test_slice() {
        let mut input = "r.items[0:3]";
        assert_eq!(expr(&mut input).unwrap(), Expr::IndexAccess {
            object: Box::new(Expr::PropertyAccess {
                object: Box::new(Expr::Identifier("r".to_string())),
                key: PropertyKey::Name("items".to_string()),
            }),
            index: Box::new(IndexOp::Slice(Some(0), Some(3))),
        });
    }

    #[test]
    fn test_collect() {
        let mut input = "r.items[].id";
        assert_eq!(expr(&mut input).unwrap(), Expr::CollectAccess {
            object: Box::new(Expr::PropertyAccess {
                object: Box::new(Expr::Identifier("r".to_string())),
                key: PropertyKey::Name("items".to_string()),
            }),
            key: PropertyKey::Name("id".to_string()),
        });
    }

    // --- Operator tests ---

    #[test]
    fn test_equality() {
        let mut input = "r.code == 200";
        assert_eq!(expr(&mut input).unwrap(), Expr::BinaryOp {
            left: Box::new(Expr::PropertyAccess {
                object: Box::new(Expr::Identifier("r".to_string())),
                key: PropertyKey::Name("code".to_string()),
            }),
            op: BinOp::Eq,
            right: Box::new(Expr::Number(200.0)),
        });
    }

    #[test]
    fn test_not_equal_null() {
        let mut input = "r.id != null";
        assert_eq!(expr(&mut input).unwrap(), Expr::BinaryOp {
            left: Box::new(Expr::PropertyAccess {
                object: Box::new(Expr::Identifier("r".to_string())),
                key: PropertyKey::Name("id".to_string()),
            }),
            op: BinOp::NotEq,
            right: Box::new(Expr::Null),
        });
    }

    #[test]
    fn test_logical_and() {
        let mut input = "a > 0 && b < 10";
        assert_eq!(expr(&mut input).unwrap(), Expr::BinaryOp {
            left: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier("a".to_string())),
                op: BinOp::Gt,
                right: Box::new(Expr::Number(0.0)),
            }),
            op: BinOp::And,
            right: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier("b".to_string())),
                op: BinOp::Lt,
                right: Box::new(Expr::Number(10.0)),
            }),
        });
    }

    #[test]
    fn test_regex_test() {
        let mut input = "r.tag ~? /test/";
        assert_eq!(expr(&mut input).unwrap(), Expr::BinaryOp {
            left: Box::new(Expr::PropertyAccess {
                object: Box::new(Expr::Identifier("r".to_string())),
                key: PropertyKey::Name("tag".to_string()),
            }),
            op: BinOp::RegexTest,
            right: Box::new(Expr::StringLiteral("test".to_string())),
        });
    }

    #[test]
    fn test_regex_extract() {
        let mut input = "r ~ /id: \"(.*?)\"/";
        assert_eq!(expr(&mut input).unwrap(), Expr::BinaryOp {
            left: Box::new(Expr::Identifier("r".to_string())),
            op: BinOp::RegexExtract,
            right: Box::new(Expr::StringLiteral("id: \"(.*?)\"".to_string())),
        });
    }

    #[test]
    fn test_arithmetic_precedence() {
        let mut input = "a + b * 2";
        assert_eq!(expr(&mut input).unwrap(), Expr::BinaryOp {
            left: Box::new(Expr::Identifier("a".to_string())),
            op: BinOp::Add,
            right: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier("b".to_string())),
                op: BinOp::Mul,
                right: Box::new(Expr::Number(2.0)),
            }),
        });
    }

    #[test]
    fn test_parenthesized() {
        let mut input = "(a + b) * 2";
        assert_eq!(expr(&mut input).unwrap(), Expr::BinaryOp {
            left: Box::new(Expr::BinaryOp {
                left: Box::new(Expr::Identifier("a".to_string())),
                op: BinOp::Add,
                right: Box::new(Expr::Identifier("b".to_string())),
            }),
            op: BinOp::Mul,
            right: Box::new(Expr::Number(2.0)),
        });
    }

    // --- JS block tests ---

    #[test]
    fn test_js_block() {
        let mut input = "js:{ r.items.filter(i => i.active) }";
        assert_eq!(expr(&mut input).unwrap(), Expr::JsExpr("r.items.filter(i => i.active)".to_string()));
    }

    #[test]
    fn test_js_block_nested_braces() {
        let mut input = "js:{ if (x) { return 1 } else { return 2 } }";
        assert_eq!(expr(&mut input).unwrap(), Expr::JsExpr("if (x) { return 1 } else { return 2 }".to_string()));
    }

    // --- JSON tests ---

    #[test]
    fn test_json_object_simple() {
        let mut input = "{ name: \"Test\", count: 3 }";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::JsonObject(vec![
            ("name".to_string(), Expr::StringLiteral("Test".to_string())),
            ("count".to_string(), Expr::Number(3.0)),
        ]));
    }

    #[test]
    fn test_json_object_quoted_keys() {
        let mut input = "{ \"content-type\": \"application/json\" }";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::JsonObject(vec![
            ("content-type".to_string(), Expr::StringLiteral("application/json".to_string())),
        ]));
    }

    #[test]
    fn test_json_object_empty() {
        let mut input = "{}";
        assert_eq!(expr(&mut input).unwrap(), Expr::JsonObject(vec![]));
    }

    #[test]
    fn test_json_object_shorthand_single() {
        let mut input = "{ name }";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::JsonObject(vec![
            ("name".to_string(), Expr::Identifier("name".to_string())),
        ]));
    }

    #[test]
    fn test_json_object_shorthand_multiple() {
        // README's exact pattern: `req.body = { name, type };`
        let mut input = "{ name, type }";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::JsonObject(vec![
            ("name".to_string(), Expr::Identifier("name".to_string())),
            ("type".to_string(), Expr::Identifier("type".to_string())),
        ]));
    }

    #[test]
    fn test_json_object_shorthand_mixed() {
        let mut input = "{ x, y: 2 }";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::JsonObject(vec![
            ("x".to_string(), Expr::Identifier("x".to_string())),
            ("y".to_string(), Expr::Number(2.0)),
        ]));
    }

    #[test]
    fn test_json_object_quoted_key_no_value_errors() {
        // `{ "foo" }` is not valid shorthand — quoted keys need explicit values.
        let mut input = "{ \"foo\" }";
        assert!(expr(&mut input).is_err());
    }

    #[test]
    fn test_json_object_nested() {
        let mut input = "{ user: { name: \"Test\" } }";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::JsonObject(vec![
            ("user".to_string(), Expr::JsonObject(vec![
                ("name".to_string(), Expr::StringLiteral("Test".to_string())),
            ])),
        ]));
    }

    #[test]
    fn test_json_object_multiline() {
        let mut input = "{\n  name: \"Test\",\n  count: 3\n}";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::JsonObject(vec![
            ("name".to_string(), Expr::StringLiteral("Test".to_string())),
            ("count".to_string(), Expr::Number(3.0)),
        ]));
    }

    #[test]
    fn test_json_array() {
        let mut input = "[1, 2, 3]";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::JsonArray(vec![
            Expr::Number(1.0),
            Expr::Number(2.0),
            Expr::Number(3.0),
        ]));
    }

    #[test]
    fn test_json_array_empty() {
        let mut input = "[]";
        assert_eq!(expr(&mut input).unwrap(), Expr::JsonArray(vec![]));
    }

    #[test]
    fn test_json_array_mixed() {
        let mut input = "[\"hello\", 42, null, true]";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::JsonArray(vec![
            Expr::StringLiteral("hello".to_string()),
            Expr::Number(42.0),
            Expr::Null,
            Expr::Bool(true),
        ]));
    }

    // --- String escape tests ---

    #[test]
    fn test_string_escaped_quotes() {
        let mut input = "\"hello \\\"world\\\"\"";
        assert_eq!(expr(&mut input).unwrap(), Expr::StringLiteral("hello \"world\"".to_string()));
    }

    #[test]
    fn test_string_escaped_newline() {
        let mut input = "\"line1\\nline2\"";
        assert_eq!(expr(&mut input).unwrap(), Expr::StringLiteral("line1\nline2".to_string()));
    }

    // --- tstr block tests ---

    #[test]
    fn test_tstr_block_simple() {
        let mut input = "{ item --> <-- item; }";
        let result = expr(&mut input).unwrap();
        assert_eq!(result, Expr::Block {
            inputs: vec!["item".to_string()],
            body: Vec::new(),
            outputs: vec!["item".to_string()],
        });
    }

    #[test]
    fn test_tstr_block_with_body() {
        let mut input = "{ item --> item.active == true | \"inactive\"; <-- item; }";
        let result = expr(&mut input).unwrap();
        match result {
            Expr::Block { inputs, body, outputs } => {
                assert_eq!(inputs, vec!["item".to_string()]);
                assert_eq!(body.len(), 1);
                assert_eq!(outputs, vec!["item".to_string()]);
            }
            _ => panic!("expected Block, got {:?}", result),
        }
    }

    #[test]
    fn test_tstr_block_no_inputs() {
        let mut input = "{ --> x = 1; <-- x; }";
        let result = expr(&mut input).unwrap();
        match result {
            Expr::Block { inputs, body, outputs } => {
                assert_eq!(inputs, Vec::<String>::new());
                assert_eq!(body.len(), 1);
                assert_eq!(outputs, vec!["x".to_string()]);
            }
            _ => panic!("expected Block, got {:?}", result),
        }
    }

    #[test]
    fn test_tstr_block_no_outputs() {
        let mut input = "{ item --> item.id != null | \"missing\"; }";
        let result = expr(&mut input).unwrap();
        match result {
            Expr::Block { inputs, body, outputs } => {
                assert_eq!(inputs, vec!["item".to_string()]);
                assert_eq!(body.len(), 1);
                assert_eq!(outputs, Vec::<String>::new());
            }
            _ => panic!("expected Block, got {:?}", result),
        }
    }

    #[test]
    fn test_tstr_block_multiple_inputs() {
        let mut input = "{ item, idx --> <-- item; }";
        let result = expr(&mut input).unwrap();
        match result {
            Expr::Block { inputs, .. } => {
                assert_eq!(inputs, vec!["item".to_string(), "idx".to_string()]);
            }
            _ => panic!("expected Block, got {:?}", result),
        }
    }
}
