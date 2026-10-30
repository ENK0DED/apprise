use crate::error::NotifyError;
use crate::notify::sms_targets::{is_phone_no, parse_phone_no, url_credentials};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::types::NotifyType;
use crate::utils::parse::{ParsedUrl, parse_bool};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{Map, Value, json};

/// Recipients per request in batch mode.
const DEFAULT_BATCH_SIZE: usize = 10;

/// Priority names (any prefix is accepted) and their values.
const PRIORITY_MAP: &[(&str, i64)] = &[("normal", 0), ("+", 1), ("high", 1)];

pub struct SmsEagle {
  host: String,
  port: Option<u16>,
  secure: bool,
  token: String,
  phones: Vec<String>,
  groups: Vec<String>,
  contacts: Vec<String>,
  priority: i64,
  batch: bool,
  status: bool,
  flash: bool,
  test: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// `[a-z0-9_-]+` at the start of `s` (any case).
fn leading_name(s: &str) -> Option<&str> {
  let end = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(s.len());
  (end > 0).then(|| &s[..end])
}

/// Upstream `GROUP_REGEX`: `^\s*(\#|\%35)(?P<group>[a-z0-9_-]+)`.
fn match_group(target: &str) -> Option<&str> {
  let s = target.trim_start();
  leading_name(s.strip_prefix('#').or_else(|| s.strip_prefix("%35"))?)
}

/// Upstream `CONTACT_REGEX`: `^\s*(\@|\%40)?(?P<contact>[a-z0-9_-]+)`.
fn match_contact(target: &str) -> Option<&str> {
  let s = target.trim_start();
  leading_name(s.strip_prefix('@').or_else(|| s.strip_prefix("%40")).unwrap_or(s))
}

/// Upstream `AppriseAsset.ascii()`.
fn ascii(notify_type: &NotifyType) -> &'static str {
  match notify_type {
    NotifyType::Info => "[i]",
    NotifyType::Success => "[+]",
    NotifyType::Failure => "[!]",
    NotifyType::Warning => "[~]",
  }
}

/// `int(priority)`, else a prefix of a priority name; only 0 and 1 are valid.
fn parse_priority(value: Option<&str>) -> Result<i64, ()> {
  let Some(value) = value else { return Ok(0) };
  let priority = match value.trim().parse::<i64>() {
    Ok(p) => p,
    Err(_) => {
      let value = value.trim().to_lowercase();
      if value.is_empty() {
        return Err(());
      }
      PRIORITY_MAP.iter().find(|(name, _)| name.starts_with(&value)).map(|(_, p)| *p).ok_or(())?
    }
  };
  if PRIORITY_MAP.iter().any(|(_, p)| *p == priority) { Ok(priority) } else { Err(()) }
}

impl SmsEagle {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (url_user, _) = url_credentials(url);
    // smseagle(s)://token@host[:port]/targets
    let token = url.get("token").map(str::trim).filter(|v| !v.is_empty()).map(str::to_string).or_else(|| url_user.clone()).unwrap_or_default();
    if token.chars().next().is_none_or(char::is_whitespace) {
      tracing::warn!("An invalid SMSEagle Access Token was specified.");
      return None;
    }
    let token = token.trim().to_string();
    let host = url.host.clone().unwrap_or_default();
    if host.is_empty() {
      return None;
    }

    let priority_arg = url.get("priority").filter(|v| !v.trim().is_empty());
    let Ok(priority) = parse_priority(priority_arg) else {
      tracing::warn!("An invalid SMSEagle priority ({}) was specified.", priority_arg.unwrap_or_default());
      return None;
    };

    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|v| !v.trim().is_empty()) {
      entries.extend(parse_phone_no([to], false));
    }
    let (mut phones, mut groups, mut contacts) = (Vec::new(), Vec::new(), Vec::new());
    for target in parse_phone_no(&entries, false) {
      if let Some(phone) = is_phone_no(&target, 9) {
        // Keep a leading + if one was given
        phones.push(format!("{}{}", if target.starts_with('+') { "+" } else { "" }, phone.full));
      } else if let Some(group) = match_group(&target) {
        groups.push(group.to_string());
      } else if let Some(contact) = match_contact(&target) {
        contacts.push(contact.to_string());
      } else {
        tracing::warn!("Dropped invalid phone/group/contact ({}) specified.", target);
      }
    }

    let flag = |key: &str| url.get(key).map(parse_bool).unwrap_or(false);
    Some(Self {
      host,
      port: url.port,
      secure: url.schema == "smseagles",
      token,
      phones,
      groups,
      contacts,
      priority,
      batch: flag("batch"),
      status: flag("status"),
      flash: flag("flash"),
      test: flag("test"),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SMS Eagle",
      service_url: Some("https://smseagle.eu"),
      setup_url: Some("https://appriseit.com/services/smseagle/"),
      protocols: vec!["smseagle", "smseagles"],
      description: "Send SMS via SMSEagle hardware gateway.",
      attachment_support: true,
    }
  }
}

/// SMSEagle replies `{"result": {"status": "ok", ...}}`, or a list of such
/// results; anything else is a failure.
fn result_ok(content: &Value) -> bool {
  match content.get("result") {
    Some(Value::Object(result)) => result.get("status").and_then(Value::as_str) == Some("ok"),
    Some(Value::Array(results)) => results.iter().all(|r| !r.is_object() || r.get("status").and_then(Value::as_str) == Some("ok")),
    _ => false,
  }
}

#[async_trait]
impl Notify for SmsEagle {
  fn schemas(&self) -> &[&str] {
    &["smseagle", "smseagles"]
  }
  fn service_name(&self) -> &str {
    "SMS Eagle"
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
    1200
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.phones.is_empty() && self.groups.is_empty() && self.contacts.is_empty() {
      tracing::warn!("There were no SMSEagle targets to notify.");
      return Ok(false);
    }

    // Only images can be sent (as MMS)
    let mut attachments = Vec::new();
    for attachment in &ctx.attachments {
      if !attachment.mime_type.to_lowercase().starts_with("image/") {
        tracing::warn!("Ignoring unsupported SMSEagle attachment {}.", attachment.name);
        continue;
      }
      attachments.push(json!({
        "content_type": attachment.mime_type,
        "content": base64::engine::general_purpose::STANDARD.encode(&attachment.data),
      }));
    }

    let mut params = Map::new();
    params.insert("access_token".into(), json!(self.token));
    let message = if self.status { format!("{} {}", ascii(&ctx.notify_type), ctx.body) } else { ctx.body.clone() };
    params.insert("message".into(), json!(message));
    params.insert("highpriority".into(), json!(self.priority));
    params.insert("unicode".into(), json!(1));
    params.insert("message_type".into(), json!(if attachments.is_empty() { "sms" } else { "mms" }));
    params.insert("responsetype".into(), json!("extended"));
    params.insert("flash".into(), json!(if self.flash { 1 } else { 0 }));
    params.insert("test".into(), json!(if self.test { 1 } else { 0 }));
    if !attachments.is_empty() {
      params.insert("attachments".into(), Value::Array(attachments));
    }

    let client = build_client(self.verify_certificate)?;
    let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}/jsonrpc/sms", if self.secure { "https" } else { "http" }, self.host, port);
    let batch_size = if self.batch { DEFAULT_BATCH_SIZE } else { 1 };
    let mut has_error = false;

    let categories: [(&str, &str, &str, &[String]); 3] = [
      ("phone", "sms.send_sms", "to", &self.phones),
      ("group", "sms.send_togroup", "groupname", &self.groups),
      ("contact", "sms.send_tocontact", "contactname", &self.contacts),
    ];
    for (category, rpc_method, target_key, targets) in categories {
      for chunk in targets.chunks(batch_size) {
        let mut chunk_params = params.clone();
        chunk_params.insert(target_key.into(), json!(chunk.join(",")));
        let payload = json!({ "method": rpc_method, "params": chunk_params });
        let to = if batch_size == 1 { format!("to {}", chunk[0]) } else { "(s)".to_string() };
        tracing::debug!("SMSEagle POST URL: {} (cert_verify={})", url, self.verify_certificate);

        let resp =
          match client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(serde_json::to_string(&payload)?).send().await {
            Ok(resp) => resp,
            Err(e) => {
              tracing::warn!("A Connection error occured sending {} SMSEagle {} notification(s).", chunk.len(), category);
              tracing::debug!("Socket Exception: {}", e);
              has_error = true;
              continue;
            }
          };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let content: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !matches!(status.as_u16(), 200 | 201) || !result_ok(&content) {
          let detail = content.get("result").map(Value::to_string).or_else(|| status.canonical_reason().map(str::to_string)).unwrap_or_default();
          tracing::warn!(
            "Failed to send {} {} SMSEagle {} notification: {}{}error={}.",
            chunk.len(),
            to,
            category,
            detail,
            if detail.is_empty() { "" } else { ", " },
            status.as_u16()
          );
          tracing::debug!("Response {} Details: {}", category.to_uppercase(), text.chars().take(2000).collect::<String>());
          has_error = true;
          continue;
        }
        tracing::info!("Sent {} SMSEagle {} notification{}.", chunk.len(), category, if batch_size == 1 { format!(" {}", to) } else { "(s)".into() });
      }
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const GOOD: &str = r#"{"result": {"message_id": "748", "status": "ok"}}"#;
  const BAD: &str = r#"{"result": {"error_text": "Wrong parameters", "status": "error"}}"#;

  fn eagle(url: &str) -> SmsEagle {
    SmsEagle::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  /// URL for a mock server, keeping the scheme's http/https choice
  fn mock_url(server: &MockServer, rest: &str) -> String {
    format!("smseagle://token@{}/{}", server.address(), rest)
  }

  fn params(target_key: &str, target: &str, extra: Value) -> Value {
    let mut p = json!({
      "access_token": "token",
      "message": "title\r\nbody",
      "highpriority": 0,
      "unicode": 1,
      "message_type": "sms",
      "responsetype": "extended",
      "flash": 0,
      "test": 0,
    });
    p[target_key] = json!(target);
    for (k, v) in extra.as_object().unwrap() {
      p[k] = v.clone();
    }
    p
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "smseagle://token@localhost/123/".to_string(),
      "smseagle://tokenb@localhost/%20/%20/".to_string(),
      format!("smseagle://token@localhost:8080/{}/", "1".repeat(11)),
      format!("smseagle://localhost:8080/{}/?token=abc1234", "1".repeat(11)),
      "smseagle://token@localhost/@user/?priority=high".to_string(),
      "smseagle://token@localhost/@user/?priority=1".to_string(),
      "smseagle://token@localhost/@user/?priority=%2B".to_string(),
      "smseagle://token@localhost/@user/?priority=n".to_string(),
      "smseagle://token@localhost:8082/#abcd/".to_string(),
      "smseagle://token@localhost:8082/@abcd/".to_string(),
      "smseagles://token@localhost:8081/contact/".to_string(),
      "smseagle://token@localhost:8082/@/#/,/".to_string(),
      format!("smseagle://token@localhost:8086/?to={},{}", "2".repeat(11), "3".repeat(11)),
      format!("smseagles://token@localhost/{}/{}?batch=True", "2".repeat(11), "3".repeat(11)),
      format!("smseagles://token@localhost/{}/?flash=yes", "3".repeat(11)),
      format!("smseagles://token@localhost/{}/?test=yes&status=yes", "3".repeat(11)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "smseagle://",
      "smseagle://:@/",
      "smseagle://localhost",
      "smseagle://%20@localhost",
      "smseagle://token@localhost/@user/?priority=invalid",
      "smseagle://token@localhost/@user/?priority=25",
      "smseagle://token@localhost/@user/?priority=2",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_target_classification() {
    let e = eagle("smseagle://token@localhost/12512222222/+15551232001/@contact/%23group/123/abc123/%2C/@/");
    assert_eq!(e.phones, vec!["12512222222", "+15551232001"]);
    assert_eq!(e.groups, vec!["group"]);
    // "123" is too short for a phone number; "abc123" yields the number in it
    assert_eq!(e.contacts, vec!["contact", "123", "123"]);
    assert_eq!(e.priority, 0);
    assert!(!e.batch && !e.status && !e.flash && !e.test);

    let e = eagle("smseagle://localhost/12512222222?token=abc&priority=high&batch=yes&status=yes&flash=yes&test=yes");
    assert_eq!(e.token, "abc");
    assert_eq!(e.priority, 1);
    assert!(e.batch && e.status && e.flash && e.test);
    assert!(eagle("smseagle://token@localhost/@/#/,/").phones.is_empty());
  }

  #[test]
  fn test_to_argument_parsing() {
    // Upstream parse_phone_no(): once a number is found in the string, the
    // entries it can't separate are lost; without one, everything is split
    let e = eagle("smseagle://token@localhost/?to=12345678901,%2B12345678902,%23g1,%23g2");
    assert_eq!(e.phones, vec!["12345678901"]);
    assert!(e.groups.is_empty());
    let e = eagle("smseagle://token@localhost/?to=12345678901,%23g1,%23g2");
    assert_eq!(e.phones, vec!["12345678901"]);
    assert_eq!(e.groups, vec!["g1", "g2"]);
  }

  #[test]
  fn test_service_details() {
    let d = SmsEagle::static_details();
    assert_eq!(d.service_name, "SMS Eagle");
    assert!(d.protocols.contains(&"smseagles"));
    assert!(d.attachment_support);
  }

  #[tokio::test]
  async fn test_send_per_category() {
    // Matches upstream pycapture output
    let server = MockServer::start().await;
    for (rpc, key, target) in
      [("sms.send_sms", "to", "12512222222"), ("sms.send_togroup", "groupname", "group"), ("sms.send_tocontact", "contactname", "contact")]
    {
      Mock::given(method("POST"))
        .and(path("/jsonrpc/sms"))
        .and(header("Content-Type", "application/json"))
        .and(body_json(json!({"method": rpc, "params": params(key, target, json!({}))})))
        .respond_with(ResponseTemplate::new(200).set_body_string(GOOD))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&mock_url(&server, "12512222222/@contact/%23group")).unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch_and_options() {
    let server = MockServer::start().await;
    let extra = json!({"highpriority": 1, "flash": 1, "test": 1, "message": "[~] title\r\nbody"});
    Mock::given(method("POST"))
      .and(body_json(json!({"method": "sms.send_sms", "params": params("to", "12345678901,+12345678902", extra.clone())})))
      .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"result": [{"message_id": "1", "status": "ok"}]}"#))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(body_json(json!({"method": "sms.send_togroup", "params": params("groupname", "g1,g2", extra)})))
      .respond_with(ResponseTemplate::new(201).set_body_string(GOOD))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&mock_url(&server, "12345678901/+12345678902/%23g1/%23g2?batch=yes&priority=high&flash=yes&test=yes&status=yes")).unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), notify_type: NotifyType::Warning, ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_image_attachments_as_mms() {
    let server = MockServer::start().await;
    let extra = json!({"message_type": "mms", "attachments": [{"content_type": "image/gif", "content": "R0lGODlh"}]});
    Mock::given(method("POST"))
      .and(body_json(json!({"method": "sms.send_tocontact", "params": params("contactname", "user", extra)})))
      .respond_with(ResponseTemplate::new(200).set_body_string(GOOD))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&mock_url(&server, "@user")).unwrap();
    let ctx = NotifyContext {
      body: "title\r\nbody".into(),
      attachments: vec![
        Attachment { name: "img.gif".into(), data: b"GIF89a".to_vec(), mime_type: "image/gif".into() },
        Attachment { name: "a.txt".into(), data: b"hi".to_vec(), mime_type: "text/plain".into() },
      ],
      ..Default::default()
    };
    assert!(svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    assert!(!eagle("smseagle://token@localhost/@/#/,/").send(&ctx).await.unwrap());

    for (status, body) in [(200, BAD), (200, "{"), (200, ""), (500, GOOD)] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status).set_body_string(body)).expect(1).mount(&server).await;
      let svc = from_url(&mock_url(&server, "@user")).unwrap();
      assert!(!svc.send(&ctx).await.unwrap(), "{} {}", status, body);
    }

    // One failed category does not stop the others
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({"method": "sms.send_sms", "params": params("to", "12512222222", json!({"message": "body"}))})))
      .respond_with(ResponseTemplate::new(200).set_body_string(BAD))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(body_json(json!({"method": "sms.send_tocontact", "params": params("contactname", "user", json!({"message": "body"}))})))
      .respond_with(ResponseTemplate::new(200).set_body_string(GOOD))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&mock_url(&server, "12512222222/@user")).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    assert!(!from_url("smseagle://token@127.0.0.1:1/@user").unwrap().send(&ctx).await.unwrap());
  }
}
