pub mod primitives;
pub mod http;
pub mod expr;
pub mod statement;

use winnow::prelude::*;
use winnow::error::ModalResult;
use winnow::combinator::opt;

use crate::ast::*;
use self::primitives::{var_list, ws, strip_comments};
use self::statement::statements_with_lines;

/// Parse the input line: `var1, var2 -->` or just `-->`
/// Returns the list of input variable names (empty if just `-->`).
fn input_line(input: &mut &str) -> ModalResult<Vec<String>> {
    let vars = opt(
        (var_list, ws).map(|(v, _)| v)
    ).parse_next(input)?;
    "-->".parse_next(input)?;
    Ok(vars.unwrap_or_default())
}

/// Determine the FileType from a filename's middle extension.
/// `create-group.test.tstr` → Test, `values.const.tstr` → Const, `foo.tstr` → Test
pub fn file_type_from_filename(filename: &str) -> FileType {
    // Strip the .tstr extension, then check the remaining extension
    let without_tstr = filename.strip_suffix(".tstr").unwrap_or(filename);

    if let Some(dot_pos) = without_tstr.rfind('.') {
        match &without_tstr[dot_pos + 1..] {
            "test" => FileType::Test,
            "fetch" => FileType::Fetch,
            "setup" => FileType::Setup,
            "cleanup" => FileType::Cleanup,
            "const" => FileType::Const,
            "exporter" => FileType::Exporter,
            "lib" => FileType::Lib,
            _ => FileType::Test,
        }
    } else {
        FileType::Test
    }
}

/// Compute line number, column, and the text of that line from a byte offset.
/// Returns (line, col, line_text) — all 1-indexed.
fn position_of(source: &str, offset: usize) -> (usize, usize, &str) {
    let before = &source[..offset.min(source.len())];
    let line = before.chars().filter(|&c| c == '\n').count() + 1;
    let last_nl = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let col = offset - last_nl + 1;
    let line_end = source[offset..].find('\n')
        .map(|p| offset + p)
        .unwrap_or(source.len());
    let line_text = &source[last_nl..line_end];
    (line, col, line_text)
}

/// Format a parse error with line number, the offending line, and a caret pointer.
/// Format a parse error with line number, the offending line, and a caret pointer.
/// `original` is the raw source (for display), `stripped` is post-comment-removal (for offset math).
fn format_parse_error_ctx(original: &str, stripped: &str, remaining: &str, hint: &str) -> String {
    let offset = stripped.len() - remaining.len();
    let (line, col, _) = position_of(stripped, offset);
    // Show the original source line (with comments intact) for readability
    let (_, _, orig_line_text) = position_of(original, offset);
    let trimmed = orig_line_text.trim_end();
    let leading_spaces = orig_line_text.len() - orig_line_text.trim_start().len();
    let display_line = trimmed.trim_start();
    let caret_pos = col.saturating_sub(1).saturating_sub(leading_spaces);
    let caret = format!("{:>width$}", "^", width = caret_pos + 1);
    format!("line {}: {}\n  {}\n  {}", line, hint, display_line, caret)
}

/// Parse a complete .tstr file into a File AST.
/// `source` is the raw file content, `filename` is used to determine the FileType.
pub fn parse_file(source: &str, filename: &str) -> Result<File, String> {
    let file_type = file_type_from_filename(filename);
    let stripped = strip_comments(source);
    let input = &mut stripped.as_str();

    // Skip leading whitespace
    let _ = ws.parse_next(input);

    // Input header is mandatory under the function form: `a, b -->`, or a bare
    // `-->` for a file that takes no inputs.
    let inputs = match input_line.parse_next(input) {
        Ok(vars) => vars,
        Err(_) => return Err(format_parse_error_ctx(source, &stripped, *input, "expected input header ('a, b -->', or '-->' for none)")),
    };

    // The body is a braced block: `--> { ... }`.
    let _ = ws.parse_next(input);
    if !input.starts_with('{') {
        return Err(format_parse_error_ctx(source, &stripped, *input, "expected '{' to open the file body"));
    }
    *input = &input[1..];

    // Parse the body statements (with line tracking)
    let (body, line_map) = match statements_with_lines(input, &stripped) {
        Ok(result) => result,
        Err(_) => return Err(format_parse_error_ctx(source, &stripped, *input, "expected statement")),
    };

    let _ = ws.parse_next(input);
    if !input.starts_with('}') {
        return Err(format_parse_error_ctx(source, &stripped, *input, "expected '}' to close the file body"));
    }
    *input = &input[1..];

    // Verify all input consumed
    let _ = ws.parse_next(input);
    if !input.is_empty() {
        return Err(format_parse_error_ctx(source, &stripped, *input, "unexpected content after end of file"));
    }

    // Declared outputs = the names the file's `return` publishes. Derived from
    // the last return so downstream skip/block messages can still name them
    // (the old `<-- a, b` line is gone; `return a, b` carries this now).
    let outputs: Vec<String> = body.iter().rev().find_map(|s| match s {
        crate::ast::Statement::Return { value: Some(crate::ast::Expr::JsonObject(pairs)) } =>
            Some(pairs.iter().map(|(k, _)| k.clone()).collect()),
        _ => None,
    }).unwrap_or_default();

    Ok(File {
        file_type,
        inputs,
        body,
        outputs,
        line_map,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- input/output line tests ---

    #[test]
    fn test_input_line_with_vars() {
        let mut input = "groupId, token -->";
        let result = input_line(&mut input).unwrap();
        assert_eq!(result, vec!["groupId", "token"]);
    }

    #[test]
    fn test_input_line_no_vars() {
        let mut input = "-->";
        let result = input_line(&mut input).unwrap();
        assert_eq!(result, Vec::<String>::new());
    }

    // --- file type detection ---

    #[test]
    fn test_file_type_test() {
        assert_eq!(file_type_from_filename("create-group.test.tstr"), FileType::Test);
    }

    #[test]
    fn test_file_type_const() {
        assert_eq!(file_type_from_filename("shared-values.const.tstr"), FileType::Const);
    }

    #[test]
    fn test_file_type_fetch() {
        assert_eq!(file_type_from_filename("site-config.fetch.tstr"), FileType::Fetch);
    }

    #[test]
    fn test_file_type_setup() {
        assert_eq!(file_type_from_filename("prepare-data.setup.tstr"), FileType::Setup);
    }

    #[test]
    fn test_file_type_cleanup() {
        assert_eq!(file_type_from_filename("remove-data.cleanup.tstr"), FileType::Cleanup);
    }

    #[test]
    fn test_file_type_exporter() {
        assert_eq!(file_type_from_filename("crud-results.exporter.tstr"), FileType::Exporter);
    }

    #[test]
    fn test_file_type_lib() {
        assert_eq!(file_type_from_filename("create-org.lib.tstr"), FileType::Lib);
    }

    #[test]
    fn test_file_type_default() {
        assert_eq!(file_type_from_filename("simple.tstr"), FileType::Test);
    }

    // --- full file parsing ---

    #[test]
    fn test_parse_minimal_file() {
        let source = r#"--> { r = req.get("/v4/groups") ? 2xx | "Failed"; }"#;
        let file = parse_file(source, "list-groups.test.tstr").unwrap();
        assert_eq!(file.file_type, FileType::Test);
        assert_eq!(file.inputs, Vec::<String>::new());
        assert_eq!(file.body.len(), 1);
    }

    #[test]
    fn test_parse_file_with_inputs() {
        let source = r#"
            groupId --> {
            r = req.get("/v4/groups") ? 2xx | "Failed";
            }
        "#;
        let file = parse_file(source, "check-group.test.tstr").unwrap();
        assert_eq!(file.inputs, vec!["groupId"]);
        assert_eq!(file.body.len(), 1);
    }

    #[test]
    fn test_parse_file_with_return() {
        let source = r#"
            --> {
            r = req.post("/v4/groups") ? 2xx | "Failed";
            groupId = r.id;
            return groupId;
            }
        "#;
        let file = parse_file(source, "create-group.test.tstr").unwrap();
        assert_eq!(file.inputs, Vec::<String>::new());
        // r = ..., groupId = ..., return groupId
        assert_eq!(file.body.len(), 3);
    }

    #[test]
    fn test_parse_full_file() {
        let source = r#"
            groupId, headers --> {
            req.headers = headers;
            req.body = "test";
            r = req.post("/v4/groups") ? 2xx | "Failed";
            r.name != null | "missing name";
            memberId = r.id;
            return memberId;
            }
        "#;
        let file = parse_file(source, "add-member.test.tstr").unwrap();
        assert_eq!(file.file_type, FileType::Test);
        assert_eq!(file.inputs, vec!["groupId", "headers"]);
        // 5 body statements + the return
        assert_eq!(file.body.len(), 6);
    }

    #[test]
    fn test_parse_const_file() {
        let source = r#"
            --> {
            testSiteId = "00000000-0000-0000-0000-000000000001";
            testAccountId = "00000000-0000-0000-0000-000000000002";
            return testSiteId, testAccountId;
            }
        "#;
        let file = parse_file(source, "shared-values.const.tstr").unwrap();
        assert_eq!(file.file_type, FileType::Const);
        assert_eq!(file.inputs, Vec::<String>::new());
        // 2 assignments + the return
        assert_eq!(file.body.len(), 3);
    }

    #[test]
    fn test_parse_file_with_comments() {
        let source = r#"
            --> {
            // This test creates a group
            r = req.post("/v4/groups") ? 2xx | "Failed";
            groupId = r.id; /* capture for downstream */
            return groupId;
            }
        "#;
        let file = parse_file(source, "create-group.test.tstr").unwrap();
        assert_eq!(file.body.len(), 3);
    }

    // --- error message tests ---

    #[test]
    fn test_parse_error_shows_line_number() {
        let source = "--> {\nr = req.get(\"/v4/groups\") ? 2xx | \"Failed\";\nthis is garbage\n}";
        let err = parse_file(source, "bad.test.tstr").unwrap_err();
        assert!(err.contains("line 3"), "should mention line 3, got: {}", err);
    }

    #[test]
    fn test_parse_error_shows_offending_line() {
        let source = "--> {\nr = req.get(\"/v4/groups\") ? 2xx | \"Failed\";\nthis is garbage\n}";
        let err = parse_file(source, "bad.test.tstr").unwrap_err();
        assert!(err.contains("this is garbage"), "should show the bad line, got: {}", err);
    }

    #[test]
    fn test_parse_error_missing_header() {
        // No `-->` header is now a hard error.
        let source = r#"r = req.get("/v4/groups") ? 2xx | "Failed";"#;
        let err = parse_file(source, "bad.test.tstr").unwrap_err();
        assert!(err.contains("input header"), "should demand a header, got: {}", err);
    }

    #[test]
    fn test_parse_error_trailing_content() {
        let source = "--> { x = 1; } extra";
        let err = parse_file(source, "bad.test.tstr").unwrap_err();
        assert!(err.contains("line 1"), "should mention line 1, got: {}", err);
        assert!(err.contains("unexpected content"), "should say unexpected, got: {}", err);
    }

    #[test]
    fn test_parse_error_with_comments_preserves_line() {
        let source = "// comment\n// another comment\n--> { bad syntax here }";
        let err = parse_file(source, "bad.test.tstr").unwrap_err();
        assert!(err.contains("line 3"), "should mention line 3, got: {}", err);
    }
}
