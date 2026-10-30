use async_trait::async_trait;
use serde_json::Value;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::qsd::validate_token;

/// The kinds of message Telesign accepts: alerts, one-time passwords and
/// marketing.
const TELESIGN_MESSAGE_TYPES: &[&str] = &["ARN", "OTP", "MKT"];

/// Telesign SMS (`telesign://{customer_id}:{apikey}@{targets}`).
pub struct Telesign {
  customer_id: String,
  apikey: String,
  /// Optional Sender ID
  source: Option<String>,
  message_type: &'static str,
  /// Target phone numbers (digits only)
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Telesign {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let Some(customer_id) = url.user.as_deref().and_then(validate_token) else {
      tracing::warn!("A Telesign Customer ID must be specified.");
      return None;
    };

    // The API Key may be given as ?key= instead
    // Upstream unquotes these query values once more
    let apikey = url.get("key").filter(|k| !k.is_empty()).map(unquote).or_else(|| url.password.clone());
    let Some(apikey) = apikey.as_deref().and_then(validate_token) else {
      tracing::warn!("A Telesign API Key must be specified.");
      return None;
    };

    // Alerts are the default message type
    let message_type = match url.get("type").filter(|t| !t.is_empty()).map(unquote) {
      Some(requested) => {
        let wanted = requested.trim().to_uppercase();
        let Some(found) = TELESIGN_MESSAGE_TYPES.iter().find(|t| **t == wanted) else {
          tracing::warn!("The Telesign message type specified ({}) is invalid.", requested);
          return None;
        };
        *found
      }
      None => TELESIGN_MESSAGE_TYPES[0],
    };

    let source = url.get("from").map(|f| unquote(f).trim().to_string()).filter(|f| !f.is_empty());

    // Our first phone number is in the host; the rest are in the path
    let mut raw_targets: Vec<String> = url.host.clone().into_iter().collect();
    raw_targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }

    let mut targets = Vec::new();
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10) {
        Some(phone) => targets.push(phone.full),
        None => tracing::warn!("Dropped invalid Telesign phone # ({}) specified.", target),
      }
    }

    Some(Self { customer_id, apikey, source, message_type, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Telesign",
      service_url: Some("https://www.telesign.com/"),
      setup_url: Some("https://appriseit.com/services/telesign/"),
      protocols: vec!["telesign"],
      description: "Send SMS messages via Telesign.",
      attachment_support: false,
    }
  }
}

/// Upstream's extra descriptions for common Telesign HTTP errors.
fn http_error_description(status: u16) -> Option<&'static str> {
  match status {
    400 => Some("Bad Request - Check the phone number and message."),
    401 => Some("Unauthorized - Invalid Customer ID or API Key."),
    429 => Some("Too many requests were made."),
    500 => Some("Telesign is unavailable, try again later."),
    _ => None,
  }
}

#[async_trait]
impl Notify for Telesign {
  fn schemas(&self) -> &[&str] {
    &["telesign"]
  }
  fn service_name(&self) -> &str {
    "Telesign"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    1600
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Telesign targets to notify.");
      return Ok(false);
    }

    let url = format!("{}/v1/messaging", service_base("https://rest-ww.telesign.com"));
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;

    for target in &self.targets {
      // A retry skips targets that already accepted the message

      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      let mut form = vec![("phone_number", target.as_str()), ("message", ctx.body.as_str()), ("message_type", self.message_type)];
      // Only name a sender when one was given
      if let Some(source) = &self.source {
        form.push(("sender_id", source.as_str()));
      }

      crate::notify::throttle().await;

      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .basic_auth(&self.customer_id, Some(&self.apikey))
        .form(&form)
        .send()
        .await;

      match result {
        Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
          tracing::info!("Sent Telesign notification to {}.", target);
          crate::delivery::mark_delivered(target, false);
        }
        Ok(resp) => {
          let status = resp.status().as_u16();
          let content: Value = resp.json().await.unwrap_or(Value::Null);
          // Prefer Telesign's own explanation when it gave one
          let reason = content.get("status").and_then(|s| s.get("description")).and_then(Value::as_str).or_else(|| http_error_description(status));
          match reason {
            Some(reason) => tracing::warn!("Failed to send Telesign notification to {}: {}, error={}.", target, reason, status),
            None => tracing::warn!("Failed to send Telesign notification to {}: error={}.", target, status),
          }
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Telesign notification to {}.", target);
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
  use serde_json::json;
  use wiremock::matchers::{body_string, body_string_contains, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const CUSTOMER_ID: &str = "FFFFFFFF-EEEE-DDDD-1234-AB1234567890";
  const APIKEY: &str = "EXAMPLE/abcdefghijklmnopqrstuvwxyz+0123456789==";
  // The same API Key, written so it fits in a URL
  const APIKEY_URL: &str = "EXAMPLE%2Fabcdefghijklmnopqrstuvwxyz%2B0123456789%3D%3D";

  fn parse(url: &str) -> Option<Telesign> {
    Telesign::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn basic_auth() -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", CUSTOMER_ID, APIKEY)))
  }

  fn ok_response() -> ResponseTemplate {
    ResponseTemplate::new(200)
      .set_body_json(json!({"reference_id": "0123456789ABCDEF0123456789ABCDEF", "status": {"code": 290, "description": "Message in progress"}}))
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "telesign://".to_string(),
      // No Customer ID or API Key
      "telesign://15551234567".to_string(),
      // No API Key
      format!("telesign://{}@15551234567", CUSTOMER_ID),
      // An unknown message type
      format!("telesign://{}:{}@15551234567?type=invalid", CUSTOMER_ID, APIKEY_URL),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    // No phone numbers: loads, but has nothing to notify
    let obj = parse(&format!("telesign://{}:{}@", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert!(obj.targets.is_empty());

    let obj = parse(&format!("telesign://{}:{}@15551234567", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert_eq!(obj.customer_id, CUSTOMER_ID);
    assert_eq!(obj.apikey, APIKEY);
    assert_eq!(obj.message_type, "ARN");
    assert!(obj.source.is_none());
    assert_eq!(obj.targets, vec!["15551234567"]);

    let obj = parse(&format!("telesign://{}:{}@15551234567/15551234568?from=Alerts&type=mkt", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert_eq!(obj.message_type, "MKT");
    assert_eq!(obj.source.as_deref(), Some("Alerts"));
    assert_eq!(obj.targets, vec!["15551234567", "15551234568"]);

    // The type and sender are cleaned up
    let obj = parse(&format!("telesign://{}:{}@15551234567?type=%20otp%20&from=%20Alerts%20", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert_eq!(obj.message_type, "OTP");
    assert_eq!(obj.source.as_deref(), Some("Alerts"));

    // API Key and phone numbers given as arguments
    let obj = parse(&format!("telesign://{}@?key={}&to=15551234567,15551234568", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert_eq!(obj.apikey, APIKEY);
    assert_eq!(obj.targets, vec!["15551234567", "15551234568"]);

    // One bad phone number is dropped; the good one still works
    let obj = parse(&format!("telesign://{}:{}@15551234567/123", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert_eq!(obj.targets, vec!["15551234567"]);
    assert!(parse(&format!("telesign://{}:{}@123", CUSTOMER_ID, APIKEY_URL)).unwrap().targets.is_empty());

    // Formatted numbers are normalised
    let obj = parse(&format!("telesign://{}:{}@?to=%2B1%20555%20123%204567", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert_eq!(obj.targets, vec!["15551234567"]);
  }

  #[test]
  fn test_attributes() {
    let svc = from_url(&format!("telesign://{}:{}@15551234567", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert_eq!(svc.body_maxlen(), 1600);
    assert_eq!(svc.title_maxlen(), 0);
    assert!(!svc.attachment_support());
  }

  #[tokio::test]
  async fn test_send_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/messaging"))
      .and(header("User-Agent", APP_ID))
      .and(header("Accept", "application/json"))
      .and(header("Authorization", basic_auth().as_str()))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(body_string("phone_number=15551234567&message=Title+%26+%3Cb%3E%0D%0ABody+_+.+%21+*+%23+%3C&message_type=ARN&sender_id=Alerts"))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("telesign://{}:{}@15551234567?from=Alerts", CUSTOMER_ID, APIKEY_URL)).unwrap();
    let ok = crate::notify::with_test_base(server.uri(), svc.send(&ctx("Title & <b>\r\nBody _ . ! * # <"))).await.unwrap();
    assert!(ok);
  }

  #[tokio::test]
  async fn test_send_without_sender() {
    let server = MockServer::start().await;
    for phone in ["15551234567", "15551234568"] {
      Mock::given(method("POST"))
        .and(path("/v1/messaging"))
        .and(body_string(format!("phone_number={}&message=123456&message_type=OTP", phone)))
        .respond_with(ok_response())
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("telesign://{}:{}@15551234567/15551234568?type=otp", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("123456"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    // No phone numbers
    let svc = from_url(&format!("telesign://{}:{}@123", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert!(!svc.send(&ctx("test")).await.unwrap());

    for response in [
      // Telesign explains the error
      ResponseTemplate::new(400).set_body_json(json!({"status": {"code": 11000, "description": "Invalid value for phone_number."}})),
      // A known HTTP error with no usable body
      ResponseTemplate::new(401).set_body_string("not json"),
      // A JSON body that is not an object
      ResponseTemplate::new(429).set_body_string("[]"),
      // A status that is not an object
      ResponseTemplate::new(500).set_body_string(r#"{"status": "bad"}"#),
      ResponseTemplate::new(403),
      ResponseTemplate::new(999),
      // Only 200 is a success
      ResponseTemplate::new(202),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/v1/messaging")).respond_with(response).expect(1).mount(&server).await;
      let svc = from_url(&format!("telesign://{}:{}@15551234567", CUSTOMER_ID, APIKEY_URL)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("test"))).await.unwrap());
    }

    // One refused phone number fails the send but not the others
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/messaging"))
      .and(body_string("phone_number=15551234567&message=test&message_type=ARN"))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/v1/messaging"))
      .and(body_string("phone_number=15551234568&message=test&message_type=ARN"))
      .respond_with(ResponseTemplate::new(400).set_body_string("{}"))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("telesign://{}:{}@15551234567/15551234568", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("test"))).await.unwrap());

    let svc = from_url(&format!("telesign://{}:{}@15551234567", CUSTOMER_ID, APIKEY_URL)).unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("test"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second target fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/v1/messaging"))
      .and(body_string_contains("phone_number=15551232222"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1/messaging")).respond_with(ok_response()).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("telesign://{}:{}@15551231111/15551232222?retry=1&wait=0", CUSTOMER_ID, APIKEY_URL)));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("b"))).await.is_success());
    let sent: Vec<String> = crate::notify::custom_json::requests_to(&server, "POST", "/v1/messaging")
      .await
      .iter()
      .map(|r| url::form_urlencoded::parse(&r.body).find(|(k, _)| k == "phone_number").unwrap().1.into_owned())
      .collect();
    assert_eq!(sent, vec!["15551231111", "15551232222", "15551232222"]);
  }
}
