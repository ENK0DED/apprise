use async_trait::async_trait;
use reqwest::multipart::{Form, Part};
use serde_json::{Map, Value};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::email::parse_list;
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::phone::is_phone_no;

/// SerwerSMS SMS/MMS (`serwersms://{user}:{password}@{sender}/{targets}`).
pub struct SerwerSms {
  user: String,
  password: String,
  sender: String,
  /// Target phone numbers (digits only)
  target_phones: Vec<String>,
  /// Target contact group IDs
  target_groups: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream's sender regex `^[a-z0-9][a-z0-9 _-]{0,10}$` (case-insensitive),
/// returning the stripped sender.
fn validate_sender(sender: &str) -> Option<String> {
  let mut chars = sender.chars();
  let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
  let rest: Vec<char> = chars.collect();
  let rest_ok = rest.len() <= 10 && rest.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-'));
  (first_ok && rest_ok).then(|| sender.trim().to_string())
}

/// Upstream's group regex `^\s*(\#|\%23)(?P<group>[0-9]+)\s*$`.
fn group_id(target: &str) -> Option<&str> {
  let trimmed = target.trim();
  let id = trimmed.strip_prefix('#').or_else(|| trimmed.strip_prefix("%23"))?;
  (!id.is_empty() && id.chars().all(|c| c.is_ascii_digit())).then_some(id)
}

/// Python truthiness of a JSON value.
fn is_truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) => false,
    Some(Value::Bool(b)) => *b,
    Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
    Some(Value::String(s)) => !s.is_empty(),
    Some(Value::Array(a)) => !a.is_empty(),
    Some(Value::Object(o)) => !o.is_empty(),
  }
}

impl SerwerSms {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let Some(user) = url.user.clone().filter(|u| !u.is_empty()) else {
      tracing::warn!("A SerwerSMS username must be specified.");
      return None;
    };
    let Some(password) = url.password.clone().filter(|p| !p.is_empty()) else {
      tracing::warn!("A SerwerSMS password must be specified.");
      return None;
    };

    // The sender name occupies the host; ?sender= wins over ?from=
    let sender = url
      .get("sender")
      .filter(|s| !s.is_empty())
      .or_else(|| url.get("from").filter(|f| !f.is_empty()))
      .map(unquote)
      .unwrap_or_else(|| url.host.clone().unwrap_or_default());
    let Some(sender) = validate_sender(&sender) else {
      tracing::warn!("A SerwerSMS sender name ({}) is invalid.", sender);
      return None;
    };

    let mut raw_targets = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      // Upstream's URLBase.parse_list() also unquotes each entry
      raw_targets.extend(parse_list(&[to]).iter().map(|t| unquote(t)));
    }

    let mut target_phones = Vec::new();
    let mut target_groups = Vec::new();
    for target in parse_list(&raw_targets) {
      if let Some(group) = group_id(&target) {
        target_groups.push(group.to_string());
      } else if let Some(phone) = is_phone_no(&target, 10) {
        target_phones.push(phone.full);
      } else {
        tracing::warn!("Dropped invalid SerwerSMS target: {}.", target);
      }
    }

    Some(Self { user, password, sender, target_phones, target_groups, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SerwerSMS",
      service_url: Some("https://serwersms.pl"),
      setup_url: Some("https://appriseit.com/services/serwersms/"),
      protocols: vec!["serwersms"],
      description: "Send SMS (or MMS with attachments) via SerwerSMS.",
      attachment_support: true,
    }
  }
}

/// One `file` part of an MMS request.
fn attachment_part(attachment: &Attachment) -> Part {
  let part = || Part::bytes(attachment.data.clone()).file_name(attachment.name.clone());
  part().mime_str(&attachment.mime_type).unwrap_or_else(|_| part())
}

#[async_trait]
impl Notify for SerwerSms {
  fn schemas(&self) -> &[&str] {
    &["serwersms"]
  }
  fn service_name(&self) -> &str {
    "SerwerSMS"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    true
  }
  fn body_maxlen(&self) -> usize {
    160
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.target_phones.is_empty() && self.target_groups.is_empty() {
      tracing::warn!("There are no SerwerSMS targets to notify.");
      return Ok(false);
    }

    // Attachments switch every call to the MMS endpoint
    let use_mms = !ctx.attachments.is_empty();
    let base = service_base("https://api2.serwersms.pl");
    let url = if use_mms { format!("{}/messages/send_mms", base) } else { format!("{}/messages/send_sms", base) };
    let client = build_client(self.verify_certificate)?;

    // (label, extra field) per call: phones first, then groups
    let calls = self
      .target_phones
      .iter()
      .map(|p| (format!("+{}", p), ("phone", format!("+{}", p))))
      .chain(self.target_groups.iter().map(|g| (format!("group {}", g), ("group_id", g.clone()))));

    let mut has_error = false;
    for (label, (key, value)) in calls {
      // A retry skips targets that already accepted the message
      if crate::delivery::is_delivered(&label, false) {
        continue;
      }
      let fields =
        [("username", self.user.clone()), ("password", self.password.clone()), ("text", ctx.body.clone()), ("sender", self.sender.clone()), (key, value)];

      let request = client.post(&url).header("User-Agent", APP_ID);
      let request = if use_mms {
        // Content-Type is left to reqwest so it carries the boundary
        let mut form = Form::new();
        for (key, value) in fields {
          form = form.text(key, value);
        }
        for attachment in &ctx.attachments {
          form = form.part("file", attachment_part(attachment));
        }
        request.multipart(form)
      } else {
        let payload: Map<String, Value> = fields.into_iter().map(|(k, v)| (k.to_string(), Value::String(v))).collect();
        request.header("Content-Type", "application/json").body(Value::Object(payload).to_string())
      };

      let kind = if use_mms { "MMS notification" } else { "notification" };
      crate::notify::throttle().await;
      let resp = match request.send().await {
        Ok(resp) => resp,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending SerwerSMS {} to {}.", kind, label);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };

      let status = resp.status();
      if status != reqwest::StatusCode::OK {
        tracing::warn!("Failed to send SerwerSMS {} to {}: error={}.", kind, label, status.as_u16());
        tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
        has_error = true;
        continue;
      }

      // API-level failures are reported in the response body
      let content: Value = resp.json().await.unwrap_or(Value::Null);
      if !is_truthy(content.get("success")) {
        match content.get("error").filter(|e| is_truthy(Some(e))) {
          Some(error) => {
            let code = error.get("code").map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
            let message = error.get("message").and_then(|m| m.as_str()).unwrap_or_default();
            tracing::warn!("Failed to send SerwerSMS {} to {}: API error {} - {}.", kind, label, code, message);
          }
          None => tracing::warn!("Failed to send SerwerSMS {} to {}: unexpected API response.", kind, label),
        }
        has_error = true;
        continue;
      }

      tracing::info!("Sent SerwerSMS {} to {}.", kind, label);
      crate::delivery::mark_delivered(&label, false);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use serde_json::json;
  use wiremock::matchers::{body_json, body_partial_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Option<SerwerSms> {
    SerwerSms::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn ok_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(r#"{"success": true}"#)
  }

  fn with_attachment(mut ctx: NotifyContext) -> NotifyContext {
    ctx.attachments.push(Attachment { name: "apprise-test.gif".into(), data: b"GIF89a".to_vec(), mime_type: "image/gif".into() });
    ctx
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "serwersms://",
      "serwersms://:@/",
      // Missing password
      "serwersms://user@SenderA/+48123456789",
      // Missing sender
      "serwersms://user:pass@/+48123456789",
      // Invalid sender names
      "serwersms://user:pass@!!invalid!!",
      "serwersms://user:pass@AAAAAAAAAAAA",
      "serwersms://user:pass@Sender?sender=_bad",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    // No targets: loads, but has nothing to notify
    let obj = parse("serwersms://user:pass@SenderA").unwrap();
    assert!(obj.target_phones.is_empty() && obj.target_groups.is_empty());

    let obj = parse("serwersms://user:pass@SenderA/+48123456789").unwrap();
    assert_eq!(obj.user, "user");
    assert_eq!(obj.password, "pass");
    assert_eq!(obj.sender, "SenderA");
    assert_eq!(obj.target_phones, vec!["48123456789"]);

    let obj = parse("serwersms://user:pass@SenderA/%23456").unwrap();
    assert_eq!(obj.target_groups, vec!["456"]);

    let obj = parse("serwersms://user:pass@SenderA/+48123456789/%23789/notaphone").unwrap();
    assert_eq!(obj.target_phones, vec!["48123456789"]);
    assert_eq!(obj.target_groups, vec!["789"]);

    let obj = parse("serwersms://user:pass@SenderA/+48111222333?to=%23999").unwrap();
    assert_eq!(obj.target_phones, vec!["48111222333"]);
    assert_eq!(obj.target_groups, vec!["999"]);

    // ?from= and ?sender= set the sender; ?sender= wins
    assert_eq!(parse("serwersms://user:pass@SenderA/+48111?from=NewSender").unwrap().sender, "NewSender");
    assert_eq!(parse("serwersms://user:pass@SenderA/+48111?from=Old&sender=New").unwrap().sender, "New");
    // Eleven characters with spaces, dashes and underscores
    assert_eq!(parse("serwersms://user:pass@S?sender=My%20App_-123").unwrap().sender, "My App_-123");
  }

  #[test]
  fn test_attributes() {
    let svc = from_url("serwersms://user:pass@SenderA").unwrap();
    assert_eq!(svc.body_maxlen(), 160);
    assert_eq!(svc.title_maxlen(), 0);
    assert!(svc.attachment_support());
  }

  #[tokio::test]
  async fn test_send_sms() {
    let server = MockServer::start().await;
    let base = json!({"username": "user", "password": "pass", "text": "body", "sender": "Sender"});
    for (key, value) in [("phone", "+15551234567"), ("group_id", "123")] {
      let mut expected = base.clone();
      expected[key] = json!(value);
      Mock::given(method("POST"))
        .and(path("/messages/send_sms"))
        .and(header("User-Agent", APP_ID))
        .and(header("Content-Type", "application/json"))
        .and(body_json(expected))
        .respond_with(ok_response())
        .expect(1)
        .mount(&server)
        .await;
    }

    let svc = from_url("serwersms://user:pass@Sender/%23123/15551234567").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());

    // Phones are sent before groups
    let requests = crate::notify::custom_json::requests_to(&server, "POST", "/messages/send_sms").await;
    assert!(requests[0].body_json::<Value>().unwrap().get("phone").is_some());
    assert!(requests[1].body_json::<Value>().unwrap().get("group_id").is_some());
  }

  #[tokio::test]
  async fn test_send_mms() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages/send_mms")).respond_with(ok_response()).expect(2).mount(&server).await;

    let svc = from_url("serwersms://user:pass@Sender/15551234567/%23500").unwrap();
    let mut ctx = with_attachment(ctx("title\r\nbody"));
    ctx.attachments.push(Attachment { name: "notes.txt".into(), data: b"plain text".to_vec(), mime_type: "text/plain".into() });
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let requests = crate::notify::custom_json::requests_to(&server, "POST", "/messages/send_mms").await;
    for (request, target) in requests.iter().zip(["name=\"phone\"\r\n\r\n+15551234567\r\n", "name=\"group_id\"\r\n\r\n500\r\n"]) {
      let content_type = request.headers.get("Content-Type").unwrap().to_str().unwrap();
      assert!(content_type.starts_with("multipart/form-data; boundary="));
      let body = String::from_utf8_lossy(&request.body).to_string();
      assert!(body.contains("name=\"username\"\r\n\r\nuser\r\n"));
      assert!(body.contains("name=\"password\"\r\n\r\npass\r\n"));
      assert!(body.contains("name=\"text\"\r\n\r\ntitle\r\nbody\r\n"));
      assert!(body.contains("name=\"sender\"\r\n\r\nSender\r\n"));
      assert!(body.contains(target), "missing {} in {}", target, body);
      assert!(body.contains("name=\"file\"; filename=\"apprise-test.gif\"\r\nContent-Type: image/gif\r\n\r\nGIF89a"));
      assert!(body.contains("name=\"file\"; filename=\"notes.txt\"\r\nContent-Type: text/plain\r\n\r\nplain text"));
    }
  }

  #[tokio::test]
  async fn test_send_failures() {
    // No targets: no request is made
    let svc = from_url("serwersms://u:p@Sender").unwrap();
    assert!(!svc.send(&ctx("msg")).await.unwrap());

    for response in [
      ResponseTemplate::new(500),
      ResponseTemplate::new(999),
      ResponseTemplate::new(200).set_body_string(r#"{"success": false, "error": {"code": 101, "message": "Auth fail"}}"#),
      ResponseTemplate::new(200).set_body_string(r#"{"success": false}"#),
      ResponseTemplate::new(200).set_body_string("not-json"),
      ResponseTemplate::new(200).set_body_string("null"),
    ] {
      for mms in [false, true] {
        let server = MockServer::start().await;
        let endpoint = if mms { "/messages/send_mms" } else { "/messages/send_sms" };
        Mock::given(method("POST")).and(path(endpoint)).respond_with(response.clone()).expect(1).mount(&server).await;
        let svc = from_url("serwersms://u:p@Sender/+48123456789").unwrap();
        let ctx = if mms { with_attachment(ctx("msg")) } else { ctx("msg") };
        assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
      }
    }

    // One success and one failure is a failure, and both are attempted
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/messages/send_sms")).respond_with(ok_response()).up_to_n_times(1).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/messages/send_sms")).respond_with(ResponseTemplate::new(999)).expect(1).mount(&server).await;
    let svc = from_url("serwersms://u:p@Sender/+48111000000/+48222000000").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("msg"))).await.unwrap());

    let svc = from_url("serwersms://u:p@Sender/+48123456789/%23100").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("msg"))).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&with_attachment(ctx("msg")))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // The group fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/messages/send_sms"))
      .and(body_partial_json(json!({"group_id": "123"})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/messages/send_sms")).respond_with(ok_response()).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("serwersms://user:pass@Sender/%23123/15551234567?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx("body"))).await.is_success());
    let sent: Vec<Value> = crate::notify::custom_json::requests_to(&server, "POST", "/messages/send_sms")
      .await
      .iter()
      .map(|r| {
        let body = r.body_json::<Value>().unwrap();
        body.get("phone").or_else(|| body.get("group_id")).cloned().unwrap()
      })
      .collect();
    assert_eq!(sent, vec![json!("+15551234567"), json!("123"), json!("123")]);
  }
}
