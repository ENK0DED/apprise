use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::urlbase::{url_credentials, url_host};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct D7Networks {
  token: String,
  source: Option<String>,
  unicode: bool,
  batch: bool,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl D7Networks {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // ?token= wins; otherwise the token is user[:password] or :password
    let (user, password) = url_credentials(url);
    let token = match (url.get("token").filter(|t| !t.is_empty()), user.filter(|u| !u.is_empty()), password.filter(|p| !p.is_empty())) {
      (Some(t), _, _) => t.to_string(),
      (None, Some(u), Some(p)) => format!("{}:{}", u, p),
      (None, Some(u), None) => u,
      (None, None, Some(p)) => format!(":{}", p),
      (None, None, None) => String::new(),
    };
    // validate_regex(): must start with a non-whitespace character
    if token.is_empty() || token.starts_with(char::is_whitespace) {
      tracing::warn!("The D7 Networks token specified is invalid.");
      return None;
    }
    let token = token.trim().to_string();

    let mut raw_targets = vec![url_host(url)];
    raw_targets.extend(url.path_parts.iter().cloned());
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

    let source =
      url.get("from").filter(|s| !s.is_empty()).or_else(|| url.get("source").filter(|s| !s.is_empty())).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    Some(Self {
      token,
      source,
      unicode: url.get("unicode").map(parse_bool).unwrap_or(false),
      batch: url.get("batch").map(parse_bool).unwrap_or(false),
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "D7 Networks",
      service_url: Some("https://d7networks.com/"),
      setup_url: Some("https://appriseit.com/services/d7networks/"),
      protocols: vec!["d7sms"],
      description: "Send SMS via D7 Networks.",
      attachment_support: false,
    }
  }

  fn payload(&self, recipients: &[String], body: &str) -> Value {
    let mut globals = json!({ "channel": "sms" });
    if let Some(source) = &self.source {
      globals["originator"] = json!(source);
    }
    json!({
      "message_globals": globals,
      "messages": [{
        "recipients": recipients,
        "content": body,
        "data_coding": if self.unicode { "unicode" } else { "auto" },
      }],
    })
  }
}

fn status_lookup(status: u16) -> &'static str {
  match status {
    401 => "Invalid Argument(s) Specified.",
    403 => "Unauthorized - Authentication Failure.",
    412 => "A Routing Error Occured",
    500 => "A Serverside Error Occured Handling the Request.",
    _ => "",
  }
}

#[async_trait]
impl Notify for D7Networks {
  fn schemas(&self) -> &[&str] {
    &["d7sms"]
  }
  fn service_name(&self) -> &str {
    "D7 Networks"
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
      tracing::warn!("There were no D7 Networks targets to notify.");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = format!("{}/messages/v1/send", service_base("https://api.d7networks.com"));
    let mut has_error = false;

    // One request for every target, or a single one for all of them
    let batches: Vec<&[String]> = if self.batch { vec![&self.targets[..]] } else { self.targets.chunks(1).collect() };
    for recipients in batches {
      let p_target = recipients.join(", ");
      let resp = match client
        .post(&notify_url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", self.token))
        .body(self.payload(recipients, &ctx.body).to_string())
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending D7 Networks: {} notification.", p_target);
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
            tracing::debug!("Failed to parse D7Networks JSON response; body: {}", content.chars().take(2000).collect::<String>());
            status_lookup(status).to_string()
          }
        };
        tracing::warn!(
          "Failed to send D7 Networks SMS notification to {}: {}{}error={}.",
          p_target,
          status_str,
          if status_str.is_empty() { "" } else { ", " },
          status
        );
        tracing::debug!("Response Details:\r\n{}", content.chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }

      if self.batch {
        tracing::info!("Sent D7 Networks batch SMS notification to {} target(s).", recipients.len());
      } else {
        tracing::info!("Sent D7 Networks SMS notification to {}.", p_target);
      }
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> D7Networks {
    D7Networks::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["d7sms://", "d7sms://:@/", "d7sms://15551231234?token="] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // invalid targets; loads but has no one to notify
      format!("d7sms://token@{}/{}/{}", "1".repeat(9), "2".repeat(15), "a".repeat(13)),
      "d7sms://token1@33333333333333?batch=yes".to_string(),
      "d7sms://token:colon2@33333333333333?batch=yes".to_string(),
      "d7sms://:token3@33333333333333?batch=yes".to_string(),
      "d7sms://33333333333333?token=token6".to_string(),
      "d7sms://token4@33333333333333?unicode=no".to_string(),
      "d7sms://token8@33333333333333/44444444444444/?unicode=yes".to_string(),
      "d7sms://token@33333333333333?batch=yes&to=66666666666666".to_string(),
      "d7sms://token@33333333333333?batch=yes&from=apprise".to_string(),
      "d7sms://token@33333333333333?batch=yes&source=apprise".to_string(),
      "d7sms://token@33333333333333?batch=no".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    assert_eq!(parse("d7sms://token1@33333333333333").token, "token1");
    assert_eq!(parse("d7sms://token:colon2@33333333333333").token, "token:colon2");
    assert_eq!(parse("d7sms://:token3@33333333333333").token, ":token3");
    assert_eq!(parse("d7sms://user:pass@33333333333333?token=token6").token, "token6");

    let obj = parse("d7sms://token8@33333333333333/44444444444444/?unicode=yes&batch=yes&to=66666666666666");
    assert!(obj.unicode);
    assert!(obj.batch);
    assert_eq!(obj.targets, vec!["33333333333333", "44444444444444", "66666666666666"]);

    // ?from= wins over ?source=
    assert_eq!(parse("d7sms://token@33333333333333?from=apprise").source.as_deref(), Some("apprise"));
    assert_eq!(parse("d7sms://token@33333333333333?source=apprise").source.as_deref(), Some("apprise"));
    assert_eq!(parse("d7sms://token@33333333333333?from=one&source=two").source.as_deref(), Some("one"));
    assert!(parse("d7sms://token@33333333333333").source.is_none());

    assert!(parse(&format!("d7sms://token@{}/{}/{}", "1".repeat(9), "2".repeat(15), "a".repeat(13))).targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let details = D7Networks::static_details();
    assert_eq!(details.service_name, "D7 Networks");
    assert_eq!(details.protocols, vec!["d7sms"]);
    let obj = parse("d7sms://token@15551231234");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send_per_target() {
    let server = MockServer::start().await;
    for to in ["15551231234", "15551231236"] {
      Mock::given(method("POST"))
        .and(path("/messages/v1/send"))
        .and(header("Authorization", "Bearer Token"))
        .and(header("Content-Type", "application/json"))
        .and(header("Accept", "application/json"))
        .and(body_json(json!({
          "message_globals": { "channel": "sms" },
          "messages": [{ "recipients": [to], "content": "title\r\ntest message", "data_coding": "auto" }],
        })))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("d7sms://Token@15551231234/15551231236").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\ntest message"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch_with_source_and_unicode() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/messages/v1/send"))
      .and(body_json(json!({
        "message_globals": { "channel": "sms", "originator": "apprise" },
        "messages": [{ "recipients": ["15551231234", "15551231236"], "content": "body", "data_coding": "unicode" }],
      })))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("d7sms://Token@15551231234/15551231236?batch=yes&unicode=yes&source=apprise").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch_failure() {
    // A failed batch is reported as a failure (upstream 5e4d74d2), sent once
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"message": "Insufficient credit"}"#))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("d7sms://Token@15551231234/15551231236?batch=yes").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("test"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure_per_target() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500).set_body_string("not json")).expect(2).mount(&server).await;
    let svc = from_url("d7sms://Token@15551231234/15551231236").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("test"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url("d7sms://token@123").unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }
}
