use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Routing groups (`?route=`).
const ROUTING_GROUPS: [&str; 3] = ["ECONOMY", "STANDARD", "PREMIUM"];

/// Most numbers one request may carry in batch mode.
const DEFAULT_BATCH_SIZE: usize = 4000;

/// BulkSMS signs in with an API Token: the Token ID is the URL user and the
/// Token Secret the password (`bulksms://{token_id}:{token_secret}@{targets}`).
/// The old user/password naming is the same thing, so both keep working.
pub struct BulkSms {
  user: Option<String>,
  password: Option<String>,
  source: Option<String>,
  route: String,
  unicode: bool,
  batch: bool,
  targets: Vec<String>,
  groups: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// `^(@?(?P<group>[A-Z0-9_-]+))$` (case-insensitive)
fn group_name(target: &str) -> Option<&str> {
  let name = target.strip_prefix('@').unwrap_or(target);
  (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then_some(name)
}

impl BulkSms {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (user, password) = (url.user.clone(), url.password.clone());

    let mut raw_targets = vec![url.host.clone().unwrap_or_default()];
    raw_targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }

    let source = match url.get("from").filter(|s| !s.is_empty()) {
      Some(from) => match is_phone_no(from, 10) {
        Some(p) => Some(format!("+{}", p.full)),
        None => {
          tracing::warn!("The Account (From) Phone # specified ({}) is invalid.", from);
          return None;
        }
      },
      None => None,
    };

    let route = url.get("route").filter(|r| !r.is_empty()).map(str::to_uppercase).unwrap_or_else(|| "STANDARD".to_string());
    if !ROUTING_GROUPS.contains(&route.as_str()) {
      tracing::warn!("The route specified ({}) is invalid.", route);
      return None;
    }

    let mut targets = Vec::new();
    let mut groups = Vec::new();
    for target in parse_phone_no(&raw_targets) {
      if let Some(p) = is_phone_no(&target, 10) {
        targets.push(format!("+{}", p.full));
        continue;
      }
      if let Some(group) = group_name(&target)
        && !target.chars().all(|c| c.is_ascii_digit())
      {
        groups.push(group.to_string());
        continue;
      }
      tracing::warn!("Dropped invalid phone # and/or Group ({}) specified.", target);
    }

    Some(Self {
      user,
      password,
      source,
      route,
      unicode: url.get("unicode").map(parse_bool).unwrap_or(true),
      batch: url.get("batch").map(parse_bool).unwrap_or(false),
      targets,
      groups,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "BulkSMS",
      service_url: Some("https://bulksms.com/"),
      setup_url: Some("https://appriseit.com/services/bulksms/"),
      protocols: vec!["bulksms"],
      description: "Send SMS via BulkSMS.",
      attachment_support: false,
    }
  }

  /// Every request's `to` value: single numbers or batches, then groups.
  fn recipients(&self) -> Vec<(Value, String)> {
    let mut out: Vec<(Value, String)> = if self.batch {
      self.targets.chunks(DEFAULT_BATCH_SIZE).map(|c| (json!(c), format!("{} targets", c.len()))).collect()
    } else {
      self.targets.iter().map(|t| (json!(t), t.clone())).collect()
    };
    out.extend(self.groups.iter().map(|g| (json!({ "type": "GROUP", "name": g }), g.clone())));
    out
  }
}

#[async_trait]
impl Notify for BulkSms {
  fn schemas(&self) -> &[&str] {
    &["bulksms"]
  }
  fn service_name(&self) -> &str {
    "BulkSMS"
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
    let (Some(user), Some(password)) = (self.user.as_deref().filter(|u| !u.is_empty()), self.password.as_deref().filter(|p| !p.is_empty())) else {
      tracing::warn!("There were no valid login credentials provided");
      return Ok(false);
    };
    if self.targets.is_empty() && self.groups.is_empty() {
      tracing::warn!("There are no BulkSMS targets to notify");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = format!("{}/v1/messages", service_base("https://api.bulksms.com"));
    let mut has_error = false;

    for (to, p_target) in self.recipients() {
      // Skip a target that already accepted this message so a retry does
      // not deliver it twice
      let key = to.to_string();
      if crate::delivery::is_delivered(&key, false) {
        continue;
      }

      let mut payload = json!({
        "to": to,
        "body": ctx.body,
        "routingGroup": self.route,
        "encoding": if self.unicode { "UNICODE" } else { "TEXT" },
        "deliveryReports": "ERRORS",
      });
      if let Some(source) = &self.source {
        payload["from"] = json!(source);
      }

      crate::notify::throttle().await;
      let resp = match client.post(&notify_url).header("User-Agent", APP_ID).basic_auth(user, Some(password)).json(&payload).send().await {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending BulkSMS: to {}", p_target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };

      let status = resp.status().as_u16();
      if status != 200 && status != 201 {
        tracing::warn!("Failed to send BulkSMS notification to {}: error={}.", p_target, status);
        tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent BulkSMS notification to {}.", p_target);
      crate::delivery::mark_delivered(&key, false);
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

  fn parse(url: &str) -> BulkSms {
    BulkSms::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      format!("bulksms://{}:{}@admin?route=invalid", "a".repeat(10), "b".repeat(10)),
      // invalid source number
      format!("bulksms://_?user={}&password={}&from={}", "a".repeat(10), "b".repeat(10), "5".repeat(3)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // These load but have nothing to send (no credentials or targets)
      "bulksms://".to_string(),
      "bulksms://:@/".to_string(),
      format!("bulksms://{}@12345678", "a".repeat(10)),
      format!("bulksms://{}:{}@{}", "a".repeat(10), "b".repeat(10), "3".repeat(5)),
      format!("bulksms://{}:{}@123/{}/abcd/", "a".repeat(5), "b".repeat(10), "3".repeat(11)),
      format!("bulksms://{}:{}@{}?batch=y&unicode=n", "b".repeat(5), "c".repeat(10), "4".repeat(11)),
      format!("bulksms://{}:{}@123456/{}", "a".repeat(10), "b".repeat(10), "4".repeat(11)),
      format!("bulksms://{}:{}@{}", "a".repeat(10), "b".repeat(10), "5".repeat(11)),
      format!("bulksms://{}:{}@admin?route=premium", "a".repeat(10), "b".repeat(10)),
      format!("bulksms://_?user={}&password={}&from={}", "a".repeat(10), "b".repeat(10), "5".repeat(11)),
      format!("bulksms://_?user={}&password={}&from={}&to={}", "a".repeat(10), "b".repeat(10), "5".repeat(11), "7".repeat(13)),
      format!("bulksms://{}:{}@{}", "a".repeat(10), "b".repeat(10), "a".repeat(3)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse("bulksms://abcd:mypass123@+1(555) 123-1234/1555 5555555/group/12/@12?batch=n");
    assert_eq!(obj.user.as_deref(), Some("abcd"));
    assert_eq!(obj.password.as_deref(), Some("mypass123"));
    assert_eq!(obj.targets, vec!["+15551231234", "+15555555555"]);
    // "12" is dropped; "@12" is a group
    assert_eq!(obj.groups, vec!["group", "12"]);
    assert_eq!(obj.route, "STANDARD");
    assert!(obj.unicode);
    assert!(!obj.batch);
    assert!(obj.source.is_none());

    let obj = parse("bulksms://a:b@15551231234?route=economy&unicode=no&from=+1 555 123 4567&batch=yes");
    assert_eq!(obj.route, "ECONOMY");
    assert!(!obj.unicode);
    assert!(obj.batch);
    assert_eq!(obj.source.as_deref(), Some("+15551234567"));

    // An empty route or from falls back to the defaults
    let obj = parse("bulksms://a:b@15551231234?route=&from=");
    assert_eq!(obj.route, "STANDARD");
    assert!(obj.source.is_none());

    // The "_" host placeholder is a valid group name upstream
    assert_eq!(parse("bulksms://_?user=a&password=b").groups, vec!["_"]);
    assert_eq!(parse("bulksms://a:b@123/33333333333/abcd/").groups, vec!["abcd"]);
  }

  #[test]
  fn test_api_token_credentials() {
    // A Token ID and Token Secret shaped like the ones BulkSMS issues; the
    // secret is URL encoded since it contains a #
    let token_id = "BBDE1B476E03498AA768F66A286AABDC-01-B";
    let token_secret = "9jSbVDK20!MXdfRGiIIFu#ffUE8*S";
    let encoded = urlencoding::encode(token_secret);
    for url in [
      format!("bulksms://{}:{}@15551231234", token_id, encoded),
      format!("bulksms://15551231234?user={}&password={}", token_id, encoded),
      format!("bulksms://15551231234?user={}&pass={}", token_id, encoded),
      format!("bulksms://{}@15551231234?user={}", encoded, token_id),
    ] {
      let obj = parse(&url);
      assert_eq!(obj.user.as_deref(), Some(token_id), "{}", url);
      assert_eq!(obj.password.as_deref(), Some(token_secret), "{}", url);
      assert_eq!(obj.targets, vec!["+15551231234"]);
    }
  }

  #[test]
  fn test_service_details() {
    let details = BulkSms::static_details();
    assert_eq!(details.service_name, "BulkSMS");
    assert_eq!(details.protocols, vec!["bulksms"]);
    let obj = parse("bulksms://a:b@15551231234");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send_per_target_and_group() {
    let server = MockServer::start().await;
    let expected = [json!("+15551231234"), json!("+15555555555"), json!({"type": "GROUP", "name": "group"}), json!({"type": "GROUP", "name": "12"})];
    for to in &expected {
      Mock::given(method("POST"))
        .and(path("/v1/messages"))
        // abcd:mypass123
        .and(header("Authorization", "Basic YWJjZDpteXBhc3MxMjM="))
        .and(header("Content-Type", "application/json"))
        .and(body_json(json!({
          "to": to, "body": "title\r\nbody", "routingGroup": "STANDARD", "encoding": "UNICODE", "deliveryReports": "ERRORS"
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("bulksms://abcd:mypass123@+1(555) 123-1234/1555 5555555/group/12/@12?batch=n").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/messages"))
      .and(body_json(json!({
        "to": ["+15551231234", "+15555555555"], "body": "body", "routingGroup": "ECONOMY",
        "encoding": "TEXT", "deliveryReports": "ERRORS", "from": "+15551234567"
      })))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/v1/messages"))
      .and(body_json(json!({
        "to": {"type": "GROUP", "name": "grp"}, "body": "body", "routingGroup": "ECONOMY",
        "encoding": "TEXT", "deliveryReports": "ERRORS", "from": "+15551234567"
      })))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("bulksms://a:b@+1(555) 123-1234/1555 5555555/@grp?batch=y&from=15551234567&unicode=no&route=economy").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_without_credentials_or_targets() {
    for url in ["bulksms://", "bulksms://:@/", "bulksms://aaaaaaaaaa@12345678", "bulksms://a:b@33333"] {
      let svc = from_url(url).unwrap();
      assert!(!svc.send(&ctx("body")).await.unwrap(), "{}", url);
    }
  }

  #[tokio::test]
  async fn test_send_failure_continues() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(ResponseTemplate::new(500)).expect(2).mount(&server).await;
    let svc = from_url("bulksms://a:b@15551231234/15555555555").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_accepted_targets() {
    let server = MockServer::start().await;
    let messages = || Mock::given(method("POST")).and(path("/v1/messages"));
    messages().and(body_partial_json(json!({"to": "+15551231234"}))).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    // The group fails once, then accepts the retry
    let group = json!({"to": {"type": "GROUP", "name": "grp"}});
    messages().and(body_partial_json(&group)).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    messages().and(body_partial_json(&group)).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("bulksms://a:b@15551231234/@grp?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("body"))).await.is_success());
  }

  #[tokio::test]
  async fn test_send_only_200_and_201_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(ResponseTemplate::new(202)).expect(1).mount(&server).await;
    let svc = from_url("bulksms://a:b@15551231234").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }
}
