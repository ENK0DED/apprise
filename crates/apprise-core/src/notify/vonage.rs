use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use async_trait::async_trait;

const DEFAULT_TTL: i64 = 900_000;
const MIN_TTL: i64 = 20_000;
const MAX_TTL: i64 = 604_800_000;

pub struct Vonage {
  apikey: String,
  secret: String,
  source: String,
  targets: Vec<String>,
  /// Message lifetime in milliseconds
  ttl: i64,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// `urllib.parse.urlencode()` as requests applies it to `data=`.
fn form_encode(pairs: &[(&str, &str)]) -> String {
  pairs
    .iter()
    .map(|(k, v)| format!("{}={}", urlencoding::encode(k).replace("%20", "+"), urlencoding::encode(v).replace("%20", "+")))
    .collect::<Vec<_>>()
    .join("&")
}

fn is_alnum(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric())
}

impl Vonage {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // vonage://apikey:secret@source/targets
    // Upstream unquotes these query values once more
    let qs = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(unquote);
    let apikey = qs("key").or_else(|| url.user.clone()).unwrap_or_default();
    let secret = qs("secret").or_else(|| url.password.clone()).unwrap_or_default();
    let source = qs("source").or_else(|| qs("from")).or_else(|| url.host.clone()).unwrap_or_default();

    if !is_alnum(&apikey) {
      tracing::warn!("An invalid Vonage API Key ({}) was specified.", apikey);
      return None;
    }
    if !is_alnum(&secret) {
      tracing::warn!("An invalid Vonage API Secret was specified.");
      return None;
    }
    // A ttl that is not a number falls back to the default
    let ttl = qs("ttl").and_then(|t| t.parse().ok()).unwrap_or(DEFAULT_TTL);
    if !(MIN_TTL..=MAX_TTL).contains(&ttl) {
      tracing::warn!("The Vonage TTL specified ({}) is out of range.", ttl);
      return None;
    }
    let Some(phone) = is_phone_no(&source, 10) else {
      tracing::warn!("The Account (From) Phone # specified ({}) is invalid.", source);
      return None;
    };
    let source = phone.full;

    let mut entries = url.path_parts.clone();
    if let Some(to) = qs("to") {
      entries.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    for target in parse_phone_no(&entries) {
      match is_phone_no(&target, 10) {
        Some(phone) => targets.push(phone.full),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self { apikey, secret, source, targets, ttl, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Vonage",
      service_url: Some("https://dashboard.nexmo.com/"),
      setup_url: Some("https://appriseit.com/services/vonage/"),
      protocols: vec!["vonage", "nexmo"],
      description: "Send SMS via Vonage (formerly Nexmo).",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Vonage {
  fn schemas(&self) -> &[&str] {
    &["vonage", "nexmo"]
  }
  fn service_name(&self) -> &str {
    "Vonage"
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
    // Without targets, notify ourselves
    let targets = if self.targets.is_empty() { std::slice::from_ref(&self.source) } else { self.targets.as_slice() };
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/sms/json", service_base("https://rest.nexmo.com"));
    let ttl = self.ttl.to_string();
    let mut has_error = false;

    for target in targets {
      // A retry skips numbers that already accepted the message
      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      let body = form_encode(&[
        ("api_key", self.apikey.as_str()),
        ("api_secret", self.secret.as_str()),
        ("ttl", ttl.as_str()),
        ("from", self.source.as_str()),
        ("text", ctx.body.as_str()),
        ("to", target.as_str()),
      ]);
      tracing::debug!("Vonage POST URL: {} (cert_verify={})", url, self.verify_certificate);
      crate::notify::throttle().await;
      match client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/x-www-form-urlencoded").body(body).send().await {
        Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
          tracing::info!("Sent Vonage notification to {}.", target);
          crate::delivery::mark_delivered(target, false);
        }
        Ok(resp) => {
          let status = resp.status();
          tracing::warn!(
            "Failed to send Vonage notification to {}: {}error={}.",
            target,
            status.canonical_reason().map(|r| format!("{}, ", r)).unwrap_or_default(),
            status.as_u16()
          );
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Vonage:{} notification.", target);
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
  use wiremock::matchers::{body_string, body_string_contains, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn vonage(url: &str) -> Vonage {
    Vonage::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_invalid_urls() {
    for schema in ["vonage", "nexmo"] {
      let urls = vec![
        format!("{}://", schema),
        format!("{}://:@/", schema),
        format!("{}://AC{}@12345678", schema, "a".repeat(8)),
        format!("{}://AC{}:{}@{}", schema, "a".repeat(8), "b".repeat(16), "3".repeat(9)),
        format!("{}://AC{}:{}@{}/?ttl=0", schema, "b".repeat(8), "c".repeat(16), "3".repeat(11)),
        format!("{}://AC{}:{}@{}/?ttl=604800001", schema, "b".repeat(8), "c".repeat(16), "3".repeat(11)),
        format!("{}://AC{}:{}@{}", schema, "d".repeat(8), "e".repeat(16), "a".repeat(11)),
        // Keys and secrets are alphanumeric
        format!("{}://A_C:{}@{}", schema, "e".repeat(16), "3".repeat(11)),
      ];
      for url in &urls {
        assert!(from_url(url).is_none(), "Should not parse: {}", url);
      }
    }
  }

  #[test]
  fn test_valid_urls() {
    for schema in ["vonage", "nexmo"] {
      let urls = vec![
        format!("{}://AC{}:{}@{}/123/{}/abcd/", schema, "f".repeat(8), "g".repeat(16), "3".repeat(11), "9".repeat(15)),
        format!("{}://AC{}:{}@{}", schema, "h".repeat(8), "i".repeat(16), "5".repeat(11)),
        format!("{}://_?key=AC{}&secret={}&from={}", schema, "a".repeat(8), "b".repeat(16), "5".repeat(11)),
        format!("{}://_?key=AC{}&secret={}&source={}", schema, "a".repeat(8), "b".repeat(16), "5".repeat(11)),
        format!("{}://_?key=AC{}&secret={}&from={}&to={}", schema, "a".repeat(8), "b".repeat(16), "5".repeat(11), "7".repeat(13)),
        format!("{}://AC{}:{}@{}?ttl=abc", schema, "h".repeat(8), "i".repeat(16), "5".repeat(11)),
      ];
      for url in &urls {
        assert!(from_url(url).is_some(), "Should parse: {}", url);
      }
    }
  }

  #[test]
  fn test_from_url_fields() {
    let v = vonage(&format!("vonage://ACkey:{}@+1%20555%20123%202000/15551232001/123?ttl=30000", "s".repeat(16)));
    assert_eq!(v.apikey, "ACkey");
    assert_eq!(v.secret, "s".repeat(16));
    assert_eq!(v.source, "15551232000");
    assert_eq!(v.targets, vec!["15551232001"]);
    assert_eq!(v.ttl, 30000);
    assert_eq!(vonage(&format!("vonage://k:{}@15551232000?ttl=abc", "s".repeat(16))).ttl, DEFAULT_TTL);
  }

  #[test]
  fn test_service_details() {
    let d = Vonage::static_details();
    assert_eq!(d.service_name, "Vonage");
    assert!(d.protocols.contains(&"vonage") && d.protocols.contains(&"nexmo"));
  }

  #[tokio::test]
  async fn test_send_form() {
    // Matches upstream pycapture output
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/sms/json"))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(body_string("api_key=ACaaaa&api_secret=bbbb&ttl=30000&from=15551232000&text=title%0D%0Aa+b~%2A&to=15551232001"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("nexmo://ACaaaa:bbbb@15551232000/15551232001?ttl=30000").unwrap();
    let ctx = NotifyContext { body: "title\r\na b~*".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_to_self_and_failures() {
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/sms/json"))
      .and(body_string("api_key=k&api_secret=s&ttl=900000&from=15551232000&text=b&to=15551232000"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("vonage://k:s@15551232000").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/sms/json")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second target fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/sms/json"))
      .and(body_string_contains("to=15551232222"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/sms/json")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("vonage://{}:{}@15551233456/15551231111/15551232222?retry=1&wait=0", "a".repeat(8), "b".repeat(16))));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { body: "b".into(), ..Default::default() })).await.is_success());
    let sent: Vec<String> = crate::notify::custom_json::requests_to(&server, "POST", "/sms/json")
      .await
      .iter()
      .map(|r| url::form_urlencoded::parse(&r.body).find(|(k, _)| k == "to").unwrap().1.into_owned())
      .collect();
    assert_eq!(sent, vec!["15551231111", "15551232222", "15551232222"]);
  }
}
