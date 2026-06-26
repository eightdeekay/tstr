use std::cell::RefCell;
use std::collections::HashMap;

use regex::Regex;

use crate::ast::*;
use crate::value::Value;

/// Runtime error during evaluation.
#[derive(Debug)]
pub struct EvalError {
    pub message: String,
}

impl EvalError {
    pub fn new(msg: impl Into<String>) -> Self {
        EvalError { message: msg.into() }
    }
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Test assertion failure — distinct from runtime errors.
#[derive(Debug)]
pub struct AssertionFailure {
    pub message: String,
    /// Source line of the failing statement, if known. Set at the deepest
    /// point (inside `if`/`retry` branch bodies), so a failure carries its own
    /// line rather than the enclosing statement's. `exec_file` formats the
    /// `line N:` prefix once, preferring this over the top-level line map.
    pub line: Option<usize>,
}

impl AssertionFailure {
    /// A failure with no line attached yet (the common case at creation —
    /// the executing context fills the line in as the failure bubbles up).
    pub fn new(message: impl Into<String>) -> Self {
        AssertionFailure { message: message.into(), line: None }
    }
}

/// Variable scope — holds named values, with optional parent for inheritance.
#[derive(Debug)]
pub struct Scope {
    vars: HashMap<String, Value>,
    /// Provenance for each variable: where it came from (e.g. "cli", "const:foo.const.tstr",
    /// "matrix:Site A", "test:01-create.test.tstr"). Missing entry = source unknown / set in-file.
    sources: HashMap<String, String>,
    /// Immutable project-wide constants namespace, accessed via `${name}` syntax.
    /// Sourced from yaml `constants:` and (future) `.const.tstr` returns.
    /// Arc so cloning a scope doesn't deep-copy the map.
    constants: std::sync::Arc<HashMap<String, Value>>,
    /// Libraries visible to this scope, by name. Populated per-file from the
    /// FileIndex according to the lib resolution rule (dir chain + lib/ subtrees).
    /// Looked up by `Expr::LibCall` and `Expr::MethodCall` (UFCS).
    libs: std::sync::Arc<HashMap<String, std::sync::Arc<crate::ast::File>>>,
    /// Suite root, used to resolve relative `@file` references. Threaded through
    /// the scope (not the process cwd) so resolution is location-independent and
    /// safe under the concurrent runner. `None` falls back to cwd-relative.
    base_dir: Option<std::sync::Arc<std::path::PathBuf>>,
    logs: RefCell<Vec<String>>,
    last_endpoint: RefCell<Option<String>>,
}

impl Clone for Scope {
    fn clone(&self) -> Self {
        Scope {
            vars: self.vars.clone(),
            sources: self.sources.clone(),
            constants: std::sync::Arc::clone(&self.constants),
            libs: std::sync::Arc::clone(&self.libs),
            base_dir: self.base_dir.clone(),
            logs: RefCell::new(self.logs.borrow().clone()),
            last_endpoint: RefCell::new(self.last_endpoint.borrow().clone()),
        }
    }
}

impl Scope {
    pub fn new() -> Self {
        Scope {
            vars: HashMap::new(),
            sources: HashMap::new(),
            constants: std::sync::Arc::new(HashMap::new()),
            libs: std::sync::Arc::new(HashMap::new()),
            base_dir: None,
            logs: RefCell::new(Vec::new()),
            last_endpoint: RefCell::new(None),
        }
    }

    pub fn with_vars(vars: HashMap<String, Value>) -> Self {
        Scope {
            vars,
            sources: HashMap::new(),
            constants: std::sync::Arc::new(HashMap::new()),
            libs: std::sync::Arc::new(HashMap::new()),
            base_dir: None,
            logs: RefCell::new(Vec::new()),
            last_endpoint: RefCell::new(None),
        }
    }

    /// Attach the suite root used to resolve relative `@file` references.
    pub fn with_base_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.base_dir = Some(std::sync::Arc::new(dir));
        self
    }

    /// The suite root for resolving relative `@file` references, if set.
    pub fn base_dir(&self) -> Option<&std::path::Path> {
        self.base_dir.as_deref().map(|p| p.as_path())
    }

    /// Attach a constants namespace (yaml constants + future .const.tstr returns).
    /// Replaces any previously-set constants. Use Arc so subsequent clones are cheap.
    pub fn with_constants(mut self, constants: std::sync::Arc<HashMap<String, Value>>) -> Self {
        self.constants = constants;
        self
    }

    /// Attach a lib namespace (name → File AST) for this scope. Cloning the
    /// scope shares the Arc; mutations to vars/sources are scope-local.
    pub fn with_libs(mut self, libs: std::sync::Arc<HashMap<String, std::sync::Arc<crate::ast::File>>>) -> Self {
        self.libs = libs;
        self
    }

    pub fn lookup_lib(&self, name: &str) -> Option<std::sync::Arc<crate::ast::File>> {
        self.libs.get(name).cloned()
    }

    /// Look up a constant by dotted path. `${orgService.baseUrl}` → ["orgService", "baseUrl"].
    /// Returns Err if the top-level name isn't in the constants namespace.
    /// Returns Null if a sub-key doesn't exist (consistent with normal field access).
    pub fn lookup_constant(&self, path: &str) -> Result<Value, String> {
        let mut parts = path.split('.');
        let head = parts.next().ok_or_else(|| "empty constant reference".to_string())?;
        let mut value = self.constants.get(head)
            .cloned()
            .ok_or_else(|| format!("unknown constant '${{{}}}'", head))?;
        for key in parts {
            value = value.get_field(key);
        }
        Ok(value)
    }

    pub fn get(&self, name: &str) -> Value {
        self.vars.get(name).cloned().unwrap_or(Value::Null)
    }

    pub fn set(&mut self, name: String, value: Value) {
        self.vars.insert(name, value);
    }

    pub fn set_with_source(&mut self, name: String, value: Value, source: String) {
        self.sources.insert(name.clone(), source);
        self.vars.insert(name, value);
    }

    pub fn source_of(&self, name: &str) -> Option<String> {
        self.sources.get(name).cloned()
    }

    /// Snapshot all vars with their sources. Used for end-of-run summary.
    pub fn snapshot(&self) -> Vec<(String, Option<String>, Value)> {
        let mut out: Vec<_> = self.vars.iter()
            .filter(|(k, _)| k.as_str() != "_in" && k.as_str() != "_out" && !k.starts_with("req"))
            .map(|(k, v)| (k.clone(), self.sources.get(k).cloned(), v.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn contains(&self, name: &str) -> bool {
        self.vars.contains_key(name)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.vars.keys()
    }

    pub fn add_log(&self, msg: String) {
        self.logs.borrow_mut().push(msg);
    }

    pub fn take_logs(&self) -> Vec<String> {
        std::mem::take(&mut self.logs.borrow_mut())
    }

    pub fn set_endpoint(&self, endpoint: String) {
        *self.last_endpoint.borrow_mut() = Some(endpoint);
    }

    pub fn last_endpoint(&self) -> Option<String> {
        self.last_endpoint.borrow().clone()
    }
}

/// Evaluate an expression in the given scope.
pub fn eval_expr(expr: &Expr, scope: &Scope) -> Result<Value, EvalError> {
    match expr {
        Expr::Null => Ok(Value::Null),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Number(n) => Ok(Value::Number(*n)),
        Expr::StringLiteral(s) => Ok(Value::String(interpolate_string(s, scope)?)),
        Expr::Identifier(name) => {
            if scope.contains(name) {
                Ok(scope.get(name))
            } else if name.starts_with('_') {
                // Runtime/legacy namespace (_in, _out, _response, _req, _item):
                // may be conditionally present (e.g. _response before a call),
                // so tolerate as null rather than erroring.
                Ok(scope.get(name))
            } else {
                // Fail loud on an unknown identifier — a typo or a dangling
                // reference is almost always a bug in a test. (Constants are
                // referenced as ${name}, not bare.)
                Err(EvalError::new(format!(
                    "undefined variable '{}' — not in scope. Set it via assignment, a setup \
                     `return`, or `--set`; for a yaml constant use ${{{}}}.",
                    name, name
                )))
            }
        }

        Expr::FileRef(path) => {
            load_file_ref(path, scope.base_dir())
        }

        Expr::Interpolated(name) => Ok(scope.get(name)),

        Expr::ConstantRef(path) => {
            scope.lookup_constant(path).map_err(EvalError::new)
        }

        Expr::JsonObject(entries) => {
            let mut map = HashMap::new();
            for (key, val_expr) in entries {
                let val = eval_expr(val_expr, scope)?;
                map.insert(key.clone(), val);
            }
            Ok(Value::Object(map))
        }

        Expr::JsonArray(items) => {
            let mut arr = Vec::new();
            for item_expr in items {
                arr.push(eval_expr(item_expr, scope)?);
            }
            Ok(Value::Array(arr))
        }

        Expr::PropertyAccess { object, key } => {
            let obj = eval_expr(object, scope)?;
            let field_name = property_key_str(key);
            Ok(obj.get_field(&field_name))
        }

        Expr::OptionalAccess { object, key } => {
            let obj = eval_expr(object, scope)?;
            if obj == Value::Null {
                Ok(Value::Null)
            } else {
                let field_name = property_key_str(key);
                Ok(obj.get_field(&field_name))
            }
        }

        Expr::IndexAccess { object, index } => {
            let obj = eval_expr(object, scope)?;
            match index.as_ref() {
                IndexOp::Single(idx) => Ok(obj.get_index(*idx)),
                IndexOp::Slice(start, end) => Ok(obj.slice(*start, *end)),
            }
        }

        Expr::CollectAccess { object, key } => {
            let obj = eval_expr(object, scope)?;
            let field_name = property_key_str(key);
            Ok(obj.collect_field(&field_name))
        }

        Expr::Not(inner) => {
            let val = eval_expr(inner, scope)?;
            Ok(Value::Bool(!val.is_truthy()))
        }

        Expr::BinaryOp { left, op, right } => {
            eval_binary_op(left, op, right, scope)
        }

        Expr::Guard { expr, message } => {
            let val = eval_expr(expr, scope)?;
            if val.is_truthy() {
                Ok(val)
            } else {
                Err(EvalError::new(interpolate_string(message, scope)?))
            }
        }

        Expr::JsExpr(_code) => {
            // TODO: boa integration
            Err(EvalError::new("js:{} blocks not yet implemented"))
        }

        Expr::Block { inputs, body, outputs } => {
            // Evaluate block in its own scope
            let mut block_scope = scope.clone();
            // Inputs are expected to be set by the caller (e.g., .filter(), .map())
            // For standalone blocks, inputs come from current scope
            for name in inputs {
                if !block_scope.contains(name) {
                    block_scope.set(name.clone(), scope.get(name));
                }
            }

            let mut last_value = Value::Null;
            for stmt in body {
                match exec_statement(stmt, &mut block_scope)? {
                    StmtResult::Ok => {}
                    StmtResult::AssertionFailed(f) => {
                        return Err(EvalError::new(f.message));
                    }
                    StmtResult::MatrixDef(_) => {
                        return Err(EvalError::new("matrix statements are only allowed in const files"));
                    }
                    StmtResult::Return(v) => {
                        // `return` inside a block expression terminates the
                        // block with that value.
                        return Ok(v);
                    }
                }
            }

            // If there are explicit outputs, return them as an object
            if !outputs.is_empty() {
                if outputs.len() == 1 {
                    last_value = block_scope.get(&outputs[0]);
                } else {
                    let mut map = HashMap::new();
                    for name in outputs {
                        map.insert(name.clone(), block_scope.get(name));
                    }
                    last_value = Value::Object(map);
                }
            }

            Ok(last_value)
        }

        Expr::HttpCallExpr { .. } => {
            Err(EvalError::new("HTTP call expressions not yet implemented"))
        }

        Expr::MethodCall { object, method, args } => {
            eval_method_call(object, method, args, scope)
        }

        Expr::BuiltinCall { name, args } => {
            eval_builtin(name, args, scope)
        }

        Expr::LibCall { name, args } => {
            let lib = scope.lookup_lib(name)
                .ok_or_else(|| EvalError::new(format!("unknown lib '{}'", name)))?;
            let arg_vals: Vec<Value> = args.iter()
                .map(|a| eval_expr(a, scope))
                .collect::<Result<_, _>>()?;
            invoke_lib(name, &lib, arg_vals, scope)
        }

        Expr::PipeOp { left, op } => {
            let collection = eval_expr(left, scope)?;
            let arr = match &collection {
                Value::Array(a) => a,
                _ => return Err(EvalError::new(format!(
                    "pipe operator requires an array, got {}", collection.type_name()
                ))),
            };

            match op {
                PipeFunc::Any(predicate) => {
                    for item in arr {
                        if eval_pipe_predicate(predicate, item, scope)? {
                            return Ok(Value::Bool(true));
                        }
                    }
                    Ok(Value::Bool(false))
                }
                PipeFunc::All(predicate) => {
                    for item in arr {
                        if !eval_pipe_predicate(predicate, item, scope)? {
                            return Ok(Value::Bool(false));
                        }
                    }
                    Ok(Value::Bool(true))
                }
            }
        }
    }
}

fn eval_binary_op(left: &Expr, op: &BinOp, right: &Expr, scope: &Scope) -> Result<Value, EvalError> {
    let lval = eval_expr(left, scope)?;

    // Short-circuit for logical operators
    match op {
        BinOp::And => {
            if !lval.is_truthy() {
                return Ok(Value::Bool(false));
            }
            let rval = eval_expr(right, scope)?;
            return Ok(Value::Bool(rval.is_truthy()));
        }
        BinOp::Or => {
            if lval.is_truthy() {
                return Ok(Value::Bool(true));
            }
            let rval = eval_expr(right, scope)?;
            return Ok(Value::Bool(rval.is_truthy()));
        }
        _ => {}
    }

    let rval = eval_expr(right, scope)?;

    match op {
        // Comparison
        BinOp::Eq => Ok(Value::Bool(lval == rval)),
        BinOp::NotEq => Ok(Value::Bool(lval != rval)),
        BinOp::Gt => Ok(Value::Bool(lval > rval)),
        BinOp::Lt => Ok(Value::Bool(lval < rval)),
        BinOp::Gte => Ok(Value::Bool(lval >= rval)),
        BinOp::Lte => Ok(Value::Bool(lval <= rval)),

        // Arithmetic
        BinOp::Add => eval_add(&lval, &rval),
        BinOp::Sub => eval_numeric_op(&lval, &rval, |a, b| a - b, "-"),
        BinOp::Mul => eval_numeric_op(&lval, &rval, |a, b| a * b, "*"),
        BinOp::Div => {
            if let (Value::Number(_), Value::Number(b)) = (&lval, &rval) {
                if *b == 0.0 {
                    return Err(EvalError::new("division by zero"));
                }
            }
            eval_numeric_op(&lval, &rval, |a, b| a / b, "/")
        }
        BinOp::Mod => eval_numeric_op(&lval, &rval, |a, b| a % b, "%"),

        // Regex
        BinOp::RegexTest => {
            let text = lval.to_display_string();
            let pattern = match &rval {
                Value::String(s) => s,
                _ => return Err(EvalError::new("regex pattern must be a string")),
            };
            match regex_match(&text, pattern) {
                Ok(Some(_)) => Ok(Value::Bool(true)),
                Ok(None) => Ok(Value::Bool(false)),
                Err(e) => Err(e),
            }
        }
        BinOp::RegexExtract => {
            let text = lval.to_display_string();
            let pattern = match &rval {
                Value::String(s) => s,
                _ => return Err(EvalError::new("regex pattern must be a string")),
            };
            match regex_match(&text, pattern) {
                Ok(Some(capture)) => Ok(Value::String(capture)),
                Ok(None) => Ok(Value::Null),
                Err(e) => Err(e),
            }
        }
        BinOp::RegexNoMatch => {
            let text = lval.to_display_string();
            let pattern = match &rval {
                Value::String(s) => s,
                _ => return Err(EvalError::new("regex pattern must be a string")),
            };
            match regex_match(&text, pattern) {
                Ok(Some(_)) => Ok(Value::Bool(false)),
                Ok(None) => Ok(Value::Bool(true)),
                Err(e) => Err(e),
            }
        }

        BinOp::And | BinOp::Or => unreachable!(), // handled above
    }
}

/// Add: numbers add, strings concatenate.
fn eval_add(left: &Value, right: &Value) -> Result<Value, EvalError> {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
        (Value::String(a), Value::String(b)) => Ok(Value::String(format!("{}{}", a, b))),
        (Value::String(a), other) => Ok(Value::String(format!("{}{}", a, other.to_display_string()))),
        (other, Value::String(b)) => Ok(Value::String(format!("{}{}", other.to_display_string(), b))),
        _ => Err(EvalError::new(format!(
            "cannot add {} and {}", left.type_name(), right.type_name()
        ))),
    }
}

fn eval_numeric_op(
    left: &Value, right: &Value,
    op: fn(f64, f64) -> f64,
    op_name: &str,
) -> Result<Value, EvalError> {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(op(*a, *b))),
        _ => Err(EvalError::new(format!(
            "cannot {} {} and {}", op_name, left.type_name(), right.type_name()
        ))),
    }
}

/// Regex matching. Returns the first capture group if present, otherwise the full match.
fn regex_match(text: &str, pattern: &str) -> Result<Option<String>, EvalError> {
    let re = Regex::new(pattern)
        .map_err(|e| EvalError::new(format!("invalid regex '{}': {}", pattern, e)))?;

    match re.captures(text) {
        Some(caps) => {
            // If there's a capture group, return it; otherwise return the full match
            if caps.len() > 1 {
                Ok(Some(caps.get(1).unwrap().as_str().to_string()))
            } else {
                Ok(Some(caps.get(0).unwrap().as_str().to_string()))
            }
        }
        None => Ok(None),
    }
}

/// Interpolate `{{varName}}` references in a string. (public for http module)
pub fn interpolate_string_pub(s: &str, scope: &Scope) -> Result<String, EvalError> {
    interpolate_string(s, scope)
}

/// Interpolate `{{varName}}` and `{{obj.field}}` references in a string.
/// Errors if the top-level identifier of any reference is not defined in scope.
fn interpolate_string(s: &str, scope: &Scope) -> Result<String, EvalError> {
    let mut result = String::new();
    let mut remaining = s;

    while let Some(start) = remaining.find("{{") {
        result.push_str(&remaining[..start]);
        let after_open = &remaining[start + 2..];
        if let Some(end) = after_open.find("}}") {
            let var_expr = after_open[..end].trim();
            let val = resolve_dotted_path(var_expr, scope)?;
            result.push_str(&val.to_display_string());
            remaining = &after_open[end + 2..];
        } else {
            // No closing }}, just pass through
            result.push_str("{{");
            remaining = after_open;
        }
    }
    result.push_str(remaining);
    Ok(result)
}

/// Resolve a dotted path inside a `{{...}}` interpolation. Resolves against
/// ambient scope first, then falls back to the constants namespace — so
/// `{{accountId}}` works whether `accountId` is a setup-published ambient
/// variable or a yaml `constants:` entry. Errors if neither has it.
fn resolve_dotted_path(path: &str, scope: &Scope) -> Result<Value, EvalError> {
    let parts: Vec<&str> = path.split('.').collect();
    let head = parts[0];
    if scope.contains(head) {
        let mut val = scope.get(head);
        for part in &parts[1..] {
            val = val.get_field(part);
        }
        return Ok(val);
    }
    // Fall back to the constants namespace (same source as bare `${name}`).
    if let Ok(val) = scope.lookup_constant(path) {
        return Ok(val);
    }
    Err(EvalError::new(format!(
        "undefined variable in interpolation: '{{{{{}}}}}' — '{}' is not in ambient scope \
         or the constants namespace (set it via a setup `return`, `--set`, or yaml `constants:`)",
        path, head
    )))
}

/// Evaluate a method call: .filter(), .find(), .map(), .each()
fn eval_method_call(object: &Expr, method: &str, args: &[Expr], scope: &Scope) -> Result<Value, EvalError> {
    let obj = eval_expr(object, scope)?;

    match method {
        "filter" => {
            let arr = expect_array(&obj, "filter")?;
            let predicate = expect_block_arg(args, "filter")?;
            let mut result = Vec::new();
            for item in arr {
                if eval_block_predicate(predicate, item, scope)? {
                    result.push(item.clone());
                }
            }
            Ok(Value::Array(result))
        }

        "find" => {
            let arr = expect_array(&obj, "find")?;
            let predicate = expect_block_arg(args, "find")?;
            for item in arr {
                if eval_block_predicate(predicate, item, scope)? {
                    return Ok(item.clone());
                }
            }
            Ok(Value::Null)
        }

        "map" => {
            let arr = expect_array(&obj, "map")?;
            let block = expect_block_arg(args, "map")?;
            let mut result = Vec::new();
            for item in arr {
                let val = eval_block_transform(block, item, scope)?;
                result.push(val);
            }
            Ok(Value::Array(result))
        }

        "each" => {
            let arr = expect_array(&obj, "each")?;
            let block = expect_block_arg(args, "each")?;
            for item in arr {
                eval_block_predicate(block, item, scope)?;
            }
            Ok(Value::Null)
        }

        _ => {
            // UFCS dispatch: if the method name matches a visible lib, call it
            // with the object as the first argument. This makes `req.createOrg(name)`
            // identical to `createOrg(req, name)` when `createOrg` is a lib.
            if let Some(lib) = scope.lookup_lib(method) {
                let mut all_args: Vec<Value> = Vec::with_capacity(args.len() + 1);
                all_args.push(obj);
                for a in args {
                    all_args.push(eval_expr(a, scope)?);
                }
                return invoke_lib(method, &lib, all_args, scope);
            }
            Err(EvalError::new(format!("unknown method '.{}()'", method)))
        }
    }
}

/// Execute a library function: build a fresh scope (constants + lib namespace
/// inherited, ambient vars NOT), bind params, run body, return its outputs.
///
/// Outputs come from `_out` writes (current legacy mechanism). Once `return`
/// lands as a statement, that becomes the primary form.
fn invoke_lib(
    name: &str,
    lib: &std::sync::Arc<crate::ast::File>,
    args: Vec<Value>,
    caller_scope: &Scope,
) -> Result<Value, EvalError> {
    let params = &lib.inputs;
    if args.len() != params.len() {
        return Err(EvalError::new(format!(
            "lib '{}' expects {} arg(s), got {}", name, params.len(), args.len()
        )));
    }

    // Fresh scope. Constants + libs inherited (libs available so nested calls
    // work); ambient vars are NOT — libs are self-contained per design.
    let mut lib_scope = Scope::new()
        .with_constants(std::sync::Arc::clone(&caller_scope.constants))
        .with_libs(std::sync::Arc::clone(&caller_scope.libs));
    // `@file` references resolve against the same suite root in a lib as at the
    // call site (root-relative, not lib-relative).
    lib_scope.base_dir = caller_scope.base_dir.clone();

    // Bind params to argument values.
    for (param, val) in params.iter().zip(args.into_iter()) {
        lib_scope.set(param.clone(), val);
    }
    // `_out` accumulates the lib's `export` bindings — that object is the lib's
    // value at the call site.
    lib_scope.set("_out".to_string(), Value::Object(HashMap::new()));

    // Execute statements. A top-level `return;` is void — it just halts; the
    // lib's value is its exports (collected below).
    for stmt in &lib.body {
        match exec_statement(stmt, &mut lib_scope)? {
            StmtResult::Ok => {}
            StmtResult::Return(_) => break,
            StmtResult::AssertionFailed(f) => {
                return Err(EvalError::new(format!("in lib '{}': {}", name, f.message)));
            }
            StmtResult::MatrixDef(_) => {
                return Err(EvalError::new(format!(
                    "matrix statements not allowed in libs ('{}')", name
                )));
            }
        }
    }

    // The lib's value is its exports, narrowed to declared outputs if any.
    let out_obj = lib_scope.get("_out");
    if lib.outputs.is_empty() {
        Ok(out_obj)
    } else {
        let mut narrowed = HashMap::new();
        if let Value::Object(map) = out_obj {
            for k in &lib.outputs {
                if let Some(v) = map.get(k) {
                    narrowed.insert(k.clone(), v.clone());
                }
            }
        }
        Ok(Value::Object(narrowed))
    }
}

fn expect_array<'a>(val: &'a Value, method_name: &str) -> Result<&'a Vec<Value>, EvalError> {
    match val {
        Value::Array(arr) => Ok(arr),
        _ => Err(EvalError::new(format!(
            ".{}() requires an array, got {}", method_name, val.type_name()
        ))),
    }
}

fn expect_block_arg<'a>(args: &'a [Expr], method_name: &str) -> Result<&'a Expr, EvalError> {
    if args.len() != 1 {
        return Err(EvalError::new(format!(
            ".{}() expects one block argument, got {}", method_name, args.len()
        )));
    }
    match &args[0] {
        Expr::Block { .. } => Ok(&args[0]),
        _ => Err(EvalError::new(format!(
            ".{}() argument must be a block", method_name
        ))),
    }
}

/// Evaluate a block as a predicate — returns true/false based on the last expression's truthiness.
/// For predicate blocks, the body is a single Assertion whose expression is the predicate.
/// We evaluate the expression but DON'T fail on false — just return the truthiness.
fn eval_block_predicate(block: &Expr, item: &Value, scope: &Scope) -> Result<bool, EvalError> {
    match block {
        Expr::Block { inputs, body, outputs: _ } => {
            let mut block_scope = scope.clone();
            if let Some(name) = inputs.first() {
                block_scope.set(name.clone(), item.clone());
            }

            let mut last_value = Value::Null;
            for stmt in body {
                match stmt {
                    // In predicate context, assertions are just expressions —
                    // false means "doesn't match," not "test failed"
                    crate::ast::Statement::Assertion { expr, .. } => {
                        last_value = eval_expr(expr, &block_scope)?;
                    }
                    _ => {
                        match exec_statement(stmt, &mut block_scope)? {
                            StmtResult::Ok => {}
                            StmtResult::AssertionFailed(_) => {
                                return Ok(false);
                            }
                            StmtResult::MatrixDef(_) => {
                                return Err(EvalError::new("matrix statements are only allowed in const files"));
                            }
                            StmtResult::Return(v) => {
                                // `return v;` in a predicate block: treat the value's
                                // truthiness as the predicate result.
                                return Ok(v.is_truthy());
                            }
                        }
                    }
                }
            }

            Ok(last_value.is_truthy())
        }
        _ => Err(EvalError::new("expected block")),
    }
}

/// Evaluate a block as a transform — returns the value of the outputs or last expression.
fn eval_block_transform(block: &Expr, item: &Value, scope: &Scope) -> Result<Value, EvalError> {
    match block {
        Expr::Block { inputs, body, outputs } => {
            let mut block_scope = scope.clone();
            if let Some(name) = inputs.first() {
                block_scope.set(name.clone(), item.clone());
            }

            for stmt in body {
                match exec_statement(stmt, &mut block_scope)? {
                    StmtResult::Ok => {}
                    StmtResult::AssertionFailed(f) => {
                        return Err(EvalError::new(f.message));
                    }
                    StmtResult::MatrixDef(_) => {
                        return Err(EvalError::new("matrix statements are only allowed in const files"));
                    }
                    StmtResult::Return(v) => {
                        // `return` inside a block expression terminates the
                        // block with that value.
                        return Ok(v);
                    }
                }
            }

            if !outputs.is_empty() {
                if outputs.len() == 1 {
                    Ok(block_scope.get(&outputs[0]))
                } else {
                    let mut map = HashMap::new();
                    for name in outputs {
                        map.insert(name.clone(), block_scope.get(name));
                    }
                    Ok(Value::Object(map))
                }
            } else {
                Ok(Value::Null)
            }
        }
        _ => Err(EvalError::new("expected block")),
    }
}

/// Evaluate a built-in function call.
fn eval_builtin(name: &str, args: &[Expr], scope: &Scope) -> Result<Value, EvalError> {
    match name {
        "uuid" => {
            if !args.is_empty() {
                return Err(EvalError::new("$.uuid() takes no arguments"));
            }
            Ok(Value::String(generate_uuid_v4()))
        }

        "string" => {
            let len = if args.len() == 1 {
                match eval_expr(&args[0], scope)? {
                    Value::Number(n) => n as usize,
                    _ => return Err(EvalError::new("$.string(length) expects a number")),
                }
            } else if args.is_empty() {
                8 // default length
            } else {
                return Err(EvalError::new("$.string() takes 0 or 1 arguments"));
            };
            Ok(Value::String(generate_random_string(len)))
        }

        "randEmail" => {
            if args.is_empty() {
                let local = generate_random_string(8);
                Ok(Value::String(format!("{}@example.com", local)))
            } else if args.len() == 1 {
                let base = match eval_expr(&args[0], scope)? {
                    Value::String(s) => s,
                    _ => return Err(EvalError::new("$.randEmail(address) expects a string")),
                };
                // Plus-addressed variant: doug+rand@... → split at @
                if let Some(at_pos) = base.find('@') {
                    let local = &base[..at_pos];
                    let domain = &base[at_pos + 1..];
                    let rand = generate_random_string(6);
                    Ok(Value::String(format!("{}+{}@{}", local, rand, domain)))
                } else {
                    let rand = generate_random_string(6);
                    Ok(Value::String(format!("{}+{}@example.com", base, rand)))
                }
            } else {
                Err(EvalError::new("$.randEmail() takes 0 or 1 arguments"))
            }
        }

        "now" => {
            // Unix timestamp in seconds
            use std::time::{SystemTime, UNIX_EPOCH};
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            Ok(Value::Number(secs as f64))
        }

        "log" => {
            let mut parts = Vec::new();
            for arg in args {
                let val = eval_expr(arg, scope)?;
                parts.push(val.to_display_string());
            }
            let msg = parts.join(" ");
            scope.add_log(msg);
            Ok(Value::Null)
        }

        "hmacSha256" => {
            // $.hmacSha256(key, message)            -> lowercase hex digest
            // $.hmacSha256(key, message, encoding)  -> "hex" (default) or "base64"
            if args.len() < 2 || args.len() > 3 {
                return Err(EvalError::new(
                    "$.hmacSha256() takes 2 or 3 arguments (key, message, [encoding])",
                ));
            }
            let key = match eval_expr(&args[0], scope)? {
                Value::String(s) => s,
                _ => return Err(EvalError::new("$.hmacSha256(key, ...) expects key to be a string")),
            };
            let message = match eval_expr(&args[1], scope)? {
                Value::String(s) => s,
                _ => {
                    return Err(EvalError::new(
                        "$.hmacSha256(key, message) expects message to be a string",
                    ))
                }
            };
            let encoding = if args.len() == 3 {
                match eval_expr(&args[2], scope)? {
                    Value::String(s) => s,
                    _ => {
                        return Err(EvalError::new(
                            "$.hmacSha256(key, message, encoding) expects encoding to be a string",
                        ))
                    }
                }
            } else {
                "hex".to_string()
            };
            Ok(Value::String(hmac_sha256(
                key.as_bytes(),
                message.as_bytes(),
                &encoding,
            )?))
        }

        "stripeSign" => {
            // $.stripeSign(secret, payload)             -> "t={now},v1={hex}"
            // $.stripeSign(secret, payload, timestamp)  -> use an explicit timestamp
            // Emulates Stripe's Stripe-Signature header: HMAC-SHA256 over
            // "{timestamp}.{payload}", hex-encoded as the v1 scheme.
            if args.len() < 2 || args.len() > 3 {
                return Err(EvalError::new(
                    "$.stripeSign() takes 2 or 3 arguments (secret, payload, [timestamp])",
                ));
            }
            let secret = match eval_expr(&args[0], scope)? {
                Value::String(s) => s,
                _ => {
                    return Err(EvalError::new(
                        "$.stripeSign(secret, ...) expects secret to be a string",
                    ))
                }
            };
            let payload = match eval_expr(&args[1], scope)? {
                Value::String(s) => s,
                _ => {
                    return Err(EvalError::new(
                        "$.stripeSign(secret, payload) expects payload to be a string",
                    ))
                }
            };
            let timestamp = if args.len() == 3 {
                match eval_expr(&args[2], scope)? {
                    Value::Number(n) => n as i64,
                    Value::String(s) => s.parse::<i64>().map_err(|_| {
                        EvalError::new("$.stripeSign() timestamp must be an integer")
                    })?,
                    _ => {
                        return Err(EvalError::new(
                            "$.stripeSign(secret, payload, timestamp) expects timestamp to be a number",
                        ))
                    }
                }
            } else {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            };
            let signed_payload = format!("{}.{}", timestamp, payload);
            let v1 = hmac_sha256(secret.as_bytes(), signed_payload.as_bytes(), "hex")?;
            Ok(Value::String(format!("t={},v1={}", timestamp, v1)))
        }

        _ => Err(EvalError::new(format!("unknown built-in function '$.{}()'", name))),
    }
}

/// Compute HMAC-SHA256 over `message` keyed by `key`, encoded as `encoding`
/// ("hex" for lowercase hex, "base64" for standard base64).
fn hmac_sha256(key: &[u8], message: &[u8], encoding: &str) -> Result<String, EvalError> {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|e| EvalError::new(format!("$.hmacSha256() invalid key: {}", e)))?;
    mac.update(message);
    let digest = mac.finalize().into_bytes();

    match encoding {
        "hex" => Ok(hex::encode(digest)),
        "base64" => {
            use base64::Engine;
            Ok(base64::engine::general_purpose::STANDARD.encode(digest))
        }
        other => Err(EvalError::new(format!(
            "$.hmacSha256() unknown encoding '{}' (expected 'hex' or 'base64')",
            other
        ))),
    }
}

/// Generate a v4 UUID using simple randomness.
fn generate_uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    // Simple LCG-based random for UUID generation
    let mut state = seed as u64 ^ 0x5DEECE66D;
    let mut bytes = [0u8; 16];
    for b in &mut bytes {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (state >> 33) as u8;
    }

    // Set version (4) and variant (8/9/a/b)
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// Generate a random alphanumeric string of given length.
fn generate_random_string(len: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let chars: Vec<char> = "abcdefghijklmnopqrstuvwxyz0123456789".chars().collect();
    let mut state = seed as u64 ^ 0xDEADBEEF;
    let mut result = String::with_capacity(len);

    for _ in 0..len {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let idx = ((state >> 33) as usize) % chars.len();
        result.push(chars[idx]);
    }

    result
}

/// Load a file reference. JSON files are parsed into objects, everything else is
/// a string. A **relative** `@path` resolves against `base_dir` (the suite root),
/// so references are independent of the process's working directory; an absolute
/// path is used as-is. With no `base_dir`, falls back to cwd-relative.
fn load_file_ref(path: &str, base_dir: Option<&std::path::Path>) -> Result<Value, EvalError> {
    let resolved = match base_dir {
        Some(dir) if std::path::Path::new(path).is_relative() => dir.join(path),
        _ => std::path::PathBuf::from(path),
    };
    let content = std::fs::read_to_string(&resolved)
        .map_err(|e| EvalError::new(format!("cannot load @{}: {}", path, e)))?;

    if path.ends_with(".json") {
        match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(json) => Ok(crate::http::json_to_value(&json)),
            Err(e) => Err(EvalError::new(format!("invalid JSON in @{}: {}", path, e))),
        }
    } else {
        Ok(Value::String(content))
    }
}

/// Evaluate a pipe predicate. The predicate can be:
/// - A block: `{ item --> item.active == true }`
/// - A shorthand expression with `.field` referring to current item: `.active == true`
fn eval_pipe_predicate(predicate: &Expr, item: &Value, scope: &Scope) -> Result<bool, EvalError> {
    match predicate {
        Expr::Block { .. } => eval_block_predicate(predicate, item, scope),
        // For non-block predicates, evaluate with a scope that has a special `.` context
        // Shorthand: `.field` expressions. We need to resolve `.field` against the current item.
        // Simple approach: create a temporary scope with the item accessible
        _ => {
            let mut item_scope = scope.clone();
            item_scope.set("_item".to_string(), item.clone());
            // Rewrite the predicate to replace PropertyAccess starting with implicit item
            let val = eval_pipe_expr(predicate, item, scope)?;
            Ok(val.is_truthy())
        }
    }
}

/// Evaluate an expression in pipe context where `.field` means `item.field`.
fn eval_pipe_expr(expr: &Expr, item: &Value, scope: &Scope) -> Result<Value, EvalError> {
    match expr {
        // `.field` shorthand — property access on implicit item
        Expr::PropertyAccess { object, key } => {
            if let Expr::Identifier(name) = object.as_ref() {
                if name.is_empty() || name == "_" {
                    // This shouldn't happen with current parser, but handle it
                    let field_name = property_key_str(key);
                    return Ok(item.get_field(&field_name));
                }
            }
            // Regular property access — evaluate normally
            let obj = eval_pipe_expr(object, item, scope)?;
            let field_name = property_key_str(key);
            Ok(obj.get_field(&field_name))
        }
        // Binary ops: evaluate both sides in pipe context
        Expr::BinaryOp { left, op, right } => {
            let lval = eval_pipe_expr(left, item, scope)?;
            let rval = eval_pipe_expr(right, item, scope)?;
            // Reuse the binary op logic
            match op {
                BinOp::Eq => Ok(Value::Bool(lval == rval)),
                BinOp::NotEq => Ok(Value::Bool(lval != rval)),
                BinOp::Gt => Ok(Value::Bool(lval > rval)),
                BinOp::Lt => Ok(Value::Bool(lval < rval)),
                BinOp::Gte => Ok(Value::Bool(lval >= rval)),
                BinOp::Lte => Ok(Value::Bool(lval <= rval)),
                BinOp::And => Ok(Value::Bool(lval.is_truthy() && rval.is_truthy())),
                BinOp::Or => Ok(Value::Bool(lval.is_truthy() || rval.is_truthy())),
                _ => eval_expr(expr, scope), // fallback
            }
        }
        // Anything else — evaluate normally
        _ => eval_expr(expr, scope),
    }
}

/// Check if an expression is a Guard (for treating errors as assertion failures).
fn is_guard_expr(expr: &Expr) -> bool {
    matches!(expr, Expr::Guard { .. })
}

fn property_key_str(key: &PropertyKey) -> String {
    match key {
        PropertyKey::Name(s) => s.clone(),
        PropertyKey::Quoted(s) => s.clone(),
    }
}

// ---------------------------------------------------------------------------
// Statement execution
// ---------------------------------------------------------------------------

/// Result of executing a statement.
pub enum StmtResult {
    /// Statement executed normally.
    Ok,
    /// Assertion failed.
    AssertionFailed(AssertionFailure),
    /// Matrix definition — collected by exec_file for the runner.
    MatrixDef(MatrixDef),
    /// `return <expr>;` — halt the file's body and emit the value as its output.
    Return(Value),
}

/// A fully evaluated matrix definition.
#[derive(Debug, Clone)]
pub struct MatrixDef {
    pub name: String,
    pub entries: Vec<EvaluatedMatrixEntry>,
}

/// A single evaluated matrix entry — label + variables to inject.
#[derive(Debug, Clone)]
pub struct EvaluatedMatrixEntry {
    pub label: String,
    pub vars: HashMap<String, Value>,
}

/// Build a short diagnostic string for a failed assertion.
/// For comparisons, shows "got X, expected Y". For simple values, shows "got X".
fn assertion_detail(expr: &Expr, scope: &Scope) -> String {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            let lval = eval_expr(left, scope).ok();
            let rval = eval_expr(right, scope).ok();
            match (lval, rval, op) {
                // != : "was null" rather than "got null, expected null"
                (Some(l), _, BinOp::NotEq) => {
                    format!("was {}", format_value_short(&l))
                }
                // ==, >, <, >=, <= : show both sides
                (Some(l), Some(r), BinOp::Eq | BinOp::Gt | BinOp::Lt | BinOp::Gte | BinOp::Lte) => {
                    let l_str = format_value_short(&l);
                    let r_str = format_value_short(&r);
                    format!("got {}, expected {}", l_str, r_str)
                }
                // Regex: show the value that didn't match
                (Some(l), _, BinOp::RegexTest | BinOp::RegexNoMatch) => {
                    format!("got {}", format_value_short(&l))
                }
                _ => String::new(),
            }
        }
        Expr::Not(inner) => {
            if let Ok(val) = eval_expr(inner, scope) {
                format!("got {}", format_value_short(&val))
            } else {
                String::new()
            }
        }
        _ => {
            // Simple truthy check — show what we got
            if let Ok(val) = eval_expr(expr, scope) {
                if matches!(val, Value::Null | Value::Bool(false)) {
                    format!("got {}", format_value_short(&val))
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        }
    }
}

/// Format a Value for short diagnostic display — truncate long values.
fn format_value_short(val: &Value) -> String {
    match val {
        Value::String(s) => {
            if s.len() > 50 {
                format!("\"{}...\"", &s[..47])
            } else {
                format!("\"{}\"", s)
            }
        }
        other => {
            let s = other.to_display_string();
            if s.len() > 60 {
                format!("{}...", &s[..57])
            } else {
                s
            }
        }
    }
}

/// Execute a sequence of statements (the body of an `if` branch or a `retry`
/// block), failing fast at the first non-`Ok` result. On an assertion failure,
/// stamp the failing statement's source line (from the parallel `lines` map)
/// if one isn't already set, so the failure carries its own line as it bubbles
/// up rather than inheriting the enclosing statement's. Unlike `exec_file`'s
/// top-level loop, this does NOT accumulate multiple failures — same fail-fast
/// convention as `.map`/`.each` blocks.
fn exec_body(body: &[Statement], lines: &[usize], scope: &mut Scope) -> Result<StmtResult, EvalError> {
    for (i, stmt) in body.iter().enumerate() {
        match exec_statement(stmt, scope)? {
            StmtResult::Ok => {}
            StmtResult::AssertionFailed(mut f) => {
                if f.line.is_none() {
                    f.line = lines.get(i).copied();
                }
                return Ok(StmtResult::AssertionFailed(f));
            }
            other => return Ok(other),
        }
    }
    Ok(StmtResult::Ok)
}

/// Execute a single statement, mutating the scope.
pub fn exec_statement(stmt: &Statement, scope: &mut Scope) -> Result<StmtResult, EvalError> {
    match stmt {
        Statement::Assignment { target, value } => {
            // Guard expressions (expr | "message") return EvalError on null —
            // treat those as assertion failures, not runtime errors.
            let val = match eval_expr(value, scope) {
                Ok(v) => v,
                Err(e) if is_guard_expr(value) => {
                    return Ok(StmtResult::AssertionFailed(AssertionFailure::new(e.message)));
                }
                Err(e) => return Err(e),
            };
            match target {
                AssignTarget::Variable(name) => {
                    scope.set(name.clone(), val);
                }
                AssignTarget::FieldAccess { object, path } => {
                    set_nested_field(scope, object, path, val)?;
                }
            }
            Ok(StmtResult::Ok)
        }

        Statement::Assertion { expr, message } => {
            let val = eval_expr(expr, scope)?;
            if val.is_truthy() {
                Ok(StmtResult::Ok)
            } else {
                let base_msg = interpolate_string(message, scope)?;
                let detail = assertion_detail(expr, scope);
                let msg = if detail.is_empty() {
                    base_msg
                } else {
                    format!("{} ({})", base_msg, detail)
                };
                Ok(StmtResult::AssertionFailed(AssertionFailure::new(msg)))
            }
        }

        Statement::If { condition, then_body, then_lines, else_body, else_lines } => {
            let cond = eval_expr(condition, scope)?;
            if cond.is_truthy() {
                exec_body(then_body, then_lines, scope)
            } else {
                exec_body(else_body, else_lines, scope)
            }
        }

        Statement::HttpCall { target, method, url, request_obj, status_check } => {
            match crate::http::execute_http_call(method, url, request_obj, status_check, scope) {
                Ok(body) => {
                    scope.set(target.clone(), body);
                    Ok(StmtResult::Ok)
                }
                Err(e) => {
                    // Status check failures are assertion-like
                    Ok(StmtResult::AssertionFailed(AssertionFailure::new(e.message)))
                }
            }
        }

        Statement::ExprStatement { expr } => {
            eval_expr(expr, scope)?;
            Ok(StmtResult::Ok)
        }

        Statement::JsBlock { code: _ } => {
            // TODO: boa integration
            Err(EvalError::new("js:{} blocks not yet implemented"))
        }

        Statement::Matrix { name, entries } => {
            let mut evaluated = Vec::new();
            for entry in entries {
                let val = eval_expr(&entry.value, scope)?;
                match val {
                    Value::Object(map) => {
                        evaluated.push(EvaluatedMatrixEntry {
                            label: entry.label.clone(),
                            vars: map,
                        });
                    }
                    _ => return Err(EvalError::new(format!(
                        "matrix entry '{}' must be an object, got {}",
                        entry.label, val.type_name()
                    ))),
                }
            }
            Ok(StmtResult::MatrixDef(MatrixDef {
                name: name.clone(),
                entries: evaluated,
            }))
        }

        Statement::Export { value } => {
            // Merge the desugared bindings into `_out`, the export accumulator
            // exec_file/invoke_lib harvest. Non-terminating, so repeated
            // `export`s accumulate.
            let exported = eval_expr(value, scope)?;
            if let Value::Object(map) = exported {
                let mut out = match scope.get("_out") {
                    Value::Object(m) => m,
                    _ => HashMap::new(),
                };
                for (k, v) in map {
                    out.insert(k, v);
                }
                scope.set("_out".to_string(), Value::Object(out));
            }
            Ok(StmtResult::Ok)
        }

        Statement::Return { value } => {
            let val = match value {
                Some(expr) => eval_expr(expr, scope)?,
                None => Value::Null,
            };
            Ok(StmtResult::Return(val))
        }

        Statement::Retry { max, interval_ms, timeout_ms, body, body_lines } => {
            // u32/u64 are Copy; deref out of the by-ref pattern bindings.
            let max = *max;
            let interval_ms = *interval_ms;
            let timeout_ms = *timeout_ms;

            let start = std::time::Instant::now();

            // Build the "gave up" failure message once we stop retrying.
            // Preserve the inner failure's line so the report points at the
            // assertion that never passed, not at the `retry` statement.
            let exhausted = |f: &AssertionFailure, attempt: u32, elapsed: std::time::Duration| {
                AssertionFailure {
                    message: format!(
                        "{} (retry exhausted after {} attempt{}, {:.1}s)",
                        f.message,
                        attempt,
                        if attempt == 1 { "" } else { "s" },
                        elapsed.as_secs_f64(),
                    ),
                    line: f.line,
                }
            };

            let mut attempt: u32 = 0;
            loop {
                attempt += 1;

                // Run the body, failing fast at the first failing assertion —
                // that's the signal to wait and try the whole block again.
                // Control-flow statements (`return`, `matrix`) don't compose
                // with re-execution — reject them rather than silently doing
                // something surprising on the 2nd pass.
                let attempt_failure: Option<AssertionFailure> = match exec_body(body, body_lines, scope)? {
                    StmtResult::Ok => None,
                    StmtResult::AssertionFailed(f) => Some(f),
                    StmtResult::Return(_) => {
                        return Err(EvalError::new(
                            "return is not allowed inside a retry block",
                        ));
                    }
                    StmtResult::MatrixDef(_) => {
                        return Err(EvalError::new(
                            "matrix is not allowed inside a retry block",
                        ));
                    }
                };

                match attempt_failure {
                    // Whole body passed — done.
                    None => {
                        if attempt > 1 {
                            scope.add_log(format!(
                                "retry: passed on attempt {} ({:.1}s)",
                                attempt,
                                start.elapsed().as_secs_f64(),
                            ));
                        }
                        return Ok(StmtResult::Ok);
                    }
                    Some(f) => {
                        let attempts_left = max.map_or(true, |m| attempt < m);
                        // Milliseconds remaining before the timeout (None = no timeout).
                        let remaining_ms = timeout_ms
                            .map(|t| (t as u128).saturating_sub(start.elapsed().as_millis()));
                        let time_left = remaining_ms.map_or(true, |r| r > 0);

                        if !attempts_left || !time_left {
                            return Ok(StmtResult::AssertionFailed(
                                exhausted(&f, attempt, start.elapsed()),
                            ));
                        }

                        // Sleep before the next attempt, clamped to the time
                        // left so a long interval can't blow past the timeout.
                        let sleep_ms = match remaining_ms {
                            Some(r) => interval_ms.min(r as u64),
                            None => interval_ms,
                        };
                        std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
                    }
                }
            }
        }
    }
}

/// Set a nested field: `req.headers."content-type" = val`
fn set_nested_field(scope: &mut Scope, object: &str, path: &[PropertyKey], value: Value) -> Result<(), EvalError> {
    let mut current = scope.get(object);

    // If the root doesn't exist yet, create an empty object
    if current == Value::Null {
        current = Value::Object(HashMap::new());
    }

    if path.len() == 1 {
        // Simple case: obj.field = val
        match &mut current {
            Value::Object(map) => {
                map.insert(property_key_str(&path[0]), value);
            }
            _ => return Err(EvalError::new(format!(
                "cannot set field on {}", current.type_name()
            ))),
        }
    } else {
        // Nested: obj.a.b.c = val — need to walk and build the chain
        set_deep_field(&mut current, &path, value)?;
    }

    scope.set(object.to_string(), current);
    Ok(())
}

/// Recursively set a deeply nested field.
fn set_deep_field(obj: &mut Value, path: &[PropertyKey], value: Value) -> Result<(), EvalError> {
    if path.is_empty() {
        return Ok(());
    }

    let key = property_key_str(&path[0]);

    if path.len() == 1 {
        match obj {
            Value::Object(map) => {
                map.insert(key, value);
                Ok(())
            }
            _ => Err(EvalError::new(format!("cannot set field '{}' on {}", key, obj.type_name()))),
        }
    } else {
        match obj {
            Value::Object(map) => {
                let child = map.entry(key).or_insert_with(|| Value::Object(HashMap::new()));
                set_deep_field(child, &path[1..], value)
            }
            _ => Err(EvalError::new(format!("cannot access field on {}", obj.type_name()))),
        }
    }
}

/// Result of executing a file.
#[derive(Debug)]
pub struct FileResult {
    /// Test name (derived from filename)
    pub name: String,
    /// Whether the file was skipped (`disabled`, or an upstream/dependency
    /// cascade set by the runner — never by the file's own execution).
    pub skipped: bool,
    /// Whether the file was intentionally turned off via a `disabled:` metadata
    /// marker. A disabled file is always also `skipped` (so it never counts as
    /// a pass), but is reported as a distinct DISABLED status.
    pub disabled: bool,
    /// Whether the file was skipped because the running binary doesn't satisfy
    /// its `requires:` constraint. Like `disabled`, an incompatible file is
    /// always also `skipped`, but is reported as a distinct INCOMPATIBLE status.
    /// `skip_reason` carries `needs <req>, have <current>`.
    pub incompatible: bool,
    /// Why the file was skipped (for log output). For `disabled`, this carries
    /// the mandatory reason from the marker.
    pub skip_reason: Option<String>,
    /// Input variables (name, source, value) captured at execution time
    pub inputs: Vec<(String, Option<String>, Value)>,
    /// Assertion failures
    pub failures: Vec<AssertionFailure>,
    /// Exported variables (from <--)
    pub exports: HashMap<String, Value>,
    /// Log messages from $.log()
    pub logs: Vec<String>,
    /// Last HTTP endpoint called (e.g. "POST https://example.com/v4/groups")
    pub endpoint: Option<String>,
    /// Execution time
    pub elapsed: std::time::Duration,
    /// Whether this is a const file (for output filtering)
    pub is_const: bool,
    /// Matrix definitions found during const execution
    pub matrices: Vec<MatrixDef>,
}

/// Execute a parsed file's body statements.
pub fn exec_file(
    file: &crate::ast::File,
    name: &str,
    scope: &mut Scope,
) -> Result<FileResult, EvalError> {
    let start = std::time::Instant::now();
    let mut failures = Vec::new();
    let mut matrices = Vec::new();
    let is_const = file.file_type == crate::ast::FileType::Const;

    // Initialize _out as empty object for export assignments
    scope.set("_out".to_string(), Value::Object(HashMap::new()));

    // Capture _in variable values + their sources for the log table
    let inputs: Vec<(String, Option<String>, Value)> = match scope.get("_in") {
        Value::Object(map) => {
            let mut v: Vec<_> = map.into_iter()
                .map(|(name, value)| {
                    let source = scope.source_of(&name);
                    (name, source, value)
                })
                .collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        }
        _ => Vec::new(),
    };

    // Intentionally-disabled file: short-circuit before running anything.
    // Position of the marker in the body is irrelevant — the whole file is
    // off — so we gate here rather than relying on statement order.
    if let Some(reason) = file.disabled_reason() {
        return Ok(FileResult {
            name: name.to_string(),
            skipped: true,
            disabled: true,
            incompatible: false,
            skip_reason: Some(reason.to_string()),
            inputs,
            failures: Vec::new(),
            endpoint: None,
            exports: HashMap::new(),
            logs: scope.take_logs(),
            elapsed: start.elapsed(),
            is_const,
            matrices: Vec::new(),
        });
    }

    // Version gate: if the file declares a `requires:` the running binary can't
    // satisfy, skip it as INCOMPATIBLE rather than running it and reporting
    // confusing failures. A new test on an old binary should bail loudly, not
    // explode. (The constraint was validated at parse time, so re-parse is
    // expected to succeed; an unexpected error degrades to "don't gate".)
    if let Some(req_str) = &file.metadata.requires {
        if let Ok(req) = crate::version::parse_requirement(req_str) {
            if !req.is_satisfied_by_current() {
                return Ok(FileResult {
                    name: name.to_string(),
                    skipped: true,
                    disabled: false,
                    incompatible: true,
                    skip_reason: Some(format!(
                        "needs {}, have {}",
                        req_str.trim(),
                        crate::version::current()
                    )),
                    inputs,
                    failures: Vec::new(),
                    endpoint: None,
                    exports: HashMap::new(),
                    logs: scope.take_logs(),
                    elapsed: start.elapsed(),
                    is_const,
                    matrices: Vec::new(),
                });
            }
        }
    }

    for (i, stmt) in file.body.iter().enumerate() {
        match exec_statement(stmt, scope) {
            Ok(StmtResult::Ok) => {}
            Ok(StmtResult::MatrixDef(def)) => {
                matrices.push(def);
            }
            Ok(StmtResult::Return(_)) => {
                // A top-level `return;` is void — it only halts execution.
                // Publishing is `export` (harvested from `_out` below).
                break;
            }
            Ok(StmtResult::AssertionFailed(mut f)) => {
                // Annotate with the line number. Prefer a line the failure
                // already carries (set deep inside an `if`/`retry` branch);
                // fall back to this top-level statement's line.
                let line = f.line.or_else(|| file.line_map.get(i).copied());
                if let Some(l) = line {
                    f.message = format!("line {}: {}", l, f.message);
                }
                failures.push(f);
            }
            Err(e) => {
                // Runtime error: record it as a final failure (preserving any
                // earlier assertion failures) and stop execution.
                let msg = if let Some(&line) = file.line_map.get(i) {
                    format!("line {}: runtime error: {}", line, e.message)
                } else {
                    format!("runtime error: {}", e.message)
                };
                failures.push(AssertionFailure::new(msg));
                break;
            }
        }
    }

    // Exports come from `export` statements, accumulated in `_out`.
    let mut exports = HashMap::new();
    if let Value::Object(out_map) = scope.get("_out") {
        for (k, v) in out_map {
            exports.insert(k, v);
        }
    }

    let logs = scope.take_logs();
    let elapsed = start.elapsed();

    Ok(FileResult {
        name: name.to_string(),
        // A file that runs to here always executed (any conditional skipping is
        // now via `if`, which doesn't skip the file). `disabled` short-circuits
        // earlier; the runner sets `skipped` for blocked/missing-input cascades.
        skipped: false,
        disabled: false,
        incompatible: false,
        skip_reason: None,
        inputs,
        failures,
        endpoint: scope.last_endpoint(),
        exports,
        logs,
        elapsed,
        is_const,
        matrices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr_str: &str) -> Value {
        eval_with_scope(expr_str, &Scope::new())
    }

    fn eval_with_scope(expr_str: &str, scope: &Scope) -> Value {
        let mut input = expr_str;
        let expr = crate::parser::expr::expr(&mut input).unwrap();
        eval_expr(&expr, scope).unwrap()
    }

    fn eval_err(expr_str: &str) -> String {
        let mut input = expr_str;
        let expr = crate::parser::expr::expr(&mut input).unwrap();
        eval_expr(&expr, &Scope::new()).unwrap_err().message
    }

    // --- Constants namespace ---

    fn scope_with_constants(pairs: &[(&str, Value)]) -> Scope {
        let map: HashMap<String, Value> = pairs.iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        Scope::new().with_constants(std::sync::Arc::new(map))
    }

    #[test]
    fn test_constant_lookup_simple() {
        let scope = scope_with_constants(&[
            ("apiVersion", Value::String("v4".to_string())),
        ]);
        assert_eq!(
            eval_with_scope("${apiVersion}", &scope),
            Value::String("v4".to_string()),
        );
    }

    #[test]
    fn interp_braces_resolve_constant() {
        // {{X}} inside a string resolves a yaml constant (Bug 3).
        let scope = scope_with_constants(&[
            ("accountId", Value::String("acct-99".to_string())),
        ]);
        assert_eq!(
            interpolate_string_pub("/v4/accounts/{{accountId}}", &scope).unwrap(),
            "/v4/accounts/acct-99",
        );
    }

    #[test]
    fn interp_braces_ambient_beats_constant() {
        // Ambient scope wins over a same-named constant.
        let mut scope = scope_with_constants(&[
            ("env", Value::String("from-constant".to_string())),
        ]);
        scope.set("env".to_string(), Value::String("from-ambient".to_string()));
        assert_eq!(
            interpolate_string_pub("env={{env}}", &scope).unwrap(),
            "env=from-ambient",
        );
    }

    #[test]
    fn interp_braces_dotted_constant() {
        let mut org = HashMap::new();
        org.insert("baseUrl".to_string(), Value::String("https://api.example.com".to_string()));
        let scope = scope_with_constants(&[("org", Value::Object(org))]);
        assert_eq!(
            interpolate_string_pub("{{org.baseUrl}}/v4", &scope).unwrap(),
            "https://api.example.com/v4",
        );
    }

    #[test]
    fn interp_braces_unknown_errors() {
        let scope = Scope::new();
        let err = interpolate_string_pub("x={{nope}}", &scope).unwrap_err().message;
        assert!(err.contains("nope"), "expected error to mention 'nope', got: {}", err);
        assert!(!err.contains("_in."), "error should no longer suggest _in.X: {}", err);
    }

    #[test]
    fn test_constant_lookup_dotted() {
        let mut org = HashMap::new();
        org.insert("baseUrl".to_string(), Value::String("https://api.example.com".to_string()));
        org.insert("version".to_string(), Value::String("v4".to_string()));
        let scope = scope_with_constants(&[
            ("orgService", Value::Object(org)),
        ]);
        assert_eq!(
            eval_with_scope("${orgService.baseUrl}", &scope),
            Value::String("https://api.example.com".to_string()),
        );
        assert_eq!(
            eval_with_scope("${orgService.version}", &scope),
            Value::String("v4".to_string()),
        );
    }

    #[test]
    fn test_constant_missing_top_level_errors() {
        // No constants set; ${anything} should error rather than silently return Null.
        let mut input = "${missing}";
        let expr = crate::parser::expr::expr(&mut input).unwrap();
        let err = eval_expr(&expr, &Scope::new()).unwrap_err().message;
        assert!(err.contains("missing"), "expected error to mention missing constant, got: {}", err);
    }

    #[test]
    fn test_constant_missing_subkey_returns_null() {
        // Sub-key lookups are forgiving (like field access on missing keys).
        let mut org = HashMap::new();
        org.insert("baseUrl".to_string(), Value::String("x".to_string()));
        let scope = scope_with_constants(&[("orgService", Value::Object(org))]);
        assert_eq!(eval_with_scope("${orgService.notThere}", &scope), Value::Null);
    }

    // --- Literals ---

    #[test]
    fn test_literals() {
        assert_eq!(eval("null"), Value::Null);
        assert_eq!(eval("true"), Value::Bool(true));
        assert_eq!(eval("false"), Value::Bool(false));
        assert_eq!(eval("42"), Value::Number(42.0));
        assert_eq!(eval("3.14"), Value::Number(3.14));
        assert_eq!(eval("\"hello\""), Value::String("hello".to_string()));
    }

    // --- Arithmetic ---

    #[test]
    fn test_arithmetic() {
        assert_eq!(eval("2 + 3"), Value::Number(5.0));
        assert_eq!(eval("10 - 4"), Value::Number(6.0));
        assert_eq!(eval("3 * 7"), Value::Number(21.0));
        assert_eq!(eval("15 / 3"), Value::Number(5.0));
        assert_eq!(eval("17 % 5"), Value::Number(2.0));
    }

    #[test]
    fn test_precedence() {
        assert_eq!(eval("2 + 3 * 4"), Value::Number(14.0));
        assert_eq!(eval("(2 + 3) * 4"), Value::Number(20.0));
    }

    #[test]
    fn test_division_by_zero() {
        assert!(eval_err("1 / 0").contains("division by zero"));
    }

    #[test]
    fn test_string_concatenation() {
        assert_eq!(eval("\"hello\" + \" \" + \"world\""), Value::String("hello world".to_string()));
    }

    // --- Comparison ---

    #[test]
    fn test_comparison() {
        assert_eq!(eval("1 == 1"), Value::Bool(true));
        assert_eq!(eval("1 == 2"), Value::Bool(false));
        assert_eq!(eval("1 != 2"), Value::Bool(true));
        assert_eq!(eval("5 > 3"), Value::Bool(true));
        assert_eq!(eval("5 < 3"), Value::Bool(false));
        assert_eq!(eval("5 >= 5"), Value::Bool(true));
        assert_eq!(eval("5 <= 4"), Value::Bool(false));
    }

    #[test]
    fn test_null_comparison() {
        assert_eq!(eval("null == null"), Value::Bool(true));
        assert_eq!(eval("null != null"), Value::Bool(false));
        assert_eq!(eval("42 != null"), Value::Bool(true));
    }

    // --- Logical ---

    #[test]
    fn test_logical() {
        assert_eq!(eval("true && true"), Value::Bool(true));
        assert_eq!(eval("true && false"), Value::Bool(false));
        assert_eq!(eval("false || true"), Value::Bool(true));
        assert_eq!(eval("false || false"), Value::Bool(false));
    }

    #[test]
    fn test_negation() {
        assert_eq!(eval("!true"), Value::Bool(false));
        assert_eq!(eval("!false"), Value::Bool(true));
        assert_eq!(eval("!null"), Value::Bool(true));
    }

    // --- Variables and scope ---

    #[test]
    fn test_variable_lookup() {
        let mut scope = Scope::new();
        scope.set("x".to_string(), Value::Number(42.0));
        scope.set("name".to_string(), Value::String("Test".to_string()));

        assert_eq!(eval_with_scope("x", &scope), Value::Number(42.0));
        assert_eq!(eval_with_scope("name", &scope), Value::String("Test".to_string()));
    }

    #[test]
    fn test_undefined_variable_errors() {
        // Undefined bare identifier is a hard error (Bug 4) — fail loud.
        let mut input = "missing";
        let expr = crate::parser::expr::expr(&mut input).unwrap();
        let err = eval_expr(&expr, &Scope::new()).unwrap_err().message;
        assert!(err.contains("missing"), "expected error to name 'missing', got: {}", err);
    }

    #[test]
    fn test_undefined_underscore_var_is_null() {
        // The runtime/legacy `_`-namespace is exempt — may be conditionally
        // present (e.g. _response before a call), so it reads as null.
        let mut input = "_response";
        let expr = crate::parser::expr::expr(&mut input).unwrap();
        assert_eq!(eval_expr(&expr, &Scope::new()).unwrap(), Value::Null);
    }

    // --- Property access ---

    #[test]
    fn test_property_access() {
        let mut scope = Scope::new();
        let mut obj = HashMap::new();
        obj.insert("id".to_string(), Value::Number(123.0));
        obj.insert("name".to_string(), Value::String("Test".to_string()));
        scope.set("r".to_string(), Value::Object(obj));

        assert_eq!(eval_with_scope("r.id", &scope), Value::Number(123.0));
        assert_eq!(eval_with_scope("r.name", &scope), Value::String("Test".to_string()));
        assert_eq!(eval_with_scope("r.missing", &scope), Value::Null);
    }

    #[test]
    fn test_nested_property() {
        let mut scope = Scope::new();
        let inner = HashMap::from([
            ("city".to_string(), Value::String("NYC".to_string())),
        ]);
        let outer = HashMap::from([
            ("address".to_string(), Value::Object(inner)),
        ]);
        scope.set("user".to_string(), Value::Object(outer));

        assert_eq!(
            eval_with_scope("user.address.city", &scope),
            Value::String("NYC".to_string())
        );
    }

    #[test]
    fn test_optional_chaining() {
        let mut scope = Scope::new();
        scope.set("r".to_string(), Value::Null);
        assert_eq!(eval_with_scope("r?.name", &scope), Value::Null);

        let obj = HashMap::from([
            ("name".to_string(), Value::String("Test".to_string())),
        ]);
        scope.set("r".to_string(), Value::Object(obj));
        assert_eq!(eval_with_scope("r?.name", &scope), Value::String("Test".to_string()));
    }

    // --- Array operations ---

    #[test]
    fn test_array_index() {
        let mut scope = Scope::new();
        scope.set("items".to_string(), Value::Array(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
            Value::String("c".to_string()),
        ]));

        assert_eq!(eval_with_scope("items[0]", &scope), Value::String("a".to_string()));
        assert_eq!(eval_with_scope("items[-1]", &scope), Value::String("c".to_string()));
    }

    // --- JSON construction ---

    #[test]
    fn test_json_object() {
        let result = eval("{ name: \"Test\", count: 3 }");
        match result {
            Value::Object(map) => {
                assert_eq!(map.get("name"), Some(&Value::String("Test".to_string())));
                assert_eq!(map.get("count"), Some(&Value::Number(3.0)));
            }
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn test_json_array() {
        assert_eq!(eval("[1, 2, 3]"), Value::Array(vec![
            Value::Number(1.0), Value::Number(2.0), Value::Number(3.0),
        ]));
    }

    // --- String interpolation ---

    #[test]
    fn test_interpolation() {
        let mut scope = Scope::new();
        scope.set("name".to_string(), Value::String("World".to_string()));
        assert_eq!(
            eval_with_scope("\"Hello {{name}}!\"", &scope),
            Value::String("Hello World!".to_string())
        );
    }

    // --- Collection properties ---

    #[test]
    fn test_string_length() {
        assert_eq!(eval("\"hello\".length"), Value::Number(5.0));
    }

    #[test]
    fn test_array_size() {
        let mut scope = Scope::new();
        scope.set("items".to_string(), Value::Array(vec![
            Value::Number(1.0), Value::Number(2.0),
        ]));
        assert_eq!(eval_with_scope("items.size", &scope), Value::Number(2.0));
    }

    // --- Statement execution ---

    /// Wrap a bare statement body in the mandatory function form so the strict
    /// parser (header + braces) accepts test sources written as plain bodies.
    fn wrap_body(source: &str) -> String {
        // Body inline after `{` so the source keeps its original line numbers
        // (line 1 stays line 1); closing brace on its own line so a trailing
        // line comment in `source` can't swallow it.
        format!("--> {{ {}\n}}", source)
    }

    fn exec(source: &str) -> (Scope, Vec<AssertionFailure>) {
        exec_with_scope(source, Scope::new())
    }

    fn exec_with_scope(source: &str, initial_scope: Scope) -> (Scope, Vec<AssertionFailure>) {
        // These tests pass a bare statement body; wrap it in the mandatory
        // function form so the strict parser accepts it.
        let file = crate::parser::parse_file(&wrap_body(source), "test.tstr").unwrap();
        let mut scope = initial_scope;
        // Seed scope with file inputs (they'd normally come from upstream)
        let result = exec_file(&file, "test", &mut scope).unwrap();
        (scope, result.failures)
    }

    #[test]
    fn test_exec_assignment() {
        let (scope, failures) = exec("x = 42;");
        assert!(failures.is_empty());
        assert_eq!(scope.get("x"), Value::Number(42.0));
    }

    #[test]
    fn test_exec_multiple_assignments() {
        let (scope, failures) = exec("a = 1; b = 2; c = a + b;");
        assert!(failures.is_empty());
        assert_eq!(scope.get("c"), Value::Number(3.0));
    }

    #[test]
    fn test_exec_field_mutation() {
        let (scope, failures) = exec("req.body = { name: \"Test\" }; req.body.count = 3;");
        assert!(failures.is_empty());
        let body = scope.get("req");
        assert_eq!(
            body.get_field("body").get_field("name"),
            Value::String("Test".to_string())
        );
        assert_eq!(
            body.get_field("body").get_field("count"),
            Value::Number(3.0)
        );
    }

    #[test]
    fn test_exec_assertion_pass() {
        let (_, failures) = exec("x = 5; x > 0 | \"should be positive\";");
        assert!(failures.is_empty());
    }

    #[test]
    fn test_exec_assertion_fail() {
        let (_, failures) = exec("x = 0; x > 0 | \"should be positive\";");
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("should be positive"));
    }

    #[test]
    fn test_exec_multiple_assertions() {
        let (_, failures) = exec(
            "x = 5; x > 0 | \"positive\"; x > 10 | \"greater than 10\"; x < 100 | \"less than 100\";"
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("greater than 10"));
    }

    #[test]
    fn test_exec_if_false_skips_body() {
        let (scope, _) = exec("x = null; if x != null { y = 42; }");
        // body didn't run because the condition was false
        assert_eq!(scope.get("y"), Value::Null);
    }

    #[test]
    fn test_exec_if_true_runs_body() {
        let (scope, _) = exec("x = 5; if x != null { y = 42; }");
        // condition true, so the body ran
        assert_eq!(scope.get("y"), Value::Number(42.0));
    }

    #[test]
    fn test_exec_if_else_branch() {
        let (scope, _) = exec("x = null; if x != null { y = 1; } else { y = 2; }");
        assert_eq!(scope.get("y"), Value::Number(2.0));
    }

    #[test]
    fn test_exec_if_else_if_chain() {
        let (scope, _) = exec(
            "x = 2; if x == 1 { y = 10; } else if x == 2 { y = 20; } else { y = 30; }",
        );
        assert_eq!(scope.get("y"), Value::Number(20.0));
    }

    #[test]
    fn test_exec_if_body_assertion_reports_inner_line() {
        // An assertion failing inside an `if` body must report its own line,
        // not the `if`'s. The body assertion sits on line 2.
        let (_, failures) = exec("if true {\n  false | \"boom\";\n}");
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].message.starts_with("line 2:"),
            "got: {}", failures[0].message,
        );
    }

    #[test]
    fn test_exec_exports() {
        let source = "groupId = 123; groupName = \"Test\"; temp = 999; export groupId, groupName;";
        let file = crate::parser::parse_file(&wrap_body(source), "test.tstr").unwrap();
        let mut scope = Scope::new();
        let result = exec_file(&file, "test", &mut scope).unwrap();

        assert_eq!(result.exports.len(), 2);
        assert_eq!(result.exports["groupId"], Value::Number(123.0));
        assert_eq!(result.exports["groupName"], Value::String("Test".to_string()));
        // temp is not exported
        assert!(!result.exports.contains_key("temp"));
    }

    #[test]
    fn test_exec_if_does_not_skip_file() {
        // A false `if` skips only its body — the file still ran to completion,
        // so it is neither `skipped` nor `disabled` (unlike the old exitIf,
        // which marked the whole file skipped and cascaded to siblings).
        let source = "if false { false | \"unreached\"; }";
        let file = crate::parser::parse_file(&wrap_body(source), "test.tstr").unwrap();
        let mut scope = Scope::new();
        let result = exec_file(&file, "test", &mut scope).unwrap();
        assert!(!result.skipped);
        assert!(!result.disabled);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn test_exec_disabled_short_circuits() {
        // The `disabled:` metadata marker must turn the whole file off — even a
        // guaranteed assertion failure must not run, and the file must report as
        // disabled (a distinct flavor of skip) carrying the reason.
        let source = format!("disabled: I-123: fix postponed\n{}", wrap_body(r#"false | "this must not fire";"#));
        let file = crate::parser::parse_file(&source, "test.tstr").unwrap();
        let mut scope = Scope::new();
        let result = exec_file(&file, "test", &mut scope).unwrap();
        assert!(result.disabled);
        assert!(result.skipped);
        assert!(result.failures.is_empty(), "disabled file must not run assertions");
        assert_eq!(result.skip_reason.as_deref(), Some("I-123: fix postponed"));
    }

    #[test]
    fn test_exec_requires_incompatible_skips() {
        // A `requires:` the running binary can't satisfy turns the file off as
        // INCOMPATIBLE — a distinct flavor of skip — without running anything.
        let source = format!("requires: >= 999.0.0\n{}", wrap_body(r#"false | "must not fire";"#));
        let file = crate::parser::parse_file(&source, "test.tstr").unwrap();
        let mut scope = Scope::new();
        let result = exec_file(&file, "test", &mut scope).unwrap();
        assert!(result.incompatible);
        assert!(result.skipped);
        assert!(!result.disabled);
        assert!(result.failures.is_empty(), "incompatible file must not run assertions");
        let reason = result.skip_reason.as_deref().unwrap_or("");
        assert!(reason.contains("needs >= 999.0.0"), "reason: {}", reason);
    }

    #[test]
    fn test_exec_requires_satisfied_runs() {
        // A satisfiable `requires:` is inert — the file runs normally.
        let source = format!("requires: >= 0.0.1\n{}", wrap_body(r#"1 == 1 | "ok";"#));
        let file = crate::parser::parse_file(&source, "test.tstr").unwrap();
        let mut scope = Scope::new();
        let result = exec_file(&file, "test", &mut scope).unwrap();
        assert!(!result.incompatible);
        assert!(!result.skipped);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn test_exec_disabled_skips_body() {
        // A `disabled:` file short-circuits before the body runs, so its
        // statements' side effects (assignments, HTTP calls) never happen.
        let source = format!("disabled: off\n{}", wrap_body("x = 1;"));
        let file = crate::parser::parse_file(&source, "test.tstr").unwrap();
        let mut scope = Scope::new();
        let result = exec_file(&file, "test", &mut scope).unwrap();
        assert!(result.disabled);
        // x was never assigned because we short-circuited before the loop.
        assert_eq!(scope.get("x"), Value::Null);
    }

    #[test]
    fn test_load_file_ref_resolves_relative_to_base_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/data.json"), r#"{"k": "v"}"#).unwrap();

        // A relative @path resolves against base_dir (the suite root), not cwd.
        let v = load_file_ref("sub/data.json", Some(root)).unwrap();
        assert_eq!(v.get_field("k"), Value::String("v".to_string()));

        // Without a base_dir the same relative path is cwd-relative and not found.
        assert!(load_file_ref("sub/data.json", None).is_err());

        // An absolute path ignores base_dir entirely.
        let abs = root.join("sub/data.json");
        let v2 = load_file_ref(abs.to_str().unwrap(), Some(std::path::Path::new("/nonexistent"))).unwrap();
        assert_eq!(v2.get_field("k"), Value::String("v".to_string()));
    }

    #[test]
    fn test_exec_assertion_with_interpolation() {
        let (_, failures) = exec("name = \"Widget\"; name == \"Gadget\" | \"expected Gadget, got {{name}}\";");
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("expected Gadget, got Widget"));
    }

    #[test]
    fn test_exec_with_inherited_scope() {
        let mut parent_scope = Scope::new();
        parent_scope.set("baseUrl".to_string(), Value::String("http://localhost".to_string()));

        let (scope, failures) = exec_with_scope(
            "url = baseUrl; url == \"http://localhost\" | \"wrong url\";",
            parent_scope,
        );
        assert!(failures.is_empty());
        assert_eq!(scope.get("url"), Value::String("http://localhost".to_string()));
    }

    // --- Collection methods ---

    #[test]
    fn test_find() {
        let mut scope = Scope::new();
        scope.set("items".to_string(), Value::Array(vec![
            Value::Object(HashMap::from([
                ("name".to_string(), Value::String("alpha".to_string())),
                ("id".to_string(), Value::Number(1.0)),
            ])),
            Value::Object(HashMap::from([
                ("name".to_string(), Value::String("beta".to_string())),
                ("id".to_string(), Value::Number(2.0)),
            ])),
        ]));

        let result = eval_with_scope("items.find({ item --> item.name == \"beta\" })", &scope);
        assert_eq!(result.get_field("id"), Value::Number(2.0));
    }

    #[test]
    fn test_find_no_match() {
        let mut scope = Scope::new();
        scope.set("items".to_string(), Value::Array(vec![
            Value::Object(HashMap::from([
                ("name".to_string(), Value::String("alpha".to_string())),
            ])),
        ]));

        let result = eval_with_scope("items.find({ item --> item.name == \"nope\" })", &scope);
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn test_filter() {
        let mut scope = Scope::new();
        scope.set("items".to_string(), Value::Array(vec![
            Value::Object(HashMap::from([
                ("active".to_string(), Value::Bool(true)),
                ("name".to_string(), Value::String("a".to_string())),
            ])),
            Value::Object(HashMap::from([
                ("active".to_string(), Value::Bool(false)),
                ("name".to_string(), Value::String("b".to_string())),
            ])),
            Value::Object(HashMap::from([
                ("active".to_string(), Value::Bool(true)),
                ("name".to_string(), Value::String("c".to_string())),
            ])),
        ]));

        let result = eval_with_scope("items.filter({ item --> item.active == true })", &scope);
        match result {
            Value::Array(arr) => {
                assert_eq!(arr.len(), 2);
                assert_eq!(arr[0].get_field("name"), Value::String("a".to_string()));
                assert_eq!(arr[1].get_field("name"), Value::String("c".to_string()));
            }
            _ => panic!("expected array"),
        }
    }

    #[test]
    fn test_map() {
        let (scope, _) = exec(
            "items = [{ name: \"a\", id: 1 }, { name: \"b\", id: 2 }]; ids = items.map({ item --> result = item.id; <-- result; });"
        );
        assert_eq!(scope.get("ids"), Value::Array(vec![
            Value::Number(1.0), Value::Number(2.0),
        ]));
    }

    #[test]
    fn test_find_and_access() {
        let (scope, failures) = exec(
            "items = [{ name: \"alpha\", id: 1 }, { name: \"beta\", id: 2 }]; match = items.find({ i --> i.name == \"beta\" }); result = match.id;"
        );
        assert!(failures.is_empty());
        assert_eq!(scope.get("result"), Value::Number(2.0));
    }

    // --- Pipe operations (any/all) ---

    #[test]
    fn test_pipe_any_true() {
        let (_, failures) = exec(
            "items = [{ active: true }, { active: false }]; items | any({ i --> i.active == true }) | \"no active items\";"
        );
        assert!(failures.is_empty());
    }

    #[test]
    fn test_pipe_any_false() {
        let (_, failures) = exec(
            "items = [{ active: false }, { active: false }]; items | any({ i --> i.active == true }) | \"no active items\";"
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("no active items"));
    }

    #[test]
    fn test_pipe_all_true() {
        let (_, failures) = exec(
            "items = [{ id: 1 }, { id: 2 }]; items | all({ i --> i.id != null }) | \"found null ids\";"
        );
        assert!(failures.is_empty());
    }

    #[test]
    fn test_pipe_all_false() {
        let (_, failures) = exec(
            "items = [{ id: 1 }, { id: null }]; items | all({ i --> i.id != null }) | \"found null ids\";"
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("found null ids"));
    }

    // --- Guard expression ---

    #[test]
    fn test_guard_pass() {
        let (scope, failures) = exec("x = 42; y = x | \"x was null\";");
        assert!(failures.is_empty());
        assert_eq!(scope.get("y"), Value::Number(42.0));
    }

    #[test]
    fn test_guard_fail() {
        let (_, failures) = exec("x = null; y = x | \"x was null\";");
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("x was null"));
    }

    #[test]
    fn test_guard_with_optional_chaining() {
        let (scope, failures) = exec(
            "obj = { name: \"test\" }; val = obj?.name | \"no name\";"
        );
        assert!(failures.is_empty());
        assert_eq!(scope.get("val"), Value::String("test".to_string()));
    }

    #[test]
    fn test_guard_null_optional_chain() {
        let (_, failures) = exec("obj = null; val = obj?.name | \"no name\";");
        assert_eq!(failures.len(), 1);
        assert!(failures[0].message.contains("no name"));
    }

    // --- Built-in functions ---

    #[test]
    fn test_builtin_uuid() {
        let (scope, _) = exec("id = $.uuid();");
        match scope.get("id") {
            Value::String(s) => {
                assert_eq!(s.len(), 36); // xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
                assert_eq!(&s[14..15], "4"); // version 4
            }
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn test_builtin_string_default() {
        let (scope, _) = exec("s = $.string();");
        match scope.get("s") {
            Value::String(s) => assert_eq!(s.len(), 8),
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn test_builtin_string_with_length() {
        let (scope, _) = exec("s = $.string(20);");
        match scope.get("s") {
            Value::String(s) => assert_eq!(s.len(), 20),
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn test_builtin_rand_email() {
        let (scope, _) = exec("e = $.randEmail();");
        match scope.get("e") {
            Value::String(s) => {
                assert!(s.contains('@'));
                assert!(s.ends_with("@example.com"));
            }
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn test_builtin_rand_email_plus_addressed() {
        let (scope, _) = exec("e = $.randEmail(\"doug@inspire360.com\");");
        match scope.get("e") {
            Value::String(s) => {
                assert!(s.starts_with("doug+"));
                assert!(s.ends_with("@inspire360.com"));
            }
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn test_builtin_now() {
        let (scope, _) = exec("t = $.now();");
        match scope.get("t") {
            Value::Number(n) => assert!(n > 1_700_000_000.0), // after 2023
            _ => panic!("expected number"),
        }
    }

    // --- hmacSha256 / stripeSign ---

    // RFC 4231 Test Case 2 — a well-known HMAC-SHA256 vector.
    const RFC4231_KEY: &str = "Jefe";
    const RFC4231_MSG: &str = "what do ya want for nothing?";
    const RFC4231_HEX: &str =
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";

    #[test]
    fn test_builtin_hmac_sha256_hex() {
        let v = eval(&format!(
            "$.hmacSha256(\"{}\", \"{}\")",
            RFC4231_KEY, RFC4231_MSG
        ));
        assert_eq!(v, Value::String(RFC4231_HEX.to_string()));
    }

    #[test]
    fn test_builtin_hmac_sha256_explicit_hex() {
        let v = eval(&format!(
            "$.hmacSha256(\"{}\", \"{}\", \"hex\")",
            RFC4231_KEY, RFC4231_MSG
        ));
        assert_eq!(v, Value::String(RFC4231_HEX.to_string()));
    }

    #[test]
    fn test_builtin_hmac_sha256_base64() {
        let v = eval(&format!(
            "$.hmacSha256(\"{}\", \"{}\", \"base64\")",
            RFC4231_KEY, RFC4231_MSG
        ));
        let b64 = match v {
            Value::String(s) => s,
            _ => panic!("expected string"),
        };
        // The base64 output must decode to the same bytes as the hex vector.
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .expect("valid base64");
        assert_eq!(decoded, hex::decode(RFC4231_HEX).unwrap());
    }

    #[test]
    fn test_builtin_hmac_sha256_arity_error() {
        let msg = eval_err("$.hmacSha256(\"key\")");
        assert!(msg.contains("2 or 3 arguments"), "got: {}", msg);
    }

    #[test]
    fn test_builtin_hmac_sha256_unknown_encoding() {
        let msg = eval_err("$.hmacSha256(\"key\", \"msg\", \"base32\")");
        assert!(msg.contains("unknown encoding"), "got: {}", msg);
    }

    #[test]
    fn test_builtin_stripe_sign_explicit_timestamp() {
        let header = match eval("$.stripeSign(\"whsec\", \"hello\", 1234567890)") {
            Value::String(s) => s,
            _ => panic!("expected string"),
        };
        // v1 is HMAC-SHA256 over "{timestamp}.{payload}", hex-encoded.
        let expected_v1 = hmac_sha256(b"whsec", b"1234567890.hello", "hex").unwrap();
        assert_eq!(header, format!("t=1234567890,v1={}", expected_v1));
    }

    #[test]
    fn test_builtin_stripe_sign_default_timestamp() {
        let header = match eval("$.stripeSign(\"whsec\", \"hello\")") {
            Value::String(s) => s,
            _ => panic!("expected string"),
        };
        assert!(header.starts_with("t="), "got: {}", header);
        let v1 = header.split(",v1=").nth(1).expect("has v1 segment");
        assert_eq!(v1.len(), 64); // SHA-256 hex digest is 64 chars
        assert!(v1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // --- retry block ---

    #[test]
    fn test_retry_passes_first_attempt() {
        let (scope, failures) = exec("retry(max: 3, interval: 1ms) { x = 1; x == 1 | \"nope\"; }");
        assert!(failures.is_empty(), "expected clean pass, got {:?}", failures);
        assert_eq!(scope.get("x"), Value::Number(1.0));
    }

    #[test]
    fn test_retry_passes_on_nth_attempt() {
        // Body increments n each attempt and asserts n >= 3 — passes on attempt 3.
        let (scope, failures) = exec(
            "n = 0; retry(max: 5, interval: 1ms) { n = n + 1; n >= 3 | \"not yet\"; }",
        );
        assert!(failures.is_empty(), "expected eventual pass, got {:?}", failures);
        assert_eq!(scope.get("n"), Value::Number(3.0));
    }

    #[test]
    fn test_retry_exhausts_max() {
        // Never satisfiable within 2 attempts — should give up and report.
        let (scope, failures) = exec(
            "n = 0; retry(max: 2, interval: 1ms) { n = n + 1; n >= 5 | \"not yet\"; }",
        );
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].message.contains("retry exhausted after 2 attempts"),
            "got: {}", failures[0].message,
        );
        // The inner assertion message is preserved.
        assert!(failures[0].message.contains("not yet"));
        assert_eq!(scope.get("n"), Value::Number(2.0));
    }

    #[test]
    fn test_retry_exhausts_on_timeout() {
        // No max, time-bounded. A never-true assertion loops until the timeout.
        let (_scope, failures) = exec(
            "retry(timeout: 5ms, interval: 1ms) { false | \"never ready\"; }",
        );
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].message.contains("retry exhausted"),
            "got: {}", failures[0].message,
        );
    }

    #[test]
    fn test_retry_rejects_return_in_body() {
        // Control-flow statements don't compose with re-execution.
        let (_scope, failures) = exec("retry(max: 2, interval: 1ms) { x = 1; return x; }");
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].message.contains("return is not allowed inside a retry block"),
            "got: {}", failures[0].message,
        );
    }
}
