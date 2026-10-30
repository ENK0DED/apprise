use crate::error::NotifyError;
use crate::notify::sms_targets::{is_email, is_phone_no, parse_phone_no, unquoted_host};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::Value;

/// VoIP.ms only sends to North American (+1) numbers.
const COUNTRY_CODE: &str = "1";

pub struct VoipMs {
  email: String,
  password: String,
  /// 10-digit DID the messages come from
  source: String,
  targets: Vec<String>,
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

impl VoipMs {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // voipms://password:email_user@email_domain/from_phone/targets
    // The URL user is the password; the URL password and host form the email.
    let Some(password) = url.user.clone() else {
      tracing::warn!("Password has to be specified.");
      return None;
    };
    let email = format!("{}@{}", url.password.as_deref().unwrap_or_default(), unquoted_host(url).unwrap_or_default());
    let Some(email) = is_email(&email) else {
      tracing::warn!("An invalid VoIPms user email: ({}) was specified.", email);
      return None;
    };

    let mut entries = url.path_parts.clone();
    let source = match url.get("from").map(str::trim).filter(|v| !v.is_empty()) {
      Some(from) => from.to_string(),
      None if !entries.is_empty() => entries.remove(0),
      None => String::new(),
    };
    if let Some(to) = url.get("to").filter(|v| !v.trim().is_empty()) {
      entries.extend(parse_phone_no([to], false));
    }

    let Some(phone) = is_phone_no(&source, 10) else {
      tracing::warn!("An invalid VoIPms source phone # ({}) was specified.", source);
      return None;
    };
    if !phone.country.is_empty() && phone.country != COUNTRY_CODE {
      tracing::warn!("VoIPms only supports +1 country code ({}) was specified.", source);
      return None;
    }
    let source = format!("{}{}", phone.area, phone.line);

    let mut targets = Vec::new();
    if entries.is_empty() {
      // Notify ourselves
      targets.push(source.clone());
    }
    for target in parse_phone_no(&entries, false) {
      // Upstream fails to load the URL at all on an invalid number
      let Some(phone) = is_phone_no(&target, 10) else {
        tracing::warn!("An invalid VoIPms target phone # ({}) was specified.", target);
        return None;
      };
      if !phone.country.is_empty() && phone.country != COUNTRY_CODE {
        tracing::warn!("Ignoring invalid phone # ({}) specified.", target);
        continue;
      }
      targets.push(format!("{}{}", phone.area, phone.line));
    }

    Some(Self { email, password, source, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "VoIPms",
      service_url: Some("https://voip.ms"),
      setup_url: Some("https://appriseit.com/services/voipms/"),
      protocols: vec!["voipms"],
      description: "Send SMS via VoIP.ms.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for VoipMs {
  fn schemas(&self) -> &[&str] {
    &["voipms"]
  }
  fn service_name(&self) -> &str {
    "VoIPms"
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
      tracing::warn!("There were no VoIPms targets to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let base = format!("{}/api/v1/rest.php", service_base("https://voip.ms"));
    let mut has_error = false;

    for target in &self.targets {
      let params = [
        ("api_username", self.email.as_str()),
        ("api_password", self.password.as_str()),
        ("did", self.source.as_str()),
        ("message", ctx.body.as_str()),
        ("method", "sendSMS"),
        ("dst", target.as_str()),
      ];
      tracing::debug!("VoIPms GET URL: {} (cert_verify={})", base, self.verify_certificate);
      let resp = match client
        .get(format!("{}?{}", base, query_encode(&params)))
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send()
        .await
      {
        Ok(resp) => resp,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending VoIPms:{} SMS notification.", target);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
          continue;
        }
      };
      let status = resp.status();
      let text = resp.text().await.unwrap_or_default();
      if status != reqwest::StatusCode::OK {
        tracing::warn!(
          "Failed to send VoIPms SMS notification to {}: {}error={}.",
          target,
          status.canonical_reason().map(|r| format!("{}, ", r)).unwrap_or_default(),
          status.as_u16()
        );
        tracing::debug!("Response Details: {}", text.chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      // An unparseable reply counts as a failure; a JSON null does not
      let (ok, api_status, message) = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Null) => (true, String::new(), String::new()),
        Ok(Value::Object(o)) => {
          let api_status = o.get("status").and_then(Value::as_str).unwrap_or("unknown").to_string();
          (api_status == "success", api_status, o.get("message").map(|m| m.as_str().map(str::to_string).unwrap_or_else(|| m.to_string())).unwrap_or_default())
        }
        _ => (false, "unknown".to_string(), String::new()),
      };
      if !ok {
        tracing::warn!("Failed to send VoIPms SMS notification to {}: status: {}, message: {}", target, api_status, message);
        has_error = true;
        continue;
      }
      tracing::info!("Sent VoIPms SMS notification to {}", target);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn voipms(url: &str) -> VoipMs {
    VoipMs::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "voipms://".to_string(),
      "voipms://@:".to_string(),
      format!("voipms://{}/{}", "user@example.com", "1".repeat(11)),
      "voipms://:password".to_string(),
      format!("voipms://{}:{}/{}", "user@", "pass", "1".repeat(11)),
      // No source
      "voipms://password:user@example.com".to_string(),
      // Source too short
      "voipms://password:user@example.com/1613".to_string(),
      // Only +1 numbers
      "voipms://password:user@example.com/01133122446688".to_string(),
      // A target that is no phone number fails the URL
      "voipms://password:user@example.com/16134448888/abcd".to_string(),
      // The email domain must look like one
      "voipms://password:user@host/16134448888".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "voipms://password:user@example.com/16134448888/26134442222/",
      "voipms://password:user@example.com/16138884444",
      "voipms://password:user@example.com/?from=16138884444",
      "voipms://password:user@example.com/16138884444/16134442222/",
      "voipms://password:user@example.com/?from=16138884444&to=16134448888",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let v = voipms("voipms://password:user@example.com/16138884444/16134442222/+1%20(613)%20444-7777/6134446666");
    assert_eq!(v.password, "password");
    assert_eq!(v.email, "user@example.com");
    assert_eq!(v.source, "6138884444");
    assert_eq!(v.targets, vec!["6134442222", "6134447777", "6134446666"]);

    // Without targets, the source is notified
    assert_eq!(voipms("voipms://password:user@example.com/?from=16138884444").targets, vec!["6138884444"]);
    // Other countries are dropped
    assert!(voipms("voipms://password:user@example.com/16134448888/26134442222/").targets.is_empty());
  }

  #[test]
  fn test_service_details() {
    let d = VoipMs::static_details();
    assert_eq!(d.service_name, "VoIPms");
    assert!(d.protocols.contains(&"voipms"));
  }

  #[tokio::test]
  async fn test_send_query() {
    // Matches upstream pycapture output
    let server = MockServer::start().await;
    for dst in ["6134449999", "6134447777"] {
      Mock::given(method("GET"))
        .and(path("/api/v1/rest.php"))
        .and(header("Content-Type", "application/x-www-form-urlencoded"))
        .and(query_param("api_username", "user@example.com"))
        .and(query_param("api_password", "pass"))
        .and(query_param("did", "6134448888"))
        .and(query_param("message", "title\r\nbody"))
        .and(query_param("method", "sendSMS"))
        .and(query_param("dst", dst))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status": "success"}"#))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("voipms://pass:user@example.com/16134448888/16134449999/+1%20(613)%20444-7777").unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_responses() {
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    let svc = from_url("voipms://password:user@example.com/16138884444/16134442222").unwrap();
    for (status, body, expected) in [
      (200, r#"{"status": "success"}"#, true),
      (200, "null", true),
      (200, r#"{"status": "invalid_dst", "message": "x"}"#, false),
      (200, r#"{"message": "x"}"#, false),
      (200, "", false),
      (400, r#"{"status": "success"}"#, false),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("GET")).respond_with(ResponseTemplate::new(status).set_body_string(body)).expect(1).mount(&server).await;
      assert_eq!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap(), expected, "{} {}", status, body);
    }
    // All targets dropped
    assert!(!voipms("voipms://password:user@example.com/16134448888/26134442222/").send(&ctx).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }
}
