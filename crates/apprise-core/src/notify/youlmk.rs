use async_trait::async_trait;
use serde_json::json;

use crate::asset::NotifyImageSize;
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, parse_bool, unquote};

/// The four priorities a sender may set, lowest first.
const PRIORITIES: [&str; 4] = ["low", "normal", "high", "critical"];

/// The payload rejects a longer label on the primary button.
const URL_LABEL_MAXLEN: usize = 24;

/// Resolve a full or short-form priority (`crit`, `c`) to one of the four.
fn youlmk_priority(value: &str) -> Option<&'static str> {
  let v = value.trim().to_lowercase();
  if v.is_empty() {
    return None;
  }
  PRIORITIES.iter().find(|p| p.starts_with(&v)).copied()
}

/// The default priority for each notification type. `critical` is never a
/// default: it can break through quiet hours, so it is opted into.
fn default_priority(notify_type: &NotifyType) -> &'static str {
  match notify_type {
    NotifyType::Info | NotifyType::Success => "normal",
    NotifyType::Warning | NotifyType::Failure => "high",
  }
}

/// YouLMK hosted notification inbox.
pub struct YouLmk {
  /// Either the bearer token (`ylk_` + 32) or the URL key (`k_` + 14)
  token: String,
  /// Forces every notification to this priority
  priority: Option<&'static str>,
  /// Priority per notification type: info, success, warning, failure
  priority_map: [&'static str; 4],
  link: Option<String>,
  url_label: Option<String>,
  group: Option<String>,
  include_image: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl YouLmk {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The registry turns the key door pasted as-is
    // (https://youlmk.com/k/{key}) into youlmk://{key}. Upstream unquotes
    // the query values once more.
    let qsd = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(unquote);
    let mut token = url.host.clone();
    if let Some(t) = qsd("token") {
      token = Some(t);
    }
    let token = token.unwrap_or_default().trim().to_string();
    if !is_token(&token) {
      tracing::warn!("An invalid YouLMK token or key was specified.");
      return None;
    }

    let priority = match qsd("priority") {
      None => None,
      Some(p) => match youlmk_priority(&p) {
        Some(p) => Some(p),
        None => {
          tracing::warn!("An invalid YouLMK priority ({}) was specified.", p);
          return None;
        }
      },
    };

    let mut priority_map = [NotifyType::Info, NotifyType::Success, NotifyType::Warning, NotifyType::Failure].map(|t| default_priority(&t));
    for (slot, key) in priority_map.iter_mut().zip(["info", "success", "warning", "failure"]) {
      if let Some(value) = qsd(key) {
        match youlmk_priority(&value) {
          Some(p) => *slot = p,
          None => {
            tracing::warn!("An invalid YouLMK priority ({}) was specified.", value);
            return None;
          }
        }
      }
    }

    let url_label = qsd("url_label").map(|l| {
      if l.chars().count() > URL_LABEL_MAXLEN {
        tracing::warn!("The YouLMK link label was truncated.");
      }
      l.chars().take(URL_LABEL_MAXLEN).collect()
    });

    Some(Self {
      token,
      priority,
      priority_map,
      link: qsd("url"),
      url_label,
      group: qsd("group"),
      include_image: url.get("image").map(parse_bool).unwrap_or(false),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "YouLMK",
      service_url: Some("https://youlmk.com/"),
      setup_url: Some("https://appriseit.com/services/youlmk/"),
      protocols: vec!["youlmk"],
      description: "Send notifications to a YouLMK inbox source.",
      attachment_support: false,
    }
  }

  /// True for the bearer token, false for the URL key.
  fn is_bearer(&self) -> bool {
    self.token.to_lowercase().starts_with("ylk_")
  }

  fn resolve_priority(&self, notify_type: &NotifyType) -> &'static str {
    self.priority.unwrap_or(match notify_type {
      NotifyType::Info => self.priority_map[0],
      NotifyType::Success => self.priority_map[1],
      NotifyType::Warning => self.priority_map[2],
      NotifyType::Failure => self.priority_map[3],
    })
  }
}

/// `ylk_` + 32 alphanumerics, or `k_` + 14 alphanumerics (either case).
fn is_token(s: &str) -> bool {
  let lower = s.to_lowercase();
  let rest = if let Some(r) = lower.strip_prefix("ylk_") {
    (r.len() == 32).then_some(r)
  } else if let Some(r) = lower.strip_prefix("k_") {
    (r.len() == 14).then_some(r)
  } else {
    None
  };
  rest.is_some_and(|r| r.bytes().all(|c| c.is_ascii_alphanumeric()))
}

#[async_trait]
impl Notify for YouLmk {
  fn schemas(&self) -> &[&str] {
    &["youlmk"]
  }
  fn service_name(&self) -> &str {
    "YouLMK"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    2000
  }
  fn title_maxlen(&self) -> usize {
    120
  }
  // 60 a minute per key
  fn request_rate_per_sec(&self) -> f64 {
    1.0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let base = service_base("https://youlmk.com");
    // The bearer token goes to the /v1 door in a header; the URL key goes
    // in the address of the /k door
    let url = if self.is_bearer() { format!("{}/v1/notify", base) } else { format!("{}/k/{}", base, self.token) };

    // A title is required; the application description stands in
    let title = if !ctx.title.is_empty() {
      ctx.title.as_str()
    } else if !ctx.asset.app_desc.is_empty() {
      ctx.asset.app_desc.as_str()
    } else {
      ctx.asset.app_id.as_str()
    };

    let mut payload = json!({
      "title": title,
      "body": ctx.body,
      "priority": self.resolve_priority(&ctx.notify_type),
    });
    if let Some(link) = &self.link {
      payload["url"] = json!(link);
      // The label only means something alongside an address
      if let Some(label) = &self.url_label {
        payload["url_label"] = json!(label);
      }
    }
    if let Some(group) = &self.group {
      payload["group"] = json!(group);
    }
    if self.include_image
      && let Some(image) = ctx.image_url(NotifyImageSize::Xy256)
    {
      payload["image"] = json!(image);
    }

    let client = build_client(self.verify_certificate)?;
    let mut req =
      client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json; charset=utf-8").body(serde_json::to_string(&payload)?);
    if self.is_bearer() {
      req = req.header("Authorization", format!("Bearer {}", self.token));
    }

    crate::notify::throttle().await;
    let resp = match req.send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred posting to YouLMK: {}", crate::error::loggable_http_error(&e));
        return Ok(false);
      }
    };
    let status = resp.status().as_u16();
    if status != 200 {
      tracing::warn!("Failed to send YouLMK notification: {}error={}.", error_hint(status), status);
      tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
      return Ok(false);
    }
    tracing::info!("Sent YouLMK notification.");
    Ok(true)
  }
}

/// YouLMK's own explanation of an HTTP status, for the log.
fn error_hint(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - The payload could not be read as JSON., ",
    401 => "Unauthorized - Unknown key or token., ",
    402 => "Payment Required - The trial's 10 notifications are used., ",
    410 => "Gone - The key was rotated or the source deleted., ",
    413 => "Payload Too Large - The body is over 8 KB., ",
    422 => "Unprocessable - A field is over its limit., ",
    429 => "Too Many Requests - 60 a minute per key., ",
    503 => "Service Unavailable - Sending is paused., ",
    _ => "",
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use serde_json::Value;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  /// The POSTs this test sent; other processes probing localhost ports can
  /// reach a mock server too, so anything else is ignored.
  async fn received(server: &MockServer) -> Vec<wiremock::Request> {
    let mut requests = crate::notify::custom_json::requests_to(server, "POST", "/v1/notify").await;
    requests.extend(crate::notify::custom_json::requests_to(server, "POST", &format!("/k/{}", KEY)).await);
    requests
  }

  const TOKEN: &str = "ylk_a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6";
  const KEY: &str = "k_7hq2nk3f9qd8w4";

  /// Parse `url`, converting a native URL the way the registry does.
  fn parse(url: &str) -> Option<YouLmk> {
    let native = crate::notify::registry::NATIVE_URLS.iter().find_map(|n| (n.convert)(url));
    YouLmk::from_url(&ParsedUrl::parse(native.as_deref().unwrap_or(url))?)
  }

  async fn send_one(url: &str, ctx: &NotifyContext) -> (bool, Vec<wiremock::Request>) {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/notify")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    Mock::given(method("POST")).and(path(format!("/k/{}", KEY))).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let svc = from_url(url).unwrap();
    let ok = crate::notify::with_test_base(server.uri(), svc.send(ctx)).await.unwrap();
    (ok, received(&server).await)
  }

  fn body(req: &wiremock::Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("youlmk://{}", TOKEN),
      format!("youlmk://{}", KEY),
      format!("youlmk://{}", TOKEN.to_uppercase()),
      format!("youlmk://?token={}", TOKEN),
      format!("youlmk://{}?priority=critical", TOKEN),
      format!("youlmk://{}?failure=critical&info=low", TOKEN),
      format!("youlmk://{}?url=https://example.com/runs/1&group=deploys", TOKEN),
      format!("youlmk://{}?url=https://example.com&url_label=View%20run", TOKEN),
      format!("youlmk://{}?image=yes", TOKEN),
      format!("https://youlmk.com/k/{}", KEY),
      format!("https://www.youlmk.com/k/{}/?priority=low", KEY),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "youlmk://".to_string(),
      "youlmk://invalid".to_string(),
      "youlmk://ylk_tooshort".to_string(),
      format!("youlmk://ylk_{}", "x".repeat(31)),
      format!("youlmk://{}?priority=bogus", TOKEN),
      format!("youlmk://{}?warning=bogus", TOKEN),
      // The bearer door is not a key door, and a key door needs a key
      "https://youlmk.com/v1/notify".to_string(),
      "https://youlmk.com/k/k_short".to_string(),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_priority_resolution() {
    assert_eq!(youlmk_priority("low"), Some("low"));
    assert_eq!(youlmk_priority("l"), Some("low"));
    assert_eq!(youlmk_priority("NORMAL"), Some("normal"));
    assert_eq!(youlmk_priority("hi"), Some("high"));
    assert_eq!(youlmk_priority("crit"), Some("critical"));
    assert_eq!(youlmk_priority(""), None);
    assert_eq!(youlmk_priority("   "), None);
    assert_eq!(youlmk_priority("bogus"), None);
  }

  #[test]
  fn test_parse_fields() {
    // The case of the token is kept
    let obj = parse(&format!("youlmk://{}", TOKEN.to_uppercase())).unwrap();
    assert_eq!(obj.token, TOKEN.to_uppercase());
    assert!(obj.is_bearer());
    assert!(!parse(&format!("youlmk://{}", KEY)).unwrap().is_bearer());

    // ?token= overrides the host
    assert_eq!(parse(&format!("youlmk://{}?token={}", KEY, TOKEN)).unwrap().token, TOKEN);

    // Native URLs carry their query arguments
    let obj = parse(&format!("https://www.youlmk.com/k/{}/?priority=low", KEY)).unwrap();
    assert_eq!(obj.token, KEY);
    assert_eq!(obj.priority, Some("low"));

    // Labels are trimmed to 24 characters
    let obj = parse(&format!("youlmk://{}?url_label={}", TOKEN, "x".repeat(30))).unwrap();
    assert_eq!(obj.url_label.as_deref(), Some("x".repeat(24).as_str()));
    assert!(!obj.include_image);
  }

  #[test]
  fn test_service_attributes() {
    let svc = from_url(&format!("youlmk://{}", TOKEN)).unwrap();
    assert_eq!(svc.body_maxlen(), 2000);
    assert_eq!(svc.title_maxlen(), 120);
    assert_eq!(svc.request_rate_per_sec(), 1.0);
    assert!(!svc.attachment_support());
  }

  #[tokio::test]
  async fn test_send_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/notify"))
      .and(header("User-Agent", APP_ID))
      .and(header("Content-Type", "application/json; charset=utf-8"))
      .and(header("Authorization", format!("Bearer {}", TOKEN).as_str()))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("youlmk://{}", TOKEN)).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = received(&server).await;
    assert_eq!(body(&requests[0]), json!({ "title": "T", "body": "B", "priority": "normal" }));
  }

  #[tokio::test]
  async fn test_send_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(format!("/k/{}", KEY))).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let svc = from_url(&format!("https://youlmk.com/k/{}?priority=low", KEY)).unwrap();
    let ctx = NotifyContext { title: "title".into(), body: "B".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = received(&server).await;
    // The key travels in the address, never in a header
    assert!(requests[0].headers.get("Authorization").is_none());
    assert_eq!(body(&requests[0]), json!({ "title": "title", "body": "B", "priority": "low" }));
  }

  #[tokio::test]
  async fn test_send_full_payload() {
    let url = format!("youlmk://{}?image=yes&url=https://e.com&url_label=View%20run%20and%20more%20text%20here&group=g&failure=crit&info=l", TOKEN);
    let ctx = NotifyContext { body: "B".into(), notify_type: NotifyType::Failure, ..Default::default() };
    let (ok, requests) = send_one(&url, &ctx).await;
    assert!(ok);
    // Matches pycapture: app_desc stands in for the missing title
    assert_eq!(
      body(&requests[0]),
      json!({
        "title": "Apprise Notifications",
        "body": "B",
        "priority": "critical",
        "url": "https://e.com",
        "url_label": "View run and more text h",
        "group": "g",
        "image": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-failure-256x256.png",
      })
    );
  }

  #[tokio::test]
  async fn test_send_priorities() {
    // Defaults per notification type
    for (notify_type, expected) in [(NotifyType::Info, "normal"), (NotifyType::Success, "normal"), (NotifyType::Warning, "high"), (NotifyType::Failure, "high")]
    {
      let ctx = NotifyContext { title: "T".into(), body: "B".into(), notify_type, ..Default::default() };
      let (_, requests) = send_one(&format!("youlmk://{}", TOKEN), &ctx).await;
      assert_eq!(body(&requests[0])["priority"], expected);
    }

    // Per-type overrides, and a forced priority beats them
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), notify_type: NotifyType::Success, ..Default::default() };
    let (_, requests) = send_one(&format!("youlmk://{}?success=crit", TOKEN), &ctx).await;
    assert_eq!(body(&requests[0])["priority"], "critical");
    let (_, requests) = send_one(&format!("youlmk://{}?success=crit&priority=low", TOKEN), &ctx).await;
    assert_eq!(body(&requests[0])["priority"], "low");
  }

  #[tokio::test]
  async fn test_send_label_needs_link() {
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    let (_, requests) = send_one(&format!("youlmk://{}?url_label=View", TOKEN), &ctx).await;
    let payload = body(&requests[0]);
    assert!(payload.get("url").is_none());
    assert!(payload.get("url_label").is_none());
  }

  #[tokio::test]
  async fn test_send_image_disabled_by_asset() {
    let mut ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    ctx.asset.image_url_mask = Some(String::new());
    let (_, requests) = send_one(&format!("youlmk://{}?image=yes", TOKEN), &ctx).await;
    assert!(body(&requests[0]).get("image").is_none());
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [401u16, 402, 500, 999, 202] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/v1/notify")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("youlmk://{}", TOKEN)).unwrap();
      let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), "status {}", status);
    }
    let svc = from_url(&format!("youlmk://{}", TOKEN)).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }

  #[test]
  fn test_blank_option_value_loads() {
    // Upstream trims query values, so a whitespace-only value counts as unset.
    let url = format!("youlmk://{}?priority=%20", TOKEN);
    assert!(crate::notify::registry::from_url(&url).is_some(), "Should parse: {}", url);
  }
}
