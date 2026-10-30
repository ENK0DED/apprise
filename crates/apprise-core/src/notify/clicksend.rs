use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use async_trait::async_trait;
use serde_json::json;

/// Most messages one request may carry in batch mode.
const DEFAULT_BATCH_SIZE: usize = 1000;

pub struct ClickSend {
  user: String,
  password: String,
  batch: bool,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl ClickSend {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (user, password) = (url.user.clone(), url.password.clone());
    // ?key= is the API key, which is the password
    let password = url.get("key").filter(|k| !k.is_empty()).map(str::to_string).or(password);
    let (Some(user), Some(password)) = (user.filter(|u| !u.is_empty()), password.filter(|p| !p.is_empty())) else {
      tracing::warn!("A ClickSend user/pass was not provided.");
      return None;
    };

    let mut raw_targets = vec![url.host.clone().unwrap_or_default()];
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

    Some(Self {
      user,
      password,
      batch: url.get("batch").map(parse_bool).unwrap_or(false),
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "ClickSend",
      service_url: Some("https://clicksend.com/"),
      setup_url: Some("https://appriseit.com/services/clicksend/"),
      protocols: vec!["clicksend"],
      description: "Send SMS via ClickSend.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for ClickSend {
  fn schemas(&self) -> &[&str] {
    &["clicksend"]
  }
  fn service_name(&self) -> &str {
    "ClickSend"
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
      tracing::warn!("There were no ClickSend targets to notify.");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = format!("{}/v3/sms/send", service_base("https://rest.clicksend.com"));
    let batch_size = if self.batch { DEFAULT_BATCH_SIZE } else { 1 };
    let mut has_error = false;

    for (index, batch) in self.targets.chunks(batch_size).enumerate() {
      // Skip a batch that already went out so a retry does not deliver it
      // to those recipients twice
      let key = (index * batch_size).to_string();
      if crate::delivery::is_delivered(&key, false) {
        continue;
      }

      let messages: Vec<_> = batch.iter().map(|to| json!({ "source": "php", "body": ctx.body, "to": format!("+{}", to) })).collect();
      let payload = json!({ "messages": messages });
      let p_target = if batch_size == 1 { format!(" to {}", batch[0]) } else { "(s)".to_string() };

      crate::notify::throttle().await;
      let resp = match client
        .post(&notify_url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json; charset=utf-8")
        .basic_auth(&self.user, Some(&self.password))
        .body(payload.to_string())
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending {} ClickSend notification(s).", batch.len());
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };

      let status = resp.status().as_u16();
      if status != 200 {
        let status_str = if status == 401 { "Unauthorized - Invalid Token., " } else { "" };
        tracing::warn!("Failed to send {} ClickSend notification{}: {}error={}.", batch.len(), p_target, status_str, status);
        tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent {} ClickSend notification{}.", batch.len(), p_target);
      crate::delivery::mark_delivered(&key, false);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, body_string_contains, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> ClickSend {
    ClickSend::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["clicksend://", "clicksend://:@/", "clicksend://user@15551234567", "clicksend://user@15551234567?key="] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // invalid targets; loads but has no one to notify
      format!("clicksend://user:pass@{}/{}/{}", "1".repeat(9), "2".repeat(15), "a".repeat(13)),
      format!("clicksend://user:pass@{}?batch=yes", "3".repeat(14)),
      format!("clicksend://user:pass@{}?batch=yes&to={}", "3".repeat(14), "6".repeat(14)),
      format!("clicksend://user:pass@{}?batch=no", "3".repeat(14)),
      format!("clicksend://user@{}?batch=no&key=abc123", "3".repeat(14)),
      format!("clicksend://{}?user=user&pass=abc123", "3".repeat(14)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse(&format!("clicksend://user:pass@{}?batch=yes&to={}", "3".repeat(14), "6".repeat(14)));
    assert_eq!(obj.user, "user");
    assert_eq!(obj.password, "pass");
    assert!(obj.batch);
    assert_eq!(obj.targets, vec!["3".repeat(14), "6".repeat(14)]);

    // ?key= wins over the URL password
    let obj = parse("clicksend://user:pass@15551234567?key=abc123");
    assert_eq!(obj.password, "abc123");
    assert!(!obj.batch);

    assert!(parse(&format!("clicksend://user:pass@{}/{}/{}", "1".repeat(9), "2".repeat(15), "a".repeat(13))).targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let details = ClickSend::static_details();
    assert_eq!(details.service_name, "ClickSend");
    assert_eq!(details.protocols, vec!["clicksend"]);
    let obj = parse("clicksend://user:pass@15551234567");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send_per_target() {
    let server = MockServer::start().await;
    for to in ["+15551231234", "+15557654321"] {
      Mock::given(method("POST"))
        .and(path("/v3/sms/send"))
        // user:pass
        .and(header("Authorization", "Basic dXNlcjpwYXNz"))
        .and(header("Content-Type", "application/json; charset=utf-8"))
        .and(body_json(json!({ "messages": [{ "source": "php", "body": "title\r\nbody", "to": to }] })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("clicksend://user:pass@15551231234/15557654321").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v3/sms/send"))
      .and(body_json(json!({ "messages": [
        { "source": "php", "body": "body", "to": "+15551231234" },
        { "source": "php", "body": "body", "to": "+15557654321" },
      ] })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("clicksend://user:pass@15551231234/15557654321?batch=yes").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url("clicksend://user:pass@123").unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v3/sms/send")).respond_with(ResponseTemplate::new(401)).expect(2).mount(&server).await;
    let svc = from_url("clicksend://user:pass@15551231234/15557654321").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_accepted_batches() {
    let server = MockServer::start().await;
    let send = || Mock::given(method("POST")).and(path("/v3/sms/send"));
    send().and(body_string_contains("+15551231234")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    // The second number fails once, then accepts the retry
    send().and(body_string_contains("+15557654321")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    send().and(body_string_contains("+15557654321")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("clicksend://user:pass@15551231234/15557654321?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("body"))).await.is_success());
  }

  #[tokio::test]
  async fn test_send_only_200_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v3/sms/send")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let svc = from_url("clicksend://user:pass@15551231234").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }
}
