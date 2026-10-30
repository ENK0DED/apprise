use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::urlbase::url_host;
use async_trait::async_trait;

pub struct MessageBird {
  apikey: String,
  source: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl MessageBird {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let apikey = url_host(url);
    // ^[a-z0-9]{25}$ (case-insensitive)
    if apikey.len() != 25 || !apikey.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("An invalid MessageBird API Key was specified.");
      return None;
    }

    // msgbird://{apikey}/{source}/{targets}; ?from= replaces the source, but
    // the first path entry is still not a target (as upstream)
    let mut raw_targets = url.path_parts.clone();
    let path_source = if raw_targets.is_empty() { None } else { Some(raw_targets.remove(0)) };
    let source = url.get("from").filter(|f| !f.is_empty()).map(str::to_string).or(path_source).unwrap_or_default();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }

    let Some(source) = is_phone_no(&source, 10).map(|p| p.full) else {
      tracing::warn!("The MessageBird source specified ({}) is invalid.", source);
      return None;
    };

    let raw_targets = parse_phone_no(&raw_targets);
    let mut targets = Vec::new();
    if raw_targets.is_empty() {
      // Notify ourselves
      targets.push(source.clone());
    }
    for target in raw_targets {
      match is_phone_no(&target, 10) {
        Some(p) => targets.push(p.full),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self { apikey, source, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "MessageBird",
      service_url: Some("https://messagebird.com"),
      setup_url: Some("https://appriseit.com/services/messagebird/"),
      protocols: vec!["msgbird"],
      description: "Send SMS via MessageBird.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for MessageBird {
  fn schemas(&self) -> &[&str] {
    &["msgbird"]
  }
  fn service_name(&self) -> &str {
    "MessageBird"
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
      tracing::warn!("There were no MessageBird targets to notify.");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = format!("{}/messages", service_base("https://rest.messagebird.com"));
    let originator = format!("+{}", self.source);
    let mut has_error = false;

    for target in &self.targets {
      let recipient = format!("+{}", target);
      let payload = [("originator", originator.as_str()), ("recipients", recipient.as_str()), ("body", ctx.body.as_str())];
      let resp =
        match client.post(&notify_url).header("User-Agent", APP_ID).header("Authorization", format!("AccessKey {}", self.apikey)).form(&payload).send().await {
          Ok(r) => r,
          Err(e) => {
            tracing::warn!("A Connection error occurred sending MessageBird:{} notification.", target);
            tracing::debug!("Socket Exception: {}", e);
            has_error = true;
            continue;
          }
        };

      let status = resp.status().as_u16();
      if status != 200 && status != 201 {
        tracing::warn!("Failed to send MessageBird notification to {}: error={}.", target, status);
        tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent MessageBird notification to {}.", target);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaa";

  fn parse(url: &str) -> MessageBird {
    MessageBird::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "msgbird://".to_string(),
      // invalid sources
      format!("msgbird://{}/abcd", KEY),
      format!("msgbird://{}/123", KEY),
      // the API key must be 25 alphanumerics
      format!("msgbird://{}/15551232000", "a".repeat(24)),
      format!("msgbird://{}-/15551232000", "a".repeat(24)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      format!("msgbird://{}/15551232000", KEY),
      // invalid targets; loads but has no one to notify
      format!("msgbird://{}/15551232000/abcd", KEY),
      format!("msgbird://{}/15551232000/123", KEY),
      format!("msgbird://{}/?from=15551233000&to=15551232000", KEY),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // No targets: notify ourselves
    let obj = parse(&format!("msgbird://{}/15551232000", KEY));
    assert_eq!(obj.apikey, KEY);
    assert_eq!(obj.source, "15551232000");
    assert_eq!(obj.targets, vec!["15551232000"]);

    let obj = parse(&format!("msgbird://{}/14051231234/15557654321?to=15550001111", KEY));
    assert_eq!(obj.source, "14051231234");
    assert_eq!(obj.targets, vec!["15557654321", "15550001111"]);

    let obj = parse(&format!("msgbird://{}/?from=15551233000&to=15551232000", KEY));
    assert_eq!(obj.source, "15551233000");
    assert_eq!(obj.targets, vec!["15551232000"]);

    // ?from= replaces the source; the first path entry is still dropped
    let obj = parse(&format!("msgbird://{}/14051231234/15557654321?from=15551233000", KEY));
    assert_eq!(obj.source, "15551233000");
    assert_eq!(obj.targets, vec!["15557654321"]);

    assert!(parse(&format!("msgbird://{}/15551232000/abcd", KEY)).targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let details = MessageBird::static_details();
    assert_eq!(details.service_name, "MessageBird");
    assert_eq!(details.protocols, vec!["msgbird"]);
    let obj = parse(&format!("msgbird://{}/15551232000", KEY));
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send_per_target() {
    let server = MockServer::start().await;
    for to in ["15557654321", "15550001111"] {
      Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("Authorization", format!("AccessKey {}", KEY).as_str()))
        .and(header("Content-Type", "application/x-www-form-urlencoded"))
        .and(body_string(format!("originator=%2B14051231234&recipients=%2B{}&body=title%0D%0Abody", to)))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("msgbird://{}/14051231234/15557654321?to=15550001111", KEY)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url(&format!("msgbird://{}/15551232000/abcd", KEY)).unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [500, 202] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url(&format!("msgbird://{}/15551232000", KEY)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap(), "{}", status);
    }
  }
}
