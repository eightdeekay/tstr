use winnow::prelude::*;
use winnow::error::ModalResult;
use winnow::combinator::{alt, opt};
use winnow::token::take_while;

use crate::ast::*;
use super::primitives::{identifier, quoted_string, property_key, ws, brace_matched_content};
use super::expr::expr;
use super::http::{http_method, http_verb_from_str, is_reserved_binding, status_check};

/// Parse `if cond { ...stmts... }` with optional `else if` / `else` tails.
///
/// Braces delimit each branch (consistent with `retry`); the condition is a
/// bare expression with no surrounding parens. `else if` is parsed as an
/// `else_body` containing a single nested `If`. Backtracks if `if` is being
/// used as an ordinary identifier prefix (e.g. `ifCount = 1`) so it only
/// fires as a keyword when followed by whitespace.
///
/// `source` is the full source string being parsed; it's threaded through so
/// branch bodies record absolute file line numbers (see `statements_with_lines`).
fn if_stmt(input: &mut &str, source: &str) -> ModalResult<Statement> {
    "if".parse_next(input)?;
    // Only a keyword when followed by whitespace — otherwise it's an identifier
    // that merely starts with "if" (e.g. `ifEnabled`). Backtrack so the
    // assignment/expression parsers handle it.
    let next = input.chars().next().unwrap_or('=');
    if !next.is_whitespace() {
        return Err(backtrack());
    }
    ws.parse_next(input)?;
    let condition = expr.parse_next(input)?;
    ws.parse_next(input)?;
    '{'.parse_next(input)?;
    let (then_body, then_lines) = statements_with_lines(input, source)?;
    ws.parse_next(input)?;
    '}'.parse_next(input)?;
    ws.parse_next(input)?;

    // Optional `else` / `else if` tail. `else` is a keyword only when followed
    // by whitespace or an opening brace (so an identifier like `elseVar` is safe).
    let is_else = input.starts_with("else")
        && input[4..].chars().next().map_or(false, |c| c.is_whitespace() || c == '{');
    let (else_body, else_lines) = if is_else {
        "else".parse_next(input)?;
        ws.parse_next(input)?;
        if input.starts_with("if")
            && input[2..].chars().next().map_or(false, |c| c.is_whitespace())
        {
            // `else if ...` — represent the chained if as the sole statement
            // of the else branch. Capture its line for the parallel line map.
            let offset = source.len() - input.len();
            let line = source[..offset].chars().filter(|&c| c == '\n').count() + 1;
            let nested = if_stmt(input, source)?;
            (vec![nested], vec![line])
        } else {
            '{'.parse_next(input)?;
            let (body, lines) = statements_with_lines(input, source)?;
            ws.parse_next(input)?;
            '}'.parse_next(input)?;
            (body, lines)
        }
    } else {
        (Vec::new(), Vec::new())
    };

    Ok(Statement::If { condition, then_body, then_lines, else_body, else_lines })
}

/// Parse `disabled "reason";` — an unconditional file-off marker. The reason
/// is mandatory (no anonymous disables). Backtracks if `disabled` is being
/// used as an ordinary identifier (`disabledFlag = ...`, `disabled = x`, etc.)
/// so it stays a usable variable name everywhere except this statement form.
fn disabled_stmt(input: &mut &str) -> ModalResult<Statement> {
    "disabled".parse_next(input)?;
    // Must be followed by whitespace, else this is an identifier that merely
    // starts with "disabled" (e.g. `disabledCount`). Backtrack to let the
    // identifier/assignment parsers handle it.
    let next = input.chars().next().unwrap_or('=');
    if !next.is_whitespace() {
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::new(),
        ));
    }
    ws.parse_next(input)?;
    // A quoted reason string is required. If it's not here (e.g. `disabled = x`),
    // quoted_string fails -> Backtrack -> alt rewinds to ident_statement.
    let reason = quoted_string.map(String::from).parse_next(input)?;
    ws.parse_next(input)?;
    ';'.parse_next(input)?;
    Ok(Statement::Disabled { reason })
}

/// Parse the optional `as <name>` alias that follows an export item. `as` is a
/// keyword only here, and only when followed by whitespace — `asThing` stays a
/// plain identifier.
fn as_alias(input: &mut &str) -> ModalResult<String> {
    "as".parse_next(input)?;
    if !input.chars().next().map_or(false, |c| c.is_whitespace()) {
        return Err(backtrack());
    }
    ws.parse_next(input)?;
    let name = identifier.parse_next(input)?;
    Ok(name.to_string())
}

/// Parse `export a, b, expr as name, ...;` — publish named bindings into the
/// file's exports (ambient broadcast). Non-terminating; may appear repeatedly.
///
/// Each item is `expr [as name]`. A bare identifier self-names (`export foo`
/// publishes `foo`); anything computed needs an alias (`export r.id as id`) — a
/// non-identifier without `as` is an error. The list desugars to an object
/// literal `{ name: expr, ... }`, which the evaluator merges into the exports.
/// A lone bare object literal (`export { ... };`) merges as-is (nested shapes).
fn export_stmt(input: &mut &str) -> ModalResult<Statement> {
    "export".parse_next(input)?;
    // Must be followed by whitespace — otherwise this is a bare identifier
    // starting with "export..." (e.g., `exportCount = ...`).
    if !input.chars().next().map_or(false, |c| c.is_whitespace()) {
        return Err(backtrack());
    }
    ws.parse_next(input)?;

    // Parse the comma-separated item list: `expr [as name]`.
    let mut items: Vec<(Option<String>, Expr)> = Vec::new();
    loop {
        let e = expr.parse_next(input)?;
        ws.parse_next(input)?;
        let alias = opt(as_alias).parse_next(input)?;
        items.push((alias, e));
        ws.parse_next(input)?;
        if input.starts_with(',') {
            ','.parse_next(input)?;
            ws.parse_next(input)?;
        } else {
            break;
        }
    }
    ';'.parse_next(input)?;

    // A lone bare object literal exports its fields as-is (nested shapes).
    if items.len() == 1 && items[0].0.is_none() && matches!(items[0].1, Expr::JsonObject(_)) {
        let (_, value) = items.pop().unwrap();
        return Ok(Statement::Export { value });
    }

    // Otherwise build an object from the named items.
    let mut pairs: Vec<(String, Expr)> = Vec::with_capacity(items.len());
    for (alias, e) in items {
        let name = match alias {
            Some(n) => n,
            None => match &e {
                Expr::Identifier(n) => n.clone(),
                _ => return Err(backtrack()),
            },
        };
        pairs.push((name, e));
    }
    Ok(Statement::Export { value: Expr::JsonObject(pairs) })
}

/// Parse `return expr;` or bare `return;`. A single value that terminates
/// execution — the lib call's result, a lambda block's yield, or an early
/// exit. (Publishing to ambient scope is `export`, not `return`.)
fn return_stmt(input: &mut &str) -> ModalResult<Statement> {
    "return".parse_next(input)?;
    // Must be followed by whitespace or ';' — otherwise this is a bare
    // identifier starting with "return..." (e.g., `returnValue = ...`).
    let next = input.chars().next().unwrap_or(';');
    if next != ';' && !next.is_whitespace() {
        return Err(backtrack());
    }
    ws.parse_next(input)?;
    if input.starts_with(';') {
        ';'.parse_next(input)?;
        return Ok(Statement::Return { value: None });
    }
    let value = expr.parse_next(input)?;
    ws.parse_next(input)?;
    ';'.parse_next(input)?;
    Ok(Statement::Return { value: Some(value) })
}

/// Shorthand for a backtracking parse error (lets `alt` try the next branch).
fn backtrack() -> winnow::error::ErrMode<winnow::error::ContextError> {
    winnow::error::ErrMode::Backtrack(winnow::error::ContextError::new())
}

/// Convert a duration count + unit to milliseconds. `ms`/`s`/`m` only.
fn duration_to_ms(n: u64, unit: &str) -> u64 {
    match unit {
        "ms" => n,
        "s" => n * 1_000,
        "m" => n * 60_000,
        _ => n,
    }
}

/// Parse a retry argument value: an unsigned integer with an optional duration
/// unit suffix. Returns `(number, Some(unit))` when a unit was present, else
/// `(number, None)`. The caller interprets units per key — `max` wants a bare
/// count, `interval`/`timeout` want a duration.
fn retry_arg_value<'a>(input: &mut &'a str) -> ModalResult<(u64, Option<&'a str>)> {
    let digits = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(input)?;
    let n: u64 = digits.parse().map_err(|_| backtrack())?;
    // Longest unit first so "ms" wins over "m".
    let unit = opt(alt(("ms", "s", "m"))).parse_next(input)?;
    Ok((n, unit))
}

/// Parse `retry(max: 10, interval: 500ms, timeout: 30s) { ...statements... }`.
///
/// Re-runs the body until every assertion inside passes or a bound is hit.
/// Backtracks if `retry` is an ordinary identifier (e.g. `retryCount = 0`) —
/// the keyword only fires when immediately followed by `(`. Commas between
/// arguments are optional. `max` takes a bare count; `interval`/`timeout` take
/// durations (`ms`/`s`/`m`, unit required). At least one of `max`/`timeout`
/// must be present, else the loop would be unbounded.
fn retry_stmt(input: &mut &str, source: &str) -> ModalResult<Statement> {
    "retry".parse_next(input)?;
    // Only a keyword when the very next char is '(' — otherwise it's an
    // identifier that merely starts with "retry". Backtrack so the
    // assignment/expression parsers handle it.
    if !input.starts_with('(') {
        return Err(backtrack());
    }
    '('.parse_next(input)?;

    let mut max: Option<u32> = None;
    let mut interval_ms: u64 = 250;
    let mut timeout_ms: Option<u64> = None;

    loop {
        ws.parse_next(input)?;
        if input.starts_with(')') {
            break;
        }
        let key = identifier.parse_next(input)?;
        ws.parse_next(input)?;
        ':'.parse_next(input)?;
        ws.parse_next(input)?;
        let (n, unit) = retry_arg_value(input)?;
        match key {
            // bare count, no unit allowed
            "max" => {
                if unit.is_some() {
                    return Err(backtrack());
                }
                max = Some(n as u32);
            }
            // duration, unit required
            "interval" => {
                let u = unit.ok_or_else(backtrack)?;
                interval_ms = duration_to_ms(n, u);
            }
            "timeout" => {
                let u = unit.ok_or_else(backtrack)?;
                timeout_ms = Some(duration_to_ms(n, u));
            }
            _ => return Err(backtrack()),
        }
        ws.parse_next(input)?;
        opt(',').parse_next(input)?;
    }
    ')'.parse_next(input)?;
    ws.parse_next(input)?;
    '{'.parse_next(input)?;
    let (body, body_lines) = statements_with_lines(input, source)?;
    ws.parse_next(input)?;
    '}'.parse_next(input)?;

    // An unbounded retry is a footgun — require an attempt cap or a timeout.
    if max.is_none() && timeout_ms.is_none() {
        return Err(backtrack());
    }

    Ok(Statement::Retry { max, interval_ms, timeout_ms, body, body_lines })
}

/// Parse `matrix name = [ "Label": { ... }, "Label": { ... } ];`
fn matrix_stmt(input: &mut &str) -> ModalResult<Statement> {
    "matrix".parse_next(input)?;
    // Must be followed by whitespace + identifier, not `=` (that would be an assignment to a var named "matrix")
    let next = input.chars().next().unwrap_or('=');
    if !next.is_whitespace() {
        return Err(winnow::error::ErrMode::Backtrack(winnow::error::ContextError::new()));
    }
    ws.parse_next(input)?;
    let name = identifier.map(String::from).parse_next(input)?;
    ws.parse_next(input)?;
    '='.parse_next(input)?;
    ws.parse_next(input)?;
    '['.parse_next(input)?;
    ws.parse_next(input)?;

    let mut entries = Vec::new();
    while !input.starts_with(']') {
        let label = quoted_string.map(String::from).parse_next(input)?;
        ws.parse_next(input)?;
        ':'.parse_next(input)?;
        ws.parse_next(input)?;
        let value = expr.parse_next(input)?;
        entries.push(MatrixEntry { label, value });
        ws.parse_next(input)?;
        if input.starts_with(',') {
            ','.parse_next(input)?;
            ws.parse_next(input)?;
        }
    }
    ']'.parse_next(input)?;
    ws.parse_next(input)?;
    ';'.parse_next(input)?;

    Ok(Statement::Matrix { name, entries })
}

/// Parse a standalone `js:{ ... };`
fn js_block_stmt(input: &mut &str) -> ModalResult<Statement> {
    "js:".parse_next(input)?;
    let code = brace_matched_content.parse_next(input)?;
    ws.parse_next(input)?;
    ';'.parse_next(input)?;
    Ok(Statement::JsBlock { code })
}

/// Parse the target of an assignment: `var` or `var.field.field`. HTTP
/// verb names (`get`/`post`/etc.) are rejected here as binding targets.
fn assign_target(input: &mut &str) -> ModalResult<AssignTarget> {
    let name = identifier.map(String::from).parse_next(input)?;
    if is_reserved_binding(&name) {
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::new(),
        ));
    }

    // Check for field path: .field, ."quoted", or ["quoted"]
    let mut path: Vec<PropertyKey> = Vec::new();
    loop {
        if input.starts_with('.') && !input.starts_with("..") {
            *input = &input[1..];
            let key = property_key.parse_next(input)?;
            path.push(key);
        } else if input.starts_with("[\"") || input.starts_with("['") {
            '['.parse_next(input)?;
            let key = quoted_string.map(String::from).parse_next(input)?;
            ']'.parse_next(input)?;
            path.push(PropertyKey::Quoted(key));
        } else {
            break;
        }
    }

    if path.is_empty() {
        Ok(AssignTarget::Variable(name))
    } else {
        Ok(AssignTarget::FieldAccess { object: name, path })
    }
}

/// Parse a statement that starts with an identifier (assignment, http call, or assertion).
/// Handles the ambiguity between `var = get(...)`, `var = expr`, `var.field = expr`,
/// and `expr | "message"` by parsing the common prefix first, then branching.
fn ident_statement(input: &mut &str) -> ModalResult<Statement> {
    // Save position for assertions (which start with an expression)
    let saved = *input;

    // Try to parse as assignment target: `var` or `var.field.field`
    let target = assign_target.parse_next(input)?;
    ws.parse_next(input)?;

    // If next is `=`, it's an assignment or HTTP call
    if input.starts_with('=') && !input.starts_with("==") {
        '='.parse_next(input)?;
        ws.parse_next(input)?;

        // Peek: is this an HTTP method followed by `(`?
        let is_http = {
            let peek = *input;
            matches!(peek.strip_prefix("get").or_else(|| peek.strip_prefix("post"))
                .or_else(|| peek.strip_prefix("patch"))
                .or_else(|| peek.strip_prefix("delete"))
                .or_else(|| peek.strip_prefix("put")),
                Some(rest) if rest.starts_with('(') || rest.starts_with(char::is_whitespace))
        };

        if is_http {
            // Function-form HTTP call: var = method(req, "url") ? status | "msg";
            // Both args required — req is no longer optional, and the order is
            // (req, url) so it round-trips with the UFCS form `req.method(url)`.
            let target_name = match target {
                AssignTarget::Variable(name) => name,
                _ => {
                    return Err(winnow::error::ErrMode::Backtrack(
                        winnow::error::ContextError::new(),
                    ));
                }
            };
            let method = http_method.parse_next(input)?;
            ws.parse_next(input)?;
            '('.parse_next(input)?;
            ws.parse_next(input)?;
            let request_obj = expr.parse_next(input)?;
            ws.parse_next(input)?;
            ','.parse_next(input)?;
            ws.parse_next(input)?;
            let url = expr.parse_next(input)?;
            ws.parse_next(input)?;
            ')'.parse_next(input)?;
            ws.parse_next(input)?;
            let sc = opt(status_check).parse_next(input)?;
            ws.parse_next(input)?;
            ';'.parse_next(input)?;

            Ok(Statement::HttpCall {
                target: target_name,
                method,
                url,
                request_obj,
                status_check: sc,
            })
        } else {
            // Regular assignment OR UFCS-form HTTP call.
            //
            //   var = expr | "guard";              -> Assignment with Guard
            //   var = expr;                        -> Assignment
            //   var = req.post(url) ? 2xx | "msg"; -> HttpCall (UFCS)
            //   var = req.post(url);               -> HttpCall (UFCS, no status check)
            //
            // We parse the RHS as a generic expression first; if it turns out
            // to be a method call whose method name is an HTTP verb, we
            // re-route to the HttpCall branch (and accept an optional `?...`
            // status check tail). Otherwise it's a plain assignment, with the
            // optional `| "guard"` tail handled inline.
            let value = expr.parse_next(input)?;

            if let Expr::MethodCall { object, method, args } = &value {
                if let Some(http_method) = http_verb_from_str(method) {
                    let target_name = match &target {
                        AssignTarget::Variable(name) => name.clone(),
                        _ => {
                            return Err(winnow::error::ErrMode::Backtrack(
                                winnow::error::ContextError::new(),
                            ));
                        }
                    };
                    if args.len() != 1 {
                        return Err(winnow::error::ErrMode::Backtrack(
                            winnow::error::ContextError::new(),
                        ));
                    }
                    let url = args[0].clone();
                    let request_obj = (**object).clone();

                    ws.parse_next(input)?;
                    let sc = opt(status_check).parse_next(input)?;
                    ws.parse_next(input)?;
                    ';'.parse_next(input)?;

                    return Ok(Statement::HttpCall {
                        target: target_name,
                        method: http_method,
                        url,
                        request_obj,
                        status_check: sc,
                    });
                }
            }

            // Plain assignment — handle optional `| "guard"` tail.
            ws.parse_next(input)?;
            let value = if input.starts_with('|') && !input.starts_with("||") {
                '|'.parse_next(input)?;
                ws.parse_next(input)?;
                let message = quoted_string.map(String::from).parse_next(input)?;
                Expr::Guard {
                    expr: Box::new(value),
                    message,
                }
            } else {
                value
            };
            ws.parse_next(input)?;
            ';'.parse_next(input)?;
            Ok(Statement::Assignment { target, value })
        }
    } else {
        // Not an assignment — backtrack and parse as expression
        *input = saved;
        let e = expr.parse_next(input)?;
        ws.parse_next(input)?;
        // If followed by `| "message"`, it's an assertion
        if input.starts_with('|') && !input.starts_with("||") {
            '|'.parse_next(input)?;
            ws.parse_next(input)?;
            let message = quoted_string.map(String::from).parse_next(input)?;
            ws.parse_next(input)?;
            ';'.parse_next(input)?;
            Ok(Statement::Assertion { expr: e, message })
        } else {
            // Standalone expression statement: items.each({...});
            ';'.parse_next(input)?;
            Ok(Statement::ExprStatement { expr: e })
        }
    }
}

/// Parse a bare expression: assertion (`expr | "message";`) or standalone (`expr;`).
/// For expressions that don't start with an identifier (e.g., `!disabled`, `$.log(r)`).
fn bare_expression(input: &mut &str) -> ModalResult<Statement> {
    let e = expr.parse_next(input)?;
    ws.parse_next(input)?;
    if input.starts_with('|') && !input.starts_with("||") {
        '|'.parse_next(input)?;
        ws.parse_next(input)?;
        let message = quoted_string.map(String::from).parse_next(input)?;
        ws.parse_next(input)?;
        ';'.parse_next(input)?;
        Ok(Statement::Assertion { expr: e, message })
    } else {
        ';'.parse_next(input)?;
        Ok(Statement::ExprStatement { expr: e })
    }
}

/// Parse a single statement. `source` is the full source being parsed,
/// threaded so body-bearing statements (`if`, `retry`) can record absolute
/// line numbers for their nested branches.
fn statement_inner(input: &mut &str, source: &str) -> ModalResult<Statement> {
    ws.parse_next(input)?;
    alt((
        // `if`/`retry` need `source`; wrap them as closures so the rest of the
        // branches keep their plain `fn(&mut &str)` signature.
        |i: &mut &str| if_stmt(i, source),
        |i: &mut &str| retry_stmt(i, source),
        disabled_stmt,
        export_stmt,
        return_stmt,
        js_block_stmt,
        matrix_stmt,
        ident_statement,
        bare_expression,
    )).parse_next(input)
}

/// Parse a single statement. Public entry (used by tests); captures the
/// current input as the source base for line tracking.
pub fn statement(input: &mut &str) -> ModalResult<Statement> {
    let source = *input;
    statement_inner(input, source)
}

/// Parse a sequence of statements (the body of a file or block), discarding
/// line info. Used where line numbers aren't surfaced (e.g. `.map`/`.each`
/// block bodies, which convert failures to runtime errors).
pub fn statements(input: &mut &str) -> ModalResult<Vec<Statement>> {
    let source = *input;
    let (stmts, _lines) = statements_with_lines(input, source)?;
    Ok(stmts)
}

/// Parse a sequence of statements, also returning a line map.
/// `source_len` is the total length of the source string being parsed,
/// used to compute byte offsets from the remaining input.
/// Parse a sequence of statements with line tracking.
/// On failure, returns Err with the remaining input positioned at the
/// start of the failing statement (so the caller can format a useful error).
pub fn statements_with_lines(input: &mut &str, source: &str) -> ModalResult<(Vec<Statement>, Vec<usize>)> {
    let mut stmts = Vec::new();
    let mut line_map = Vec::new();
    let source_len = source.len();
    loop {
        ws.parse_next(input)?;
        if input.is_empty() || input.starts_with("<--") || input.starts_with('}') {
            break;
        }
        // Record byte offset before parsing statement
        let offset = source_len - input.len();
        let line = source[..offset].chars().filter(|&c| c == '\n').count() + 1;
        // Save position — on failure, leave input here so the caller
        // can point the caret at the start of the problematic line.
        let saved = *input;
        match statement_inner(input, source) {
            Ok(stmt) => {
                line_map.push(line);
                stmts.push(stmt);
            }
            Err(_) => {
                *input = saved;
                return Err(winnow::error::ErrMode::Backtrack(
                    winnow::error::ContextError::new(),
                ));
            }
        }
    }
    Ok((stmts, line_map))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_assignment() {
        let mut input = "groupId = r.id;";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::Assignment {
            target: AssignTarget::Variable("groupId".to_string()),
            value: Expr::PropertyAccess {
                object: Box::new(Expr::Identifier("r".to_string())),
                key: PropertyKey::Name("id".to_string()),
            },
        });
    }

    #[test]
    fn test_field_mutation() {
        let mut input = "req.headers.\"content-type\" = \"application/json\";";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::Assignment {
            target: AssignTarget::FieldAccess {
                object: "req".to_string(),
                path: vec![
                    PropertyKey::Name("headers".to_string()),
                    PropertyKey::Quoted("content-type".to_string()),
                ],
            },
            value: Expr::StringLiteral("application/json".to_string()),
        });
    }

    #[test]
    fn test_bracket_field_mutation() {
        let mut input = "req.headers[\"account-id\"] = $.uuid();";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::Assignment {
            target: AssignTarget::FieldAccess {
                object: "req".to_string(),
                path: vec![
                    PropertyKey::Name("headers".to_string()),
                    PropertyKey::Quoted("account-id".to_string()),
                ],
            },
            value: Expr::BuiltinCall {
                name: "uuid".to_string(),
                args: Vec::new(),
            },
        });
    }

    #[test]
    fn test_assertion() {
        let mut input = "r.id != null | \"missing id\";";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::Assertion {
            expr: Expr::BinaryOp {
                left: Box::new(Expr::PropertyAccess {
                    object: Box::new(Expr::Identifier("r".to_string())),
                    key: PropertyKey::Name("id".to_string()),
                }),
                op: BinOp::NotEq,
                right: Box::new(Expr::Null),
            },
            message: "missing id".to_string(),
        });
    }

    #[test]
    fn test_if_stmt() {
        let mut input = "if existing != null { junk = req.delete(\"/x\"); }";
        let result = statement(&mut input).unwrap();
        match result {
            Statement::If { condition, then_body, else_body, .. } => {
                assert_eq!(condition, Expr::BinaryOp {
                    left: Box::new(Expr::Identifier("existing".to_string())),
                    op: BinOp::NotEq,
                    right: Box::new(Expr::Null),
                });
                assert_eq!(then_body.len(), 1);
                assert!(else_body.is_empty());
            }
            other => panic!("expected If, got {:?}", other),
        }
    }

    #[test]
    fn test_if_else_stmt() {
        let mut input = "if x > 0 { a = 1; } else { a = 2; }";
        let result = statement(&mut input).unwrap();
        match result {
            Statement::If { then_body, else_body, .. } => {
                assert_eq!(then_body.len(), 1);
                assert_eq!(else_body.len(), 1);
            }
            other => panic!("expected If, got {:?}", other),
        }
    }

    #[test]
    fn test_if_else_if_chain() {
        // `else if` becomes an else_body holding a single nested If.
        let mut input = "if x == 1 { a = 1; } else if x == 2 { a = 2; } else { a = 3; }";
        let result = statement(&mut input).unwrap();
        match result {
            Statement::If { else_body, .. } => {
                assert_eq!(else_body.len(), 1);
                match &else_body[0] {
                    Statement::If { then_body, else_body: inner_else, .. } => {
                        assert_eq!(then_body.len(), 1);
                        assert_eq!(inner_else.len(), 1);
                    }
                    other => panic!("expected nested If, got {:?}", other),
                }
            }
            other => panic!("expected If, got {:?}", other),
        }
    }

    #[test]
    fn test_if_as_identifier_still_works() {
        // `if` not followed by whitespace is an ordinary identifier prefix.
        let mut input = "ifEnabled = true;";
        let result = statement(&mut input).unwrap();
        assert!(matches!(result, Statement::Assignment { .. }));
    }

    #[test]
    fn test_if_records_inner_line() {
        // A two-line if: the body statement should map to line 2.
        let mut input = "if x {\n  a = 1;\n}";
        let result = statement(&mut input).unwrap();
        match result {
            Statement::If { then_lines, .. } => {
                assert_eq!(then_lines, vec![2]);
            }
            other => panic!("expected If, got {:?}", other),
        }
    }

    #[test]
    fn test_disabled_stmt() {
        let mut input = r#"disabled "I-123: auth refactor pending";"#;
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::Disabled {
            reason: "I-123: auth refactor pending".to_string(),
        });
    }

    #[test]
    fn test_disabled_as_identifier_still_works() {
        // `disabled` without a quoted reason is just an ordinary identifier:
        // assignment target, field access, etc. must still parse.
        let mut input = "disabledCount = 5;";
        let result = statement(&mut input).unwrap();
        assert!(matches!(result, Statement::Assignment { .. }));

        let mut input2 = "disabled = false;";
        let result2 = statement(&mut input2).unwrap();
        assert!(matches!(result2, Statement::Assignment { .. }));
    }

    #[test]
    fn test_disabled_requires_reason() {
        // No reason -> not a Disabled statement (falls through; bare `disabled;`
        // becomes an expr statement that eval rejects as an undefined identifier).
        let mut input = "disabled;";
        let result = statement(&mut input).unwrap();
        assert!(!matches!(result, Statement::Disabled { .. }));
    }

    #[test]
    fn test_retry_full_args() {
        let mut input = "retry(max: 10, interval: 500ms, timeout: 30s) { x == 1 | \"nope\"; }";
        let result = statement(&mut input).unwrap();
        match result {
            Statement::Retry { max, interval_ms, timeout_ms, body, .. } => {
                assert_eq!(max, Some(10));
                assert_eq!(interval_ms, 500);
                assert_eq!(timeout_ms, Some(30_000));
                assert_eq!(body.len(), 1);
                assert!(matches!(body[0], Statement::Assertion { .. }));
            }
            other => panic!("expected Retry, got {:?}", other),
        }
    }

    #[test]
    fn test_retry_interval_defaults_to_250ms() {
        // Only `max` given — interval defaults, timeout stays None.
        let mut input = "retry(max: 3) { x == 1 | \"nope\"; }";
        let result = statement(&mut input).unwrap();
        match result {
            Statement::Retry { max, interval_ms, timeout_ms, .. } => {
                assert_eq!(max, Some(3));
                assert_eq!(interval_ms, 250);
                assert_eq!(timeout_ms, None);
            }
            other => panic!("expected Retry, got {:?}", other),
        }
    }

    #[test]
    fn test_retry_timeout_only_is_allowed() {
        // No `max`, but a timeout bound — valid (time-bounded loop).
        let mut input = "retry(timeout: 2m) { x == 1 | \"nope\"; }";
        let result = statement(&mut input).unwrap();
        match result {
            Statement::Retry { max, timeout_ms, .. } => {
                assert_eq!(max, None);
                assert_eq!(timeout_ms, Some(120_000));
            }
            other => panic!("expected Retry, got {:?}", other),
        }
    }

    #[test]
    fn test_retry_unbounded_rejected() {
        // Neither max nor timeout -> not a valid Retry. retry_stmt backtracks,
        // and `retry( ... )` isn't a valid identifier statement either, so the
        // whole statement parse fails.
        let mut input = "retry(interval: 500ms) { x == 1 | \"nope\"; }";
        assert!(statement(&mut input).is_err());
    }

    #[test]
    fn test_retry_as_identifier_still_works() {
        // `retry` not followed by '(' is an ordinary identifier.
        let mut input = "retryCount = 0;";
        let result = statement(&mut input).unwrap();
        assert!(matches!(result, Statement::Assignment { .. }));

        let mut input2 = "retry = 5;";
        let result2 = statement(&mut input2).unwrap();
        assert!(matches!(result2, Statement::Assignment { .. }));
    }

    #[test]
    fn test_retry_max_rejects_unit() {
        // `max` is a bare count; a duration unit on it is a malformed retry.
        let mut input = "retry(max: 10s) { x == 1 | \"nope\"; }";
        assert!(statement(&mut input).is_err());
    }

    #[test]
    fn test_http_call_function_form() {
        // Function form: method(req, url). req-first, both args required.
        let mut input = "r = get(req, \"/v4/groups\") ? 2xx | \"Failed\";";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::HttpCall {
            target: "r".to_string(),
            method: HttpMethod::Get,
            url: Expr::StringLiteral("/v4/groups".to_string()),
            request_obj: Expr::Identifier("req".to_string()),
            status_check: Some(StatusCheck {
                patterns: vec![StatusPattern::Wildcard(2)],
                message: "Failed".to_string(),
            }),
        });
    }

    #[test]
    fn test_http_call_function_form_multiple_status() {
        let mut input = "r = post(req, \"/v4/groups\") ? 200 201 | \"Bad status\";";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::HttpCall {
            target: "r".to_string(),
            method: HttpMethod::Post,
            url: Expr::StringLiteral("/v4/groups".to_string()),
            request_obj: Expr::Identifier("req".to_string()),
            status_check: Some(StatusCheck {
                patterns: vec![StatusPattern::Exact(200), StatusPattern::Exact(201)],
                message: "Bad status".to_string(),
            }),
        });
    }

    #[test]
    fn test_http_call_no_status_check() {
        let mut input = "r = get(req, \"/v4/groups\");";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::HttpCall {
            target: "r".to_string(),
            method: HttpMethod::Get,
            url: Expr::StringLiteral("/v4/groups".to_string()),
            request_obj: Expr::Identifier("req".to_string()),
            status_check: None,
        });
    }

    #[test]
    fn test_http_call_function_form_requires_req() {
        // Bare verb call with no request object is now a parse error.
        let mut input = "r = get(\"/v4/groups\");";
        assert!(statement(&mut input).is_err());
    }

    // --- UFCS HTTP-call form: `var = req.method(url) ? ... | ...;` ---

    #[test]
    fn test_http_call_ufcs() {
        let mut input = "r = req.get(\"/v4/groups\") ? 2xx | \"Failed\";";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::HttpCall {
            target: "r".to_string(),
            method: HttpMethod::Get,
            url: Expr::StringLiteral("/v4/groups".to_string()),
            request_obj: Expr::Identifier("req".to_string()),
            status_check: Some(StatusCheck {
                patterns: vec![StatusPattern::Wildcard(2)],
                message: "Failed".to_string(),
            }),
        });
    }

    #[test]
    fn test_http_call_ufcs_no_status_check() {
        let mut input = "r = req.post(\"/v4/groups\");";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::HttpCall {
            target: "r".to_string(),
            method: HttpMethod::Post,
            url: Expr::StringLiteral("/v4/groups".to_string()),
            request_obj: Expr::Identifier("req".to_string()),
            status_check: None,
        });
    }

    #[test]
    fn test_http_call_ufcs_subject_from_field() {
        // Subject can be any expression — `_in.req.get(url)` should parse.
        let mut input = "r = _in.req.get(\"/v4/groups\");";
        let result = statement(&mut input).unwrap();
        if let Statement::HttpCall { method, request_obj, .. } = result {
            assert_eq!(method, HttpMethod::Get);
            assert!(matches!(request_obj, Expr::PropertyAccess { .. }));
        } else {
            panic!("expected HttpCall, got {:?}", result);
        }
    }

    #[test]
    fn test_reserved_verb_as_assign_target() {
        // HTTP verbs are reserved as variable names.
        let mut input = "get = 5;";
        assert!(statement(&mut input).is_err());
    }

    #[test]
    fn test_method_call_non_http_is_assignment() {
        // A method call whose method isn't an HTTP verb stays a regular
        // assignment (the eval will route it as a normal MethodCall).
        let mut input = "x = arr.someFunc();";
        let result = statement(&mut input).unwrap();
        assert!(matches!(result, Statement::Assignment { .. }));
    }

    #[test]
    fn test_js_block_stmt() {
        let mut input = "js:{ console.log(r) };";
        let result = statement(&mut input).unwrap();
        assert_eq!(result, Statement::JsBlock {
            code: "console.log(r)".to_string(),
        });
    }

    #[test]
    fn test_multiple_statements() {
        let mut input = "r = req.get(\"/v4/groups\") ? 2xx | \"Failed\"; groupId = r.id; r.name != null | \"no name\";";
        let result = statements(&mut input).unwrap();
        assert_eq!(result.len(), 3);
        assert!(matches!(result[0], Statement::HttpCall { .. }));
        assert!(matches!(result[1], Statement::Assignment { .. }));
        assert!(matches!(result[2], Statement::Assertion { .. }));
    }

    #[test]
    fn test_arithmetic_assignment() {
        let mut input = "total = price * quantity + tax;";
        let result = statement(&mut input).unwrap();
        assert!(matches!(result, Statement::Assignment { .. }));
    }

    #[test]
    fn test_negation_assertion() {
        let mut input = "!disabled | \"should not be disabled\";";
        let result = statement(&mut input).unwrap();
        assert!(matches!(result, Statement::Assertion { .. }));
    }

    #[test]
    fn test_multiline_statements() {
        let mut input = "r = req.get(\"/v4/groups\")\n  ? 2xx | \"Failed\";\n\ngroupId = r.id;\n";
        let result = statements(&mut input).unwrap();
        assert_eq!(result.len(), 2);
        assert!(matches!(result[0], Statement::HttpCall { .. }));
        assert!(matches!(result[1], Statement::Assignment { .. }));
    }

    #[test]
    fn test_statements_stop_at_output() {
        let mut input = "groupId = r.id; <-- groupId";
        let result = statements(&mut input).unwrap();
        assert_eq!(result.len(), 1);
        assert!(input.starts_with("<-- groupId"));
    }

    #[test]
    fn test_matrix_stmt() {
        let mut input = r#"matrix sites = [
            "Site A": { siteId: "aaa", siteName: "Site A" },
            "Site B": { siteId: "bbb", siteName: "Site B" }
        ];"#;
        let result = statement(&mut input).unwrap();
        match result {
            Statement::Matrix { name, entries } => {
                assert_eq!(name, "sites");
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].label, "Site A");
                assert_eq!(entries[1].label, "Site B");
            }
            _ => panic!("expected Matrix statement"),
        }
    }

    #[test]
    fn test_matrix_var_named_matrix() {
        // `matrix = "foo"` should parse as a regular assignment, not a matrix statement
        let mut input = r#"matrix = "foo";"#;
        let result = statement(&mut input).unwrap();
        assert!(matches!(result, Statement::Assignment { .. }));
    }
}
