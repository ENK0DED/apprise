use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::phone::{is_phone_no, parse_phone_no};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;

/// SMS modes, in the order upstream matches a `?mode=` prefix against them.
const MODES: [&str; 3] = ["bulksms", "premium", "sandbox"];

/// Default sender (`?from=`).
const DEFAULT_SENDER: &str = "AFRICASTKNG";

/// Most numbers one request may carry in batch mode.
const DEFAULT_BATCH_SIZE: usize = 50;

/// Recipient `statusCode` values meaning the message was accepted
/// (Processed, Sent, Queued). These are not HTTP status codes.
const RECIPIENT_OK: [i64; 3] = [100, 101, 102];

pub struct AfricasTalking {
  appuser: String,
  apikey: String,
  sender: String,
  mode: &'static str,
  batch: bool,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

fn is_token(s: &str) -> bool {
  // ^[A-Z0-9_-]+$ (case-insensitive)
  !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl AfricasTalking {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let appuser = url.user.clone().unwrap_or_default();
    if !is_token(&appuser) {
      tracing::warn!("The Africas Talking appuser specified ({}) is invalid.", appuser);
      return None;
    }

    let mut raw_targets = Vec::new();
    let apikey = match url.get("apikey").filter(|k| !k.is_empty()) {
      Some(key) => {
        // The host is a phone number (target) in this case
        raw_targets.push(url.host.clone().unwrap_or_default());
        key.to_string()
      }
      None => url.host.clone().unwrap_or_default(),
    };
    if !is_token(&apikey) {
      tracing::warn!("The Africas Talking apikey specified is invalid.");
      return None;
    }

    raw_targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }

    let sender = url.get("from").filter(|s| !s.is_empty()).unwrap_or(DEFAULT_SENDER).to_string();

    let mode = match url.get("mode").filter(|m| !m.is_empty()) {
      Some(m) => {
        let m = m.to_lowercase();
        match MODES.iter().find(|v| v.starts_with(&m)) {
          Some(v) => *v,
          None => {
            tracing::warn!("The Africas Talking mode specified ({}) is invalid.", m);
            return None;
          }
        }
      }
      None => MODES[0],
    };

    let mut targets = Vec::new();
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10) {
        // Carry forward '+' if defined, otherwise do not
        Some(p) => targets.push(if target.trim_start().starts_with('+') { format!("+{}", p.full) } else { p.full }),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }

    Some(Self {
      appuser,
      apikey,
      sender,
      mode,
      batch: url.get("batch").map(parse_bool).unwrap_or(false),
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Africas Talking",
      service_url: Some("https://africastalking.com/"),
      setup_url: Some("https://appriseit.com/services/africas_talking/"),
      protocols: vec!["atalk"],
      description: "Send SMS via Africa's Talking.",
      attachment_support: false,
    }
  }

  fn notify_url(&self) -> String {
    let base = match self.mode {
      "premium" => "https://content.africastalking.com",
      "sandbox" => "https://api.sandbox.africastalking.com",
      _ => "https://api.africastalking.com",
    };
    format!("{}/version1/messaging", service_base(base))
  }
}

/// Upstream `_accepted()`: the numbers in `batch` the service accepted.
///
/// - An unreadable body trusts the 2xx reply for the whole batch.
/// - Otherwise a number counts only when its `Recipients` entry has a
///   success `statusCode` (Processed, Sent or Queued).
fn accepted(content: &[u8], batch: &[String]) -> Vec<String> {
  let content: Value = serde_json::from_slice(content).unwrap_or(Value::Null);
  let Some(recipients) = content.get("SMSMessageData").and_then(|d| d.get("Recipients")).and_then(Value::as_array) else {
    // Nothing to inspect; the 2xx reply is all we have to go on
    return batch.to_vec();
  };

  // The digits of every number in the reply, and of the accepted ones
  let mut numbers = HashSet::new();
  let mut accepted = Vec::new();
  for entry in recipients {
    let Some(obj) = entry.as_object() else { continue };
    let raw_number = match obj.get("number") {
      Some(Value::String(s)) => s.clone(),
      Some(v) => v.to_string(),
      None => String::new(),
    };
    let number: String = raw_number.chars().filter(char::is_ascii_digit).collect::<String>().trim_start_matches('0').to_string();
    numbers.insert(number.clone());

    let code = match obj.get("statusCode") {
      Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
      Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
      Some(Value::Bool(b)) => Some(*b as i64),
      _ => None,
    };
    if code.is_some_and(|c| RECIPIENT_OK.contains(&c)) {
      accepted.push(number);
      continue;
    }

    tracing::warn!(
      "Africas Talking rejected {}: {}statusCode={}.",
      raw_number,
      recipient_status(code),
      obj.get("statusCode").map(|v| v.to_string()).unwrap_or_default()
    );
  }

  // Targets are stored as digits with an optional leading "+"
  let digits: Vec<(&String, &str)> = batch.iter().map(|t| (t, t.trim_start_matches(['+', '0']))).collect();

  // Targets whose own number appears in the reply, accepted or not
  let exact: HashSet<&String> = digits.iter().filter(|(_, d)| numbers.contains(*d)).map(|(t, _)| *t).collect();

  let mut result: HashSet<&String> = HashSet::new();
  for number in &accepted {
    // An exact match always identifies the right target
    let mut matched: Vec<&String> = digits.iter().filter(|(_, d)| d == number).map(|(t, _)| *t).collect();
    if matched.is_empty() {
      // Replies may change local numbers to international form. Match
      // ending digits only when they identify one target.
      matched = digits.iter().filter(|(t, d)| !exact.contains(t) && (number.ends_with(d) || d.ends_with(number.as_str()))).map(|(t, _)| *t).collect();
      if matched.len() != 1 {
        continue;
      }
    }
    result.extend(matched);
  }

  // Keep the batch order
  batch.iter().filter(|t| result.contains(t)).cloned().collect()
}

fn recipient_status(code: Option<i64>) -> &'static str {
  match code {
    Some(100) => "Processed, ",
    Some(101) => "Sent, ",
    Some(102) => "Queued, ",
    Some(401) => "Risk Hold, ",
    Some(402) => "Invalid Sender ID, ",
    Some(403) => "Invalid Phone Number, ",
    Some(404) => "Unsupported Number Type, ",
    Some(405) => "Insufficient Balance, ",
    Some(406) => "User In Blacklist, ",
    Some(407) => "Could Not Route, ",
    Some(409) => "Do Not Disturb Rejection, ",
    Some(500) => "Internal Server Error, ",
    Some(501) => "Gateway Error, ",
    Some(502) => "Rejected By Gateway, ",
    _ => "",
  }
}

#[async_trait]
impl Notify for AfricasTalking {
  fn schemas(&self) -> &[&str] {
    &["atalk"]
  }
  fn service_name(&self) -> &str {
    "Africas Talking"
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
      tracing::warn!("There are no Africas Talking recipients to notify");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let notify_url = self.notify_url();
    let batch_size = if self.batch { DEFAULT_BATCH_SIZE } else { 1 };
    let mut has_error = false;

    // Retry only recipients that have not accepted the message
    let pending: Vec<String> = self.targets.iter().filter(|t| !crate::delivery::is_delivered(t, false)).cloned().collect();

    for batch in pending.chunks(batch_size) {
      let to = batch.join(",");
      let payload = [("username", self.appuser.as_str()), ("to", to.as_str()), ("from", self.sender.as_str()), ("message", ctx.body.as_str())];
      let p_target = if batch_size == 1 { batch[0].clone() } else { format!("{} target(s)", batch.len()) };

      crate::notify::throttle().await;
      let resp = match client
        .post(&notify_url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("apiKey", self.apikey.as_str())
        .form(&payload)
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Africas Talking notification to {}.", p_target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };

      // The service answers with 201 Created; accept any 2xx
      let status = resp.status();
      let content = resp.bytes().await.unwrap_or_default();
      if !status.is_success() {
        tracing::warn!("Failed to send Africas Talking notification to {}: error={}.", p_target, status.as_u16());
        tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&content[..content.len().min(2000)]));
        has_error = true;
        continue;
      }

      let delivered = accepted(&content, batch);
      if delivered.len() != batch.len() {
        // A retry sends to the rest again
        has_error = true;
      }
      for target in &delivered {
        crate::delivery::mark_delivered(target, false);
      }
      if !delivered.is_empty() {
        tracing::info!("Sent Africas Talking notification to {}/{} recipient(s).", delivered.len(), batch.len());
      }
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_string, body_string_contains, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> AfricasTalking {
    AfricasTalking::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "atalk://",
      "atalk://:@/",
      // invalid apikey
      "atalk://user@^/",
      // invalid mode
      "atalk://user@apikey/+44444444444?mode=invalid",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      // invalid number; loads but has no one to notify
      "atalk://user@apikey/33333",
      "atalk://user@apikey/123/33333333333/abcd/+44444444444",
      "atalk://user@apikey/+44444444444?batch=y",
      "atalk://user@apikey/+44444444444?mode=s",
      "atalk://user@apikey/+44444444444?mode=PREM",
      "atalk://11111111111?apikey=key&user=user&from=FROMUSER",
      "atalk://_?user=user&to=11111111111,22222222222&key=bbbbbbbbbb&from=5555555555555",
      "atalk://user@apikey/11111111111/",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = parse("atalk://user@apikey/123/33333333333/abcd/+44444444444");
    assert_eq!(obj.appuser, "user");
    assert_eq!(obj.apikey, "apikey");
    assert_eq!(obj.targets, vec!["33333333333", "+44444444444"]);
    assert_eq!(obj.sender, "AFRICASTKNG");
    assert_eq!(obj.mode, "bulksms");
    assert!(!obj.batch);

    let obj = parse("atalk://user@apikey/+44444444444?mode=PREM&batch=y");
    assert_eq!(obj.mode, "premium");
    assert!(obj.batch);
    assert_eq!(parse("atalk://user@apikey/+44444444444?mode=s").mode, "sandbox");

    // ?apikey= turns the host into a target
    let obj = parse("atalk://11111111111?apikey=key&user=user&from=FROMUSER");
    assert_eq!(obj.apikey, "key");
    assert_eq!(obj.appuser, "user");
    assert_eq!(obj.sender, "FROMUSER");
    assert_eq!(obj.targets, vec!["11111111111"]);

    // ?key= is not an alias: the host stays the apikey
    let obj = parse("atalk://_?user=user&to=11111111111,22222222222&key=bbbbbbbbbb&from=5555555555555");
    assert_eq!(obj.apikey, "_");
    assert_eq!(obj.sender, "5555555555555");
    assert_eq!(obj.targets, vec!["11111111111", "22222222222"]);

    assert!(parse("atalk://user@apikey/33333").targets.is_empty());
  }

  #[test]
  fn test_notify_url_per_mode() {
    assert_eq!(parse("atalk://u@k/11111111111").notify_url(), "https://api.africastalking.com/version1/messaging");
    assert_eq!(parse("atalk://u@k/11111111111?mode=premium").notify_url(), "https://content.africastalking.com/version1/messaging");
    assert_eq!(parse("atalk://u@k/11111111111?mode=sandbox").notify_url(), "https://api.sandbox.africastalking.com/version1/messaging");
  }

  #[test]
  fn test_service_details() {
    let details = AfricasTalking::static_details();
    assert_eq!(details.service_name, "Africas Talking");
    assert_eq!(details.protocols, vec!["atalk"]);
    assert!(!details.attachment_support);
    let obj = parse("atalk://u@k/11111111111");
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.body_maxlen(), 160);
  }

  #[tokio::test]
  async fn test_send_one_request_per_target() {
    let server = MockServer::start().await;
    for (to, expect) in [("%2B15551231234", 1), ("15555555555", 1)] {
      Mock::given(method("POST"))
        .and(path("/version1/messaging"))
        .and(header("apiKey", "my-api-key"))
        .and(header("Accept", "application/json"))
        .and(header("Content-Type", "application/x-www-form-urlencoded"))
        .and(body_string(format!("username=my-app-user&to={}&from=AFRICASTKNG&message=title%0D%0Abody", to)))
        .respond_with(ResponseTemplate::new(200))
        .expect(expect)
        .mount(&server)
        .await;
    }
    let svc = from_url("atalk://my-app-user@my-api-key/+1(555) 123-1234/1555 5555555/12?batch=n").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("title\r\nbody"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/version1/messaging"))
      .and(header("apiKey", "my-api-key"))
      .and(body_string("username=my-app-user&to=%2B15551231234%2C15555555555&from=TEST&message=body"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("atalk://+1(555) 123-1234/1555 5555555/12?user=my-app-user&apikey=my-api-key&batch=y&from=TEST").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_no_targets() {
    let svc = from_url("atalk://user@apikey/33333").unwrap();
    assert!(!svc.send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_http_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/version1/messaging")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    let svc = from_url("atalk://user@apikey/11111111111/").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_accepted_recipients() {
    let server = MockServer::start().await;
    let messaging = || Mock::given(method("POST")).and(path("/version1/messaging"));
    messaging().and(body_string_contains("to=%2B15551231234")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    // The second number fails once, then accepts the retry
    messaging().and(body_string_contains("to=%2B15555555555")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    messaging().and(body_string_contains("to=%2B15555555555")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("atalk://user@apikey/+15551231234/+15555555555?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("body"))).await.is_success());
  }

  async fn send_with_reply(url: &str, reply: &str) -> bool {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/version1/messaging")).respond_with(ResponseTemplate::new(201).set_body_string(reply)).mount(&server).await;
    let svc = from_url(url).unwrap();
    crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap()
  }

  #[tokio::test]
  async fn test_send_created_is_success() {
    let reply = r#"{"SMSMessageData": {"Message": "Sent to 2/2", "Recipients": [
      {"number": "+15551231234", "statusCode": 101}, {"number": "+15555555555", "statusCode": "102"}]}}"#;
    assert!(send_with_reply("atalk://user@apikey/+15551231234/+15555555555?batch=yes", reply).await);
  }

  #[tokio::test]
  async fn test_send_recipient_failure() {
    let reply = r#"{"SMSMessageData": {"Recipients": [
      {"number": "+15551231234", "statusCode": 101}, {"number": "+15555555555", "statusCode": 403}]}}"#;
    assert!(!send_with_reply("atalk://user@apikey/+15551231234/+15555555555?batch=yes", reply).await);
  }

  #[tokio::test]
  async fn test_send_empty_recipients_is_failure() {
    let reply = r#"{"SMSMessageData": {"Message": "InvalidSenderId", "Recipients": []}}"#;
    assert!(!send_with_reply("atalk://user@apikey/+15551231234", reply).await);
  }

  #[tokio::test]
  async fn test_send_local_number_matches() {
    let reply = r#"{"SMSMessageData": {"Recipients": ["garbage", {"statusCode": null},
      {"number": "+254711000111", "statusCode": "abc"}, {"number": "+254711000111", "statusCode": 100}]}}"#;
    assert!(send_with_reply("atalk://user@apikey/0711000111", reply).await);
  }

  #[tokio::test]
  async fn test_send_unreadable_reply_trusts_http() {
    for reply in ["not json", "[]", r#"{"SMSMessageData": []}"#, r#"{"SMSMessageData": {"Recipients": {}}}"#] {
      assert!(send_with_reply("atalk://user@apikey/+15551231234", reply).await, "{}", reply);
    }
  }

  #[test]
  fn test_accepted_matching() {
    let batch = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    // A number from another country never marks a local one delivered
    let reply = br#"{"SMSMessageData": {"Recipients": [
      {"number": "+254711222333", "statusCode": 403}, {"number": "+255711222333", "statusCode": 101}]}}"#;
    assert_eq!(accepted(reply, &batch(&["0711222333", "+255711222333"])), vec!["+255711222333"]);

    // An accepted number that could mean two targets marks neither
    let reply = br#"{"SMSMessageData": {"Recipients": [{"number": "+254711222333", "statusCode": 101}]}}"#;
    assert!(accepted(reply, &batch(&["0711222333", "00711222333"])).is_empty());

    // Batch order is kept
    let reply = br#"{"SMSMessageData": {"Recipients": [
      {"number": "+15555555555", "statusCode": 101}, {"number": "+15551231234", "statusCode": 101}]}}"#;
    assert_eq!(accepted(reply, &batch(&["+15551231234", "+15555555555"])), vec!["+15551231234", "+15555555555"]);
  }
}
