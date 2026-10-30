use crate::error::NotifyError;
use crate::notify::sms_targets::{is_phone_no, parse_phone_no, unquoted_host, url_credentials};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::Value;

/// How the notification reaches the recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
  Sms,
  Call,
}

impl Method {
  const ALL: [(&'static str, Method); 2] = [("sms", Method::Sms), ("call", Method::Call)];

  /// Upstream accepts any prefix of a method name.
  fn from_prefix(value: &str) -> Option<Self> {
    let value = value.to_lowercase();
    Self::ALL.iter().find(|(name, _)| name.starts_with(&value)).map(|(_, m)| *m)
  }
}

/// Plain text messages or WhatsApp (a `w:` prefix on a number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  Text,
  WhatsApp,
}

pub struct Twilio {
  account_sid: String,
  auth_token: String,
  /// API key (`SK...`) used in place of the account SID to authenticate
  apikey: Option<String>,
  /// `+E.164` number, or a bare 5/6 digit short code
  source: String,
  default_mode: Mode,
  targets: Vec<(Mode, String)>,
  method: Method,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `MODE_DETECT_RE`: `\s*((?P<mode>[^:]+)\s*:\s*)?(?P<phoneno>.+)$`.
/// Returns the optional mode label and the number.
fn detect_mode(entry: &str) -> Option<(Option<&str>, &str)> {
  let entry = entry.trim_start();
  if let Some((mode, rest)) = entry.split_once(':') {
    let phoneno = rest.trim_start();
    if !mode.is_empty() && !phoneno.is_empty() {
      return Some((Some(mode), phoneno));
    }
  }
  (!entry.is_empty()).then_some((None, entry))
}

fn mode_of(label: Option<&str>, default: Mode) -> Mode {
  if label.is_some_and(|m| m.to_lowercase().starts_with('w')) { Mode::WhatsApp } else { default }
}

/// `^prefix[a-f0-9]+$` (any case), as for `AC...` SIDs and `SK...` keys.
fn is_prefixed_hex(value: &str, prefix: &str) -> bool {
  value.len() > prefix.len() && value[..prefix.len()].eq_ignore_ascii_case(prefix) && value[prefix.len()..].chars().all(|c| c.is_ascii_hexdigit())
}

/// `xml.sax.saxutils.escape()`
fn xml_escape(text: &str) -> String {
  text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `urllib.parse.urlencode()` as requests applies it to `data=`.
fn form_encode(pairs: &[(&str, &str)]) -> String {
  pairs
    .iter()
    .map(|(k, v)| format!("{}={}", urlencoding::encode(k).replace("%20", "+"), urlencoding::encode(v).replace("%20", "+")))
    .collect::<Vec<_>>()
    .join("&")
}

impl Twilio {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (url_user, url_password) = url_credentials(url);
    // twilio://account_sid:auth_token@source/targets
    let qs = |key: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let account_sid = qs("sid").or_else(|| url_user.clone()).unwrap_or_default();
    let auth_token = qs("token").or_else(|| url_password.clone()).unwrap_or_default();
    let source = qs("source").or_else(|| qs("from")).or_else(|| unquoted_host(url)).unwrap_or_default();

    if !is_prefixed_hex(&account_sid, "AC") {
      tracing::warn!("An invalid Twilio Account SID ({}) was specified.", account_sid);
      return None;
    }
    if auth_token.is_empty() || !auth_token.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("An invalid Twilio Authentication Token was specified.");
      return None;
    }
    // An invalid API key is ignored, as upstream does
    let apikey = qs("apikey").filter(|k| is_prefixed_hex(k, "SK"));

    let method = match qs("method") {
      Some(m) => match Method::from_prefix(&m) {
        Some(method) => method,
        None => {
          tracing::warn!("The Twilio notification method specified ({}) is invalid.", m);
          return None;
        }
      },
      None => Method::Sms,
    };

    let Some((label, phoneno)) = detect_mode(&source) else {
      tracing::warn!("The Account (From) Phone # or Short-code specified ({}) is invalid.", source);
      return None;
    };
    let default_mode = mode_of(label, Mode::Text);
    if method == Method::Call && default_mode == Mode::WhatsApp {
      tracing::warn!("The notification method Call is not valid along message mode Whatsapp.");
      return None;
    }
    let Some(phone) = is_phone_no(phoneno, 5) else {
      tracing::warn!("The Account (From) Phone # or Short-code specified ({}) is invalid.", source);
      return None;
    };
    let source_no = match phone.full.len() {
      5 | 6 => phone.full,
      11..=14 => format!("+{}", phone.full),
      _ => {
        tracing::warn!("The Account (From) Phone # specified ({}) is invalid.", source);
        return None;
      }
    };
    let short_code = !source_no.starts_with('+');

    let mut entries = url.path_parts.clone();
    if let Some(to) = qs("to") {
      entries.extend(parse_phone_no([to], true));
    }
    let mut targets = Vec::new();
    for entry in parse_phone_no(&entries, true) {
      let Some((label, phoneno)) = detect_mode(&entry) else { continue };
      let mode = mode_of(label, default_mode);
      let Some(phone) = is_phone_no(phoneno, 10) else {
        tracing::warn!("Dropped invalid phone # ({}) specified.", entry);
        continue;
      };
      if (short_code || method == Method::Call) && mode == Mode::WhatsApp {
        tracing::warn!("Dropped WhatsApp phone # ({}) because source provided is a short-code or because notification method is phone call.", entry);
        continue;
      }
      targets.push((mode, format!("+{}", phone.full)));
    }

    Some(Self {
      account_sid,
      auth_token,
      apikey,
      source: source_no,
      default_mode,
      targets,
      method,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Twilio",
      service_url: Some("https://www.twilio.com/"),
      setup_url: Some("https://appriseit.com/services/twilio/"),
      protocols: vec!["twilio"],
      description: "Send SMS, WhatsApp messages or voice calls via Twilio.",
      attachment_support: false,
    }
  }

  fn is_short_code(&self) -> bool {
    !self.source.starts_with('+')
  }
}

#[async_trait]
impl Notify for Twilio {
  fn schemas(&self) -> &[&str] {
    &["twilio"]
  }
  fn service_name(&self) -> &str {
    "Twilio"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    if self.method == Method::Sms { 160 } else { 4000 }
  }
  fn title_maxlen(&self) -> usize {
    // Calls speak the title; SMS fold it into the body
    if self.method == Method::Sms { 0 } else { 250 }
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.20
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() && self.is_short_code() {
      tracing::warn!("There are no valid Twilio targets to notify.");
      return Ok(false);
    }
    let base = service_base("https://api.twilio.com");
    let (url, content_key, content) = match self.method {
      Method::Sms => (format!("{}/2010-04-01/Accounts/{}/Messages.json", base, self.account_sid), "Body", ctx.body.clone()),
      Method::Call => {
        let twiml = if ctx.body.trim_start().starts_with('<') {
          if !ctx.title.is_empty() {
            tracing::debug!("Twilio call body is TwiML; the title is not used.");
          }
          ctx.body.clone()
        } else {
          let says: String = [&ctx.title, &ctx.body].iter().filter(|t| !t.is_empty()).map(|t| format!("<Say>{}</Say>", xml_escape(t))).collect();
          format!("<Response>{}</Response>", says)
        };
        (format!("{}/2010-04-01/Accounts/{}/Calls.json", base, self.account_sid), "Twiml", twiml)
      }
    };

    // Without targets a message goes to the source itself; a call does not
    let mut targets = self.targets.clone();
    if targets.is_empty() && self.method != Method::Call {
      targets.push((self.default_mode, self.source.clone()));
    }

    let client = build_client(self.verify_certificate)?;
    let user = self.apikey.as_deref().unwrap_or(&self.account_sid);
    let mut has_error = false;
    for (mode, target) in &targets {
      let (from, to) = match mode {
        Mode::Text => (self.source.clone(), target.clone()),
        Mode::WhatsApp => (format!("whatsapp:{}", self.source), format!("whatsapp:{}", target)),
      };
      let body = form_encode(&[(content_key, content.as_str()), ("From", from.as_str()), ("To", to.as_str())]);
      tracing::debug!("Twilio POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let result = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .basic_auth(user, Some(&self.auth_token))
        .body(body)
        .send()
        .await;
      match result {
        Ok(resp) if matches!(resp.status().as_u16(), 200 | 201) => tracing::info!("Sent Twilio notification to {}.", target),
        Ok(resp) => {
          let status = resp.status();
          let text = resp.text().await.unwrap_or_default();
          // Twilio explains failures in a JSON body
          let detail: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
          let code = detail.get("code").map(|c| c.to_string().trim_matches('"').to_string()).unwrap_or_else(|| status.as_u16().to_string());
          let message = detail.get("message").and_then(Value::as_str).or(status.canonical_reason()).unwrap_or("");
          tracing::warn!("Failed to send Twilio notification to {}: {}{}error={}.", target, message, if message.is_empty() { "" } else { ", " }, code);
          tracing::debug!("Response Details: {}", text.chars().take(2000).collect::<String>());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Twilio:{} notification.", target);
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
  use wiremock::matchers::{body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn sid() -> String {
    format!("AC{}", "a".repeat(32))
  }

  fn twilio(url: &str) -> Twilio {
    Twilio::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  fn basic(user: &str, pass: &str) -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, pass)))
  }

  #[test]
  fn test_invalid_urls() {
    let (a, b) = (sid(), "b".repeat(32));
    let urls: Vec<String> = vec![
      "twilio://".into(),
      "twilio://:@/".into(),
      // Just SID, no token
      format!("twilio://{}@12345678", a),
      // Invalid source
      format!("twilio://{}:{}@_", a, b),
      // 9 digits is neither a short code nor a full number
      format!("twilio://{}:{}@{}", a, b, "3".repeat(9)),
      // "w:12345" in the host is a host "w" and port 12345
      format!("twilio://{}:{}@w:12345/{}/{}", a, b, "4".repeat(11), "5".repeat(11)),
      format!("twilio://{}:{}@{}?method=mms", a, b, "5".repeat(11)),
      // Calls can't come from a WhatsApp source
      format!("twilio://_?sid={}&token={}&from=w:{}&method=call", a, b, "5".repeat(11)),
      // SIDs start with AC and are hex
      format!("twilio://{}:{}@{}", "a".repeat(34), b, "5".repeat(11)),
      format!("twilio://AC{}:{}@{}", "z".repeat(32), b, "5".repeat(11)),
      format!("twilio://{}:{}@{}", a, "b_c", "5".repeat(11)),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let (a, b) = (sid(), "b".repeat(32));
    let urls: Vec<String> = vec![
      // Short code with no targets (notify fails)
      format!("twilio://{}:{}@{}", a, b, "3".repeat(5)),
      format!("twilio://{}:{}@{}/123/{}/abcd/w:{}", a, b, "3".repeat(11), "9".repeat(15), 8 * 11),
      format!("twilio://{}:{}@12345/{}", a, b, "4".repeat(11)),
      format!("twilio://{}:{}@98765/{}/w:{}/", a, b, "4".repeat(11), "5".repeat(11)),
      format!("twilio://{}:{}@123456/{}", a, b, "4".repeat(11)),
      format!("twilio://{}:{}@{}", a, b, "5".repeat(11)),
      format!("twilio://{}:{}@{}?method=sms", a, b, "5".repeat(11)),
      format!("twilio://{}:{}@{}?method=c", a, b, "5".repeat(11)),
      format!("twilio://_?sid={}&token={}&from={}", a, b, "5".repeat(11)),
      format!("twilio://_?sid={}&token={}&from={}&to=w:{}", a, b, "5".repeat(11), "6".repeat(11)),
      format!("twilio://_?sid={}&token={}&source={}", a, b, "5".repeat(11)),
      format!("twilio://_?sid={}&token={}&from={}&to={}", a, b, "5".repeat(11), "7".repeat(13)),
      format!("twilio://_?sid={}&token={}&from=w:{}", a, b, "5".repeat(11)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let (a, b) = (sid(), "b".repeat(32));
    let t = twilio(&format!("twilio://{}:{}@15551233456/15559876543/w:15559876544/abcd?apikey=SK{}", a, b, "c".repeat(32)));
    assert_eq!(t.account_sid, a);
    assert_eq!(t.auth_token, b);
    assert_eq!(t.apikey, Some(format!("SK{}", "c".repeat(32))));
    assert_eq!(t.source, "+15551233456");
    assert_eq!(t.default_mode, Mode::Text);
    assert_eq!(t.targets, vec![(Mode::Text, "+15559876543".to_string()), (Mode::WhatsApp, "+15559876544".to_string())]);
    assert_eq!(t.method, Method::Sms);
    assert_eq!((t.title_maxlen(), t.body_maxlen()), (0, 160));

    // An invalid API key is ignored
    assert_eq!(twilio(&format!("twilio://{}:{}@15551233456?apikey=bad", a, b)).apikey, None);

    // A WhatsApp source makes every target WhatsApp
    let t = twilio(&format!("twilio://_?sid={}&token={}&from=w:15551233456&to=15559876543", a, b));
    assert_eq!(t.default_mode, Mode::WhatsApp);
    assert_eq!(t.targets, vec![(Mode::WhatsApp, "+15559876543".to_string())]);

    // Short codes and calls drop WhatsApp targets
    let t = twilio(&format!("twilio://{}:{}@98765/{}/w:{}/", a, b, "4".repeat(11), "5".repeat(11)));
    assert_eq!(t.source, "98765");
    assert_eq!(t.targets, vec![(Mode::Text, format!("+{}", "4".repeat(11)))]);
    let t = twilio(&format!("twilio://{}:{}@15551233456/15559876543/w:15559876544?method=call", a, b));
    assert_eq!(t.method, Method::Call);
    assert_eq!(t.targets, vec![(Mode::Text, "+15559876543".to_string())]);
    assert_eq!((t.title_maxlen(), t.body_maxlen()), (250, 4000));
  }

  #[test]
  fn test_credentials_from_arguments() {
    let t = twilio(&format!("twilio://15551233456?user={}&password={}", sid(), "b".repeat(32)));
    assert_eq!(t.account_sid, sid());
    assert_eq!(t.auth_token, "b".repeat(32));
  }

  #[test]
  fn test_service_details() {
    let d = Twilio::static_details();
    assert_eq!(d.service_name, "Twilio");
    assert!(d.protocols.contains(&"twilio"));
  }

  #[tokio::test]
  async fn test_send_sms_and_whatsapp() {
    // Matches upstream pycapture output
    let (a, b) = (sid(), "b".repeat(32));
    let key = format!("SK{}", "c".repeat(32));
    let server = MockServer::start().await;
    for form in [
      "Body=title%0D%0Aa+%3Cb%3E+%26+c~%2A&From=%2B15551233456&To=%2B15559876543",
      "Body=title%0D%0Aa+%3Cb%3E+%26+c~%2A&From=whatsapp%3A%2B15551233456&To=whatsapp%3A%2B15559876544",
    ] {
      Mock::given(method("POST"))
        .and(path(format!("/2010-04-01/Accounts/{}/Messages.json", a)))
        .and(header("Accept", "application/json"))
        .and(header("Authorization", basic(&key, &b).as_str()))
        .and(body_string(form))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("twilio://{}:{}@15551233456/15559876543/w:15559876544?apikey={}", a, b, key)).unwrap();
    let ctx = NotifyContext { body: "title\r\na <b> & c~*".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_call_twiml() {
    let (a, b) = (sid(), "b".repeat(32));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(format!("/2010-04-01/Accounts/{}/Calls.json", a)))
      .and(header("Authorization", basic(&a, &b).as_str()))
      .and(body_string(
        "Twiml=%3CResponse%3E%3CSay%3ET%26amp%3B1%3C%2FSay%3E%3CSay%3EB%26lt%3B2%26gt%3B%3C%2FSay%3E%3C%2FResponse%3E&From=%2B15551233456&To=%2B15559876543",
      ))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("twilio://{}:{}@15551233456/15559876543?method=call", a, b)).unwrap();
    let ctx = NotifyContext { title: "T&1".into(), body: "B<2>".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // A TwiML body is sent as is and the title dropped
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("Twiml=%3CResponse%3E%3CPlay%2F%3E%3C%2FResponse%3E&From=%2B15551233456&To=%2B15559876543"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let ctx = NotifyContext { title: "T".into(), body: "  <Response><Play/></Response>".trim().into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_without_targets() {
    let (a, b) = (sid(), "b".repeat(32));
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    // A message goes to the source itself
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("Body=body&From=%2B15551233456&To=%2B15551233456"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("twilio://{}:{}@15551233456", a, b)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // A call without targets places none (and succeeds, like upstream)
    let svc = from_url(&format!("twilio://{}:{}@15551233456?method=call", a, b)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // A short code can't notify itself
    let svc = from_url(&format!("twilio://{}:{}@12345", a, b)).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let (a, b) = (sid(), "b".repeat(32));
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_string("Body=body&From=%2B15551233456&To=%2B15559876543"))
      .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"code": 21211, "message": "The 'To' number is not a valid phone number."}"#))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(body_string("Body=body&From=%2B15551233456&To=%2B15559876544"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("twilio://{}:{}@15551233456/15559876543/15559876544", a, b)).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }
}
