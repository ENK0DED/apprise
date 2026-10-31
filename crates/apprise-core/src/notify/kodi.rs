use async_trait::async_trait;
use serde_json::json;

use crate::asset::NotifyImageSize;
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, parse_bool};

/// XBMC listens on 8080 unless told otherwise.
const XBMC_DEFAULT_PORT: u16 = 8080;

/// Seconds a notification stays on screen.
const DEFAULT_DURATION: u64 = 12;

/// Which JSON-RPC protocol revision the host speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
  /// XBMC v2 (`xbmc://`, `xbmcs://`)
  Xbmc,
  /// Kodi v6 (`kodi://`, `kodis://`)
  Kodi,
}

/// Kodi (and legacy XBMC) on-screen notifications over JSON-RPC.
pub struct Kodi {
  host: String,
  port: Option<u16>,
  user: Option<String>,
  password: Option<String>,
  secure: bool,
  protocol: Protocol,
  duration: u64,
  include_image: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Kodi {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let (protocol, port) =
      if url.schema.starts_with("xbmc") { (Protocol::Xbmc, Some(url.port.unwrap_or(XBMC_DEFAULT_PORT))) } else { (Protocol::Kodi, url.port) };

    // An unreadable duration keeps the default; a negative one is made positive
    let duration = url.get("duration").and_then(|d| d.trim().parse::<i64>().ok()).map(i64::unsigned_abs).unwrap_or(DEFAULT_DURATION);

    Some(Self {
      host,
      port,
      user: url.user.clone().filter(|u| !u.is_empty()),
      password: url.password.clone(),
      secure: url.schema.ends_with('s'),
      protocol,
      duration,
      include_image: url.get("image").map(parse_bool).unwrap_or(true),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Kodi/XBMC",
      service_url: Some("http://kodi.tv/"),
      setup_url: Some("https://appriseit.com/services/kodi/"),
      protocols: vec!["kodi", "xbmc", "kodis", "xbmcs"],
      description: "Show notifications on Kodi (or legacy XBMC) through its JSON-RPC API.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> serde_json::Value {
    let mut payload = json!({
      "jsonrpc": "2.0",
      "method": "GUI.ShowNotification",
      "params": {
        "title": ctx.title,
        "message": ctx.body,
        // displaytime is in milliseconds
        "displaytime": self.duration.saturating_mul(1000),
      },
      "id": 1,
    });
    if self.include_image
      && let Some(image) = ctx.image_url(NotifyImageSize::Xy128)
    {
      payload["params"]["image"] = json!(image);
      // Kodi v6 also takes the notification type (upstream puts it at the
      // top level of the request, not in params)
      if self.protocol == Protocol::Kodi {
        payload["type"] = json!(match ctx.notify_type {
          NotifyType::Failure => "error",
          NotifyType::Warning => "warning",
          _ => "info",
        });
      }
    }
    payload
  }
}

#[async_trait]
impl Notify for Kodi {
  fn schemas(&self) -> &[&str] {
    &["kodi", "xbmc", "kodis", "xbmcs"]
  }
  fn service_name(&self) -> &str {
    "Kodi/XBMC"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn request_rate_per_sec(&self) -> f64 {
    // Upstream disables throttling for this service
    0.0
  }
  // Anything past the first two lines is too much to display
  fn body_max_line_count(&self) -> usize {
    2
  }
  /// Upstream's: the protocol's schema, the credentials, the host and the
  /// port (8080, or 443 over TLS, when none is given), each as Python's
  /// `str()` writes it, so a missing value is `None`.
  fn url_identifier(&self) -> Option<Vec<String>> {
    let schema = match self.protocol {
      Protocol::Xbmc => "xbmc",
      Protocol::Kodi => "kodi",
    };
    let port = self.port.unwrap_or(if self.secure { 443 } else { XBMC_DEFAULT_PORT });
    let text = |v: &Option<String>| v.clone().unwrap_or_else(|| "None".into());
    Some(vec![format!("{}{}", schema, if self.secure { "s" } else { "" }), text(&self.user), text(&self.password), self.host.clone(), port.to_string()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}/jsonrpc", schema, self.host, port);

    let client = build_client(self.verify_certificate)?;
    let mut req = client.post(&url).header("User-Agent", APP_ID).json(&self.payload(ctx));
    if let Some(user) = &self.user {
      // Upstream passes a missing password through as Python's None, which
      // requests encodes as the literal "None"; send an empty one instead,
      // as apprise-go does
      req = req.basic_auth(user, Some(self.password.as_deref().unwrap_or("")));
    }

    crate::notify::throttle().await;
    let resp = match req.send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending XBMC/KODI notification: {}", crate::error::loggable_http_error(&e));
        return Ok(false);
      }
    };
    let status = resp.status().as_u16();
    if status != 200 {
      tracing::warn!("Failed to send XBMC/KODI notification: error={}.", status);
      tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
      return Ok(false);
    }
    tracing::info!("Sent XBMC/KODI notification.");
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use serde_json::Value;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  /// The POSTs this test sent; other processes probing localhost ports can
  /// reach a mock server too, so anything else is ignored.
  async fn received(server: &MockServer) -> Vec<wiremock::Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "POST").collect()
  }

  fn parse(url: &str) -> Option<Kodi> {
    Kodi::from_url(&ParsedUrl::parse(url)?)
  }

  /// Send through a mock server, with `url` given as `schema://[auth@]` and
  /// `query` appended after the mock server's address.
  async fn send_to_mock(prefix: &str, query: &str, ctx: &NotifyContext, status: u16) -> (bool, Vec<wiremock::Request>) {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/jsonrpc")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    let addr = server.address();
    let svc = from_url(&format!("{}{}:{}{}", prefix, addr.ip(), addr.port(), query)).unwrap();
    let ok = svc.send(ctx).await.unwrap();
    (ok, received(&server).await)
  }

  fn body(req: &wiremock::Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
  }

  fn ctx(notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: "T".into(), body: "L1\r\nL2".into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "kodi://localhost",
      "kodi://192.168.4.1",
      "kodi://[2001:db8:002a:3256:adfe:05c0:0003:0006]",
      "kodi://[2001:db8:002a:3256:adfe:05c0:0003:0006]:8282",
      "kodi://user:pass@localhost",
      "kodi://localhost:8080",
      "kodi://user:pass@localhost:8080",
      "kodis://localhost",
      "kodis://user:pass@localhost",
      "kodis://localhost:8080/path/",
      "kodis://user:password@localhost:8080",
      "kodis://localhost:443",
      "kodi://user:pass@localhost:8083",
      "xbmc://localhost",
      "xbmc://localhost?duration=14",
      "xbmc://localhost?duration=invalid",
      "xbmc://localhost?duration=-1",
      "xbmc://user:pass@localhost",
      "xbmc://localhost:8080",
      "xbmc://user:pass@localhost:8080",
      "xbmc://user@localhost",
      "xbmc://user:pass@localhost:8083",
      "xbmcs://localhost",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["kodi://", "kodis://", "kodi://:@/", "xbmc://", "xbmcs://", "xbmc://:@/"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_fields() {
    // Kodi keeps no port unless given one; XBMC defaults to 8080
    let obj = parse("kodi://localhost").unwrap();
    assert_eq!((obj.protocol, obj.port, obj.secure), (Protocol::Kodi, None, false));
    let obj = parse("kodis://localhost").unwrap();
    assert_eq!((obj.protocol, obj.port, obj.secure), (Protocol::Kodi, None, true));
    let obj = parse("xbmc://localhost").unwrap();
    assert_eq!((obj.protocol, obj.port, obj.secure), (Protocol::Xbmc, Some(8080), false));
    let obj = parse("xbmcs://localhost:443").unwrap();
    assert_eq!((obj.protocol, obj.port, obj.secure), (Protocol::Xbmc, Some(443), true));
    assert_eq!(parse("kodi://localhost:9090").unwrap().port, Some(9090));

    // Durations: default, explicit, negative made positive, zero, garbage
    for (query, expected) in [("", 12), ("?duration=14", 14), ("?duration=-3", 3), ("?duration=0", 0), ("?duration=invalid", 12), ("?duration=", 12)] {
      assert_eq!(parse(&format!("xbmc://localhost{}", query)).unwrap().duration, expected, "{}", query);
    }

    // Images are on unless turned off
    assert!(parse("kodi://localhost").unwrap().include_image);
    assert!(!parse("kodi://localhost?image=no").unwrap().include_image);
    assert!(parse("kodi://localhost?image=yes").unwrap().include_image);

    let obj = parse("kodi://user:pass@localhost").unwrap();
    assert_eq!(obj.user.as_deref(), Some("user"));
    assert_eq!(obj.password.as_deref(), Some("pass"));
  }

  #[test]
  fn test_storage_id_matches_upstream() {
    // The uid upstream prints for every URL of its test file that loads
    for (url, uid) in [
      ("kodi://192.168.4.1", "96b97d3c"),
      ("kodi://[2001:db8:002a:3256:adfe:05c0:0003:0006]:8282", "9e488471"),
      ("kodi://[2001:db8:002a:3256:adfe:05c0:0003:0006]", "af66fbd7"),
      ("kodi://localhost", "1b25ac18"),
      ("kodi://localhost:8080", "1b25ac18"),
      ("kodis://localhost:443", "e59aa063"),
      ("kodis://localhost:8080/path/", "b2314261"),
      ("kodis://localhost", "e59aa063"),
      ("kodis://user:pass@localhost", "b440afb0"),
      ("kodis://user:password@localhost:8080", "76500587"),
      ("kodi://user:pass@localhost", "50de9b67"),
      ("kodi://user:pass@localhost:8080", "50de9b67"),
      ("kodi://user:pass@localhost:8081", "e0589209"),
      ("kodi://user:pass@localhost:8082", "aca7bf8a"),
      ("kodi://user:pass@localhost:8083", "f4b48387"),
      ("xbmc://localhost:8080", "ca3b303f"),
      ("xbmc://localhost", "ca3b303f"),
      ("xbmc://localhost?duration=14", "ca3b303f"),
      ("xbmc://localhost?duration=-1", "ca3b303f"),
      ("xbmc://localhost?duration=invalid", "ca3b303f"),
      ("xbmc://user@localhost", "8a053651"),
      ("xbmc://user:pass@localhost:8080", "cd2639e5"),
      ("xbmc://user:pass@localhost:8081", "f0406091"),
      ("xbmc://user:pass@localhost:8082", "3570680a"),
      ("xbmc://user:pass@localhost:8083", "380b9870"),
      ("xbmc://user:pass@localhost", "cd2639e5"),
    ] {
      let svc = from_url(url).unwrap_or_else(|| panic!("should load: {url}"));
      assert_eq!(crate::notify::options::url_id(svc.as_ref(), b"", 8).as_deref(), Some(uid), "{url}");
    }
  }

  #[test]
  fn test_service_attributes() {
    let svc = from_url("kodi://localhost").unwrap();
    assert_eq!(svc.body_max_line_count(), 2);
    assert_eq!(svc.request_rate_per_sec(), 0.0);
    assert!(!svc.attachment_support());
    let details = Kodi::static_details();
    assert_eq!(details.service_name, "Kodi/XBMC");
    // Upstream's order, insecure schemas first, as `--details` prints it
    assert_eq!(details.protocols, vec!["kodi", "xbmc", "kodis", "xbmcs"]);
  }

  #[tokio::test]
  async fn test_send_kodi_payload() {
    // Matches pycapture for kodi://user:pass@localhost with a warning
    let (ok, requests) = send_to_mock("kodi://user:pass@", "", &ctx(NotifyType::Warning), 200).await;
    assert!(ok);
    assert_eq!(
      body(&requests[0]),
      json!({
        "jsonrpc": "2.0",
        "method": "GUI.ShowNotification",
        "params": {
          "title": "T",
          "message": "L1\r\nL2",
          "displaytime": 12000,
          "image": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-warning-128x128.png",
        },
        "id": 1,
        "type": "warning",
      })
    );
    // "user:pass"
    assert_eq!(requests[0].headers.get("Authorization").unwrap(), "Basic dXNlcjpwYXNz");
    assert_eq!(requests[0].headers.get("User-Agent").unwrap(), APP_ID);
    assert_eq!(requests[0].headers.get("Content-Type").unwrap(), "application/json");
  }

  #[tokio::test]
  async fn test_send_kodi_types() {
    for (notify_type, expected) in [(NotifyType::Info, "info"), (NotifyType::Success, "info"), (NotifyType::Failure, "error")] {
      let image = format!("apprise-{}-128x128.png", notify_type.as_str());
      let (_, requests) = send_to_mock("kodi://", "", &ctx(notify_type), 200).await;
      let payload = body(&requests[0]);
      assert_eq!(payload["type"], expected);
      assert!(payload["params"]["image"].as_str().unwrap().ends_with(&image));
    }
  }

  #[tokio::test]
  async fn test_send_without_image() {
    // Without an image, Kodi gets no type either
    let (_, requests) = send_to_mock("kodi://", "?duration=0&image=no", &ctx(NotifyType::Warning), 200).await;
    assert_eq!(
      body(&requests[0]),
      json!({ "jsonrpc": "2.0", "method": "GUI.ShowNotification", "params": { "title": "T", "message": "L1\r\nL2", "displaytime": 0 }, "id": 1 })
    );
    assert!(requests[0].headers.get("Authorization").is_none());
  }

  #[tokio::test]
  async fn test_send_xbmc_payload() {
    // XBMC v2 carries the image but never a type
    let (ok, requests) = send_to_mock("xbmc://user@", "?duration=-3", &ctx(NotifyType::Failure), 200).await;
    assert!(ok);
    let payload = body(&requests[0]);
    assert_eq!(payload["params"]["displaytime"], 3000);
    assert!(payload["params"]["image"].as_str().unwrap().ends_with("apprise-failure-128x128.png"));
    assert!(payload.get("type").is_none());
    // A user without a password sends an empty one ("user:")
    assert_eq!(requests[0].headers.get("Authorization").unwrap(), "Basic dXNlcjo=");
  }

  #[tokio::test]
  async fn test_send_image_disabled_by_asset() {
    let mut ctx = ctx(NotifyType::Warning);
    ctx.asset.image_url_mask = Some(String::new());
    let (_, requests) = send_to_mock("kodi://", "", &ctx, 200).await;
    let payload = body(&requests[0]);
    assert!(payload["params"].get("image").is_none());
    assert!(payload.get("type").is_none());
  }

  #[tokio::test]
  async fn test_send_failures() {
    for status in [500u16, 999, 204] {
      let (ok, requests) = send_to_mock("kodi://user:pass@", "", &ctx(NotifyType::Info), status).await;
      assert!(!ok, "status {}", status);
      assert_eq!(requests.len(), 1);
    }
    let svc = from_url("kodi://127.0.0.1:1").unwrap();
    assert!(!svc.send(&ctx(NotifyType::Info)).await.unwrap());
  }
}
