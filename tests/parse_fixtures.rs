use tstr::parser::parse_file;
use tstr::ast::*;

fn load_fixture(name: &str) -> String {
    std::fs::read_to_string(format!("tests/fixtures/{}", name))
        .unwrap_or_else(|e| panic!("failed to load fixture {}: {}", name, e))
}

// ---------------------------------------------------------------------------
// Basic file structure
// ---------------------------------------------------------------------------

#[test]
fn minimal_test() {
    let file = parse_file(&load_fixture("minimal.tstr"), "minimal.tstr").unwrap();
    assert_eq!(file.file_type, FileType::Test);
    assert_eq!(file.inputs, vec!["req"]);
    assert_eq!(file.body.len(), 1);
    assert!(file.outputs.is_empty());
    assert!(matches!(file.body[0], Statement::HttpCall { .. }));
}

#[test]
fn const_file() {
    let file = parse_file(&load_fixture("const-values.const.tstr"), "const-values.const.tstr").unwrap();
    assert_eq!(file.file_type, FileType::Const);
    assert!(file.inputs.is_empty());
    assert_eq!(file.body.len(), 4); // three assignments + return
    assert_eq!(file.outputs, vec!["testSiteId", "testAccountId", "baseGroup"]);
}

#[test]
fn test_with_inputs_and_outputs() {
    let file = parse_file(&load_fixture("create-group.test.tstr"), "create-group.test.tstr").unwrap();
    assert_eq!(file.file_type, FileType::Test);
    assert_eq!(file.inputs, vec!["headers", "req"]);
    assert_eq!(file.outputs, vec!["groupId"]);
    // ...6 statements + return = 7
    assert_eq!(file.body.len(), 7);
}

#[test]
fn test_with_multiple_inputs() {
    let file = parse_file(&load_fixture("add-member.test.tstr"), "add-member.test.tstr").unwrap();
    assert_eq!(file.inputs, vec!["groupId", "headers", "req"]);
    assert!(file.outputs.is_empty());
    // headers assign, body assign, http call, assertion = 4
    assert_eq!(file.body.len(), 4);
}

#[test]
fn exporter_file() {
    let file = parse_file(&load_fixture("exporter.exporter.tstr"), "exporter.exporter.tstr").unwrap();
    assert_eq!(file.file_type, FileType::Exporter);
    assert_eq!(file.inputs, vec!["groupId", "groupName"]);
    assert_eq!(file.body.len(), 1); // just the return
    assert_eq!(file.outputs, vec!["groupId", "groupName"]);
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[test]
fn expressions_parse() {
    let file = parse_file(&load_fixture("expressions.test.tstr"), "expressions.test.tstr").unwrap();
    // Count: 3 arithmetic assignments + 4 comparison assertions + 2 logical assertions
    //        + 1 negation assertion + 2 null check assertions + 1 parenthesized assignment = 13
    assert_eq!(file.body.len(), 13);
}

#[test]
fn expressions_arithmetic() {
    let file = parse_file(&load_fixture("expressions.test.tstr"), "expressions.test.tstr").unwrap();
    // First statement: total = price * quantity + tax;
    match &file.body[0] {
        Statement::Assignment { target: AssignTarget::Variable(name), value } => {
            assert_eq!(name, "total");
            // Should be (price * quantity) + tax due to precedence
            assert!(matches!(value, Expr::BinaryOp { op: BinOp::Add, .. }));
        }
        other => panic!("expected assignment, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Property access
// ---------------------------------------------------------------------------

#[test]
fn property_access_parse() {
    let file = parse_file(&load_fixture("property-access.test.tstr"), "property-access.test.tstr").unwrap();
    // http call + 11 assignments = 12
    assert_eq!(file.body.len(), 12);
}

#[test]
fn property_access_optional_chaining() {
    let file = parse_file(&load_fixture("property-access.test.tstr"), "property-access.test.tstr").unwrap();
    // Statement index 5: maybeName = r.user?.name;
    match &file.body[5] {
        Statement::Assignment { target: AssignTarget::Variable(name), value } => {
            assert_eq!(name, "maybeName");
            assert!(matches!(value, Expr::OptionalAccess { .. }));
        }
        other => panic!("expected optional chaining assignment, got {:?}", other),
    }
}

#[test]
fn property_access_collect() {
    let file = parse_file(&load_fixture("property-access.test.tstr"), "property-access.test.tstr").unwrap();
    // Statement index 10: allIds = r.items[].id;
    match &file.body[10] {
        Statement::Assignment { target: AssignTarget::Variable(name), value } => {
            assert_eq!(name, "allIds");
            assert!(matches!(value, Expr::CollectAccess { .. }));
        }
        other => panic!("expected collect access assignment, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Regex
// ---------------------------------------------------------------------------

#[test]
fn regex_ops_parse() {
    let file = parse_file(&load_fixture("regex-ops.test.tstr"), "regex-ops.test.tstr").unwrap();
    // http call + 2 assertions + 2 assignments = 5
    assert_eq!(file.body.len(), 5);
}

#[test]
fn regex_test_operator() {
    let file = parse_file(&load_fixture("regex-ops.test.tstr"), "regex-ops.test.tstr").unwrap();
    // Statement 1: r.tag ~? /test/ | "...";
    match &file.body[1] {
        Statement::Assertion { expr, message } => {
            assert!(matches!(expr, Expr::BinaryOp { op: BinOp::RegexTest, .. }));
            assert_eq!(message, "tag should contain test");
        }
        other => panic!("expected regex test assertion, got {:?}", other),
    }
}

#[test]
fn regex_no_match_operator() {
    let file = parse_file(&load_fixture("regex-ops.test.tstr"), "regex-ops.test.tstr").unwrap();
    // Statement 2: r.name !~ /^temp/ | "...";
    match &file.body[2] {
        Statement::Assertion { expr, message } => {
            assert!(matches!(expr, Expr::BinaryOp { op: BinOp::RegexNoMatch, .. }));
            assert_eq!(message, "name should not start with temp");
        }
        other => panic!("expected regex no-match assertion, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Status patterns
// ---------------------------------------------------------------------------

#[test]
fn status_patterns_parse() {
    let file = parse_file(&load_fixture("status-patterns.test.tstr"), "status-patterns.test.tstr").unwrap();
    assert_eq!(file.body.len(), 6);
}

#[test]
fn status_wildcard() {
    let file = parse_file(&load_fixture("status-patterns.test.tstr"), "status-patterns.test.tstr").unwrap();
    match &file.body[0] {
        Statement::HttpCall { status_check: Some(sc), .. } => {
            assert_eq!(sc.patterns, vec![StatusPattern::Wildcard(2)]);
        }
        other => panic!("expected http call with wildcard status, got {:?}", other),
    }
}

#[test]
fn status_exact() {
    let file = parse_file(&load_fixture("status-patterns.test.tstr"), "status-patterns.test.tstr").unwrap();
    match &file.body[1] {
        Statement::HttpCall { status_check: Some(sc), .. } => {
            assert_eq!(sc.patterns, vec![StatusPattern::Exact(201)]);
        }
        other => panic!("expected http call with exact status, got {:?}", other),
    }
}

#[test]
fn status_multiple_exact() {
    let file = parse_file(&load_fixture("status-patterns.test.tstr"), "status-patterns.test.tstr").unwrap();
    match &file.body[2] {
        Statement::HttpCall { status_check: Some(sc), .. } => {
            assert_eq!(sc.patterns, vec![StatusPattern::Exact(200), StatusPattern::Exact(204)]);
        }
        other => panic!("expected http call with multiple status codes, got {:?}", other),
    }
}

#[test]
fn status_range() {
    let file = parse_file(&load_fixture("status-patterns.test.tstr"), "status-patterns.test.tstr").unwrap();
    match &file.body[3] {
        Statement::HttpCall { status_check: Some(sc), .. } => {
            assert_eq!(sc.patterns, vec![StatusPattern::Range(200, 204)]);
        }
        other => panic!("expected http call with range status, got {:?}", other),
    }
}

#[test]
fn status_comparison() {
    let file = parse_file(&load_fixture("status-patterns.test.tstr"), "status-patterns.test.tstr").unwrap();
    match &file.body[4] {
        Statement::HttpCall { status_check: Some(sc), .. } => {
            assert_eq!(sc.patterns, vec![StatusPattern::Comparison(CompOp::Lt, 400)]);
        }
        other => panic!("expected http call with comparison status, got {:?}", other),
    }
}

#[test]
fn status_none() {
    let file = parse_file(&load_fixture("status-patterns.test.tstr"), "status-patterns.test.tstr").unwrap();
    match &file.body[5] {
        Statement::HttpCall { status_check, .. } => {
            assert!(status_check.is_none());
        }
        other => panic!("expected http call without status check, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Conditionals
// ---------------------------------------------------------------------------

#[test]
fn if_cond_parse() {
    let file = parse_file(&load_fixture("if-cond.test.tstr"), "if-cond.test.tstr").unwrap();
    assert_eq!(file.inputs, vec!["groupId", "req"]);
    // A single `if` wrapping the body assign + http call.
    assert_eq!(file.body.len(), 1);
    match &file.body[0] {
        Statement::If { then_body, else_body, .. } => {
            assert_eq!(then_body.len(), 2);
            assert!(else_body.is_empty());
        }
        other => panic!("expected If, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// JS blocks
// ---------------------------------------------------------------------------

#[test]
fn js_blocks_parse() {
    let file = parse_file(&load_fixture("js-blocks.test.tstr"), "js-blocks.test.tstr").unwrap();
    // http call + 2 js assignments + 1 standalone js block = 4
    assert_eq!(file.body.len(), 4);
}

#[test]
fn js_assignment() {
    let file = parse_file(&load_fixture("js-blocks.test.tstr"), "js-blocks.test.tstr").unwrap();
    // Statement 1: filtered = js:{ ... };
    match &file.body[1] {
        Statement::Assignment { target: AssignTarget::Variable(name), value: Expr::JsExpr(code) } => {
            assert_eq!(name, "filtered");
            assert!(code.contains("filter"));
        }
        other => panic!("expected js assignment, got {:?}", other),
    }
}

#[test]
fn js_standalone() {
    let file = parse_file(&load_fixture("js-blocks.test.tstr"), "js-blocks.test.tstr").unwrap();
    // Last statement: js:{ console.log(...) };
    match &file.body[3] {
        Statement::JsBlock { code } => {
            assert!(code.contains("console.log"));
        }
        other => panic!("expected standalone js block, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

#[test]
fn comments_stripped() {
    let file = parse_file(&load_fixture("comments.test.tstr"), "comments.test.tstr").unwrap();
    // After comment stripping: http call, groupId assign, msg assign, msg2 assign = 4
    assert_eq!(file.body.len(), 4);
}

#[test]
fn comments_preserve_strings() {
    let file = parse_file(&load_fixture("comments.test.tstr"), "comments.test.tstr").unwrap();
    // msg = "hello // not a comment"
    match &file.body[2] {
        Statement::Assignment { target: AssignTarget::Variable(name), value: Expr::StringLiteral(s) } => {
            assert_eq!(name, "msg");
            assert_eq!(s, "hello // not a comment");
        }
        other => panic!("expected string with preserved comment chars, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// JSON objects and arrays
// ---------------------------------------------------------------------------

#[test]
fn json_objects_parse() {
    let file = parse_file(&load_fixture("json-objects.test.tstr"), "json-objects.test.tstr").unwrap();
    // simple, nested, quoted, empty, ids, emptyArr, complex = 7
    assert_eq!(file.body.len(), 7);
}

#[test]
fn json_empty_object() {
    let file = parse_file(&load_fixture("json-objects.test.tstr"), "json-objects.test.tstr").unwrap();
    // Statement 3: empty = {};
    match &file.body[3] {
        Statement::Assignment { value: Expr::JsonObject(entries), .. } => {
            assert!(entries.is_empty());
        }
        other => panic!("expected empty json object, got {:?}", other),
    }
}

#[test]
fn json_nested_object() {
    let file = parse_file(&load_fixture("json-objects.test.tstr"), "json-objects.test.tstr").unwrap();
    // Statement 1: nested = { user: { ... }, active: true }
    match &file.body[1] {
        Statement::Assignment { value: Expr::JsonObject(entries), .. } => {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].0, "user");
            assert!(matches!(entries[0].1, Expr::JsonObject(_)));
            assert_eq!(entries[1].0, "active");
            assert_eq!(entries[1].1, Expr::Bool(true));
        }
        other => panic!("expected nested json object, got {:?}", other),
    }
}

#[test]
fn json_array() {
    let file = parse_file(&load_fixture("json-objects.test.tstr"), "json-objects.test.tstr").unwrap();
    // Statement 4: ids = [1, 2, 3];
    match &file.body[4] {
        Statement::Assignment { value: Expr::JsonArray(items), .. } => {
            assert_eq!(items.len(), 3);
        }
        other => panic!("expected json array, got {:?}", other),
    }
}

#[test]
fn json_empty_array() {
    let file = parse_file(&load_fixture("json-objects.test.tstr"), "json-objects.test.tstr").unwrap();
    // Statement 5: emptyArr = [];
    match &file.body[5] {
        Statement::Assignment { value: Expr::JsonArray(items), .. } => {
            assert!(items.is_empty());
        }
        other => panic!("expected empty json array, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Field mutation
// ---------------------------------------------------------------------------

#[test]
fn field_mutation_parse() {
    let file = parse_file(&load_fixture("field-mutation.test.tstr"), "field-mutation.test.tstr").unwrap();
    assert_eq!(file.inputs, vec!["headers", "req"]);
    // body assign, headers assign, 2 header mutations, file ref, 2 field overrides, http call = 8
    assert_eq!(file.body.len(), 8);
}

#[test]
fn field_mutation_nested_quoted() {
    let file = parse_file(&load_fixture("field-mutation.test.tstr"), "field-mutation.test.tstr").unwrap();
    // Statement 2: req.headers."content-type" = "text/plain";
    // (index 2: after req.body assign, req.headers assign)
    match &file.body[2] {
        Statement::Assignment {
            target: AssignTarget::FieldAccess { object, path },
            value: Expr::StringLiteral(val),
        } => {
            assert_eq!(object, "req");
            assert_eq!(path, &vec![
                PropertyKey::Name("headers".to_string()),
                PropertyKey::Quoted("content-type".to_string()),
            ]);
            assert_eq!(val, "text/plain");
        }
        other => panic!("expected quoted field mutation, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Whitespace insensitivity
// ---------------------------------------------------------------------------

#[test]
fn whitespace_insensitive_parse() {
    let file = parse_file(&load_fixture("whitespace-insensitive.test.tstr"), "whitespace-insensitive.test.tstr").unwrap();
    // One-liner: http call + assign + assertion = 3
    // Multi-line http call = 1
    // Extra whitespace assign = 1
    // Tab assign = 1
    // Total = 6
    assert_eq!(file.body.len(), 6);
}

// ---------------------------------------------------------------------------
// Multi-service (URL prefix switching)
// ---------------------------------------------------------------------------

#[test]
fn multi_service_parse() {
    let file = parse_file(&load_fixture("multi-service.test.tstr"), "multi-service.test.tstr").unwrap();
    assert_eq!(file.inputs, vec!["profile", "commerce", "req", "req2"]);
    assert_eq!(file.outputs, vec!["accountId", "orderId"]);
    // urlPrefix assign, body assign, http, accountId assign,
    // ...8 statements + return = 9
    assert_eq!(file.body.len(), 9);
}

// ---------------------------------------------------------------------------
// String escapes
// ---------------------------------------------------------------------------

#[test]
fn string_escapes_parse() {
    let file = parse_file(&load_fixture("string-escapes.test.tstr"), "string-escapes.test.tstr").unwrap();
    // msg, tabStr, newlineStr, backslash, url = 5
    assert_eq!(file.body.len(), 5);
}

#[test]
fn string_escape_quotes() {
    let file = parse_file(&load_fixture("string-escapes.test.tstr"), "string-escapes.test.tstr").unwrap();
    match &file.body[0] {
        Statement::Assignment { value: Expr::StringLiteral(s), .. } => {
            assert_eq!(s, "he said \"hello\"");
        }
        other => panic!("expected escaped quote string, got {:?}", other),
    }
}
