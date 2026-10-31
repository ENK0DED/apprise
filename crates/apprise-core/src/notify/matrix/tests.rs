//! Request expectations below were checked against upstream with
//! `pycapture.py` (login, register, joins, sends, uploads, webhooks) and the
//! apprise-go Matrix goldens.

use super::*;
use crate::notify::registry::from_url;
use crate::notify::with_test_base;
use crate::{Apprise, AppriseAsset};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn load(url: &str) -> Matrix {
  Matrix::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("{url} loads"))
}

fn ctx(title: &str, body: &str) -> NotifyContext {
  NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
}

async fn mock(server: &MockServer, verb: &str, route: &str, status: u16, body: Value) {
  Mock::given(method(verb)).and(path(route)).respond_with(ResponseTemplate::new(status).set_body_json(body)).mount(server).await;
}

async fn mock_re(server: &MockServer, verb: &str, route: &str, status: u16, body: Value) {
  Mock::given(method(verb)).and(path_regex(route)).respond_with(ResponseTemplate::new(status).set_body_json(body)).mount(server).await;
}

/// The Matrix requests a server saw, without the stray probes some local
/// processes send to every port.
async fn seen(server: &MockServer) -> Vec<Request> {
  server.received_requests().await.unwrap().into_iter().filter(|r| r.url.path() != "/").collect()
}

fn line(r: &Request) -> String {
  format!("{} {}", r.method, r.url.path())
}

fn body(r: &Request) -> Value {
  serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

fn sends(requests: &[Request]) -> Vec<&Request> {
  requests.iter().filter(|r| r.url.path().contains("/send/")).collect()
}

/// Answer a join with the room it names (`#alias:hs` becomes `!alias:hs`).
struct JoinEcho;

impl Respond for JoinEcho {
  fn respond(&self, request: &Request) -> ResponseTemplate {
    let room = crate::utils::parse::unquote(request.url.path().rsplit('/').next().unwrap());
    let room_id = format!("!{}", room.trim_start_matches(['!', '#']));
    ResponseTemplate::new(200).set_body_json(json!({"room_id": room_id}))
  }
}

/// A homeserver that accepts logins, joins and room events.
async fn homeserver(server: &MockServer) {
  mock_re(server, "POST", r"^/_matrix/client/(v3|r0)/login$", 200, json!({"access_token": "abcd1234", "user_id": "@apprise:localhost", "device_id": "DEVICE"}))
    .await;
  Mock::given(method("POST")).and(path_regex(r"^/_matrix/client/(v3|r0)/join/")).respond_with(JoinEcho).mount(server).await;
  mock_re(server, "PUT", r"/send/", 200, json!({"event_id": "$event"})).await;
  mock_re(server, "GET", r"/account/whoami$", 200, json!({"user_id": "@apprise:localhost", "device_id": "DEVICE"})).await;
}

async fn send(m: &Matrix, server: &MockServer, c: &NotifyContext) -> bool {
  with_test_base(server.uri(), m.send(c)).await.unwrap()
}

// ── URLs ─────────────────────────────────────────────────────────────────────

#[test]
fn test_upstream_url_table() {
  // Upstream apprise_url_tests: what loads and what does not.
  let valid = [
    "matrix://localhost?mode=off".to_string(),
    "matrix://user:pass@localhost/#room1/#room2/#room3".into(),
    "matrix://user:pass@localhost/#room1/#room2/!room1".into(),
    "matrix://user:pass@localhost:1234/#room".into(),
    "matrix://user:token@localhost?mode=matrix&format=text".into(),
    "matrix://user:token@localhost?mode=matrix&format=html".into(),
    "matrix://user:token@localhost:123/#general/?version=3".into(),
    "matrixs://user:token@localhost/#general?v=2".into(),
    "matrix://user:token@localhost?mode=slack&format=text".into(),
    "matrixs://user:token@localhost?mode=SLACK&format=markdown".into(),
    "matrix://user@localhost?mode=SLACK&format=markdown&token=mytoken".into(),
    format!("matrix://_?mode=t2bot&token={}", "b".repeat(64)),
    "matrixs://hookuser:hooktoken@hookshot.example?mode=hookshot&path=%2Fpublic-hooks".into(),
    "matrixs://hookuser:hooktoken@hookshot.example?mode=hookshot&path=public-hooks".into(),
    "matrixs://hooktoken@hookshot.example?mode=hookshot".into(),
    "matrixs://user:token@localhost?mode=slack&format=markdown&image=True".into(),
    "matrixs://user:token@localhost?mode=slack&format=markdown&image=False".into(),
    format!("matrixs://user@{}?mode=t2bot&format=markdown&image=True", "a".repeat(64)),
    format!("matrix://user@{}?mode=t2bot&format=html&image=False", "z".repeat(64)),
    format!("matrixs://{}", "c".repeat(64)),
    format!("https://webhooks.t2bot.io/api/v1/matrix/hook/{}/", "d".repeat(64)),
    "matrix://token@localhost/?mode=Matrix".into(),
    "matrix://user:token@localhost/mode=matrix".into(),
    "matrix://token@localhost:8080/?mode=slack".into(),
    format!("matrix://{}/?mode=t2bot", "b".repeat(64)),
    "matrix://user:passwd@hostname/#abcd?msgtype=notice".into(),
  ];
  for url in &valid {
    assert!(from_url(url).is_some(), "should load: {url}");
  }
  let invalid = [
    "matrix://",
    "matrixs://",
    // t2bot (no password or target), and the host is no webhook ID.
    "matrix://localhost",
    "matrix://user:token@localhost:123/#general/?v=invalid",
    "matrixs://user:pass@hostname:port/#room_alias",
    "matrixs://user:pass@hostname:0/#room_alias",
    "matrixs://user:pass@hostname:65536/#room_alias",
    "matrix://user:token@localhost?mode=On",
    "matrix://user:passwd@hostname/#abcd?msgtype=invalid",
    "matrix://user:pass@host_name!/#room",
  ];
  for url in invalid {
    assert!(from_url(url).is_none(), "should not load: {url}");
  }
}

#[test]
fn test_targets_and_options() {
  // Targets are sorted and deduplicated; @users become DM targets.
  let m = load("matrixs://user:pass@example.com/%23zeta/!abc:example.com/@bob/@carol:other.org/%23zeta?to=%23alpha,@dave");
  assert_eq!(m.rooms, vec!["!abc:example.com", "#alpha", "#zeta"]);
  assert_eq!(m.users, vec!["@bob", "@carol:other.org", "@dave"]);
  assert_eq!(m.mode, Mode::Off);
  assert_eq!(m.version, Version::V3);
  assert_eq!(m.msgtype, MsgType::Text);
  assert!(m.discovery && m.hsreq && m.e2ee && !m.autoverify && !m.include_image);
  assert_eq!((m.user.as_deref(), m.password.as_deref()), (Some("user"), Some("pass")));

  // A lone user is the access token.
  let m = load("matrix://mytoken@localhost/#room");
  assert_eq!((m.user.as_deref(), m.password.as_deref()), (None, Some("mytoken")));
  assert!(m.token_auth());
  // ?token= wins over the URL's password.
  let m = load("matrix://user:pass@localhost/#room?token=other");
  assert_eq!((m.user.as_deref(), m.password.as_deref()), (Some("user"), Some("other")));

  let m = load("matrix://user:pass@localhost/#room?v=2&msgtype=NOTICE&image=yes&discovery=no&hsreq=no&e2ee=no&autoverify=yes");
  assert_eq!(m.version, Version::V2);
  assert_eq!(m.msgtype, MsgType::Notice);
  assert!(m.include_image && !m.discovery && !m.hsreq && !m.e2ee && m.autoverify);
  // As upstream's parse_bool(): anything unrecognized is false.
  let m = load("matrix://user:pass@localhost/#room?discovery=maybe&hsreq=maybe");
  assert!(!m.discovery && !m.hsreq);
  // autoverify implies e2ee unless e2ee= says otherwise.
  assert!(load("matrix://user:pass@localhost/#room?autoverify=yes").e2ee);

  // Webhook modes never discover.
  assert!(!load("matrixs://user:token@localhost?mode=matrix").discovery);
  // No credentials or targets: t2bot, the host is the webhook ID.
  let token = "c".repeat(64);
  let m = load(&format!("matrixs://{token}"));
  assert_eq!(m.mode, Mode::T2Bot);
  assert_eq!(m.t2bot_token.as_deref(), Some(token.as_str()));
  // A target alone means server mode (anonymous registration).
  assert_eq!(load("matrix://localhost/#room").mode, Mode::Off);
}

#[test]
fn test_hookshot_path_normalization() {
  for (arg, expected) in [("", "/webhook"), ("&path=hooks", "/hooks"), ("&path=%2Fhooks%2F", "/hooks"), ("&path=%2F", "/"), ("&path=%2Fa%2Fb", "/a/b")] {
    assert_eq!(load(&format!("matrixs://tok@hookshot.example?mode=hookshot{arg}")).webhook_path, expected, "{arg}");
  }
}

#[test]
fn test_identifiers() {
  assert_eq!(parse_room_id("!abc:example.com"), Some(("abc".into(), Some("example.com".into()))));
  assert_eq!(parse_room_id(" %21abc%3Aexample.com "), Some(("abc".into(), Some("example.com".into()))));
  assert_eq!(parse_room_id("&#33;abc"), Some(("abc".into(), None)));
  assert_eq!(parse_room_id("#abc"), None);
  assert_eq!(parse_room_id("!abc:"), None);
  assert_eq!(parse_room_alias("#general"), Some(("general".into(), None)));
  assert_eq!(parse_room_alias("general:hs.org"), Some(("general".into(), Some("hs.org".into()))));
  assert_eq!(parse_room_alias("%23gen.eral%3ahs"), Some(("gen.eral".into(), Some("hs".into()))));
  assert_eq!(parse_room_alias("#bad room"), None);
  assert_eq!(parse_room_alias("#a:b_c"), None);
  assert_eq!(parse_user("@bob+x/y:hs.org"), Some(("bob+x/y".into(), Some("hs.org".into()))));
  assert_eq!(parse_user("%40bob"), Some(("bob".into(), None)));
  assert_eq!(parse_user("bob"), None);
}

#[test]
fn test_escape_html() {
  assert_eq!(escape_html("a <b> & 'c' \"d\"\n\te", false, false), "a &lt;b&gt; &amp; &apos;c&apos; &quot;d&quot;\n\te");
  assert_eq!(escape_html("a b\n\tc", true, true), "a&nbsp;b<br/>&emsp;c");
}

#[test]
fn test_url_identifier() {
  // str() of each upstream url_identifier element.
  let m = load("matrixs://user:pass@example.com/#room");
  assert_eq!(m.url_identifier().unwrap(), vec!["matrixs", "off", "example.com", "443", "None", "user", "pass"]);
  let m = load("matrix://tok@example.com:8008?mode=hookshot&path=hooks");
  assert_eq!(m.url_identifier().unwrap(), vec!["matrix", "hookshot", "example.com", "8008", "/hooks", "None", "tok"]);
  let token = "a".repeat(64);
  let m = load(&format!("matrix://{token}"));
  assert_eq!(m.url_identifier().unwrap(), vec!["matrix".to_string(), "t2bot".into(), token, "80".into(), "None".into(), "None".into(), "None".into()]);
  // Targets do not change it.
  assert_eq!(load("matrixs://user:pass@example.com/#a").url_identifier(), load("matrixs://user:pass@example.com/#b?image=yes").url_identifier());
}

#[test]
fn test_static_details_and_limits() {
  let m = load("matrixs://user:pass@example.com/#room");
  assert_eq!(m.details().protocols, vec!["matrix", "matrixs"]);
  assert!(m.attachment_support());
  assert_eq!(m.notify_formats(), vec![NotifyFormat::Text, NotifyFormat::Html, NotifyFormat::Markdown]);
  assert_eq!(m.title_maxlen(), 250);
  assert_eq!(m.request_rate_per_sec(), 0.5);
  assert!(m.has_dialect());
  // The fallback limits outside content-aware preparation.
  let encrypted_limit = if E2EE_SUPPORT { 40000 } else { 65000 };
  assert_eq!(m.body_maxlen(), encrypted_limit);
  assert_eq!(load("matrix://user:pass@example.com/#room").body_maxlen(), 65000);
  assert_eq!(load("matrixs://user:pass@example.com/#room?e2ee=no").body_maxlen(), 65000);
  assert_eq!(load("matrixs://user:tok@example.com?mode=matrix").body_maxlen(), 65000);
}

#[test]
fn test_body_limit_is_content_aware() {
  let plain = load("matrix://user:pass@example.com/#room");
  assert_eq!(plain.body_maxlen_for("", &"a".repeat(70000), &NotifyFormat::Text), 61336);
  // HTML carries two copies of the body.
  assert_eq!(plain.body_maxlen_for(" Title ", &"a".repeat(70000), &NotifyFormat::Html), 30653);
  // Escape-heavy text gets a lower limit.
  assert!(plain.body_maxlen_for("", &"\"".repeat(70000), &NotifyFormat::Text) < 31000);
  // Webhooks keep their flat limit.
  let webhook = load("matrix://user:tok@example.com?mode=matrix");
  assert_eq!(webhook.body_maxlen_for("", &"\"".repeat(70000), &NotifyFormat::Text), 65000);
  if E2EE_SUPPORT {
    let encrypted = load("matrixs://user:pass@example.com/#room");
    assert_eq!(encrypted.body_maxlen_for("Title", &"a".repeat(70000), &NotifyFormat::Markdown), 22652);
  }
}

#[test]
fn test_dialect_convert() {
  let m = load("matrix://user:pass@example.com/#room");
  assert_eq!(m.dialect_convert("**bold**", &NotifyFormat::Markdown), "<p><strong>bold</strong></p>");
  assert_eq!(m.dialect_convert("<b>x</b>", &NotifyFormat::Html), "<b>x</b>");
  assert_eq!(m.dialect_convert("**x**", &NotifyFormat::Text), "**x**");
  // Slack keeps CommonMark and gets HTML as text.
  let slack = load("matrix://user:tok@example.com?mode=slack");
  assert_eq!(slack.dialect_convert("**x**", &NotifyFormat::Markdown), "**x**");
  assert_eq!(slack.dialect_convert("<b>x</b> y", &NotifyFormat::Html), "x y");
}

// ── Webhooks ─────────────────────────────────────────────────────────────────

async fn webhook(url: &str, title: &str, body_text: &str, format: NotifyFormat, status: u16) -> (bool, Vec<Request>) {
  let server = MockServer::start().await;
  Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
  let m = load(url);
  let mut c = ctx(title, body_text);
  c.body_format = format;
  let ok = send(&m, &server, &c).await;
  (ok, seen(&server).await)
}

#[tokio::test]
async fn test_t2bot_webhook() {
  let token = "a".repeat(64);
  let (ok, requests) = webhook(&format!("matrixs://{token}?image=yes"), "T", "B", NotifyFormat::Text, 200).await;
  assert!(ok);
  assert_eq!(requests.len(), 1);
  assert_eq!(line(&requests[0]), format!("POST /api/v1/matrix/hook/{token}"));
  assert_eq!(requests[0].headers.get("content-type").unwrap(), "application/json");
  assert_eq!(
    body(&requests[0]),
    json!({"displayName": "Apprise", "format": "plain", "text": "T\r\nB",
           "avatarUrl": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-32x32.png"})
  );
  // No avatar unless asked.
  let (_, requests) = webhook(&format!("matrixs://{token}"), "", "B", NotifyFormat::Text, 200).await;
  assert_eq!(body(&requests[0]), json!({"displayName": "Apprise", "format": "plain", "text": "B"}));
}

#[tokio::test]
async fn test_matrix_webhook() {
  let url = "matrix://user:tok@localhost:8080?mode=matrix";
  let (ok, requests) = webhook(url, "My <T>", "**b** & <i>x</i>\nl2", NotifyFormat::Text, 200).await;
  assert!(ok);
  assert_eq!(line(&requests[0]), "POST /api/v1/matrix/hook/tok");
  assert_eq!(body(&requests[0]), json!({"displayName": "user", "format": "plain", "text": "My <T>\r\n**b** & <i>x</i>\nl2"}));
  // HTML: the escaped title (spaces too) and a break.
  let html = "<p><strong>b</strong> &amp; <i>x</i><br />\nl2</p>";
  let (_, requests) = webhook(url, "My <T>", html, NotifyFormat::Html, 200).await;
  assert_eq!(body(&requests[0])["text"], format!("<h1>My&nbsp;&lt;T&gt;</h1><br/>{html}"));
  assert_eq!(body(&requests[0])["format"], "html");
  // Rendered Markdown starts on its own line.
  let (_, requests) = webhook(url, "My <T>", "<p>x</p>", NotifyFormat::Markdown, 200).await;
  assert_eq!(body(&requests[0])["text"], "<h1>My&nbsp;&lt;T&gt;</h1><p>x</p>");
}

#[tokio::test]
async fn test_slack_webhook() {
  let (ok, requests) = webhook("matrix://user:tok@localhost:8080?mode=slack", "My <T>", "**b** & <i>x</i>\nl2", NotifyFormat::Markdown, 200).await;
  assert!(ok);
  assert_eq!(line(&requests[0]), "POST /api/v1/matrix/hook/tok");
  let payload = body(&requests[0]);
  assert_eq!(payload["username"], "user");
  assert_eq!(payload["mrkdwn"], true);
  let attachment = &payload["attachments"][0];
  assert_eq!(attachment["title"], "My &lt;T&gt;");
  assert_eq!(attachment["text"], "**b** &amp; &lt;i&gt;x&lt;/i&gt;\nl2");
  assert_eq!(attachment["color"], "#3AA3E3");
  assert_eq!(attachment["footer"], "Apprise");
  assert!(attachment["ts"].as_f64().unwrap() > 1.7e9);
  // A lone token is the token; the app ID is the username.
  let (_, requests) = webhook("matrix://tok@localhost?mode=slack", "", "b", NotifyFormat::Text, 200).await;
  assert_eq!(line(&requests[0]), "POST /api/v1/matrix/hook/tok");
  assert_eq!(body(&requests[0])["username"], "Apprise");
  assert_eq!(body(&requests[0])["mrkdwn"], false);
}

#[tokio::test]
async fn test_hookshot_webhook() {
  // The apprise-go golden for hookshot-mode.
  let (ok, requests) = webhook("matrixs://token@matrix.example.com?mode=hookshot", "t", "body", NotifyFormat::Text, 200).await;
  assert!(ok);
  assert_eq!(line(&requests[0]), "POST /webhook/token");
  assert_eq!(body(&requests[0]), json!({"username": "Apprise", "text": "t\r\nbody", "html": "t\r<br/>body"}));
  let (_, requests) = webhook("matrixs://token@matrix.example.com?mode=hookshot&path=%2Fcustom%2Fhook", "", "a <b>", NotifyFormat::Text, 200).await;
  assert_eq!(line(&requests[0]), "POST /custom/hook/token");
  assert_eq!(body(&requests[0]), json!({"username": "Apprise", "text": "a <b>", "html": "a &lt;b&gt;"}));
  // Markup stays out of the plain text.
  let html = "<p><strong>b</strong> &amp; <i>x</i>\nl2</p>";
  let (_, requests) = webhook("matrix://user:tok@localhost?mode=hookshot", "My <T>", html, NotifyFormat::Markdown, 200).await;
  assert_eq!(body(&requests[0]), json!({"username": "user", "text": "My <T>\r\nb & x l2", "html": format!("<h1>My&nbsp;&lt;T&gt;</h1>{html}")}));
}

#[tokio::test]
async fn test_webhook_status_handling() {
  // 202 Accepted counts as delivered (upstream a763f3bb).
  assert!(webhook("matrixs://tok@hookshot.example?mode=hookshot", "", "b", NotifyFormat::Text, 202).await.0);
  assert!(!webhook("matrixs://tok@hookshot.example?mode=hookshot", "", "b", NotifyFormat::Text, 500).await.0);
  assert!(!webhook("matrix://user:tok@localhost?mode=matrix", "", "b", NotifyFormat::Text, 403).await.0);
  // A connection failure is a failed send.
  let m = load("matrix://user:tok@127.0.0.1:9?mode=matrix");
  assert!(!m.send(&ctx("", "b")).await.unwrap());
}

// ── Server mode ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_login_join_and_send() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  let m = load("matrix://user:pass@localhost/!abc:localhost/#general?e2ee=no");
  assert!(send(&m, &server, &ctx("T", "B")).await);
  let requests = seen(&server).await;
  let lines: Vec<String> = requests.iter().map(line).collect();
  assert_eq!(
    lines,
    vec![
      "POST /_matrix/client/v3/login",
      "POST /_matrix/client/v3/join/%21abc%3Alocalhost",
      "PUT /_matrix/client/v3/rooms/%21abc%3Alocalhost/send/m.room.message/0",
      "POST /_matrix/client/v3/join/%23general%3Alocalhost",
      "PUT /_matrix/client/v3/rooms/%21general%3Alocalhost/send/m.room.message/1",
    ]
  );
  assert_eq!(
    body(&requests[0]),
    json!({"type": "m.login.password", "identifier": {"type": "m.id.user", "user": "user"}, "password": "pass", "initial_device_display_name": "Apprise"})
  );
  assert!(requests[0].headers.get("authorization").is_none());
  assert_eq!(body(&requests[1]), json!({}));
  for r in &requests[1..] {
    assert_eq!(r.headers.get("authorization").unwrap(), "Bearer abcd1234");
    assert_eq!(r.headers.get("content-type").unwrap(), "application/json");
    assert_eq!(r.headers.get("accept").unwrap(), "application/json");
  }
  assert_eq!(body(&requests[2]), json!({"msgtype": "m.text", "body": "# T\r\nB"}));

  // The next notification reuses the login and joins, and keeps counting.
  assert!(send(&m, &server, &ctx("", "again")).await);
  let lines: Vec<String> = seen(&server).await[5..].iter().map(line).collect();
  assert_eq!(
    lines,
    vec!["PUT /_matrix/client/v3/rooms/%21abc%3Alocalhost/send/m.room.message/2", "PUT /_matrix/client/v3/rooms/%21general%3Alocalhost/send/m.room.message/3",]
  );
}

#[tokio::test]
async fn test_login_v2() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  let m = load("matrix://user:pass@localhost:8008/!abc:localhost?v=2");
  assert!(send(&m, &server, &ctx("T", "B")).await);
  let requests = seen(&server).await;
  assert_eq!(line(&requests[0]), "POST /_matrix/client/r0/login");
  assert_eq!(body(&requests[0]), json!({"type": "m.login.password", "user": "user", "password": "pass", "initial_device_display_name": "Apprise"}));
  assert_eq!(line(&requests[2]), "PUT /_matrix/client/r0/rooms/%21abc%3Alocalhost/send/m.room.message/0");
}

#[tokio::test]
async fn test_register_when_login_fails() {
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/login", 403, json!({"errcode": "M_FORBIDDEN"})).await;
  mock(&server, "POST", "/_matrix/client/v3/register", 200, json!({"access_token": "reg", "user_id": "@user:localhost", "device_id": "D2"})).await;
  Mock::given(method("POST")).and(path_regex("/join/")).respond_with(JoinEcho).mount(&server).await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  let m = load("matrix://user:pass@localhost/!abc?e2ee=no");
  assert!(send(&m, &server, &ctx("", "B")).await);
  let requests = seen(&server).await;
  let register = requests.iter().find(|r| r.url.path().ends_with("/register")).unwrap();
  assert_eq!(register.url.query(), Some("kind=user"));
  assert_eq!(
    body(register),
    json!({"kind": "user", "auth": {"type": "m.login.dummy"}, "username": "user", "password": "pass", "initial_device_display_name": "Apprise"})
  );
  // hsreq=yes completes a bare room ID with the derived home server.
  assert!(requests.iter().any(|r| line(r) == "POST /_matrix/client/v3/join/%21abc%3Alocalhost"));
  let m = load("matrix://user:pass@localhost/!abc?e2ee=no");
  let server2 = MockServer::start().await;
  mock_re(&server2, "POST", "/(login|register)$", 404, json!({})).await;
  assert!(!send(&m, &server2, &ctx("", "B")).await);
}

#[tokio::test]
async fn test_anonymous_registration_and_joined_rooms() {
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/register", 200, json!({"access_token": "anon", "user_id": "@x:localhost"})).await;
  mock(&server, "GET", "/_matrix/client/v3/joined_rooms", 200, json!({"joined_rooms": ["!one:localhost", "!two:localhost"]})).await;
  Mock::given(method("POST")).and(path_regex("/join/")).respond_with(JoinEcho).mount(&server).await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  let m = load("matrix://localhost?mode=off");
  assert!(send(&m, &server, &ctx("", "B")).await);
  let requests = seen(&server).await;
  let register = requests.iter().find(|r| r.url.path().ends_with("/register")).unwrap();
  assert_eq!(body(register), json!({"kind": "user", "auth": {"type": "m.login.dummy"}, "initial_device_display_name": "Apprise"}));
  assert_eq!(sends(&requests).len(), 2);
  // GET requests carry JSON null, as upstream sends them.
  let joined = requests.iter().find(|r| r.url.path().ends_with("/joined_rooms")).unwrap();
  assert_eq!(joined.body, b"null");

  // Nothing joined: nothing to notify.
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/register", 200, json!({"access_token": "anon", "user_id": "@x:localhost"})).await;
  mock(&server, "GET", "/_matrix/client/v3/joined_rooms", 200, json!({"joined_rooms": []})).await;
  assert!(!send(&load("matrix://localhost?mode=off"), &server, &ctx("", "B")).await);
}

#[tokio::test]
async fn test_login_reuses_a_known_device() {
  // A remembered device ID keeps Matrix from creating a new device.
  let server = MockServer::start().await;
  homeserver(&server).await;
  let m = load("matrix://user:pass@localhost/!a:localhost?e2ee=no");
  m.session.lock().await.local.insert("device_id".into(), (json!("KNOWN"), None));
  assert!(send(&m, &server, &ctx("", "B")).await);
  let login = body(&seen(&server).await[0]);
  assert_eq!(login["device_id"], "KNOWN");
  assert!(login.get("initial_device_display_name").is_none());
}

#[tokio::test]
async fn test_whoami_supplies_the_home_server() {
  // A raw token learns its home server from whoami, for aliases and DMs.
  let server = MockServer::start().await;
  mock(&server, "GET", "/_matrix/client/v3/account/whoami", 200, json!({"user_id": "@bot:example.org", "device_id": "D"})).await;
  Mock::given(method("POST")).and(path_regex("/join/")).respond_with(JoinEcho).mount(&server).await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  assert!(send(&load("matrix://token@localhost/%23general?e2ee=no"), &server, &ctx("", "B")).await);
  assert!(seen(&server).await.iter().any(|r| line(r) == "POST /_matrix/client/v3/join/%23general%3Aexample.org"));
  // Without whoami the URL's host stands in.
  let server = MockServer::start().await;
  mock(&server, "GET", "/_matrix/client/v3/account/whoami", 500, json!({})).await;
  Mock::given(method("POST")).and(path_regex("/join/")).respond_with(JoinEcho).mount(&server).await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  assert!(send(&load("matrix://token@localhost/%23general?e2ee=no"), &server, &ctx("", "B")).await);
  assert!(seen(&server).await.iter().any(|r| line(r) == "POST /_matrix/client/v3/join/%23general%3Alocalhost"));
}

/// Every send's transaction ID, from the request paths.
fn txn_ids(requests: &[Request]) -> Vec<String> {
  sends(requests).iter().map(|r| r.url.path().rsplit('/').next().unwrap().to_string()).collect()
}

#[tokio::test]
async fn test_token_transaction_ids_are_unique() {
  // apprise-go #87/#91: upstream reuses one UUID for every event of a
  // raw-token send, and Synapse drops the repeats.
  let server = MockServer::start().await;
  homeserver(&server).await;
  mock_re(&server, "POST", "/_matrix/media/v3/upload$", 200, json!({"content_uri": "mxc://localhost/abc"})).await;
  let m = load("matrix://tokenvalue@localhost/!a:localhost/!b:localhost?e2ee=no");
  let mut c = ctx("T", "B");
  c.attachments = vec![
    Attachment { name: "a.png".into(), data: b"PNG".to_vec(), mime_type: "image/png".into() },
    Attachment { name: "b.txt".into(), data: b"text".to_vec(), mime_type: "text/plain".into() },
  ];
  assert!(send(&m, &server, &c).await);
  assert!(send(&m, &server, &ctx("", "second")).await);
  let requests = seen(&server).await;
  // whoami fills in the identity a raw token leaves out.
  assert_eq!(line(&requests[0]), "GET /_matrix/client/v3/account/whoami");
  assert_eq!(requests[0].headers.get("authorization").unwrap(), "Bearer tokenvalue");
  let ids = txn_ids(&requests);
  // Two files and the text in each room, then the text again.
  assert_eq!(ids.len(), 8);
  let unique: std::collections::HashSet<&String> = ids.iter().collect();
  assert_eq!(unique.len(), ids.len(), "{ids:?}");
  assert!(ids.iter().all(|id| id.len() == 36 && id.chars().nth(14) == Some('4')));
}

#[tokio::test]
async fn test_attachments_v3() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  mock_re(&server, "POST", "/_matrix/media/v3/upload$", 200, json!({"content_uri": "mxc://matrix.example.com/abc"})).await;
  // The apprise-go attachment-image-and-file golden.
  let m = load("matrix://user:pass@localhost/!room:localhost/!other:localhost?e2ee=no");
  let mut c = ctx("t", "body");
  c.attachments = vec![
    Attachment { name: "pixel.png".into(), data: vec![0x89; 70], mime_type: "image/png".into() },
    Attachment { name: "notes.txt".into(), data: b"plain text attachment\n".to_vec(), mime_type: "text/plain".into() },
  ];
  assert!(send(&m, &server, &c).await);
  let requests = seen(&server).await;
  let uploads: Vec<&Request> = requests.iter().filter(|r| r.url.path().ends_with("/upload")).collect();
  // Uploaded once, shared by both rooms.
  assert_eq!(uploads.len(), 2);
  assert_eq!(uploads[0].url.query(), Some("filename=pixel.png"));
  assert_eq!(uploads[0].headers.get("content-type").unwrap(), "image/png");
  assert_eq!(uploads[0].headers.get("authorization").unwrap(), "Bearer abcd1234");
  assert_eq!(uploads[0].body, vec![0x89; 70]);
  assert_eq!(uploads[1].headers.get("content-type").unwrap(), "text/plain");
  let events = sends(&requests);
  assert_eq!(events.len(), 6);
  assert_eq!(
    body(events[0]),
    json!({"body": "pixel.png", "info": {"mimetype": "image/png", "size": 70}, "msgtype": "m.image", "url": "mxc://matrix.example.com/abc",
           "room_id": "!other:localhost", "type": "m.room.message"})
  );
  assert_eq!(
    body(events[1]),
    json!({"body": "notes.txt", "info": {"mimetype": "text/plain", "size": 22}, "msgtype": "m.file", "url": "mxc://matrix.example.com/abc",
           "filename": "notes.txt", "room_id": "!other:localhost", "type": "m.room.message"})
  );
  assert_eq!(body(events[2]), json!({"msgtype": "m.text", "body": "# t\r\nbody"}));
  assert_eq!(txn_ids(&requests), vec!["0", "1", "2", "3", "4", "5"]);
}

#[tokio::test]
async fn test_attachments_v2_and_image() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  mock_re(&server, "POST", "/_matrix/media/r0/upload$", 200, json!({"content_uri": "mxc://localhost/img"})).await;
  let m = load("matrix://user:pass@localhost/!room:localhost?v=2&image=yes");
  let mut c = ctx("Title", "body");
  c.attachments = vec![
    Attachment { name: "a.jpg".into(), data: b"JPG".to_vec(), mime_type: "image/jpeg".into() },
    // v2 only posts images.
    Attachment { name: "b.pdf".into(), data: b"PDF".to_vec(), mime_type: "application/pdf".into() },
  ];
  assert!(send(&m, &server, &c).await);
  let requests = seen(&server).await;
  assert_eq!(requests.iter().filter(|r| r.url.path().ends_with("/upload")).count(), 1);
  let events = sends(&requests);
  assert_eq!(
    body(events[0]),
    json!({"msgtype": "m.image", "url": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-32x32.png", "body": "Title"})
  );
  assert_eq!(
    body(events[1]),
    json!({"info": {"mimetype": "image/jpeg"}, "msgtype": "m.image", "body": "tta.webp", "url": "mxc://localhost/img", "room_id": "!room:localhost", "type": "m.room.message"})
  );
  assert_eq!(body(events[2]), json!({"msgtype": "m.text", "body": "# Title\r\nbody"}));
  assert_eq!(txn_ids(&requests), vec!["0", "1", "2"]);
}

#[tokio::test]
async fn test_upload_failure_fails_the_send() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  mock_re(&server, "POST", "/upload$", 500, json!({"errcode": "M_UNKNOWN"})).await;
  let m = load("matrix://user:pass@localhost/!room:localhost?e2ee=no");
  let mut c = ctx("", "body");
  c.attachments = vec![Attachment { name: "a.png".into(), data: b"x".to_vec(), mime_type: "image/png".into() }];
  assert!(!send(&m, &server, &c).await);
  assert!(sends(&seen(&server).await).is_empty());
}

#[tokio::test]
async fn test_room_failures_do_not_stop_other_rooms() {
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/login", 200, json!({"access_token": "t", "user_id": "@u:localhost"})).await;
  mock(&server, "POST", "/_matrix/client/v3/join/%21bad%3Alocalhost", 403, json!({"errcode": "M_FORBIDDEN"})).await;
  Mock::given(method("POST")).and(path_regex("/join/")).respond_with(JoinEcho).mount(&server).await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  let m = load("matrix://user:pass@localhost/!bad:localhost/!good:localhost?e2ee=no");
  assert!(!send(&m, &server, &ctx("", "B")).await);
  let requests = seen(&server).await;
  assert_eq!(sends(&requests).len(), 1);
  assert!(sends(&requests)[0].url.path().contains("%21good%3Alocalhost"));
}

#[tokio::test]
async fn test_room_alias_creation() {
  // An alias that does not exist is created, encrypted when E2EE is on.
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/login", 200, json!({"access_token": "t", "user_id": "@u:example.com"})).await;
  mock(&server, "POST", "/_matrix/client/v3/join/%23new%3Aexample.com", 404, json!({"errcode": "M_NOT_FOUND"})).await;
  mock(&server, "POST", "/_matrix/client/v3/createRoom", 200, json!({"room_id": "!created:example.com", "room_alias": "#new:example.com"})).await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  let m = load("matrix://user:pass@localhost/%23new?e2ee=no");
  assert!(send(&m, &server, &ctx("", "B")).await);
  let requests = seen(&server).await;
  let create = requests.iter().find(|r| r.url.path().ends_with("/createRoom")).unwrap();
  assert_eq!(
    body(create),
    json!({"room_alias_name": "new", "name": "#new - Apprise Notifications", "visibility": "private", "preset": "trusted_private_chat"})
  );
  assert!(sends(&requests)[0].url.path().contains("%21created%3Aexample.com"));
  // The created room is cached under its alias.
  assert!(send(&m, &server, &ctx("", "again")).await);
  assert_eq!(seen(&server).await.iter().filter(|r| r.url.path().contains("/join/")).count(), 1);
}

#[tokio::test]
async fn test_room_alias_join_errors() {
  // A join refused for another reason never creates a room.
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/login", 200, json!({"access_token": "t", "user_id": "@u:example.com"})).await;
  mock(&server, "POST", "/_matrix/client/v3/join/%23private%3Aexample.com", 403, json!({"errcode": "M_FORBIDDEN"})).await;
  mock(&server, "POST", "/_matrix/client/v3/join/%23taken%3Aexample.com", 404, json!({})).await;
  mock(&server, "POST", "/_matrix/client/v3/createRoom", 400, json!({"errcode": "M_ROOM_IN_USE"})).await;
  mock(&server, "GET", "/_matrix/client/v3/directory/room/%23taken%3Aexample.com", 200, json!({"room_id": "!taken:example.com"})).await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  let m = load("matrix://user:pass@localhost/%23private/%23taken?e2ee=no");
  assert!(!send(&m, &server, &ctx("", "B")).await);
  let requests = seen(&server).await;
  assert_eq!(requests.iter().filter(|r| r.url.path().ends_with("/createRoom")).count(), 1);
  // An alias already taken is looked up instead.
  assert_eq!(sends(&requests).len(), 1);
  assert!(sends(&requests)[0].url.path().contains("%21taken%3Aexample.com"));
}

#[tokio::test]
async fn test_room_id_without_home_server() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  let m = load("matrix://user:pass@localhost/!abc?hsreq=no&e2ee=no");
  assert!(send(&m, &server, &ctx("", "B")).await);
  assert!(seen(&server).await.iter().any(|r| line(r) == "POST /_matrix/client/v3/join/%21abc"));
}

#[tokio::test]
async fn test_rate_limit_is_retried() {
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/login", 200, json!({"access_token": "t", "user_id": "@u:localhost"})).await;
  Mock::given(method("POST")).and(path_regex("/join/")).respond_with(JoinEcho).mount(&server).await;
  Mock::given(method("PUT"))
    .and(path_regex("/send/"))
    .respond_with(ResponseTemplate::new(429).set_body_json(json!({"errcode": "M_LIMIT_EXCEEDED", "retry_after_ms": 1})))
    .up_to_n_times(1)
    .mount(&server)
    .await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  let m = load("matrix://user:pass@localhost/!a:localhost?e2ee=no");
  assert!(send(&m, &server, &ctx("", "B")).await);
  let ids = txn_ids(&seen(&server).await);
  // The retry repeats the same event.
  assert_eq!(ids, vec!["0", "0"]);

  // Still limited after the retries: a failure.
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/login", 200, json!({"access_token": "t", "user_id": "@u:localhost"})).await;
  Mock::given(method("POST")).and(path_regex("/join/")).respond_with(JoinEcho).mount(&server).await;
  mock_re(&server, "PUT", "/send/", 429, json!({"error": {"retry_after_ms": 1}})).await;
  assert!(!send(&load("matrix://user:pass@localhost/!a:localhost?e2ee=no"), &server, &ctx("", "B")).await);
  assert_eq!(sends(&seen(&server).await).len(), 2);
}

#[tokio::test]
async fn test_invalid_json_fails() {
  let server = MockServer::start().await;
  Mock::given(method("POST")).and(path("/_matrix/client/v3/login")).respond_with(ResponseTemplate::new(200).set_body_string("not json")).mount(&server).await;
  mock(&server, "POST", "/_matrix/client/v3/register", 500, json!({})).await;
  assert!(!send(&load("matrix://user:pass@localhost/!a?e2ee=no"), &server, &ctx("", "B")).await);
}

#[tokio::test]
async fn test_login_response_is_bounded() {
  let server = MockServer::start().await;
  let huge = json!({"access_token": "t", "padding": "x".repeat(MAX_RESPONSE_BYTES)});
  mock(&server, "POST", "/_matrix/client/v3/login", 200, huge).await;
  mock(&server, "POST", "/_matrix/client/v3/register", 500, json!({})).await;
  assert!(!send(&load("matrix://user:pass@localhost/!a?e2ee=no"), &server, &ctx("", "B")).await);
}

#[tokio::test]
async fn test_login_refuses_redirects() {
  // A redirect must not carry the password to another server.
  let server = MockServer::start().await;
  let other = MockServer::start().await;
  Mock::given(method("POST"))
    .and(path("/_matrix/client/v3/login"))
    .respond_with(ResponseTemplate::new(307).insert_header("Location", format!("{}/_matrix/client/v3/login", other.uri())))
    .mount(&server)
    .await;
  mock(&server, "POST", "/_matrix/client/v3/register", 500, json!({})).await;
  mock(&other, "POST", "/_matrix/client/v3/login", 200, json!({"access_token": "stolen"})).await;
  assert!(!send(&load("matrix://user:pass@localhost/!a?e2ee=no"), &server, &ctx("", "B")).await);
  assert!(seen(&other).await.is_empty());
}

// ── Discovery ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_discovery_not_found_uses_the_host() {
  let server = MockServer::start().await;
  mock(&server, "GET", "/.well-known/matrix/client", 404, json!({})).await;
  homeserver(&server).await;
  let m = load("matrixs://user:pass@example.com/!room:example.com?e2ee=no");
  assert!(send(&m, &server, &ctx("t", "body")).await);
  assert!(send(&m, &server, &ctx("t", "body")).await);
  let requests = seen(&server).await;
  // Probed once (cached), and without our token.
  let probes: Vec<&Request> = requests.iter().filter(|r| r.url.path().starts_with("/.well-known")).collect();
  assert_eq!(probes.len(), 1);
  assert!(probes[0].headers.get("authorization").is_none());
  assert_eq!(probes[0].body, b"null");
  assert_eq!(sends(&requests).len(), 2);
}

#[tokio::test]
async fn test_discovery_validates_the_base_url() {
  for (well_known, versions_status) in [
    // Plain HTTP or a query could send our token anywhere.
    (json!({"m.homeserver": {"base_url": "http://matrix.example.com"}}), 200),
    (json!({"m.homeserver": {"base_url": "https://matrix.example.com/?x=1"}}), 200),
    (json!({"m.homeserver": {"base_url": "https://user:pw@matrix.example.com"}}), 200),
    (json!({"m.homeserver": {"base": "x"}}), 200),
    (json!({"m.homeserver": {"base_url": 5}}), 200),
    // The version check of the discovered server fails.
    (json!({"m.homeserver": {"base_url": "https://127.0.0.1:9"}}), 200),
    // An identity server that does not check out.
    (json!({"m.homeserver": {"base_url": "https://127.0.0.1:9"}, "m.identity_server": {"base_url": "http://id.example"}}), 200),
  ] {
    let server = MockServer::start().await;
    mock(&server, "GET", "/.well-known/matrix/client", 200, well_known.clone()).await;
    mock(&server, "GET", "/_matrix/client/versions", versions_status, json!({})).await;
    homeserver(&server).await;
    let m = load("matrixs://user:pass@example.com/!room:example.com?e2ee=no");
    assert!(!send(&m, &server, &ctx("t", "body")).await, "{well_known}");
    // Nothing went to the homeserver, and the failure is not cached.
    let requests = seen(&server).await;
    assert!(requests.iter().all(|r| !r.url.path().contains("/login")), "{well_known}");
    let session = m.session.lock().await;
    assert!(!session.local.contains_key(DISCOVERY_BASE_KEY));
  }
  // A server error on the probe also fails.
  let server = MockServer::start().await;
  mock(&server, "GET", "/.well-known/matrix/client", 500, json!({})).await;
  assert!(!send(&load("matrixs://user:pass@example.com/!room?e2ee=no"), &server, &ctx("", "b")).await);
}

#[tokio::test]
async fn test_discovery_empty_answer_and_disabled() {
  let server = MockServer::start().await;
  mock(&server, "GET", "/.well-known/matrix/client", 200, json!({})).await;
  homeserver(&server).await;
  assert!(send(&load("matrixs://user:pass@example.com/!room?e2ee=no"), &server, &ctx("", "b")).await);
  let server = MockServer::start().await;
  homeserver(&server).await;
  assert!(send(&load("matrixs://user:pass@example.com/!room?e2ee=no&discovery=no"), &server, &ctx("", "b")).await);
  assert!(seen(&server).await.iter().all(|r| !r.url.path().starts_with("/.well-known")));
}

// ── Direct messages ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_dm_existing_room() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  mock(&server, "GET", "/_matrix/client/v3/user/%40apprise%3Alocalhost/account_data/m.direct", 200, json!({"@bob:localhost": ["!dm:localhost"]})).await;
  let m = load("matrix://user:pass@localhost/@bob?e2ee=no");
  assert!(send(&m, &server, &ctx("", "hi")).await);
  let requests = seen(&server).await;
  assert!(requests.iter().all(|r| !r.url.path().ends_with("/createRoom")));
  assert!(sends(&requests)[0].url.path().contains("%21dm%3Alocalhost"));
  // Cached: no second m.direct lookup.
  assert!(send(&m, &server, &ctx("", "again")).await);
  assert_eq!(seen(&server).await.iter().filter(|r| r.url.path().ends_with("m.direct")).count(), 1);
}

#[tokio::test]
async fn test_dm_creates_a_room() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  mock(&server, "GET", "/_matrix/client/v3/user/%40apprise%3Alocalhost/account_data/m.direct", 200, json!({"@other:localhost": ["!x:localhost"]})).await;
  mock(&server, "PUT", "/_matrix/client/v3/user/%40apprise%3Alocalhost/account_data/m.direct", 200, json!({})).await;
  mock(&server, "POST", "/_matrix/client/v3/createRoom", 200, json!({"room_id": "!newdm:localhost"})).await;
  let m = load("matrix://user:pass@localhost/@carol:other.org?e2ee=no");
  assert!(send(&m, &server, &ctx("", "hi")).await);
  let requests = seen(&server).await;
  let create = requests.iter().find(|r| r.url.path().ends_with("/createRoom")).unwrap();
  assert_eq!(body(create), json!({"is_direct": true, "preset": "trusted_private_chat", "invite": ["@carol:other.org"]}));
  let update = requests.iter().find(|r| r.method.as_str() == "PUT" && r.url.path().ends_with("m.direct")).unwrap();
  assert_eq!(body(update), json!({"@other:localhost": ["!x:localhost"], "@carol:other.org": ["!newdm:localhost"]}));
  // DM targets alone never fall back to joined rooms.
  assert!(requests.iter().all(|r| !r.url.path().ends_with("/joined_rooms")));
  // The DM room is joined like any room, then sent to.
  assert!(sends(&requests)[0].url.path().contains("%21newdm%3Alocalhost"));
}

#[tokio::test]
async fn test_dm_failure_is_reported() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  mock_re(&server, "GET", "m.direct$", 404, json!({"errcode": "M_NOT_FOUND"})).await;
  mock(&server, "POST", "/_matrix/client/v3/createRoom", 500, json!({})).await;
  let m = load("matrix://user:pass@localhost/@bob/!room:localhost?e2ee=no");
  // The room still gets the message; the notification reports the failure.
  assert!(!send(&m, &server, &ctx("", "hi")).await);
  assert_eq!(sends(&seen(&server).await).len(), 1);
}

// ── Message content ──────────────────────────────────────────────────────────

async fn apprise_send(server: &MockServer, url: &str, title: &str, body_text: &str, input: NotifyFormat) -> Vec<Request> {
  let mut a = Apprise::new();
  assert!(a.add(url), "{url}");
  let c = NotifyContext { title: title.into(), body: body_text.into(), body_format: input, ..Default::default() };
  let result = with_test_base(server.uri(), a.notify(&c)).await;
  assert!(result.is_success(), "{url}");
  seen(server).await
}

#[tokio::test]
async fn test_formats_match_upstream() {
  // pycapture.py with --input-format markdown and each ?format=.
  let title = "My <T> & \"x\"";
  let cases = [
    (
      "html",
      json!({"msgtype": "m.text", "body": "# My <T> & \"x\"\r\nbold it\n line2", "format": "org.matrix.custom.html",
             "formatted_body": "<h1>My &lt;T&gt; &amp; &quot;x&quot;</h1><br/><p><strong>bold</strong> <i>it</i><br />\nline2</p>"}),
    ),
    (
      "markdown",
      json!({"msgtype": "m.text", "body": "# My <T> & \"x\"\r\nbold it line2", "format": "org.matrix.custom.html",
             "formatted_body": "<h1>My &lt;T&gt; &amp; &quot;x&quot;</h1><p><strong>bold</strong> <i>it</i>\nline2</p>"}),
    ),
  ];
  for (format, expected) in cases {
    let server = MockServer::start().await;
    homeserver(&server).await;
    let requests =
      apprise_send(&server, &format!("matrix://user:pass@localhost/!r:localhost?format={format}"), title, "**bold** <i>it</i>\nline2", NotifyFormat::Markdown)
        .await;
    assert_eq!(body(sends(&requests)[0]), expected, "{format}");
  }
  // Plain input as HTML, and a notice.
  let server = MockServer::start().await;
  homeserver(&server).await;
  let requests = apprise_send(&server, "matrix://user:pass@localhost/!r:localhost?format=html&msgtype=notice", "T", "a <b>\nc", NotifyFormat::Text).await;
  assert_eq!(
    body(sends(&requests)[0]),
    json!({"msgtype": "m.notice", "body": "# T\r\na <b>\nc", "format": "org.matrix.custom.html", "formatted_body": "<h1>T</h1><br/>a&nbsp;&lt;b&gt;<br/>c"})
  );
}

/// Content with no declared format, as upstream sends it for
/// `notify(body_format=None)`: the plain fallback keeps it as written, and
/// Markdown is rendered for the formatted body because it skipped
/// dialect_convert().
#[tokio::test]
async fn test_passthrough_formats_match_upstream() {
  let (title, text) = ("T <x>", "**b** <i>x</i> & y");
  let rendered = "<p><strong>b</strong> <i>x</i> &amp; y</p>";
  let token = "a".repeat(64);
  let room = "matrix://user:pass@localhost/!r:localhost";
  let cases = [
    (
      format!("{room}?format=html"),
      json!({"msgtype": "m.text", "body": "# T <x>\r\n**b** <i>x</i> & y", "format": "org.matrix.custom.html", "formatted_body": "<h1>T &lt;x&gt;</h1><br/>**b** <i>x</i> & y"}),
    ),
    (
      format!("{room}?format=markdown"),
      json!({"msgtype": "m.text", "body": "# T <x>\r\n**b** <i>x</i> & y", "format": "org.matrix.custom.html", "formatted_body": format!("<h1>T &lt;x&gt;</h1>{rendered}")}),
    ),
    (format!("{room}?format=text"), json!({"msgtype": "m.text", "body": "# T <x>\r\n**b** <i>x</i> & y"})),
    (
      "matrix://user:token@localhost?mode=matrix&format=html".into(),
      json!({"displayName": "user", "format": "html", "text": "<h1>T&nbsp;&lt;x&gt;</h1><br/>**b** <i>x</i> & y"}),
    ),
    (
      "matrix://user:token@localhost?mode=matrix&format=markdown".into(),
      json!({"displayName": "user", "format": "html", "text": format!("<h1>T&nbsp;&lt;x&gt;</h1>{rendered}")}),
    ),
    (
      format!("matrix://{token}?mode=t2bot&format=markdown"),
      json!({"displayName": "Apprise", "format": "html", "text": format!("<h1>T&nbsp;&lt;x&gt;</h1>{rendered}")}),
    ),
    (
      "matrix://user:token@localhost?mode=hookshot&format=html".into(),
      json!({"username": "user", "text": "T <x>\r\n**b** <i>x</i> & y", "html": "<h1>T&nbsp;&lt;x&gt;</h1><br/>**b** <i>x</i> & y"}),
    ),
    (
      "matrix://user:token@localhost?mode=hookshot&format=markdown".into(),
      json!({"username": "user", "text": "T <x>\r\n**b** <i>x</i> & y", "html": format!("<h1>T&nbsp;&lt;x&gt;</h1>{rendered}")}),
    ),
  ];
  for (url, expected) in cases {
    let server = MockServer::start().await;
    homeserver(&server).await;
    Mock::given(method("POST")).and(path_regex(r"/hook/|/webhook/")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = Apprise::new();
    assert!(a.add(&url), "{url}");
    let c = NotifyContext { body_passthrough: true, ..ctx(title, text) };
    assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success(), "{url}");
    let requests = seen(&server).await;
    let sent = sends(&requests).first().copied().or_else(|| requests.iter().find(|r| r.method.as_str() == "POST" && r.url.path().contains("hook/")));
    assert_eq!(body(sent.unwrap()), expected, "{url}");
  }
  // Slack mode escapes the text as it is
  for (format, mrkdwn) in [("html", false), ("markdown", true)] {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path_regex(r"/hook/")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("matrix://user:token@localhost?mode=slack&format={format}")));
    let c = NotifyContext { body_passthrough: true, ..ctx(title, text) };
    assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
    let payload = body(&seen(&server).await[0]);
    assert_eq!((&payload["mrkdwn"], &payload["attachments"][0]["text"]), (&json!(mrkdwn), &json!("**b** &lt;i&gt;x&lt;/i&gt; &amp; y")), "{format}");
  }
}

#[tokio::test]
async fn test_slack_formats_match_upstream() {
  let server = MockServer::start().await;
  Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
  let requests =
    apprise_send(&server, "matrix://user:tok@localhost:8080?mode=slack&format=html", "My <T>", "**b** & <i>x</i>\nl2", NotifyFormat::Markdown).await;
  assert_eq!(body(&requests[0])["attachments"][0]["text"], "b &amp; x\n l2");
}

#[tokio::test]
async fn test_split_messages() {
  // An escape-heavy body splits by bytes, not characters.
  let server = MockServer::start().await;
  homeserver(&server).await;
  let text = "\"".repeat(40000);
  let requests = apprise_send(&server, "matrix://user:pass@localhost/!r:localhost?overflow=split&e2ee=no", "", &text, NotifyFormat::Text).await;
  let events = sends(&requests);
  assert_eq!(events.len(), 2);
  for event in &events {
    assert!(event.body.len() < MATRIX_SAFE_EVENT_BYTES, "{}", event.body.len());
  }
  let joined: String = events.iter().map(|e| body(e)["body"].as_str().unwrap().to_string()).collect();
  assert_eq!(joined, text);
}

const MATRIX_SAFE_EVENT_BYTES: usize = sizing::MATRIX_EVENT_BYTE_LIMIT - sizing::MATRIX_EVENT_SAFETY_MARGIN;

// ── Persistence and retries ──────────────────────────────────────────────────

#[tokio::test]
async fn test_login_state_persists_across_instances() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  let dir = tempfile::tempdir().unwrap();
  let url = format!("matrix://user:pass@{}/!r:localhost?e2ee=no", server.address());
  for _ in 0..2 {
    // A fresh Apprise each time, sharing only the storage directory.
    let asset = AppriseAsset { storage_path: Some(dir.path().to_path_buf()), ..Default::default() };
    let mut a = Apprise::with_asset(asset);
    assert!(a.add(&url));
    assert!(a.notify(&ctx("", "B")).await.is_success());
  }
  let requests = seen(&server).await;
  // One login; the token, room and transaction counter were reused.
  assert_eq!(requests.iter().filter(|r| r.url.path().ends_with("/login")).count(), 1);
  assert_eq!(requests.iter().filter(|r| r.url.path().contains("/join/")).count(), 1);
  assert_eq!(txn_ids(&requests), vec!["0", "1"]);
}

// ── Logout (upstream `__del__`) ──────────────────────────────────────────────

/// [`homeserver`] plus registration, webhooks and a logout that answers
/// `status` and `answer`.
async fn homeserver_with_logout(server: &MockServer, status: u16, answer: Value) {
  homeserver(server).await;
  mock_re(server, "POST", r"^/_matrix/client/(v3|r0)/logout$", status, answer).await;
  mock_re(server, "POST", r"^/_matrix/client/(v3|r0)/register$", 200, json!({"access_token": "reg5678", "user_id": "@anon:localhost", "device_id": "DEV2"}))
    .await;
  Mock::given(method("POST")).and(path_regex(r"/hook/|/webhook/")).respond_with(ResponseTemplate::new(200)).mount(server).await;
}

fn logouts(requests: &[Request]) -> Vec<&Request> {
  requests.iter().filter(|r| r.url.path().ends_with("/logout")).collect()
}

fn memory_asset() -> AppriseAsset {
  AppriseAsset { storage_mode: StorageMode::Memory, ..Default::default() }
}

#[tokio::test]
async fn test_close_logs_out_a_login_kept_in_memory() {
  // The CLI runs of upstream and the port against a fake homeserver agree
  // on each case (round-3 report, r5-matrix-smpp-yaml).
  let dir = tempfile::tempdir().unwrap();
  let disk = || AppriseAsset { storage_path: Some(dir.path().to_path_buf()), ..Default::default() };
  let login = "matrix://user:pass@localhost/!r:localhost?e2ee=no";
  let cases = [
    (memory_asset(), login.to_string(), Some("abcd1234")),
    // No storage path: the store lives in memory.
    (AppriseAsset::default(), login.into(), Some("abcd1234")),
    (disk(), format!("{login}&store=no"), Some("abcd1234")),
    // A registration is given back too.
    (memory_asset(), "matrix://localhost/!r:localhost?e2ee=no".into(), Some("reg5678")),
    // The store keeps the login for the next run.
    (disk(), login.into(), None),
    // The URL's own access token is not ours to give back.
    (memory_asset(), "matrix://rawtoken@localhost/!r:localhost?e2ee=no".into(), None),
    // Webhooks never log in.
    (memory_asset(), "matrix://user:pass@localhost?mode=matrix".into(), None),
    (memory_asset(), "matrix://user:pass@localhost?mode=hookshot".into(), None),
  ];
  for (asset, url, token) in cases {
    let server = MockServer::start().await;
    homeserver_with_logout(&server, 200, json!({})).await;
    let mut a = Apprise::with_asset(asset);
    assert!(a.add(&url), "{url}");
    with_test_base(server.uri(), async {
      assert!(a.notify(&ctx("", "B")).await.is_success(), "{url}");
      a.close().await;
    })
    .await;
    let requests = seen(&server).await;
    let logouts = logouts(&requests);
    match token {
      Some(token) => {
        assert_eq!(logouts.len(), 1, "{url}");
        assert_eq!(line(logouts[0]), "POST /_matrix/client/v3/logout", "{url}");
        assert_eq!(logouts[0].headers.get("authorization").unwrap(), &format!("Bearer {token}"), "{url}");
        assert_eq!(body(logouts[0]), json!({}), "{url}");
        // Nothing is sent after the logout.
        assert!(requests.last().unwrap().url.path().ends_with("/logout"), "{url}");
      }
      None => assert!(logouts.is_empty(), "{url}"),
    }
  }
}

#[tokio::test]
async fn test_close_forgets_the_login() {
  let server = MockServer::start().await;
  homeserver_with_logout(&server, 200, json!({})).await;
  let mut a = Apprise::with_asset(memory_asset());
  assert!(a.add("matrix://user:pass@localhost/!r:localhost?e2ee=no&v=2"));
  with_test_base(server.uri(), async {
    assert!(a.notify(&ctx("", "1")).await.is_success());
    a.close().await;
    // Nothing is left to give back.
    a.close().await;
    // The service still works: it logs in again.
    assert!(a.notify(&ctx("", "2")).await.is_success());
    a.close().await;
  })
  .await;
  let lines: Vec<String> = seen(&server).await.iter().map(line).filter(|l| !l.contains("/join/")).collect();
  assert_eq!(
    lines,
    [
      "POST /_matrix/client/r0/login",
      "PUT /_matrix/client/r0/rooms/%21r%3Alocalhost/send/m.room.message/0",
      "POST /_matrix/client/r0/logout",
      "POST /_matrix/client/r0/login",
      "PUT /_matrix/client/r0/rooms/%21r%3Alocalhost/send/m.room.message/1",
      "POST /_matrix/client/r0/logout",
    ]
  );
}

#[tokio::test]
async fn test_failed_logout_keeps_the_login() {
  // Upstream `_logout()`: an expired token counts as logged out; any other
  // failure keeps the login, so a later close tries again.
  for (status, errcode, attempts) in [(500, "M_UNKNOWN", 2), (401, "M_UNKNOWN_TOKEN", 1)] {
    let server = MockServer::start().await;
    homeserver_with_logout(&server, status, json!({"errcode": errcode, "error": "x"})).await;
    // Used directly, without a dispatch or store.
    let m = load("matrix://user:pass@localhost/!r:localhost?e2ee=no");
    assert!(send(&m, &server, &ctx("", "B")).await);
    m.close().await;
    m.close().await;
    assert_eq!(logouts(&seen(&server).await).len(), attempts, "{errcode}");
    assert_eq!(m.session.lock().await.access_token.is_some(), attempts == 2, "{errcode}");
  }
}

#[test]
fn test_logout_conditions() {
  // Upstream `__del__`: not for t2bot, not when the store persists, and not
  // for the access token the URL gave (`access_token == password and not user`).
  let session = |token: Option<&str>, memory_store: bool| Session {
    access_token: token.map(str::to_string),
    base_url: Some("https://hs.example".into()),
    last_use: Some(LastUse { http: HttpOptions::default(), store: None, memory_store }),
    ..Default::default()
  };
  let due = |url: &str, s: &Session| load(url).logout_due(s).map(|l| (l.url, l.token));
  let logout = |path: &str, token: &str| Some((format!("https://hs.example{path}/logout"), token.to_string()));
  assert_eq!(due("matrix://user:pass@localhost/#r", &session(Some("t"), true)), logout("/_matrix/client/v3", "t"));
  assert_eq!(due("matrix://user:pass@localhost/#r?v=2", &session(Some("t"), true)), logout("/_matrix/client/r0", "t"));
  assert_eq!(due("matrix://user:pass@localhost/#r", &session(Some("t"), false)), None);
  assert_eq!(due("matrix://user:pass@localhost/#r", &session(None, true)), None);
  assert_eq!(due("matrix://t@localhost/#r", &session(Some("t"), true)), None);
  // A token that is not the URL's was obtained by this process.
  assert_eq!(due("matrix://t@localhost/#r", &session(Some("other"), true)), logout("/_matrix/client/v3", "other"));
  // With a user, the password is a password even if it equals the token.
  assert_eq!(due("matrix://user:t@localhost/#r", &session(Some("t"), true)), logout("/_matrix/client/v3", "t"));
  let t2bot = format!("matrix://{}?mode=t2bot", "a".repeat(64));
  assert_eq!(due(&t2bot, &session(Some("t"), true)), None);
}

#[tokio::test]
async fn test_dropped_service_logs_out_in_the_background() {
  let server = MockServer::start().await;
  homeserver_with_logout(&server, 200, json!({})).await;
  let url = "matrix://user:pass@localhost/!r:localhost?e2ee=no";
  let logged_out = || async { logouts(&seen(&server).await).len() };
  super::with_drop_logout_in_tests(async {
    // Dropped without close(): the logout runs on this runtime.
    let mut a = Apprise::with_asset(memory_asset());
    assert!(a.add(url));
    assert!(with_test_base(server.uri(), a.notify(&ctx("", "B"))).await.is_success());
    drop(a);
    let started = Instant::now();
    while logged_out().await == 0 {
      assert!(started.elapsed() < Duration::from_secs(5), "no logout after the drop");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Closed, then dropped: no second logout.
    let mut a = Apprise::with_asset(memory_asset());
    assert!(a.add(url));
    with_test_base(server.uri(), async {
      assert!(a.notify(&ctx("", "B")).await.is_success());
      a.close().await;
    })
    .await;
    drop(a);
    tokio::time::sleep(Duration::from_millis(200)).await;
  })
  .await;
  assert_eq!(logged_out().await, 2);
}

/// Fails the first PUT to one room, then accepts everything.
struct FlakyRoom {
  hits: std::sync::Mutex<u32>,
}

impl Respond for FlakyRoom {
  fn respond(&self, _request: &Request) -> ResponseTemplate {
    let mut hits = self.hits.lock().unwrap();
    *hits += 1;
    if *hits == 1 { ResponseTemplate::new(500).set_body_json(json!({"errcode": "M_UNKNOWN"})) } else { ResponseTemplate::new(200).set_body_json(json!({})) }
  }
}

#[tokio::test]
async fn test_retry_skips_delivered_targets_and_reuses_uploads() {
  let server = MockServer::start().await;
  mock(&server, "POST", "/_matrix/client/v3/login", 200, json!({"access_token": "t", "user_id": "@u:localhost"})).await;
  Mock::given(method("POST")).and(path_regex("/join/")).respond_with(JoinEcho).mount(&server).await;
  mock_re(&server, "POST", "/upload$", 200, json!({"content_uri": "mxc://localhost/f"})).await;
  Mock::given(method("PUT"))
    .and(path_regex(r"/rooms/%21b%3Alocalhost/send/m\.room\.message/\d+$"))
    .respond_with(FlakyRoom { hits: std::sync::Mutex::new(0) })
    .mount(&server)
    .await;
  mock_re(&server, "PUT", "/send/", 200, json!({})).await;
  let file = tempfile::NamedTempFile::new().unwrap();
  std::fs::write(file.path(), b"data").unwrap();
  let mut a = Apprise::new();
  assert!(a.add(&format!("matrix://user:pass@{}/!a:localhost/!b:localhost?e2ee=no&retry=1&wait=0", server.address())));
  let result = a.notify_with_attachments(&ctx("", "B"), &[file.path().to_string_lossy().into_owned()]).await;
  assert!(result.is_success());
  let requests = seen(&server).await;
  // One upload for both attempts; room a got its two events once, room b's
  // failed attachment event was sent again, its message once.
  assert_eq!(requests.iter().filter(|r| r.url.path().ends_with("/upload")).count(), 1);
  let to = |room: &str| sends(&requests).iter().filter(|r| r.url.path().contains(room)).count();
  assert_eq!(to("%21a%3Alocalhost"), 2);
  assert_eq!(to("%21b%3Alocalhost"), 3);
}

// ── Encrypted rooms without encryption support ───────────────────────────────

#[cfg(not(feature = "matrix-e2ee"))]
#[tokio::test]
async fn test_encrypted_room_fails_closed_without_support() {
  let server = MockServer::start().await;
  homeserver(&server).await;
  mock(&server, "GET", "/_matrix/client/v3/rooms/%21enc%3Alocalhost/state/m.room.encryption", 200, json!({"algorithm": "m.megolm.v1.aes-sha2"})).await;
  mock(&server, "GET", "/_matrix/client/v3/rooms/%21plain%3Alocalhost/state/m.room.encryption", 404, json!({"errcode": "M_NOT_FOUND"})).await;
  let m = load("matrixs://user:pass@localhost/!enc:localhost/!plain:localhost?discovery=no");
  assert!(!send(&m, &server, &ctx("", "secret")).await);
  let requests = seen(&server).await;
  // The plaintext room got the message; the encrypted one got nothing.
  assert_eq!(sends(&requests).len(), 1);
  assert!(sends(&requests)[0].url.path().contains("%21plain%3Alocalhost"));
  // Without TLS or with e2ee=no nothing is looked up.
  let server = MockServer::start().await;
  homeserver(&server).await;
  assert!(send(&load("matrixs://user:pass@localhost/!enc:localhost?discovery=no&e2ee=no"), &server, &ctx("", "x")).await);
  assert!(seen(&server).await.iter().all(|r| !r.url.path().contains("/state/")));
}

#[cfg(feature = "matrix-e2ee")]
mod encrypted;
