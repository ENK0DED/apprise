use crate::error::NotifyError;
use crate::notify::sms_targets::{is_phone_no, parse_phone_no, unquoted_host, url_credentials};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::Value;

const DEFAULT_LANG: &str = "fr_FR";
const DEFAULT_MEDIA: &str = "SMSUnicode";
const DEFAULT_VOICE: &str = "claire08s";
const DEFAULT_TIMEOUT: i64 = 2880;

pub struct Sfr {
  service_id: String,
  service_password: String,
  space_id: String,
  targets: Vec<String>,
  lang: String,
  sender: String,
  media: String,
  timeout: i64,
  voice: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// A value in one of the JSON documents SFR expects.
enum JsonValue<'a> {
  Str(&'a str),
  Int(i64),
}

/// Python's `json.dumps()` of a flat object: `", "`/`": "` separators, keys in
/// order and every non-ASCII character escaped (`ensure_ascii`).
fn py_json_dumps(fields: &[(&str, JsonValue<'_>)]) -> String {
  fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
      match c {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{08}' => out.push_str("\\b"),
        '\u{0C}' => out.push_str("\\f"),
        c if (c as u32) < 0x20 || !c.is_ascii() => {
          let mut units = [0u16; 2];
          for unit in c.encode_utf16(&mut units) {
            out.push_str(&format!("\\u{:04x}", unit));
          }
        }
        c => out.push(c),
      }
    }
    out.push('"');
    out
  }
  let body: Vec<String> = fields
    .iter()
    .map(|(k, v)| {
      let value = match v {
        JsonValue::Str(s) => quote(s),
        JsonValue::Int(i) => i.to_string(),
      };
      format!("{}: {}", quote(k), value)
    })
    .collect();
  format!("{{{}}}", body.join(", "))
}

/// `urllib.parse.urlencode()` as requests applies it to `params=`.
fn query_encode(pairs: &[(&str, &str)]) -> String {
  pairs
    .iter()
    .map(|(k, v)| format!("{}={}", urlencoding::encode(k).replace("%20", "+"), urlencoding::encode(v).replace("%20", "+")))
    .collect::<Vec<_>>()
    .join("&")
}

/// Python truthiness of a JSON value.
fn is_truthy(value: Option<&Value>) -> bool {
  match value {
    None | Some(Value::Null) => false,
    Some(Value::Bool(b)) => *b,
    Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
    Some(Value::String(s)) => !s.is_empty(),
    Some(Value::Array(a)) => !a.is_empty(),
    Some(Value::Object(o)) => !o.is_empty(),
  }
}

impl Sfr {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let (url_user, url_password) = url_credentials(url);
    // sfr://service_id:service_password@space_id/targets
    let service_id = url_user.clone().unwrap_or_default();
    let service_password = url_password.clone().unwrap_or_default();
    if service_id.is_empty() || service_password.is_empty() {
      tracing::warn!("A SFR user (serviceId) and password (servicePassword) combination was not provided.");
      return None;
    }
    let space_id = unquoted_host(url).unwrap_or_default();
    if space_id.is_empty() {
      tracing::warn!("A SFR Space ID is required.");
      return None;
    }

    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|v| !v.trim().is_empty()) {
      entries.extend(parse_phone_no([to], false));
    }
    let mut targets = Vec::new();
    for target in parse_phone_no(&entries, false) {
      match is_phone_no(&target, 10) {
        Some(phone) => targets.push(phone.full),
        None => tracing::warn!("Dropped invalid phone # ({}) specified.", target),
      }
    }
    if targets.is_empty() {
      tracing::warn!("No receiver phone number has been provided. Please provide as least one valid phone number.");
      return None;
    }

    // An argument given but left empty falls back to its default too
    let arg = |key: &str, default: &str| url.get(key).map(str::trim).filter(|v| !v.is_empty()).unwrap_or(default).to_string();
    let sender = match url.get("sender") {
      Some(sender) => sender.trim().to_string(),
      None => url.get("from").map(str::trim).unwrap_or_default().to_string(),
    };
    let timeout = url.get("timeout").and_then(|t| t.trim().parse().ok()).unwrap_or(DEFAULT_TIMEOUT);

    Some(Self {
      service_id,
      service_password,
      space_id,
      targets,
      lang: arg("lang", DEFAULT_LANG),
      sender,
      media: arg("media", DEFAULT_MEDIA),
      timeout,
      voice: arg("voice", DEFAULT_VOICE),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Société Française du Radiotéléphone",
      service_url: Some("https://www.sfr.fr/"),
      setup_url: Some("https://appriseit.com/services/sfr/"),
      protocols: vec!["sfr"],
      description: "Send SMS via SFR (France).",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Sfr {
  fn schemas(&self) -> &[&str] {
    &["sfr"]
  }
  fn service_name(&self) -> &str {
    "Société Française du Radiotéléphone"
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
    let client = build_client(self.verify_certificate)?;
    let authenticate = py_json_dumps(&[
      ("serviceId", JsonValue::Str(&self.service_id)),
      ("servicePassword", JsonValue::Str(&self.service_password)),
      ("spaceId", JsonValue::Str(&self.space_id)),
      ("lang", JsonValue::Str(&self.lang)),
    ]);
    let base = format!("{}/DmcWS/1.5.8/JsonService/MessagesUnitairesWS/addSingleCall", service_base("https://www.dmc.sfr-sh.fr"));
    let mut has_error = false;

    for target in &self.targets {
      let message = py_json_dumps(&[
        ("media", JsonValue::Str(&self.media)),
        ("textMsg", JsonValue::Str(&ctx.body)),
        ("to", JsonValue::Str(target)),
        ("from", JsonValue::Str(&self.sender)),
        ("timeout", JsonValue::Int(self.timeout)),
        ("ttsVoice", JsonValue::Str(&self.voice)),
      ]);
      // Everything travels in the query string; the body is empty
      let url = format!("{}?{}", base, query_encode(&[("authenticate", &authenticate), ("messageUnitaire", &message)]));
      tracing::debug!("SFR POST URL: {} (cert_verify={})", base, self.verify_certificate);

      let resp = match client.post(&url).header("User-Agent", APP_ID).send().await {
        Ok(resp) => resp,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending SFR:{} notification.", target);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
          continue;
        }
      };
      let status = resp.status();
      let text = resp.text().await.unwrap_or_default();
      let content: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
        tracing::debug!("Failed to parse SFR JSON response; body: {}", text.chars().take(2000).collect::<String>());
        Value::Object(Default::default())
      });
      if !matches!(status.as_u16(), 200 | 204) {
        tracing::warn!(
          "Failed to send SFR notification to {}: {}error={}.",
          target,
          status.canonical_reason().map(|r| format!("{}, ", r)).unwrap_or_default(),
          status.as_u16()
        );
        tracing::debug!("Response Details: {}", text.chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      if !is_truthy(content.get("success")) {
        tracing::warn!(
          "SFR Notification to {} was not sent by the server: server_error={}, fatal={}.",
          target,
          content.get("errorCode").map(|v| v.to_string()).unwrap_or_else(|| "UNKNOWN".into()),
          content.get("fatal").map(|v| v.to_string()).unwrap_or_else(|| "True".into())
        );
        has_error = true;
        continue;
      }
      tracing::info!("Sent SFR notification to {}.", target);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const GOOD: &str = r#"{"success": true, "reponse": 8888888}"#;
  const BAD: &str = r#"{"success": false, "errorCode": "THIS_IS_AN_ERROR", "errorDetail": "Appel api en erreur", "fatal": true, "invalidParams": true}"#;

  fn sfr(url: &str) -> Sfr {
    Sfr::from_url(&ParsedUrl::parse(url).expect("parse")).expect("from_url")
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "sfr://".to_string(),
      "sfr://:@/".to_string(),
      "sfr://:service_password".to_string(),
      "sfr://testing:serv@ice_password".to_string(),
      "sfr://testing:service_password@/5555555555".to_string(),
      "sfr://testing:service_password@12345/".to_string(),
      format!("sfr://:service_password@12345/{}", 3 * 13),
      "sfr://:service_password@space_id/targets?media=TEST".to_string(),
      "sfr://service_id:".to_string(),
      "sfr://service_id:@".to_string(),
      "sfr://service_id:@000".to_string(),
      "sfr://service_id:@000/".to_string(),
      "sfr://service_id:@000/targets".to_string(),
      "sfr://service_id:@000/targets?media=TEST".to_string(),
      // Only invalid phone numbers
      "sfr://service_id:service_password@000/12345".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "sfr://service_id:service_password@000/0000000000?from=MyApp&timeout=30",
      "sfr://service_id:service_password@000/0000000000?voice=laura8k&lang=en_US",
      "sfr://service_id:service_password@000/0000000000?media=SMS",
      "sfr://service_id:service_password@000/0000000000",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let s = sfr("sfr://srv:pwd@11111111/0000000000?media=SMSLong");
    assert_eq!(s.service_id, "srv");
    assert_eq!(s.service_password, "pwd");
    assert_eq!(s.space_id, "11111111");
    assert_eq!(s.targets, vec!["0000000000"]);
    assert_eq!(s.media, "SMSLong");
    assert_eq!(s.lang, "fr_FR");
    assert_eq!(s.sender, "");
    assert_eq!(s.timeout, 2880);
    assert_eq!(s.voice, "claire08s");

    let s = sfr("sfr://444444:other_fjv&8password@11111111/?to=6666666666,8888888888&from=MyCustomUser&timeout=abc");
    assert_eq!(s.service_id, "444444");
    assert_eq!(s.service_password, "other_fjv&8password");
    assert_eq!(s.targets, vec!["6666666666", "8888888888"]);
    assert_eq!(s.sender, "MyCustomUser");
    assert_eq!(s.media, "SMSUnicode");
    assert_eq!(s.timeout, 2880);

    // sender= wins over from=
    assert_eq!(sfr("sfr://a:b@c/0000000000?sender=X&from=Y&timeout=30").sender, "X");
    assert_eq!(sfr("sfr://a:b@c/0000000000?timeout=30").timeout, 30);
  }

  #[test]
  fn test_credentials_from_arguments() {
    let s = sfr("sfr://11111111/0000000000?user=srv&password=pwd");
    assert_eq!((s.service_id.as_str(), s.service_password.as_str()), ("srv", "pwd"));
    // ?user= turns the URL user into the password
    let s = sfr("sfr://pwd@11111111/0000000000?user=srv");
    assert_eq!((s.service_id.as_str(), s.service_password.as_str()), ("srv", "pwd"));
  }

  #[test]
  fn test_py_json_dumps() {
    assert_eq!(py_json_dumps(&[("a", JsonValue::Str("Tést\r\n\"x\"")), ("n", JsonValue::Int(10))]), r#"{"a": "T\u00e9st\r\n\"x\"", "n": 10}"#);
    assert_eq!(py_json_dumps(&[("e", JsonValue::Str("\u{1F600}"))]), r#"{"e": "\ud83d\ude00"}"#);
  }

  #[test]
  fn test_service_details() {
    let d = Sfr::static_details();
    assert_eq!(d.service_name, "Société Française du Radiotéléphone");
    assert!(d.protocols.contains(&"sfr"));
  }

  #[tokio::test]
  async fn test_send_query_payload() {
    // Matches upstream pycapture output
    let server = MockServer::start().await;
    for to in ["0612345678", "33612345679"] {
      Mock::given(method("POST"))
        .and(path("/DmcWS/1.5.8/JsonService/MessagesUnitairesWS/addSingleCall"))
        .and(query_param("authenticate", r#"{"serviceId": "srv", "servicePassword": "pw", "spaceId": "spc", "lang": "en_US"}"#))
        .and(query_param(
          "messageUnitaire",
          format!(r#"{{"media": "SMS", "textMsg": "T\u00e9st\r\nbody", "to": "{}", "from": "Me", "timeout": 10, "ttsVoice": "laura8k"}}"#, to),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(GOOD))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("sfr://srv:pw@spc/0612345678/+33%206%2012%2034%2056%2079?from=Me&media=SMS&voice=laura8k&lang=en_US&timeout=10").unwrap();
    let ctx = NotifyContext { body: "Tést\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let req = &server.received_requests().await.unwrap()[0];
    assert!(req.body.is_empty());
    assert!(req.url.query().unwrap().starts_with("authenticate=%7B%22serviceId%22%3A+%22srv%22"));
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "body".into(), ..Default::default() };
    let svc = from_url("sfr://service_id:service_password@000/0000000000").unwrap();

    // 200 but the server says it failed
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string(BAD)).mount(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // Unparseable body
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("not json")).mount(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // A 204 carries no body, so no success flag either
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(204)).mount(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    // HTTP error
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(500).set_body_string(GOOD)).mount(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    assert!(!crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.unwrap());
  }
}
