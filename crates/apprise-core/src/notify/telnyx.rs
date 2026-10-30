use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::qsd::validate_token;

/// Telnyx SMS (`telnyx://{apikey}@{from_phone}/{targets}`).
pub struct Telnyx {
  apikey: String,
  /// Source phone number (digits only)
  source: String,
  /// Target phone numbers (digits only)
  targets: Vec<String>,
  /// Optional Messaging Profile ID
  profile: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Telnyx {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().unwrap_or_default();

    // Upstream unquotes these query values once more
    let apikey = url.get("key").filter(|k| !k.is_empty()).map(unquote).or_else(|| url.user.clone()).unwrap_or_default();

    // With ?from= the host becomes a target instead of the source
    let (source, mut raw_targets) = match url.get("from").filter(|f| !f.is_empty()) {
      Some(from) => {
        let mut targets = parse_phone_no(&[host.as_str()]);
        targets.extend(url.path_parts.iter().cloned());
        (unquote(from), targets)
      }
      None => (host, url.path_parts.clone()),
    };
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }

    let Some(apikey) = validate_token(&apikey) else {
      tracing::warn!("An invalid Telnyx API Key was specified.");
      return None;
    };

    let Some(source) = is_phone_no(&source, 10).map(|p| p.full) else {
      tracing::warn!("The Account (From) Phone # specified ({}) is invalid.", source);
      return None;
    };

    let profile = url.get("profile").filter(|p| !p.is_empty()).and_then(|p| validate_token(&unquote(p)));

    let mut targets = Vec::new();
    let mut has_error = false;
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10).map(|p| p.full) {
        Some(phone) => targets.push(phone),
        None => {
          has_error = true;
          tracing::warn!("Dropped invalid phone # ({}) specified.", target);
        }
      }
    }

    // Default the SMS message to ourselves
    if raw_targets.is_empty() && !has_error {
      targets.push(source.clone());
    }

    Some(Self { apikey, source, targets, profile, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Telnyx",
      service_url: Some("https://telnyx.com"),
      setup_url: Some("https://appriseit.com/services/telnyx/"),
      protocols: vec!["telnyx"],
      description: "Send SMS messages via Telnyx.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Telnyx {
  fn schemas(&self) -> &[&str] {
    &["telnyx"]
  }
  fn service_name(&self) -> &str {
    "Telnyx"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    160
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Telnyx targets to notify");
      return Ok(false);
    }

    let url = format!("{}/v2/messages", service_base("https://api.telnyx.com"));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;

    for target in &self.targets {
      // A retry skips targets that already accepted the message

      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      let mut payload = json!({
        "from": format!("+{}", self.source),
        "to": format!("+{}", target),
        "text": ctx.body,
      });
      if let Some(profile) = &self.profile {
        payload["messaging_profile_id"] = json!(profile);
      }

      crate::notify::throttle().await;

      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Authorization", format!("Bearer {}", self.apikey))
        .header("Accept", "application/json")
        .json(&payload)
        .send()
        .await;

      match result {
        Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
          tracing::info!("Sent Telnyx notification to {}.", target);
          crate::delivery::mark_delivered(target, false);
        }
        Ok(resp) => {
          tracing::warn!("Failed to send Telnyx notification to {}: error={}.", target, resp.status().as_u16());
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Telnyx: to {}", target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
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
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, body_partial_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Option<Telnyx> {
    Telnyx::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "telnyx://",
      "telnyx://:@/",
      // Invalid source number
      &format!("telnyx://{}:{}@{}", "u".repeat(10), "p".repeat(10), "3".repeat(5)),
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("telnyx://{}@{}/{}", "p".repeat(10), "1".repeat(10), "55"),
      format!("telnyx://{}@{}", "p".repeat(10), "2".repeat(10)),
      format!("telnyx://{}@9876543210/{}/abcd/", "b".repeat(10), "3".repeat(11)),
      format!("telnyx://{}@{}", "c".repeat(10), "4".repeat(11)),
      format!("telnyx://?key={}&from={}", "y".repeat(10), "5".repeat(11)),
      format!("telnyx://?key={}&from={}&to={}", "b".repeat(10), "5".repeat(11), "7".repeat(13)),
      format!("telnyx://{}@{}?profile=abcd-1234", "d".repeat(10), "5".repeat(11)),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_targets() {
    // No targets: we text ourselves
    let obj = parse(&format!("telnyx://{}@{}", "p".repeat(10), "2".repeat(10))).unwrap();
    assert_eq!(obj.targets, vec!["2".repeat(10)]);

    // Only an invalid target: nothing to notify
    let obj = parse(&format!("telnyx://{}@{}/55", "p".repeat(10), "1".repeat(10))).unwrap();
    assert!(obj.targets.is_empty());

    // Short numbers and garbage are dropped
    let obj = parse(&format!("telnyx://{}@9876543210/{}/123/abcd/", "b".repeat(10), "3".repeat(11))).unwrap();
    assert_eq!(obj.targets, vec!["3".repeat(11)]);

    // Upstream's edge case URL: formatted numbers, "12" and "@12" dropped
    let obj = parse("telnyx://KEY0123456789ABCDEF_abcdefghij@1 (405) 123 1234/+1(555) 123-1234/1555 5555555/12/@12").unwrap();
    assert_eq!(obj.apikey, "KEY0123456789ABCDEF_abcdefghij");
    assert_eq!(obj.source, "14051231234");
    assert_eq!(obj.targets, vec!["15551231234", "15555555555"]);

    // ?from= makes the host a target; ?key= overrides the user
    let obj = parse("telnyx://old-key@15557654321?from=15551234567&key=new-key&to=15559876543").unwrap();
    assert_eq!(obj.apikey, "new-key");
    assert_eq!(obj.source, "15551234567");
    assert_eq!(obj.targets, vec!["15557654321", "15559876543"]);

    let obj = parse(&format!("telnyx://{}@{}?profile=abcd-1234", "d".repeat(10), "5".repeat(11))).unwrap();
    assert_eq!(obj.profile.as_deref(), Some("abcd-1234"));
  }

  #[test]
  fn test_attributes() {
    let svc = from_url(&format!("telnyx://{}@{}", "p".repeat(10), "2".repeat(10))).unwrap();
    assert_eq!(svc.body_maxlen(), 160);
    assert_eq!(svc.title_maxlen(), 0);
    assert!(!svc.attachment_support());
    assert_eq!(Telnyx::static_details().protocols, vec!["telnyx"]);
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    for to in ["+15551231234", "+15555555555"] {
      let expected = json!({"from": "+14051231234", "to": to, "text": "title\r\nbody", "messaging_profile_id": "40017-abcd"});
      Mock::given(method("POST"))
        .and(path("/v2/messages"))
        .and(header("Authorization", "Bearer KEY0123456789ABCDEF_abcdefghij"))
        .and(header("Accept", "application/json"))
        .and(header("Content-Type", "application/json"))
        .and(header("User-Agent", APP_ID))
        .and(body_json(expected))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }

    let svc = from_url("telnyx://KEY0123456789ABCDEF_abcdefghij@1 (405) 123 1234/+1(555) 123-1234/1555 5555555/12/@12?profile=40017-abcd").unwrap();
    let ok = crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap();
    assert!(ok);
  }

  #[tokio::test]
  async fn test_send_without_profile() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v2/messages"))
      .and(body_json(json!({"from": "+15551234567", "to": "+15551234567", "text": "Body"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("telnyx://api-key@15551234567").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    // No valid targets
    let svc = from_url(&format!("telnyx://{}@{}/55", "p".repeat(10), "1".repeat(10))).unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());

    // Server errors and non-200 success codes fail
    for status in [500, 999, 201] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/v2/messages")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("telnyx://{}@{}", "b".repeat(10), "5".repeat(11))).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap(), "status {}", status);
    }

    // A failure for one target still notifies the rest
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v2/messages"))
      .and(body_json(json!({"from": "+15551234567", "to": "+15557654321", "text": "body"})))
      .respond_with(ResponseTemplate::new(500))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/v2/messages"))
      .and(body_json(json!({"from": "+15551234567", "to": "+15559876543", "text": "body"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("telnyx://key@15551234567/15557654321/15559876543").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());

    // Connection errors are reported as a failed send
    let svc = from_url("telnyx://key@15551234567").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second target fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/v2/messages"))
      .and(body_partial_json(json!({"to": "+15551232222"})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v2/messages")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("telnyx://KEY0123456789ABCDEF_abcdefghij@14051231234/15551231111/15551232222?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("b"))).await.is_success());
    let sent: Vec<serde_json::Value> = crate::notify::custom_json::requests_to(&server, "POST", "/v2/messages")
      .await
      .iter()
      .map(|r| r.body_json::<serde_json::Value>().unwrap()["to"].clone())
      .collect();
    assert_eq!(sent, vec![json!("+15551231111"), json!("+15551232222"), json!("+15551232222")]);
  }
}
