use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::urlbase::{url_credentials, url_host};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Most numbers one request may carry in batch mode.
const DEFAULT_BATCH_SIZE: usize = 4000;

pub struct BulkVs {
  user: String,
  password: String,
  source: String,
  batch: bool,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl BulkVs {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (user, password) = url_credentials(url);
    let (Some(user), Some(password)) = (user.filter(|u| !u.is_empty()), password.filter(|p| !p.is_empty())) else {
      tracing::warn!("A BulkVS user/pass was not provided.");
      return None;
    };

    let host = url_host(url);
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

    Some(Self {
      user,
      password,
      source,
      batch: url.get("batch").map(parse_bool).unwrap_or(false),
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "BulkVS",
      service_url: Some("https://www.bulkvs.com/"),
      setup_url: Some("https://appriseit.com/services/bulkvs/"),
      protocols: vec!["bulkvs"],
      description: "Send SMS via BulkVS.",
      attachment_support: false,
    }
  }

  /// Every request's `To` value with a printable form: one number per
  /// request, or batches of numbers.
  fn recipients(&self) -> Vec<(Value, String)> {
    if self.batch {
      self.targets.chunks(DEFAULT_BATCH_SIZE).map(|c| (json!(c), format!("{} targets", c.len()))).collect()
    } else {
      self.targets.iter().map(|t| (json!(t), t.clone())).collect()
    }
  }
}

#[async_trait]
impl Notify for BulkVs {
  fn schemas(&self) -> &[&str] {
    &["bulkvs"]
  }
  fn service_name(&self) -> &str {
    "BulkVS"
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
      tracing::warn!("There are no BulkVS targets to notify");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = format!("{}/api/v1.0/messageSend", service_base("https://portal.bulkvs.com"));
    let mut has_error = false;

    for (to, p_target) in self.recipients() {
      let payload = json!({ "From": self.source, "To": to, "Message": ctx.body });
      let resp = match client
        .post(&notify_url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .basic_auth(&self.user, Some(&self.password))
        .json(&payload)
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending BulkVS: to {}", p_target);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
          continue;
        }
      };

      let status = resp.status().as_u16();
      if status != 200 {
        tracing::warn!("Failed to send BulkVS notification to {}: error={}.", p_target, status);
        tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent BulkVS notification to {}.", p_target);
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

  fn parse(url: &str) -> BulkVs {
    BulkVs::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "bulkvs://".to_string(),
      "bulkvs://:@/".to_string(),
      // no password
      format!("bulkvs://{}@9876543210/", "a".repeat(10)),
      // invalid source number
      format!("bulkvs://{}:{}@{}", "u".repeat(10), "p".repeat(10), "3".repeat(5)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // invalid target; loads but has no one to notify
      format!("bulkvs://{}:{}@{}/{}", "u".repeat(10), "p".repeat(10), "1".repeat(10), "5".repeat(2)),
      format!("bulkvs://{}:{}@{}", "u".repeat(10), "p".repeat(10), "2".repeat(10)),
      format!("bulkvs://{}:{}@9876543210/{}/abcd/", "a".repeat(5), "b".repeat(10), "3".repeat(11)),
      format!("bulkvs://{}:{}@{}?batch=y", "b".repeat(5), "c".repeat(10), "4".repeat(11)),
      format!("bulkvs://?user={}&password={}&from={}", "z".repeat(10), "y".repeat(10), "5".repeat(11)),
      format!("bulkvs://?user={}&password={}&from={}&to={}", "a".repeat(10), "b".repeat(10), "5".repeat(11), "7".repeat(13)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse("bulkvs://abcd:mypass123@1 (405) 123 1234/+1(555) 123-1234/1555 5555555/12/@12?batch=n");
    assert_eq!(obj.user, "abcd");
    assert_eq!(obj.password, "mypass123");
    assert_eq!(obj.source, "14051231234");
    assert_eq!(obj.targets, vec!["15551231234", "15555555555"]);
    assert!(!obj.batch);

    // No targets: notify ourselves
    let obj = parse("bulkvs://u:p@44444444444?batch=y");
    assert_eq!(obj.targets, vec!["44444444444"]);
    assert!(obj.batch);

    // Only invalid targets: nobody to notify
    assert!(parse("bulkvs://u:p@1111111111/55").targets.is_empty());

    // ?from= turns the host into a target
    let obj = parse("bulkvs://15551231234?user=u&password=p&from=14051231234");
    assert_eq!(obj.source, "14051231234");
    assert_eq!(obj.targets, vec!["15551231234"]);

    let obj = parse(&format!("bulkvs://?user=a&password=b&from={}&to={}", "5".repeat(11), "7".repeat(13)));
    assert_eq!(obj.targets, vec!["7".repeat(13)]);
  }

  #[test]
  fn test_service_details() {
    let details = BulkVs::static_details();
    assert_eq!(details.service_name, "BulkVS");
    assert_eq!(details.protocols, vec!["bulkvs"]);
    let obj = parse("bulkvs://u:p@14051231234");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send_per_target() {
    let server = MockServer::start().await;
    for to in ["15551231234", "15555555555"] {
      Mock::given(method("POST"))
        .and(path("/api/v1.0/messageSend"))
        // abcd:mypass123
        .and(header("Authorization", "Basic YWJjZDpteXBhc3MxMjM="))
        .and(header("Accept", "application/json"))
        .and(header("Content-Type", "application/json"))
        .and(body_json(json!({ "From": "14051231234", "To": to, "Message": "title\r\nbody" })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("bulkvs://abcd:mypass123@1 (405) 123 1234/+1(555) 123-1234/1555 5555555/12?batch=n").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v1.0/messageSend"))
      .and(body_json(json!({ "From": "14051231234", "To": ["15551231234", "15555555555"], "Message": "body" })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("bulkvs://u:p@14051231234/15551231234/15555555555?batch=y").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url("bulkvs://u:p@1111111111/55").unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_only_200_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201)).expect(2).mount(&server).await;
    let svc = from_url("bulkvs://u:p@14051231234/15551231234/15555555555").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }
}
