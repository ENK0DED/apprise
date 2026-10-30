use async_trait::async_trait;
use serde_json::json;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::email::is_email;
use crate::utils::parse::{ParsedUrl, parse_bool, unquote};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::qsd::validate_token;

const OCTOPUSH_PREMIUM: &str = "sms_premium";
const OCTOPUSH_LOW_COST: &str = "sms_low_cost";

/// Prefixes that select a message type, checked in this order.
const OCTOPUSH_TYPE_MAP: &[(&str, &str)] = &[
  ("p", OCTOPUSH_PREMIUM),
  ("sms_p", OCTOPUSH_PREMIUM),
  ("smsp", OCTOPUSH_PREMIUM),
  ("+", OCTOPUSH_PREMIUM),
  ("l", OCTOPUSH_LOW_COST),
  ("sms_l", OCTOPUSH_LOW_COST),
  ("smsl", OCTOPUSH_LOW_COST),
  ("-", OCTOPUSH_LOW_COST),
];

const OCTOPUSH_PURPOSES: &[&str] = &["alert", "wholesale"];

/// The most phone numbers a single batch request may carry.
const OCTOPUSH_BATCH_SIZE: usize = 500;

/// Octopush SMS (`octopush://[{sender}:]{api_login}/{api_key}/{targets}`).
pub struct Octopush {
  /// API Login (an email address)
  api_login: String,
  api_key: String,
  /// Optional sender; the app id is used when unset
  sender: Option<String>,
  mtype: &'static str,
  purpose: String,
  batch: bool,
  replies: bool,
  /// Target phone numbers in E.164 format
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Octopush {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().unwrap_or_default();
    let mut tokens = url.path_parts.clone();
    // Upstream unquotes these values once more
    let qsd = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(unquote);

    let api_key = match qsd("key") {
      Some(key) => Some(key),
      None if !tokens.is_empty() => Some(unquote(&tokens.remove(0))),
      None => None,
    };

    // The remaining elements are the phone numbers we want to contact
    let mut raw_targets = tokens;
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }

    let user = url.user.as_deref().filter(|u| !u.is_empty());
    let password = url.password.as_deref().filter(|p| !p.is_empty());

    // With a user and password, the password is the login's local part and
    // the user names the sender
    let api_login = match qsd("login") {
      Some(login) => Some(login),
      None => password.or(user).map(|local| format!("{}@{}", local, host)),
    };
    let sender = match qsd("sender") {
      Some(sender) => Some(sender),
      None => user.filter(|_| password.is_some()).map(str::to_string),
    };

    let Some(api_login) = api_login.as_deref().and_then(validate_token).filter(|l| is_email(l).is_some()) else {
      tracing::warn!("An invalid Octopush API Login was specified.");
      return None;
    };

    let Some(api_key) = api_key.as_deref().and_then(validate_token) else {
      tracing::warn!("An invalid Octopush API Key was specified.");
      return None;
    };

    let mtype = match qsd("type") {
      None => OCTOPUSH_PREMIUM,
      Some(requested) => {
        let wanted = requested.to_lowercase().trim().to_string();
        match OCTOPUSH_TYPE_MAP.iter().find(|(prefix, _)| !wanted.is_empty() && wanted.starts_with(prefix)) {
          Some((_, mtype)) => mtype,
          None => {
            tracing::warn!("The Octopush type specified ({}) is invalid.", wanted);
            return None;
          }
        }
      }
    };

    let purpose = match qsd("purpose") {
      None => OCTOPUSH_PURPOSES[0].to_string(),
      Some(requested) => match validate_token(&requested).map(|p| p.to_lowercase()) {
        Some(purpose) if OCTOPUSH_PURPOSES.contains(&purpose.as_str()) => purpose,
        _ => {
          tracing::warn!("The Octopush purpose specified ({}) is invalid.", requested);
          return None;
        }
      },
    };

    let sender = match sender {
      Some(sender) => match validate_token(&sender) {
        Some(sender) => Some(sender),
        None => {
          tracing::warn!("An invalid Octopush sender ({}) was specified.", sender);
          return None;
        }
      },
      None => None,
    };

    let mut targets = Vec::new();
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10) {
        Some(phone) => targets.push(format!("+{}", phone.full)),
        None => tracing::warn!("Dropped invalid phone ({}) specified.", target),
      }
    }

    Some(Self {
      api_login,
      api_key,
      sender,
      mtype,
      purpose,
      batch: url.get("batch").is_some_and(parse_bool),
      replies: url.get("replies").is_some_and(parse_bool),
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Octopush",
      service_url: Some("https://octopush.com/"),
      setup_url: Some("https://appriseit.com/services/octopush/"),
      protocols: vec!["octopush"],
      description: "Send SMS messages via Octopush.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Octopush {
  fn schemas(&self) -> &[&str] {
    &["octopush"]
  }
  fn service_name(&self) -> &str {
    "Octopush"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    1224
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("No Octopush targets to notify.");
      return Ok(false);
    }

    let url = format!("{}/v1/public/sms-campaign/send", service_base("https://api.octopush.com"));
    let client = build_client(self.verify_certificate)?;
    let batch_size = if self.batch { OCTOPUSH_BATCH_SIZE } else { 1 };
    let sender = self.sender.as_deref().unwrap_or(ctx.asset.app_id.as_str());
    let mut has_error = false;

    for (index, chunk) in self.targets.chunks(batch_size).enumerate() {
      // A retry skips batches that already accepted the message
      let key = index.to_string();
      if crate::delivery::is_delivered(&key, false) {
        continue;
      }
      let recipients: Vec<_> = chunk.iter().map(|phone| json!({ "phone_number": phone })).collect();
      let payload = json!({
        "recipients": recipients,
        "text": ctx.body,
        "type": self.mtype,
        "purpose": self.purpose,
        "sender": sender,
        "with_replies": self.replies,
      });
      let verbose_dest = if chunk.len() <= 3 { chunk.join(", ") } else { format!("{} recipients", chunk.len()) };

      crate::notify::throttle().await;
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json; charset=utf-8")
        .header("api-key", &self.api_key)
        .header("api-login", &self.api_login)
        .header("cache-control", "no-cache")
        .body(payload.to_string())
        .send()
        .await;

      match result {
        Ok(resp) if resp.status() == reqwest::StatusCode::CREATED => {
          tracing::info!("Sent Octopush notification to {}.", verbose_dest);
          crate::delivery::mark_delivered(&key, false);
        }
        Ok(resp) => {
          tracing::warn!("Failed to send Octopush notification to {}: error={}.", verbose_dest, resp.status().as_u16());
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Octopush:{} notification.", verbose_dest);
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

  fn parse(url: &str) -> Option<Octopush> {
    Octopush::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "octopush://",
      "octopush://:@/",
      // Valid API Login, but no API Key
      "octopush://user@myaccount.com",
      // Invalid login
      "octopush://_/apikey?login=invalid",
      // Invalid API Key
      "octopush://user@myaccount.com/%20",
      // Invalid sender
      "octopush://%20:user@myaccount.com/apikey",
      // Invalid purpose
      "octopush://?login=user@myaccount.com&key=key&to=9999999999&purpose=invalid",
      // Invalid type
      "octopush://?login=user@myaccount.com&key=key&to=9999999999&type=invalid",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      // No targets: loads, but has nothing to notify
      "octopush://user@myaccount.com/apikey",
      "octopush://user@myaccount.com/apikey/+0987654321",
      "octopush://sender:user@myaccount.com/apikey/+1111111111",
      "octopush://?login=user@myaccount.com&key=key&to=9999999999&purpose=wholesale",
      "octopush://?login=user@myaccount.com&key=key&to=9999999999&type=premium",
      "octopush://user@myaccount.com/apikey/+3333333333?replies=yes",
      "octopush://sender:user@myaccount.com/apikey/1111111111/222/33333333333/?batch=yes",
      "octopush://_?key=abc123&login=user@myaccount.com&sender=abc&to=2222222222",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_parse_fields() {
    let obj = parse("octopush://sender:user@myaccount.com/apikey/1234567890?to=2345678901&type=sms_premium&purpose=alert&batch=yes&replies=no").unwrap();
    assert_eq!(obj.api_key, "apikey");
    assert_eq!(obj.api_login, "user@myaccount.com");
    assert_eq!(obj.sender.as_deref(), Some("sender"));
    assert_eq!(obj.targets, vec!["+1234567890", "+2345678901"]);
    assert_eq!(obj.mtype, OCTOPUSH_PREMIUM);
    assert_eq!(obj.purpose, "alert");
    assert!(obj.batch);
    assert!(!obj.replies);

    let obj =
      parse("octopush://_?login=user@myaccount.com&key=apikey&sender=abc&to=3456789012&type=sms_low_cost&purpose=WHOLESALE&batch=no&replies=yes").unwrap();
    assert_eq!(obj.api_key, "apikey");
    assert_eq!(obj.api_login, "user@myaccount.com");
    assert_eq!(obj.sender.as_deref(), Some("abc"));
    assert_eq!(obj.targets, vec!["+3456789012"]);
    assert_eq!(obj.mtype, OCTOPUSH_LOW_COST);
    assert_eq!(obj.purpose, "wholesale");
    assert!(!obj.batch);
    assert!(obj.replies);

    // A user alone is the login's local part; no sender
    let obj = parse("octopush://user@myaccount.com/apikey/1111111111/222/33333333333").unwrap();
    assert!(obj.sender.is_none());
    assert_eq!(obj.targets, vec!["+1111111111", "+33333333333"]);

    // Type prefixes
    for (requested, expected) in
      [("premium", OCTOPUSH_PREMIUM), ("low_cost", OCTOPUSH_LOW_COST), ("SMSL", OCTOPUSH_LOW_COST), ("-", OCTOPUSH_LOW_COST), ("%2B", OCTOPUSH_PREMIUM)]
    {
      let obj = parse(&format!("octopush://user@myaccount.com/apikey?type={}", requested)).unwrap();
      assert_eq!(obj.mtype, expected, "type={}", requested);
    }
  }

  #[test]
  fn test_attributes() {
    let svc = from_url("octopush://user@myaccount.com/apikey").unwrap();
    assert_eq!(svc.body_maxlen(), 1224);
    assert_eq!(svc.title_maxlen(), 0);
    assert!(!svc.attachment_support());
  }

  #[tokio::test]
  async fn test_send_batch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/public/sms-campaign/send"))
      .and(header("User-Agent", APP_ID))
      .and(header("Accept", "application/json"))
      .and(header("Content-Type", "application/json; charset=utf-8"))
      .and(header("api-key", "apikey"))
      .and(header("api-login", "user@myaccount.com"))
      .and(header("cache-control", "no-cache"))
      .and(body_json(json!({
        "recipients": [{"phone_number": "+1234567890"}, {"phone_number": "+2345678901"}],
        "text": "title\r\nbody",
        "type": "sms_low_cost",
        "purpose": "wholesale",
        "sender": "sender",
        "with_replies": true,
      })))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("octopush://sender:user@myaccount.com/apikey/1234567890/2345678901?batch=yes&replies=yes&type=low_cost&purpose=wholesale").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_one_per_target_with_default_sender() {
    let server = MockServer::start().await;
    for phone in ["+15551234567", "+15559876543"] {
      Mock::given(method("POST"))
        .and(path("/v1/public/sms-campaign/send"))
        .and(body_json(json!({
          "recipients": [{"phone_number": phone}],
          "text": "body",
          "type": "sms_premium",
          "purpose": "alert",
          "sender": "Apprise",
          "with_replies": false,
        })))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    }

    let svc = from_url("octopush://user@example.com/apikey123/15551234567/15559876543").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_splits_large_batches() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/public/sms-campaign/send")).respond_with(ResponseTemplate::new(201)).expect(2).mount(&server).await;

    let targets: Vec<String> = (0..501).map(|i| (1_000_000_000u64 + i).to_string()).collect();
    let svc = from_url(&format!("octopush://user@myaccount.com/apikey/{}?batch=yes", targets.join("/"))).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());

    let requests = crate::notify::custom_json::requests_to(&server, "POST", "/v1/public/sms-campaign/send").await;
    let counts: Vec<usize> = requests.iter().map(|r| r.body_json::<serde_json::Value>().unwrap()["recipients"].as_array().unwrap().len()).collect();
    assert_eq!(counts, vec![500, 1]);
  }

  #[tokio::test]
  async fn test_send_failures() {
    // No targets
    let svc = from_url("octopush://user@myaccount.com/apikey").unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());

    // Only 201 Created is a success
    for status in [200, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/v1/public/sms-campaign/send")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url("octopush://user@myaccount.com/apikey/1234567890").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap(), "status {}", status);
    }

    // One failed target still notifies the rest
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/public/sms-campaign/send"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1/public/sms-campaign/send")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let svc = from_url("octopush://user@myaccount.com/apikey/1234567890/2345678901").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());

    let svc = from_url("octopush://user@myaccount.com/apikey/1234567890").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("body"))).await.unwrap());
  }

  #[test]
  fn test_blank_option_value_loads() {
    // Upstream trims query values, so a whitespace-only value counts as unset.
    for url in ["octopush://?login=user@myaccount.com&key=key&to=9999999999&purpose=%20", "octopush://?login=user@myaccount.com&key=key&to=9999999999&type=%20"]
    {
      assert!(crate::notify::registry::from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second number fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/v1/public/sms-campaign/send"))
      .and(body_partial_json(json!({"recipients": [{"phone_number": "+2345678901"}]})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1/public/sms-campaign/send")).respond_with(ResponseTemplate::new(201)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("octopush://user@myaccount.com/apikey/1234567890/2345678901?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("body"))).await.is_success());
    let sent: Vec<serde_json::Value> = crate::notify::custom_json::requests_to(&server, "POST", "/v1/public/sms-campaign/send")
      .await
      .iter()
      .map(|r| r.body_json::<serde_json::Value>().unwrap()["recipients"][0]["phone_number"].clone())
      .collect();
    assert_eq!(sent, vec![json!("+1234567890"), json!("+2345678901"), json!("+2345678901")]);
  }
}
