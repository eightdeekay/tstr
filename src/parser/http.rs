use winnow::prelude::*;
use winnow::error::ModalResult;
use winnow::combinator::{alt, separated};
use winnow::token::{take_while};
use winnow::ascii::digit1;

use crate::ast::*;
use super::primitives::{quoted_string, ws};

/// Parse an HTTP method keyword.
pub fn http_method(input: &mut &str) -> ModalResult<HttpMethod> {
    alt((
        "get".map(|_| HttpMethod::Get),
        "post".map(|_| HttpMethod::Post),
        "put".map(|_| HttpMethod::Put),
        "patch".map(|_| HttpMethod::Patch),
        "delete".map(|_| HttpMethod::Delete),
        "head".map(|_| HttpMethod::Head),
        "options".map(|_| HttpMethod::Options),
    )).parse_next(input)
}

/// Map an identifier name to its HTTP verb, if any. Used by the statement
/// parser to recognize UFCS-form HTTP calls (`req.post(url)`) after the
/// expression has been parsed as a generic `MethodCall`.
pub fn http_verb_from_str(s: &str) -> Option<HttpMethod> {
    match s {
        "get" => Some(HttpMethod::Get),
        "post" => Some(HttpMethod::Post),
        "put" => Some(HttpMethod::Put),
        "patch" => Some(HttpMethod::Patch),
        "delete" => Some(HttpMethod::Delete),
        "head" => Some(HttpMethod::Head),
        "options" => Some(HttpMethod::Options),
        _ => None,
    }
}

/// True if `name` is reserved at identifier binding sites (variable
/// assignment targets, block input/output declarations). HTTP verbs are
/// reserved so a user can't shadow `get`/`post`/etc. with a local
/// variable and inadvertently disable the HTTP-call detection on that
/// name. Member-access uses (`obj.get`, `obj.get(url)`) are unaffected —
/// they consume a property name, not a binding.
pub fn is_reserved_binding(name: &str) -> bool {
    http_verb_from_str(name).is_some()
}

/// Parse a single status pattern: `200`, `2xx`, `200-204`, `>=200`, `<500`
pub fn status_pattern(input: &mut &str) -> ModalResult<StatusPattern> {
    alt((
        // Comparison: >=200, <=500, >200, <500
        (alt((">=", "<=", ">", "<")), digit1).map(|(op, n): (&str, &str)| {
            let code = n.parse::<u16>().unwrap();
            let comp = match op {
                ">=" => CompOp::Gte,
                "<=" => CompOp::Lte,
                ">" => CompOp::Gt,
                "<" => CompOp::Lt,
                _ => unreachable!(),
            };
            StatusPattern::Comparison(comp, code)
        }),
        // Wildcard: 2xx, 4xx, etc.
        (take_while(1, |c: char| c.is_ascii_digit()), "xx").map(|(d, _): (&str, &str)| {
            StatusPattern::Wildcard(d.parse::<u8>().unwrap())
        }),
        // Range: 200-204
        (digit1, '-', digit1).map(|(lo, _, hi): (&str, char, &str)| {
            StatusPattern::Range(lo.parse::<u16>().unwrap(), hi.parse::<u16>().unwrap())
        }),
        // Exact: 200
        digit1.map(|n: &str| StatusPattern::Exact(n.parse::<u16>().unwrap())),
    )).parse_next(input)
}

/// Parse the status check portion: `? 2xx 200 201 | "message"`
pub fn status_check(input: &mut &str) -> ModalResult<StatusCheck> {
    '?'.parse_next(input)?;
    ws.parse_next(input)?;
    let patterns: Vec<StatusPattern> = separated(1.., status_pattern, take_while(1.., |c: char| c.is_whitespace())).parse_next(input)?;
    ws.parse_next(input)?;
    '|'.parse_next(input)?;
    ws.parse_next(input)?;
    let message = quoted_string.map(String::from).parse_next(input)?;
    Ok(StatusCheck { patterns, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_method() {
        let mut input = "post";
        assert_eq!(http_method(&mut input), Ok(HttpMethod::Post));
    }

    #[test]
    fn test_status_pattern_exact() {
        let mut input = "200";
        assert_eq!(status_pattern(&mut input), Ok(StatusPattern::Exact(200)));
    }

    #[test]
    fn test_status_pattern_wildcard() {
        let mut input = "2xx";
        assert_eq!(status_pattern(&mut input), Ok(StatusPattern::Wildcard(2)));
    }

    #[test]
    fn test_status_pattern_range() {
        let mut input = "200-204";
        assert_eq!(status_pattern(&mut input), Ok(StatusPattern::Range(200, 204)));
    }

    #[test]
    fn test_status_pattern_comparison() {
        let mut input = ">=400";
        assert_eq!(
            status_pattern(&mut input),
            Ok(StatusPattern::Comparison(CompOp::Gte, 400))
        );
    }

    #[test]
    fn test_status_check() {
        let mut input = "? 2xx | \"Request failed\"";
        let result = status_check(&mut input).unwrap();
        assert_eq!(result.patterns, vec![StatusPattern::Wildcard(2)]);
        assert_eq!(result.message, "Request failed");
    }

    #[test]
    fn test_status_check_multiple() {
        let mut input = "? 200 201 | \"Unexpected status\"";
        let result = status_check(&mut input).unwrap();
        assert_eq!(result.patterns, vec![StatusPattern::Exact(200), StatusPattern::Exact(201)]);
        assert_eq!(result.message, "Unexpected status");
    }
}
