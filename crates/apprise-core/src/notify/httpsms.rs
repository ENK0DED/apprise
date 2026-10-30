use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::{is_phone_no, parse_phone_no};
use async_trait::async_trait;
use serde_json::json;

pub struct HttpSms {
  apikey: String,
  source: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl HttpSms {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = url.get("key").filter(|k| !k.is_empty()).map(str::to_string).or_else(|| url.user.clone()).unwrap_or_default();
    // validate_regex(): must start with a non-whitespace character
    if apikey.is_empty() || apikey.starts_with(char::is_whitespace) {
      tracing::warn!("An invalid API Key was specified.");
      return None;
    }
    let apikey = apikey.trim().to_string();

    let host = url.host.clone().unwrap_or_default();
    let (source, mut raw_targets) = match url.get("from").filter(|f| !f.is_empty()) {
      // The host is a target when ?from= names the source
      Some(from) => (from.to_string(), parse_phone_no(&[host])),
      None => (host, Vec::new()),
    };
    raw_targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }

    let Some(source) = is_phone_no(&source, 10).map(|p| p.full) else {
      tracing::warn!("The Account (From) Phone # specified ({}) is invalid.", source);
      return None;
    };

    let mut targets = Vec::new();
    let mut has_error = false;
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10) {
        Some(p) => targets.push(p.full),
        None => {
          has_error = true;
          tracing::warn!("Dropped invalid phone # ({}) specified.", target);
        }
      }
    }
    if raw_targets.is_empty() && !has_error {
      // Notify ourselves
      targets.push(source.clone());
    }

    Some(Self { apikey, source, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "httpSMS",
      service_url: Some("https://httpsms.com"),
      setup_url: Some("https://appriseit.com/services/httpsms/"),
      protocols: vec!["httpsms"],
      description: "Send SMS via httpSMS.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for HttpSms {
  fn schemas(&self) -> &[&str] {
    &["httpsms"]
  }
  fn service_name(&self) -> &str {
    "httpSMS"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    160
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no HttpSMS targets to notify");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = format!("{}/v1/messages/send", service_base("https://api.httpsms.com"));
    let mut has_error = false;

    for target in &self.targets {
      // Skip a target that already accepted this message so a retry does
      // not deliver it twice
      if crate::delivery::is_delivered(target, false) {
        continue;
      }

      let payload = json!({ "from": format!("+{}", self.source), "to": format!("+{}", target), "content": ctx.body });
      crate::notify::throttle().await;
      let resp = match client
        .post(&notify_url)
        .header("User-Agent", APP_ID)
        .header("x-api-key", self.apikey.as_str())
        .header("Content-Type", "application/json")
        .body(payload.to_string())
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending HttpSMS: to {}", target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };

      let status = resp.status().as_u16();
      if status != 200 {
        tracing::warn!("Failed to send HttpSMS notification to {}: error={}.", target, status);
        tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent HttpSMS notification to {}.", target);
      crate::delivery::mark_delivered(target, false);
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

  fn parse(url: &str) -> HttpSms {
    HttpSms::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "httpsms://".to_string(),
      "httpsms://:@/".to_string(),
      // invalid source number
      format!("httpsms://{}:{}@{}", "u".repeat(10), "p".repeat(10), "3".repeat(5)),
      // no API key
      format!("httpsms://{}", "2".repeat(10)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // invalid target; loads but has no one to notify
      format!("httpsms://{}@{}/{}", "p".repeat(10), "1".repeat(10), "5".repeat(2)),
      format!("httpsms://{}@{}", "p".repeat(10), "2".repeat(10)),
      format!("httpsms://{}@9876543210/{}/abcd/", "b".repeat(10), "3".repeat(11)),
      format!("httpsms://{}@{}", "c".repeat(10), "4".repeat(11)),
      format!("httpsms://?key={}&from={}", "y".repeat(10), "5".repeat(11)),
      format!("httpsms://?key={}&from={}&to={}", "b".repeat(10), "5".repeat(11), "7".repeat(13)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse("httpsms://bbbbbbbbbb@9876543210/33333333333/abcd/");
    assert_eq!(obj.apikey, "bbbbbbbbbb");
    assert_eq!(obj.source, "9876543210");
    assert_eq!(obj.targets, vec!["33333333333"]);

    // No targets: notify ourselves
    assert_eq!(parse("httpsms://key@+1 555 123 4567").targets, vec!["15551234567"]);
    // Only invalid targets: nobody to notify
    assert!(parse("httpsms://key@1111111111/55").targets.is_empty());

    // ?from= turns the host into a target; ?key= wins over the user
    let obj = parse("httpsms://user@15551231234?key=apikey&from=14051231234");
    assert_eq!(obj.apikey, "apikey");
    assert_eq!(obj.source, "14051231234");
    assert_eq!(obj.targets, vec!["15551231234"]);
  }

  #[test]
  fn test_service_details() {
    let details = HttpSms::static_details();
    assert_eq!(details.service_name, "httpSMS");
    assert_eq!(details.protocols, vec!["httpsms"]);
    let obj = parse("httpsms://key@15551231234");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send_per_target() {
    let server = MockServer::start().await;
    for to in ["+15551231234", "+15555555555"] {
      Mock::given(method("POST"))
        .and(path("/v1/messages/send"))
        .and(header("x-api-key", "mykey"))
        .and(header("Content-Type", "application/json"))
        .and(body_json(json!({ "from": "+14051231234", "to": to, "content": "title\r\nbody" })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("httpsms://mykey@1 (405) 123 1234/+1(555) 123-1234/1555 5555555/12").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url("httpsms://key@1111111111/55").unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_accepted_targets() {
    let server = MockServer::start().await;
    let send = || Mock::given(method("POST")).and(path("/v1/messages/send"));
    send().and(body_partial_json(json!({"to": "+15551231234"}))).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    // The second number fails once, then accepts the retry
    let second = json!({"to": "+15555555555"});
    send().and(body_partial_json(&second)).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    send().and(body_partial_json(&second)).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("httpsms://key@14051231234/15551231234/15555555555?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("body"))).await.is_success());
  }

  #[tokio::test]
  async fn test_send_only_200_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages/send")).respond_with(ResponseTemplate::new(202)).expect(2).mount(&server).await;
    let svc = from_url("httpsms://key@14051231234/15551231234/15555555555").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }
}
