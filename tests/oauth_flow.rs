//! End-to-end: a `.test.tstr` drives an authorization-code login with
//! `response_mode=form_post` against two local fake servers — "sso" and
//! "keycloak" — using only general features: `req.follow = false`,
//! `_response.cookies`, `req.cookies`, `$.cookies`, `$.form` and `req.form`.
//! The fakes check every cookie and form field they should receive and answer
//! 400 with the reason otherwise, so the test passing means the wire was right.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use tstr::output::{BarStyle, OutputMode, Printer};
use tstr::runner::{run_structural, RunOptions};
use tstr::value::ValueMap;

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> &str {
        self.headers.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }
}

fn read_request(stream: &mut TcpStream) -> Request {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).unwrap();
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len: usize = headers.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body).unwrap();
    Request { method, target, headers, body: String::from_utf8(body).unwrap() }
}

/// (status, extra headers, body)
type Reply = (u16, Vec<(&'static str, String)>, String);

fn serve(listener: TcpListener, handler: impl Fn(&Request) -> Reply + Send + 'static) {
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let req = read_request(&mut stream);
            let (code, headers, body) = handler(&req);
            let mut out = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n", code, body.len());
            for (k, v) in headers {
                out.push_str(&format!("{}: {}\r\n", k, v));
            }
            out.push_str("\r\n");
            out.push_str(&body);
            let _ = stream.write_all(out.as_bytes());
        }
    });
}

fn bad(why: String) -> Reply {
    (400, vec![], why)
}

#[test]
fn form_post_login_flow_end_to_end() {
    let sso_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let kc_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let sso = format!("http://{}", sso_listener.local_addr().unwrap());
    let kc = format!("http://{}", kc_listener.local_addr().unwrap());

    // sso: authorize sets two cookies and redirects to keycloak; the token
    // endpoint needs the state cookie and the posted code/state.
    {
        let kc = kc.clone();
        serve(sso_listener, move |r| match (r.method.as_str(), r.target.as_str()) {
            ("GET", "/auth/client1/authorize") => (302, vec![
                ("Location", format!("{}/realms/layer/protocol/openid-connect/auth?client_id=sso&state=st1&response_mode=form_post", kc)),
                ("Set-Cookie", "layer_login_st1=nonce1; Path=/auth; Max-Age=600; SameSite=None; Secure; HttpOnly".into()),
                ("Set-Cookie", "layer_redirect=%2Fhome; Path=/; Expires=Wed, 21 Oct 2026 07:28:00 GMT".into()),
            ], String::new()),
            ("POST", "/auth/client1/token") => {
                if !r.header("cookie").contains("layer_login_st1=nonce1") {
                    return bad(format!("state cookie missing: {:?}", r.header("cookie")));
                }
                if r.header("content-type") != "application/x-www-form-urlencoded" {
                    return bad(format!("content-type {:?}", r.header("content-type")));
                }
                if r.body != "code=c0de.1&state=st1" {
                    return bad(format!("token body {:?}", r.body));
                }
                (302, vec![
                    ("Location", "/home".into()),
                    ("Set-Cookie", "layer_account=acct1; Path=/; SameSite=Lax; HttpOnly".into()),
                    ("Set-Cookie", "layer_login_st1=; Path=/auth; Max-Age=0".into()),
                ], String::new())
            }
            _ => bad(format!("sso: unexpected {} {}", r.method, r.target)),
        });
    }

    // keycloak: the login page carries an entity-escaped absolute action; the
    // login POST needs its session cookie and credentials, and answers with
    // the auto-submitting form_post page.
    {
        let (sso, kc) = (sso.clone(), kc.clone());
        serve(kc_listener, move |r| {
            let path = r.target.split('?').next().unwrap_or("");
            match (r.method.as_str(), path) {
                ("GET", "/realms/layer/protocol/openid-connect/auth") => (200, vec![
                    ("Set-Cookie", "AUTH_SESSION_ID=as1; Path=/realms/layer/; SameSite=None; Secure; HttpOnly".into()),
                    ("Set-Cookie", "KC_RESTART=kr1; Path=/realms/layer/; HttpOnly".into()),
                ], format!(r#"<html><body>
                    <form id="kc-form-login" action="{kc}/realms/layer/login-actions/authenticate?session_code=sc1&amp;execution=ex2&amp;tab_id=t3" method="post">
                      <input id="username" name="username" type="text" value="">
                      <input id="password" name="password" type="password">
                      <input type="hidden" name="credentialId" value="">
                      <input name="login" type="submit" value="Sign In">
                    </form></body></html>"#)),
                ("POST", "/realms/layer/login-actions/authenticate") => {
                    if r.target != "/realms/layer/login-actions/authenticate?session_code=sc1&execution=ex2&tab_id=t3" {
                        return bad(format!("action not decoded: {}", r.target));
                    }
                    if !r.header("cookie").contains("AUTH_SESSION_ID=as1") {
                        return bad(format!("session cookie missing: {:?}", r.header("cookie")));
                    }
                    if r.body != "username=doug&password=p%40ss&credentialId=" {
                        return bad(format!("login body {:?}", r.body));
                    }
                    (200, vec![
                        ("Set-Cookie", "KC_RESTART=; Path=/realms/layer/; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT".into()),
                        ("Set-Cookie", "KEYCLOAK_IDENTITY=id1; Path=/realms/layer/; HttpOnly".into()),
                    ], format!(r#"<HTML><BODY onload="document.forms[0].submit()">
                      <FORM METHOD="POST" ACTION="{sso}/auth/client1/token">
                        <INPUT TYPE="HIDDEN" NAME="code" VALUE="c0de&#46;1"/>
                        <INPUT TYPE="HIDDEN" NAME="state" VALUE="st1"/>
                        <NOSCRIPT><INPUT TYPE="SUBMIT" VALUE="CONTINUE"/></NOSCRIPT>
                      </FORM></BODY></HTML>"#))
                }
                _ => bad(format!("keycloak: unexpected {} {}", r.method, r.target)),
            }
        });
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("tstr.yaml"), "constants: {}\n").unwrap();
    std::fs::write(root.join("01-login.test.tstr"), format!(r#"--> {{
  sso = "{sso}";
  req = {{ follow: false }};

  // 1. sso authorize: two cookies, 302 to keycloak.
  r = req.get("{{{{sso}}}}/auth/client1/authorize") ? 302 | "authorize should redirect";
  kcUrl = _response.headers.location;
  kcUrl ~? /openid-connect\/auth/ | "redirect should go to keycloak: {{{{kcUrl}}}}";
  state = _response.cookies.layer_login_st1;
  state.value == "nonce1" | "state cookie value";
  state.sameSite == "None" | "state cookie SameSite";
  state.secure == true | "state cookie Secure";
  state.httpOnly == true | "state cookie HttpOnly";
  state.maxAge == 600 | "state cookie Max-Age";
  _response.cookies.layer_redirect.expires == "Wed, 21 Oct 2026 07:28:00 GMT" | "Expires kept whole";
  ssoJar = _response.cookies;

  // 2. keycloak login page.
  r = req.get(kcUrl) ? 200 | "login page";
  kcJar = _response.cookies;
  login = $.form(_response.text, "kc-form-login");
  login.method == "POST" | "login form method";
  fields = login.fields;
  fields.username = "doug";
  fields.password = "p@ss";

  // 3. post credentials with keycloak's cookies.
  creds = {{ follow: false, cookies: kcJar, form: fields }};
  r = creds.post(login.action) ? 200 | "login post: {{{{_response.text}}}}";
  kcJar = $.cookies(kcJar, _response.cookies);
  kcJar.KC_RESTART == null | "Max-Age=0 should delete KC_RESTART";
  kcJar.KEYCLOAK_IDENTITY.value == "id1" | "identity cookie";

  // 4. the form_post hop back to sso, with sso's state cookie.
  cb = $.form(_response.text);
  tok = {{ follow: false, cookies: ssoJar, form: cb.fields }};
  r = tok.post(cb.action) ? 302 | "token: {{{{_response.text}}}}";
  _response.headers.location == "/home" | "redirect to the saved page";
  _response.url == "{sso}/auth/client1/token" | "unfollowed response url";
  _response.cookies.layer_account.value == "acct1" | "account cookie";
  ssoJar = $.cookies(ssoJar, _response.cookies);
  ssoJar.layer_login_st1 == null | "state cookie cleared";
}}
"#)).unwrap();

    let suite = tstr::discovery::discover(root).unwrap();
    let index = tstr::scheduler::FileIndex::build(suite.clone(), root.to_path_buf());
    let printer = Arc::new(Printer::new(OutputMode::Quiet, BarStyle::Auto));
    let totals = run_structural(&suite, &index, &ValueMap::new(), &RunOptions::default(), &printer);

    if totals.passed != 1 {
        // Re-run through the normal printer so the failure reason is visible.
        let printer = Arc::new(Printer::new(OutputMode::Verbose, BarStyle::Auto));
        run_structural(&suite, &index, &ValueMap::new(), &RunOptions::default(), &printer);
    }
    assert_eq!((totals.passed, totals.failed, totals.skipped), (1, 0, 0));
}

#[test]
fn follow_default_still_follows_and_reports_final_url() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    serve(listener, |r| match r.target.as_str() {
        "/start" => (302, vec![("Location", "/end".into())], String::new()),
        "/end" => (200, vec![], "{\"ok\": true}".into()),
        _ => bad(r.target.clone()),
    });

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("tstr.yaml"), "constants: {}\n").unwrap();
    std::fs::write(root.join("01-follow.test.tstr"), format!(r#"--> {{
  r = {{}}.get("{base}/start") ? 200 | "should land on /end";
  r.ok == true | "body from the final hop";
  _response.url == "{base}/end" | "final url";
}}
"#)).unwrap();
    let suite = tstr::discovery::discover(root).unwrap();
    let index = tstr::scheduler::FileIndex::build(suite.clone(), root.to_path_buf());
    let printer = Arc::new(Printer::new(OutputMode::Quiet, BarStyle::Auto));
    let totals = run_structural(&suite, &index, &ValueMap::new(), &RunOptions::default(), &printer);
    assert_eq!((totals.passed, totals.failed, totals.skipped), (1, 0, 0));
}
