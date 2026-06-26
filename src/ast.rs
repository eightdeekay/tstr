/// A parsed .tstr file — the top-level unit.
/// Files are blocks: optional inputs (-->), a body of statements, optional outputs (<--).
#[derive(Debug, PartialEq, Clone)]
pub struct File {
    pub file_type: FileType,
    /// Header-region metadata block (`key: value` lines above the function
    /// block). See `Metadata`.
    pub metadata: Metadata,
    pub inputs: Vec<String>,
    pub body: Vec<Statement>,
    pub outputs: Vec<String>,
    /// Maps each statement index to its source line number (1-indexed).
    /// Empty if not populated (e.g., in tests that construct File directly).
    pub line_map: Vec<usize>,
}

impl File {
    /// If this file is turned off via `disabled:`, return the reason. Single
    /// source of truth for both the runner (to skip execution) and
    /// `tstr list --disabled`.
    pub fn disabled_reason(&self) -> Option<&str> {
        self.metadata.disabled.as_deref()
    }
}

/// The header-region metadata block — `key: value` lines that sit above the
/// function block (param header + braced body). Static, file-level config;
/// HTTP-header-like, no sigil, value = rest-of-line (trimmed, unquoted).
/// See TODO.md "File metadata block" and the README "Metadata block" section.
#[derive(Debug, PartialEq, Clone, Default)]
pub struct Metadata {
    /// `requires:` — minimum tstr version requirement, raw (e.g. `">= 0.5.3"`).
    pub requires: Option<String>,
    /// `disabled:` — reason the file is turned off. Unconditional "don't run,
    /// fix postponed"; the runner short-circuits before any statement executes
    /// and reports a distinct DISABLED status.
    pub disabled: Option<String>,
    /// `blast-radius:` — how far this file's collateral extends, forward, within
    /// its leaf.
    pub blast_radius: Option<BlastRadius>,
}

/// The span a `disabled:`/failed file's collateral skip covers. Forward-only,
/// leaf-local. See TODO.md for the full semantics.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum BlastRadius {
    /// `N` — the next N tests in the leaf (saturates at remaining).
    Count(u32),
    /// `all` / `*` — every remaining test in the leaf.
    All,
    /// `<=PREFIX` — through the file whose name matches filename-prefix `PREFIX`.
    /// Parsed now; runtime not yet wired (future addition per the spec).
    Through(String),
}

/// Determined by the middle extension: create-group.test.tstr, values.const.tstr, etc.
#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum FileType {
    Test,
    Fetch,
    Setup,
    Cleanup,
    Const,
    Exporter,
    /// `name.lib.tstr` — callable function. Not auto-scheduled by the runner;
    /// invoked explicitly from test code as `name(args...)` or `recv.name(args...)`.
    /// Inputs declared via the existing `name1, name2 -->` header become its params.
    Lib,
}

/// Every line in the body is one of these.
#[derive(Debug, PartialEq, Clone)]
pub enum Statement {
    /// `variable = expression`
    Assignment {
        target: AssignTarget,
        value: Expr,
    },
    /// `expression | "failure message"`
    Assertion {
        expr: Expr,
        message: String,
    },
    /// `r = req.get("/url") ? 2xx | "failed"`  (UFCS form, idiomatic)
    /// `r = get(req, "/url") ? 2xx | "failed"` (function form, equivalent)
    /// The request object is required — there is no magic `_req` fallback.
    HttpCall {
        target: String,
        method: HttpMethod,
        url: Expr,
        request_obj: Expr,
        status_check: Option<StatusCheck>,
    },
    /// `if cond { ... } else if cond { ... } else { ... }` — conditional
    /// execution. The condition picks a branch; the chosen branch's statements
    /// run in the current scope. `else if` is represented as an `else_body`
    /// holding a single nested `If`. Each branch carries a parallel line map
    /// (same convention as `File.line_map`) so a failing assertion inside a
    /// branch reports its own source line, not the enclosing `if`'s.
    ///
    /// Replaces the old `exitIf` guard-clause: "do X only when cond" is a
    /// conditional, not an early-exit, and `if` doesn't poison sibling files
    /// the way an `exitIf`-skip in a setup did (via the runner's block cascade).
    If {
        condition: Expr,
        then_body: Vec<Statement>,
        then_lines: Vec<usize>,
        else_body: Vec<Statement>,
        else_lines: Vec<usize>,
    },
    /// `eval { ... }` or `js:{ ... }`
    JsBlock {
        code: String,
    },
    /// Standalone expression statement: `items.each({ ... });`
    ExprStatement {
        expr: Expr,
    },
    /// `matrix sites = [ "Site A": { ... }, "Site B": { ... } ];`
    Matrix {
        name: String,
        entries: Vec<MatrixEntry>,
    },
    /// `retry(max: 10, interval: 500ms, timeout: 30s) { ... }` — re-run the
    /// body until every assertion inside it passes, or the bounds are reached.
    /// Built for eventual-consistency gaps (e.g. POST to A fires a Kafka
    /// message that B consumes asynchronously, so a GET on B only reflects the
    /// change after some delay). The body fails fast within an attempt: the
    /// first failing `|` assertion (or HTTP status/connection error, which the
    /// evaluator already surfaces as an assertion failure) is the retry
    /// trigger. A clean pass stops immediately; exhausting the bounds reports
    /// the last failure annotated with the attempt count and elapsed time.
    ///
    /// At least one of `max`/`timeout` is required (otherwise the loop is
    /// unbounded) — the parser enforces this. `interval_ms` defaults to 250.
    Retry {
        /// Total attempts including the first. `None` means bounded only by time.
        max: Option<u32>,
        /// Delay between attempts, in milliseconds.
        interval_ms: u64,
        /// Wall-clock cap in milliseconds. `None` means bounded only by `max`.
        timeout_ms: Option<u64>,
        body: Vec<Statement>,
        /// Per-statement source lines for `body` (same convention as
        /// `File.line_map`), so an exhausted retry reports the inner failing
        /// assertion's line rather than the `retry` statement's.
        body_lines: Vec<usize>,
    },
    /// `export a, b, expr as name, ...;` — publish named bindings into the
    /// file's exports (ambient broadcast for setup/test/const). Non-terminating;
    /// may appear multiple times, each merging more bindings. The parser
    /// desugars the item list to a `JsonObject` whose keys are the export names.
    Export {
        value: Expr,
    },
    /// `return expr;` or bare `return;` — a single value that terminates
    /// execution: a lib call's result, a lambda block's yield, or an early exit.
    /// Publishing to ambient scope is `export`, not `return`.
    Return {
        value: Option<Expr>,
    },
}

/// A single entry in a matrix definition — a label and an object expression.
#[derive(Debug, PartialEq, Clone)]
pub struct MatrixEntry {
    pub label: String,
    pub value: Expr,
}

/// The left-hand side of an assignment: simple variable or field mutation.
#[derive(Debug, PartialEq, Clone)]
pub enum AssignTarget {
    /// `myVar = ...`
    Variable(String),
    /// `obj.field = ...` or `obj."hyphenated" = ...`
    FieldAccess {
        object: String,
        path: Vec<PropertyKey>,
    },
}

#[derive(Debug, PartialEq, Clone)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

/// The `? 2xx 201 | "message"` part of an HTTP call.
#[derive(Debug, PartialEq, Clone)]
pub struct StatusCheck {
    pub patterns: Vec<StatusPattern>,
    pub message: String,
}

#[derive(Debug, PartialEq, Clone)]
pub enum StatusPattern {
    /// `200`
    Exact(u16),
    /// `2xx`
    Wildcard(u8),
    /// `200-204`
    Range(u16, u16),
    /// `>=200`, `<500`
    Comparison(CompOp, u16),
}

#[derive(Debug, PartialEq, Clone)]
pub enum CompOp {
    Gt,
    Lt,
    Gte,
    Lte,
}

/// Expressions — the core of the language.
#[derive(Debug, PartialEq, Clone)]
pub enum Expr {
    /// `null`
    Null,
    /// `true`, `false`
    Bool(bool),
    /// `200`, `3.14`
    Number(f64),
    /// `"hello"`
    StringLiteral(String),
    /// `myVar`
    Identifier(String),
    /// `@fixtures/group.json`
    FileRef(String),
    /// `{{varName}}`
    Interpolated(String),
    /// `${name}` or `${name.sub.field}` — reference to the constants namespace
    /// (project-wide yaml `constants:` plus dir-scoped `.const.tstr` returns).
    ConstantRef(String),
    /// `r.id`, `r."content-type"`, `r.items[0]`
    PropertyAccess {
        object: Box<Expr>,
        key: PropertyKey,
    },
    /// `r?.field` — null-safe access
    OptionalAccess {
        object: Box<Expr>,
        key: PropertyKey,
    },
    /// `r.items[0]`, `r.items[-1]`, `r.items[0:3]`
    IndexAccess {
        object: Box<Expr>,
        index: Box<IndexOp>,
    },
    /// `r.items[].id` — collect field from all elements
    CollectAccess {
        object: Box<Expr>,
        key: PropertyKey,
    },
    /// `a == b`, `a != b`, `a > b`, etc.
    BinaryOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    /// `!expr`
    Not(Box<Expr>),
    /// `expr | "failure message"` — assertion / null guard
    Guard {
        expr: Box<Expr>,
        message: String,
    },
    /// `{ ... }` — inline JSON object
    JsonObject(Vec<(String, Expr)>),
    /// `[ ... ]` — inline JSON array
    JsonArray(Vec<Expr>),
    /// `method("url")` or `method("url", req)` — HTTP call as expression (for chaining)
    HttpCallExpr {
        method: HttpMethod,
        url: Box<Expr>,
        request_obj: Option<Box<Expr>>,
    },
    /// `js:{ code }` — opaque JavaScript block
    JsExpr(String),
    /// `{ inputs --> body <-- outputs }` — tstr block
    Block {
        inputs: Vec<String>,
        body: Vec<Statement>,
        outputs: Vec<String>,
    },
    /// `collection.map(block)`, `collection.each(block)`
    MethodCall {
        object: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
    /// `$.uuid()`, `$.string(10)`, `$.randEmail()` — built-in functions
    BuiltinCall {
        name: String,
        args: Vec<Expr>,
    },
    /// `createTag(name, type)` — call into the lib namespace.
    /// Reserved HTTP verbs (get/post/put/patch/delete/head/options) do
    /// not parse as LibCall; they fall through to HTTP-call parsing.
    LibCall {
        name: String,
        args: Vec<Expr>,
    },
    /// `collection | any(.field == val)`, `collection | all(.field != null)`
    PipeOp {
        left: Box<Expr>,
        op: PipeFunc,
    },
}

/// Property keys — plain identifiers or quoted strings for special characters.
#[derive(Debug, PartialEq, Clone)]
pub enum PropertyKey {
    /// `.fieldName`
    Name(String),
    /// `."hyphenated-name"`
    Quoted(String),
}

/// Array/slice indexing.
#[derive(Debug, PartialEq, Clone)]
pub enum IndexOp {
    /// `[0]`, `[-1]`
    Single(i64),
    /// `[0:3]`
    Slice(Option<i64>, Option<i64>),
}

#[derive(Debug, PartialEq, Clone)]
pub enum BinOp {
    // Comparison
    Eq,
    NotEq,
    Gt,
    Lt,
    Gte,
    Lte,
    // Logical
    And,
    Or,
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    // Regex
    RegexExtract, // ~
    RegexTest,    // ~?
    RegexNoMatch, // !~
}

/// Pipe functions: `| any(...)`, `| all(...)`
#[derive(Debug, PartialEq, Clone)]
pub enum PipeFunc {
    Any(Box<Expr>),
    All(Box<Expr>),
}