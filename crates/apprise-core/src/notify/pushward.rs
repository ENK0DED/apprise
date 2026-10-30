use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, unquote};

/// The PushWard notification (interruption) levels
const LEVELS: &[&str] = &["passive", "active", "time-sensitive", "critical"];

/// Resolve a full or short-form level (e.g. "crit", "c") to a PushWard level
fn parse_level(value: &str) -> Option<&'static str> {
  let value = value.trim().to_lowercase();
  if value.is_empty() {
    return None;
  }
  LEVELS.iter().copied().find(|level| level.starts_with(value.as_str()))
}

/// PushWard hosted push notifications.
///
/// `pushward://{integration_key}` (or `?apikey=`), with an optional `level`
/// forcing every notification, per-type `info`/`success`/`warning`/`failure`
/// levels, and a `volume` (0.0 to 1.0) for critical notifications.
pub struct PushWard {
  apikey: String,
  level: Option<&'static str>,
  /// Levels for info, success, warning and failure notifications
  level_map: [&'static str; 4],
  volume: Option<f64>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl PushWard {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream unquotes the query values once more
    let get = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(unquote);

    // The Integration Key is in the host position; ?apikey= overrides it
    let apikey = get("apikey").or_else(|| url.host.clone()).unwrap_or_default();
    let valid = apikey.len() > 4 && apikey.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("hlk_")) && apikey[4..].chars().all(|c| c.is_ascii_alphanumeric());
    if !valid {
      tracing::warn!("An invalid PushWard Integration Key was specified.");
      return None;
    }

    // An explicit level forces every notification to that level
    let level = match get("level") {
      Some(l) => match parse_level(&l) {
        Some(level) => Some(level),
        None => {
          tracing::warn!("An invalid PushWard level ({}) was specified.", l);
          return None;
        }
      },
      None => None,
    };

    // Per-notification-type levels; "critical" is never a default since it
    // bypasses the device's silent mode / Focus
    let mut level_map = ["active", "active", "time-sensitive", "time-sensitive"];
    for (slot, key) in level_map.iter_mut().zip(["info", "success", "warning", "failure"]) {
      if let Some(value) = get(key) {
        match parse_level(&value) {
          Some(level) => *slot = level,
          None => {
            tracing::warn!("An invalid PushWard level ({}) was specified.", value);
            return None;
          }
        }
      }
    }

    // Volume is only applied to critical notifications
    let volume = match get("volume") {
      Some(v) => match v.trim().parse::<f64>() {
        Ok(volume) if !(volume < 0.0 || volume > 1.0) => Some(volume),
        _ => {
          tracing::warn!("An invalid PushWard volume ({}) was specified.", v);
          return None;
        }
      },
      None => None,
    };

    Some(Self { apikey, level, level_map, volume, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "PushWard",
      service_url: Some("https://pushward.app/"),
      setup_url: Some("https://appriseit.com/services/pushward/"),
      protocols: vec!["pushward"],
      description: "Send push notifications through PushWard.",
      attachment_support: false,
    }
  }

  /// An explicit level wins; otherwise the per-type mapping applies
  fn level_for(&self, notify_type: &NotifyType) -> &'static str {
    self.level.unwrap_or(match notify_type {
      NotifyType::Info => self.level_map[0],
      NotifyType::Success => self.level_map[1],
      NotifyType::Warning => self.level_map[2],
      NotifyType::Failure => self.level_map[3],
    })
  }

  fn status_str(status: u16) -> &'static str {
    match status {
      401 => "Unauthorized - Invalid Integration Key.",
      403 => "Forbidden - The Integration Key lacks permission.",
      422 => "Unprocessable - The notification payload was rejected.",
      429 => "Too many requests; rate-limit exceeded.",
      _ => "",
    }
  }
}

#[async_trait]
impl Notify for PushWard {
  fn schemas(&self) -> &[&str] {
    &["pushward"]
  }
  fn service_name(&self) -> &str {
    "PushWard"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    256
  }
  fn body_maxlen(&self) -> usize {
    3000
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let level = self.level_for(&ctx.notify_type);

    // PushWard requires a non-empty title
    let title = [ctx.title.as_str(), ctx.asset.app_desc.as_str(), ctx.asset.app_id.as_str()].into_iter().find(|t| !t.is_empty()).unwrap_or_default();
    let mut payload = json!({ "title": title, "body": ctx.body, "level": level });

    // Reference a 128x128 icon when the asset defines an image URL mask
    if let Some(icon_url) = ctx.image_url(crate::asset::NotifyImageSize::Xy128) {
      payload["icon_url"] = json!(icon_url);
    }

    if level == "critical"
      && let Some(volume) = self.volume
    {
      payload["volume"] = json!(volume);
    }

    let url = format!("{}/notifications", service_base("https://api.pushward.app"));
    tracing::debug!("PushWard POST URL: {} (cert_verify={})", url, self.verify_certificate);
    tracing::debug!("PushWard Payload: {}", payload);
    let client = build_client(self.verify_certificate)?;
    crate::notify::throttle().await;
    let resp = client
      .post(&url)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json; charset=utf-8")
      .header("Authorization", format!("Bearer {}", self.apikey))
      .body(payload.to_string())
      .send()
      .await?;

    let status = resp.status().as_u16();
    if matches!(status, 200 | 201) {
      tracing::info!("Sent PushWard notification.");
      Ok(true)
    } else {
      let status_str = Self::status_str(status);
      tracing::warn!("Failed to send PushWard notification: {}{}error={}.", status_str, if status_str.is_empty() { "" } else { ", " }, status);
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

  /// What upstream sends with the default asset (its GitHub-hosted theme).
  const ICON_URL: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-128x128.png";
  const KEY: &str = "hlk_apikey1234567890";

  fn parse(url: &str) -> PushWard {
    PushWard::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      format!("pushward://{}", KEY),
      format!("pushward://?apikey={}", KEY),
      "pushward://HLK_ABCdef123?level=critical&volume=0.8".to_string(),
      format!("pushward://{}?level=c", KEY),
      format!("pushward://{}?info=passive&success=a&warning=t&failure=critical", KEY),
      format!("pushward://{}?volume=0", KEY),
      format!("pushward://{}?volume=1", KEY),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "pushward://".to_string(),
      "pushward://:@/".to_string(),
      "pushward://invalid".to_string(),
      "pushward://hlk_".to_string(),
      "pushward://hlk_bad-key".to_string(),
      format!("pushward://{}?level=invalid", KEY),
      format!("pushward://{}?info=bogus", KEY),
      format!("pushward://{}?failure=x", KEY),
      format!("pushward://{}?volume=abc", KEY),
      format!("pushward://{}?volume=1.5", KEY),
      format!("pushward://{}?volume=-0.1", KEY),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_levels() {
    let p = parse(&format!("pushward://{}", KEY));
    assert_eq!(p.level_for(&NotifyType::Info), "active");
    assert_eq!(p.level_for(&NotifyType::Success), "active");
    assert_eq!(p.level_for(&NotifyType::Warning), "time-sensitive");
    assert_eq!(p.level_for(&NotifyType::Failure), "time-sensitive");

    let p = parse(&format!("pushward://{}?info=p&failure=CRIT", KEY));
    assert_eq!(p.level_for(&NotifyType::Info), "passive");
    assert_eq!(p.level_for(&NotifyType::Failure), "critical");

    // An explicit level overrides every type
    let p = parse(&format!("pushward://{}?level=time&info=passive", KEY));
    assert_eq!(p.level_for(&NotifyType::Info), "time-sensitive");

    // The key keeps its case
    assert_eq!(parse("pushward://HLK_ABCdef123").apikey, "HLK_ABCdef123");
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/notifications"))
      .and(header("Authorization", format!("Bearer {}", KEY).as_str()))
      .and(header("Content-Type", "application/json; charset=utf-8"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({"title": "title", "body": "body", "level": "critical", "icon_url": ICON_URL, "volume": 0.5})))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("pushward://{}?level=critical&volume=0.5", KEY)).unwrap();
    let ctx = NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_defaults_and_icon() {
    // Volume only applies to critical notifications; an empty title falls
    // back to the app description; the icon follows the asset's mask
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/notifications"))
      .and(body_json(json!({
        "title": "Apprise Notifications",
        "body": "body",
        "level": "time-sensitive",
        "icon_url": "https://example.com/default/apprise-warning-128x128.png",
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("pushward://{}?volume=0.5", KEY)).unwrap();
    let mut ctx = NotifyContext { body: "body".into(), notify_type: NotifyType::Warning, ..Default::default() };
    ctx.asset.image_url_mask = Some("https://example.com/{THEME}/apprise-{TYPE}-{XY}{EXTENSION}".into());
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_title_falls_back_to_app_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/notifications"))
      .and(body_json(json!({"title": "Apprise", "body": "body only", "level": "active", "icon_url": ICON_URL})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("pushward://{}", KEY)).unwrap();
    let mut ctx = NotifyContext { body: "body only".into(), ..Default::default() };
    ctx.asset.app_desc = String::new();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    for code in [401u16, 403, 422, 429, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/notifications")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url(&format!("pushward://{}", KEY)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.is_err(), "code {}", code);
    }
  }

  #[test]
  fn test_service_details() {
    let details = PushWard::static_details();
    assert_eq!(details.service_name, "PushWard");
    assert_eq!(details.protocols, vec!["pushward"]);
    let svc = parse(&format!("pushward://{}", KEY));
    assert_eq!(svc.title_maxlen(), 256);
    assert_eq!(svc.body_maxlen(), 3000);
  }
}
