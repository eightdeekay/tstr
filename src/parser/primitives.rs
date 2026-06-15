use winnow::prelude::*;
use winnow::error::ModalResult;
use winnow::combinator::{alt, separated, delimited};
use winnow::token::take_while;

use crate::ast::*;

/// Consume zero or more whitespace characters (spaces, tabs, newlines).
/// Replaces winnow's `ws` since in tstr all whitespace is equivalent.
pub fn ws(input: &mut &str) -> ModalResult<()> {
    take_while(0.., |c: char| c.is_whitespace())
        .map(|_| ())
        .parse_next(input)
}

/// Parse a variable name: letters, digits, underscores. Must start with letter or underscore.
pub fn identifier<'a>(input: &mut &'a str) -> ModalResult<&'a str> {
    take_while(1.., |c: char| c.is_alphanumeric() || c == '_')
        .parse_next(input)
}

/// Parse a quoted string: "content" (no escape handling — for simple contexts)
pub fn quoted_string<'a>(input: &mut &'a str) -> ModalResult<&'a str> {
    delimited('"', take_while(0.., |c: char| c != '"'), '"')
        .parse_next(input)
}

/// Parse a quoted string with escape handling: "hello \"world\""
/// Returns an owned String with escapes resolved.
pub fn escaped_string(input: &mut &str) -> ModalResult<String> {
    '"'.parse_next(input)?;
    let mut result = String::new();
    loop {
        match input.chars().next() {
            Some('"') => {
                *input = &input[1..];
                return Ok(result);
            }
            Some('\\') => {
                *input = &input[1..];
                match input.chars().next() {
                    Some('n') => { result.push('\n'); *input = &input[1..]; }
                    Some('t') => { result.push('\t'); *input = &input[1..]; }
                    Some('\\') => { result.push('\\'); *input = &input[1..]; }
                    Some('"') => { result.push('"'); *input = &input[1..]; }
                    Some(c) => { result.push('\\'); result.push(c); *input = &input[c.len_utf8()..]; }
                    None => {
                        return Err(winnow::error::ErrMode::Backtrack(
                            winnow::error::ContextError::new(),
                        ));
                    }
                }
            }
            Some(c) => {
                result.push(c);
                *input = &input[c.len_utf8()..];
            }
            None => {
                return Err(winnow::error::ErrMode::Backtrack(
                    winnow::error::ContextError::new(),
                ));
            }
        }
    }
}

/// Parse a comma-separated list of identifiers: `var1, var2, var3`.
/// Each name is rejected if it's a reserved binding (HTTP verb).
pub fn var_list(input: &mut &str) -> ModalResult<Vec<String>> {
    let names: Vec<String> = separated(1.., identifier.map(String::from), (ws, ',', ws))
        .parse_next(input)?;
    if names.iter().any(|n| super::http::is_reserved_binding(n)) {
        return Err(winnow::error::ErrMode::Backtrack(
            winnow::error::ContextError::new(),
        ));
    }
    Ok(names)
}

/// Parse a property key: identifier or "quoted-string"
pub fn property_key(input: &mut &str) -> ModalResult<PropertyKey> {
    alt((
        quoted_string.map(|s| PropertyKey::Quoted(s.to_string())),
        identifier.map(|s| PropertyKey::Name(s.to_string())),
    )).parse_next(input)
}

/// Parse brace-matched content: `{ ... }` handling nested braces.
/// Returns the content between the outer braces.
pub fn brace_matched_content(input: &mut &str) -> ModalResult<String> {
    '{'.parse_next(input)?;
    let mut depth = 1;
    let mut content = String::new();
    let mut chars = input.chars();
    let mut consumed = 0;

    while depth > 0 {
        match chars.next() {
            Some('{') => {
                depth += 1;
                content.push('{');
                consumed += 1;
            }
            Some('}') => {
                depth -= 1;
                if depth > 0 {
                    content.push('}');
                }
                consumed += 1;
            }
            Some(c) => {
                content.push(c);
                consumed += c.len_utf8();
            }
            None => {
                return Err(winnow::error::ErrMode::Backtrack(
                    winnow::error::ContextError::new(),
                ));
            }
        }
    }
    *input = &input[consumed..];
    Ok(content.trim().to_string())
}

/// Parse a regex literal: `/pattern/` (handles escaped slashes `\/`)
pub fn regex_literal(input: &mut &str) -> ModalResult<String> {
    '/'.parse_next(input)?;
    let mut pattern = String::new();
    loop {
        match input.chars().next() {
            Some('/') => {
                *input = &input[1..];
                return Ok(pattern);
            }
            Some('\\') => {
                pattern.push('\\');
                *input = &input[1..];
                if let Some(c) = input.chars().next() {
                    pattern.push(c);
                    *input = &input[c.len_utf8()..];
                }
            }
            Some(c) => {
                pattern.push(c);
                *input = &input[c.len_utf8()..];
            }
            None => {
                return Err(winnow::error::ErrMode::Backtrack(
                    winnow::error::ContextError::new(),
                ));
            }
        }
    }
}

/// Strip comments from source code before parsing.
/// Handles `// line comments` and `/* block comments */`.
/// Preserves string literals (comments inside quotes are not stripped).
/// Replaces comment content with spaces (preserving newlines) so line numbers stay valid.
pub fn strip_comments(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();

    while let Some(&c) = chars.peek() {
        match c {
            // String literal — pass through verbatim
            '"' => {
                result.push(chars.next().unwrap());
                while let Some(&sc) = chars.peek() {
                    result.push(chars.next().unwrap());
                    if sc == '"' {
                        break;
                    }
                    // Skip escaped characters inside strings
                    if sc == '\\' {
                        if let Some(&_) = chars.peek() {
                            result.push(chars.next().unwrap());
                        }
                    }
                }
            }
            // Possible comment start
            '/' => {
                chars.next();
                match chars.peek() {
                    // Line comment: replace with spaces up to newline
                    Some(&'/') => {
                        result.push(' '); // replace first /
                        chars.next();
                        result.push(' '); // replace second /
                        while let Some(&lc) = chars.peek() {
                            if lc == '\n' {
                                break;
                            }
                            chars.next();
                            result.push(' ');
                        }
                    }
                    // Block comment: replace with spaces, preserve newlines
                    Some(&'*') => {
                        result.push(' '); // replace /
                        chars.next();
                        result.push(' '); // replace *
                        loop {
                            match chars.next() {
                                Some('*') => {
                                    if chars.peek() == Some(&'/') {
                                        chars.next();
                                        result.push(' '); // replace *
                                        result.push(' '); // replace /
                                        break;
                                    }
                                    result.push(' ');
                                }
                                Some('\n') => result.push('\n'),
                                Some(_) => result.push(' '),
                                None => break, // unterminated block comment
                            }
                        }
                    }
                    // Not a comment — it's a `/` (division or regex)
                    _ => {
                        result.push('/');
                    }
                }
            }
            _ => {
                result.push(chars.next().unwrap());
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identifier() {
        let mut input = "myVar rest";
        assert_eq!(identifier(&mut input), Ok("myVar"));
        assert_eq!(input, " rest");
    }

    #[test]
    fn test_quoted_string() {
        let mut input = "\"hello world\" rest";
        assert_eq!(quoted_string(&mut input), Ok("hello world"));
        assert_eq!(input, " rest");
    }

    #[test]
    fn test_var_list() {
        let mut input = "groupId, accountId, token";
        let result = var_list(&mut input).unwrap();
        assert_eq!(result, vec!["groupId", "accountId", "token"]);
    }

    #[test]
    fn test_strip_line_comment() {
        let input = "groupId = r.id // capture the id\nnext = r.name";
        let result = strip_comments(input);
        assert_eq!(result, "groupId = r.id                  \nnext = r.name");
        assert_eq!(result.len(), input.len(), "length must be preserved");
    }

    #[test]
    fn test_strip_block_comment() {
        let input = "a = 1 /* this is\na block comment */ b = 2";
        let result = strip_comments(input);
        // "/* this is\na block comment */" → "           \n                   "
        assert_eq!(result, "a = 1           \n                   b = 2");
        assert_eq!(result.len(), input.len(), "length must be preserved");
    }

    #[test]
    fn test_comment_in_string_preserved() {
        let input = "msg = \"hello // world\"";
        assert_eq!(strip_comments(input), "msg = \"hello // world\"");
    }

    #[test]
    fn test_block_comment_in_string_preserved() {
        let input = "msg = \"hello /* world */\"";
        assert_eq!(strip_comments(input), "msg = \"hello /* world */\"");
    }

    #[test]
    fn test_slash_not_comment() {
        let input = "x = a / b";
        assert_eq!(strip_comments(input), "x = a / b");
    }

    #[test]
    fn test_full_line_comment() {
        let input = "// this whole line is a comment\nx = 1";
        let result = strip_comments(input);
        assert_eq!(result, "                               \nx = 1");
        assert_eq!(result.len(), input.len(), "length must be preserved");
    }
}