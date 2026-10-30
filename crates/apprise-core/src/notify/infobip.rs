//! Infobip SMS notifications.
//!
//! ```text
//! infobip://{apikey}@{base_url}/{to_phone}/{to_phone}
//! ```
//!
//! The base URL is the account's personal `xxxxxx.api.infobip.com` host;
//! the first part alone is enough. `?from=` names a sender, otherwise
//! Infobip picks one.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::{is_phone_no, parse_phone_no};

/// Added to a base URL given without its domain.
const DOMAIN: &str = "api.infobip.com";

/// Message status groups that mean the message will never arrive.
const FAILED_GROUPS: &[&str] = &["UNDELIVERABLE", "EXPIRED", "REJECTED"];

pub struct Infobip {
  apikey: String,
  host: String,
  source: Option<String>,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Infobip {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream strips query values and ignores empty ones
    let qsd = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());

    // The host must be a valid hostname or IP address
    let host = url.verified_host()?;

    let apikey = qsd("key").map(str::to_string).or_else(|| url.user.clone()).unwrap_or_default();
    let Some(apikey) = validate(&apikey) else {
      tracing::warn!("An invalid Infobip API Key was specified.");
      return None;
    };

    // A base URL given as just its first part gets the Infobip domain
    let host = if host.contains('.') { host } else { format!("{}.{}", host, DOMAIN) };

    let source = qsd("from").map(str::to_string);

    let mut entries = url.path_parts.clone();
    if let Some(to) = qsd("to") {
      entries.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    for entry in parse_phone_no(&entries) {
      match is_phone_no(&entry, 10) {
        Some(no) => targets.push(no.full),
        None => tracing::warn!("Dropped invalid Infobip phone # ({}) specified.", entry),
      }
    }

    Some(Self { apikey, host, source, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Infobip",
      service_url: Some("https://www.infobip.com/"),
      setup_url: Some("https://appriseit.com/services/infobip/"),
      protocols: vec!["infobip"],
      description: "Send SMS via Infobip.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Infobip {
  fn schemas(&self) -> &[&str] {
    &["infobip"]
  }
  fn service_name(&self) -> &str {
    "Infobip"
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
      tracing::warn!("There are no Infobip targets to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/sms/3/messages", service_base(&format!("https://{}", self.host)));

    let mut ok = true;
    for target in &self.targets {
      // Skip a target that already accepted this message so a retry does
      // not deliver it twice
      if crate::delivery::is_delivered(target, false) {
        continue;
      }

      let mut message = json!({ "destinations": [{ "to": target }], "content": { "text": ctx.body } });
      if let Some(source) = &self.source {
        message["sender"] = json!(source);
      }
      let payload = json!({ "messages": [message] });

      tracing::debug!("Infobip POST URL: {} (cert_verify={})", url, self.verify_certificate);
      crate::notify::throttle().await;
      let resp = match client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Authorization", format!("App {}", self.apikey))
        .header("Accept", "application/json")
        .json(&payload)
        .send()
        .await
      {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Infobip notification to {}.", target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          ok = false;
          continue;
        }
      };
      let status = resp.status().as_u16();
      let text = resp.text().await.unwrap_or_default();
      let content = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);

      // The first message status that says it will never arrive
      let failed = content
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("status").filter(|s| s.is_object()))
        .find(|s| s.get("groupName").and_then(Value::as_str).is_some_and(|g| FAILED_GROUPS.contains(&g)));

      if status != 200 || failed.is_some() {
        let reason = match failed {
          Some(f) => {
            f.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()).or_else(|| f.get("groupName").and_then(Value::as_str)).unwrap_or("")
          }
          None => status_reason(status),
        };
        tracing::warn!("Failed to send Infobip notification to {}: {}, error={}.", target, reason, status);
        tracing::debug!("Response Details: {}", truncate(&text));
        ok = false;
        continue;
      }
      tracing::info!("Sent Infobip notification to {}.", target);
      crate::delivery::mark_delivered(target, false);
    }
    Ok(ok)
  }
}

/// Upstream `validate_regex()` with its default pattern: the value must not
/// start with whitespace, and is returned stripped.
fn validate(value: &str) -> Option<String> {
  if value.is_empty() || value.starts_with(char::is_whitespace) { None } else { Some(value.trim().to_string()) }
}

fn status_reason(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - Check the phone number and sender.",
    401 => "Unauthorized - Invalid API Key.",
    403 => "Forbidden - The API Key lacks the sms:message:send scope.",
    429 => "Too many requests were made.",
    _ => reqwest::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or(""),
  }
}

fn truncate(text: &str) -> &str {
  match text.char_indices().nth(2000) {
    Some((i, _)) => &text[..i],
    None => text,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const APIKEY: &str = "0123456789abcdef0123456789abcdef-01234567-89ab-cdef-0123";

  fn parse(url: &str) -> Option<Infobip> {
    Infobip::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  fn status(group: &str, description: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
      "bulkId": "2034072219640523072",
      "messages": [{
        "messageId": "2250be2d4219-3af1-78856-aabe",
        "status": {"groupId": 1, "groupName": group, "id": 26, "name": format!("{}_ACCEPTED", group), "description": description},
        "destination": "15551234567",
      }],
    }))
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      format!("infobip://{APIKEY}@xyz123"),
      format!("infobip://{APIKEY}@xyz123/15551234567"),
      format!("infobip://{APIKEY}@api.infobip.com/15551234567/15551234568?from=Alerts"),
      format!("infobip://xyz123/?key={APIKEY}&to=15551234567,15551234568"),
      format!("infobip://{APIKEY}@xyz123/15551234567/123"),
      format!("infobip://{APIKEY}@xyz123/123"),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["infobip://", "infobip://xyz123/15551234567", "infobip://KEY@-bad/15551234567", "infobip://KEY@bad_/15551234567", "infobip://KEY@_/15551234567"]
    {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values checked against upstream
    let i = parse(&format!("infobip://{APIKEY}@xyz123/15551234567")).unwrap();
    assert_eq!(i.apikey, APIKEY);
    assert_eq!(i.host, "xyz123.api.infobip.com");
    assert_eq!(i.source, None);
    assert_eq!(i.targets, vec!["15551234567"]);

    let i = parse("infobip://KEY@api.infobip.com/15551234567/15551234568?from=Alerts").unwrap();
    assert_eq!(i.host, "api.infobip.com");
    assert_eq!(i.source.as_deref(), Some("Alerts"));
    assert_eq!(i.targets, vec!["15551234567", "15551234568"]);

    let i = parse("infobip://xyz123/?key=K&to=15551234567,15551234568").unwrap();
    assert_eq!((i.apikey.as_str(), i.host.as_str()), ("K", "xyz123.api.infobip.com"));
    assert_eq!(i.targets.len(), 2);

    // The port is not used; underscores and a trailing dot are accepted
    assert_eq!(parse("infobip://KEY@xyz123:8080/15551234567").unwrap().host, "xyz123.api.infobip.com");
    assert_eq!(parse("infobip://KEY@XYZ_123/15551234567").unwrap().host, "XYZ_123.api.infobip.com");
    assert_eq!(parse("infobip://KEY@xyz123./15551234567").unwrap().host, "xyz123.api.infobip.com");
    assert_eq!(parse("infobip://KEY@10.0.0.1/15551234567").unwrap().host, "10.0.0.1");
    assert_eq!(parse("infobip://KEY@a/15551234567").unwrap().host, "a.api.infobip.com");
    assert_eq!(parse("infobip://KEY@xyz123/15551234567?from=%20").unwrap().source, None);

    let i = parse("infobip://KEY@xyz123/15551234567/123/abcd").unwrap();
    assert_eq!(i.targets, vec!["15551234567"]);
  }

  #[test]
  fn test_service_details() {
    let i = parse("infobip://KEY@xyz123/15551234567").unwrap();
    assert_eq!(i.body_maxlen(), 160);
    assert_eq!(i.title_maxlen(), 0);
    assert!(!i.attachment_support());
  }

  #[tokio::test]
  async fn test_send_payload() {
    // Verified against upstream with pycapture.py
    let server = MockServer::start().await;
    for to in ["15551234567", "15551234568"] {
      Mock::given(method("POST"))
        .and(path("/sms/3/messages"))
        .and(header("Authorization", "App KEY"))
        .and(header("Content-Type", "application/json"))
        .and(header("Accept", "application/json"))
        .and(header("User-Agent", APP_ID))
        .and(body_json(json!({"messages": [{"destinations": [{"to": to}], "content": {"text": "B"}, "sender": "Alerts"}]})))
        .respond_with(status("PENDING", "Message sent to next instance"))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("infobip://KEY@xyz123/15551234567/15551234568?from=Alerts").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // No sender unless one was given
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({"messages": [{"destinations": [{"to": "15551234567"}], "content": {"text": "B"}}]})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("infobip://KEY@xyz123/15551234567").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let svc = from_url("infobip://KEY@xyz123/15551234567").unwrap();
    for (response, label) in [
      (status("REJECTED", "Not enough credits"), "rejected"),
      (status("UNDELIVERABLE", ""), "undeliverable"),
      (status("EXPIRED", ""), "expired"),
      (ResponseTemplate::new(401), "401"),
      (ResponseTemplate::new(500), "500"),
      (ResponseTemplate::new(999), "999"),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/sms/3/messages")).respond_with(response).expect(1).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap(), "{}", label);
    }

    // A 200 answer that is not JSON relies on the status code
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/sms/3/messages")).respond_with(ResponseTemplate::new(200).set_body_string("garbage")).mount(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // One failed target fails the send, but the rest are still tried
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/sms/3/messages"))
      .and(wiremock::matchers::body_string_contains("15551234567"))
      .respond_with(status("REJECTED", "Not enough credits"))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/sms/3/messages")).respond_with(status("PENDING", "")).expect(1).mount(&server).await;
    let svc = from_url("infobip://KEY@xyz123/15551234567/15551234568").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());

    // Nothing to notify
    let svc = from_url("infobip://KEY@xyz123/123").unwrap();
    assert!(!svc.send(&ctx("B")).await.unwrap());

    // Connection errors fail the send
    let svc = from_url("infobip://KEY@xyz123/15551234567").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_accepted_targets() {
    use wiremock::matchers::body_string_contains;
    let server = MockServer::start().await;
    let send = || Mock::given(method("POST")).and(path("/sms/3/messages"));
    send().and(body_string_contains("15551234567")).respond_with(status("PENDING", "")).expect(1).mount(&server).await;
    // The second number fails once, then accepts the retry
    send().and(body_string_contains("15551234568")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    send().and(body_string_contains("15551234568")).respond_with(status("PENDING", "")).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add("infobip://KEY@xyz123/15551234567/15551234568?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), apprise.notify(&ctx("B"))).await.is_success());
  }
}
