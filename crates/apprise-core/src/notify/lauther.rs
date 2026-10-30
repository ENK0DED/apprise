use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;

/// Priority names, longest first: "lowest" and "low" share a prefix, so a
/// value starting with "lowest" must not resolve to "low".
const PRIORITIES: &[(&str, i32)] = &[("emergency", 2), ("lowest", -2), ("normal", 0), ("high", 1), ("low", -1)];

/// Lauther push notifications.
///
/// `lauther://{token}` or `lauther://?token={token}`, with optional
/// `priority`, `sound`, `click`, `icon`, `color`, `group` and `route`.
pub struct Lauther {
  token: String,
  priority: i32,
  sound: Option<String>,
  click: Option<String>,
  icon: Option<String>,
  color: Option<String>,
  group: Option<String>,
  route: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream's priority parsing: a value starting with a priority name maps to
/// it; a signed integer is taken as-is; anything else falls back to normal.
/// `None` for values upstream fails on (out of range, or a malformed number).
fn parse_priority(value: &str) -> Option<i32> {
  let lower = value.to_lowercase();
  let priority = match PRIORITIES.iter().find(|(name, _)| lower.starts_with(name)) {
    Some((_, p)) => *p,
    None => {
      let digits = value.trim_start_matches(['+', '-']);
      if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        // int() refuses more than one sign
        value.parse::<i64>().ok()?.clamp(i32::MIN as i64, i32::MAX as i64) as i32
      } else {
        0
      }
    }
  };
  (-2..=2).contains(&priority).then_some(priority)
}

impl Lauther {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let get = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(String::from);

    // ?token= overrides the host
    let token = get("token").or_else(|| url.host.clone()).unwrap_or_default();
    let valid = token.len() > 4 && token.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("lpt_")) && token[4..].chars().all(|c| c.is_ascii_alphanumeric());
    if !valid {
      tracing::warn!("The Lauther token specified is invalid.");
      return None;
    }

    let priority = match url.get("priority").filter(|p| !p.is_empty()) {
      Some(p) => match parse_priority(p) {
        Some(priority) => priority,
        None => {
          tracing::warn!("The Lauther priority specified ({}) is invalid.", p);
          return None;
        }
      },
      None => 0,
    };

    Some(Self {
      token,
      priority,
      sound: get("sound"),
      click: get("click"),
      icon: get("icon"),
      color: get("color"),
      group: get("group"),
      route: get("route"),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Lauther",
      service_url: Some("https://lauther.app/"),
      setup_url: Some("https://appriseit.com/services/lauther/"),
      protocols: vec!["lauther"],
      description: "Send push notifications to the Lauther app.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Lauther {
  fn schemas(&self) -> &[&str] {
    &["lauther"]
  }
  fn service_name(&self) -> &str {
    "Lauther"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// The maximum body length documented by the Lauther API
  fn body_maxlen(&self) -> usize {
    2000
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let mut payload = json!({
      "title": ctx.title,
      "message": ctx.body,
      "priority": self.priority,
    });
    // "group" and "route" avoid Apprise's own ?tag= and path; Lauther still
    // receives them as "tag" and "path"
    for (key, value) in
      [("sound", &self.sound), ("url", &self.click), ("icon", &self.icon), ("color", &self.color), ("tag", &self.group), ("path", &self.route)]
    {
      if let Some(value) = value {
        payload[key] = json!(value);
      }
    }

    let url = format!("{}/v1/push", service_base("https://api.lauther.id"));
    tracing::debug!("Lauther POST URL: {} (cert_verify={})", url, self.verify_certificate);
    tracing::debug!("Lauther Payload: {}", payload);
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json")
      .header("Authorization", format!("Bearer {}", self.token))
      .body(payload.to_string())
      .send()
      .await?;

    let status = resp.status().as_u16();
    if status == 200 {
      tracing::info!("Sent Lauther notification.");
      Ok(true)
    } else {
      tracing::warn!("Failed to send Lauther notification: error={}.", status);
      Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Lauther {
    Lauther::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "lauther://lpt_abc123",
      "lauther://LPT_ABC123",
      "lauther://?token=lpt_abc123",
      "lauther://lpt_abc123?priority=high",
      "lauther://lpt_abc123?priority=2",
      // An unknown name falls back to normal
      "lauther://lpt_abc123?priority=invalid",
      "lauther://lpt_abc123?sound=default&click=https://example.ca",
      "lauther://lpt_abc123?icon=https://example.ca/i.png&color=%23ff0000&group=g&route=/x",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "lauther://",
      "lauther://:@/",
      "lauther://%badtoken%",
      "lauther://abc123",
      "lauther://lpt_",
      "lauther://lpt_abc123?priority=99",
      "lauther://lpt_abc123?priority=-3",
      "lauther://lpt_abc123?priority=--1",
      "lauther://l%C3%A9t_abc",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_priority() {
    assert_eq!(parse("lauther://lpt_abc123").priority, 0);
    assert_eq!(parse("lauther://lpt_abc123?priority=emergency").priority, 2);
    assert_eq!(parse("lauther://lpt_abc123?priority=-2").priority, -2);
    assert_eq!(parse("lauther://lpt_abc123?priority=%2B1").priority, 1);
    assert_eq!(parse("lauther://lpt_abc123?priority=low").priority, -1);
    assert_eq!(parse("lauther://lpt_abc123?priority=LOWEST").priority, -2);
    assert_eq!(parse("lauther://lpt_abc123?priority=highest").priority, 1);
    assert_eq!(parse("lauther://lpt_abc123?priority=h").priority, 0);
  }

  #[tokio::test]
  async fn test_send_minimal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/push"))
      .and(header("Authorization", "Bearer lpt_abc123"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({"title": "title", "message": "body", "priority": 0})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("lauther://?token=lpt_abc123").unwrap();
    let ctx = NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_all_options() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/push"))
      .and(body_json(json!({
        "title": "",
        "message": "body",
        "priority": 1,
        "sound": "default",
        "url": "https://example.ca",
        "icon": "https://example.ca/i.png",
        "color": "#ff0000",
        "tag": "g",
        "path": "/x",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc =
      from_url("lauther://lpt_abc123?priority=high&sound=default&click=https://example.ca&icon=https://example.ca/i.png&color=%23ff0000&group=g&route=/x")
        .unwrap();
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for code in [500u16, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/v1/push")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url("lauther://lpt_abc123").unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.is_err());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Lauther::static_details();
    assert_eq!(details.service_name, "Lauther");
    assert_eq!(details.protocols, vec!["lauther"]);
    assert_eq!(parse("lauther://lpt_abc123").body_maxlen(), 2000);
  }
}
