pub mod primitives;
pub mod http;
pub mod expr;
pub mod statement;

use winnow::prelude::*;
use winnow::error::ModalResult;
use winnow::combinator::opt;
use winnow::token::take_while;

use crate::ast::*;
use self::primitives::{var_list, ws, strip_comments};
use self::statement::statements_with_lines;

/// The metadata keys we recognize. Anything else is a hard error (a typo'd
/// directive that silently no-ops defeats the point of the feature).
const META_KEYS: &[&str] = &["requires", "disabled", "blast-radius"];

/// Parse a metadata key token followed by its colon: `blast-radius :`. Keys
/// allow dashes (unlike identifiers). Backtracks cleanly if there's no `:` — so
/// a param header (`a, b -->`) or a body-opening `{` falls through to the
/// normal header parse rather than being mistaken for metadata.
fn meta_key<'a>(input: &mut &'a str) -> ModalResult<&'a str> {
    let key = take_while(1.., |c: char| c.is_alphanumeric() || c == '-' || c == '_')
        .parse_next(input)?;
    let _ = ws.parse_next(input);
    ':'.parse_next(input)?;
    Ok(key)
}

/// Consume the rest of the current physical line (up to, but not including, the
/// newline). The metadata value is line-oriented — everything after the colon.
fn rest_of_line<'a>(input: &mut &'a str) -> &'a str {
    let end = input.find('\n').unwrap_or(input.len());
    let line = &input[..end];
    *input = &input[end..];
    line
}

/// Parse the header-region metadata block: zero or more `key: value` lines that
/// precede the function header. `value` is the rest of the line, trimmed and
/// unquoted. Stops at the first line that isn't a `key:` directive — i.e. the
/// param header or the body's `{`. Unknown keys and malformed values are hard
/// errors (with source-line context).
fn metadata_block(input: &mut &str, source: &str, stripped: &str) -> Result<Metadata, String> {
    let mut meta = Metadata::default();
    loop {
        let _ = ws.parse_next(input);
        // Checkpoint so a non-metadata line (the header / body) is left intact
        // for the next parse stage, and so error context points at the key.
        let at = *input;
        let key = match meta_key.parse_next(input) {
            Ok(k) => k,
            Err(_) => {
                *input = at;
                break;
            }
        };
        let value = rest_of_line(input).trim().to_string();
        apply_meta(&mut meta, key, &value)
            .map_err(|msg| format_parse_error_ctx(source, stripped, at, &msg))?;
    }
    Ok(meta)
}

/// Fold one parsed `key: value` directive into the accumulating `Metadata`.
/// Returns a bare message on error; the caller wraps it with line context.
fn apply_meta(meta: &mut Metadata, key: &str, value: &str) -> Result<(), String> {
    match key {
        "requires" => {
            if value.is_empty() {
                return Err("`requires:` needs a version requirement (e.g. `>= 0.5.3`)".into());
            }
            // Validate the constraint now so a typo (`requires: soonish`) fails at
            // parse time with line context, not silently at run time. We keep the
            // raw string; the runner re-parses it to gate execution.
            crate::version::parse_requirement(value)?;
            meta.requires = Some(value.to_string());
        }
        "disabled" => {
            if value.is_empty() {
                return Err("`disabled:` needs a reason".into());
            }
            meta.disabled = Some(value.to_string());
        }
        "blast-radius" => {
            meta.blast_radius = Some(parse_blast_radius(value)?);
        }
        other => {
            return Err(format!(
                "unknown metadata key '{}' (known: {})",
                other,
                META_KEYS.join(", ")
            ));
        }
    }
    Ok(())
}

/// Parse a `blast-radius:` value into its span form: a bare count (`3`), the
/// whole-leaf sentinels (`all` / `*`), or a filename-prefix endpoint (`<=PREFIX`).
fn parse_blast_radius(value: &str) -> Result<BlastRadius, String> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("all") || v == "*" {
        Ok(BlastRadius::All)
    } else if let Some(prefix) = v.strip_prefix("<=") {
        let prefix = prefix.trim();
        if prefix.is_empty() {
            return Err("`blast-radius: <=` needs a filename prefix (e.g. `<=05`)".into());
        }
        Ok(BlastRadius::Through(prefix.to_string()))
    } else {
        v.parse::<u32>().map(BlastRadius::Count).map_err(|_| {
            format!(
                "`blast-radius` must be a count, `all`, `*`, or `<=prefix` — got '{}'",
                v
            )
        })
    }
}

/// Parse the input line: `var1, var2 -->`, a bare `-->`, or nothing at all.
/// Returns the list of input variable names (empty if there's no param list).
///
/// The arrow is mandatory *when parameters are declared* (`a, b -->`) but
/// optional when there are none — a no-input file may open straight into its
/// `{ ... }` body. `--> { ... }` stays valid as an explicit synonym for the
/// no-param case.
fn input_line(input: &mut &str) -> ModalResult<Vec<String>> {
    let vars = opt(
        (var_list, ws).map(|(v, _)| v)
    ).parse_next(input)?;
    match vars {
        // Params declared → the arrow is required: `a, b -->`.
        Some(v) => {
            "-->".parse_next(input)?;
            Ok(v)
        }
        // No params → the arrow is optional. Consume it if present, but a bare
        // `{ ... }` body is equally valid.
        None => {
            let _ = opt("-->").parse_next(input)?;
            Ok(Vec::new())
        }
    }
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

    // Header-region metadata block (`requires:`, `disabled:`, `blast-radius:`).
    // Sits above the function header; consumes nothing if there's none.
    let metadata = metadata_block(input, source, &stripped)?;

    // Input header: `a, b -->` declares params, a bare `-->` (or nothing at
    // all) means no params. The arrow is only required when params are present,
    // so a failure here means params were declared without the trailing `-->`.
    let inputs = match input_line.parse_next(input) {
        Ok(vars) => vars,
        Err(_) => return Err(format_parse_error_ctx(source, &stripped, *input, "expected '-->' after input parameters")),
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

    // A top-level `return` is void — it only halts. Publishing is `export`.
    // (Lambda yields live inside block expressions, not file.body, so they're
    // unaffected.)
    if body.iter().any(|s| matches!(s, crate::ast::Statement::Return { value: Some(_) })) {
        return Err(format_parse_error_ctx(source, &stripped, *input,
            "a top-level `return` takes no value — use `export` to publish, or `return;` to halt"));
    }

    // Declared outputs = the names the file's `export` statements publish.
    // Downstream skip/block messages name these.
    let outputs: Vec<String> = body.iter().filter_map(|s| match s {
        crate::ast::Statement::Export { value: crate::ast::Expr::JsonObject(pairs) } =>
            Some(pairs.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()),
        _ => None,
    }).flatten().collect();

    Ok(File {
        file_type,
        metadata,
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
    fn test_parse_no_param_bare_brace() {
        // A no-input file may skip the arrow and open straight into its body.
        let source = r#"{ r = req.get("/v4/groups") ? 2xx | "Failed"; }"#;
        let file = parse_file(source, "list-groups.test.tstr").unwrap();
        assert_eq!(file.inputs, Vec::<String>::new());
        assert_eq!(file.body.len(), 1);
    }

    #[test]
    fn test_input_line_bare_brace() {
        // No params, no arrow: input_line consumes nothing and yields no vars.
        let mut input = "{ x = 1; }";
        let result = input_line(&mut input).unwrap();
        assert_eq!(result, Vec::<String>::new());
        assert_eq!(input, "{ x = 1; }");
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
            export groupId;
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
            export memberId;
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
            export testSiteId, testAccountId;
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
            export groupId;
            }
        "#;
        let file = parse_file(source, "create-group.test.tstr").unwrap();
        assert_eq!(file.body.len(), 3);
    }

    // --- metadata block tests ---

    #[test]
    fn test_no_metadata_is_default() {
        let file = parse_file("{ x = 1; }", "t.test.tstr").unwrap();
        assert_eq!(file.metadata, Metadata::default());
    }

    #[test]
    fn test_parse_metadata_all_keys() {
        let source = r#"
            requires: >= 0.5.3
            disabled: I-55555 something bad to fix
            blast-radius: 2

            a, b --> {
            r = req.get("/v4/groups") ? 2xx | "Failed";
            }
        "#;
        let file = parse_file(source, "check.test.tstr").unwrap();
        assert_eq!(file.metadata.requires.as_deref(), Some(">= 0.5.3"));
        assert_eq!(file.metadata.disabled.as_deref(), Some("I-55555 something bad to fix"));
        assert_eq!(file.metadata.blast_radius, Some(BlastRadius::Count(2)));
        // Metadata sits above the header — params and body still parse normally.
        assert_eq!(file.inputs, vec!["a", "b"]);
        assert_eq!(file.body.len(), 1);
    }

    #[test]
    fn test_metadata_above_bare_brace() {
        // Metadata + no-param file: opens straight into `{ ... }`.
        let source = "requires: >= 0.5.3\n{ x = 1; }";
        let file = parse_file(source, "t.test.tstr").unwrap();
        assert_eq!(file.metadata.requires.as_deref(), Some(">= 0.5.3"));
        assert_eq!(file.inputs, Vec::<String>::new());
        assert_eq!(file.body.len(), 1);
    }

    #[test]
    fn test_metadata_value_is_rest_of_line_unquoted() {
        // The reason is the whole line after the colon — embedded colons and
        // spaces are kept verbatim, no quotes needed.
        let file = parse_file("disabled: I-9: blocked on upstream\n{ x = 1; }", "t.test.tstr").unwrap();
        assert_eq!(file.metadata.disabled.as_deref(), Some("I-9: blocked on upstream"));
    }

    #[test]
    fn test_metadata_disabled_feeds_disabled_reason() {
        // The metadata form is wired through File::disabled_reason().
        let file = parse_file("disabled: fix pending\n{ false | \"nope\"; }", "t.test.tstr").unwrap();
        assert_eq!(file.disabled_reason(), Some("fix pending"));
    }

    #[test]
    fn test_blast_radius_forms() {
        let cases = [
            ("3", BlastRadius::Count(3)),
            ("all", BlastRadius::All),
            ("*", BlastRadius::All),
            ("<=05", BlastRadius::Through("05".to_string())),
            ("<= create-org", BlastRadius::Through("create-org".to_string())),
        ];
        for (val, expected) in cases {
            let src = format!("blast-radius: {}\n{{ x = 1; }}", val);
            let file = parse_file(&src, "t.test.tstr").unwrap();
            assert_eq!(file.metadata.blast_radius, Some(expected), "value: {}", val);
        }
    }

    #[test]
    fn test_metadata_unknown_key_errors() {
        let err = parse_file("requires: >= 0.5.3\nfoo: bar\n{ x = 1; }", "t.test.tstr").unwrap_err();
        assert!(err.contains("unknown metadata key 'foo'"), "got: {}", err);
    }

    #[test]
    fn test_metadata_empty_disabled_reason_errors() {
        let err = parse_file("disabled:\n{ x = 1; }", "t.test.tstr").unwrap_err();
        assert!(err.contains("disabled"), "got: {}", err);
    }

    #[test]
    fn test_metadata_bad_requires_errors_at_parse() {
        // A malformed version constraint is caught at parse time, not deferred.
        let err = parse_file("requires: soonish\n{ x = 1; }", "t.test.tstr").unwrap_err();
        assert!(err.contains("requires"), "got: {}", err);
    }

    #[test]
    fn test_metadata_bad_blast_radius_errors() {
        let err = parse_file("blast-radius: soon\n{ x = 1; }", "t.test.tstr").unwrap_err();
        assert!(err.contains("blast-radius"), "got: {}", err);
    }

    #[test]
    fn test_param_header_not_mistaken_for_metadata() {
        // `orgId -->` has no colon, so it must fall through to the header parse,
        // not be eaten as a metadata line.
        let file = parse_file("orgId --> { x = 1; }", "t.test.tstr").unwrap();
        assert_eq!(file.inputs, vec!["orgId"]);
        assert_eq!(file.metadata, Metadata::default());
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
    fn test_parse_error_params_without_arrow() {
        // Declaring params but omitting the `-->` is a hard error. (Here `r`
        // reads as a param name, so the parser then demands the arrow.)
        let source = r#"r, token = req.get("/v4/groups") ? 2xx | "Failed";"#;
        let err = parse_file(source, "bad.test.tstr").unwrap_err();
        assert!(err.contains("-->"), "should demand the arrow, got: {}", err);
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
