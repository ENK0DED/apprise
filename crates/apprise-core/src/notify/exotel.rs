//! Exotel SMS notifications.
//!
//! ```text
//! exotel://{sid}:{token}@{from_phone}
//! exotel://{sid}:{token}@{from_phone}/{to_phone}/{to_phone}
//! ```
//!
//! The source is an ExoPhone or an approved Sender ID. Without a target the
//! message goes to the source itself. `?apikey=` (or `?key=`) sets an API
//! key that differs from the SID; `?region=in` uses the Indian cluster.

use async_trait::async_trait;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::phone::{is_phone_no, parse_phone_no};

/// The most recipients placed in one request in batch mode.
const BATCH_SIZE: usize = 100;

/// Priority names, in the order upstream matches a prefix against them.
const PRIORITIES: &[(&str, &str)] = &[("normal", "normal"), ("+", "high"), ("high", "high")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
  Us,
  In,
}

impl Region {
  fn api_base(self) -> &'static str {
    match self {
      Region::Us => "https://api.exotel.com",
      Region::In => "https://api.in.exotel.com",
    }
  }
}

pub struct Exotel {
  sid: String,
  token: String,
  apikey: String,
  source: String,
  targets: Vec<String>,
  region: Region,
  priority: &'static str,
  unicode: bool,
  batch: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Exotel {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream strips query values and ignores empty ones
    let qsd = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty());

    let sid = qsd("sid").map(str::to_string).or_else(|| url.user.clone()).unwrap_or_default();
    let Some(sid) = validate(&sid) else {
      tracing::warn!("An invalid Exotel SID was specified.");
      return None;
    };
    let token = qsd("token").map(str::to_string).or_else(|| url.password.clone()).unwrap_or_default();
    let Some(token) = validate(&token) else {
      tracing::warn!("An invalid Exotel API Token was specified.");
      return None;
    };
    // The API key defaults to the SID
    let Some(apikey) = validate(qsd("apikey").or_else(|| qsd("key")).unwrap_or(&sid)) else {
      tracing::warn!("An invalid Exotel API Key was specified.");
      return None;
    };

    let region = match qsd("region").map(str::to_lowercase).as_deref() {
      None | Some("us") => Region::Us,
      Some("in") => Region::In,
      Some(other) => {
        tracing::warn!("The Exotel region specified ({}) is invalid.", other);
        return None;
      }
    };

    // Any unambiguous start of a priority name is accepted
    let priority = match qsd("priority").map(str::to_lowercase) {
      None => "normal",
      Some(p) => match PRIORITIES.iter().find(|(key, _)| key.starts_with(p.as_str())) {
        Some((_, priority)) => priority,
        None => {
          tracing::warn!("An invalid Exotel priority ({}) was specified.", p);
          return None;
        }
      },
    };

    // ?source= wins over ?from=, which wins over the host
    let source = qsd("source").or_else(|| qsd("from")).map(str::to_string).unwrap_or_else(|| decode(url.host.as_deref().unwrap_or_default()));
    let Some(source) = parse_source(&source) else {
      tracing::warn!("The Account (From) Phone # / Sender ID specified ({}) is invalid.", source);
      return None;
    };

    let mut entries = url.path_parts.clone();
    if let Some(to) = qsd("to") {
      entries.extend(parse_phone_no(&[to]));
    }
    let mut targets = Vec::new();
    let mut invalid = false;
    for entry in parse_phone_no(&entries) {
      match is_phone_no(&entry, 9) {
        Some(no) => targets.push(no.full),
        None => {
          tracing::warn!("Dropped invalid phone # ({}) specified.", entry);
          invalid = true;
        }
      }
    }
    if targets.is_empty() && !invalid {
      // Notify ourselves
      targets.push(source.clone());
    }

    // An empty ?unicode= turns it off, as upstream's parse_bool("") does
    let unicode = url.get("unicode").map(parse_bool).unwrap_or(true);
    let batch = url.get("batch").map(parse_bool).unwrap_or(false);

    Some(Self { sid, token, apikey, source, targets, region, priority, unicode, batch, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Exotel",
      service_url: Some("https://exotel.com/"),
      setup_url: Some("https://appriseit.com/services/exotel/"),
      protocols: vec!["exotel"],
      description: "Send SMS via Exotel.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Exotel {
  fn schemas(&self) -> &[&str] {
    &["exotel"]
  }
  fn service_name(&self) -> &str {
    "Exotel"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    2000
  }
  fn title_maxlen(&self) -> usize {
    0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There were no Exotel targets to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let url = format!("{}/v1/Accounts/{}/Sms/send", service_base(self.region.api_base()), self.sid);
    let encoding = if self.unicode { "unicode" } else { "plain" };
    let batch_size = if self.batch { BATCH_SIZE } else { 1 };

    let mut ok = true;
    for targets in self.targets.chunks(batch_size) {
      let label = if self.batch { format!("{} targets", targets.len()) } else { targets[0].clone() };
      let mut form: Vec<(&str, &str)> = vec![("From", &self.source), ("Body", &ctx.body), ("EncodingType", encoding), ("Priority", self.priority)];
      form.extend(targets.iter().map(|t| ("To", t.as_str())));

      tracing::debug!("Exotel POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let resp = client.post(&url).header("User-Agent", APP_ID).basic_auth(&self.apikey, Some(&self.token)).form(&form).send().await;
      match resp {
        Ok(r) if r.status().as_u16() == 200 => tracing::info!("Sent Exotel notification to {}.", label),
        Ok(r) => {
          let status = r.status().as_u16();
          let reason = reqwest::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or("");
          tracing::warn!("Failed to send Exotel notification to {}: {}, error={}.", label, reason, status);
          tracing::debug!("Response Details: {}", truncate(&r.text().await.unwrap_or_default()));
          ok = false;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Exotel:{} notification.", label);
          tracing::debug!("Socket Exception: {}", e);
          ok = false;
        }
      }
    }
    Ok(ok)
  }
}

/// Upstream `parse_exotel_source()`: an ExoPhone (at least 9 digits) or an
/// approved Sender ID of 3 to 16 letters, digits, dots and dashes. A
/// numeric Sender ID must be a 6 digit short code.
fn parse_source(source: &str) -> Option<String> {
  let source = validate(source)?;
  if let Some(no) = is_phone_no(&source, 9) {
    return Some(no.full);
  }
  // ^[A-Z0-9][A-Z0-9.-]{2,15}$
  let len = source.chars().count();
  let shaped = (3..=16).contains(&len)
    && source.starts_with(|c: char| c.is_ascii_alphanumeric())
    && source.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
  let numeric = source.chars().all(|c| c.is_ascii_digit());
  if shaped && (!numeric || len == 6) { Some(source) } else { None }
}

/// Upstream `validate_regex()` with its default pattern: the value must not
/// start with whitespace, and is returned stripped.
fn validate(value: &str) -> Option<String> {
  if value.is_empty() || value.starts_with(char::is_whitespace) { None } else { Some(value.trim().to_string()) }
}

fn decode(s: &str) -> String {
  urlencoding::decode(s).map(|d| d.into_owned()).unwrap_or_else(|_| s.to_string())
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
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse(url: &str) -> Option<Exotel> {
    Exotel::from_url(&ParsedUrl::parse(url)?)
  }

  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let (a, b) = ("a".repeat(32), "b".repeat(32));
    let (n9, n11) = ("3".repeat(9), "5".repeat(11));
    for url in [
      format!("exotel://{a}:{b}@{n9}"),
      format!("exotel://{a}:{b}@EXOTEL/{n9}"),
      format!("exotel://{a}:{b}@600123/{n9}"),
      format!("exotel://{a}:{b}@{}/123/{}/abcd/", "3".repeat(11), "9".repeat(15)),
      format!("exotel://{a}:{b}@{n11}"),
      format!("exotel://{a}:{b}@{n11}?batch=yes"),
      format!("exotel://{a}:{b}@{n11}?batch=no"),
      format!("exotel://_?sid={a}&token={b}&from={n11}"),
      format!("exotel://_?sid={a}&token={b}&from={n11}&unicode=Yes"),
      format!("exotel://_?sid={a}&token={b}&from={n11}&unicode=no"),
      format!("exotel://_?sid={a}&token={b}&from={n11}&region=us"),
      format!("exotel://_?sid={a}&token={b}&from={n11}&region=in"),
      format!("exotel://_?sid={a}&token={b}&from={n11}&priority=normal"),
      format!("exotel://_?sid={a}&token={b}&from={n11}&priority=high"),
      format!("exotel://_?sid={a}&token={b}&source={n11}"),
      format!("exotel://_?sid={a}&token={b}&from={n11}&to={}", "7".repeat(13)),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let (a, b, n11) = ("a".repeat(32), "b".repeat(32), "5".repeat(11));
    for url in [
      "exotel://".to_string(),
      "exotel://:@/".to_string(),
      format!("exotel://{a}@12345678"),
      format!("exotel://{a}:{b}@_"),
      format!("exotel://{a}:{b}@/%20"),
      format!("exotel://{a}:{b}@{}", "3".repeat(8)),
      format!("exotel://{a}:{b}@12345/{}", "4".repeat(11)),
      format!("exotel://_?sid={a}&token={b}&from={n11}&region=invalid"),
      format!("exotel://_?sid={a}&token={b}&from={n11}&priority=invalid"),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    // Values checked against upstream
    let e = parse("exotel://sid:tok@EXOTEL").unwrap();
    assert_eq!((e.sid.as_str(), e.token.as_str(), e.apikey.as_str()), ("sid", "tok", "sid"));
    assert_eq!(e.source, "EXOTEL");
    assert_eq!(e.targets, vec!["EXOTEL"]);
    assert_eq!((e.region, e.priority, e.unicode, e.batch), (Region::Us, "normal", true, false));

    let e = parse("exotel://sid:tok@600123/123456789").unwrap();
    assert_eq!(e.source, "600123");
    assert_eq!(e.targets, vec!["123456789"]);

    // ?source= wins over ?from=; a short priority is expanded
    let e = parse("exotel://_?sid=a&token=b&from=EXO&source=15551234567&priority=n&key=KEY").unwrap();
    assert_eq!((e.sid.as_str(), e.apikey.as_str()), ("a", "KEY"));
    assert_eq!(e.source, "15551234567");
    assert_eq!(e.priority, "normal");
    assert_eq!(parse("exotel://sid:tok@EXOTEL?priority=%2B").unwrap().priority, "high");
    assert_eq!(parse("exotel://sid:tok@EXOTEL?priority=Hi").unwrap().priority, "high");

    let e = parse("exotel://sid:tok@+1 555 123 4567/15551234567?unicode=garbage&region=%20IN").unwrap();
    assert_eq!(e.source, "15551234567");
    assert!(!e.unicode);
    assert_eq!(e.region, Region::In);

    // An invalid target stops the source from being notified
    let e = parse("exotel://sid:tok@EXO.TEL-1/x").unwrap();
    assert_eq!(e.source, "EXO.TEL-1");
    assert!(e.targets.is_empty());

    // Empty values fall back to the defaults
    let e = parse("exotel://sid:tok@EXOTEL?priority=&region=").unwrap();
    assert_eq!((e.region, e.priority), (Region::Us, "normal"));
  }

  #[test]
  fn test_service_details() {
    let e = parse("exotel://sid:tok@EXOTEL").unwrap();
    assert_eq!(e.body_maxlen(), 2000);
    assert_eq!(e.title_maxlen(), 0);
    assert!(!e.attachment_support());
    assert_eq!(Region::In.api_base(), "https://api.in.exotel.com");
    assert_eq!(Region::Us.api_base(), "https://api.exotel.com");
  }

  #[tokio::test]
  async fn test_send_payload() {
    // Matches the apprise-go goldens captured from upstream
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/Accounts/sid123/Sms/send"))
      .and(header("Authorization", "Basic c2lkMTIzOnRva2VuNDU2"))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .and(header("User-Agent", APP_ID))
      .and(body_string("From=15550000000&Body=apprise+parity%0D%0Ahello+from+python&EncodingType=unicode&Priority=normal&To=15551234567"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("exotel://sid123:token456@15550000000/15551234567").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("apprise parity\r\nhello from python"))).await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("From=15550000000&Body=body&EncodingType=plain&Priority=high&To=15551234567"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("exotel://sid123:token456@15550000000/15551234567?region=in&priority=high&unicode=no").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_batch() {
    // Batch mode repeats To= for each recipient; the API key replaces the SID
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v1/Accounts/sid/Sms/send"))
      .and(header("Authorization", "Basic S0VZOnRvaw=="))
      .and(body_string("From=EXOTEL&Body=B&EncodingType=unicode&Priority=high&To=15551234567&To=15551234568"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    // The invalid 123 is dropped without blocking the valid numbers
    let svc = from_url("exotel://sid:tok@EXOTEL/15551234567/15551234568/123?batch=yes&key=KEY&priority=h&region=IN").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let svc = from_url("exotel://sid:tok@15550000000/15551234567/15551234568").unwrap();
    for status in [202, 400, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(2).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B"))).await.unwrap(), "status {}", status);
    }

    // Nothing to notify
    let svc = from_url("exotel://sid:tok@EXOTEL/abcd").unwrap();
    assert!(!svc.send(&ctx("B")).await.unwrap());

    // Connection errors fail the send
    let svc = from_url("exotel://sid:tok@15550000000").unwrap();
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx("B"))).await.unwrap());
  }
}
