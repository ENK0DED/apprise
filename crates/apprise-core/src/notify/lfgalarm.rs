use async_trait::async_trait;
use serde_json::Value;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, OverflowMode, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;

/// LFG Alarm "Anything" alarms: a POST to the alarm's private link rings it
/// on every phone that joined it.
///
/// `lfg://{token}`, `lfg://?token={token}`, or the link itself:
/// `https://api.lfg.gm/horn/{token}`. The title and body are never sent.
pub struct LfgAlarm {
  token: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Tokens are 32 letters and digits
fn valid_token(token: &str) -> bool {
  token.len() == 32 && token.chars().all(|c| c.is_ascii_alphanumeric())
}

impl LfgAlarm {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let token = if url.schema == "https" || url.schema == "http" {
      // Native link: https://api.lfg.gm/horn/{token}[/][?params]
      if url.user.is_some() || url.password.is_some() || url.port.is_some() || !url.host.as_deref().is_some_and(|h| h.eq_ignore_ascii_case("api.lfg.gm")) {
        return None;
      }
      let path = url.path.strip_suffix('/').unwrap_or(&url.path);
      let (horn, token) = path.split_once('/')?;
      if !horn.eq_ignore_ascii_case("horn") || !valid_token(token) {
        return None;
      }
      token.to_string()
    } else {
      // Our token is kept in the host
      url.host.clone().unwrap_or_default()
    };

    // Support ?token= for an easier yaml configuration
    let token = url.get("token").filter(|t| !t.is_empty()).map(String::from).unwrap_or(token);
    if !valid_token(&token) {
      tracing::warn!("An invalid LFG Alarm token was specified.");
      return None;
    }
    Some(Self { token, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "LFG Alarm",
      service_url: Some("https://lfg.gm/"),
      setup_url: Some("https://appriseit.com/services/lfgalarm/"),
      protocols: vec!["lfg"],
      description: "Ring an LFG Alarm Anything alarm on every phone that joined it.",
      attachment_support: false,
    }
  }

  fn status_str(status: u16) -> &'static str {
    match status {
      404 => "No alarm was found at this link.",
      409 => "The alarm is switched off; nothing rang.",
      429 => "The alarm rang moments ago and is cooling down; nothing rang.",
      _ => "",
    }
  }
}

#[async_trait]
impl Notify for LfgAlarm {
  fn schemas(&self) -> &[&str] {
    &["lfg"]
  }
  fn service_name(&self) -> &str {
    "LFG Alarm"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// The body is never sent, so splitting it would only ring the alarm again
  /// during its cooldown and report a failure. Upstream forces this even
  /// over `?overflow=`.
  fn overflow_mode(&self) -> OverflowMode {
    OverflowMode::Upstream
  }

  async fn send(&self, _ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let base = service_base("https://api.lfg.gm");
    tracing::debug!("LFG Alarm POST URL: {}/horn/*** (cert_verify={})", base, self.verify_certificate);

    // LFG Alarm never reads the request body, so none is sent; the alarm
    // shows its own name when it rings
    let client = build_client(self.verify_certificate)?;
    let resp = client.post(format!("{}/horn/{}", base, self.token)).header("User-Agent", APP_ID).send().await?;

    // The reply tells us how many phones rang, or how long to wait before
    // the alarm can ring again
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let response = match serde_json::from_str::<Value>(&text) {
      Ok(Value::Object(map)) => map,
      _ => serde_json::Map::new(),
    };

    if status != 200 {
      let mut status_str = Self::status_str(status).to_string();
      if status == 429
        && let Some(retry_after) = response.get("retryAfterSeconds").and_then(Value::as_i64)
      {
        status_str = format!("{} It can ring again in {}s.", status_str, retry_after);
      }
      tracing::warn!("Failed to ring LFG Alarm: {}{}error={}.", status_str, if status_str.is_empty() { "" } else { ", " }, status);
      return Err(NotifyError::ServiceError { status, body: text });
    }

    // A 200 reply only counts when it confirms the alarm rang
    let fired = response.get("fired").cloned().unwrap_or(Value::Null);
    if fired != Value::Bool(true) {
      tracing::warn!("LFG Alarm did not ring: fired={}.", fired);
      return Ok(false);
    }

    match response.get("devices").and_then(Value::as_i64) {
      Some(devices) => tracing::info!("Rang LFG Alarm on {} phone{}.", devices, if devices == 1 { "" } else { "s" }),
      None => tracing::info!("Rang LFG Alarm."),
    }
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use serde_json::json;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TOKEN: &str = "mk2ayqxeypxpyhbjonesvdsm1vvwkxjb";

  fn rang() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"fired": true, "wokeUp": 1, "devices": 3}))
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      format!("lfg://{}", TOKEN),
      format!("lfg://?token={}", TOKEN),
      format!("https://api.lfg.gm/horn/{}", TOKEN),
      format!("https://api.lfg.gm/horn/{}/?verify=no", TOKEN),
      format!("http://API.LFG.GM/Horn/{}", TOKEN.to_uppercase()),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "lfg://".to_string(),
      "lfg://invalid!token".to_string(),
      format!("lfg://{}", &TOKEN[..31]),
      format!("lfg://{}a", TOKEN),
      // Native links must match exactly
      format!("https://api.lfg.gm/horn/{}/extra", TOKEN),
      format!("https://api.lfg.gm/other/{}", TOKEN),
      format!("https://user@api.lfg.gm/horn/{}", TOKEN),
      format!("https://api.lfg.gm:8443/horn/{}", TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_native_url_token() {
    let parsed = ParsedUrl::parse(&format!("https://api.lfg.gm/horn/{}", TOKEN)).unwrap();
    assert_eq!(LfgAlarm::from_url(&parsed).unwrap().token, TOKEN);
  }

  #[test]
  fn test_overflow_is_always_upstream() {
    let svc = from_url(&format!("lfg://{}?overflow=split", TOKEN)).unwrap();
    assert_eq!(svc.overflow_mode(), OverflowMode::Upstream);
  }

  #[tokio::test]
  async fn test_send_rings_without_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(format!("/horn/{}", TOKEN))).and(header("User-Agent", APP_ID)).respond_with(rang()).expect(1).mount(&server).await;
    let svc = from_url(&format!("https://api.lfg.gm/horn/{}", TOKEN)).unwrap();
    let ctx = NotifyContext { title: "Ignored".into(), body: "Also ignored".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = server.received_requests().await.unwrap();
    let ring = requests.iter().find(|r| r.url.path() == format!("/horn/{}", TOKEN)).unwrap();
    assert!(ring.body.is_empty());
    assert!(ring.headers.get("Content-Type").is_none());
  }

  #[tokio::test]
  async fn test_send_fired_replies() {
    for reply in [json!({"fired": true, "devices": 1}), json!({"fired": true})] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(reply)).expect(1).mount(&server).await;
      let svc = from_url(&format!("lfg://{}", TOKEN)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.unwrap());
    }

    // Anything short of fired=true is a failure
    for reply in [r#"{"fired": null}"#, r#"{"fired": false}"#, r#"{"fired": "cooling_down"}"#, r#"{"fired": 1}"#, "[true]", "not json", ""] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string(reply)).expect(1).mount(&server).await;
      let svc = from_url(&format!("lfg://{}", TOKEN)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.unwrap(), "reply {}", reply);
    }
  }

  #[tokio::test]
  async fn test_send_http_errors() {
    for (code, reply) in
      [(429u16, r#"{"fired": false, "reason": "cooling_down", "retryAfterSeconds": 42}"#), (409, r#"{"fired": false}"#), (404, ""), (500, ""), (999, "")]
    {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code).set_body_string(reply)).expect(1).mount(&server).await;
      let svc = from_url(&format!("lfg://{}", TOKEN)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&NotifyContext::default())).await.is_err(), "code {}", code);
    }
  }

  #[test]
  fn test_service_details() {
    let details = LfgAlarm::static_details();
    assert_eq!(details.service_name, "LFG Alarm");
    assert_eq!(details.protocols, vec!["lfg"]);
  }
}
