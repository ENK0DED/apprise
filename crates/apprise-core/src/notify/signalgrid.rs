use async_trait::async_trait;

use crate::error::NotifyError;
use crate::notify::messaging_targets::{parse_list, unquote, validate_token};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, parse_bool};

/// Signalgrid push notifications (`signalgrid://{client_key}/{channels}`).
pub struct Signalgrid {
  client_key: String,
  /// Channel tokens to notify
  targets: Vec<String>,
  critical: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Signalgrid {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // ?client_key= overrides the hostname-derived value
    let client_key = match url.get("client_key").filter(|k| !k.is_empty()) {
      Some(key) => key.to_string(),
      None => url.host.as_deref().map(unquote).unwrap_or_default(),
    };
    let Some(client_key) = validate_token(&client_key) else {
      tracing::warn!("An invalid Signalgrid Client Key was specified.");
      return None;
    };

    let mut channels = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      channels.extend(parse_list(&[to]));
    }

    // No channels is valid at load time; send() simply has nothing to notify
    Some(Self {
      client_key,
      targets: parse_list(&channels),
      critical: url.get("critical").is_some_and(parse_bool),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Signalgrid",
      service_url: Some("https://signalgrid.co/"),
      setup_url: Some("https://docs.signalgrid.co/integrations/apprise/"),
      protocols: vec!["signalgrid"],
      description: "Send push notifications to Signalgrid channels.",
      attachment_support: false,
    }
  }
}

/// Signalgrid's own name for each notification type.
fn signalgrid_type(notify_type: &NotifyType) -> &'static str {
  match notify_type {
    NotifyType::Info => "INFO",
    NotifyType::Success => "SUCCESS",
    NotifyType::Warning => "WARN",
    NotifyType::Failure => "CRIT",
  }
}

#[async_trait]
impl Notify for Signalgrid {
  fn schemas(&self) -> &[&str] {
    &["signalgrid"]
  }
  fn service_name(&self) -> &str {
    "Signalgrid"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if !ctx.attachments.is_empty() {
      tracing::warn!("Signalgrid does not support attachments; they will be ignored.");
    }

    if self.targets.is_empty() {
      tracing::warn!("There are no Signalgrid channels to notify, aborting.");
      return Ok(false);
    }

    let url = format!("{}/v1/push", service_base("https://api.signalgrid.co"));
    let client = build_client(self.verify_certificate)?;
    let critical = if self.critical { "true" } else { "false" };
    let mut has_error = false;

    for channel in &self.targets {
      let form = [
        ("client_key", self.client_key.as_str()),
        ("channel", channel.as_str()),
        ("title", ctx.title.as_str()),
        ("body", ctx.body.as_str()),
        ("type", signalgrid_type(&ctx.notify_type)),
        ("critical", critical),
      ];

      // Never log the client key or channel token
      tracing::debug!("Signalgrid Payload: title={:?}, type={}, critical={}", ctx.title, form[4].1, critical);

      match client.post(&url).header("User-Agent", APP_ID).form(&form).send().await {
        Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
          tracing::info!("Sent Signalgrid notification.");
        }
        Ok(resp) => {
          tracing::warn!("Failed to send Signalgrid notification: error={}.", resp.status().as_u16());
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Signalgrid notification.");
          tracing::debug!("Socket Exception: {}", e.without_url());
          has_error = true;
        }
      }
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Option<Signalgrid> {
    Signalgrid::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_urls() {
    assert!(from_url("signalgrid://").is_none());
    for url in [
      "signalgrid://CLIENTKEY/CHANNEL",
      "signalgrid://CLIENTKEY/CHANNEL1/CHANNEL2",
      "signalgrid://CLIENTKEY/CHANNEL?critical=yes",
      "signalgrid://CLIENTKEY/CHANNEL?to=CHANNEL2,CHANNEL3",
      // No channels: loads, but has nothing to notify
      "signalgrid://CLIENTKEY",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_parse_fields() {
    let obj = parse("signalgrid://CLIENTKEY/CHANNEL2/CHANNEL1/CHANNEL2?to=CHANNEL3&critical=on").unwrap();
    assert_eq!(obj.client_key, "CLIENTKEY");
    // Channels are de-duplicated and sorted, as upstream's parse_list() does
    assert_eq!(obj.targets, vec!["CHANNEL1", "CHANNEL2", "CHANNEL3"]);
    assert!(obj.critical);

    let obj = parse("signalgrid://IGNOREME/CHANNEL?client_key=REALKEY").unwrap();
    assert_eq!(obj.client_key, "REALKEY");
    assert!(!obj.critical);

    let obj = parse("signalgrid://CLIENTKEY").unwrap();
    assert!(obj.targets.is_empty());
  }

  #[test]
  fn test_attributes() {
    let svc = from_url("signalgrid://CLIENTKEY/CHANNEL").unwrap();
    assert!(!svc.attachment_support());
    assert_eq!(svc.title_maxlen(), 250);
    assert_eq!(svc.body_maxlen(), 32768);
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/push"))
      .and(header("User-Agent", APP_ID))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(body_string("client_key=CLIENTKEY&channel=CHANNEL&title=Server+Down&body=api01+is+unreachable&type=CRIT&critical=true"))
      .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("signalgrid://CLIENTKEY/CHANNEL?critical=yes").unwrap();
    let ok = crate::notify::with_test_base(server.uri(), svc.send(&ctx("Server Down", "api01 is unreachable", NotifyType::Failure))).await.unwrap();
    assert!(ok);
  }

  #[tokio::test]
  async fn test_type_mapping() {
    for (notify_type, expected) in [(NotifyType::Info, "INFO"), (NotifyType::Success, "SUCCESS"), (NotifyType::Warning, "WARN"), (NotifyType::Failure, "CRIT")]
    {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(body_string(format!("client_key=CLIENTKEY&channel=CHANNEL&title=Test&body=Test&type={}&critical=false", expected)))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
      let svc = from_url("signalgrid://CLIENTKEY/CHANNEL").unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Test", "Test", notify_type))).await.unwrap());
    }
  }

  #[tokio::test]
  async fn test_multi_channel_and_partial_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("client_key=CLIENTKEY&channel=CHANNEL1&title=Test&body=Test&type=INFO&critical=false"))
      .respond_with(ResponseTemplate::new(500).set_body_string("error"))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(body_string("client_key=CLIENTKEY&channel=CHANNEL2&title=Test&body=Test&type=INFO&critical=false"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    // One failed channel still notifies the rest, and reports failure
    let svc = from_url("signalgrid://CLIENTKEY/CHANNEL1/CHANNEL2").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("Test", "Test", NotifyType::Info))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    // No channels
    let svc = from_url("signalgrid://CLIENTKEY").unwrap();
    assert!(!svc.send(&ctx("Test", "Test", NotifyType::Info)).await.unwrap());

    for status in [500, 204] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url("signalgrid://CLIENTKEY/CHANNEL").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("T", "B", NotifyType::Info))).await.unwrap(), "status {}", status);
    }

    let svc = from_url("signalgrid://CLIENTKEY/CHANNEL").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("T", "B", NotifyType::Info))).await.unwrap());
  }

  #[tokio::test]
  async fn test_attachments_are_ignored() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("client_key=CLIENTKEY&channel=CHANNEL&title=Test&body=Test&type=INFO&critical=false"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("signalgrid://CLIENTKEY/CHANNEL").unwrap();
    let mut ctx = ctx("Test", "Test", NotifyType::Info);
    ctx.attachments.push(Attachment { name: "a.gif".into(), data: vec![1, 2, 3], mime_type: "image/gif".into() });
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }
}
