use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::urlbase::{url_credentials, url_host};
use async_trait::async_trait;
use serde_json::Value;

pub struct Kavenegar {
  apikey: String,
  source: Option<String>,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Kavenegar {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = url_host(url);
    // ^[a-z0-9]+$ (case-insensitive)
    if apikey.is_empty() || !apikey.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("An invalid Kavenegar API Key was specified.");
      return None;
    }

    let source = url.get("from").filter(|f| !f.is_empty()).map(str::to_string).or_else(|| url_credentials(url).0.filter(|u| !u.is_empty()));
    let source = match source {
      Some(s) => match is_phone_no(&s, 10) {
        Some(p) => Some(p.full),
        None => {
          tracing::warn!("The Kavenegar source specified ({}) is invalid.", s);
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
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10) {
        Some(p) => targets.push(p.full),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self { apikey, source, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Kavenegar",
      service_url: Some("https://kavenegar.com/"),
      setup_url: Some("https://appriseit.com/services/kavenegar/"),
      protocols: vec!["kavenegar"],
      description: "Send SMS via Kavenegar.",
      attachment_support: false,
    }
  }
}

fn status_lookup(status: u16) -> &'static str {
  match status {
    200 => "The request was approved",
    400 => "Parameters are incomplete",
    401 => "Account has been disabled",
    402 => "The operation failed",
    403 => "The API Key is invalid",
    404 => "The method is unknown",
    405 => "The GET/POST request is wrong",
    406 => "Invalid mandatory parameters sent",
    407 => "You canot access the information you want",
    409 => "The server is unable to response",
    411 => "The recipient is invalid",
    412 => "The sender is invalid",
    413 => "Message empty or message length exceeded",
    414 => "The number of recipients is more than 200",
    415 => "The start index is larger then the total",
    416 => "The source IP of the service does not match the settings",
    417 => "The submission date is incorrect, either expired or not in the correct format",
    418 => "Your account credit is insufficient",
    422 => "Data cannot be processed due to invalid characters",
    501 => "SMS can only be sent to the account holder number",
    _ => "",
  }
}

#[async_trait]
impl Notify for Kavenegar {
  fn schemas(&self) -> &[&str] {
    &["kavenegar"]
  }
  fn service_name(&self) -> &str {
    "Kavenegar"
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
      tracing::warn!("There were no Kavenegar targets to notify.");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    // Upstream posts to the plain-http endpoint
    let notify_url = format!("{}/v1/{}/sms/send.json", service_base("http://api.kavenegar.com"), self.apikey);
    let mut has_error = false;

    for target in &self.targets {
      let mut params = vec![("receptor", target.as_str()), ("message", ctx.body.as_str())];
      if let Some(source) = &self.source {
        params.push(("sender", source.as_str()));
      }

      let resp = match client.post(&notify_url).header("User-Agent", APP_ID).header("Accept", "application/json").query(&params).send().await {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Kavenegar:{} notification.", self.targets.join(", "));
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
          continue;
        }
      };

      let status = resp.status().as_u16();
      let content = resp.text().await.unwrap_or_default();
      if status != 200 && status != 201 {
        // Prefer the service's own error message
        let status_str = match serde_json::from_str::<Value>(&content) {
          Ok(v) => match v.get("message") {
            Some(Value::String(m)) => m.clone(),
            Some(m) if !m.is_null() => m.to_string(),
            _ => status_lookup(status).to_string(),
          },
          Err(_) => {
            tracing::debug!("Failed to parse Kavenegar JSON response; body: {}", content.chars().take(2000).collect::<String>());
            status_lookup(status).to_string()
          }
        };
        tracing::warn!(
          "Failed to send Kavenegar SMS notification to {}: {}{}error={}.",
          target,
          status_str,
          if status_str.is_empty() { "" } else { ", " },
          status
        );
        tracing::debug!("Response Details:\r\n{}", content.chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent Kavenegar SMS notification to {}.", target);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  fn parse(url: &str) -> Kavenegar {
    Kavenegar::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "kavenegar://".to_string(),
      "kavenegar://:@/".to_string(),
      // invalid sources
      format!("kavenegar://{}@{}/{}", "a".repeat(14), "b".repeat(24), "3".repeat(14)),
      format!("kavenegar://{}@{}/{}", "3".repeat(4), "b".repeat(24), "3".repeat(14)),
      // invalid API key characters
      format!("kavenegar://ab-cd/{}", "3".repeat(14)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // invalid targets; loads but has no one to notify
      format!("kavenegar://{}/{}/{}", "1".repeat(10), "2".repeat(15), "a".repeat(13)),
      format!("kavenegar://{}/{}", "a".repeat(24), "3".repeat(14)),
      format!("kavenegar://{}?to={}", "a".repeat(24), "3".repeat(14)),
      format!("kavenegar://{}@{}/{}", "1".repeat(14), "b".repeat(24), "3".repeat(14)),
      format!("kavenegar://{}/{}?from={}", "b".repeat(24), "3".repeat(14), "1".repeat(14)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse(&format!("kavenegar://{}@{}/{}", "1".repeat(14), "b".repeat(24), "3".repeat(14)));
    assert_eq!(obj.apikey, "b".repeat(24));
    assert_eq!(obj.source, Some("1".repeat(14)));
    assert_eq!(obj.targets, vec!["3".repeat(14)]);

    let obj = parse(&format!("kavenegar://{}/{}?from={}", "b".repeat(24), "3".repeat(14), "1".repeat(14)));
    assert_eq!(obj.source, Some("1".repeat(14)));

    let obj = parse(&format!("kavenegar://{}?to={}", "a".repeat(24), "3".repeat(14)));
    assert!(obj.source.is_none());
    assert_eq!(obj.targets, vec!["3".repeat(14)]);

    assert!(parse(&format!("kavenegar://{}/{}/{}", "1".repeat(10), "2".repeat(15), "a".repeat(13))).targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let details = Kavenegar::static_details();
    assert_eq!(details.service_name, "Kavenegar");
    assert_eq!(details.protocols, vec!["kavenegar"]);
    let obj = parse("kavenegar://apikey/15551231234");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  fn query_is(expected: &'static str) -> impl Fn(&Request) -> bool {
    move |req: &Request| req.url.query() == Some(expected)
  }

  #[tokio::test]
  async fn test_send_per_target() {
    let server = MockServer::start().await;
    for q in ["receptor=15551231234&message=title%0D%0Abody", "receptor=15557654321&message=title%0D%0Abody"] {
      Mock::given(method("POST"))
        .and(path("/v1/apikey123/sms/send.json"))
        .and(header("Accept", "application/json"))
        .and(query_is(q))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("kavenegar://apikey123/15551231234/15557654321").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_with_sender() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/apikey123/sms/send.json"))
      .and(query_is("receptor=15551231234&message=hello&sender=14051231234"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("kavenegar://apikey123/15551231234?from=14051231234").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("hello"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .respond_with(ResponseTemplate::new(418).set_body_string(r#"{"return": {"status": 418, "message": "credit"}}"#))
      .expect(2)
      .mount(&server)
      .await;
    let svc = from_url("kavenegar://apikey123/15551231234/15557654321").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url("kavenegar://apikey123/123").unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }
}
