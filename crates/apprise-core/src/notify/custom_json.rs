use crate::error::NotifyError;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd;
use async_trait::async_trait;
use base64::Engine;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

/// Methods the custom JSON, XML and form plugins accept for `?method=`.
pub(crate) const METHODS: &[&str] = &["POST", "GET", "DELETE", "PUT", "HEAD", "PATCH", "UPDATE", "OPTIONS"];

/// Parse `?method=` as upstream does: an empty value means POST and anything
/// outside [`METHODS`] rejects the URL.
pub(crate) fn parse_method(url: &ParsedUrl) -> Option<String> {
  let method = match url.get("method").filter(|m| !m.is_empty()) {
    Some(m) => qsd::unquote(m).to_uppercase(),
    None => return Some(METHODS[0].to_string()),
  };
  if METHODS.contains(&method.as_str()) {
    Some(method)
  } else {
    tracing::warn!("The method specified ({}) is invalid.", method);
    None
  }
}

/// The `+`, `-` or `:` arguments of a custom plugin URL. These plugins
/// unquote what `parse_qsd` already decoded a second time.
pub(crate) fn custom_args(url: &ParsedUrl, prefix: char) -> Vec<(String, String)> {
  qsd::prefixed(url, prefix).into_iter().map(|(k, v)| (qsd::unquote(&k), qsd::unquote(&v))).collect()
}

/// The URL path as upstream's `fullpath`: still percent-encoded, with a
/// leading slash, or empty when the URL has no path.
pub(crate) fn fullpath(url: &ParsedUrl) -> String {
  if url.path.is_empty() { String::new() } else { format!("/{}", url.path) }
}

/// Build request headers from `base`, then apply the `+` overrides on top
/// (case-insensitively, as `requests` merges them).
pub(crate) fn header_map(base: &[(&str, &str)], overrides: &[(String, String)]) -> Result<HeaderMap, NotifyError> {
  let mut map = HeaderMap::new();
  for (k, v) in base.iter().map(|(k, v)| (*k, *v)).chain(overrides.iter().map(|(k, v)| (k.as_str(), v.as_str()))) {
    let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| NotifyError::InvalidParam(format!("invalid header name: {}", k)))?;
    let value = HeaderValue::from_str(v).map_err(|_| NotifyError::InvalidParam(format!("invalid value for header {}", k)))?;
    map.insert(name, value);
  }
  Ok(map)
}

/// Basic auth for a custom plugin. Upstream passes `(user, password)` to
/// `requests` whenever a user is set, so a missing password is sent as the
/// string `None`.
pub(crate) fn basic_auth(user: &Option<String>, password: &Option<String>) -> Option<(String, String)> {
  let user = user.as_ref().filter(|u| !u.is_empty())?;
  Some((user.clone(), password.clone().unwrap_or_else(|| "None".to_string())))
}

/// Apply `:key=value` payload extras the way upstream does, one at a time:
/// an existing key is renamed to `value` (or dropped when `value` is empty)
/// and an unknown key is added with `value`.
pub(crate) fn apply_payload_extras(payload: &mut Map<String, Value>, extras: &[(String, String)]) {
  for (key, value) in extras {
    if payload.contains_key(key) {
      if !value.is_empty() {
        let moved = payload[key].clone();
        payload.insert(value.clone(), moved);
      }
      payload.remove(key);
    } else {
      payload.insert(key.clone(), Value::String(value.clone()));
    }
  }
}

/// The filename upstream sends for attachment `no` (1-based).
pub(crate) fn attachment_filename(att: &Attachment, no: usize) -> String {
  if att.name.is_empty() { format!("file{:03}.dat", no) } else { att.name.clone() }
}

pub struct Json {
  host: String,
  port: Option<u16>,
  fullpath: String,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  method: String,
  headers: Vec<(String, String)>,
  params: Vec<(String, String)>,
  payload_extras: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Json {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    Some(Self {
      host,
      port: url.port,
      fullpath: fullpath(url),
      secure: url.schema == "jsons",
      user: url.user.clone(),
      password: url.password.clone(),
      method: parse_method(url)?,
      headers: custom_args(url, '+'),
      params: custom_args(url, '-'),
      payload_extras: custom_args(url, ':'),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "JSON",
      service_url: None,
      setup_url: Some("https://appriseit.com/services/json/"),
      protocols: vec!["json", "jsons"],
      description: "Send a JSON notification to any HTTP endpoint.",
      attachment_support: true,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    let attachments: Vec<Value> = ctx
      .attachments
      .iter()
      .enumerate()
      .map(|(i, att)| {
        json!({
          "filename": attachment_filename(att, i + 1),
          "base64": base64::engine::general_purpose::STANDARD.encode(&att.data),
          "mimetype": att.mime_type,
        })
      })
      .collect();

    let mut payload = Map::new();
    payload.insert("version".into(), json!("1.0"));
    payload.insert("title".into(), json!(ctx.title));
    payload.insert("message".into(), json!(ctx.body));
    payload.insert("attachments".into(), Value::Array(attachments));
    payload.insert("type".into(), json!(ctx.notify_type.as_str()));
    apply_payload_extras(&mut payload, &self.payload_extras);
    Value::Object(payload)
  }
}

#[async_trait]
impl Notify for Json {
  fn schemas(&self) -> &[&str] {
    &["json", "jsons"]
  }
  fn service_name(&self) -> &str {
    "JSON"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    true
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}{}", schema, self.host, port_str, self.fullpath);

    let headers = header_map(&[("User-Agent", APP_ID), ("Content-Type", "application/json")], &self.headers)?;
    let body = serde_json::to_string(&self.payload(ctx))?;
    let method = reqwest::Method::from_bytes(self.method.as_bytes()).map_err(|_| NotifyError::InvalidParam(self.method.clone()))?;

    tracing::debug!("JSON {} URL: {} (cert_verify={})", self.method, url, self.verify_certificate);

    let client = build_client(self.verify_certificate)?;
    let mut req = client.request(method, &url).headers(headers).query(&self.params).body(body);
    if let Some((user, password)) = basic_auth(&self.user, &self.password) {
      req = req.basic_auth(user, Some(password));
    }

    let resp = req.send().await?;
    let status = resp.status();
    if status.is_success() {
      tracing::info!("Sent JSON {} notification.", self.method);
      Ok(true)
    } else {
      tracing::warn!("Failed to send JSON {} notification: error={}.", self.method, status.as_u16());
      Err(NotifyError::ServiceError { status: status.as_u16(), body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "json://localhost",
      "json://user:pass@localhost",
      "json://user@localhost",
      "json://user@localhost?method=put",
      "json://user@localhost?method=get",
      "json://user@localhost?method=post",
      "json://user@localhost?method=head",
      "json://user@localhost?method=delete",
      "json://user@localhost?method=patch",
      "json://user@localhost?method=update",
      "json://user@localhost?method=options",
      "json://user@localhost?method=",
      "json://localhost:8080",
      "json://user:pass@localhost:8080",
      "jsons://localhost",
      "jsons://user:pass@localhost",
      "jsons://localhost:8080/path/",
      "json://localhost:8080/path?-ParamA=Value",
      "jsons://user:password@localhost:8080",
      "json://localhost:8080/path?+HeaderKey=HeaderValue",
      "json://user:pass@localhost:8083",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["json://:@/", "json://", "jsons://", "json://user@localhost?method=invalid"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_fields() {
    let j =
      Json::from_url(&ParsedUrl::parse("json://localhost:8080/command?:message=msg&:test=value&method=GET&:type=&+X-Head=v+1&-Param=a+b").unwrap()).unwrap();
    assert_eq!(j.port, Some(8080));
    assert_eq!(j.fullpath, "/command");
    assert_eq!(j.method, "GET");
    assert_eq!(j.headers, vec![("X-Head".to_string(), "v+1".to_string())]);
    assert_eq!(j.params, vec![("Param".to_string(), "a+b".to_string())]);
    assert_eq!(
      j.payload_extras,
      vec![("message".to_string(), "msg".to_string()), ("test".to_string(), "value".to_string()), ("type".to_string(), String::new())]
    );

    // An empty method falls back to POST
    let j = Json::from_url(&ParsedUrl::parse("json://localhost?method=").unwrap()).unwrap();
    assert_eq!(j.method, "POST");

    // No path at all
    let j = Json::from_url(&ParsedUrl::parse("json://localhost").unwrap()).unwrap();
    assert_eq!(j.fullpath, "");
  }

  #[test]
  fn test_payload_extras_follow_upstream_order() {
    let mut payload = Map::new();
    payload.insert("title".into(), json!("t"));
    payload.insert("message".into(), json!("m"));
    // Rename, drop, add, then rename a key onto itself (upstream drops it)
    apply_payload_extras(
      &mut payload,
      &[("message".into(), "msg".into()), ("msg".into(), "text".into()), ("missing".into(), String::new()), ("title".into(), "title".into())],
    );
    assert_eq!(Value::Object(payload), json!({"text": "m", "missing": ""}));
  }

  #[test]
  fn test_basic_auth_without_password() {
    assert_eq!(basic_auth(&Some("user".into()), &None), Some(("user".into(), "None".into())));
    assert_eq!(basic_auth(&Some("user".into()), &Some("pass".into())), Some(("user".into(), "pass".into())));
    assert_eq!(basic_auth(&None, &Some("pass".into())), None);
  }

  #[test]
  fn test_service_details() {
    let j = Json::from_url(&ParsedUrl::parse("json://localhost").unwrap()).unwrap();
    assert!(j.attachment_support());
    assert_eq!(j.title_maxlen(), 250);
    assert_eq!(j.body_maxlen(), 32768);
    assert_eq!(Json::static_details().setup_url, Some("https://appriseit.com/services/json/"));
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  fn json_for(url: &str) -> Json {
    Json::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[tokio::test]
  async fn test_default_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/notify"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let j = json_for(&format!("json://localhost:{}/notify", server.address().port()));
    assert!(j.send(&ctx("hello", "world")).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("Authorization").is_none());
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body, json!({"version": "1.0", "title": "hello", "message": "world", "attachments": [], "type": "info"}));
  }

  #[tokio::test]
  async fn test_extras_headers_params_and_auth() {
    // Matches apprise-go's `extras` golden captured from upstream
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
      .and(path("/notify"))
      .and(query_param("ref", "xyz"))
      .and(header("X-Token", "abc"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let j = json_for(&format!(
      "json://user:pass@localhost:{}/notify?method=put&:message=body&:attachments=&:extra=note&+X-Token=abc&-ref=xyz",
      server.address().port()
    ));
    let c = NotifyContext { notify_type: NotifyType::Success, ..ctx("title", "hello") };
    assert!(j.send(&c).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body, json!({"version": "1.0", "title": "title", "body": "hello", "type": "success", "extra": "note"}));
  }

  #[tokio::test]
  async fn test_user_without_password_and_header_override() {
    let server = MockServer::start().await;
    // "user:None", as upstream sends it
    Mock::given(method("POST"))
      .and(header("Authorization", "Basic dXNlcjpOb25l"))
      .and(header("Content-Type", "text/plain"))
      .and(query_param("Param", "a+b"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let j = json_for(&format!("json://user@localhost:{}/?+Content-Type=text/plain&-Param=a+b", server.address().port()));
    assert!(j.send(&ctx("t", "b")).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].headers.get_all("Content-Type").iter().count(), 1);
  }

  #[tokio::test]
  async fn test_synology_style_remap() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/webapi/entry.cgi"))
      .and(query_param("api", "SYNO.Chat.External"))
      .and(query_param("method", "incoming"))
      .and(query_param("version", "2"))
      .and(query_param("token", "abc123"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let j = json_for(&format!(
      "json://localhost:{}/webapi/entry.cgi?-api=SYNO.Chat.External&-method=incoming&-version=2&-token=abc123\
       &:message=text&:version=&:type=&:title=&:attachments&:file_url=https://i.redd.it/my2t4d2fx0u31.jpg",
      server.address().port()
    ));
    assert_eq!(j.method, "POST");
    assert!(j.send(&ctx("title", "body")).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body, json!({"text": "body", "file_url": "https://i.redd.it/my2t4d2fx0u31.jpg"}));
  }

  #[tokio::test]
  async fn test_custom_methods_and_encoded_path() {
    for m in ["UPDATE", "OPTIONS", "GET", "DELETE", "PATCH"] {
      let server = MockServer::start().await;
      Mock::given(method(m)).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
      let j = json_for(&format!("json://localhost:{}/api/v4/projects/my%2Frepo/a%20b?method={}", server.address().port(), m.to_lowercase()));
      assert!(j.send(&ctx("t", "b")).await.unwrap(), "{}", m);
      let requests = server.received_requests().await.unwrap();
      assert_eq!(requests[0].url.path(), "/api/v4/projects/my%2Frepo/a%20b");
    }
  }

  #[tokio::test]
  async fn test_attachments() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let j = json_for(&format!("json://localhost:{}/", server.address().port()));
    let mut c = ctx("t", "b");
    c.attachments.push(Attachment { name: "a.txt".into(), data: b"hello".to_vec(), mime_type: "text/plain".into() });
    c.attachments.push(Attachment { name: String::new(), data: b"\x89PNG\r\n".to_vec(), mime_type: "image/png".into() });
    assert!(j.send(&c).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
      body["attachments"],
      json!([
        {"filename": "a.txt", "base64": "aGVsbG8=", "mimetype": "text/plain"},
        {"filename": "file002.dat", "base64": "iVBORw0K", "mimetype": "image/png"},
      ])
    );
  }

  #[tokio::test]
  async fn test_failure_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500).set_body_string("boom")).mount(&server).await;
    let j = json_for(&format!("jsons://localhost:{}/", server.address().port()));
    // jsons:// talks TLS, which the mock server does not; use json:// for the status check
    assert!(j.send(&ctx("t", "b")).await.is_err());

    let j = json_for(&format!("json://localhost:{}/", server.address().port()));
    match j.send(&ctx("t", "b")).await {
      Err(NotifyError::ServiceError { status, body }) => {
        assert_eq!(status, 500);
        assert_eq!(body, "boom");
      }
      other => panic!("unexpected: {:?}", other),
    }
  }

  #[tokio::test]
  async fn test_connection_refused() {
    let j = json_for("json://127.0.0.1:1/");
    assert!(matches!(j.send(&ctx("t", "b")).await, Err(NotifyError::Http(_))));
  }
}
