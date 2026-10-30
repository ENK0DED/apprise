use crate::error::NotifyError;
use crate::notify::custom_json::{attachment_filename, custom_args, header_map};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd;
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Value, json};
use std::sync::OnceLock;

/// `?method=` values; the first is the default.
const METHODS: &[&str] = &["form", "json"];

/// `?version=` values. Version 1 puts the token in the path, version 2 (the
/// default) sends it in the `X-Apprise-Config-ID` header.
const VERSIONS: &[&str] = &["1", "2"];

/// A random (version 4) UUID.
pub(crate) fn uuid4() -> String {
  let mut b: [u8; 16] = rand::random();
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  let h = hex::encode(b);
  format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Identifies this process to Apprise API servers so they can spot relay
/// loops (upstream `AppriseAsset._uid`, one UUID4 per process).
fn apprise_uid() -> &'static str {
  static UID: OnceLock<String> = OnceLock::new();
  UID.get_or_init(uuid4)
}

/// A token of 1 to `max` letters, digits, `_` or `-`.
fn valid_token(token: &str, max: usize) -> bool {
  (1..=max).contains(&token.len()) && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub struct AppriseApi {
  host: String,
  port: Option<u16>,
  /// Path in front of `/notify`, without surrounding slashes.
  fullpath: String,
  token: String,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  method: String,
  version: String,
  headers: Vec<(String, String)>,
  /// `?tags=`: tags the remote server notifies.
  relay_tags: Vec<String>,
  verify_certificate: bool,
  /// `?tag=`: this service's own tags. `?tags=` belongs to the relay.
  tags: Vec<String>,
}

impl AppriseApi {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let native = url.schema == "http" || url.schema == "https";
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let encoded: Vec<&str> = url.path.split('/').filter(|s| !s.is_empty()).collect();

    let (token, fullpath, secure) = if native {
      // http(s)://hostname[:port][/path]/notify/{token}
      if url.user.is_some() || url.password.is_some() || !host.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return None;
      }
      let n = url.path_parts.len();
      if n < 2 || !url.path_parts[n - 2].eq_ignore_ascii_case("notify") || !valid_token(&url.path_parts[n - 1], 32) {
        return None;
      }
      (url.path_parts[n - 1].clone(), encoded[..encoded.len() - 2].join("/"), url.schema == "https")
    } else if let Some(t) = qsd::get(url, "token").filter(|t| !t.is_empty()) {
      (qsd::unquote(&t), encoded.join("/"), url.schema == "apprises")
    } else if let Some(t) = qsd::get(url, "to").filter(|t| !t.is_empty()) {
      (qsd::unquote(&t), encoded.join("/"), url.schema == "apprises")
    } else {
      let token = url.path_parts.last().cloned().unwrap_or_default();
      (token, encoded[..encoded.len().saturating_sub(1)].join("/"), url.schema == "apprises")
    };
    let token = token.trim().to_string();
    if !valid_token(&token, 128) {
      tracing::warn!("The Apprise API token specified is invalid.");
      return None;
    }

    let method = qsd::get(url, "method").filter(|m| !m.is_empty()).map(|m| qsd::unquote(&m).to_lowercase()).unwrap_or_else(|| METHODS[0].to_string());
    if !METHODS.contains(&method.as_str()) {
      tracing::warn!("The method specified ({}) is invalid.", method);
      return None;
    }

    let version = qsd::get(url, "version")
      .filter(|v| !v.is_empty())
      .or_else(|| qsd::get(url, "v").filter(|v| !v.is_empty()))
      .map(|v| qsd::unquote(&v))
      // A native URL is a version 1 endpoint unless told otherwise
      .unwrap_or_else(|| if native { "1" } else { "2" }.to_string());
    if !VERSIONS.contains(&version.as_str()) {
      tracing::warn!("The Apprise API version specified ({}) is invalid.", version);
      return None;
    }

    Some(Self {
      host,
      port: url.port,
      fullpath,
      token,
      secure,
      user: url.user.clone(),
      password: url.password.clone(),
      method,
      version,
      headers: custom_args(url, '+'),
      relay_tags: qsd::get(url, "tags").map(|t| qsd::parse_list(&t)).unwrap_or_default(),
      verify_certificate: url.verify_certificate(),
      tags: url.get("tag").map(qsd::parse_list).unwrap_or_default(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Apprise API",
      service_url: Some("https://github.com/caronc/apprise-api"),
      setup_url: Some("https://appriseit.com/services/apprise_api/"),
      protocols: vec!["apprise", "apprises"],
      description: "Send via the Apprise REST API.",
      attachment_support: true,
    }
  }

  fn notify_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let path = if self.fullpath.is_empty() { String::new() } else { format!("/{}", self.fullpath) };
    let token = if self.version == "1" { format!("/{}", self.token) } else { String::new() };
    format!("{}://{}{}{}/notify{}", schema, self.host, port_str, path, token)
  }

  /// The payload fields shared by both methods. The body is relayed in the
  /// format it arrived in; the remote server converts it for its targets.
  fn fields(&self, ctx: &NotifyContext) -> Vec<(&'static str, String)> {
    let mut fields = vec![("title", ctx.title.clone()), ("body", ctx.body.clone()), ("type", ctx.notify_type.as_str().to_string())];
    fields.push(("format", ctx.body_format.as_str().to_string()));
    fields.extend(self.relay_tags.iter().map(|t| ("tag", t.clone())));
    fields
  }

  fn json_payload(&self, ctx: &NotifyContext) -> Value {
    let mut payload = json!({
      "title": ctx.title,
      "body": ctx.body,
      "type": ctx.notify_type.as_str(),
      "format": ctx.body_format.as_str(),
    });
    if !self.relay_tags.is_empty() {
      payload["tag"] = json!(self.relay_tags);
    }
    if !ctx.attachments.is_empty() {
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
      payload["attachments"] = Value::Array(attachments);
    }
    payload
  }
}

#[async_trait]
impl Notify for AppriseApi {
  fn schemas(&self) -> &[&str] {
    &["apprise", "apprises"]
  }
  fn service_name(&self) -> &str {
    "Apprise API"
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
    let url = self.notify_url();

    let mut base: Vec<(&str, &str)> = vec![("User-Agent", APP_ID)];
    if self.method == "json" {
      base.push(("Content-Type", "application/json"));
    }
    // These go on top of any `+` header overrides
    let mut overrides = self.headers.clone();
    if self.version == "2" {
      overrides.push(("X-Apprise-Config-ID".into(), self.token.clone()));
    }
    overrides.push(("Accept".into(), "application/json".into()));
    overrides.push(("X-Apprise-ID".into(), apprise_uid().into()));
    overrides.push(("X-Apprise-Recursion-Count".into(), "1".into()));
    let headers = header_map(&base, &overrides)?;

    tracing::debug!("Apprise API POST URL: {} (cert_verify={})", url, self.verify_certificate);

    let client = build_client(self.verify_certificate)?;
    let mut req = client.post(&url);
    req = if self.method == "json" {
      req.body(serde_json::to_string(&self.json_payload(ctx))?)
    } else if ctx.attachments.is_empty() {
      req.form(&self.fields(ctx))
    } else {
      let mut form = reqwest::multipart::Form::new();
      for (k, v) in self.fields(ctx) {
        form = form.text(k, v);
      }
      for (i, att) in ctx.attachments.iter().enumerate() {
        let part = reqwest::multipart::Part::bytes(att.data.clone())
          .file_name(attachment_filename(att, i + 1))
          .mime_str(&att.mime_type)
          .map_err(|_| NotifyError::InvalidParam(format!("invalid attachment mime type: {}", att.mime_type)))?;
        form = form.part(format!("file{:02}", i + 1), part);
      }
      req.multipart(form)
    };
    req = req.headers(headers);
    if self.user.is_some() || self.password.is_some() {
      // `requests` sends a missing password as the string `None`
      req = req.basic_auth(self.user.clone().unwrap_or_default(), Some(self.password.clone().unwrap_or_else(|| "None".to_string())));
    }

    let resp = req.send().await?;
    let status = resp.status().as_u16();
    // Upstream accepts 200 only
    if status == 200 {
      tracing::info!("Sent Apprise API notification; method={}.", self.method);
      Ok(true)
    } else {
      tracing::warn!("Failed to send Apprise API notification: error={}.", status);
      Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use crate::types::NotifyFormat;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn api(url: &str) -> AppriseApi {
    AppriseApi::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let a32 = "a".repeat(32);
    let urls = vec![
      format!("apprise://localhost/{}", a32),
      format!("apprise://localhost/{}", "a".repeat(128)),
      format!("apprise://localhost:8080/{}", "b".repeat(32)),
      format!("apprises://localhost/{}", "c".repeat(32)),
      format!("https://example.com/path/notify/{}", "d".repeat(32)),
      format!("http://example.com/notify/{}", "d".repeat(32)),
      format!("apprises://localhost/?to={}", "e".repeat(32)),
      format!("apprise://localhost/?token={}&to=abcd", "f".repeat(32)),
      "apprise://localhost/?token=abcd&tags=admin,team".to_string(),
      "apprise://user@localhost/mytoken0/?format=markdown".to_string(),
      "apprise://user@localhost/mytoken1/".to_string(),
      "apprise://localhost/mytoken1/?v=1".to_string(),
      "apprise://localhost:8080/mytoken/".to_string(),
      "apprise://user:pass@localhost:8080/mytoken2/".to_string(),
      "apprises://localhost/mytoken/".to_string(),
      "apprises://localhost:8080/abc123/?method=json".to_string(),
      "apprises://localhost:8080/abc123/?method=form".to_string(),
      "apprises://user:password@localhost:8080/mytoken5/".to_string(),
      "apprises://localhost:8080/path?+HeaderKey=HeaderValue".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "apprise://".to_string(),
      "apprise://:@/".to_string(),
      "apprise://localhost".to_string(),
      "apprise://localhost/!".to_string(),
      "apprise://localhost/%%20".to_string(),
      format!("apprise://localhost/{}", "a".repeat(129)),
      "apprise://localhost/mytoken1/?version=3".to_string(),
      "apprises://localhost:8080/abc123/?method=invalid".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    // Native URLs take a token of at most 32 characters
    assert!(AppriseApi::from_url(&ParsedUrl::parse(&format!("http://example.com/notify/{}", "d".repeat(33))).unwrap()).is_none());
  }

  #[test]
  fn test_parse_fields() {
    let a = api("apprises://user:pass@localhost:8080/my/path/mytoken/?tags=team,admin&tag=local&+X-H=a+b&v=1");
    assert_eq!(a.token, "mytoken");
    assert_eq!(a.fullpath, "my/path");
    assert!(a.secure);
    assert_eq!(a.method, "form");
    assert_eq!(a.version, "1");
    assert_eq!(a.relay_tags, vec!["admin", "team"]);
    assert_eq!(a.tags, vec!["local"]);
    assert_eq!(a.headers, vec![("X-H".to_string(), "a+b".to_string())]);
    assert_eq!(a.notify_url(), "https://localhost:8080/my/path/notify/mytoken");

    // `?token=` keeps the whole path
    let a = api("apprise://localhost/path/?token=abcd&to=efgh");
    assert_eq!((a.token.as_str(), a.fullpath.as_str(), a.version.as_str()), ("abcd", "path", "2"));
    assert_eq!(a.notify_url(), "http://localhost/path/notify");

    // Native URLs default to version 1
    let a = api(&format!("https://example.com/path/notify/{}", "d".repeat(32)));
    assert_eq!((a.secure, a.fullpath.as_str(), a.version.as_str()), (true, "path", "1"));
    assert_eq!(a.notify_url(), format!("https://example.com/path/notify/{}", "d".repeat(32)));
    let a = api("http://example.com/notify/tok/?version=2&method=json");
    assert_eq!((a.secure, a.version.as_str(), a.method.as_str()), (false, "2", "json"));
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_form_v2() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/notify"))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(header("Accept", "application/json"))
      .and(header("X-Apprise-Config-ID", "mytoken"))
      .and(header("X-Apprise-Recursion-Count", "1"))
      .and(header("X-Apprise-ID", apprise_uid()))
      .and(header("User-Agent", APP_ID))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let a = api(&format!("apprise://localhost:{}/mytoken?tags=b,a", server.address().port()));
    assert!(a.send(&ctx("title", "**b**")).await.unwrap());
    let requests = requests_to(&server, "POST", "/notify").await;
    assert!(requests[0].headers.get("Authorization").is_none());
    let got: Vec<(String, String)> = url::form_urlencoded::parse(&requests[0].body).into_owned().collect();
    let expected: Vec<(String, String)> = [("title", "title"), ("body", "**b**"), ("type", "info"), ("format", "text"), ("tag", "a"), ("tag", "b")]
      .iter()
      .map(|(k, v)| (k.to_string(), v.to_string()))
      .collect();
    assert_eq!(got, expected);
  }

  #[tokio::test]
  async fn test_json_v1_with_auth_and_attachments() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/path/notify/mytoken"))
      .and(header("Content-Type", "application/json"))
      .and(header("X-H", "1"))
      // "user:None", as `requests` sends a missing password
      .and(header("Authorization", "Basic dXNlcjpOb25l"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let a = api(&format!("apprise://user@localhost:{}/path/mytoken?method=json&tags=a,b&+X-H=1&version=1", server.address().port()));
    let mut c = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("title", "**b**") };
    c.attachments.push(Attachment { name: "a.txt".into(), data: b"hello".to_vec(), mime_type: "text/plain".into() });
    c.attachments.push(Attachment { name: String::new(), data: b"x".to_vec(), mime_type: "image/png".into() });
    assert!(a.send(&c).await.unwrap());

    let requests = requests_to(&server, "POST", "/path/notify/mytoken").await;
    assert!(requests[0].headers.get("X-Apprise-Config-ID").is_none());
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
      body,
      json!({
        "title": "title", "body": "**b**", "type": "info", "format": "markdown", "tag": ["a", "b"],
        "attachments": [
          {"filename": "a.txt", "base64": "aGVsbG8=", "mimetype": "text/plain"},
          {"filename": "file002.dat", "base64": "eA==", "mimetype": "image/png"},
        ],
      })
    );
  }

  #[tokio::test]
  async fn test_form_multipart_attachments() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/notify")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    // A password without a user authenticates as ":pass"
    let a = api(&format!("apprise://:pass@localhost:{}/tok?tags=x", server.address().port()));
    let mut c = ctx("t", "b");
    c.attachments.push(Attachment { name: "a.txt".into(), data: b"hello".to_vec(), mime_type: "text/plain".into() });
    c.attachments.push(Attachment { name: "b.gif".into(), data: b"GIF89a".to_vec(), mime_type: "image/gif".into() });
    assert!(a.send(&c).await.unwrap());

    let requests = requests_to(&server, "POST", "/notify").await;
    assert_eq!(requests[0].headers.get("Authorization").unwrap(), "Basic OnBhc3M=");
    let body = String::from_utf8_lossy(&requests[0].body);
    for expected in [
      "name=\"title\"\r\n\r\nt\r\n",
      "name=\"format\"\r\n\r\ntext\r\n",
      "name=\"tag\"\r\n\r\nx\r\n",
      "name=\"file01\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n",
      "name=\"file02\"; filename=\"b.gif\"\r\nContent-Type: image/gif\r\n\r\nGIF89a\r\n",
    ] {
      assert!(body.contains(expected), "missing {expected:?}");
    }
  }

  #[tokio::test]
  async fn test_only_200_is_success() {
    for status in [201u16, 204, 500] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/notify")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let a = api(&format!("apprise://localhost:{}/tok", server.address().port()));
      assert!(matches!(a.send(&ctx("t", "b")).await, Err(NotifyError::ServiceError { status: s, .. }) if s == status));
    }
    assert!(matches!(api("apprise://127.0.0.1:1/tok").send(&ctx("t", "b")).await, Err(NotifyError::Http(_))));
  }

  #[test]
  fn test_uid_is_uuid4() {
    let uid = apprise_uid();
    assert_eq!(uid.len(), 36);
    assert_eq!(&uid[14..15], "4");
    assert_eq!(uid, apprise_uid());
  }
}
