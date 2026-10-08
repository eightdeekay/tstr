//! Browser-ish helpers for driving login flows from a test: cookies and HTML
//! forms. Deliberately general — tstr has no OAuth command; a suite scripts
//! the flow itself from these pieces (see README "Cookies, redirects, forms").
//!
//! Cookies stay plain values. `_response.cookies` is a parsed map, a request
//! sends whatever `req.cookies` holds, and `$.cookies` merges maps. There is no
//! mutable jar and no domain matching: the test decides what goes where, which
//! matters when a suite rewrites a public host to a direct service URL.

use std::sync::OnceLock;

use regex::Regex;

use crate::value::{Value, ValueMap};

/// Parse every `Set-Cookie` header value into `name → { value, path, domain,
/// maxAge, expires, sameSite, secure, httpOnly }`. Absent attributes are null
/// (the two flags are false). A cookie set twice in one response: last wins.
/// A header with no `name=value` pair is ignored, as browsers do.
pub fn parse_set_cookies<'a>(headers: impl IntoIterator<Item = &'a str>) -> ValueMap {
    let mut out = ValueMap::new();
    for header in headers {
        if let Some((name, cookie)) = parse_set_cookie(header) {
            out.insert(name, Value::Object(cookie));
        }
    }
    out
}

fn parse_set_cookie(header: &str) -> Option<(String, ValueMap)> {
    let mut parts = header.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }

    let mut path = Value::Null;
    let mut domain = Value::Null;
    let mut max_age = Value::Null;
    let mut expires = Value::Null;
    let mut same_site = Value::Null;
    let mut secure = false;
    let mut http_only = false;
    for attr in parts {
        let (key, val) = match attr.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (attr.trim(), ""),
        };
        match key.to_ascii_lowercase().as_str() {
            "path" => path = Value::String(val.to_string()),
            "domain" => domain = Value::String(val.to_string()),
            // A Max-Age that isn't an integer is ignored (RFC 6265 §5.2.2).
            "max-age" => {
                if let Ok(n) = val.parse::<i64>() {
                    max_age = Value::Number(n as f64);
                }
            }
            "expires" => expires = Value::String(val.to_string()),
            "samesite" => same_site = Value::String(val.to_string()),
            "secure" => secure = true,
            "httponly" => http_only = true,
            _ => {}
        }
    }

    let mut cookie = ValueMap::new();
    cookie.insert("value".to_string(), Value::String(value.trim().to_string()));
    cookie.insert("path".to_string(), path);
    cookie.insert("domain".to_string(), domain);
    cookie.insert("maxAge".to_string(), max_age);
    cookie.insert("expires".to_string(), expires);
    cookie.insert("sameSite".to_string(), same_site);
    cookie.insert("secure".to_string(), Value::Bool(secure));
    cookie.insert("httpOnly".to_string(), Value::Bool(http_only));
    Some((name.to_string(), cookie))
}

/// A cookie's value, from either shape a map may hold: a bare value, or a
/// parsed `{ value: … }` object straight from `_response.cookies`.
fn cookie_value(name: &str, v: &Value) -> Result<String, String> {
    match v {
        Value::Object(m) => match m.get("value") {
            Some(Value::Null) | None => Err(format!("cookie '{}' has no `value`", name)),
            Some(inner) => cookie_value(name, inner),
        },
        Value::Array(_) => Err(format!("cookie '{}' must be a value or {{ value: … }}, got array", name)),
        Value::Null => Err(format!("cookie '{}' is null", name)),
        other => Ok(other.to_display_string()),
    }
}

/// The `Cookie` request header for `req.cookies`: `a=1; b=2`, in map order.
pub fn cookie_header(cookies: &ValueMap) -> Result<String, String> {
    let mut pairs = Vec::with_capacity(cookies.len());
    for (name, v) in cookies {
        pairs.push(format!("{}={}", name, cookie_value(name, v)?));
    }
    Ok(pairs.join("; "))
}

/// Whether a cookie entry is a deletion: an empty value, or `Max-Age <= 0`.
fn is_deletion(v: &Value) -> bool {
    match v {
        Value::String(s) => s.is_empty(),
        Value::Object(m) => {
            let empty = matches!(m.get("value"), Some(Value::String(s)) if s.is_empty());
            let expired = matches!(m.get("maxAge"), Some(Value::Number(n)) if *n <= 0.0);
            empty || expired
        }
        _ => false,
    }
}

/// `$.cookies(jar, more, …)`: merge cookie maps left to right. Later entries
/// replace earlier ones; a deletion (empty value, `Max-Age<=0`) removes the
/// cookie. Null arguments are skipped, so a first call can start from nothing.
pub fn merge_cookies(maps: &[Value]) -> Result<ValueMap, String> {
    let mut jar = ValueMap::new();
    for (i, m) in maps.iter().enumerate() {
        match m {
            Value::Null => {}
            Value::Object(map) => {
                for (name, v) in map {
                    if is_deletion(v) {
                        jar.shift_remove(name);
                    } else {
                        jar.insert(name.clone(), v.clone());
                    }
                }
            }
            other => {
                return Err(format!(
                    "$.cookies() argument {} must be a cookie map, got {}",
                    i + 1, other.type_name()
                ))
            }
        }
    }
    Ok(jar)
}

/// Which form `$.form` reads when a page has several.
pub enum FormPick {
    First,
    Index(usize),
    /// Matches the form's `id`, else its `name`.
    Id(String),
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

/// `$.form(html[, idOrIndex])` → `{ action, method, fields }`. `action` is the
/// raw attribute with entities decoded (relative actions are returned as
/// written; `""` when absent). `method` is upper-cased, default `GET`.
/// `fields` holds the named `<input>`s a browser would submit without a click:
/// submit/button/image/reset/file inputs are left out, a checkbox or radio
/// only when `checked`. A name that repeats becomes an array.
pub fn extract_form(html: &str, pick: &FormPick) -> Result<ValueMap, String> {
    static FORM: OnceLock<Regex> = OnceLock::new();
    static INPUT: OnceLock<Regex> = OnceLock::new();
    let forms: Vec<(ValueMap, &str)> = re(&FORM, r"(?is)<form\b([^>]*)>(.*?)</form\s*>")
        .captures_iter(html)
        .map(|c| (attrs(c.get(1).unwrap().as_str()), c.get(2).unwrap().as_str()))
        .collect();
    if forms.is_empty() {
        return Err("no <form> found in the HTML".to_string());
    }

    let (form_attrs, body) = match pick {
        FormPick::First => &forms[0],
        FormPick::Index(i) => forms.get(*i).ok_or_else(|| {
            format!("form index {} out of range — the page has {} form(s)", i, forms.len())
        })?,
        FormPick::Id(id) => {
            let by = |key: &str| forms.iter().find(|(a, _)| a.get(key).map(|v| v.to_display_string()).as_deref() == Some(id.as_str()));
            by("id").or_else(|| by("name")).ok_or_else(|| {
                let ids: Vec<String> = forms.iter()
                    .filter_map(|(a, _)| a.get("id").or_else(|| a.get("name")).map(|v| format!("'{}'", v.to_display_string())))
                    .collect();
                format!("no form with id or name '{}' (found: {})", id,
                    if ids.is_empty() { "none named".to_string() } else { ids.join(", ") })
            })?
        }
    };

    let mut fields = ValueMap::new();
    for c in re(&INPUT, r"(?is)<input\b([^>]*)>").captures_iter(body) {
        let a = attrs(c.get(1).unwrap().as_str());
        let Some(name) = a.get("name").map(|v| v.to_display_string()) else { continue };
        let ty = a.get("type").map(|v| v.to_display_string().to_ascii_lowercase()).unwrap_or_default();
        match ty.as_str() {
            "submit" | "button" | "image" | "reset" | "file" => continue,
            "checkbox" | "radio" if !a.contains_key("checked") => continue,
            _ => {}
        }
        let default = if matches!(ty.as_str(), "checkbox" | "radio") { "on" } else { "" };
        let value = a.get("value").cloned().unwrap_or_else(|| Value::String(default.to_string()));
        match fields.get_mut(&name) {
            None => { fields.insert(name, value); }
            Some(Value::Array(vals)) => vals.push(value),
            Some(existing) => *existing = Value::Array(vec![existing.clone(), value]),
        }
    }

    let get = |k: &str| form_attrs.get(k).map(|v| v.to_display_string());
    let mut out = ValueMap::new();
    out.insert("action".to_string(), Value::String(get("action").unwrap_or_default()));
    out.insert("method".to_string(), Value::String(get("method").unwrap_or_else(|| "get".to_string()).to_ascii_uppercase()));
    out.insert("fields".to_string(), Value::Object(fields));
    Ok(out)
}

/// A tag's attributes, names lower-cased and values entity-decoded. A bare
/// attribute (`checked`) maps to `""`.
fn attrs(src: &str) -> ValueMap {
    static ATTR: OnceLock<Regex> = OnceLock::new();
    let mut out = ValueMap::new();
    let pattern = r#"(?s)([^\s"'<>/=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?"#;
    for c in re(&ATTR, pattern).captures_iter(src) {
        let name = c.get(1).unwrap().as_str().to_ascii_lowercase();
        let raw = c.get(2).or(c.get(3)).or(c.get(4)).map(|m| m.as_str()).unwrap_or("");
        // First occurrence wins, as in an HTML parser.
        out.entry(name).or_insert_with(|| Value::String(decode_entities(raw)));
    }
    out
}

/// Decode HTML character references: the common named ones and numeric
/// (`&#39;`, `&#x27;`). Anything unrecognized is left as written.
pub fn decode_entities(s: &str) -> String {
    static ENTITY: OnceLock<Regex> = OnceLock::new();
    re(&ENTITY, r"&(#[0-9]+|#[xX][0-9a-fA-F]+|[a-zA-Z]+);")
        .replace_all(s, |c: &regex::Captures| {
            let body = &c[1];
            let decoded = if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = body.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)
            } else {
                match body {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some('\u{a0}'),
                    _ => None,
                }
            };
            decoded.map(String::from).unwrap_or_else(|| c[0].to_string())
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn field<'a>(m: &'a ValueMap, k: &str) -> &'a Value {
        m.get(k).unwrap_or_else(|| panic!("missing '{}' in {:?}", k, m))
    }

    fn obj(v: &Value) -> &ValueMap {
        match v {
            Value::Object(m) => m,
            other => panic!("expected object, got {:?}", other),
        }
    }

    #[test]
    fn parses_every_set_cookie_with_attributes() {
        let jar = parse_set_cookies([
            "layer_login_abc=nonce123; Path=/auth; Max-Age=600; SameSite=None; Secure; HttpOnly",
            "layer_redirect=%2Fhome; Path=/; Domain=.example.com; Expires=Wed, 21 Oct 2026 07:28:00 GMT",
        ]);
        assert_eq!(jar.len(), 2);
        let state = obj(field(&jar, "layer_login_abc"));
        assert_eq!(field(state, "value"), &s("nonce123"));
        assert_eq!(field(state, "path"), &s("/auth"));
        assert_eq!(field(state, "maxAge"), &Value::Number(600.0));
        assert_eq!(field(state, "sameSite"), &s("None"));
        assert_eq!(field(state, "secure"), &Value::Bool(true));
        assert_eq!(field(state, "httpOnly"), &Value::Bool(true));
        assert_eq!(field(state, "domain"), &Value::Null);

        let redirect = obj(field(&jar, "layer_redirect"));
        assert_eq!(field(redirect, "domain"), &s(".example.com"));
        // The comma inside Expires must not split the cookie.
        assert_eq!(field(redirect, "expires"), &s("Wed, 21 Oct 2026 07:28:00 GMT"));
        assert_eq!(field(redirect, "secure"), &Value::Bool(false));
        assert_eq!(field(redirect, "maxAge"), &Value::Null);
    }

    #[test]
    fn value_keeps_equals_signs_and_bad_headers_are_ignored() {
        let jar = parse_set_cookies(["tok=a=b==; path=/", "novalue", "=anon"]);
        assert_eq!(jar.len(), 1);
        assert_eq!(field(obj(field(&jar, "tok")), "value"), &s("a=b=="));
    }

    #[test]
    fn max_age_zero_deletes_on_merge() {
        let first = Value::Object(parse_set_cookies(["KC_RESTART=abc; Path=/", "AUTH_SESSION_ID=s1; Path=/"]));
        let second = Value::Object(parse_set_cookies([
            "KC_RESTART=; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            "KEYCLOAK_IDENTITY=id1; Path=/",
        ]));
        let jar = merge_cookies(&[Value::Null, first, second]).unwrap();
        let names: Vec<&str> = jar.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["AUTH_SESSION_ID", "KEYCLOAK_IDENTITY"]);
    }

    #[test]
    fn merge_later_wins_and_accepts_bare_values() {
        let mut a = ValueMap::new();
        a.insert("x".to_string(), s("1"));
        a.insert("y".to_string(), s("2"));
        let mut b = ValueMap::new();
        b.insert("x".to_string(), s("3"));
        b.insert("y".to_string(), s(""));
        let jar = merge_cookies(&[Value::Object(a), Value::Object(b)]).unwrap();
        assert_eq!(jar.get("x"), Some(&s("3")));
        assert!(!jar.contains_key("y"), "empty value deletes");
        assert!(merge_cookies(&[s("nope")]).is_err());
    }

    #[test]
    fn cookie_header_accepts_both_shapes() {
        let mut m = parse_set_cookies(["sid=abc; Secure"]);
        m.insert("lang".to_string(), s("en"));
        assert_eq!(cookie_header(&m).unwrap(), "sid=abc; lang=en");
        let mut bad = ValueMap::new();
        bad.insert("x".to_string(), Value::Null);
        assert!(cookie_header(&bad).is_err());
    }

    const KEYCLOAK: &str = r#"
        <html><body>
        <form id="kc-locale" action="/locale"><input type="hidden" name="kc_locale" value="en"></form>
        <form id="kc-form-login" onsubmit="login.disabled = true; return true;"
              action="https://kc.example.com/realms/layer/login-actions/authenticate?session_code=sc1&amp;execution=ex2&amp;client_id=sso&amp;tab_id=t3"
              method="post">
            <input tabindex="1" id="username" name="username" value="" type="text" autofocus>
            <input tabindex="2" id="password" name="password" type="password">
            <input type="hidden" id="id-hidden-input" name="credentialId" value=""/>
            <input type="checkbox" name="rememberMe"> <input type='checkbox' name='terms' checked>
            <input tabindex="4" name="login" id="kc-login" type="submit" value="Sign In"/>
        </form></body></html>"#;

    #[test]
    fn form_by_id_decodes_action_and_skips_submit() {
        let f = extract_form(KEYCLOAK, &FormPick::Id("kc-form-login".to_string())).unwrap();
        assert_eq!(field(&f, "action"), &s(
            "https://kc.example.com/realms/layer/login-actions/authenticate?session_code=sc1&execution=ex2&client_id=sso&tab_id=t3"));
        assert_eq!(field(&f, "method"), &s("POST"));
        let fields = obj(field(&f, "fields"));
        let names: Vec<&str> = fields.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["username", "password", "credentialId", "terms"]);
        assert_eq!(field(fields, "terms"), &s("on"));
    }

    #[test]
    fn form_by_index_and_default_is_first() {
        let first = extract_form(KEYCLOAK, &FormPick::First).unwrap();
        assert_eq!(field(&first, "action"), &s("/locale"));
        assert_eq!(field(&first, "method"), &s("GET"));
        let second = extract_form(KEYCLOAK, &FormPick::Index(1)).unwrap();
        assert_eq!(field(&second, "method"), &s("POST"));
        let err = extract_form(KEYCLOAK, &FormPick::Index(2)).unwrap_err();
        assert!(err.contains("2 form(s)"), "{}", err);
        let err = extract_form(KEYCLOAK, &FormPick::Id("nope".to_string())).unwrap_err();
        assert!(err.contains("'kc-locale'"), "{}", err);
    }

    #[test]
    fn auto_post_form_hidden_inputs_with_entities() {
        let html = r#"<HTML><BODY onload="document.forms[0].submit()">
            <FORM METHOD="POST" ACTION="https://site.example.com/auth/abc/token">
              <INPUT TYPE="HIDDEN" NAME="code" VALUE="c0de&#x2e;1&#46;2" />
              <INPUT TYPE="HIDDEN" NAME="state" VALUE="st&amp;te" />
              <NOSCRIPT><INPUT TYPE="SUBMIT" VALUE="CONTINUE" /></NOSCRIPT>
            </FORM></BODY></HTML>"#;
        let f = extract_form(html, &FormPick::First).unwrap();
        assert_eq!(field(&f, "action"), &s("https://site.example.com/auth/abc/token"));
        let fields = obj(field(&f, "fields"));
        assert_eq!(field(fields, "code"), &s("c0de.1.2"));
        assert_eq!(field(fields, "state"), &s("st&te"));
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn repeated_input_names_become_arrays() {
        let html = r#"<form><input name="scope" value="a"><input name="scope" value="b"></form>"#;
        let f = extract_form(html, &FormPick::First).unwrap();
        assert_eq!(field(obj(field(&f, "fields")), "scope"), &Value::Array(vec![s("a"), s("b")]));
    }

    #[test]
    fn no_form_is_an_error() {
        assert!(extract_form("<p>hi</p>", &FormPick::First).is_err());
    }

    #[test]
    fn unknown_entities_pass_through() {
        assert_eq!(decode_entities("a&amp;b &bogus; &#169; &lt;x&gt;"), "a&b &bogus; © <x>");
    }
}
