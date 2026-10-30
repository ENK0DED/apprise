use crate::error::NotifyError;
use crate::notify::sms_targets::{is_phone_no, parse_phone_no};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct Seven {
  apikey: String,
  targets: Vec<String>,
  source: Option<String>,
  flash: bool,
  label: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Seven {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // seven://apikey/targets?from=source&flash=yes&label=x
    let qs = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());
    let apikey = url.host.clone().unwrap_or_default();
    if apikey.is_empty() {
      tracing::warn!("An invalid seven API Key was specified.");
      return None;
    }
    let mut entries = url.path_parts.clone();
    if let Some(to) = qs("to") {
      entries.extend(parse_phone_no([to], false));
    }
    let mut targets = Vec::new();
    for target in parse_phone_no(&entries, false) {
      match is_phone_no(&target, 10) {
        Some(phone) => targets.push(phone.full),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }
    let source = qs("from").or_else(|| qs("source")).map(str::to_string);
    let flash = url.get("flash").map(parse_bool).unwrap_or(false);
    let label = url.get("label").map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    Some(Self { apikey, targets, source, flash, label, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "seven",
      service_url: Some("https://www.seven.io"),
      setup_url: Some("https://appriseit.com/services/seven/"),
      protocols: vec!["seven"],
      description: "Send SMS via seven.io.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Seven {
  fn schemas(&self) -> &[&str] {
    &["seven"]
  }
  fn service_name(&self) -> &str {
    "seven"
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
    if self.targets.is_empty() {
      tracing::warn!("There were no seven targets to notify.");
      return Ok(false);
    }
    let mut payload = json!({ "to": Value::Null, "text": ctx.body });
    if let Some(source) = &self.source {
      payload["from"] = json!(source);
    }
    if self.flash {
      payload["flash"] = json!(true);
    }
    if let Some(label) = &self.label {
      payload["label"] = json!(label);
    }

    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/api/sms", service_base("https://gateway.seven.io"));
    let mut has_error = false;
    for target in &self.targets {
      payload["to"] = json!(format!("+{}", target));
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .header("SentWith", "Apprise")
        .header("X-Api-Key", &self.apikey)
        .body(serde_json::to_string(&payload)?)
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200 | 201) => tracing::info!("Sent seven notification to {}.", target),
        Ok(resp) => {
          let status = resp.status();
          tracing::warn!(
            "Failed to send seven notification to {}: {}error={}.",
            target,
            status.canonical_reason().map(|r| format!("{}, ", r)).unwrap_or_default(),
            status.as_u16()
          );
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending seven:{} notification.", target);
          tracing::debug!("Socket Exception: {}", e);
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
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn seven(url: &str) -> Seven {
    Seven::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_valid_urls() {
    let apikey = "a".repeat(25);
    let urls = vec![
      format!("seven://{}/15551232000", apikey),
      format!("seven://{}/?to=15551232000", apikey),
      // Too short a number is dropped (notify then fails)
      format!("seven://{}/15551", apikey),
      format!("seven://{}/", apikey),
      format!("seven://{}/15551232000?from=apprise", "3".repeat(14)),
      format!("seven://{}/15551232000?source=apprise", "3".repeat(14)),
      format!("seven://{}/15551232000?from=apprise&flash=true", "3".repeat(14)),
      format!("seven://{}/15551232000?source=apprise&flash=true", "3".repeat(14)),
      format!("seven://{}/15551232000?source=AR&flash=1&label=123", "3".repeat(14)),
      format!("seven://{}/15551232000?from=AR&flash=yes&label=123&verify=no", "3".repeat(14)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["seven://"];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let apikey = "a".repeat(25);
    let s = seven(&format!("seven://{}/15551232000/+1%20(555)%20123-2001?to=15551232002", apikey));
    assert_eq!(s.apikey, apikey);
    assert_eq!(s.targets, vec!["15551232000", "15551232001", "15551232002"]);
    assert_eq!(s.source, None);
    assert!(!s.flash);
    assert_eq!(s.label, None);

    // from= wins over source=
    let s = seven(&format!("seven://{}/15551232000?from=AR&source=XX&flash=yes&label=123", apikey));
    assert_eq!(s.source.as_deref(), Some("AR"));
    assert!(s.flash);
    assert_eq!(s.label.as_deref(), Some("123"));
    let s = seven(&format!("seven://{}/15551232000?source=XX&label=", apikey));
    assert_eq!(s.source.as_deref(), Some("XX"));
    assert_eq!(s.label, None);

    assert!(seven(&format!("seven://{}/15551", apikey)).targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let d = Seven::static_details();
    assert_eq!(d.service_name, "seven");
    assert!(d.protocols.contains(&"seven"));
    assert!(!d.attachment_support);
  }

  #[tokio::test]
  async fn test_send_per_target() {
    // Matches upstream pycapture output
    let server = MockServer::start().await;
    for to in ["+15551232000", "+15551232001"] {
      Mock::given(method("POST"))
        .and(path("/api/sms"))
        .and(header("X-Api-Key", "a".repeat(25).as_str()))
        .and(header("SentWith", "Apprise"))
        .and(header("Accept", "application/json"))
        .and(header("Content-Type", "application/json"))
        .and(body_json(json!({"to": to, "text": "title\r\nbody", "from": "apprise", "flash": true, "label": "lbl"})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("seven://{}/15551232000/15551232001?from=apprise&flash=yes&label=lbl", "a".repeat(25))).unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_minimal_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({"to": "+15551232000", "text": "body"})))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("seven://{}/15551232000", "a".repeat(25))).unwrap();
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    let svc = from_url(&format!("seven://{}/15551", "a".repeat(25))).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    let svc = from_url(&format!("seven://{}/15551232000", "a".repeat(25))).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }
}
