use crate::conversion::pystr::{escape_html, escape_html_quote, strip};
use crate::conversion::{Attrs, HtmlTagReducer, TagReducer, convert_between, reduce_with, reducer_end, reducer_handle_starttag, reducer_start, reducer_text};
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_bool_or, parse_list, split_path};
use crate::utils::qsd;
use async_trait::async_trait;

/// Sent as the device to reach every device of the user.
const PUSHOVER_SEND_TO_ALL: &str = "ALL_DEVICES";

/// Upstream `PUSHOVER_SOUNDS`; any other sound is passed on as a custom one.
const PUSHOVER_SOUNDS: &[&str] = &[
  "pushover",
  "bike",
  "bugle",
  "cashregister",
  "classical",
  "cosmic",
  "falling",
  "gamelan",
  "incoming",
  "intermission",
  "magic",
  "mechanical",
  "pianobar",
  "siren",
  "spacealarm",
  "tugboat",
  "alien",
  "climb",
  "persistent",
  "echo",
  "updown",
  "none",
];

/// Upstream `PUSHOVER_PRIORITY_MAP`, checked in order against the start of
/// the lower-cased `?priority=` value.
const PUSHOVER_PRIORITY_MAP: &[(&str, i32)] = &[("l", -2), ("m", -1), ("n", 0), ("h", 1), ("e", 2), ("-2", -2), ("-1", -1), ("0", 0), ("1", 1), ("2", 2)];

const PRIORITY_EMERGENCY: i32 = 2;

/// Images up to 5 MB can be attached (https://pushover.net/api#limits).
const ATTACH_MAX_SIZE_BYTES: usize = 5_242_880;

/// The line break Pushover's apps understand in HTML messages.
const PUSHOVER_HTML_BR: &str = "<br>";

/// Block tags that end with a blank line (a paragraph break).
const PUSHOVER_HTML_BLOCKS: &[&str] = &["p", "div", "blockquote", "pre", "table", "ul", "ol", "hr"];

/// Tags whose text keeps its own line breaks.
const PUSHOVER_HTML_LITERAL: &[&str] = &["pre", "code"];

/// Upstream `VALIDATE_DEVICE`: 1 to 25 letters, digits, `_` or `-`.
fn device_name(target: &str) -> Option<&str> {
  let t = target.trim();
  (!t.is_empty() && t.chars().count() <= 25 && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then_some(t)
}

/// Upstream `VALIDATE_GROUP`: a group key written as `#key` (or `%23key`).
fn group_key(target: &str) -> Option<&str> {
  let t = target.trim();
  let key = t.strip_prefix('#').or_else(|| t.strip_prefix("%23"))?;
  (!key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric())).then_some(key)
}

/// Replace the value of form field `name`, keeping its position.
fn set_field(payload: &mut [(&str, String)], name: &str, value: String) {
  if let Some(entry) = payload.iter_mut().find(|(k, _)| *k == name) {
    entry.1 = value;
  }
}

/// Python `str.splitlines()`.
fn splitlines(s: &str) -> Vec<&str> {
  let is_break = |c: char| matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}');
  let mut lines = Vec::new();
  let mut start = 0;
  let mut chars = s.char_indices().peekable();
  while let Some((i, c)) = chars.next() {
    if is_break(c) {
      lines.push(&s[start..i]);
      start = i + c.len_utf8();
      if c == '\r'
        && let Some((j, '\n')) = chars.peek().copied()
      {
        chars.next();
        start = j + 1;
      }
    }
  }
  if start < s.len() {
    lines.push(&s[start..]);
  }
  lines
}

/// Rewrite HTML so it only uses the markup Pushover supports (upstream
/// `PushoverHTMLReducer`):
///
/// - b, i, u, font color and a href are kept; strong, em and ins become
///   b, i and u.
/// - Headings become bold text on their own line.
/// - List items become "- " (or "1. ") lines.
/// - Paragraphs, quotes, code blocks and table rows become line breaks.
/// - Any other tag is dropped and only its text is kept.
/// - Every opened tag is closed, so the result is always balanced.
struct PushoverHtmlReducer {
  base: HtmlTagReducer,
  /// Depth of tags whose line breaks must be kept.
  literal: usize,
}

impl PushoverHtmlReducer {
  fn new() -> Self {
    let base = HtmlTagReducer {
      inline_map: vec![("b", "b"), ("strong", "b"), ("i", "i"), ("em", "i"), ("u", "u"), ("ins", "u")],
      skip_tags: vec!["script", "style", "head", "title"],
      line_break: PUSHOVER_HTML_BR,
      ..HtmlTagReducer::default()
    };
    Self { base, literal: 0 }
  }

  /// Leave a blank line, or just a new line inside a list.
  fn paragraph(&mut self) {
    self.line();
    let out = &mut self.base.out;
    if !out.is_empty() && self.base.lists.is_empty() && !(out.len() >= 2 && out[out.len() - 2..].iter().all(|l| l == PUSHOVER_HTML_BR)) {
      out.push(PUSHOVER_HTML_BR.to_string());
    }
  }

  fn reduce(&mut self, html: &str) -> String {
    reduce_with(self, html)
  }
}

/// The last value of attribute `name` (as Python's `dict(attrs)` keeps it).
fn last_attr<'a>(attrs: &'a Attrs, name: &str) -> Option<&'a str> {
  attrs.iter().rev().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
}

impl TagReducer for PushoverHtmlReducer {
  fn state(&mut self) -> &mut HtmlTagReducer {
    &mut self.base
  }

  fn on_reset(&mut self) {
    self.literal = 0;
  }

  fn handle_starttag(&mut self, tag: &str, attrs: &Attrs) {
    // Attribute lookups ignore empty values
    let kept: Vec<(String, Option<String>)> = attrs.iter().filter(|(_, v)| v.as_deref().is_some_and(|v| !v.is_empty())).cloned().collect();
    reducer_handle_starttag(self, tag, &kept);
  }

  fn start(&mut self, tag: &str, attrs: &Attrs) {
    if let (true, Some(href)) = (tag == "a", last_attr(attrs, "href")) {
      // Links keep only their destination
      let href = format!(" href=\"{}\"", escape_html_quote(href));
      self.base.open(tag, "a", &href);
    } else if let (true, Some(color)) = (tag == "font", last_attr(attrs, "color")) {
      // Colored text keeps only its color
      let color = format!(" color=\"{}\"", escape_html_quote(color));
      self.base.open(tag, "font", &color);
    } else if tag == "br" {
      self.base.out.push(PUSHOVER_HTML_BR.to_string());
    } else if tag == "hr" {
      // A divider becomes a paragraph break
      self.paragraph();
    } else {
      reducer_start(self, tag, attrs);
    }
    if PUSHOVER_HTML_LITERAL.contains(&tag) {
      self.literal += 1;
    }
  }

  fn end(&mut self, tag: &str) {
    if PUSHOVER_HTML_LITERAL.contains(&tag) {
      self.literal = self.literal.saturating_sub(1);
    }
    reducer_end(self, tag);
    if PUSHOVER_HTML_BLOCKS.contains(&tag) {
      self.paragraph();
    }
  }

  fn text(&mut self, data: &str) {
    if self.literal == 0 {
      // Line breaks in HTML source only count as a space
      reducer_text(self, data);
      return;
    }
    // Code keeps its line breaks as HTML line breaks
    let lines = escape_html(data).replace("\r\n", "\n");
    for (no, line) in lines.split('\n').enumerate() {
      if no > 0 {
        self.base.out.push(PUSHOVER_HTML_BR.to_string());
      }
      if !line.is_empty() {
        self.base.out.push(line.to_string());
      }
    }
  }
}

/// Encrypt one message field for Pushover E2EE (https://pushover.net/api#e2ee):
/// gzip the text, AES-256-CBC it under a random IV with PKCS7 padding, and
/// return base64(IV || ciphertext || HMAC-SHA256(IV || ciphertext)).
fn encrypt_field(plaintext: &str, key: &[u8]) -> Option<String> {
  use base64::Engine;
  use cbc::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
  use ring::rand::SecureRandom;
  use std::io::Write;

  let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
  gz.write_all(plaintext.as_bytes()).ok()?;
  let compressed = gz.finish().ok()?;

  let mut iv = [0u8; 16];
  ring::rand::SystemRandom::new().fill(&mut iv).ok()?;
  let ciphertext = cbc::Encryptor::<aes_gcm::aes::Aes256>::new_from_slices(key, &iv).ok()?.encrypt_padded_vec::<Pkcs7>(&compressed);

  let mut blob = iv.to_vec();
  blob.extend_from_slice(&ciphertext);
  let mac = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), &blob);
  blob.extend_from_slice(mac.as_ref());
  Some(base64::engine::general_purpose::STANDARD.encode(blob))
}

pub struct Pushover {
  user_key: String,
  token: String,
  devices: Vec<String>,
  groups: Vec<String>,
  priority: i32,
  sound: String,
  interval: i64,
  expire: i64,
  supplemental_url: Option<String>,
  supplemental_url_title: Option<String>,
  encryption_key: Option<Vec<u8>>,
  e2ee: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Pushover {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // pover://{user_key}@{token}/{targets}. A '#' anywhere in the path is a
    // group prefix, not a fragment
    let reparsed;
    let url = {
      let (path, query) = url.raw.split_at(url.raw.find('?').unwrap_or(url.raw.len()));
      if path.contains('#') {
        let mut fixed = ParsedUrl::parse(&format!("{}{}", path.replace('#', "%23"), query))?;
        fixed.assigned_tags = url.assigned_tags.clone();
        reparsed = fixed;
        &reparsed
      } else {
        url
      }
    };

    let Some(token) = url.host.as_deref().map(qsd::unquote).as_deref().and_then(qsd::validate_token) else {
      tracing::warn!("An invalid Pushover Access Token was specified.");
      return None;
    };
    let Some(user_key) = url.user.as_deref().map(qsd::unquote).as_deref().and_then(qsd::validate_token) else {
      tracing::warn!("An invalid Pushover User Key was specified.");
      return None;
    };

    let mut targets = split_path(url);
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.extend(qsd::parse_list(to).iter().map(|t| qsd::unquote(t)).filter(|t| !t.is_empty()));
    }
    let targets = parse_list(&targets, true, true);

    let mut devices = Vec::new();
    let mut groups = Vec::new();
    if targets.is_empty() {
      // No target reaches every device of the user
      devices.push(PUSHOVER_SEND_TO_ALL.to_string());
    }
    for target in &targets {
      if let Some(group) = group_key(target) {
        groups.push(group.to_string());
      } else if let Some(device) = device_name(target) {
        devices.push(device.to_string());
      } else {
        tracing::warn!("Dropped invalid Pushover target ({}) specified.", target);
      }
    }

    let text_arg = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(qsd::unquote);
    let sound = text_arg("sound").map(|s| s.to_lowercase()).unwrap_or_else(|| "pushover".to_string());
    if !PUSHOVER_SOUNDS.contains(&sound.as_str()) {
      tracing::debug!("Using custom sound specified ({}). ", sound);
    }

    let priority = text_arg("priority")
      .and_then(|p| {
        let p = p.to_lowercase();
        PUSHOVER_PRIORITY_MAP.iter().find(|(k, _)| p.starts_with(k)).map(|(_, v)| *v)
      })
      .unwrap_or(0);

    // The emergency retry interval is ?interval=; ?retry= is the common
    // service retry count
    let int_arg = |key: &str, default: i64| url.get(key).and_then(|v| v.trim().parse::<i64>().ok()).unwrap_or(default);
    let (interval, expire) = (int_arg("interval", 900), int_arg("expire", 3600));
    if priority == PRIORITY_EMERGENCY {
      if interval < 30 {
        tracing::warn!("Pushover emergency interval must be at least 30 seconds.");
        return None;
      }
      if !(0..=10800).contains(&expire) {
        tracing::warn!("Pushover expire must reside in the range of 0 to 10800 seconds.");
        return None;
      }
    }

    let encryption_key = match text_arg("key") {
      Some(key) => {
        let key = key.trim().to_lowercase();
        let decoded = (key.len() == 64).then(|| hex::decode(&key).ok()).flatten();
        if decoded.is_none() {
          tracing::warn!("Pushover encryption_key must be exactly 64 hex characters (256-bit AES key); got {} chars", key.chars().count());
          return None;
        }
        decoded
      }
      None => None,
    };
    let e2ee = url.get("e2ee").filter(|v| !v.is_empty()).map(|v| parse_bool_or(Some(v), false)).unwrap_or(true);

    Some(Self {
      user_key,
      token,
      devices,
      groups,
      priority,
      sound,
      interval,
      expire,
      supplemental_url: text_arg("url"),
      supplemental_url_title: text_arg("url_title"),
      encryption_key,
      e2ee,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Pushover",
      service_url: Some("https://pushover.net/"),
      setup_url: Some("https://appriseit.com/services/pushover/"),
      protocols: vec!["pover"],
      description: "Send push notifications via Pushover.",
      attachment_support: true,
    }
  }

  /// Post one message, with `attachment` as a file when it is an image.
  async fn post(&self, client: &reqwest::Client, payload: &[(&str, String)], attachment: Option<&Attachment>) -> bool {
    let target = payload.iter().find(|(k, _)| *k == "device").map(|(_, v)| v.clone()).unwrap_or_else(|| {
      let user = payload.iter().find(|(k, _)| *k == "user").map(|(_, v)| v.as_str()).unwrap_or("");
      format!("group:{}", user)
    });

    // Only images can be attached; other files are skipped
    let attachment = match attachment {
      Some(a) if !a.mime_type.to_lowercase().starts_with("image/") => {
        tracing::debug!("Ignored unsupported Pushover attachment ({}): {}", a.mime_type, a.name);
        None
      }
      Some(a) if a.data.is_empty() || a.data.len() > ATTACH_MAX_SIZE_BYTES => {
        tracing::warn!("Pushover attachment size ({}B) exceeds limit: {}", a.data.len(), a.name);
        return false;
      }
      other => other,
    };

    let url = format!("{}/1/messages.json", service_base("https://api.pushover.net"));
    tracing::debug!("Pushover POST URL: {} (cert_verify={})", url, self.verify_certificate);

    throttle().await;
    let request = client.post(&url).header("User-Agent", APP_ID).basic_auth(&self.token, Some(""));
    let request = match attachment {
      Some(a) => {
        tracing::debug!("Posting Pushover attachment {}", a.name);
        let form = payload.iter().fold(reqwest::multipart::Form::new(), |form, (k, v)| form.text(k.to_string(), v.clone()));
        request.multipart(form.part("attachment", reqwest::multipart::Part::bytes(a.data.clone()).file_name(a.name.clone())))
      }
      None => request.form(payload),
    };

    match request.send().await {
      Ok(resp) if resp.status().as_u16() == 200 => {
        tracing::info!("Sent Pushover notification to {}.", target);
        true
      }
      Ok(resp) => {
        let status = resp.status();
        let reason = if status.as_u16() == 401 { Some("Unauthorized - Invalid Token.") } else { status.canonical_reason() };
        tracing::warn!(
          "Failed to send Pushover notification to {}: {}{}error={}.",
          target,
          reason.unwrap_or(""),
          if reason.is_some() { ", " } else { "" },
          status.as_u16()
        );
        tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default());
        false
      }
      Err(e) => {
        tracing::warn!("A Connection error occurred sending Pushover:{} notification.", target);
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        false
      }
    }
  }
}

#[async_trait]
impl Notify for Pushover {
  fn schemas(&self) -> &[&str] {
    &["pover"]
  }
  fn service_name(&self) -> &str {
    "Pushover"
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
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Text, NotifyFormat::Html, NotifyFormat::Markdown]
  }
  fn body_maxlen(&self) -> usize {
    1024
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["pover".into(), self.user_key.clone(), self.token.clone()])
  }

  /// Markdown becomes HTML and HTML is reduced to the tags Pushover shows;
  /// plain text is unchanged. Markup that leaves no text (a lone `---`) is
  /// sent as escaped text, since Pushover rejects an empty message.
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    let html = match body_format {
      NotifyFormat::Markdown => convert_between(&NotifyFormat::Markdown, &NotifyFormat::Html, body),
      NotifyFormat::Html => body.to_string(),
      _ => return body.to_string(),
    };
    let reduced = PushoverHtmlReducer::new().reduce(&html);
    if !reduced.is_empty() || strip(body).is_empty() {
      return reduced;
    }
    splitlines(strip(body)).into_iter().map(escape_html).collect::<Vec<_>>().join(PUSHOVER_HTML_BR)
  }
  fn has_dialect(&self) -> bool {
    true
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.devices.is_empty() && self.groups.is_empty() {
      tracing::warn!("There were no Pushover targets to notify.");
      return Ok(false);
    }

    let key = self.encryption_key.as_deref().filter(|_| self.e2ee);
    let encrypt = |value: &str| -> Result<String, ()> {
      match key {
        Some(key) => encrypt_field(value, key).ok_or(()),
        None => Ok(value.to_string()),
      }
    };
    let failed = || {
      // Never fall back to sending what was meant to be encrypted
      tracing::warn!("Pushover E2EE encryption failed; notification not sent.");
      Ok(false)
    };

    let title = if ctx.title.is_empty() { ctx.asset.app_desc.clone() } else { ctx.title.clone() };
    let (Ok(message), Ok(title)) = (encrypt(&ctx.body), encrypt(&title)) else { return failed() };
    let mut base: Vec<(&str, String)> =
      vec![("token", self.token.clone()), ("priority", self.priority.to_string()), ("title", title), ("message", message), ("sound", self.sound.clone())];
    for (name, value) in [("url", &self.supplemental_url), ("url_title", &self.supplemental_url_title)] {
      if let Some(value) = value {
        let Ok(value) = encrypt(value) else { return failed() };
        base.push((name, value));
      }
    }
    // Declared Markdown arrives here already turned into Pushover's HTML;
    // Markdown with no declared format is sent untouched
    if ctx.body_format == NotifyFormat::Html || (ctx.body_format == NotifyFormat::Markdown && !ctx.body_passthrough) {
      base.push(("html", "1".into()));
    }
    if self.priority == PRIORITY_EMERGENCY {
      base.push(("retry", self.interval.to_string()));
      base.push(("expire", self.expire.to_string()));
    }
    if key.is_some() {
      base.push(("encrypted", "1".into()));
    }

    // One call for every device, one per delivery group
    let mut payloads: Vec<(String, Vec<(&str, String)>)> = Vec::new();
    if !self.devices.is_empty() {
      let mut payload = base.clone();
      payload.push(("user", self.user_key.clone()));
      payload.push(("device", self.devices.join(",")));
      payloads.push((format!("account:{}", self.user_key), payload));
    }
    for group in &self.groups {
      let mut payload = base.clone();
      payload.push(("user", group.clone()));
      payloads.push((format!("group:{}", group), payload));
    }

    let client = build_client(self.verify_certificate)?;

    let mut has_error = false;
    for (recipient, payload) in payloads {
      // Skip a recipient that already accepted this message
      if is_delivered(&recipient, false) {
        continue;
      }

      if ctx.attachments.is_empty() {
        if !self.post(&client, &payload, None).await {
          has_error = true;
          continue;
        }
      } else {
        // One message per attachment; later ones carry the file name
        let mut payload = payload;
        let mut attach_ok = true;
        for (no, attachment) in ctx.attachments.iter().enumerate() {
          if no > 0 || ctx.body.is_empty() {
            // Upstream sends the name unencrypted; it is encrypted with the
            // rest of the message here
            let Ok(name) = encrypt(&attachment.name) else { return failed() };
            set_field(&mut payload, "message", name);
          }
          let attachment_key = format!("attachment:{}:{}", recipient, no);
          if !is_delivered(&attachment_key, false) {
            if self.post(&client, &payload, Some(attachment)).await {
              mark_delivered(&attachment_key, false);
            } else {
              has_error = true;
              attach_ok = false;
            }
          }
          // Only the first message has a title and a sound
          set_field(&mut payload, "title", String::new());
          set_field(&mut payload, "sound", "none".into());
        }
        if !attach_ok {
          // A retry tries this recipient again
          continue;
        }
      }

      mark_delivered(&recipient, false);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use crate::notify::with_test_base;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const MESSAGES: &str = "/1/messages.json";

  fn u30() -> String {
    "u".repeat(30)
  }

  fn a30() -> String {
    "a".repeat(30)
  }

  fn pover(rest: &str) -> Pushover {
    Pushover::from_url(&ParsedUrl::parse(&format!("pover://{}@{}{}", u30(), a30(), rest)).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), body_format: format, ..Default::default() }
  }

  fn image(name: &str, size: usize) -> Attachment {
    Attachment { name: name.into(), data: vec![b'a'; size], mime_type: "image/gif".into() }
  }

  async fn messages_server(code: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(MESSAGES)).respond_with(ResponseTemplate::new(code).set_body_string(r#"{"status":1}"#)).mount(&server).await;
    server
  }

  fn form(req: &wiremock::Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&req.body).into_owned().collect()
  }

  fn field(fields: &[(String, String)], key: &str) -> Option<String> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
  }

  /// The text fields and file name of a multipart request.
  fn multipart(req: &wiremock::Request) -> (Vec<(String, String)>, Option<String>) {
    let body = String::from_utf8_lossy(&req.body).to_string();
    let mut fields = Vec::new();
    let mut file = None;
    for part in body.split("Content-Disposition: form-data; name=\"").skip(1) {
      let (name, rest) = part.split_once('"').unwrap();
      if let Some(filename) = rest.strip_prefix("; filename=\"") {
        file = Some(filename.split('"').next().unwrap().to_string());
        // The file part has no content type of its own
        assert!(rest.split("\r\n\r\n").next().unwrap().lines().all(|l| !l.to_lowercase().starts_with("content-type")), "{}", rest);
        continue;
      }
      let value = rest.split_once("\r\n\r\n").unwrap().1.rsplit_once("\r\n--").unwrap().0;
      fields.push((name.to_string(), value.to_string()));
    }
    (fields, file)
  }

  fn decrypt(value: &str, key: &[u8]) -> String {
    use base64::Engine;
    use cbc::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
    use std::io::Read;
    let blob = base64::engine::general_purpose::STANDARD.decode(value).unwrap();
    assert!(blob.len() >= 64);
    let (iv, rest) = blob.split_at(16);
    let (ciphertext, mac) = rest.split_at(rest.len() - 32);
    ring::hmac::verify(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key), &blob[..blob.len() - 32], mac).expect("HMAC");
    let compressed = cbc::Decryptor::<aes_gcm::aes::Aes256>::new_from_slices(key, iv).unwrap().decrypt_padded_vec::<Pkcs7>(ciphertext).unwrap();
    let mut out = String::new();
    flate2::read::GzDecoder::new(&compressed[..]).read_to_string(&mut out).unwrap();
    out
  }

  #[test]
  fn test_invalid_urls() {
    let (u, a) = (u30(), a30());
    for url in [
      "pover://".to_string(),
      "pover://:@/".to_string(),
      // No user key
      format!("pover://{}", a),
      format!("pover://{}@{}?priority=emergency&expire=100000", u, a),
      format!("pover://{}@{}?priority=emergency&interval=15", u, a),
      format!("pover://{}@{}?key=tooshort", u, a),
      format!("pover://{}@{}?key={}", u, a, "z".repeat(64)),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let (u, a, g) = (u30(), a30(), "g".repeat(30));
    for rest in [
      "?sound=mysound".to_string(),
      "?sound=spacealarm".to_string(),
      "?url=my-url&url_title=title".to_string(),
      String::new(),
      "/DEVICE".to_string(),
      "?to=DEVICE".to_string(),
      "/DEVICE1/Device-with-dash/".to_string(),
      format!("/{}/", "d".repeat(30)),
      format!("/%23{}", g),
      format!("/#{}/", g),
      format!("?to=%23{}", g),
      format!("/%23{}/%23{}", g, "h".repeat(30)),
      format!("/#{}/#{}/", g, "h".repeat(30)),
      format!("/DEVICE/#{}", g),
      "/%23invalid-group/".to_string(),
      "?priority=high&format=markdown".to_string(),
      "?priority=invalid".to_string(),
      "?priority=emergency".to_string(),
      "?priority=2".to_string(),
      "?priority=emergency&interval=30&expire=300".to_string(),
      "?priority=emergency&interval=invalid&expire=300".to_string(),
      "?priority=emergency&interval=30&expire=invalid".to_string(),
      "?priority=".to_string(),
      format!("?key={}", "b".repeat(64)),
      format!("?key={}&e2ee=no", "D".repeat(64)),
    ] {
      let url = format!("pover://{}@{}{}", u, a, rest);
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_url_fields() {
    let g = "g".repeat(30);
    let p = pover("/device2/device1/,,/dddddddddddddddddddddddddddddddddddd");
    assert_eq!(p.devices, vec!["device1", "device2"]);
    assert!(p.groups.is_empty());
    // No target reaches every device
    assert_eq!(pover("").devices, vec![PUSHOVER_SEND_TO_ALL]);
    let p = pover(&format!("/DEVICE/#{}", g));
    assert_eq!((p.devices.clone(), p.groups.clone()), (vec!["DEVICE".to_string()], vec![g.clone()]));
    // A '#' after a comma still starts a group, not a URL fragment
    let p = pover(&format!("/dev,#{}?to=%23zz", g));
    assert_eq!(p.devices, vec!["dev"]);
    assert_eq!(p.groups, vec![g.clone(), "zz".into()]);
    // Only invalid targets leaves none
    let p = pover("/%23invalid-group/");
    assert!(p.devices.is_empty() && p.groups.is_empty());

    for (value, expected) in [("low", -2), ("moderate", -1), ("normal", 0), ("high", 1), ("emergency", 2), ("-2", -2), ("10", 1), ("invalid", 0), ("", 0)] {
      assert_eq!(pover(&format!("?priority={}", value)).priority, expected, "{}", value);
    }
    let p = pover("?priority=emergency");
    assert_eq!((p.interval, p.expire), (900, 3600));
    let p = pover("?priority=emergency&interval=60&expire=100&sound=Bike&url=my-url&url_title=Hello%20World");
    assert_eq!((p.interval, p.expire, p.sound.as_str()), (60, 100, "bike"));
    assert_eq!((p.supplemental_url.as_deref(), p.supplemental_url_title.as_deref()), (Some("my-url"), Some("Hello World")));
    // ?retry= is the common retry count, not the emergency interval
    assert_eq!(pover("?priority=emergency&retry=10").interval, 900);

    let p = pover(&format!("?key={}", "AB".repeat(32)));
    assert_eq!(p.encryption_key, Some(vec![0xab; 32]));
    assert!(p.e2ee);
    assert!(!pover(&format!("?key={}&e2ee=no", "ab".repeat(32))).e2ee);
    assert!(pover("").encryption_key.is_none());
  }

  #[test]
  fn test_html_reducer() {
    let reduce = |html: &str| PushoverHtmlReducer::new().reduce(html);
    assert_eq!(reduce("<strong>a</strong><em>b</em><ins>c</ins><u>d</u>"), "<b>a</b><i>b</i><u>c</u><u>d</u>");
    assert_eq!(reduce("<font color=\"#ff0000\">red</font> <font>plain</font>"), "<font color=\"#ff0000\">red</font> plain");
    assert_eq!(reduce("<a href=\"http://e.com?a=1&amp;b=2\">l</a> <a name=\"x\">n</a>"), "<a href=\"http://e.com?a=1&amp;b=2\">l</a> n");
    assert_eq!(reduce("<script>bad()</script><style>x</style>hi <span>you</span>"), "hi you");
    assert_eq!(reduce("<img alt=\"pic\" src=\"x.png\" /><img src=\"y.png\">"), "pic");
    assert_eq!(reduce("a<br/>b<hr/>c"), "a<br>b<br><br>c");
    assert_eq!(reduce("<pre><code>x &lt; 1\r\ny\n</code></pre>after"), "x &lt; 1<br>y<br><br>after");
    assert_eq!(reduce("<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>"), "a | b<br>1 | 2");
    assert_eq!(
      reduce("<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul><ol><li>one</li><li>two</li></ol>end"),
      "- a<br>  - b<br>- c<br><br>1. one<br>2. two<br><br>end"
    );
    assert_eq!(reduce("<li>orphan</li>"), "- orphan");
    assert_eq!(reduce("</b>stray <b>open <i>both"), "stray <b>open <i>both</i></b>");
    assert_eq!(reduce("<b>x <i>y</b> z</i>"), "<b>x <i>y</i></b> z");
    assert_eq!(reduce("a<b/>c"), "ac");
    assert_eq!(reduce("a</ul>b"), "a<br><br>b");
    assert_eq!(reduce("<b>a</b>\n<i>b</i>"), "<b>a</b><br><i>b</i>");
    assert_eq!(reduce("   \n  "), "");
    // Empty attribute values are ignored; the last value wins
    assert_eq!(reduce("<a href=\"\">x</a> <a href=\"1\" href=\"2\">y</a>"), "x <a href=\"2\">y</a>");
    assert_eq!(reduce(&("<b>".repeat(28571) + &"</i>".repeat(28571))), "<b>".repeat(28571) + &"</b>".repeat(28571));
  }

  #[test]
  fn test_dialect_convert() {
    let p = pover("");
    assert_eq!(p.dialect_convert("<b>hi</b>", &NotifyFormat::Html), "<b>hi</b>");
    assert_eq!(p.dialect_convert("*hi*", &NotifyFormat::Markdown), "<i>hi</i>");
    assert_eq!(p.dialect_convert("a < b & c", &NotifyFormat::Text), "a < b & c");
    // Markup with no text is sent as escaped text
    assert_eq!(p.dialect_convert("<hr>\n<img src='x.png'>", &NotifyFormat::Html), "&lt;hr&gt;<br>&lt;img src='x.png'&gt;");
    assert_eq!(p.dialect_convert("---\n", &NotifyFormat::Markdown), "---");
    assert_eq!(p.dialect_convert("a\n\n---\n", &NotifyFormat::Markdown), "a");
    assert_eq!(p.dialect_convert("  \n", &NotifyFormat::Markdown), "");
    let body = "# Head\n\npara *one*\n\n- a & b\n- **bold**\n\n> quote\n\n[link](https://example.com/?a=1&b=2)";
    assert_eq!(
      p.dialect_convert(body, &NotifyFormat::Markdown),
      "<b>Head</b><br>para <i>one</i><br><br>- a &amp; b<br>- <b>bold</b><br><br>quote<br><br><a href=\"https://example.com/?a=1&amp;b=2\">link</a>"
    );
  }

  #[tokio::test]
  async fn test_send_requests() {
    // Verified against upstream with pycapture.py
    let server = messages_server(200).await;
    let p = pover("");
    assert!(with_test_base(server.uri(), p.send(&ctx("My *T*", "# Head\nHello", NotifyFormat::Text))).await.unwrap());
    let req = &requests_to(&server, "POST", MESSAGES).await[0];
    assert_eq!(req.headers.get("content-type").unwrap(), "application/x-www-form-urlencoded");
    assert_eq!(req.headers.get("user-agent").unwrap(), APP_ID);
    // The token is also sent as basic auth with an empty password
    assert_eq!(
      req.headers.get("authorization").unwrap(),
      &format!("Basic {}", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{}:", a30())))
    );
    let expected: Vec<(String, String)> = [
      ("token", a30()),
      ("priority", "0".into()),
      ("title", "My *T*".into()),
      ("message", "# Head\nHello".into()),
      ("sound", "pushover".into()),
      ("user", u30()),
      ("device", "ALL_DEVICES".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(form(req), expected);

    // Emergency messages to devices and groups
    let server = messages_server(200).await;
    let p = pover("/dev1/%23grp1/%23grp2/devB?priority=emergency&interval=60&expire=100&url=http://x&url_title=X&sound=Bike");
    assert!(with_test_base(server.uri(), p.send(&ctx("", "B", NotifyFormat::Text))).await.unwrap());
    let reqs = requests_to(&server, "POST", MESSAGES).await;
    assert_eq!(reqs.len(), 3);
    let first = form(&reqs[0]);
    let keys: Vec<&str> = first.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["token", "priority", "title", "message", "sound", "url", "url_title", "retry", "expire", "user", "device"]);
    assert_eq!(field(&first, "title").as_deref(), Some("Apprise Notifications"));
    assert_eq!((field(&first, "retry").as_deref(), field(&first, "expire").as_deref()), (Some("60"), Some("100")));
    assert_eq!((field(&first, "priority").as_deref(), field(&first, "sound").as_deref()), (Some("2"), Some("bike")));
    assert_eq!(field(&first, "device").as_deref(), Some("dev1,devB"));
    assert_eq!(field(&form(&reqs[1]), "user").as_deref(), Some("grp1"));
    assert_eq!(field(&form(&reqs[1]), "device"), None);
    assert_eq!(field(&form(&reqs[2]), "user").as_deref(), Some("grp2"));
  }

  #[tokio::test]
  async fn test_html_flag() {
    // HTML and Markdown (already converted) set html=1; text does not
    for (format, html) in [(NotifyFormat::Html, Some("1")), (NotifyFormat::Markdown, Some("1")), (NotifyFormat::Text, None)] {
      let server = messages_server(200).await;
      assert!(with_test_base(server.uri(), pover("").send(&ctx("T", "<b>hi</b>", format.clone()))).await.unwrap());
      assert_eq!(field(&form(&requests_to(&server, "POST", MESSAGES).await[0]), "html").as_deref(), html, "{:?}", format);
    }
  }

  #[tokio::test]
  async fn test_formats_through_apprise() {
    // The format follows the input: Markdown becomes Pushover's HTML
    let server = messages_server(200).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("pover://{}@{}", u30(), a30())));
    let body = "# Head\nHello **world** <b>x</b> & a_b";
    for format in [NotifyFormat::Text, NotifyFormat::Markdown, NotifyFormat::Html] {
      let c = ctx("My *T*", body, format);
      assert!(with_test_base(server.uri(), apprise.notify(&c)).await.is_success());
    }
    let sent: Vec<Vec<(String, String)>> = requests_to(&server, "POST", MESSAGES).await.iter().map(form).collect();
    assert_eq!(field(&sent[0], "message").as_deref(), Some(body));
    assert_eq!(field(&sent[0], "html"), None);
    assert_eq!(field(&sent[1], "message").as_deref(), Some("<b>Head</b><br>Hello <b>world</b> <b>x</b> &amp; a_b"));
    assert_eq!(field(&sent[1], "html").as_deref(), Some("1"));
    assert_eq!(field(&sent[2], "message").as_deref(), Some("# Head<br>Hello **world** <b>x</b> &amp; a_b"));
    assert_eq!(field(&sent[2], "html").as_deref(), Some("1"));
    // The title is not converted
    assert_eq!(field(&sent[1], "title").as_deref(), Some("My *T*"));

    // ?format=html with markup only
    let server = messages_server(200).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("pover://{}@{}?format=html", u30(), a30())));
    assert!(with_test_base(server.uri(), apprise.notify(&ctx("T", "<hr/>", NotifyFormat::Html))).await.is_success());
    assert_eq!(field(&form(&requests_to(&server, "POST", MESSAGES).await[0]), "message").as_deref(), Some("&lt;hr/&gt;"));
  }

  #[tokio::test]
  async fn test_passthrough_markdown_is_not_flagged_as_html() {
    // What upstream sends for notify(body_format=None), then "markdown"
    let body = "**b** <i>x</i> & y";
    for (query, passthrough, message, html) in [
      ("", true, body, None),
      ("", false, "<b>b</b> <i>x</i> &amp; y", Some("1")),
      ("?format=markdown", true, body, None),
      ("?format=markdown", false, "<b>b</b> <i>x</i> &amp; y", Some("1")),
      ("?format=html", true, body, Some("1")),
      ("?format=text", true, body, None),
    ] {
      let server = messages_server(200).await;
      let mut apprise = crate::Apprise::new();
      assert!(apprise.add(&format!("pover://{}@{}{query}", u30(), a30())));
      let format = if passthrough { NotifyFormat::Text } else { NotifyFormat::Markdown };
      let c = NotifyContext { body_passthrough: passthrough, ..ctx("T", body, format) };
      assert!(with_test_base(server.uri(), apprise.notify(&c)).await.is_success());
      let sent = form(&requests_to(&server, "POST", MESSAGES).await[0]);
      assert_eq!((field(&sent, "message").as_deref(), field(&sent, "html").as_deref()), (Some(message), html), "{query} passthrough={passthrough}");
    }
  }

  #[tokio::test]
  async fn test_markdown_split_keeps_tags_balanced() {
    let server = messages_server(200).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("pover://{}@{}?overflow=split", u30(), a30())));
    assert!(with_test_base(server.uri(), apprise.notify(&ctx("", &"**bold** word ".repeat(100), NotifyFormat::Markdown))).await.is_success());
    let sent: Vec<String> = requests_to(&server, "POST", MESSAGES).await.iter().filter_map(|r| field(&form(r), "message")).collect();
    assert!(sent.len() > 1);
    for message in sent {
      assert!(message.chars().count() <= 1024);
      assert_eq!(message.matches("<b>").count(), message.matches("</b>").count());
    }
  }

  #[tokio::test]
  async fn test_attachments() {
    let server = messages_server(200).await;
    let p = pover("/%23grp1");
    let mut c = ctx("T", "B", NotifyFormat::Text);
    c.attachments = vec![image("a.png", 10), Attachment { name: "notes.txt".into(), data: b"hi".to_vec(), mime_type: "text/plain".into() }, image("b.gif", 10)];
    assert!(with_test_base(server.uri(), p.send(&c)).await.unwrap());
    let reqs = requests_to(&server, "POST", MESSAGES).await;
    assert_eq!(reqs.len(), 3);

    // The first message keeps the title, body and sound
    let (fields, file) = multipart(&reqs[0]);
    assert_eq!(file.as_deref(), Some("a.png"));
    assert!(reqs[0].headers.get("content-type").unwrap().to_str().unwrap().starts_with("multipart/form-data"));
    assert_eq!(
      (field(&fields, "title").as_deref(), field(&fields, "message").as_deref(), field(&fields, "sound").as_deref()),
      (Some("T"), Some("B"), Some("pushover"))
    );
    // A file Pushover cannot show is left off; its name is the message
    let plain = form(&reqs[1]);
    assert_eq!(
      (field(&plain, "title").as_deref(), field(&plain, "message").as_deref(), field(&plain, "sound").as_deref()),
      (Some(""), Some("notes.txt"), Some("none"))
    );
    let (fields, file) = multipart(&reqs[2]);
    assert_eq!(file.as_deref(), Some("b.gif"));
    assert_eq!(field(&fields, "message").as_deref(), Some("b.gif"));
    assert_eq!(field(&fields, "user").as_deref(), Some("grp1"));

    // Without a body the first message is the file name
    let server = messages_server(200).await;
    let mut c = ctx("T", "", NotifyFormat::Text);
    c.attachments = vec![image("a.png", 10)];
    assert!(with_test_base(server.uri(), pover("").send(&c)).await.unwrap());
    let (fields, _) = multipart(&requests_to(&server, "POST", MESSAGES).await[0]);
    assert_eq!((field(&fields, "title").as_deref(), field(&fields, "message").as_deref()), (Some("T"), Some("a.png")));

    // The size limit
    let server = messages_server(200).await;
    let mut c = ctx("", "test", NotifyFormat::Text);
    c.attachments = vec![image("test.jpg", ATTACH_MAX_SIZE_BYTES)];
    assert!(with_test_base(server.uri(), pover("").send(&c)).await.unwrap());
    c.attachments = vec![image("test.jpg", ATTACH_MAX_SIZE_BYTES + 1)];
    assert!(!with_test_base(server.uri(), pover("").send(&c)).await.unwrap());
    c.attachments = vec![image("empty.jpg", 0)];
    assert!(!with_test_base(server.uri(), pover("").send(&c)).await.unwrap());
    assert_eq!(requests_to(&server, "POST", MESSAGES).await.len(), 1);
  }

  #[tokio::test]
  async fn test_failures() {
    let c = ctx("", "test", NotifyFormat::Text);
    for code in [500, 401, 999] {
      let server = messages_server(code).await;
      assert!(!with_test_base(server.uri(), pover("").send(&c)).await.unwrap());
      let mut with_file = c.clone();
      with_file.attachments.push(image("a.gif", 10));
      assert!(!with_test_base(server.uri(), pover("").send(&with_file)).await.unwrap());
    }
    assert!(!with_test_base("http://127.0.0.1:1", pover("").send(&c)).await.unwrap());
    // No valid target
    let server = messages_server(200).await;
    assert!(!with_test_base(server.uri(), pover("/%23invalid-group/").send(&c)).await.unwrap());
    assert!(requests_to(&server, "POST", MESSAGES).await.is_empty());
  }

  #[test]
  fn test_encrypt_field() {
    let key = [0xab_u8; 32];
    for text in ["hello world", ""] {
      let encrypted = encrypt_field(text, &key).unwrap();
      assert_eq!(decrypt(&encrypted, &key), text);
    }
    // A fresh IV every time
    assert_ne!(encrypt_field("x", &key), encrypt_field("x", &key));
  }

  #[tokio::test]
  async fn test_e2ee() {
    let key = vec![0xab_u8; 32];
    let server = messages_server(200).await;
    let p = pover(&format!("/%23grp?key={}&url=https://example.com&url_title=Click", "ab".repeat(32)));
    let mut c = ctx("World", "Hello", NotifyFormat::Text);
    assert!(with_test_base(server.uri(), p.send(&c)).await.unwrap());
    let sent = form(&requests_to(&server, "POST", MESSAGES).await[0]);
    assert_eq!(field(&sent, "encrypted").as_deref(), Some("1"));
    for (name, plain) in [("message", "Hello"), ("title", "World"), ("url", "https://example.com"), ("url_title", "Click")] {
      let value = field(&sent, name).unwrap();
      assert_ne!(value, plain);
      assert_eq!(decrypt(&value, &key), plain, "{}", name);
    }
    // Everything else stays readable
    assert_eq!((field(&sent, "sound").as_deref(), field(&sent, "user").as_deref()), (Some("pushover"), Some("grp")));

    // The file names of later attachments are encrypted too
    let server = messages_server(200).await;
    c.attachments = vec![image("a.png", 10), image("b.png", 10)];
    assert!(with_test_base(server.uri(), p.send(&c)).await.unwrap());
    let reqs = requests_to(&server, "POST", MESSAGES).await;
    let (first, _) = multipart(&reqs[0]);
    let (second, _) = multipart(&reqs[1]);
    assert_eq!(decrypt(&field(&first, "message").unwrap(), &key), "Hello");
    assert_eq!(decrypt(&field(&second, "message").unwrap(), &key), "b.png");

    // e2ee=no sends the message as written
    let server = messages_server(200).await;
    let p = pover(&format!("?key={}&e2ee=no", "ab".repeat(32)));
    assert!(with_test_base(server.uri(), p.send(&ctx("ptitle", "plaintext", NotifyFormat::Text))).await.unwrap());
    let sent = form(&requests_to(&server, "POST", MESSAGES).await[0]);
    assert_eq!(field(&sent, "encrypted"), None);
    assert_eq!((field(&sent, "message").as_deref(), field(&sent, "title").as_deref()), (Some("plaintext"), Some("ptitle")));
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_recipients() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(MESSAGES))
      .and(wiremock::matchers::body_string_contains("user=grp2"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(MESSAGES)).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("pover://{}@{}/dev1/%23grp1/%23grp2?retry=1&wait=0", u30(), a30())));
    assert!(with_test_base(server.uri(), apprise.notify(&ctx("", "body", NotifyFormat::Text))).await.is_success());
    let users: Vec<String> = requests_to(&server, "POST", MESSAGES).await.iter().filter_map(|r| field(&form(r), "user")).collect();
    assert_eq!(users, vec![u30(), "grp1".into(), "grp2".into(), "grp2".into()]);
  }
}
