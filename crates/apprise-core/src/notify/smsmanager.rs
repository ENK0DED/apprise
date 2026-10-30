use crate::error::NotifyError;
use crate::notify::sms_targets::{is_phone_no, parse_phone_no, unquoted_host, url_credentials};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use async_trait::async_trait;

const GATEWAYS: &[&str] = &["high", "economy", "low", "direct"];

/// Recipients per request in batch mode.
const DEFAULT_BATCH_SIZE: usize = 4000;

pub struct SmsManager {
  apikey: String,
  sender: Option<String>,
  targets: Vec<String>,
  gateway: String,
  batch: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// `urllib.parse.urlencode()` as requests applies it to `params=`.
fn query_encode(pairs: &[(&str, &str)]) -> String {
  pairs
    .iter()
    .map(|(k, v)| format!("{}={}", urlencoding::encode(k).replace("%20", "+"), urlencoding::encode(v).replace("%20", "+")))
    .collect::<Vec<_>>()
    .join("&")
}

impl SmsManager {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (url_user, _) = url_credentials(url);
    // smsmgr://apikey@target/targets
    let qs = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());
    let apikey = qs("key").map(str::to_string).or_else(|| url_user.clone()).unwrap_or_default();
    // Upstream validate_regex(): not blank and no leading whitespace
    if apikey.chars().next().is_none_or(char::is_whitespace) {
      tracing::warn!("An invalid API Key was specified.");
      return None;
    }
    let apikey = apikey.trim().to_string();

    let gateway = qs("gateway").map(str::to_lowercase).unwrap_or_else(|| GATEWAYS[0].to_string());
    if !GATEWAYS.contains(&gateway.as_str()) {
      tracing::warn!("The Gateway specified ({}) is invalid.", gateway);
      return None;
    }
    // Sender names are capped at 11 characters
    let sender = qs("from").or_else(|| qs("sender")).map(|s| s.chars().take(11).collect());
    let batch = url.get("batch").map(parse_bool).unwrap_or(false);

    // The host is the first target
    let mut entries = parse_phone_no(unquoted_host(url), false);
    entries.extend(url.path_parts.iter().cloned());
    if let Some(to) = qs("to") {
      entries.extend(parse_phone_no([to], false));
    }
    let mut targets = Vec::new();
    for target in parse_phone_no(&entries, false) {
      match is_phone_no(&target, 9) {
        Some(phone) => targets.push(if target.trim_start().starts_with('+') { format!("+{}", phone.full) } else { phone.full }),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self { apikey, sender, targets, gateway, batch, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SMS Manager",
      service_url: Some("https://smsmanager.cz"),
      setup_url: Some("https://appriseit.com/services/sms_manager/"),
      protocols: vec!["smsmgr", "smsmanager"],
      description: "Send SMS via SMS Manager (CZ).",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for SmsManager {
  fn schemas(&self) -> &[&str] {
    &["smsmgr", "smsmanager"]
  }
  fn service_name(&self) -> &str {
    "SMS Manager"
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
      tracing::warn!("There are no SMS Manager targets to notify");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let base = format!("{}/Send", service_base("https://http-api.smsmanager.cz"));
    let batch_size = if self.batch { DEFAULT_BATCH_SIZE } else { 1 };
    let mut has_error = false;

    for chunk in self.targets.chunks(batch_size) {
      let number = chunk.join(";");
      let p_target = if batch_size == 1 { number.clone() } else { format!("{} targets", chunk.len()) };
      let mut params = vec![("apikey", self.apikey.as_str()), ("gateway", self.gateway.as_str()), ("number", number.as_str()), ("message", ctx.body.as_str())];
      if let Some(sender) = &self.sender {
        params.push(("sender", sender.as_str()));
      }
      tracing::debug!("SMS Manager POST URL: {} (cert_verify={})", base, self.verify_certificate);
      match client.get(format!("{}?{}", base, query_encode(&params))).header("User-Agent", APP_ID).send().await {
        Ok(resp) if resp.status() == reqwest::StatusCode::OK => tracing::info!("Sent SMS Manager notification to {}.", p_target),
        Ok(resp) => {
          let status = resp.status();
          tracing::warn!(
            "Failed to send SMS Manager notification to {}: {}error={}.",
            p_target,
            status.canonical_reason().map(|r| format!("{}, ", r)).unwrap_or_default(),
            status.as_u16()
          );
          tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending SMS Manager: to {} ", p_target);
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
  use wiremock::matchers::{method, path, query_param, query_param_is_missing};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn mgr(url: &str) -> SmsManager {
    SmsManager::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec!["smsmgr://".to_string(), "smsmgr://:@/".to_string(), format!("smsmgr://{}@{}?gateway=invalid", "a".repeat(10), "1".repeat(11))];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // Too short a number (notify fails)
      format!("smsmgr://{}@{}", "b".repeat(10), "3".repeat(5)),
      format!("smsmgr://{}@123/{}/abcd/+{}", "z".repeat(10), "3".repeat(11), "4".repeat(11)),
      format!("smsmgr://{}@{}?batch=y", "b".repeat(5), "4".repeat(11)),
      format!("smsmgr://{}@{}?gateway=low", "a".repeat(10), "1".repeat(11)),
      format!("smsmgr://{}@{}?gateway=DIRECT", "a".repeat(10), "1".repeat(11)),
      format!("smsmgr://{}?key={}&from=user", "1".repeat(11), "a".repeat(10)),
      format!("smsmgr://_?to={},{}&key={}&sender={}", "1".repeat(11), "2".repeat(11), "a".repeat(10), "5".repeat(3)),
      format!("smsmanager://{}@{}", "a".repeat(10), "1".repeat(11)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let m = mgr(&format!("smsmgr://{}@123/{}/abcd/+{}", "z".repeat(10), "3".repeat(11), "4".repeat(11)));
    assert_eq!(m.apikey, "z".repeat(10));
    assert_eq!(m.targets, vec!["3".repeat(11), format!("+{}", "4".repeat(11))]);
    assert_eq!(m.gateway, "high");
    assert!(!m.batch);
    assert_eq!(m.sender, None);

    let m = mgr(&format!("smsmgr://_?to={},{}&key={}&sender=MySenderNameXYZ&gateway=LOW&batch=yes", "1".repeat(11), "2".repeat(11), "a".repeat(10)));
    assert_eq!(m.apikey, "a".repeat(10));
    assert_eq!(m.targets, vec!["1".repeat(11), "2".repeat(11)]);
    assert_eq!(m.sender.as_deref(), Some("MySenderNam"));
    assert_eq!(m.gateway, "low");
    assert!(m.batch);
    // from= wins over sender=
    assert_eq!(mgr(&format!("smsmgr://{}?key=k&from=a&sender=b", "1".repeat(11))).sender.as_deref(), Some("a"));
  }

  #[test]
  fn test_service_details() {
    let d = SmsManager::static_details();
    assert_eq!(d.service_name, "SMS Manager");
    assert!(d.protocols.contains(&"smsmgr") && d.protocols.contains(&"smsmanager"));
  }

  #[tokio::test]
  async fn test_send_per_target() {
    // Matches upstream pycapture output
    let server = MockServer::start().await;
    for number in ["+15551231234", "15555555555"] {
      Mock::given(method("GET"))
        .and(path("/Send"))
        .and(query_param("apikey", "keyabc"))
        .and(query_param("gateway", "high"))
        .and(query_param("number", number))
        .and(query_param("message", "title\r\nbody"))
        .and(query_param_is_missing("sender"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("smsmgr://keyabc@+15551231234/15555555555?batch=n").unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch_with_sender() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(query_param("gateway", "low"))
      .and(query_param("number", "+15551231234;15551231235"))
      .and(query_param("message", "Hi there~*"))
      .and(query_param("sender", "MySenderNam"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("smsmgr://keyabc@+15551231234/15551231235?from=MySenderNameXYZ&gateway=LOW&batch=yes").unwrap();
    let ctx = NotifyContext { body: "Hi there~*".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    let req = requests.iter().find(|r| r.url.path() == "/Send").unwrap();
    assert_eq!(req.url.query().unwrap(), "apikey=keyabc&gateway=low&number=%2B15551231234%3B15551231235&message=Hi+there~%2A&sender=MySenderNam");
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(!mgr(&format!("smsmgr://{}@{}", "b".repeat(10), "3".repeat(5))).send(&ctx).await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("GET")).and(query_param("number", "15551231234")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("GET")).and(query_param("number", "15551231235")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("smsmgr://keyabc@15551231234/15551231235").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }
}
