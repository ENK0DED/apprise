use async_trait::async_trait;
use reqwest::multipart::{Form, Part};
use serde_json::Value;

use crate::error::NotifyError;
use crate::notify::messaging_targets::{is_phone_no, parse_phone_no, unquote, validate_token};
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};

/// SMSC SMS/MMS (`smsc://{login}:{password}@{targets}`).
pub struct Smsc {
  login: String,
  password: String,
  /// Optional Sender ID
  sender: Option<String>,
  /// Transliterate Cyrillic characters to Latin
  translit: bool,
  /// Target phone numbers (digits only)
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Smsc {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let Some(login) = url.user.clone().filter(|u| !u.is_empty()) else {
      tracing::warn!("An SMSC login must be specified.");
      return None;
    };
    let Some(password) = url.password.clone().filter(|p| !p.is_empty()) else {
      tracing::warn!("An SMSC password must be specified.");
      return None;
    };

    let sender = match url.get("sender").filter(|s| !s.is_empty()) {
      Some(sender) => match validate_token(sender) {
        Some(sender) => Some(sender),
        None => {
          tracing::warn!("The SMSC sender ID specified ({}) is invalid.", sender);
          return None;
        }
      },
      None => None,
    };

    // The host is the first phone number target
    let mut raw_targets = vec![url.host.as_deref().map(unquote).unwrap_or_default()];
    raw_targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_phone_no(&[to]));
    }

    let mut targets = Vec::new();
    for target in parse_phone_no(&raw_targets) {
      match is_phone_no(&target, 10) {
        Some(phone) => targets.push(phone),
        None => tracing::warn!("Dropped invalid SMSC phone # ({}).", target),
      }
    }

    Some(Self {
      login,
      password,
      sender,
      translit: url.get("translit").filter(|t| !t.is_empty()).is_some_and(parse_bool),
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "SMSC",
      service_url: Some("https://smsc.ru/"),
      setup_url: Some("https://appriseit.com/services/smsc/"),
      protocols: vec!["smsc"],
      description: "Send SMS (or MMS with attachments) via SMSC.",
      attachment_support: true,
    }
  }

  /// Query parameters shared by SMS and MMS, in upstream's order.
  fn params<'a>(&'a self, phones: &'a str, body: &'a str) -> Vec<(&'static str, &'a str)> {
    // fmt=3 asks for a JSON response
    let mut params = vec![("login", self.login.as_str()), ("psw", self.password.as_str()), ("phones", phones), ("fmt", "3")];
    if let Some(sender) = &self.sender {
      params.push(("sender", sender.as_str()));
    }
    if self.translit {
      params.push(("translit", "1"));
    }
    params.push(("mes", body));
    params
  }
}

/// One `mes{N}` file part of an MMS request.
fn attachment_part(attachment: &Attachment) -> Part {
  let name = if attachment.name.is_empty() { "attachment.dat" } else { attachment.name.as_str() };
  let part = || Part::bytes(attachment.data.clone()).file_name(name.to_string());
  part().mime_str(&attachment.mime_type).unwrap_or_else(|_| part())
}

/// Upstream's descriptions for the error codes SMSC returns in a 200 body.
fn api_error_description(code: i64) -> &'static str {
  match code {
    1 => "Invalid parameter(s) in request.",
    2 => "Invalid login or password.",
    3 => "Insufficient account funds.",
    4 => "IP address temporarily blocked.",
    5 => "Invalid date/time specified.",
    6 => "Message forbidden by content filters.",
    7 => "Invalid phone number format.",
    8 => "Message body is required.",
    9 => "Too many phone numbers specified.",
    10 => "No phone numbers available for delivery.",
    99 => "Internal server error.",
    _ => "Unknown error.",
  }
}

#[async_trait]
impl Notify for Smsc {
  fn schemas(&self) -> &[&str] {
    &["smsc"]
  }
  fn service_name(&self) -> &str {
    "SMSC"
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
    if self.targets.is_empty() {
      tracing::warn!("No SMSC targets to notify.");
      return Ok(false);
    }

    let url = format!("{}/sys/send.php", service_base("https://smsc.ru"));
    let client = build_client(self.verify_certificate)?;
    let phones = self.targets.join(",");
    let params = self.params(&phones, &ctx.body);

    // Attachments switch the message to MMS
    let use_mms = !ctx.attachments.is_empty();
    let kind = if use_mms { "MMS" } else { "SMS" };
    let request = if use_mms {
      let mut form = Form::new();
      for (key, value) in params {
        form = form.text(key, value.to_string());
      }
      form = form.text("mms", "1");
      for (idx, attachment) in ctx.attachments.iter().enumerate() {
        form = form.part(format!("mes{}", idx), attachment_part(attachment));
      }
      client.post(&url).header("User-Agent", APP_ID).multipart(form)
    } else {
      client.post(&url).header("User-Agent", APP_ID).form(&params)
    };

    let resp = match request.send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending SMSC {}.", kind);
        tracing::debug!("Socket Exception: {}", e.without_url());
        return Ok(false);
      }
    };

    let status = resp.status();
    if status != reqwest::StatusCode::OK {
      tracing::warn!("Failed to send SMSC {}: error={}.", kind, status.as_u16());
      tracing::debug!("Response Details: {}", resp.text().await.unwrap_or_default());
      return Ok(false);
    }

    // Check for an API-level error embedded in an otherwise 200 response
    let content: Value = resp.json().await.unwrap_or(Value::Null);
    if let Some(code) = content.get("error_code") {
      let message = content.get("error").and_then(Value::as_str).unwrap_or_else(|| api_error_description(code.as_i64().unwrap_or(0)));
      tracing::warn!("SMSC API error (code={}): {}", code, message);
      return Ok(false);
    }

    tracing::info!("Sent SMSC {} notification.", kind);
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use serde_json::json;
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Option<Smsc> {
    Smsc::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn ok_response() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"id": "1234", "cnt": 1}))
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "smsc://",
      // Missing password
      "smsc://login@+71234567890",
      // Missing login
      "smsc://:password@+71234567890",
      // Whitespace-only sender
      "smsc://login:password@+71234567890?sender=%20%20",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let obj = parse("smsc://login:password@+71234567890").unwrap();
    assert_eq!(obj.login, "login");
    assert_eq!(obj.password, "password");
    assert_eq!(obj.targets, vec!["71234567890"]);
    assert!(obj.sender.is_none());
    assert!(!obj.translit);

    let obj = parse("smsc://login:password@+71234567890/+79876543210").unwrap();
    assert_eq!(obj.targets, vec!["71234567890", "79876543210"]);

    let obj = parse("smsc://login:password@+71234567890?sender=MySender&translit=yes").unwrap();
    assert_eq!(obj.sender.as_deref(), Some("MySender"));
    assert!(obj.translit);

    let obj = parse("smsc://login:password@+71234567890?to=%2B79876543210").unwrap();
    assert_eq!(obj.targets, vec!["71234567890", "79876543210"]);

    // Invalid phone numbers are dropped
    let obj = parse("smsc://login:password@+71234567890/notaphone").unwrap();
    assert_eq!(obj.targets, vec!["71234567890"]);
    assert!(parse("smsc://login:password@notaphone").unwrap().targets.is_empty());
  }

  #[test]
  fn test_attributes() {
    let svc = from_url("smsc://login:password@+71234567890").unwrap();
    assert_eq!(svc.body_maxlen(), 160);
    assert_eq!(svc.title_maxlen(), 0);
    assert!(svc.attachment_support());
    assert!(Smsc::static_details().attachment_support);
  }

  #[tokio::test]
  async fn test_send_sms() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/sys/send.php"))
      .and(header("User-Agent", APP_ID))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(body_string("login=login&psw=password&phones=15551234567&fmt=3&mes=apprise+parity%0D%0Ahello+from+python"))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("smsc://login:password@15551234567").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("apprise parity\r\nhello from python"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_sms_sender_translit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("login=login&psw=password&phones=15551234567%2C15559876543&fmt=3&sender=Apprise&translit=1&mes=body"))
      .respond_with(ok_response())
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("smsc://login:password@15551234567/15559876543?sender=Apprise&translit=yes").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_mms() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/sys/send.php")).respond_with(ok_response()).expect(1).mount(&server).await;

    let svc = from_url("smsc://login:password@15551234567").unwrap();
    let mut ctx = ctx("title\r\nbody");
    ctx.attachments.push(Attachment { name: "pixel.png".into(), data: b"PNGDATA".to_vec(), mime_type: "image/png".into() });
    ctx.attachments.push(Attachment { name: "notes.txt".into(), data: b"plain text".to_vec(), mime_type: "text/plain".into() });
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // Only our POSTs: other processes on the host may probe the mock port
    let requests: Vec<_> = server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "POST").collect();
    let content_type = requests[0].headers.get("Content-Type").unwrap().to_str().unwrap().to_string();
    assert!(content_type.starts_with("multipart/form-data; boundary="));
    let body = String::from_utf8_lossy(&requests[0].body).to_string();
    // Parts arrive in upstream's order
    let mut last = 0;
    for name in ["\"login\"", "\"psw\"", "\"phones\"", "\"fmt\"", "\"mes\"", "\"mms\"", "\"mes0\"; filename=\"pixel.png\"", "\"mes1\"; filename=\"notes.txt\""]
    {
      let pos = body.find(&format!("name={}", name)).unwrap_or_else(|| panic!("missing part {} in {}", name, body));
      assert!(pos > last, "part {} out of order", name);
      last = pos;
    }
    assert!(body.contains("name=\"fmt\"\r\n\r\n3\r\n"));
    assert!(body.contains("name=\"mms\"\r\n\r\n1\r\n"));
    assert!(body.contains("name=\"mes\"\r\n\r\ntitle\r\nbody\r\n"));
    assert!(body.contains("Content-Type: image/png\r\n\r\nPNGDATA"));
  }

  #[tokio::test]
  async fn test_send_failures() {
    // No targets: no request is made
    let svc = from_url("smsc://login:password@notaphone").unwrap();
    assert!(!svc.send(&ctx("Hello")).await.unwrap());

    for response in [
      // API-level error in a 200 response
      ResponseTemplate::new(200).set_body_json(json!({"error": "Invalid login", "error_code": 2})),
      ResponseTemplate::new(200).set_body_json(json!({"error_code": 3})),
      ResponseTemplate::new(400).set_body_json(json!({})),
      ResponseTemplate::new(500),
      ResponseTemplate::new(999),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(response).expect(1).mount(&server).await;
      let svc = from_url("smsc://login:password@+71234567890").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("Hello"))).await.unwrap());
    }

    // A 200 is authoritative even when the body is not a JSON object
    for body in ["OK", "null", "[]"] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string(body)).expect(1).mount(&server).await;
      let svc = from_url("smsc://login:password@+71234567890").unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Hello"))).await.unwrap(), "body {}", body);
    }

    // MMS errors
    let mut mms = ctx("Image");
    mms.attachments.push(Attachment { name: "a.png".into(), data: vec![1], mime_type: "image/png".into() });
    for response in [ResponseTemplate::new(500), ResponseTemplate::new(200).set_body_json(json!({"error": "Insufficient funds", "error_code": 3}))] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(response).expect(1).mount(&server).await;
      let svc = from_url("smsc://login:password@+71234567890").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&mms)).await.unwrap());
    }

    let svc = from_url("smsc://login:password@+71234567890").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("Hello"))).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&mms)).await.unwrap());
  }
}
