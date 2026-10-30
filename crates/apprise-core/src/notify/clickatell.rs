use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::{is_phone_no, parse_phone_no, parse_phone_no_with_prefix};
use async_trait::async_trait;

pub struct Clickatell {
  apikey: String,
  source: Option<String>,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Clickatell {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = url.host.clone().unwrap_or_default();
    // validate_regex(): must start with a non-whitespace character
    if apikey.is_empty() || apikey.starts_with(char::is_whitespace) {
      tracing::warn!("An invalid Clickatell API Token was specified.");
      return None;
    }
    let apikey = apikey.trim().to_string();

    let source = url.get("from").filter(|f| !f.is_empty()).map(str::to_string).or_else(|| url.user.clone());
    let source = match source {
      Some(s) => match is_phone_no(&s, 10) {
        Some(p) => Some(p.full),
        None => {
          tracing::warn!("The Account (From) Phone # specified ({}) is invalid.", s);
          return None;
        }
      },
      None => None,
    };

    let mut raw_targets = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    for target in parse_phone_no_with_prefix(&raw_targets) {
      match is_phone_no(&target, 10) {
        Some(p) => targets.push(p.full),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self { apikey, source, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Clickatell",
      service_url: Some("https://www.clickatell.com/"),
      setup_url: Some("https://appriseit.com/services/clickatell/"),
      protocols: vec!["clickatell"],
      description: "Send SMS via Clickatell.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Clickatell {
  fn schemas(&self) -> &[&str] {
    &["clickatell"]
  }
  fn service_name(&self) -> &str {
    "Clickatell"
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
      tracing::warn!("There were no Clickatell targets to notify");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = format!("{}/messages/http/send", service_base("https://platform.clickatell.com"));
    let mut has_error = false;

    for target in &self.targets {
      // Skip a target that already accepted this message so a retry does
      // not deliver it twice
      if crate::delivery::is_delivered(target, false) {
        continue;
      }

      let mut params = vec![("apiKey", self.apikey.as_str())];
      if let Some(source) = &self.source {
        params.push(("from", source.as_str()));
      }
      params.push(("content", ctx.body.as_str()));
      params.push(("to", target.as_str()));

      crate::notify::throttle().await;
      let resp = match client
        .get(&notify_url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .query(&params)
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Clickatell: to {}", target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };

      let status = resp.status().as_u16();
      if status != 200 && status != 202 {
        tracing::warn!("Failed to send Clickatell notification: error={}.", status);
        tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent Clickatell notification to {}", target);
      crate::delivery::mark_delivered(target, false);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  fn parse(url: &str) -> Clickatell {
    Clickatell::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "clickatell://".to_string(),
      "clickatell:///".to_string(),
      "clickatell://@/".to_string(),
      format!("clickatell://{}@/", "1".repeat(10)),
      // invalid source number
      format!("clickatell://{}@{}/", "1".repeat(3), "a".repeat(32)),
      format!("clickatell://{}/?from=123", "a".repeat(32)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let (k, p) = ("a".repeat(32), "1".repeat(10));
    let urls = vec![
      // These load but have no valid targets
      format!("clickatell://{}/", k),
      format!("clickatell://{}@{}/", p, k),
      format!("clickatell://{}@{}/123/{}/abcd", p, k, "3".repeat(15)),
      format!("clickatell://{}/{}", p, k),
      format!("clickatell://_?apikey={}", k),
      format!("clickatell://_?apikey={}&from={}", k, p),
      // These have targets
      format!("clickatell://{}@{}/{}", p, k, p),
      format!("clickatell://{}/{}", k, p),
      format!("clickatell://_?apikey={}&from={}&to={},{}", k, p, p, p),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse("clickatell://1 555 123 4567@apikey/wa:15557654321/15551112222");
    assert_eq!(obj.apikey, "apikey");
    assert_eq!(obj.source.as_deref(), Some("15551234567"));
    // Prefixed numbers are split out but are not valid phone numbers
    assert_eq!(obj.targets, vec!["15551112222"]);

    // ?apikey= is not an alias upstream; the host stays the key
    let obj = parse("clickatell://_/15551234567?apikey=XYZ&from=14051231234");
    assert_eq!(obj.apikey, "_");
    assert_eq!(obj.source.as_deref(), Some("14051231234"));
    assert_eq!(obj.targets, vec!["15551234567"]);

    assert!(parse(&format!("clickatell://{}/{}", "1".repeat(10), "a".repeat(32))).targets.is_empty());
    assert!(parse("clickatell://apikey/15551234567").source.is_none());
  }

  #[test]
  fn test_service_details() {
    let details = Clickatell::static_details();
    assert_eq!(details.service_name, "Clickatell");
    assert_eq!(details.protocols, vec!["clickatell"]);
    let obj = parse("clickatell://apikey/15551234567");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  fn query_is(expected: &'static str) -> impl Fn(&Request) -> bool {
    move |req: &Request| req.url.query() == Some(expected)
  }

  #[tokio::test]
  async fn test_send_without_source() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/messages/http/send"))
      .and(header("Accept", "application/json"))
      .and(header("Content-Type", "application/json"))
      .and(query_is("apiKey=apikey123&content=title%0D%0Abody&to=15551231234"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("clickatell://apikey123/15551231234").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_with_source() {
    let server = MockServer::start().await;
    for q in ["apiKey=apikey123&from=14051231234&content=hello&to=15557654321", "apiKey=apikey123&from=14051231234&content=hello&to=15550001111"] {
      Mock::given(method("GET")).and(path("/messages/http/send")).and(query_is(q)).respond_with(ResponseTemplate::new(202)).expect(1).mount(&server).await;
    }
    let svc = from_url("clickatell://14051231234@apikey123/15557654321/15550001111").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("hello"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url(&format!("clickatell://{}/", "a".repeat(32))).unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_error_response() {
    let server = MockServer::start().await;
    let reply = r#"{"code": 21211, "message": "The 'To' number +1234567 is not a valid phone number."}"#;
    Mock::given(method("GET")).and(path("/messages/http/send")).respond_with(ResponseTemplate::new(400).set_body_string(reply)).expect(2).mount(&server).await;
    let svc = from_url("clickatell://apikey123/15551231234/15557654321").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_accepted_targets() {
    let server = MockServer::start().await;
    let send = || Mock::given(method("GET")).and(path("/messages/http/send"));
    send().and(query_param("to", "15551231234")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    // The second number fails once, then accepts the retry
    send().and(query_param("to", "15557654321")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    send().and(query_param("to", "15557654321")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("clickatell://apikey123/15551231234/15557654321?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("body"))).await.is_success());
  }

  #[tokio::test]
  async fn test_send_201_is_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/messages/http/send")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let svc = from_url("clickatell://apikey123/15551231234").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }
}
