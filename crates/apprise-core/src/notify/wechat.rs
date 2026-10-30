use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::parse_list;
use crate::utils::parse::{ParsedUrl, unquote};

/// Errors that mean the cached access token is no longer usable.
const TOKEN_ERROR_CODES: &[i64] = &[40001, 40014, 42001];

/// WeCom (WeChat Work / Enterprise WeChat) application messages sent to
/// users, departments or tags of an organisation.
///
/// The access token is cached in the persistent store for its two-hour
/// lifetime, as upstream does; without a store every send fetches one.
pub struct WeChat {
  corpid: String,
  corpsecret: String,
  agentid: u64,
  /// WeCom user IDs, or `@all` for the whole organisation
  users: Vec<String>,
  departments: Vec<String>,
  tag_ids: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl WeChat {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let corpid = url.user.clone().unwrap_or_default().trim().to_string();
    if corpid.is_empty() {
      tracing::warn!("A WeChat (WeCom) Corp ID must be specified.");
      return None;
    }
    let corpsecret = url.password.clone().unwrap_or_default().trim().to_string();
    if corpsecret.is_empty() {
      tracing::warn!("A WeChat (WeCom) App Secret must be specified.");
      return None;
    }
    let agentid = url.host.clone().unwrap_or_default();
    let agentid = agentid.trim();
    let agentid = match agentid.bytes().all(|c| c.is_ascii_digit()).then(|| agentid.parse::<u64>().ok()).flatten() {
      Some(id) => id,
      None => {
        tracing::warn!("The WeChat (WeCom) Agent ID ({}) is invalid; it must be a non-negative integer.", agentid);
        return None;
      }
    };

    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|v| !v.is_empty()) {
      // Upstream's URLBase.parse_list() also unquotes each entry
      entries.extend(parse_list(&[to]).iter().map(|t| unquote(t)));
    }
    let (mut users, mut departments, mut tag_ids) = (Vec::new(), Vec::new(), Vec::new());
    for target in parse_list(&entries) {
      if let Some(dept) = target.strip_prefix('#').filter(|d| is_numeric(d)) {
        departments.push(dept.to_string());
      } else if let Some(tag) = target.strip_prefix('+').filter(|t| is_numeric(t)) {
        tag_ids.push(tag.to_string());
      } else if let Some(user) = user_id(&target) {
        // Both "@all" and a bare "all" address the whole organisation
        users.push(if user == "all" { "@all".to_string() } else { user.to_string() });
      } else {
        tracing::warn!("Invalid/unrecognized WeChat (WeCom) target: {}", target);
      }
    }

    Some(Self { corpid, corpsecret, agentid, users, departments, tag_ids, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "WeChat (WeCom)",
      service_url: Some("https://work.weixin.qq.com/"),
      setup_url: Some("https://appriseit.com/services/wechat/"),
      protocols: vec!["wechat"],
      description: "Send WeCom application messages to users, departments or tags.",
      attachment_support: false,
    }
  }

  /// The cached access token, or a fresh one; `None` (after logging why)
  /// on any failure.
  async fn access_token(&self, client: &reqwest::Client, base: &str) -> Option<String> {
    let cached = crate::storage::with_service_store(|s| s.get("access_token")).flatten();
    if let Some(token) = cached.as_ref().and_then(Value::as_str).filter(|t| !t.is_empty()) {
      return Some(token.to_string());
    }
    crate::notify::throttle().await;
    let resp = client
      .get(format!("{}/cgi-bin/gettoken", base))
      .header("User-Agent", APP_ID)
      .query(&[("corpid", self.corpid.as_str()), ("corpsecret", self.corpsecret.as_str())])
      .send()
      .await;
    let resp = match resp {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred fetching the WeChat access token.");
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return None;
      }
    };
    let status = resp.status().as_u16();
    if status != 200 {
      tracing::warn!("Failed to fetch WeChat access token: error={}.", status);
      return None;
    }
    let content: Value = serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    if let Some(error) = api_error(&content) {
      tracing::warn!("WeChat access token request failed: {}.", error);
      return None;
    }
    match content.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()) {
      Some(token) => {
        // Keep it until five minutes before it expires (at least a minute)
        let expires_in = content.get("expires_in").and_then(Value::as_i64).unwrap_or(7200);
        let ttl = std::time::Duration::from_secs((expires_in - 300).max(60) as u64);
        crate::storage::with_service_store(|s| s.set("access_token", json!(token), Some(ttl), true));
        Some(token.to_string())
      }
      None => {
        tracing::warn!("WeChat access token response contained no token.");
        None
      }
    }
  }
}

/// IS_USER upstream: an optional `@`, then an alphanumeric followed by
/// alphanumerics or `_ @ . -`. Returns the ID without the leading `@`.
fn user_id(target: &str) -> Option<&str> {
  let user = target.strip_prefix('@').unwrap_or(target);
  let b = user.as_bytes();
  (!b.is_empty() && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'@' | b'.' | b'-'))).then_some(user)
}

fn is_numeric(s: &str) -> bool {
  !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// The WeCom `errcode` check shared by both endpoints: an empty or
/// unreadable body is a failure, a missing errcode is success, and any
/// errcode other than 0 is a failure. Returns the reason on failure.
fn api_error(content: &Value) -> Option<String> {
  let obj = match content.as_object() {
    Some(obj) if !obj.is_empty() => obj,
    _ => return Some("errcode=-1: Unknown error".to_string()),
  };
  let errcode = match obj.get("errcode") {
    None => return None,
    Some(code) if code.as_f64() == Some(0.0) => return None,
    Some(code) => code,
  };
  let message = match errcode.as_i64() {
    Some(40001) => "Invalid credential or access token expired.",
    Some(40003) => "Invalid user ID.",
    Some(40014) => "Invalid access token.",
    Some(42001) => "Access token expired.",
    Some(48001) => "API not authorized; check application permissions.",
    Some(60020) => "IP address not in the allowlist.",
    Some(81013) => "All recipients are invalid or unauthorized.",
    _ => obj.get("errmsg").and_then(Value::as_str).unwrap_or("Unknown error"),
  };
  Some(format!("errcode={}: {}", errcode, message))
}

#[async_trait]
impl Notify for WeChat {
  fn schemas(&self) -> &[&str] {
    &["wechat"]
  }
  fn service_name(&self) -> &str {
    "WeChat (WeCom)"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  // Text and markdown bodies are limited to 2048 bytes
  fn body_maxlen(&self) -> usize {
    2048
  }
  // No title field; the title is folded into the body
  fn title_maxlen(&self) -> usize {
    0
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["wechat".into(), self.corpid.clone(), self.corpsecret.clone(), self.agentid.to_string()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.users.is_empty() && self.departments.is_empty() && self.tag_ids.is_empty() {
      tracing::warn!("No WeChat (WeCom) recipients configured; aborting.");
      return Ok(false);
    }

    let base = service_base("https://qyapi.weixin.qq.com");
    let client = build_client(self.verify_certificate)?;
    let Some(token) = self.access_token(&client, &base).await else {
      return Ok(false);
    };

    // Markdown is sent natively; text and HTML go out as text
    let msgtype = if ctx.body_format == NotifyFormat::Markdown { "markdown" } else { "text" };
    let mut payload = json!({ "agentid": self.agentid, "msgtype": msgtype });
    payload[msgtype] = json!({ "content": ctx.body });
    if !self.users.is_empty() {
      payload["touser"] = json!(self.users.join("|"));
    }
    if !self.departments.is_empty() {
      payload["toparty"] = json!(self.departments.join("|"));
    }
    if !self.tag_ids.is_empty() {
      payload["totag"] = json!(self.tag_ids.join("|"));
    }

    crate::notify::throttle().await;
    let resp = client
      .post(format!("{}/cgi-bin/message/send?access_token={}", base, token))
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .body(serde_json::to_vec(&payload)?)
      .send()
      .await;
    let resp = match resp {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending WeChat (WeCom) notification.");
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return Ok(false);
      }
    };
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status != 200 {
      tracing::warn!("Failed to send WeChat (WeCom) notification: error={}.", status);
      tracing::debug!("Response Details: {}", text);
      return Ok(false);
    }
    let content: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(error) = api_error(&content) {
      // A rejected token is dropped so the next send fetches a new one
      if content.get("errcode").and_then(Value::as_i64).is_some_and(|c| TOKEN_ERROR_CODES.contains(&c)) {
        crate::storage::with_service_store(|s| s.remove("access_token"));
      }
      tracing::warn!("Failed to send WeChat (WeCom) notification: {}.", error);
      return Ok(false);
    }
    tracing::info!("Sent WeChat (WeCom) notification.");
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const CORPID: &str = "wwcorpid1234567890";
  const CORPSECRET: &str = "abcSecret9";
  const AGENTID: &str = "1000002";
  const TOKEN_RESPONSE: &str = r#"{"errcode": 0, "errmsg": "ok", "access_token": "TOKEN12345678", "expires_in": 7200}"#;
  const SEND_OK: &str = r#"{"errcode": 0, "errmsg": "ok"}"#;

  fn url(targets: &str) -> String {
    format!("wechat://{}:{}@{}{}", CORPID, CORPSECRET, AGENTID, targets)
  }

  fn parse(url: &str) -> Option<WeChat> {
    WeChat::from_url(&ParsedUrl::parse(url)?)
  }

  /// Mount the token and send endpoints and run one send.
  async fn run(url: &str, ctx: &NotifyContext, token: (u16, &str), send: (u16, &str)) -> (bool, MockServer) {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/cgi-bin/gettoken")).respond_with(ResponseTemplate::new(token.0).set_body_string(token.1)).mount(&server).await;
    Mock::given(method("POST")).and(path("/cgi-bin/message/send")).respond_with(ResponseTemplate::new(send.0).set_body_string(send.1)).mount(&server).await;
    let svc = from_url(url).unwrap();
    let ok = crate::notify::with_test_base(server.uri(), svc.send(ctx)).await.unwrap();
    (ok, server)
  }

  async fn posts(server: &MockServer) -> Vec<Value> {
    crate::notify::custom_json::requests_to(server, "POST", "/cgi-bin/message/send").await.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      url(""),
      url("/@all"),
      url("/johndoe"),
      url("/@johndoe"),
      url("/user1/user2/user3"),
      url("/%23100"),
      url("/+7"),
      url("/john/%23200/+3"),
      url("?to=@all"),
      url("/@all?format=markdown"),
      url("/!!bad!!"),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "wechat://".to_string(),
      format!("wechat://{}", CORPID),
      format!("wechat://{}:{}@", CORPID, CORPSECRET),
      format!("wechat://{}:{}@notanumber", CORPID, CORPSECRET),
      format!("wechat://{}@{}", CORPID, AGENTID),
      format!("wechat://:{}@{}", CORPSECRET, AGENTID),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_target_classification() {
    let obj = parse(&url("/validuser/!!invalid!!/%2342/+7")).unwrap();
    assert_eq!(obj.users, vec!["validuser"]);
    assert_eq!(obj.departments, vec!["42"]);
    assert_eq!(obj.tag_ids, vec!["7"]);
    assert_eq!(obj.corpid, CORPID);
    assert_eq!(obj.corpsecret, CORPSECRET);
    assert_eq!(obj.agentid, 1000002);

    // The @ prefix is optional and stripped; "all" and "@all" both broadcast
    let obj = parse(&url("/@johndoe/@all/all")).unwrap();
    assert_eq!(obj.users, vec!["@all", "johndoe", "@all"]);

    // ?to= adds targets; # must be %23-encoded in a URL
    let obj = parse(&url("/alice?to=bob,%2310")).unwrap();
    assert_eq!(obj.users, vec!["alice", "bob"]);
    assert_eq!(obj.departments, vec!["10"]);

    // Non-numeric departments and tags are not departments or tags
    let obj = parse(&url("/%23abc/+x1")).unwrap();
    assert!(obj.departments.is_empty() && obj.tag_ids.is_empty() && obj.users.is_empty());
  }

  #[test]
  fn test_api_error() {
    assert!(api_error(&json!({ "errcode": 0, "errmsg": "ok" })).is_none());
    assert!(api_error(&json!({ "access_token": "x" })).is_none());
    assert_eq!(api_error(&json!({ "errcode": 40014 })).unwrap(), "errcode=40014: Invalid access token.");
    assert_eq!(api_error(&json!({ "errcode": 12345, "errmsg": "custom" })).unwrap(), "errcode=12345: custom");
    assert_eq!(api_error(&json!({ "errcode": 12345 })).unwrap(), "errcode=12345: Unknown error");
    for content in [json!({}), Value::Null, json!([0])] {
      assert!(api_error(&content).is_some(), "{}", content);
    }
  }

  #[test]
  fn test_service_attributes() {
    let svc = from_url(&url("/@all")).unwrap();
    assert_eq!(svc.body_maxlen(), 2048);
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.notify_format(), NotifyFormat::Text);
    assert!(!svc.attachment_support());
  }

  #[tokio::test]
  async fn test_send_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/cgi-bin/gettoken"))
      .and(query_param("corpid", CORPID))
      .and(query_param("corpsecret", CORPSECRET))
      .respond_with(ResponseTemplate::new(200).set_body_string(TOKEN_RESPONSE))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/cgi-bin/message/send"))
      .and(query_param("access_token", "TOKEN12345678"))
      .and(header("User-Agent", APP_ID))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(200).set_body_string(SEND_OK))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&url("/@all")).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Hello 中"))).await.unwrap());
    let payloads = posts(&server).await;
    assert_eq!(payloads[0], json!({ "agentid": 1000002, "msgtype": "text", "text": { "content": "Hello 中" }, "touser": "@all" }));
  }

  #[tokio::test]
  async fn test_send_markdown_all_targets() {
    // Matches pycapture: users, departments and tags in one request
    let ctx = NotifyContext { body: "# T\n**B** 中".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
    let (ok, server) = run(&url("/john/%23200/+3/@all/bad!target"), &ctx, (200, TOKEN_RESPONSE), (200, SEND_OK)).await;
    assert!(ok);
    assert_eq!(
      posts(&server).await[0],
      json!({
        "agentid": 1000002,
        "msgtype": "markdown",
        "markdown": { "content": "# T\n**B** 中" },
        "touser": "@all|john",
        "toparty": "200",
        "totag": "3",
      })
    );
  }

  #[tokio::test]
  async fn test_send_html_goes_out_as_text() {
    let ctx = NotifyContext { body: "<b>B</b>".into(), body_format: NotifyFormat::Html, ..Default::default() };
    let (ok, server) = run(&url("/alice/bob/%2310/%2320/+3/+5"), &ctx, (200, TOKEN_RESPONSE), (200, SEND_OK)).await;
    assert!(ok);
    let payload = &posts(&server).await[0];
    assert_eq!(payload["msgtype"], "text");
    assert_eq!(payload["touser"], "alice|bob");
    assert_eq!(payload["toparty"], "10|20");
    assert_eq!(payload["totag"], "3|5");
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let server = MockServer::start().await;
    let svc = from_url(&url("")).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
    // Nothing is requested without a recipient (other processes probing
    // localhost ports can still reach the mock server, so only look at ours)
    assert!(posts(&server).await.is_empty());
    assert!(crate::notify::custom_json::requests_to(&server, "GET", "/cgi-bin/gettoken").await.is_empty());
  }

  #[tokio::test]
  async fn test_send_token_failures() {
    for token in [
      (500u16, TOKEN_RESPONSE),
      (200, r#"{"errcode": 40001, "errmsg": "invalid credential"}"#),
      (200, r#"{"errcode": 99999, "errmsg": "something odd"}"#),
      (200, r#"{"errcode": 0, "errmsg": "ok"}"#),
      (200, r#"{"errcode": 0, "access_token": ""}"#),
      (200, "not-json"),
      (200, "{}"),
    ] {
      let (ok, server) = run(&url("/@all"), &ctx("B"), token, (200, SEND_OK)).await;
      assert!(!ok, "{:?}", token);
      // No message is sent without a token
      assert!(posts(&server).await.is_empty(), "{:?}", token);
    }
  }

  #[tokio::test]
  async fn test_send_failures() {
    for send in [
      (500u16, SEND_OK),
      (200, r#"{"errcode": 42001, "errmsg": "access_token expired"}"#),
      (200, r#"{"errcode": 81013, "errmsg": "user & party & tag all invalid"}"#),
      (200, r#"{"errcode": 99999}"#),
      (200, "not-json"),
      (200, ""),
    ] {
      let (ok, _) = run(&url("/@all"), &ctx("B"), (200, TOKEN_RESPONSE), send).await;
      assert!(!ok, "{:?}", send);
    }
    let svc = from_url(&url("/@all")).unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_without_store_fetches_token_every_time() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/cgi-bin/gettoken"))
      .respond_with(ResponseTemplate::new(200).set_body_string(TOKEN_RESPONSE))
      .expect(2)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/cgi-bin/message/send"))
      .respond_with(ResponseTemplate::new(200).set_body_string(SEND_OK))
      .expect(2)
      .mount(&server)
      .await;
    let svc = from_url(&url("/@all")).unwrap();
    for _ in 0..2 {
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
    }
  }

  #[tokio::test]
  async fn test_store_keeps_the_access_token() {
    let server = MockServer::start().await;
    // One token serves both notifications until WeCom rejects it
    Mock::given(method("GET"))
      .and(path("/cgi-bin/gettoken"))
      .respond_with(ResponseTemplate::new(200).set_body_string(TOKEN_RESPONSE))
      .expect(2)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/cgi-bin/message/send"))
      .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"errcode": 42001, "errmsg": "access_token expired"}"#))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/cgi-bin/message/send")).respond_with(ResponseTemplate::new(200).set_body_string(SEND_OK)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&url("/@all")));
    // The first send is refused with an expired token, which is dropped
    assert!(!crate::notify::with_test_base(server.uri(), a.notify(&ctx("B"))).await.is_success());
    for _ in 0..2 {
      assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("B"))).await.is_success());
    }
    let tokens: Vec<String> = crate::notify::custom_json::requests_to(&server, "POST", "/cgi-bin/message/send")
      .await
      .iter()
      .map(|r| r.url.query_pairs().find(|(k, _)| k == "access_token").unwrap().1.into_owned())
      .collect();
    assert_eq!(tokens, vec!["TOKEN12345678"; 3]);
  }
}
