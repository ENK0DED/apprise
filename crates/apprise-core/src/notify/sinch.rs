use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use async_trait::async_trait;
use serde_json::{Value, json};

const SINCH_REGIONS: &[&str] = &["us", "eu"];

pub struct Sinch {
  service_plan_id: String,
  api_token: String,
  /// `+E.164` number, or a bare 5/6 digit short code
  source: String,
  targets: Vec<String>,
  region: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

fn is_hex(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit())
}

impl Sinch {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // sinch://service_plan_id:api_token@source/targets
    // Upstream unquotes these query values once more
    let qs = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(unquote);
    let service_plan_id = qs("spi").or_else(|| url.user.clone()).unwrap_or_default();
    let api_token = qs("token").or_else(|| url.password.clone()).unwrap_or_default();
    let source = qs("source").or_else(|| qs("from")).or_else(|| url.host.clone()).unwrap_or_default();
    let region = qs("region").map(|r| r.to_lowercase()).unwrap_or_else(|| "us".to_string());

    if !is_hex(&service_plan_id) {
      tracing::warn!("An invalid Sinch Account SID ({}) was specified.", service_plan_id);
      return None;
    }
    if !is_hex(&api_token) {
      tracing::warn!("An invalid Sinch Authentication Token was specified.");
      return None;
    }
    if !SINCH_REGIONS.contains(&region.as_str()) {
      tracing::warn!("The region specified ({}) is invalid.", region);
      return None;
    }
    let Some(phone) = is_phone_no(&source, 5) else {
      tracing::warn!("The Account (From) Phone # or Short-code specified ({}) is invalid.", source);
      return None;
    };
    let source = match phone.full.len() {
      // Short codes are sent as is
      5 | 6 => phone.full,
      11..=14 => format!("+{}", phone.full),
      _ => {
        tracing::warn!("The Account (From) Phone # specified ({}) is invalid.", source);
        return None;
      }
    };

    let mut entries = url.path_parts.clone();
    if let Some(to) = qs("to") {
      entries.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    for target in parse_phone_no(&entries) {
      match is_phone_no(&target, 10) {
        Some(phone) => targets.push(format!("+{}", phone.full)),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self { service_plan_id, api_token, source, targets, region, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Sinch",
      service_url: Some("https://sinch.com/"),
      setup_url: Some("https://appriseit.com/services/sinch/"),
      protocols: vec!["sinch"],
      description: "Send SMS via Sinch.",
      attachment_support: false,
    }
  }

  fn is_short_code(&self) -> bool {
    !self.source.starts_with('+')
  }
}

#[async_trait]
impl Notify for Sinch {
  fn schemas(&self) -> &[&str] {
    &["sinch"]
  }
  fn service_name(&self) -> &str {
    "Sinch"
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
  fn request_rate_per_sec(&self) -> f64 {
    0.20
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // A short code can't receive messages, so it can't notify itself
    if self.targets.is_empty() && self.is_short_code() {
      tracing::warn!("There are no valid Sinch targets to notify.");
      return Ok(false);
    }
    let targets = if self.targets.is_empty() { std::slice::from_ref(&self.source) } else { self.targets.as_slice() };
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/xms/v1/{}/batches", service_base(&format!("https://{}.sms.api.sinch.com", self.region)), self.service_plan_id);
    let mut has_error = false;

    for target in targets {
      // A retry skips numbers that already accepted the message
      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      let payload = json!({ "body": ctx.body, "from": self.source, "to": [target] });
      crate::notify::throttle().await;
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Authorization", format!("Bearer {}", self.api_token))
        .header("Content-Type", "application/json")
        .body(serde_json::to_string(&payload)?)
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200 | 201) => {
          tracing::info!("Sent Sinch notification to {}.", target);
          crate::delivery::mark_delivered(target, false);
        }
        Ok(resp) => {
          let status = resp.status();
          let text = resp.text().await.unwrap_or_default();
          // Sinch explains failures in a JSON body
          let detail: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
          let code = detail.get("code").map(|c| c.to_string().trim_matches('"').to_string()).unwrap_or_else(|| status.as_u16().to_string());
          let message = detail.get("message").and_then(Value::as_str).or(status.canonical_reason()).unwrap_or("");
          tracing::warn!("Failed to send Sinch notification to {}: {}{}error={}.", target, message, if message.is_empty() { "" } else { ", " }, code);
          tracing::debug!("Response Details: {}", text.chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Sinch:{} notification.", target);
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

  fn sinch(url: &str) -> Sinch {
    Sinch::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_invalid_urls() {
    let a32 = "a".repeat(32);
    let b32 = "b".repeat(32);
    let urls = vec![
      "sinch://".to_string(),
      "sinch://:@/".to_string(),
      // Just the service plan id
      format!("sinch://{}@12345678", a32),
      // Invalid source
      format!("sinch://{}:{}@_", a32, b32),
      // 9 digits is neither a short code nor a full number
      format!("sinch://{}:{}@{}", a32, b32, "3".repeat(9)),
      format!("sinch://{}:{}@{}?region=invalid", a32, b32, "5".repeat(11)),
      // Ids and tokens are hexadecimal
      format!("sinch://{}:{}@{}", "z".repeat(32), b32, "5".repeat(11)),
      format!("sinch://{}:{}@{}", a32, "z".repeat(32), "5".repeat(11)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let a32 = "a".repeat(32);
    let b32 = "b".repeat(32);
    let urls = vec![
      // Short code with no targets (notify fails)
      format!("sinch://{}:{}@{}", a32, b32, "3".repeat(5)),
      format!("sinch://{}:{}@{}/123/{}/abcd/", a32, b32, "5".repeat(11), "3".repeat(11)),
      format!("sinch://{}:{}@12345/{}", a32, b32, "4".repeat(11)),
      format!("sinch://{}:{}@123456/{}", a32, b32, "4".repeat(11)),
      format!("sinch://{}:{}@{}", a32, b32, "5".repeat(11)),
      format!("sinch://{}:{}@{}?region=eu", a32, b32, "5".repeat(11)),
      format!("sinch://{}:{}@{}?region=EU", a32, b32, "5".repeat(11)),
      format!("sinch://_?spi={}&token={}&from={}", a32, b32, "5".repeat(11)),
      format!("sinch://_?spi={}&token={}&source={}", a32, b32, "5".repeat(11)),
      format!("sinch://_?spi={}&token={}&from={}&to={}", a32, b32, "5".repeat(11), "7".repeat(13)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let a32 = "a".repeat(32);
    let b32 = "b".repeat(32);
    let s = sinch(&format!("sinch://{}:{}@{}/123/{}/abcd/", a32, b32, "5".repeat(11), "3".repeat(11)));
    assert_eq!(s.service_plan_id, a32);
    assert_eq!(s.api_token, b32);
    assert_eq!(s.source, format!("+{}", "5".repeat(11)));
    assert_eq!(s.targets, vec![format!("+{}", "3".repeat(11))]);
    assert_eq!(s.region, "us");

    let s = sinch(&format!("sinch://{}:{}@12345/{}?region=EU", a32, b32, "4".repeat(11)));
    assert_eq!(s.source, "12345");
    assert!(s.is_short_code());
    assert_eq!(s.region, "eu");

    let s = sinch(&format!("sinch://_?spi={}&token={}&from={}&to={}", a32, b32, "5".repeat(11), "7".repeat(13)));
    assert_eq!(s.targets, vec![format!("+{}", "7".repeat(13))]);
  }

  #[test]
  fn test_service_details() {
    let d = Sinch::static_details();
    assert_eq!(d.service_name, "Sinch");
    assert!(d.protocols.contains(&"sinch"));
  }

  #[tokio::test]
  async fn test_send_payload() {
    // Matches upstream pycapture output
    let a32 = "a".repeat(32);
    let b32 = "b".repeat(32);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/xms/v1/{}/batches", a32)))
      .and(header("Authorization", format!("Bearer {}", b32).as_str()))
      .and(header("Content-Type", "application/json"))
      .and(body_json(json!({"body": "title\r\nbody", "from": "+15551231234", "to": ["+15551232000"]})))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("sinch://{}:{}@15551231234/15551232000?region=eu", a32, b32)).unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_to_self_without_targets() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/xms/v1/{}/batches", "a".repeat(32))))
      .and(body_json(json!({"body": "b", "from": "+15551231234", "to": ["+15551231234"]})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("sinch://{}:{}@15551231234", "a".repeat(32), "b".repeat(32))).unwrap();
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    // Short code without targets
    let svc = from_url(&format!("sinch://{}:{}@12345", "a".repeat(32), "b".repeat(32))).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/xms/v1/{}/batches", "a".repeat(32))))
      .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"code": 21211, "message": "The 'To' number +1234567 is not a valid phone number."}"#))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("sinch://{}:{}@15551231234/15551232000", "a".repeat(32), "b".repeat(32))).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The second target fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path(format!("/xms/v1/{}/batches", "a".repeat(32))))
      .and(body_partial_json(json!({"to": ["+15551232222"]})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(format!("/xms/v1/{}/batches", "a".repeat(32)))).respond_with(ResponseTemplate::new(201)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add(&format!("sinch://{}:{}@15551231234/15551231111/15551232222?retry=1&wait=0", "a".repeat(32), "b".repeat(32))));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { body: "b".into(), ..Default::default() })).await.is_success());
    let sent: Vec<Value> = crate::notify::custom_json::requests_to(&server, "POST", &format!("/xms/v1/{}/batches", "a".repeat(32)))
      .await
      .iter()
      .map(|r| r.body_json::<Value>().unwrap()["to"][0].clone())
      .collect();
    assert_eq!(sent, vec![json!("+15551231111"), json!("+15551232222"), json!("+15551232222")]);
  }
}
