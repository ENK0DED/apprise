use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use serde_json::json;

/// Facebook Graph API version; bump this when Meta deprecates it.
const FB_GRAPH_VERSION: &str = "v21.0";

/// Default template language code.
const DEFAULT_LANGUAGE: &str = "en_US";

/// One template body parameter (`?:N=text`, `?:body=N`, `?:type=N`).
#[derive(Debug, Clone, PartialEq)]
enum Component {
  /// Fixed text
  Text(String),
  /// The notification body
  Body,
  /// The notification type (`info`, `warning`, ...)
  Type,
}

/// WhatsApp Cloud API: `whatsapp://[{template}:]{token}@{from_phone_id}/{targets}`.
pub struct WhatsApp {
  token: String,
  phone_id: String,
  /// Template name; plain text messages are sent without one
  template: Option<String>,
  language: String,
  /// `+{digits}` phone numbers and `#{digits}` group IDs
  targets: Vec<String>,
  components: Vec<Component>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Python `unquote()`: invalid UTF-8 becomes U+FFFD instead of failing.
fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| String::from_utf8_lossy(&urlencoding::decode_binary(s.as_bytes())).into_owned())
}

/// The `?:key=value` arguments in URL order (upstream `qsd:`). A repeated
/// key keeps its first position and takes the last value.
fn colon_args(raw: &str) -> Vec<(String, String)> {
  let mut out: Vec<(String, String)> = Vec::new();
  let Some((_, query)) = raw.split_once('?') else { return out };
  for pair in query.split(['&', ';']) {
    let (key, val) = pair.split_once('=').unwrap_or((pair, ""));
    // Like upstream parse_qsd(), a '+' after the first character of a key
    // reads as a space
    let key = match key.char_indices().nth(1) {
      Some((i, _)) => format!("{}{}", &key[..i], key[i..].replace('+', " ")),
      None => key.to_string(),
    };
    let Some(name) = unquote(&key).strip_prefix(':').map(String::from) else { continue };
    let val = unquote(val).trim().to_string();
    match out.iter_mut().find(|(k, _)| *k == name) {
      Some(entry) => entry.1 = val,
      None => out.push((name, val)),
    }
  }
  out
}

/// Upstream `component_key_re` (matched at the start only): a 1-based index
/// or one of the `body` / `type` mappings.
enum ComponentKey {
  Index(String),
  Body,
  Type,
}

fn component_key(s: &str) -> Option<ComponentKey> {
  let digits = s.chars().take_while(char::is_ascii_digit).count();
  if digits > 0 && !s.starts_with('0') {
    return Some(ComponentKey::Index(s[..digits].to_string()));
  }
  match s.get(..4).map(str::to_ascii_lowercase).as_deref() {
    Some("body") => Some(ComponentKey::Body),
    Some("type") => Some(ComponentKey::Type),
    _ => None,
  }
}

/// Validate the template mapping and return its components in order.
fn parse_components(args: &[(String, String)]) -> Option<Vec<Component>> {
  let mut indexes: Vec<String> = Vec::new();
  let mut components = Vec::new();
  for (key, val) in args {
    let (index, component) = match component_key(key) {
      Some(ComponentKey::Index(index)) => (index, Component::Text(val.clone())),
      Some(map) => match component_key(val) {
        Some(ComponentKey::Index(index)) => (index, if matches!(map, ComponentKey::Body) { Component::Body } else { Component::Type }),
        _ => {
          tracing::warn!("An invalid Template Component Mapping (:{}={}) was specified.", key, val);
          return None;
        }
      },
      None => {
        tracing::warn!("An invalid Template Component ID ({}) was specified.", key);
        return None;
      }
    };
    if indexes.contains(&index) {
      tracing::warn!("The Template Component index ({}) was already assigned.", key);
      return None;
    }
    indexes.push(index);
    components.push(component);
  }
  Some(components)
}

/// One match of upstream `PHONE_NO_DETECTION_RE` starting at `start`:
/// `(?:[+(][+(\s]{0,32})?[0-9][0-9()\s-]{1,32}[0-9]` followed by
/// `(?=$|[\s,+(]+[0-9])`. Returns where the match ends.
fn phone_match_at(c: &[char], start: usize) -> Option<usize> {
  let digit = |i: usize| c.get(i).is_some_and(char::is_ascii_digit);
  let lead_rest = |ch: &&char| matches!(ch, '+' | '(') || ch.is_whitespace();
  let middle = |ch: &&char| ch.is_ascii_digit() || matches!(ch, '(' | ')' | '-') || ch.is_whitespace();
  let sep = |ch: &&char| matches!(ch, ',' | '+' | '(') || ch.is_whitespace();

  // The optional prefix is greedy: try the longest one first, then none
  let mut starts = Vec::new();
  if matches!(c.get(start), Some('+' | '(')) {
    let n = c[start + 1..].iter().take(32).take_while(lead_rest).count();
    starts.extend((0..=n).rev().map(|k| start + 1 + k));
  }
  starts.push(start);

  for p in starts {
    if !digit(p) {
      continue;
    }
    let n = c[p + 1..].iter().take(32).take_while(middle).count();
    for m in (1..=n).rev() {
      let last = p + 1 + m;
      if !digit(last) {
        continue;
      }
      let end = last + 1;
      let run = c[end..].iter().take_while(sep).count();
      if end == c.len() || (run > 0 && digit(end + run)) {
        return Some(end);
      }
    }
  }
  None
}

/// Upstream `parse_phone_no()`: every phone-number-looking run in each
/// entry. An entry without any is kept, split on the list delimiters, so it
/// can be reported.
fn parse_phone_no<'a>(entries: impl IntoIterator<Item = &'a str>) -> Vec<String> {
  let mut result = Vec::new();
  for entry in entries {
    let chars: Vec<char> = entry.chars().collect();
    let mut found = Vec::new();
    let mut pos = 0;
    while pos < chars.len() {
      match phone_match_at(&chars, pos) {
        Some(end) => {
          found.push(chars[pos..end].iter().collect::<String>());
          pos = end;
        }
        None => pos += 1,
      }
    }
    if found.is_empty() {
      result.extend(entry.split(|c: char| matches!(c, '[' | ']' | ';' | ',') || c.is_whitespace()).filter(|s| !s.is_empty()).map(String::from));
    } else {
      result.extend(found);
    }
  }
  result
}

/// Upstream `is_phone_no()` (minimum length 10): the digits of a phone number.
fn is_phone_no(s: &str) -> Option<String> {
  let rest = s.strip_prefix('+').unwrap_or(s);
  if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || matches!(c, '(' | ')' | '+' | '-') || c.is_whitespace()) {
    return None;
  }
  let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
  (10..=14).contains(&digits.len()).then_some(digits)
}

/// A WhatsApp group ID: 16 or more digits, with an optional `#` prefix and
/// `@g.us` suffix.
fn group_id(s: &str) -> Option<String> {
  let s = match s.len().checked_sub(5).and_then(|i| s.get(i..).map(|tail| (i, tail))) {
    Some((i, tail)) if tail.eq_ignore_ascii_case("@g.us") => &s[..i],
    _ => s,
  };
  let gid = s.trim().trim_start_matches('#');
  (gid.len() >= 16 && gid.chars().all(|c| c.is_ascii_digit())).then(|| gid.to_string())
}

impl WhatsApp {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let mut phone_id = unquote(url.host.as_deref().unwrap_or_default());
    let (mut template, mut token) = match url.password {
      Some(ref password) => (url.user.clone().filter(|u| !u.is_empty()), password.clone()),
      None => (None, url.user.clone().unwrap_or_default()),
    };
    if let Some(t) = url.get("token").filter(|t| !t.is_empty()) {
      token = t.to_string();
    }
    if let Some(t) = url.get("template").filter(|t| !t.is_empty()) {
      template = Some(t.to_string());
    }
    let language = url.get("language").filter(|l| !l.is_empty()).or_else(|| url.get("lang").filter(|l| !l.is_empty()));
    if let Some(f) = url.get("source").filter(|f| !f.is_empty()).or_else(|| url.get("from").filter(|f| !f.is_empty())) {
      phone_id = f.to_string();
    }

    let token = token.trim().to_string();
    if token.is_empty() || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("An invalid WhatsApp Access Token was specified.");
      return None;
    }
    let phone_id = phone_id.trim().to_string();
    if phone_id.is_empty() || !phone_id.chars().all(|c| c.is_ascii_digit()) {
      tracing::warn!("An invalid WhatsApp From Phone ID ({}) was specified.", phone_id);
      return None;
    }

    // The language only matters (and is only checked) with a template
    let mut lang = DEFAULT_LANGUAGE.to_string();
    if let Some(ref name) = template {
      let name = name.trim();
      if name.is_empty() || name.chars().any(char::is_whitespace) {
        tracing::warn!("An invalid WhatsApp Template Name ({}) was specified.", name);
        return None;
      }
      template = Some(name.to_string());
      if let Some(l) = language {
        let l = l.trim();
        if l.is_empty() || l.chars().any(|c| c.is_ascii_digit() || c.is_whitespace()) {
          tracing::warn!("An invalid WhatsApp Template Language Code ({}) was specified.", l);
          return None;
        }
        lang = l.to_string();
      }
    }

    let mut entries: Vec<String> = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.extend(parse_phone_no([to]));
    }
    let mut targets = Vec::new();
    for target in parse_phone_no(entries.iter().map(String::as_str)) {
      if let Some(digits) = is_phone_no(&target) {
        targets.push(format!("+{}", digits));
      } else if let Some(gid) = group_id(&target) {
        targets.push(format!("#{}", gid));
      } else {
        tracing::warn!("Dropped invalid WhatsApp target ({}) specified.", target);
      }
    }

    let components = parse_components(&colon_args(&url.raw))?;

    Some(Self { token, phone_id, template, language: lang, targets, components, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "WhatsApp",
      service_url: Some("https://developers.facebook.com/docs/whatsapp/cloud-api/get-started"),
      setup_url: Some("https://appriseit.com/services/whatsapp/"),
      protocols: vec!["whatsapp"],
      description: "Send messages via WhatsApp Cloud API.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> serde_json::Value {
    let Some(ref name) = self.template else {
      return json!({ "messaging_product": "whatsapp", "type": "text", "text": { "body": ctx.body } });
    };
    let mut template = json!({ "name": name, "language": { "code": self.language } });
    if !self.components.is_empty() {
      let parameters: Vec<_> = self
        .components
        .iter()
        .map(|c| match c {
          Component::Text(text) => json!({ "type": "text", "text": text }),
          Component::Body => json!({ "type": "text", "text": ctx.body }),
          Component::Type => json!({ "type": "text", "text": ctx.notify_type.as_str() }),
        })
        .collect();
      template["components"] = json!([{ "type": "body", "parameters": parameters }]);
    }
    json!({ "messaging_product": "whatsapp", "type": "template", "template": template })
  }
}

#[async_trait]
impl Notify for WhatsApp {
  fn schemas(&self) -> &[&str] {
    &["whatsapp"]
  }
  fn service_name(&self) -> &str {
    "WhatsApp"
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
    1024
  }
  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no valid WhatsApp targets to notify.");
      return Ok(false);
    }
    let url = format!("{}/{}/{}/messages", service_base("https://graph.facebook.com"), FB_GRAPH_VERSION, self.phone_id);
    let client = build_client(self.verify_certificate)?;
    let mut payload = self.payload(ctx);
    let mut all_ok = true;
    for target in &self.targets {
      // Groups are stored as '#{id}'; the API wants '{id}@g.us'
      match target.strip_prefix('#') {
        Some(gid) => {
          payload["to"] = json!(format!("{}@g.us", gid));
          payload["recipient_type"] = json!("group");
        }
        None => {
          payload["to"] = json!(target);
          payload["recipient_type"] = json!("individual");
        }
      }
      let resp = match client.post(&url).header("User-Agent", APP_ID).header("Accept", "application/json").bearer_auth(&self.token).json(&payload).send().await
      {
        Ok(resp) => resp,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending WhatsApp:{} notification: {}", target, e);
          all_ok = false;
          continue;
        }
      };
      let status = resp.status().as_u16();
      if status == 200 || status == 201 {
        tracing::info!("Sent WhatsApp notification to {}.", target);
        continue;
      }
      let error = resp.json::<serde_json::Value>().await.ok().map(|v| v["error"].clone()).unwrap_or_default();
      tracing::warn!(
        "Failed to send WhatsApp notification to {}: {} error={}.",
        target,
        error["message"].as_str().unwrap_or_default(),
        error["code"].as_i64().map(|c| c.to_string()).unwrap_or_else(|| status.to_string())
      );
      all_ok = false;
    }
    Ok(all_ok)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const GROUP: &str = "120363043968066561";

  fn parse(url: &str) -> WhatsApp {
    WhatsApp::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_invalid_urls() {
    let (e, four) = ("e".repeat(32), "4".repeat(11));
    let urls: Vec<String> = vec![
      "whatsapp://".into(),
      "whatsapp://:@/".into(),
      format!("whatsapp://{}@_", "a".repeat(32)),
      // Whitespace template name
      format!("whatsapp://%20:{}@12345/{}", e, four),
      // Language codes cannot hold digits (with a template)
      format!("whatsapp://template:{}@12345/{}?lang=1234", e, four),
      format!("whatsapp://tem:{}@12345/4?lang=1x", e),
      // Template components
      format!("whatsapp://template:{}@12345/{}?:invalid=23", e, four),
      format!("whatsapp://template:{}@12345/{}?:body=", e, four),
      format!("whatsapp://template:{}@12345/{}?:1=Test&:body=1", e, four),
      format!("whatsapp://{}@12345/4?:0=x", e),
      format!("whatsapp://{}@12345/4?:header=1", e),
      format!("whatsapp://{}@12345/4?:1abc=x&:Body=1", e),
      // Token and phone ID patterns
      "whatsapp://ab_c@12345".into(),
      format!("whatsapp://{}@12a45", e),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let (e, four) = ("e".repeat(32), "4".repeat(11));
    let urls: Vec<String> = vec![
      format!("whatsapp://{}@{}", "b".repeat(32), 10u64.pow(9)),
      format!("whatsapp://{}:{}@{}/123/{}/abcd/", "a".repeat(32), "b".repeat(32), "3".repeat(5), "4".repeat(11)),
      format!("whatsapp://{}@12345/{}", e, four),
      format!("whatsapp://template:{}@12345/{}", e, four),
      format!("whatsapp://template:{}@12345/{}?lang=fr_CA", e, four),
      format!("whatsapp://{}@12345/{}?template=template&lang=fr_CA", e, four),
      format!("whatsapp://template:{}@12345/{}?:1=test&:body=3&:type=2", e, four),
      format!("whatsapp://{}:{}@123456/{}", "a".repeat(32), "b".repeat(32), four),
      format!("whatsapp://_?token={}&from={}&to={}", "d".repeat(32), "5".repeat(11), "6".repeat(11)),
      format!("whatsapp://_?token={}&source={}&to={}", "d".repeat(32), "5".repeat(11), "6".repeat(11)),
      format!("whatsapp://{}@12345/%23{}", e, GROUP),
      format!("whatsapp://{}@12345/{}/%23{}", e, four, GROUP),
      // Lenient cases upstream accepts
      format!("whatsapp://{}@12345/4?:1=", e),
      format!("whatsapp://{}@12345/4?:type=2x", e),
      format!("whatsapp://{}@12345/4?lang=1x", e),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let e = "e".repeat(32);
    let w = parse(&format!("whatsapp://{}@12345/{}", e, "4".repeat(11)));
    assert_eq!(w.token, e);
    assert_eq!(w.phone_id, "12345");
    assert_eq!(w.template, None);
    assert_eq!(w.targets, vec![format!("+{}", "4".repeat(11))]);

    // user:password means template:token
    let w = parse(&format!("whatsapp://tmpl:{}@12345?lang=fr_CA", e));
    assert_eq!(w.template.as_deref(), Some("tmpl"));
    assert_eq!(w.token, e);
    assert_eq!(w.language, "fr_CA");

    // ?language= wins over ?lang=, ?source= over ?from=, and the default is en_US
    let w = parse(&format!("whatsapp://_?token={}&from=111&source=222&template=t&lang=xx&language=yy", e));
    assert_eq!(w.phone_id, "222");
    assert_eq!(w.language, "yy");
    assert_eq!(parse(&format!("whatsapp://t:{}@1", e)).language, "en_US");
  }

  #[test]
  fn test_targets() {
    let e = "e".repeat(32);
    let w =
      parse(&format!("whatsapp://{}@1/+1%20(555)%20987-6543/%23{}/{}@g.us/%23{}@G.US/bad/%23badgroupid/123?to=4444444444,5555555555", e, GROUP, GROUP, GROUP));
    let group = format!("#{}", GROUP);
    assert_eq!(w.targets, vec!["+15559876543".to_string(), group.clone(), group.clone(), group, "+4444444444".into(), "+5555555555".into()]);
    // Whitespace-only entries are skipped
    assert!(parse(&format!("whatsapp://{}@1/%20%20/", e)).targets.is_empty());
  }

  #[test]
  fn test_parse_phone_no() {
    assert_eq!(parse_phone_no(["+1 (555) 987-6543"]), vec!["+1 (555) 987-6543"]);
    assert_eq!(parse_phone_no(["4444444444,5555555555 +16666666666"]), vec!["4444444444", "5555555555", "+16666666666"]);
    // A '#' prefix is dropped by the pattern; '@g.us' keeps the whole entry
    assert_eq!(parse_phone_no([format!("#{}", GROUP).as_str()]), vec![GROUP]);
    assert_eq!(parse_phone_no([format!("{}@g.us", GROUP).as_str()]), vec![format!("{}@g.us", GROUP)]);
    assert_eq!(parse_phone_no(["abc, def"]), vec!["abc", "def"]);
    // Reference results from upstream parse_phone_no()
    assert_eq!(parse_phone_no(["(+1) 555 1234567, +44 20 1234 5678"]), vec!["(+1) 555 1234567", "+44 20 1234 5678"]);
    assert_eq!(parse_phone_no(["1".repeat(40).as_str()]), vec!["1".repeat(34)]);
    assert_eq!(parse_phone_no(["12 34", "+(1)23", "++  1-2-3 ,4"]), vec!["12 34", "+(1)23", "++  1-2-3"]);
  }

  #[test]
  fn test_components_keep_url_order() {
    let w = parse(&format!("whatsapp://t:{}@1/4444444444?:type=2&:1=Hello&:body=3", "e".repeat(32)));
    assert_eq!(w.components, vec![Component::Type, Component::Text("Hello".into()), Component::Body]);
  }

  #[test]
  fn test_service_details() {
    let details = WhatsApp::static_details();
    assert_eq!(details.service_name, "WhatsApp");
    assert!(details.protocols.contains(&"whatsapp"));
    assert!(!details.attachment_support);
    let svc = from_url(&format!("whatsapp://{}@1/4444444444", "e".repeat(32))).unwrap();
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.body_maxlen(), 1024);
  }

  fn ctx(body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { body: body.into(), notify_type, ..Default::default() }
  }

  #[tokio::test]
  async fn test_send_text_per_target() {
    let e = "e".repeat(32);
    let server = MockServer::start().await;
    for (to, kind) in [("+4444444444", "individual"), (&*format!("{}@g.us", GROUP), "group")] {
      Mock::given(method("POST"))
        .and(path("/v21.0/12345/messages"))
        .and(header("Authorization", format!("Bearer {}", e).as_str()))
        .and(header("Accept", "application/json"))
        .and(header("Content-Type", "application/json"))
        .and(body_json(json!({ "messaging_product": "whatsapp", "to": to, "recipient_type": kind, "type": "text", "text": { "body": "T\r\nB" } })))
        .respond_with(ResponseTemplate::new(if kind == "group" { 201 } else { 200 }))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url(&format!("whatsapp://{}@12345/4444444444/%23{}", e, GROUP)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T\r\nB", NotifyType::Info))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_template() {
    let e = "e".repeat(32);
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v21.0/12345/messages"))
      .and(body_json(json!({
        "messaging_product": "whatsapp",
        "to": "+15559876543",
        "recipient_type": "individual",
        "type": "template",
        "template": {
          "name": "template",
          "language": { "code": "fr_CA" },
          "components": [{ "type": "body", "parameters": [
            { "type": "text", "text": "warning" },
            { "type": "text", "text": "Hello" },
            { "type": "text", "text": "T\r\nB" },
          ] }],
        },
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("whatsapp://template:{}@12345/+1%20(555)%20987-6543?:type=2&:1=Hello&:body=3&lang=fr_CA", e)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("T\r\nB", NotifyType::Warning))).await.unwrap());

    // Without components the template is sent bare
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(body_json(json!({
        "messaging_product": "whatsapp", "to": "+15559876543", "recipient_type": "individual", "type": "template",
        "template": { "name": "tmpl", "language": { "code": "en_US" } },
      })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url(&format!("whatsapp://tmpl:{}@12345/15559876543", e)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("B", NotifyType::Info))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let e = "e".repeat(32);
    // No targets: nothing is sent
    assert!(!from_url(&format!("whatsapp://{}@12345", e)).unwrap().send(&ctx("B", NotifyType::Info)).await.unwrap());

    // A failing target does not stop the others, but fails the send
    let server = MockServer::start().await;
    let failing = |req: &wiremock::Request| serde_json::from_slice::<serde_json::Value>(&req.body).map(|v| v["to"] == "+4444444444").unwrap_or(false);
    Mock::given(method("POST"))
      .and(failing)
      .respond_with(ResponseTemplate::new(400).set_body_json(json!({ "error": { "code": 131, "message": "bad" } })))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let svc = from_url(&format!("whatsapp://{}@12345/4444444444/5555555555", e)).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B", NotifyType::Info))).await.unwrap());

    for code in [500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code)).expect(1).mount(&server).await;
      let svc = from_url(&format!("whatsapp://{}@12345/%23{}", e, GROUP)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("B", NotifyType::Info))).await.unwrap());
    }
  }
}
