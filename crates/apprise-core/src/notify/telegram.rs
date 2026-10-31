//! Telegram bot notifications (upstream `apprise/plugins/telegram.py`).
//!
//! Messages go to `sendMessage` as HTML or Telegram Markdown (v1 or v2);
//! attachments to the endpoint matching their type, optionally grouped into
//! photo/video albums; and a `?template=` file turns the message into a
//! Rich Message (`sendRichMessage`).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use async_trait::async_trait;
use regex_lite::Regex;
use serde_json::{Map, Value, json};

use crate::asset::NotifyImageSize;
use crate::attachment::{AttachSource, parse_attachment};
use crate::conversion::pystr;
use crate::conversion::{
  Attrs, EmphasisDelimiter, HtmlTagReducer, TagReducer, commonmark_emphasis_run, commonmark_find_backtick_run, commonmark_headings_to_bold,
  commonmark_index_backtick_runs, commonmark_match_emphasis, commonmark_new_scan_budget, commonmark_pick_emphasis_sentinel, commonmark_scan_angle_dest,
  commonmark_scan_autolink_dest, commonmark_scan_paren_dest, reduce_with, reducer_end, reducer_start, reducer_text,
};
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::plugin_args::{parse_list, qsd_bool};
use crate::utils::templates::{TemplateType, apply_template};

/// The image size sent with `?image=yes` (upstream `TELEGRAM_IMAGE_XY`).
const TELEGRAM_IMAGE_XY: NotifyImageSize = NotifyImageSize::Xy256;

/// A Telegram album carries between 2 and 10 photos and/or videos.
const TELEGRAM_MEDIA_GROUP_MIN: usize = 2;
const TELEGRAM_MEDIA_GROUP_MAX: usize = 10;

/// Telegram's upload limits: photos up to 10 MB, every other file 50 MB.
const TELEGRAM_PHOTO_MAX_BYTES: usize = 10_000_000;
const TELEGRAM_FILE_MAX_BYTES: usize = 50_000_000;

/// A body shorter than this rides along as the first attachment's caption.
const TELEGRAM_CAPTION_MAXLEN: usize = 1024;

/// Rich Message templates larger than this are refused.
const TELEGRAM_TEMPLATE_MAX_BYTES: usize = 35000;

/// The line break used in Telegram HTML messages; Telegram has no `<br>`.
const TELEGRAM_HTML_BR: &str = "\r\n";

/// Formatting tags Telegram accepts, and the tag written for each one.
const TELEGRAM_HTML_INLINE_MAP: &[(&str, &str)] =
  &[("b", "b"), ("strong", "b"), ("i", "i"), ("em", "i"), ("u", "u"), ("ins", "u"), ("s", "s"), ("strike", "s"), ("del", "s"), ("tg-spoiler", "tg-spoiler")];

/// Heading tags; they become bold text on a line of their own.
const TELEGRAM_HTML_HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6", "title"];

/// Block tags that start and end their own line.
const TELEGRAM_HTML_BLOCKS: &[&str] = &["p", "div", "table", "hr"];

/// Upstream `__telegram_escape_html_entries`: the substitutions that make
/// HTML with no declared format acceptable to Telegram, applied in order.
/// Declared HTML goes through [`reduce_html`] instead.
const TELEGRAM_ESCAPE_HTML_ENTRIES: &[(&str, &str)] = &[
  // Comments
  (r"\s*<!.+?-->\s*", ""),
  // The following tags are not supported
  (r"\s*<\s*(!?DOCTYPE|p|div|span|body|script|link|meta|html|font|head|label|form|input|textarea|select|iframe|source|script)([^a-z0-9>][^>]*)?>\s*", ""),
  // All closing tags to be removed are put here
  (r"\s*<\s*/(span|body|script|meta|html|font|head|label|form|input|textarea|select|ol|ul|link|iframe|source|script)([^a-z0-9>][^>]*)?>\s*", ""),
  // Bold
  (r"<\s*(strong)([^a-z0-9>][^>]*)?>", "<b>"),
  (r"<\s*/\s*(strong)([^a-z0-9>][^>]*)?>", "</b>"),
  (r"\s*<\s*(h[1-6]|title)([^a-z0-9>][^>]*)?>\s*", "<b>"),
  (r"\s*<\s*/\s*(h[1-6]|title)([^a-z0-9>][^>]*)?>\s*", "</b>"),
  // Italic
  (r"<\s*(caption|em)([^a-z0-9>][^>]*)?>", "<i>"),
  (r"<\s*/\s*(caption|em)([^a-z0-9>][^>]*)?>", "</i>"),
  // Bullet lists
  (r"<\s*li([^a-z0-9>][^>]*)?>\s*", " -"),
  // New lines
  (r"\s*<\s*/?\s*(ol|ul|br|hr)\s*/?>\s*", "\r\n"),
  (r"\s*<\s*/\s*(br|p|hr|li|div)([^a-z0-9>][^>]*)?>\s*", "\r\n"),
  // HTML spaces (&nbsp;) and tabs (&emsp;) are not supported; tabs
  // become three spaces
  (r"\&nbsp;?", " "),
  (r"\&emsp;?", "   "),
  // Telegram escapes these again, so they are turned back
  (r"\&apos;?", "'"),
  (r"\&quot;?", "\""),
  // New line cleanup
  (r"\r*\n[\r\n]+", "\r\n"),
];

static TELEGRAM_ESCAPE_HTML_RES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
  TELEGRAM_ESCAPE_HTML_ENTRIES.iter().map(|(pattern, with)| (Regex::new(&py_ignorecase_pattern(pattern)).expect("Telegram HTML pattern"), *with)).collect()
});

/// One of upstream's `re.I | re.S` patterns for regex-lite, whose `\s` is
/// ASCII and whose case folding is ASCII only. Python's `\s` also matches
/// Unicode spaces, and under `re.I` the letters i, k and s also match
/// İ, ı, K and ſ, so those are spelled out.
fn py_ignorecase_pattern(pattern: &str) -> String {
  const NOT_TAG_CHAR: &str = "[^a-z0-9>]";
  let mut out = String::from("(?is)");
  let mut rest = pattern;
  while let Some(c) = rest.chars().next() {
    if let Some(after) = rest.strip_prefix(NOT_TAG_CHAR) {
      out.push_str(r"[^a-zA-Z0-9>\x{130}\x{131}\x{17F}\x{212A}]");
      rest = after;
      continue;
    }
    rest = &rest[c.len_utf8()..];
    match c {
      '\\' => {
        let Some(escaped) = rest.chars().next() else { break };
        rest = &rest[escaped.len_utf8()..];
        if escaped == 's' {
          out.push_str(r"[\t\n\x0B\x0C\r\x1C-\x20\x{85}\x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}]");
        } else {
          out.push('\\');
          out.push(escaped);
        }
      }
      'i' | 'I' => out.push_str(r"[iI\x{130}\x{131}]"),
      'k' | 'K' => out.push_str(r"[kK\x{212A}]"),
      's' | 'S' => out.push_str(r"[sS\x{17F}]"),
      c => out.push(c),
    }
  }
  out
}

/// Strip and rewrite the HTML Telegram rejects in a body with no declared
/// format (upstream's `__telegram_escape_html_entries` loop in `send()`).
fn escape_undeclared_html(body: &str) -> String {
  TELEGRAM_ESCAPE_HTML_RES.iter().fold(body.to_string(), |body, (re, with)| re.replace_all(&body, regex_lite::NoExpand(with)).into_owned())
}

/// MarkdownV2 reserved characters escaped in literal text.
const TELEGRAM_STRICT_CHARS: &str = "~#+=|{}.!<>-";

/// Legacy Markdown recognizes escapes only for these characters.
const TELEGRAM_V1_ESCAPABLE: &str = "`*_[]";

/// The full MarkdownV2 reserved set, escaped inside autolinks.
const TELEGRAM_RESERVED_FULL: &str = "~#+=|{}.!<>-_*[]()`";

/// The Telegram Markdown dialect (`?mdv=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkdownVersion {
  /// Classic Telegram Markdown.
  One,
  /// MarkdownV2, with strikethrough and stricter escaping.
  Two,
}

impl MarkdownVersion {
  /// Upstream `TELEGRAM_MARKDOWN_VERSION_MAP`, checked in order against the
  /// start of the lower-cased value; anything else is the `v1` default.
  fn parse(value: Option<&str>) -> Self {
    const MAP: &[(&str, MarkdownVersion)] = &[
      ("v1", MarkdownVersion::One),
      ("1", MarkdownVersion::One),
      ("v2", MarkdownVersion::Two),
      ("2", MarkdownVersion::Two),
      ("default", MarkdownVersion::Two),
    ];
    let Some(value) = value else { return Self::One };
    let value = value.to_lowercase();
    MAP.iter().find(|(k, _)| value.starts_with(k)).map(|(_, v)| *v).unwrap_or(Self::One)
  }

  /// The `parse_mode` Telegram expects.
  fn parse_mode(self) -> &'static str {
    match self {
      Self::One => "MARKDOWN",
      Self::Two => "MarkdownV2",
    }
  }
}

/// Where the message text goes relative to the attachments (`?content=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
  Before,
  After,
}

/// A chat to message.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChatId {
  /// A numeric chat or group ID, sent as a JSON number.
  Id(i128),
  /// A `@channel` name, or a detected bot owner (sent as a string, as
  /// upstream stores it).
  Text(String),
}

impl ChatId {
  fn to_json(&self) -> Value {
    match self {
      // IDs too large for a JSON number go out as text.
      Self::Id(id) => serde_json::Number::from_i128(*id).map(Value::Number).unwrap_or_else(|| Value::String(id.to_string())),
      Self::Text(s) => Value::String(s.clone()),
    }
  }
}

impl std::fmt::Display for ChatId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Id(id) => write!(f, "{id}"),
      Self::Text(s) => f.write_str(s),
    }
  }
}

/// A chat and the topic (forum thread) to post in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
  chat_id: ChatId,
  /// `None` (or upstream's falsy 0) posts outside any topic.
  topic: Option<i64>,
}

impl Target {
  /// The chat as written in logs (`chat` or `chat:topic`).
  fn printable(&self) -> String {
    match self.topic {
      Some(topic) => format!("{}:{}", self.chat_id, topic),
      None => self.chat_id.to_string(),
    }
  }

  /// The delivery tracking key for this chat.
  fn key(&self) -> String {
    let kind = if matches!(self.chat_id, ChatId::Id(_)) { "id" } else { "text" };
    format!("{kind}:{}", self.printable())
  }
}

/// How an attachment travels when album mode is on (upstream `TelegramMediaKind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
  Photo,
  Video,
  /// Sent in its own message.
  Single,
}

impl MediaKind {
  fn as_str(self) -> &'static str {
    match self {
      Self::Photo => "photo",
      Self::Video => "video",
      Self::Single => "single",
    }
  }
}

/// The outcome of an album upload (upstream `TelegramGroupResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupResult {
  /// Telegram accepted the album.
  Ok,
  /// Telegram refused the album; its items may still go out one by one.
  Retry,
  /// The album could not be delivered at all.
  Fail,
}

/// The caption a message body becomes when it fits on an attachment.
#[derive(Debug, Clone)]
struct Caption {
  caption: String,
  show_caption_above_media: bool,
  parse_mode: &'static str,
}

/// What [`Telegram::send_media`] uploads.
#[derive(Clone, Copy)]
enum Media<'a> {
  /// The notification type's image (`?image=yes`).
  Image,
  /// An attachment, on the endpoint for its type.
  Attachment(&'a Attachment),
  /// An attachment sent as a plain document.
  Document(&'a Attachment),
}

/// One file to upload.
struct Upload<'a> {
  name: String,
  data: &'a [u8],
}

/// Python `str()` of a form value, as `requests` writes it in a multipart body.
fn py_bool(value: bool) -> &'static str {
  if value { "True" } else { "False" }
}

/// Python `int()` of a string: surrounding whitespace, a sign, and single
/// underscores between digits are allowed.
fn py_int(value: &str) -> Option<i64> {
  let s = pystr::strip(value);
  let (sign, digits) = match s.strip_prefix('-') {
    Some(rest) => ("-", rest),
    None => ("", s.strip_prefix('+').unwrap_or(s)),
  };
  if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") || !digits.chars().all(|c| c.is_ascii_digit() || c == '_') {
    return None;
  }
  format!("{sign}{}", digits.replace('_', "")).parse().ok()
}

/// Upstream's `parse_url()` hack: a bot token holds a colon, which is not a
/// valid URL port, so the URL is matched by hand.
fn token_url_regex() -> &'static Regex {
  static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
  RE.get_or_init(|| Regex::new(r"(?i)^(tgram://)(bot)?(([a-z0-9_-]+)(:[a-z0-9_-]+)?@)?([0-9]+)(:|%3A)+(.*)$").unwrap())
}

/// Upstream `IS_CHAT_ID_RE`.
fn chat_id_regex() -> &'static Regex {
  static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
  RE.get_or_init(|| Regex::new(r"(?i)^((-?[0-9]{1,32})|(@|%40)?([a-z_-][a-z0-9_-]+))((:|%3A)([0-9]+))?$").unwrap())
}

/// Upstream's bot token check (`^(bot)?(?P<key>[0-9]+:[a-z0-9_-]+)$`).
fn bot_token_regex() -> &'static Regex {
  static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
  RE.get_or_init(|| Regex::new(r"(?i)^(bot)?([0-9]+:[a-z0-9_-]+)$").unwrap())
}

pub struct Telegram {
  bot_token: String,
  targets: Vec<Target>,
  /// The default topic (`?topic=` / `?thread=`).
  topic: Option<i64>,
  detect_owner: bool,
  include_image: bool,
  silent: bool,
  preview: bool,
  album: bool,
  content: Placement,
  markdown_ver: MarkdownVersion,
  /// The Rich Message template (`?template=`).
  template: Option<AttachSource>,
  /// Rich Message template tokens (`?:key=value`).
  tokens: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
  /// The bot owner found by `getUpdates` when no chat was given; upstream
  /// adds it to its targets so later notifications skip the lookup.
  owner: Mutex<Option<Target>>,
}

impl Telegram {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Pull the bot token out of the URL by hand, then parse the rest as a
    // normal URL with the token's first half as the host.
    let raw = url.raw.trim();
    let Some(caps) = token_url_regex().captures(raw) else {
      tracing::error!("Unparseable Telegram URL");
      return None;
    };
    let group = |i: usize| caps.get(i).map(|m| m.as_str()).unwrap_or("");
    let rebuilt = format!("{}{}{}/{}", group(1), group(3), group(6), group(8));
    let reparsed = ParsedUrl::parse(&rebuilt)?;

    // The first path entry is the second half of the token.
    let mut entries = reparsed.path_parts.clone();
    if entries.is_empty() {
      tracing::warn!("The Telegram Bot Token specified is invalid.");
      return None;
    }
    let token = format!("{}:{}", unquote(reparsed.host.as_deref().unwrap_or("")), entries.remove(0));
    let Some(bot_token) = bot_token_regex().captures(pystr::strip(&token)).and_then(|c| c.get(2)).map(|m| m.as_str().to_string()) else {
      tracing::warn!("The Telegram Bot Token specified is invalid.");
      return None;
    };

    // The remaining entries (and ?to=) are the chats.
    let mut raw_targets = entries;
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw_targets.extend(parse_list([to], true, true));
    }

    let content = match url.get("content").filter(|c| !c.is_empty()) {
      None => Placement::Before,
      Some(c) => match c.to_lowercase().as_str() {
        "before" => Placement::Before,
        "after" => Placement::After,
        _ => {
          tracing::warn!("The content placement specified ({}) is invalid.", c);
          return None;
        }
      },
    };

    let topic = match url.get("topic").filter(|t| !t.is_empty()).or_else(|| url.get("thread").filter(|t| !t.is_empty())) {
      None => None,
      Some(t) => match py_int(t) {
        Some(t) => Some(t).filter(|t| *t != 0),
        None => {
          tracing::warn!("The Telegram Topic ID specified ({}) is invalid.", t);
          return None;
        }
      },
    };

    let markdown_ver = MarkdownVersion::parse(url.get("mdv").filter(|m| !m.is_empty()));

    // Owner detection defaults on when no chat was given at all.
    let mut detect_owner = qsd_bool(url, "detect", raw_targets.is_empty());

    let mut targets = Vec::new();
    for target in parse_list(&raw_targets, true, true) {
      let Some(caps) = chat_id_regex().captures(&target) else {
        tracing::warn!("Dropped invalid Telegram chat/group ({}) specified.", target);
        // Do not fall back to owner detection.
        detect_owner = false;
        continue;
      };
      let chat_topic = match caps.get(7) {
        Some(t) => t.as_str().parse::<i64>().ok().filter(|t| *t != 0),
        None => topic,
      };
      let chat_id = match (caps.get(2), caps.get(4)) {
        (Some(id), _) => match id.as_str().parse::<i128>() {
          Ok(id) => ChatId::Id(id),
          Err(_) => continue,
        },
        (None, Some(name)) => ChatId::Text(format!("@{}", name.as_str())),
        (None, None) => continue,
      };
      targets.push(Target { chat_id, topic: chat_topic });
    }

    let template = match url.get("template").filter(|t| !t.is_empty()) {
      None => None,
      Some(t) => match parse_attachment(&unquote(t)) {
        Ok(source) => Some(source),
        Err(_) => {
          tracing::warn!("The Telegram Rich Message template could not be loaded.");
          return None;
        }
      },
    };
    let tokens = url.qsd_colon.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

    Some(Self {
      bot_token,
      targets,
      topic,
      detect_owner,
      include_image: qsd_bool(url, "image", false),
      silent: qsd_bool(url, "silent", false),
      preview: qsd_bool(url, "preview", false),
      album: qsd_bool(url, "album", false),
      content,
      markdown_ver,
      template,
      tokens,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      owner: Mutex::new(None),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Telegram",
      service_url: Some("https://telegram.org/"),
      setup_url: Some("https://appriseit.com/services/telegram/"),
      protocols: vec!["tgram"],
      description: "Send Telegram messages, attachments and Rich Messages via a bot.",
      attachment_support: true,
    }
  }

  fn api_url(&self, method: &str) -> String {
    format!("{}/bot{}/{}", service_base("https://api.telegram.org"), self.bot_token, method)
  }

  /// The chats to message: the configured ones, else the detected owner.
  fn current_targets(&self) -> Vec<Target> {
    if !self.targets.is_empty() {
      return self.targets.clone();
    }
    self.owner.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
  }

  /// Find the bot owner when no chat was given (upstream `send()` preamble).
  async fn resolve_owner(&self, client: &reqwest::Client) {
    if !self.targets.is_empty() || !self.detect_owner || self.owner.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
      return;
    }
    let stored = crate::storage::with_service_store(|s| s.get("bot_owner")).flatten().and_then(|v| owner_text(&v));
    let id = match stored {
      Some(id) => Some(id),
      None => self.detect_bot_owner(client).await,
    };
    if let Some(id) = id {
      tracing::info!("Update your Telegram Apprise URL to include the detected chat {}.", id);
      *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(Target { chat_id: ChatId::Text(id), topic: self.topic });
    }
  }

  /// Ask `getUpdates` who last messaged the bot (upstream `detect_bot_owner`).
  async fn detect_bot_owner(&self, client: &reqwest::Client) -> Option<String> {
    let resp = match client.post(self.api_url("getUpdates")).header("User-Agent", APP_ID).header(reqwest::header::CONTENT_TYPE, "application/json").send().await
    {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A connection error occurred detecting the Telegram User.");
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return None;
      }
    };
    let status = resp.status();
    let content = resp.bytes().await.unwrap_or_default();
    if status.as_u16() != 200 {
      match error_description(&content) {
        Some(msg) => tracing::warn!("Failed to detect the Telegram user: ({}) {}.", status.as_u16(), msg),
        None => tracing::warn!("Failed to detect the Telegram user: {}.", status_text(status)),
      }
      return None;
    }
    let Ok(response) = serde_json::from_slice::<Value>(&content) else {
      tracing::warn!("A communication error occurred detecting the Telegram User.");
      return None;
    };
    if response.get("ok").is_some_and(truthy) {
      for entry in response.get("result").and_then(Value::as_array).into_iter().flatten() {
        if let Some(from) = entry.get("message").and_then(|m| m.get("from")) {
          let id = from.get("id").cloned().unwrap_or(json!(0));
          let user = from.get("first_name").and_then(|n| n.as_str()).unwrap_or("None");
          tracing::info!("Detected Telegram user {} (userid={})", user, id);
          crate::storage::with_service_store(|s| s.set("bot_owner", id.clone(), None, true));
          return owner_text(&id);
        }
      }
    }
    tracing::warn!("Failed to detect a Telegram user; try sending your bot a message first.");
    None
  }

  /// Upload an attachment or the notification image (upstream `send_media`).
  async fn send_media(
    &self,
    client: &reqwest::Client,
    ctx: &NotifyContext,
    target: &Target,
    caption: Option<&Caption>,
    title: Option<&str>,
    media: Media<'_>,
  ) -> bool {
    let image;
    let (attachment, document) = match media {
      Media::Image => (None, false),
      Media::Attachment(a) => (Some(a), false),
      Media::Document(a) => (Some(a), true),
    };
    let (function, key, upload) = match attachment {
      Some(attachment) => {
        let size = attachment.data.len();
        if size > TELEGRAM_FILE_MAX_BYTES {
          tracing::warn!("Telegram attachment {} is larger than the 50 MB upload limit.", attachment.name);
          return false;
        }
        tracing::debug!("Posting Telegram attachment {}", attachment.name);
        let (mut function, mut key) = mime_endpoint(&attachment.mime_type);
        if document || (function == "sendPhoto" && size > TELEGRAM_PHOTO_MAX_BYTES) {
          // Documents take larger photos and any other file type.
          (function, key) = ("sendDocument", "document");
        }
        (function, key, Upload { name: attachment.name.clone(), data: &attachment.data })
      }
      None => {
        // The notification image, from the image path or the bundled icon.
        let path = ctx.image_path(TELEGRAM_IMAGE_XY);
        let bytes = match &path {
          Some(p) => match std::fs::read(p) {
            Ok(bytes) => Some(std::borrow::Cow::Owned(bytes)),
            Err(_) => {
              tracing::error!("File can not be opened for read: {}", p.display());
              return false;
            }
          },
          None => ctx.image_raw(TELEGRAM_IMAGE_XY),
        };
        let Some(bytes) = bytes else {
          // Nothing to send.
          return true;
        };
        let name = match &path {
          Some(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
          None => format!("apprise-{}-{}{}", ctx.notify_type.as_str(), TELEGRAM_IMAGE_XY.as_str(), ctx.asset.default_extension),
        };
        image = bytes;
        ("SendPhoto", "photo", Upload { name, data: &image })
      }
    };

    // Telegram throttles before the upload so the content arrives together.
    throttle().await;

    let mut form = reqwest::multipart::Form::new();
    if let Some(c) = caption {
      form = form.text("caption", c.caption.clone()).text("show_caption_above_media", py_bool(c.show_caption_above_media)).text("parse_mode", c.parse_mode);
    }
    if let Some(title) = title {
      form = form.text("title", title.to_string());
    }
    form = form.text("chat_id", target.chat_id.to_string());
    if let Some(topic) = target.topic {
      form = form.text("message_thread_id", topic.to_string());
    }
    form = form.part(key, reqwest::multipart::Part::bytes(upload.data.to_vec()).file_name(upload.name));

    tracing::debug!("Telegram attachment POST URL: {} (cert_verify={})", function, self.verify_certificate);
    match client.post(self.api_url(function)).header("User-Agent", APP_ID).multipart(form).send().await {
      Ok(resp) if resp.status().as_u16() == 200 => true,
      Ok(resp) => {
        tracing::warn!("Failed to send Telegram attachment: {}.", status_text(resp.status()));
        tracing::debug!("Response Details:\r\n{}", truncated(&resp.text().await.unwrap_or_default()));
        false
      }
      Err(e) => {
        tracing::warn!("A connection error occurred posting Telegram attachment.");
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        false
      }
    }
  }

  /// Send every attachment to one chat (upstream `_send_attachments`).
  async fn send_attachments(&self, client: &reqwest::Client, ctx: &NotifyContext, target: &Target, caption: Option<&Caption>) -> bool {
    if self.album {
      // Group eligible media instead of one message per attachment.
      return self.send_album_attachments(client, ctx, target, caption).await;
    }
    for (index, attachment) in ctx.attachments.iter().enumerate() {
      let no = index + 1;
      let key = format!("attachment:{}:{}", target.key(), no);
      if is_delivered(&key, false) {
        continue;
      }
      let title = if attachment.name.is_empty() { format!("file{no:03}.dat") } else { attachment.name.clone() };
      let caption = if no == 1 { caption } else { None };
      if !self.send_media(client, ctx, target, caption, Some(&title), Media::Attachment(attachment)).await {
        return false;
      }
      tracing::info!("Sent Telegram attachment: {}.", attachment.name);
      mark_delivered(&key, false);
    }
    true
  }

  /// Send attachments with photos and videos grouped into albums (upstream
  /// `_send_album_attachments`).
  async fn send_album_attachments(&self, client: &reqwest::Client, ctx: &NotifyContext, target: &Target, caption: Option<&Caption>) -> bool {
    let attachments = &ctx.attachments;
    let kinds: Vec<MediaKind> = attachments.iter().map(album_kind).collect();
    // Track each item so retries skip attachments already delivered.
    let keys: Vec<String> = (1..=attachments.len()).map(|no| format!("attachment:{}:{}", target.key(), no)).collect();
    let grouped = |kind: MediaKind| kind != MediaKind::Single;

    // Only the first thing sent carries the caption.
    let mut caption = caption.cloned();
    let mut no = 0;
    while no < attachments.len() {
      if is_delivered(&keys[no], false) {
        // It arrived on an earlier attempt, with the caption if it was first.
        caption = None;
        no += 1;
        continue;
      }

      // Group adjacent media without changing their order, within 50 MB.
      let mut end = no;
      let mut total = 0;
      while end < attachments.len()
        && grouped(kinds[end])
        && end - no < TELEGRAM_MEDIA_GROUP_MAX
        && total + attachments[end].data.len() <= TELEGRAM_FILE_MAX_BYTES
        && !is_delivered(&keys[end], false)
      {
        total += attachments[end].data.len();
        end += 1;
      }

      // Too few media for an album are sent one at a time.
      let result = if end - no >= TELEGRAM_MEDIA_GROUP_MIN {
        self.send_media_group(client, target, &attachments[no..end], &kinds[no..end], caption.as_ref()).await
      } else {
        GroupResult::Retry
      };

      match result {
        GroupResult::Fail => return false,
        GroupResult::Retry => {
          // Send rejected or ungrouped items separately.
          end = end.max(no + 1);
          for idx in no..end {
            let item_caption = if idx == no { caption.as_ref() } else { None };
            if !self.send_album_item(client, ctx, target, &attachments[idx], kinds[idx], item_caption).await {
              return false;
            }
            tracing::info!("Sent Telegram attachment: {}.", attachments[idx].name);
            mark_delivered(&keys[idx], false);
          }
        }
        GroupResult::Ok => {
          for idx in no..end {
            tracing::info!("Sent Telegram attachment: {}.", attachments[idx].name);
            mark_delivered(&keys[idx], false);
          }
        }
      }

      // The caption has been spent.
      caption = None;
      no = end;
    }
    true
  }

  /// Send one attachment on its own in album mode, falling back to a plain
  /// document for a refused photo or video (upstream `_send_album_item`).
  async fn send_album_item(
    &self,
    client: &reqwest::Client,
    ctx: &NotifyContext,
    target: &Target,
    attachment: &Attachment,
    kind: MediaKind,
    caption: Option<&Caption>,
  ) -> bool {
    if self.send_media(client, ctx, target, caption, None, Media::Attachment(attachment)).await {
      return true;
    }
    if kind == MediaKind::Single {
      return false;
    }
    tracing::info!("Sending Telegram attachment {} as a document.", attachment.name);
    self.send_media(client, ctx, target, caption, None, Media::Document(attachment)).await
  }

  /// Upload a batch of photos and videos as one album (upstream `_send_media_group`).
  async fn send_media_group(
    &self,
    client: &reqwest::Client,
    target: &Target,
    batch: &[Attachment],
    kinds: &[MediaKind],
    caption: Option<&Caption>,
  ) -> GroupResult {
    let mut form = reqwest::multipart::Form::new().text("chat_id", target.chat_id.to_string()).text("disable_notification", py_bool(self.silent));
    if let Some(topic) = target.topic {
      form = form.text("message_thread_id", topic.to_string());
    }

    // Link each album item to its uploaded file.
    let mut media = Vec::new();
    let mut parts = Vec::new();
    for (no, (attachment, kind)) in batch.iter().zip(kinds).enumerate() {
      let field = format!("file{no}");
      let mut item = Map::new();
      item.insert("type".into(), json!(kind.as_str()));
      item.insert("media".into(), json!(format!("attach://{field}")));
      // Telegram shows the caption of the first item only.
      if no == 0
        && let Some(c) = caption
      {
        item.insert("caption".into(), json!(c.caption));
        item.insert("show_caption_above_media".into(), json!(c.show_caption_above_media));
        item.insert("parse_mode".into(), json!(c.parse_mode));
      }
      media.push(Value::Object(item));

      let name = if attachment.name.is_empty() { format!("file{no:03}.dat") } else { attachment.name.clone() };
      let part = reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(name.clone());
      let part = part.mime_str(&attachment.mime_type).unwrap_or_else(|_| reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(name));
      parts.push((field, part));
    }
    form = form.text("media", Value::Array(media).to_string());
    for (field, part) in parts {
      form = form.part(field, part);
    }

    tracing::debug!("Telegram media group POST URL: sendMediaGroup (cert_verify={})", self.verify_certificate);
    throttle().await;

    let resp = match client.post(self.api_url("sendMediaGroup")).header("User-Agent", APP_ID).multipart(form).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A connection error occurred posting Telegram media group.");
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return GroupResult::Fail;
      }
    };
    let status = resp.status();
    if status.as_u16() == 200 {
      return GroupResult::Ok;
    }
    tracing::warn!("Failed to send Telegram media group: {}.", status_text(status));
    tracing::debug!("Response Details:\r\n{}", truncated(&resp.text().await.unwrap_or_default()));
    // A refused album may still work as separate messages.
    if status.as_u16() == 400 { GroupResult::Retry } else { GroupResult::Fail }
  }

  /// Build the Rich Message from the template (upstream `_gen_rich_payload`).
  async fn rich_payload(&self, ctx: &NotifyContext) -> Option<Value> {
    let source = self.template.as_ref()?;
    let loaded = source.load_limited(TELEGRAM_TEMPLATE_MAX_BYTES as u64).await.ok().filter(|a| a.data.len() <= TELEGRAM_TEMPLATE_MAX_BYTES);
    let Some(template) = loaded else {
      tracing::warn!("Could not access Telegram Rich Message template {}.", source.loggable());
      return None;
    };
    let Ok(text) = String::from_utf8(template.data) else {
      tracing::warn!("Telegram Rich Message template {} could not be read.", source.loggable());
      return None;
    };

    let color = ctx.asset.color(&ctx.notify_type);
    let mut tokens: HashMap<String, String> = self.tokens.iter().cloned().collect();
    let image_url = if self.include_image { ctx.image_url(TELEGRAM_IMAGE_XY).unwrap_or_default() } else { String::new() };
    for (key, value) in [
      ("app_body", ctx.body.clone()),
      ("app_title", ctx.title.clone()),
      ("app_type", ctx.notify_type.as_str().to_string()),
      ("app_id", ctx.asset.app_id.clone()),
      ("app_desc", ctx.asset.app_desc.clone()),
      ("app_color", color.clone()),
      ("app_color_hex", color),
      ("app_image_url", image_url),
      ("app_url", ctx.asset.app_url.clone()),
    ] {
      tokens.insert(key.to_string(), value);
    }
    // Templates are always Rich Message JSON, so values are JSON-escaped.
    let content = apply_template(&text, TemplateType::Json, &tokens);

    let content: Value = match serde_json::from_str(&content) {
      Ok(v) => v,
      Err(e) => {
        tracing::warn!("Telegram Rich Message template {} contains invalid JSON.", source.loggable());
        tracing::debug!("JSONDecodeError: {}", e);
        return None;
      }
    };
    if !content.is_object() {
      tracing::warn!("Telegram Rich Message template {} must be a JSON object (got {}).", source.loggable(), json_type_name(&content));
      return None;
    }
    let Some(blocks) = content.get("blocks").and_then(Value::as_array).filter(|b| !b.is_empty()) else {
      tracing::warn!("Telegram Rich Message template {} must contain a non-empty 'blocks' list.", source.loggable());
      return None;
    };
    if !blocks.iter().all(|b| b.get("type").is_some_and(Value::is_string)) {
      tracing::warn!("Telegram Rich Message template {} contains a block missing a 'type' string.", source.loggable());
      return None;
    }
    Some(content)
  }

  /// Send a Rich Message to every chat, then any attachments (upstream
  /// `_send_rich_message`).
  async fn send_rich_message(&self, client: &reqwest::Client, ctx: &NotifyContext, targets: &[Target]) -> bool {
    let Some(rich_message) = self.rich_payload(ctx).await else { return false };

    let mut has_error = false;
    for target in targets {
      let pchat_id = target.printable();
      let mut payload = Map::new();
      payload.insert("chat_id".into(), target.chat_id.to_json());
      payload.insert("rich_message".into(), rich_message.clone());
      if let Some(topic) = target.topic {
        payload.insert("message_thread_id".into(), json!(topic));
      }
      if !self.preview {
        payload.insert("link_preview_options".into(), json!({"is_disabled": true}));
      }

      throttle().await;
      tracing::debug!("Telegram POST URL: sendRichMessage (cert_verify={})", self.verify_certificate);
      match client.post(self.api_url("sendRichMessage")).header("User-Agent", APP_ID).json(&Value::Object(payload)).send().await {
        Ok(resp) if resp.status().as_u16() == 200 => {}
        Ok(resp) => {
          let status = resp.status();
          let content = resp.bytes().await.unwrap_or_default();
          let reason = error_description(&content).unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_string());
          tracing::warn!("Failed to send Telegram Rich Message to {}: {}, error={}.", pchat_id, reason, status.as_u16());
          has_error = true;
          continue;
        }
        Err(e) => {
          tracing::warn!("A connection error occurred sending a Telegram Rich Message to {}.", pchat_id);
          tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
          has_error = true;
          continue;
        }
      }
      tracing::info!("Sent Telegram Rich Message.");

      // Attachments follow exactly as for a normal notification.
      if !ctx.attachments.is_empty() && !self.send_attachments(client, ctx, target, None).await {
        has_error = true;
      }
    }
    !has_error
  }
}

/// A detected owner ID as upstream stores it (`str(id_)` of a truthy value).
fn owner_text(id: &Value) -> Option<String> {
  match id {
    Value::Number(n) if n.as_f64().is_some_and(|f| f != 0.0) => Some(n.to_string()),
    Value::String(s) if !s.is_empty() => Some(s.clone()),
    Value::Bool(true) => Some("True".into()),
    _ => None,
  }
}

/// Python truthiness of a JSON value.
fn truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(b) => *b,
    Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
    Value::String(s) => !s.is_empty(),
    Value::Array(a) => !a.is_empty(),
    Value::Object(o) => !o.is_empty(),
  }
}

/// Python's type name for a JSON value, for the template error.
fn json_type_name(value: &Value) -> &'static str {
  match value {
    Value::Null => "NoneType",
    Value::Bool(_) => "bool",
    Value::Number(n) if n.is_f64() => "float",
    Value::Number(_) => "int",
    Value::String(_) => "str",
    Value::Array(_) => "list",
    Value::Object(_) => "dict",
  }
}

/// Telegram's `description` from an error response (upstream
/// `loads(r.content).get("description", "unknown")`), or `None` when the
/// body is not a JSON object or the description is empty.
fn error_description(content: &[u8]) -> Option<String> {
  let value: Value = serde_json::from_slice(content).ok()?;
  let object = value.as_object()?;
  match object.get("description") {
    None => Some("unknown".into()),
    Some(Value::String(s)) => Some(s.clone()).filter(|s| !s.is_empty()),
    Some(other) => truthy(other).then(|| other.to_string()),
  }
}

/// `"Reason, error=CODE"` (or `"error=CODE"`) for a failure log line.
fn status_text(status: reqwest::StatusCode) -> String {
  match status.canonical_reason() {
    Some(reason) => format!("{}, error={}", reason, status.as_u16()),
    None => format!("error={}", status.as_u16()),
  }
}

/// The first 2000 characters of a response, for debug logs.
fn truncated(text: &str) -> String {
  text.chars().take(2000).collect()
}

/// Case-insensitive `str.startswith`.
fn starts_with_ci(value: &str, prefix: &str) -> bool {
  value.len() >= prefix.len() && value.is_char_boundary(prefix.len()) && value[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// The endpoint and form field for an attachment's MIME type (upstream
/// `mime_lookup`, checked top to bottom; the last entry catches the rest).
fn mime_endpoint(mime: &str) -> (&'static str, &'static str) {
  if starts_with_ci(mime, "image/gif") || starts_with_ci(mime, "video/H264") {
    // Animations only take GIF or H.264/MPEG-4.
    ("sendAnimation", "animation")
  } else if starts_with_ci(mime, "image/") {
    ("sendPhoto", "photo")
  } else if starts_with_ci(mime, "video/mp4") {
    ("sendVideo", "video")
  } else if starts_with_ci(mime, "application/ogg") || starts_with_ci(mime, "audio/ogg") {
    ("sendVoice", "voice")
  } else if starts_with_ci(mime, "audio/mpeg") || starts_with_ci(mime, "audio/mp4a-latm") {
    ("sendAudio", "audio")
  } else {
    ("sendDocument", "document")
  }
}

/// Whether an attachment can join an album (upstream `_album_kind`). GIFs
/// use the animation endpoint, which albums do not support.
fn album_kind(attachment: &Attachment) -> MediaKind {
  let mime = attachment.mime_type.to_lowercase();
  let size = attachment.data.len();
  if mime.starts_with("image/") && mime != "image/gif" && size <= TELEGRAM_PHOTO_MAX_BYTES {
    MediaKind::Photo
  } else if mime == "video/mp4" && size <= TELEGRAM_FILE_MAX_BYTES {
    MediaKind::Video
  } else {
    MediaKind::Single
  }
}

// ── HTML dialect ─────────────────────────────────────────────────────────────

/// Rewrite HTML so it only uses the markup Telegram accepts (upstream
/// `TelegramHTMLReducer`):
///
/// - b, i, u, s, spoilers, a href, code, pre, blockquote and tg-emoji are
///   kept; strong, em, ins, strike and del become b, i, u and s.
/// - Headings become bold text on their own line.
/// - List items become `- ` (or `1. `) lines and table rows become lines
///   with cells split by ` | `.
/// - `<br>` and block tags become new lines.
/// - Formatting is dropped inside code, and quotes are never nested,
///   because Telegram rejects both.
/// - Any other tag is dropped and only its text is kept.
/// - Every opened tag is closed, so the result is always balanced.
struct TelegramHtmlReducer {
  base: HtmlTagReducer,
  /// True once the current line holds text (anything but tags).
  has_text: bool,
  /// How many pieces of the output were already checked for text.
  checked: usize,
}

impl TelegramHtmlReducer {
  fn new() -> Self {
    let base = HtmlTagReducer {
      inline_map: TELEGRAM_HTML_INLINE_MAP.to_vec(),
      headings: TELEGRAM_HTML_HEADINGS.to_vec(),
      line_break: TELEGRAM_HTML_BR,
      // Telegram does not know &nbsp; or &emsp;, so use plain spaces.
      text_map: vec![('\u{a0}', " "), ('\u{2003}', "   ")],
      ..HtmlTagReducer::default()
    };
    Self { base, has_text: false, checked: 0 }
  }

  fn is_open(&self, tag: &str) -> bool {
    self.base.is_open(tag)
  }

  fn in_code(&self) -> bool {
    self.is_open("pre") || self.is_open("code")
  }

  /// End the current line, dropping spaces left at its end (`_break`).
  fn break_line(&mut self) {
    let in_code = self.in_code();
    if let Some(last) = self.base.out.last_mut()
      && !last.starts_with('<')
      && !in_code
    {
      *last = pystr::rstrip(last).to_string();
    }
    self.base.out.push(TELEGRAM_HTML_BR.to_string());
  }
}

/// The value of attribute `name`, the last one winning as in `dict(attrs)`.
fn attr_value<'a>(attrs: &'a Attrs, name: &str) -> Option<&'a str> {
  attrs.iter().rev().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
}

impl TagReducer for TelegramHtmlReducer {
  fn state(&mut self) -> &mut HtmlTagReducer {
    &mut self.base
  }

  fn on_reset(&mut self) {
    self.has_text = false;
    self.checked = 0;
  }

  fn line(&mut self) {
    // Check only newly written pieces.
    for piece in &self.base.out[self.checked.min(self.base.out.len())..] {
      if piece == TELEGRAM_HTML_BR {
        self.has_text = false;
      } else if !piece.starts_with('<') {
        self.has_text = true;
      }
    }
    self.checked = self.base.out.len();
    if self.has_text {
      self.break_line();
    }
  }

  fn start(&mut self, tag: &str, attrs: &Attrs) {
    if tag == "br" {
      // Line breaks are always kept.
      self.break_line();
      return;
    }
    if self.is_open("code") || (self.is_open("pre") && tag != "code") {
      // No formatting inside code, except the code tag naming the
      // language of a code block.
      return;
    }
    if tag == "span" && attr_value(attrs, "class").unwrap_or("").split(' ').any(|c| c == "tg-spoiler") {
      // The span form of a spoiler.
      self.base.open(tag, "tg-spoiler", "");
    } else if tag == "a"
      && let Some(href) = attr_value(attrs, "href").filter(|h| !h.is_empty())
      && !self.is_open("a")
    {
      // Links keep only their destination and are never nested.
      let attr = format!(" href=\"{}\"", pystr::escape_html_quote(href));
      self.base.open(tag, "a", &attr);
    } else if tag == "tg-emoji"
      && let Some(emoji) = attr_value(attrs, "emoji-id").filter(|e| !e.is_empty())
    {
      // Custom emoji keep their identifier.
      let attr = format!(" emoji-id=\"{}\"", pystr::escape_html_quote(emoji));
      self.base.open(tag, "tg-emoji", &attr);
    } else if tag == "pre" || tag == "code" {
      // Code keeps only a "language-" class, and only inside pre.
      let language = attr_value(attrs, "class").unwrap_or("");
      let attr = if tag == "code" && self.is_open("pre") && language.starts_with("language-") {
        format!(" class=\"{}\"", pystr::escape_html_quote(language))
      } else {
        String::new()
      };
      if tag == "pre" {
        self.line();
      }
      self.base.open(tag, tag, &attr);
    } else if tag == "blockquote" {
      // Quotes start a new line and cannot be nested.
      self.line();
      if !self.is_open("blockquote") {
        let attr = if attrs.iter().any(|(k, _)| k == "expandable") { " expandable" } else { "" };
        self.base.open(tag, "blockquote", attr);
      }
    } else if TELEGRAM_HTML_BLOCKS.contains(&tag) {
      // Other blocks (and dividers) begin on a new line too.
      self.line();
    } else {
      reducer_start(self, tag, attrs);
    }
  }

  fn end(&mut self, tag: &str) {
    if tag == "br" {
      // A closing </br> is a line break too.
      self.break_line();
      return;
    }
    reducer_end(self, tag);
    if TELEGRAM_HTML_BLOCKS.contains(&tag) || tag == "pre" || tag == "blockquote" {
      self.line();
    }
  }

  fn text(&mut self, data: &str) {
    if !self.in_code() {
      // Outside code, HTML source spacing rules apply.
      reducer_text(self, data);
    } else if !data.is_empty() {
      // Code keeps its spaces and line breaks as they are.
      let mapped: String = data
        .chars()
        .map(|c| {
          if c == '\u{a0}' {
            " ".to_string()
          } else if c == '\u{2003}' {
            "   ".to_string()
          } else {
            c.to_string()
          }
        })
        .collect();
      self.base.out.push(pystr::escape_html(&mapped));
    }
  }
}

/// Accept the common entities written without their semicolon
/// (upstream `TELEGRAM_HTML_LOOSE_ENTITY_RE`: `&(apos|quot|nbsp|emsp)(?!;)`).
fn fix_loose_entities(html: &str) -> String {
  const NAMES: [&str; 4] = ["apos", "quot", "nbsp", "emsp"];
  let mut out = String::with_capacity(html.len());
  let mut rest = html;
  while let Some(p) = rest.find('&') {
    out.push_str(&rest[..=p]);
    rest = &rest[p + 1..];
    if let Some(name) = NAMES.iter().find(|n| starts_with_ci(rest, n))
      && !rest[name.len()..].starts_with(';')
    {
      out.push_str(name);
      out.push(';');
      rest = &rest[name.len()..];
    }
  }
  out.push_str(rest);
  out
}

/// Rewrite HTML for Telegram (upstream `TelegramHTMLReducer().reduce()`).
fn reduce_html(html: &str) -> String {
  reduce_with(&mut TelegramHtmlReducer::new(), &fix_loose_entities(html))
}

// ── Markdown dialect ─────────────────────────────────────────────────────────

/// Translate CommonMark to Telegram Markdown v1, or MarkdownV2 when `strict`
/// (upstream `_commonmark_to_telegram`).
///
/// | CommonMark  | Markdown v1    | MarkdownV2            |
/// |-------------|----------------|-----------------------|
/// | `# heading` | `*heading*`    | `*heading*`           |
/// | `**bold**`  | `*bold*`       | `*bold*`              |
/// | `*italic*`  | `_italic_`     | `_italic_`            |
/// | `` `code` ``| `` `code` ``   | `` `code` ``          |
/// | `[l](<u>)`  | `[l](u)`       | `[l](u)`              |
/// | `\*`        | known escapes  | all reserved escapes  |
///
/// v1 keeps only the outermost of nested spans and escapes a stray marker
/// only outside a span, since Telegram reads text inside a span literally.
fn commonmark_to_telegram(body: &str, strict: bool) -> String {
  // Telegram Markdown writes headings as bold text.
  let body = commonmark_headings_to_bold(body);
  let chars: Vec<char> = body.chars().collect();
  let n = chars.len();

  // Output fragments; a "[" that opens no link is rewritten in place.
  let mut out: Vec<String> = Vec::new();
  // `*` and `_` runs, matched once the whole body is scanned.
  let mut delimiters: Vec<EmphasisDelimiter> = Vec::new();
  // Possible link-label openings, innermost last.
  let mut link_stack: Vec<usize> = Vec::new();

  let backtick_runs = commonmark_index_backtick_runs(&chars);
  // A temporary marker that does not occur in the message.
  let sentinel = commonmark_pick_emphasis_sentinel(&body);
  // A "[" that opens no link is literal text. v1 escapes it only outside a
  // span, which is known once the scan is done.
  let orphan = |out: &mut Vec<String>, idx: usize| {
    out[idx] = if strict { "\\[".to_string() } else { format!("{sentinel}[1{sentinel}") };
  };
  // Bound the total work spent scanning labeled-link destinations.
  let mut budget = commonmark_new_scan_budget(&chars);
  let starts = |i: usize, pat: &str| pat.chars().enumerate().all(|(k, c)| chars.get(i + k) == Some(&c));

  let mut i = 0;
  while i < n {
    let ch = chars[i];

    // Keep the escapes MarkdownV2 needs or legacy v1 recognizes.
    if ch == '\\' && i + 1 < n {
      let next = chars[i + 1];
      if strict || TELEGRAM_V1_ESCAPABLE.contains(next) {
        out.push(format!("\\{next}"));
      } else {
        // v1 does not escape this character; write it as is.
        out.push(next.to_string());
      }
      i += 2;
      continue;
    }

    // Matched code spans become Telegram backticks.
    if ch == '`' {
      let mut j = i;
      while j < n && chars[j] == '`' {
        j += 1;
      }
      let run = j - i;
      if let Some(close) = commonmark_find_backtick_run(&backtick_runs, j, run) {
        let content: String = chars[j..close].iter().collect();
        let content = content.replace('\\', "\\\\").replace('`', "\\`");
        // Fence multiline or wide spans.
        let delim = if run >= 3 || content.contains('\n') { "```" } else { "`" };
        out.push(format!("{delim}{content}{delim}"));
        i = close + run;
        continue;
      }
      if strict {
        out.push("\\`".repeat(run));
      } else {
        // v1 escapes them only outside a span.
        out.push(format!("{sentinel}`{run}{sentinel}"));
      }
      i = j;
      continue;
    }

    // A possible link-label opening.
    if ch == '[' {
      link_stack.push(out.len());
      out.push("[".into());
      i += 1;
      continue;
    }

    // Angle-bracket destinations, when a pending label can use one.
    if starts(i, "](<")
      && !link_stack.is_empty()
      && let Some(close) = commonmark_scan_angle_dest(&chars, i, n, Some(&mut budget))
    {
      let url: String = chars[i + 3..close].iter().collect();
      let url = url.replace('\\', "\\\\").replace(')', "\\)");
      link_stack.pop();
      out.push(format!("]({url})"));
      i = close + 2;
      continue;
    }

    // Complete links; invalid destinations stay literal.
    if starts(i, "](")
      && let Some(&opener) = link_stack.last()
    {
      if let Some(close) = commonmark_scan_paren_dest(&chars, i + 1, n, Some(&mut budget)) {
        link_stack.pop();
        // Telegram link destinations escape parentheses only.
        let mut dest = String::new();
        let mut k = i + 2;
        while k < close {
          if chars[k] == '\\' && k + 1 < close {
            dest.push(chars[k]);
            dest.push(chars[k + 1]);
            k += 2;
            continue;
          }
          if chars[k] == '(' || chars[k] == ')' {
            dest.push('\\');
          }
          dest.push(chars[k]);
          k += 1;
        }
        out.push(format!("]({dest})"));
        i = close + 1;
        continue;
      }
      link_stack.pop();
      orphan(&mut out, opener);
    }

    // Autolinks lose their brackets; MarkdownV2 escapes their markup.
    if ch == '<'
      && let (Some(close), _) = commonmark_scan_autolink_dest(&chars, i, n)
    {
      for &c in &chars[i + 1..close] {
        if strict && TELEGRAM_RESERVED_FULL.contains(c) {
          out.push(format!("\\{c}"));
        } else {
          out.push(c.to_string());
        }
      }
      i = close + 1;
      continue;
    }

    // A "]" that never forms "](" closes no link; retire the innermost
    // pending "[" so a later "](" cannot reuse it.
    if ch == ']'
      && let Some(opener) = link_stack.pop()
    {
      orphan(&mut out, opener);
    }

    // MarkdownV2 escapes non-link brackets and parentheses.
    if strict && matches!(ch, ']' | '(' | ')') {
      out.push(format!("\\{ch}"));
      i += 1;
      continue;
    }

    // `*` and `_` runs are resolved after the scan.
    if ch == '*' || ch == '_' {
      let mut placeholder = String::new();
      i = commonmark_emphasis_run(&chars, i, n, &mut delimiters, &mut placeholder, &sentinel);
      out.push(placeholder);
      continue;
    }

    // The remaining MarkdownV2 reserved characters.
    if strict && TELEGRAM_STRICT_CHARS.contains(ch) {
      out.push(format!("\\{ch}"));
      i += 1;
      continue;
    }

    out.push(ch.to_string());
    i += 1;
  }

  // Escape dangling "[" markers.
  for idx in std::mem::take(&mut link_stack) {
    orphan(&mut out, idx);
  }

  // Resolve every run by the CommonMark rules.
  commonmark_match_emphasis(&mut delimiters);

  // MarkdownV2 keeps nesting; v1 drops nested markers but keeps their text.
  // The depth in source order tells which spans are visible.
  struct Run {
    close: String,
    open: String,
    /// Span depth at the run's unmatched markers.
    depth: i64,
    /// Span depth just after the run.
    depth_after: i64,
    ch: char,
    numdelims: usize,
  }
  let marker = |strong: bool| if strong { '*' } else { '_' };
  let mut depth: i64 = 0;
  let mut runs = Vec::with_capacity(delimiters.len());
  for d in &delimiters {
    let mut close = String::new();
    for e in d.events.iter().filter(|e| !e.open) {
      if strict || depth == 1 {
        close.push(marker(e.strong));
      }
      depth -= 1;
    }
    let run_depth = depth;
    let mut open = String::new();
    for e in d.events.iter().filter(|e| e.open).rev() {
      if strict || depth == 0 {
        open.push(marker(e.strong));
      }
      depth += 1;
    }
    runs.push(Run { close, open, depth: run_depth, depth_after: depth, ch: d.ch, numdelims: d.numdelims });
  }

  // Replace the placeholders, tracking the span depth as we go.
  let text = out.concat();
  let mut result = String::with_capacity(text.len());
  let mut span_depth: i64 = 0;
  let mut rest = text.as_str();
  while let Some(p) = rest.find(&sentinel) {
    result.push_str(&rest[..p]);
    let after = &rest[p + sentinel.len()..];
    let (kind, after_kind) = match after.chars().next() {
      Some(c @ ('`' | '[')) => (Some(c), &after[1..]),
      _ => (None, after),
    };
    let digits = after_kind.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || !after_kind[digits..].starts_with(&sentinel) {
      // Not a placeholder; keep the character and move on.
      let first = sentinel.chars().next().map(char::len_utf8).unwrap_or(1);
      result.push_str(&rest[p..p + first]);
      rest = &rest[p + first..];
      continue;
    }
    let number: usize = after_kind[..digits].parse().unwrap_or(0);
    rest = &after_kind[digits + sentinel.len()..];
    match kind {
      Some(kind) => {
        // A literal v1 "`" or "[" opens an entity that never ends unless it
        // is escaped; inside a span it is read literally.
        let m = if span_depth == 0 { format!("\\{kind}") } else { kind.to_string() };
        result.push_str(&m.repeat(number));
      }
      None => {
        let Some(run) = runs.get(number) else { continue };
        span_depth = run.depth_after;
        // Telegram rejects an unmatched marker as an entity that never
        // ends. MarkdownV2 escapes it everywhere; v1 only outside a span.
        let m = if strict || run.depth == 0 { format!("\\{}", run.ch) } else { run.ch.to_string() };
        result.push_str(&run.close);
        result.push_str(&m.repeat(run.numdelims));
        result.push_str(&run.open);
      }
    }
  }
  result.push_str(rest);
  result
}

#[async_trait]
impl Notify for Telegram {
  fn schemas(&self) -> &[&str] {
    &["tgram"]
  }
  fn service_name(&self) -> &str {
    "Telegram"
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
  /// With `content=before`, split text goes out before the attachment, so
  /// the last piece carries it; otherwise the first does.
  fn attachment_send_index(&self, total: usize) -> usize {
    if self.content == Placement::Before { total.saturating_sub(1) } else { 0 }
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Html
  }
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Html, NotifyFormat::Markdown]
  }
  fn body_maxlen(&self) -> usize {
    4096
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  /// Telegram allows up to 100 requests per second.
  fn request_rate_per_sec(&self) -> f64 {
    0.001
  }
  /// HTML is reduced to the tags Telegram accepts; CommonMark becomes the
  /// configured Telegram Markdown.
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    match body_format {
      NotifyFormat::Html => reduce_html(body),
      NotifyFormat::Markdown => commonmark_to_telegram(body, self.markdown_ver == MarkdownVersion::Two),
      NotifyFormat::Text => body.to_string(),
    }
  }
  fn has_dialect(&self) -> bool {
    true
  }
  /// A Rich Message template takes the title and body as substitution
  /// tokens, not as text to merge, split or convert.
  fn bypass_overflow(&self) -> bool {
    self.template.is_some()
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["tgram".into(), self.bot_token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;

    self.resolve_owner(&client).await;
    let targets = self.current_targets();
    if targets.is_empty() {
      tracing::warn!("There were not Telegram chat_ids to notify.");
      return Ok(false);
    }

    if self.template.is_some() {
      // The template defines the content entirely.
      return Ok(self.send_rich_message(&client, ctx, &targets).await);
    }

    let markdown = ctx.body_format == NotifyFormat::Markdown;
    let parse_mode = if markdown { self.markdown_ver.parse_mode() } else { "HTML" };
    // Declared HTML was already reduced by dialect_convert(); only HTML with
    // no declared format still needs the unsupported tags removed.
    let text = if ctx.body_passthrough && !markdown { escape_undeclared_html(&ctx.body) } else { ctx.body.clone() };
    let text = text.as_str();
    let has_body = !text.is_empty();
    let has_attach = !ctx.attachments.is_empty();

    // A short body rides on the first attachment as its caption.
    let caption = (has_attach && has_body && text.chars().count() < TELEGRAM_CAPTION_MAXLEN).then(|| Caption {
      caption: text.to_string(),
      show_caption_above_media: self.content == Placement::Before,
      parse_mode,
    });
    // Without a body (or with a caption) the attachments go first.
    let attach_content = if !has_body || caption.is_some() { Placement::After } else { self.content };

    let mut has_error = false;
    for target in &targets {
      // Skip a chat that accepted this message on an earlier attempt.
      let target_key = target.key();
      if is_delivered(&target_key, false) {
        continue;
      }
      let pchat_id = target.printable();

      // The image is its own message and goes out once per notification,
      // however many pieces the body is split into.
      let image_key = format!("image:{target_key}");
      if self.include_image && !is_delivered(&image_key, true) {
        if self.send_media(&client, ctx, target, None, None, Media::Image).await {
          mark_delivered(&image_key, true);
        } else {
          tracing::warn!("Failed to send Telegram attachment to {}.", pchat_id);
        }
      }

      if has_attach && attach_content == Placement::After {
        if !self.send_attachments(&client, ctx, target, caption.as_ref()).await {
          has_error = true;
          continue;
        }
        if !has_body {
          mark_delivered(&target_key, false);
          continue;
        }
      }

      if caption.is_some() {
        // The body went out as the caption.
        mark_delivered(&target_key, false);
        continue;
      }

      let piece_key = format!("body:{target_key}:0");
      if !is_delivered(&piece_key, false) {
        let mut payload = Map::new();
        payload.insert("disable_notification".into(), json!(self.silent));
        payload.insert("disable_web_page_preview".into(), json!(!self.preview));
        payload.insert("parse_mode".into(), json!(parse_mode));
        payload.insert("text".into(), json!(text));
        payload.insert("chat_id".into(), target.chat_id.to_json());
        if let Some(topic) = target.topic {
          payload.insert("message_thread_id".into(), json!(topic));
        }

        // Throttle before every request so content arrives together.
        throttle().await;
        tracing::debug!("Telegram POST URL: sendMessage (cert_verify={})", self.verify_certificate);

        match client.post(self.api_url("sendMessage")).header("User-Agent", APP_ID).json(&Value::Object(payload)).send().await {
          Ok(resp) if resp.status().as_u16() == 200 => {
            tracing::info!("Sent Telegram notification.");
            mark_delivered(&piece_key, false);
          }
          Ok(resp) => {
            let status = resp.status();
            let content = resp.bytes().await.unwrap_or_default();
            let reason = error_description(&content).unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_string());
            tracing::warn!("Failed to send Telegram notification to {}: {}, error={}.", pchat_id, reason, status.as_u16());
            tracing::debug!("Response Details:\r\n{}", truncated(&String::from_utf8_lossy(&content)));
            has_error = true;
            continue;
          }
          Err(e) => {
            tracing::warn!("A connection error occurred sending Telegram:{} notification.", pchat_id);
            tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
            has_error = true;
            continue;
          }
        }
      }

      if has_attach && attach_content == Placement::Before && !self.send_attachments(&client, ctx, target, None).await {
        has_error = true;
        continue;
      }

      // This chat is done; a retry can skip it.
      mark_delivered(&target_key, false);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Apprise;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{method, path_regex};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  const TOKEN: &str = "123456789:abcdefg_hijklmnop";

  fn tg(url: &str) -> Telegram {
    Telegram::from_url(&ParsedUrl::parse(url).unwrap()).unwrap_or_else(|| panic!("should load: {url}"))
  }

  fn ctx(title: &str, body: &str, body_format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), body_format, ..NotifyContext::default() }
  }

  fn attachment(name: &str, mime: &str, size: usize) -> Attachment {
    Attachment { name: name.into(), data: vec![b'x'; size], mime_type: mime.into() }
  }

  fn owner_response() -> Value {
    json!({"ok": true, "result": [
      {"update_id": 645421319},
      {"update_id": 645421320, "message": {"message_id": 2, "chat": {"id": 532389719}}},
      {"update_id": 645421321, "message": {"message_id": 2, "from": {"id": 532389719, "is_bot": false, "first_name": "Chris"}}}
    ]})
  }

  /// Answer every Telegram call with `{"ok": true}` (and `getUpdates` with an owner).
  async fn mount_ok(server: &MockServer) {
    Mock::given(method("POST"))
      .and(path_regex(r"^/bot[^/]+/getUpdates$"))
      .respond_with(ResponseTemplate::new(200).set_body_json(owner_response()))
      .mount(server)
      .await;
    Mock::given(method("POST"))
      .and(path_regex(r"^/bot[^/]+/[A-Za-z]+$"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
      .mount(server)
      .await;
  }

  /// The Telegram API calls received, as `(method, request)` in order.
  async fn calls(server: &MockServer) -> Vec<(String, Request)> {
    server
      .received_requests()
      .await
      .unwrap_or_default()
      .into_iter()
      .filter(|r| r.method.as_str() == "POST" && r.url.path().starts_with("/bot"))
      .filter_map(|r| r.url.path().rsplit_once('/').map(|(_, m)| m.to_string()).map(|m| (m, r)))
      .collect()
  }

  fn methods(calls: &[(String, Request)]) -> Vec<&str> {
    calls.iter().map(|(m, _)| m.as_str()).collect()
  }

  fn body_json(req: &Request) -> Value {
    serde_json::from_slice(&req.body).unwrap_or(Value::Null)
  }

  /// One part of a multipart body.
  #[derive(Debug)]
  struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    data: Vec<u8>,
  }

  fn parts(req: &Request) -> Vec<Part> {
    let ct = req.headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let boundary = format!("--{}", ct.split("boundary=").nth(1).expect("multipart body"));
    let body = String::from_utf8_lossy(&req.body).to_string();
    let mut out = Vec::new();
    for chunk in body.split(&boundary).skip(1) {
      if chunk.starts_with("--") {
        break;
      }
      let chunk = chunk.strip_prefix("\r\n").unwrap_or(chunk);
      let (head, data) = chunk.split_once("\r\n\r\n").unwrap();
      let data = data.strip_suffix("\r\n").unwrap_or(data);
      let quoted = |key: &str| head.split(&format!("{key}=\"")).nth(1).and_then(|s| s.split('"').next()).map(str::to_string);
      let content_type = head.lines().find_map(|l| l.strip_prefix("Content-Type: ").or_else(|| l.strip_prefix("content-type: "))).map(str::to_string);
      out.push(Part { name: quoted("name").unwrap(), filename: quoted("filename"), content_type, data: data.as_bytes().to_vec() });
    }
    out
  }

  /// The text fields of a multipart body, in order.
  fn fields(req: &Request) -> Vec<(String, String)> {
    parts(req).into_iter().filter(|p| p.filename.is_none()).map(|p| (p.name, String::from_utf8(p.data).unwrap())).collect()
  }

  fn field_pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
  }

  async fn notify(apprise: &Apprise, server: &MockServer, c: &NotifyContext) -> bool {
    crate::notify::with_test_base(server.uri(), apprise.notify(c)).await.is_success()
  }

  fn apprise(url: &str) -> Apprise {
    let mut a = Apprise::new();
    assert!(a.add(url), "should load: {url}");
    a
  }

  // ── URLs ──────────────────────────────────────────────────────────────

  #[test]
  fn test_valid_urls() {
    for url in [
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/",
      "tgram://123456789:abcdefg_hijklmnop/id1/id2/",
      "tgram://123456789:abcdefg_hijklmnop/?to=id1,id2",
      "tgram://123456789:abcdefg_hijklmnop/%$/",
      "tgram://123456789:abcdefg_hijklmnop/id1/id2/23423/-30/",
      "tgram://bottest@123456789:abcdefg_hijklmnop/lead2gold/",
      "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?topic=12345",
      "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?thread=12345",
      "tgram://bottest@123456789:abcdefg_hijklmnop/id1:invalid/?thread=12345",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?image=Yes",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=invalid",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=markdown",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=markdown&mdv=v1",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=markdown&mdv=v2",
      "tgram://123456789:abcdefg_hijklmnop/l2g/?format=markdown&mdv=bad",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=html",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?format=text",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?silent=yes",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?silent=no",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?preview=yes",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?preview=no",
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?album=yes",
      "tgram://123456789%3Aabcdefg_hijklmnop/lead2gold/",
      "tgram://bot123456789:abcdefg_hijklmnop/lead2gold/",
      "tgram://123456789:ABCdefghijkl123456789opqyz/-123456789525",
      "tgram://123456789:abcdefg_hijklmnop/",
      // Upstream takes the first path entry as the token's second half.
      "tgram://123456789:/lead2gold/",
    ] {
      assert!(from_url(url).is_some(), "should parse: {url}");
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "tgram://",
      "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?topic=invalid",
      "tgram://bottest@123456789:abcdefg_hijklmnop/id1/?content=invalid",
      "tgram://alpha:abcdefg_hijklmnop/lead2gold/",
      "tgram://:@/",
      // Upstream needs the colon; a slash is not accepted in its place.
      "tgram://123456789/abcdefg_hijklmnop/lead2gold/",
      // The second half of the token holds invalid characters.
      "tgram://123456789:abc.def/lead2gold/",
      // A template that is not a usable attachment source.
      "tgram://123456789:abcdefg_hijklmnop/lead2gold/?template=ftp://x/y.json",
    ] {
      assert!(from_url(url).is_none(), "should not parse: {url}");
    }
  }

  #[test]
  fn test_targets() {
    // Targets are sorted and deduplicated; names gain an "@", IDs are numbers.
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/id1/id2/23423/-30/@lead2gold/id1");
    let ids: Vec<Value> = obj.targets.iter().map(|t| t.chat_id.to_json()).collect();
    assert_eq!(ids, vec![json!(-30), json!(23423), json!("@lead2gold"), json!("@id1"), json!("@id2")]);
    assert!(obj.targets.iter().all(|t| t.topic.is_none()));
    assert!(!obj.detect_owner);

    // ?to= adds more; a per-chat topic wins over ?topic=.
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/9876:1234?to=abc,%40def:5&topic=77");
    let got: Vec<(Value, Option<i64>)> = obj.targets.iter().map(|t| (t.chat_id.to_json(), t.topic)).collect();
    assert_eq!(got, vec![(json!(9876), Some(1234)), (json!("@def"), Some(5)), (json!("@abc"), Some(77))]);

    // An invalid chat is dropped and turns owner detection off.
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/%$/");
    assert!(obj.targets.is_empty());
    assert!(!obj.detect_owner);
    let obj = tg("tgram://bottest@123456789:abcdefg_hijklmnop/id1:invalid/?thread=12345");
    assert!(obj.targets.is_empty());

    // No chat at all detects the bot owner unless ?detect=no.
    assert!(tg("tgram://123456789:abcdefg_hijklmnop/").detect_owner);
    assert!(!tg("tgram://123456789:abcdefg_hijklmnop/?detect=no").detect_owner);
    assert!(tg("tgram://123456789:abcdefg_hijklmnop/12345?detect=yes").detect_owner);

    // Group IDs keep their sign.
    let obj = tg("tgram://123456789:ABCdefghijkl123456789opqyz/-123456789525");
    assert_eq!(obj.targets, vec![Target { chat_id: ChatId::Id(-123456789525), topic: None }]);
    assert_eq!(obj.bot_token, "123456789:ABCdefghijkl123456789opqyz");
  }

  #[test]
  fn test_bot_token_forms() {
    for url in [
      "tgram://123456789:abcdefg_hijklmnop/l2g",
      "tgram://bottest@123456789:abcdefg_hijklmnop/l2g",
      "tgram://bot123456789:abcdefg_hijklmnop/l2g",
      "tgram://123456789%3Aabcdefg_hijklmnop/l2g",
      "tgram://user:pass@123456789:abcdefg_hijklmnop/l2g",
    ] {
      let obj = tg(url);
      assert_eq!(obj.bot_token, TOKEN, "{url}");
      assert_eq!(obj.targets, vec![Target { chat_id: ChatId::Text("@l2g".into()), topic: None }], "{url}");
    }
  }

  #[test]
  fn test_options() {
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/l2g");
    assert_eq!(obj.markdown_ver, MarkdownVersion::One);
    assert_eq!(obj.content, Placement::Before);
    assert!(!obj.silent && !obj.preview && !obj.album && !obj.include_image);
    assert_eq!(obj.topic, None);

    let obj = tg("tgram://123456789:abcdefg_hijklmnop/l2g?silent=yes&preview=yes&album=yes&image=yes&content=AFTER&thread=12");
    assert!(obj.silent && obj.preview && obj.album && obj.include_image);
    assert_eq!(obj.content, Placement::After);
    assert_eq!(obj.topic, Some(12));
    assert_eq!(obj.targets[0].topic, Some(12));

    // ?topic= wins over ?thread=; upstream's int() accepts spaces and signs.
    assert_eq!(tg("tgram://123456789:abcdefg_hijklmnop/l2g?topic=5&thread=6").topic, Some(5));
    assert_eq!(tg("tgram://123456789:abcdefg_hijklmnop/l2g?topic=+5").topic, Some(5));
    assert_eq!(tg("tgram://123456789:abcdefg_hijklmnop/l2g?topic=1_000").topic, Some(1000));
    assert_eq!(tg("tgram://123456789:abcdefg_hijklmnop/l2g?topic=0").topic, None);

    for (mdv, expected) in [
      ("v1", MarkdownVersion::One),
      ("1", MarkdownVersion::One),
      ("v2", MarkdownVersion::Two),
      ("2", MarkdownVersion::Two),
      ("V2", MarkdownVersion::Two),
      ("default", MarkdownVersion::Two),
      ("bad", MarkdownVersion::One),
      ("", MarkdownVersion::One),
    ] {
      assert_eq!(tg(&format!("tgram://123456789:abcdefg_hijklmnop/l2g?mdv={mdv}")).markdown_ver, expected, "mdv={mdv}");
    }

    // Rich Message template tokens.
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/l2g?template=/tmp/x.json&:mykey=myval");
    assert!(obj.template.is_some());
    assert_eq!(obj.tokens, vec![("mykey".to_string(), "myval".to_string())]);
  }

  #[test]
  fn test_service_properties() {
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/l2g");
    assert_eq!(obj.notify_formats(), vec![NotifyFormat::Html, NotifyFormat::Markdown]);
    assert_eq!(obj.body_maxlen(), 4096);
    assert_eq!(obj.title_maxlen(), 0);
    assert_eq!(obj.request_rate_per_sec(), 0.001);
    assert!(obj.attachment_support() && obj.has_dialect() && !obj.bypass_overflow());
    assert_eq!(obj.attachment_send_index(3), 2);
    assert_eq!(tg("tgram://123456789:abcdefg_hijklmnop/l2g?content=after").attachment_send_index(3), 0);
    assert_eq!(obj.url_identifier(), Some(vec!["tgram".to_string(), TOKEN.to_string()]));
    assert!(tg("tgram://123456789:abcdefg_hijklmnop/l2g?template=/tmp/x.json").bypass_overflow());
  }

  #[test]
  fn test_mime_endpoints() {
    for (mime, expected) in [
      ("image/gif", ("sendAnimation", "animation")),
      ("video/H264", ("sendAnimation", "animation")),
      ("image/jpeg", ("sendPhoto", "photo")),
      ("IMAGE/PNG", ("sendPhoto", "photo")),
      ("video/mp4", ("sendVideo", "video")),
      ("audio/ogg", ("sendVoice", "voice")),
      ("application/ogg", ("sendVoice", "voice")),
      ("audio/mpeg", ("sendAudio", "audio")),
      ("audio/mp4a-latm", ("sendAudio", "audio")),
      // Other audio types are plain documents upstream.
      ("audio/wav", ("sendDocument", "document")),
      ("video/quicktime", ("sendDocument", "document")),
      ("application/pdf", ("sendDocument", "document")),
    ] {
      assert_eq!(mime_endpoint(mime), expected, "{mime}");
    }
  }

  #[test]
  fn test_album_kind() {
    assert_eq!(album_kind(&attachment("a.jpg", "image/jpeg", 10)), MediaKind::Photo);
    assert_eq!(album_kind(&attachment("a.png", "IMAGE/PNG", 10)), MediaKind::Photo);
    assert_eq!(album_kind(&attachment("a.gif", "image/gif", 10)), MediaKind::Single);
    assert_eq!(album_kind(&attachment("a.mp4", "video/mp4", 10)), MediaKind::Video);
    assert_eq!(album_kind(&attachment("a.zip", "application/zip", 10)), MediaKind::Single);
    assert_eq!(album_kind(&attachment("a.jpg", "image/jpeg", TELEGRAM_PHOTO_MAX_BYTES)), MediaKind::Photo);
    assert_eq!(album_kind(&attachment("a.jpg", "image/jpeg", TELEGRAM_PHOTO_MAX_BYTES + 1)), MediaKind::Single);
  }

  // ── Dialects ──────────────────────────────────────────────────────────

  #[test]
  fn test_html_reduced_to_supported_tags() {
    let reduce = reduce_html;
    // Supported tags are kept; their aliases are mapped.
    assert_eq!(
      reduce("<strong>a</strong><em>b</em><ins>c</ins><strike>d</strike><del>e</del><s>f</s><tg-spoiler>g</tg-spoiler>"),
      "<b>a</b><i>b</i><u>c</u><s>d</s><s>e</s><s>f</s><tg-spoiler>g</tg-spoiler>"
    );
    // Spoiler spans are kept; other spans only keep their text.
    assert_eq!(reduce("<span class=\"x tg-spoiler\">a</span> <span class=\"y\">b</span>"), "<tg-spoiler>a</tg-spoiler> b");
    // Links and custom emoji keep only the attribute Telegram needs.
    assert_eq!(reduce("<a href=\"http://e.com?a=1&amp;b=2\" rel=\"x\">l</a> <a>n</a>"), "<a href=\"http://e.com?a=1&amp;b=2\">l</a> n");
    assert_eq!(reduce("<tg-emoji emoji-id=\"5368\">x</tg-emoji><tg-emoji>y</tg-emoji>"), "<tg-emoji emoji-id=\"5368\">x</tg-emoji>y");
    // Links are never nested.
    assert_eq!(reduce("<a href=\"http://a\">x <a href=\"http://b\">y</a></a>"), "<a href=\"http://a\">x y</a>");
    // Code blocks keep their language, but no formatting inside them.
    assert_eq!(
      reduce("<pre><code class=\"language-py\">a <b>&lt;</b>\n b</code></pre><code class=\"language-py\">c</code>"),
      "<pre><code class=\"language-py\">a &lt;\n b</code></pre>\r\n<code>c</code>"
    );
    // A character reference that decodes to nothing adds nothing to code.
    assert_eq!(reduce("<pre>&#1;</pre>x"), "<pre></pre>x");
    // Quotes are never nested and can be expandable.
    assert_eq!(reduce("<blockquote expandable>a<blockquote>b</blockquote></blockquote>"), "<blockquote expandable>a\r\nb</blockquote>");
    // Hidden content, images, tables, dividers and unknown tags.
    assert_eq!(
      reduce(
        "<script>bad()</script><style>x</style><img alt=\"pic\" src=\"a\"><img src=\"b\"><hr/><table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table><font>c</font>"
      ),
      "pic\r\na | b\r\n1 | 2\r\nc"
    );
    // Nested and numbered lists.
    assert_eq!(reduce("<ul><li>a<ul><li>b</li></ul></li></ul><ol><li>one</li><li>two</li></ol>"), "- a\r\n  - b\r\n1. one\r\n2. two");
    assert_eq!(reduce("<li>orphan</li>"), "- orphan");
    // Unsupported spaces become plain ones.
    assert_eq!(reduce("a&nbsp;b&emsp;c&nbspd"), "a b   c d");
    // Stray, crossed and unclosed tags still give balanced output.
    assert_eq!(reduce("</b>stray <b>open <i>both"), "stray <b>open <i>both</i></b>");
    assert_eq!(reduce("<b>x <i>y</b> z</i>"), "<b>x <i>y</i></b> z");
    // Self-closed tags other than br, hr and img are ignored.
    assert_eq!(reduce("a<b/>c"), "ac");
    // A list closed without being opened only ends the line.
    assert_eq!(reduce("a</ul>b"), "a\r\nb");
    // A line break written between tags stays a line break.
    assert_eq!(reduce("<b>a</b>\n<b>b</b>"), "<b>a</b>\r\n<b>b</b>");
    assert_eq!(reduce("<s>a</s>\n<b>b</b>"), "<s>a</s>\r\n<b>b</b>");
    assert_eq!(reduce("<b>a</b>\n\n<b>b</b>"), "<b>a</b>\r\n\r\n<b>b</b>");
    // A space written between tags still collapses to a space.
    assert_eq!(reduce("<b>a</b> <b>b</b>"), "<b>a</b> <b>b</b>");
    // A line break between table cells only lays out the source.
    assert_eq!(reduce("<table><tr><td><b>a</b></td>\n<td>b</td></tr></table>"), "<b>a</b>  | b");
    // Blank input stays blank.
    assert_eq!(reduce("   \n  "), "");
    // Consecutive breaks collapse, as upstream's line tracking does.
    assert_eq!(reduce("a<br><br>b"), "a\r\nb");
    assert_eq!(reduce("<pre>x  \n</pre>y"), "<pre>x  \n</pre>\r\ny");
    // Loose entities, in any case.
    assert_eq!(reduce("&QUOTx &apos;y&aposz"), "\"x 'y'z");
  }

  #[test]
  fn test_html_reducer_many_open_tags_is_fast() {
    let start = std::time::Instant::now();
    reduce_html(&"<b><i>".repeat(5000));
    reduce_html(&"<li>x".repeat(5000));
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
  }

  #[test]
  fn test_dialect_leaves_text_alone() {
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/1");
    assert_eq!(obj.dialect_convert("<b>&</b>", &NotifyFormat::Text), "<b>&</b>");
  }

  #[test]
  fn test_v1_unmatched_markers_escaped() {
    let f1 = |s: &str| commonmark_to_telegram(s, false);
    // CommonMark literals outside a span are escaped.
    assert_eq!(f1("Backup of app_data failed"), r"Backup of app\_data failed");
    assert_eq!(f1("snake_case_name"), r"snake\_case\_name");
    assert_eq!(f1("5 * 3 = 15"), r"5 \* 3 = 15");
    assert_eq!(f1("a lone ` tick"), r"a lone \` tick");
    assert_eq!(f1("x ``` y"), r"x \`\`\` y");
    // Text inside a visible span is left as is.
    assert_eq!(f1("**disk a_b** on c_d"), r"*disk a_b* on c\_d");
    assert_eq!(f1("_it a*b_ x*y"), r"_it a*b_ x\*y");
    assert_eq!(f1("**a ` b** c"), "*a ` b* c");
    assert_eq!(f1("**[a** b"), "*[a* b");
    // A "[" that opens no link is escaped too.
    assert_eq!(f1("[ERROR] disk full"), r"\[ERROR] disk full");
    assert_eq!(f1("a [b"), r"a \[b");
    // Real markup and code spans are unchanged.
    assert_eq!(f1("**bold** and _it_"), "*bold* and _it_");
    assert_eq!(f1("`code_x` y_z"), r"`code_x` y\_z");
    // MarkdownV2 output.
    assert_eq!(commonmark_to_telegram("app_data", true), r"app\_data");
    assert_eq!(commonmark_to_telegram("a ` b", true), r"a \` b");
  }

  #[test]
  fn test_standalone_autolink_dialect() {
    assert_eq!(commonmark_to_telegram("see <https://a*b> now", false), "see https://a*b now");
    assert_eq!(commonmark_to_telegram("see <https://a*b-c> now", true), "see https://a\\*b\\-c now");
    assert_eq!(commonmark_to_telegram("see <https://x.example/*a*_b_[c](d)`e`> now", true), "see https://x\\.example/\\*a\\*\\_b\\_\\[c\\]\\(d\\)\\`e\\` now");
  }

  #[test]
  fn test_markdown_dialect_cases() {
    let v2 = |s: &str| commonmark_to_telegram(s, true);
    let v1 = |s: &str| commonmark_to_telegram(s, false);
    assert_eq!(v2("[x](<https://example.com/a(b)c>)"), "[x](https://example.com/a(b\\)c)");
    assert_eq!(v2("`a\\b`"), "`a\\\\b`");
    assert_eq!(v2("*__x__*"), "_*x*_");
    assert_eq!(v1("*__x__*"), "_x_");
    assert_eq!(v1("**a *b* c**"), "*a b c*");
    assert_eq!(v2("**A****B**"), "*A\\*\\*\\*\\*B*");
    assert_eq!(v2(r"3:00 p.m. \(sharp\)! Don't be late - see you there."), r"3:00 p\.m\. \(sharp\)\! Don't be late \- see you there\.");
    assert_eq!(v2("# Title\nbody"), "*Title*\nbody");
    // Angle destinations lose their brackets.
    assert_eq!(v1("[l](<http://x.io/a b>)"), "[l](http://x.io/a b)");
    // A sentinel already in the text does not confuse placeholders.
    assert_eq!(v1("a\u{e000}0\u{e000} *b*"), "a\u{e000}0\u{e000} _b_");
  }

  /// The upstream vectors (see `testdata/gen_telegram.py`).
  fn vectors() -> Value {
    serde_json::from_str(include_str!("testdata/telegram.json")).unwrap()
  }

  #[test]
  fn test_dialect_vectors() {
    let v = vectors();
    let cases = v["dialect"].as_array().unwrap();
    let v1 = tg("tgram://123456789:abcdefg_hijklmnop/1?mdv=v1");
    let v2 = tg("tgram://123456789:abcdefg_hijklmnop/1?mdv=v2");
    let mut failures = Vec::new();
    for case in cases {
      let obj = if case["mdv"] == "v2" { &v2 } else { &v1 };
      let format: NotifyFormat = case["format"].as_str().unwrap().parse().unwrap();
      let body = case["body"].as_str().unwrap();
      let got = obj.dialect_convert(body, &format);
      if got != case["out"].as_str().unwrap() {
        failures.push(format!("{} {}: {:?}\n  want {:?}\n  got  {:?}", case["mdv"], case["format"], body, case["out"], got));
      }
    }
    assert!(
      failures.is_empty(),
      "{} of {} dialect vectors differ:\n{}",
      failures.len(),
      cases.len(),
      failures.iter().take(15).cloned().collect::<Vec<_>>().join("\n")
    );
  }

  #[tokio::test]
  async fn test_send_vectors() {
    let v = vectors();
    let long_bodies = v["long_bodies"].as_array().unwrap();
    let cases = v["send"].as_array().unwrap();
    let server = MockServer::start().await;
    let mut failures = Vec::new();
    for case in cases {
      server.reset().await;
      mount_ok(&server).await;
      let url = case["url"].as_str().unwrap();
      let body = match &case["body"] {
        Value::String(s) => s.clone(),
        other => long_bodies[other["long"].as_u64().unwrap() as usize].as_str().unwrap().to_string(),
      };
      let format: NotifyFormat = case["format"].as_str().unwrap().parse().unwrap();
      let a = apprise(url);
      let ok = notify(&a, &server, &ctx(case["title"].as_str().unwrap(), &body, format.clone())).await;
      let got: Vec<Value> = calls(&server).await.iter().map(|(m, r)| json!({"method": m, "json": body_json(r)})).collect();
      let want: Vec<Value> = case["requests"].as_array().unwrap().iter().map(|r| json!({"method": r["method"], "json": r["json"]})).collect();
      if got != want || ok != case["result"].as_bool().unwrap() {
        let show = |v: &[Value]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join("\n    ");
        failures.push(format!(
          "{url} {:?} {:?} {:?} ({:?}):\n  want {}\n    {}\n  got {}\n    {}",
          case["title"],
          body,
          format,
          case["result"],
          want.len(),
          show(&want),
          got.len(),
          show(&got)
        ));
      }
    }
    assert!(
      failures.is_empty(),
      "{} of {} send vectors differ:\n{}",
      failures.len(),
      cases.len(),
      failures.iter().take(8).cloned().collect::<Vec<_>>().join("\n")
    );
  }

  // ── Requests ──────────────────────────────────────────────────────────

  fn payload(chat_id: Value, text: &str, extra: Value) -> Value {
    let mut p = json!({"disable_notification": false, "disable_web_page_preview": true, "parse_mode": "HTML", "text": text, "chat_id": chat_id});
    for (k, v) in extra.as_object().unwrap() {
      p[k] = v.clone();
    }
    p
  }

  #[tokio::test]
  async fn test_send_message_payloads() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let a = apprise("tgram://123456789:abcdefg_hijklmnop/12345/@chan:7/?silent=yes&preview=yes");
    assert!(notify(&a, &server, &ctx("T", "B", NotifyFormat::Text)).await);

    let calls = calls(&server).await;
    assert_eq!(methods(&calls), ["sendMessage", "sendMessage"]);
    let extra = json!({"disable_notification": true, "disable_web_page_preview": false});
    assert_eq!(body_json(&calls[0].1), payload(json!(12345), "<b>T</b>\r\nB", extra.clone()));
    let mut chan = payload(json!("@chan"), "<b>T</b>\r\nB", extra);
    chan["message_thread_id"] = json!(7);
    assert_eq!(body_json(&calls[1].1), chan);
    let headers = &calls[0].1.headers;
    assert_eq!(headers.get("user-agent").unwrap(), APP_ID);
    assert_eq!(headers.get("content-type").unwrap(), "application/json");
  }

  /// HTML with no declared format loses the tags Telegram rejects, as
  /// upstream's `__telegram_escape_html_entries` does. Each case is what
  /// upstream sends for `Apprise().notify(body_format=None)`.
  #[tokio::test]
  async fn test_undeclared_html_loses_unsupported_tags() {
    let server = MockServer::start().await;
    let base = "tgram://123456789:abcdefg_hijklmnop/12345/";
    for (query, title, body, mode, text) in [
      (
        "",
        "T <b>",
        "<div><p>Hello <strong>world</strong> &amp; <span>more</span></p><h1>Head</h1>a&nbsp;b &lt;x&gt; &quot;q&quot; <ul><li>one</li></ul></div>",
        "HTML",
        "<b>T <b></b>\r\nHello <b>world</b> &amp;more\r\n<b>Head</b>a b &lt;x&gt; \"q\"\r\n-one\r\n",
      ),
      // The comment pattern runs from "<!DOCTYPE" to the first "-->"
      ("", "", "<!DOCTYPE html><html><head><title>Hi</title></head><body><!-- note --> <em>x</em>&emsp;y&APOS;z</body></html>", "HTML", "<i>x</i>   y'z"),
      // Python's \s and re.IGNORECASE reach beyond ASCII
      ("", "", "a\u{2003}<p>b</p>\u{2003}c <\u{17F}pan class=x>d</\u{17F}pan> <LINK rel=\u{212A}> <\u{130}nput>", "HTML", "ab\r\ncd"),
      ("", "", "1 < 2 & 3 **bold** _u_\n\n\nend<br/>x<hr>y", "HTML", "1 < 2 & 3 **bold** _u_\r\nend\r\nx\r\ny"),
      ("", "", "<b>keep</b> <i>it</i> <code>c</code> <caption>t</caption>", "HTML", "<b>keep</b> <i>it</i> <code>c</code> <i>t</i>"),
      // Markdown is sent as written
      ("?format=markdown", "Ti", "**md** <span>x</span> &nbsp;", "MARKDOWN", "Ti\r\n**md** <span>x</span> &nbsp;"),
    ] {
      server.reset().await;
      mount_ok(&server).await;
      let passthrough = NotifyContext { title: title.into(), body: body.into(), body_passthrough: true, ..NotifyContext::default() };
      assert!(notify(&apprise(&format!("{base}{query}")), &server, &passthrough).await, "{body}");
      let calls = calls(&server).await;
      assert_eq!(body_json(&calls[0].1), payload(json!(12345), text, json!({"parse_mode": mode})), "{body}");
    }
  }

  #[tokio::test]
  async fn test_markdown_parse_modes() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    for (url, mode, text) in [
      ("tgram://123456789:abcdefg_hijklmnop/12345?format=markdown", "MARKDOWN", "*Backup*\nBackup of app\\_data failed"),
      ("tgram://123456789:abcdefg_hijklmnop/12345?format=markdown&mdv=2", "MarkdownV2", "*Backup*\nBackup of app\\_data failed"),
      ("tgram://123456789:abcdefg_hijklmnop/12345", "MARKDOWN", "*Backup*\nBackup of app\\_data failed"),
    ] {
      server.reset().await;
      mount_ok(&server).await;
      assert!(notify(&apprise(url), &server, &ctx("Backup", "Backup of app_data failed", NotifyFormat::Markdown)).await);
      let calls = calls(&server).await;
      assert_eq!(body_json(&calls[0].1), payload(json!(12345), text, json!({"parse_mode": mode})), "{url}");
    }
  }

  /// An asset whose icons are files in `dir` (upstream's default layout).
  fn icon_asset(dir: &tempfile::TempDir) -> crate::asset::AppriseAsset {
    std::fs::write(dir.path().join("apprise-info-256x256.png"), b"PNG-ICON").unwrap();
    let mask = dir.path().join("apprise-{TYPE}-{XY}{EXTENSION}").to_string_lossy().into_owned();
    crate::asset::AppriseAsset { image_path_mask: Some(mask), ..Default::default() }
  }

  #[tokio::test]
  async fn test_image_upload() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/lead2gold:55/?image=yes");
    let mut c = ctx("", "<b>T</b>\r\nB", NotifyFormat::Html);
    c.asset = icon_asset(&dir);
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());

    let sent = calls(&server).await;
    assert_eq!(methods(&sent), ["SendPhoto", "sendMessage"]);
    assert_eq!(fields(&sent[0].1), field_pairs(&[("chat_id", "@lead2gold"), ("message_thread_id", "55")]));
    let file = parts(&sent[0].1).into_iter().find(|p| p.filename.is_some()).unwrap();
    assert_eq!((file.name.as_str(), file.filename.as_deref(), file.content_type.as_deref()), ("photo", Some("apprise-info-256x256.png"), None));
    assert_eq!(file.data, b"PNG-ICON");
    assert_eq!(body_json(&sent[1].1)["message_thread_id"], json!(55));

    // Without an icon there is nothing to upload.
    server.reset().await;
    mount_ok(&server).await;
    c.asset.image_path_mask = Some(dir.path().join("missing-{TYPE}.png").to_string_lossy().into_owned());
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
    assert_eq!(methods(&calls(&server).await), ["sendMessage"]);
  }

  #[cfg(feature = "images")]
  #[tokio::test]
  async fn test_bundled_image_upload() {
    // The icon bundled with the library, named as upstream's file is.
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/12345/?image=yes");
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&ctx("", "B", NotifyFormat::Html))).await.unwrap());
    let calls = calls(&server).await;
    assert_eq!(methods(&calls), ["SendPhoto", "sendMessage"]);
    let file = parts(&calls[0].1).into_iter().find(|p| p.filename.is_some()).unwrap();
    assert_eq!(file.filename.as_deref(), Some("apprise-info-256x256.png"));
    let icon = ctx("", "", NotifyFormat::Html).image_raw(TELEGRAM_IMAGE_XY).unwrap();
    assert!(calls[0].1.body.windows(icon.len()).any(|w| w == &icon[..]));
  }

  #[tokio::test]
  async fn test_image_failure_still_sends_message() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path_regex("/SendPhoto$")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/12345/?image=yes");
    let mut c = ctx("", "B", NotifyFormat::Html);
    c.asset = icon_asset(&dir);
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
    assert_eq!(methods(&calls(&server).await), ["SendPhoto", "sendMessage"]);
  }

  #[tokio::test]
  async fn test_image_per_split_piece() {
    // As upstream: the image is only tracked once per notification while
    // delivery tracking is on (retries enabled); otherwise every piece
    // repeats it.
    let dir = tempfile::tempdir().unwrap();
    for (url, images) in [
      ("tgram://123456789:abcdefg_hijklmnop/12345/?image=yes&overflow=split", 5),
      ("tgram://123456789:abcdefg_hijklmnop/12345/?image=yes&overflow=split&retry=1", 1),
    ] {
      let server = MockServer::start().await;
      mount_ok(&server).await;
      let mut a = Apprise::with_asset(icon_asset(&dir));
      assert!(a.add(url));
      assert!(notify(&a, &server, &ctx("", &"word ".repeat(2000), NotifyFormat::Text)).await);
      let calls = calls(&server).await;
      let methods = methods(&calls);
      assert_eq!(methods.iter().filter(|m| **m == "SendPhoto").count(), images, "{url}");
      assert_eq!(methods.iter().filter(|m| **m == "sendMessage").count(), 5, "{url}");
    }
  }

  #[tokio::test]
  async fn test_attachment_caption_placement() {
    for (content, above) in [("before", "True"), ("after", "False")] {
      let server = MockServer::start().await;
      mount_ok(&server).await;
      let obj = tg(&format!("tgram://123456789:abcdefg_hijklmnop/12345/?content={content}"));
      let mut c = ctx("", "<b>T</b>\r\nB", NotifyFormat::Html);
      c.attachments = vec![attachment("apprise-test.gif", "image/gif", 10), attachment("tg_a.txt", "text/plain", 5)];
      assert!(crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());

      let calls = calls(&server).await;
      assert_eq!(methods(&calls), ["sendAnimation", "sendDocument"], "{content}");
      assert_eq!(
        fields(&calls[0].1),
        field_pairs(&[
          ("caption", "<b>T</b>\r\nB"),
          ("show_caption_above_media", above),
          ("parse_mode", "HTML"),
          ("title", "apprise-test.gif"),
          ("chat_id", "12345")
        ])
      );
      let file = parts(&calls[0].1).into_iter().find(|p| p.filename.is_some()).unwrap();
      assert_eq!((file.name.as_str(), file.filename.as_deref(), file.content_type.as_deref()), ("animation", Some("apprise-test.gif"), None));
      assert_eq!(fields(&calls[1].1), field_pairs(&[("title", "tg_a.txt"), ("chat_id", "12345")]));
    }
  }

  #[tokio::test]
  async fn test_caption_topic_stays_with_its_chat() {
    // Upstream reuses one caption dict for every chat, so a later chat
    // inherits an earlier chat's topic; each chat keeps its own here.
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/12345:9/67890");
    let mut c = ctx("", "hi", NotifyFormat::Html);
    c.attachments = vec![attachment("tg_a.txt", "text/plain", 5)];
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
    let calls = calls(&server).await;
    let caption = [("caption", "hi"), ("show_caption_above_media", "True"), ("parse_mode", "HTML"), ("title", "tg_a.txt")];
    assert_eq!(fields(&calls[0].1), field_pairs(&[&caption[..], &[("chat_id", "12345"), ("message_thread_id", "9")]].concat()));
    assert_eq!(fields(&calls[1].1), field_pairs(&[&caption[..], &[("chat_id", "67890")]].concat()));
  }

  #[tokio::test]
  async fn test_long_body_attachment_placement() {
    for (content, expected) in [("before", vec!["sendMessage", "sendPhoto", "sendVideo"]), ("after", vec!["sendPhoto", "sendVideo", "sendMessage"])] {
      let server = MockServer::start().await;
      mount_ok(&server).await;
      let obj = tg(&format!("tgram://123456789:abcdefg_hijklmnop/12345/?content={content}"));
      let mut c = ctx("", &"x".repeat(1100), NotifyFormat::Html);
      c.attachments = vec![attachment("apprise-test.png", "image/png", 10), attachment("apprise-test.mp4", "video/mp4", 10)];
      assert!(crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
      let calls = calls(&server).await;
      assert_eq!(methods(&calls), expected, "{content}");
      let photo = calls.iter().find(|(m, _)| m == "sendPhoto").unwrap();
      assert_eq!(fields(&photo.1), field_pairs(&[("title", "apprise-test.png"), ("chat_id", "12345")]));
    }
  }

  #[tokio::test]
  async fn test_attachment_without_body() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/12345:3/");
    let mut c = ctx("", "", NotifyFormat::Html);
    c.attachments = vec![attachment("", "application/pdf", 3)];
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
    let calls = calls(&server).await;
    assert_eq!(methods(&calls), ["sendDocument"]);
    // An unnamed attachment gets a numbered name.
    assert_eq!(fields(&calls[0].1), field_pairs(&[("title", "file001.dat"), ("chat_id", "12345"), ("message_thread_id", "3")]));
  }

  #[tokio::test]
  async fn test_attachment_size_limits() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/12345/");

    // Oversized photos go out as documents.
    let mut c = ctx("", "", NotifyFormat::Html);
    c.attachments = vec![attachment("big.jpg", "image/jpeg", TELEGRAM_PHOTO_MAX_BYTES + 1)];
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
    assert_eq!(methods(&calls(&server).await), ["sendDocument"]);

    // Anything over 50 MB is refused before uploading.
    server.reset().await;
    mount_ok(&server).await;
    c.attachments = vec![attachment("huge.bin", "application/octet-stream", TELEGRAM_FILE_MAX_BYTES + 1)];
    assert!(!crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
    assert!(calls(&server).await.is_empty());
  }

  #[tokio::test]
  async fn test_attachment_failure_stops() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path_regex("/sendDocument$")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/12345/");
    let mut c = ctx("", "body", NotifyFormat::Html);
    c.attachments = vec![attachment("a.pdf", "application/pdf", 3), attachment("b.pdf", "application/pdf", 3)];
    assert!(!crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
    // The caption rode on the first upload, which failed; nothing else goes out.
    assert_eq!(methods(&calls(&server).await), ["sendDocument"]);
  }

  #[tokio::test]
  async fn test_dialect_attachment_order() {
    for (content, attach_first) in [("before", false), ("after", true)] {
      let server = MockServer::start().await;
      mount_ok(&server).await;
      let a = apprise(&format!("tgram://123456789:abcdefg_hijklmnop/12345?format=markdown&mdv=2&overflow=split&content={content}"));
      let mut c = ctx("", &".".repeat(3000), NotifyFormat::Markdown);
      c.attachments = vec![attachment("test.txt", "text/plain", 11)];
      assert!(notify(&a, &server, &c).await);
      let calls = calls(&server).await;
      let kinds: Vec<&str> = calls.iter().map(|(m, _)| if m == "sendMessage" { "text" } else { "attach" }).collect();
      assert!(kinds.iter().filter(|k| **k == "text").count() > 1);
      assert_eq!(kinds.iter().filter(|k| **k == "attach").count(), 1);
      assert_eq!(kinds[0] == "attach", attach_first, "{content}: {kinds:?}");
      assert_eq!(kinds[kinds.len() - 1] == "attach", !attach_first, "{content}: {kinds:?}");
    }
  }

  // ── Albums ────────────────────────────────────────────────────────────

  fn media(req: &Request) -> Value {
    serde_json::from_str(&fields(req).into_iter().find(|(k, _)| k == "media").unwrap().1).unwrap()
  }

  async fn send_album(server: &MockServer, url: &str, attachments: Vec<Attachment>) -> bool {
    let obj = tg(url);
    let mut c = ctx("", "hello", NotifyFormat::Html);
    c.attachments = attachments;
    crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap()
  }

  #[tokio::test]
  async fn test_album_groups_media() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let files =
      vec![attachment("apprise-test.jpeg", "image/jpeg", 4), attachment("apprise-test.png", "image/png", 4), attachment("apprise-test.mp4", "video/mp4", 4)];
    assert!(send_album(&server, "tgram://123456789:abcdefg_hijklmnop/12345:9?album=yes&silent=yes", files).await);

    let calls = calls(&server).await;
    assert_eq!(methods(&calls), ["sendMediaGroup"]);
    let req = &calls[0].1;
    let form: Vec<(String, String)> = fields(req).into_iter().filter(|(k, _)| k != "media").collect();
    assert_eq!(form, field_pairs(&[("chat_id", "12345"), ("disable_notification", "True"), ("message_thread_id", "9")]));
    assert_eq!(
      media(req),
      json!([
        {"type": "photo", "media": "attach://file0", "caption": "hello", "show_caption_above_media": true, "parse_mode": "HTML"},
        {"type": "photo", "media": "attach://file1"},
        {"type": "video", "media": "attach://file2"}
      ])
    );
    let files: Vec<(String, Option<String>, Option<String>)> =
      parts(req).into_iter().filter(|p| p.filename.is_some()).map(|p| (p.name, p.filename, p.content_type)).collect();
    assert_eq!(
      files,
      vec![
        ("file0".into(), Some("apprise-test.jpeg".into()), Some("image/jpeg".into())),
        ("file1".into(), Some("apprise-test.png".into()), Some("image/png".into())),
        ("file2".into(), Some("apprise-test.mp4".into()), Some("video/mp4".into())),
      ]
    );
  }

  #[tokio::test]
  async fn test_album_preserves_order() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let files = vec![
      attachment("apprise-test.jpeg", "image/jpeg", 4),
      attachment("apprise-archive.zip", "application/zip", 4),
      attachment("apprise-test.png", "image/png", 4),
      attachment("apprise-test.jpeg", "image/jpeg", 4),
    ];
    assert!(send_album(&server, "tgram://123456789:abcdefg_hijklmnop/12345?album=yes", files).await);
    let calls = calls(&server).await;
    // A lone photo can't form an album, the zip never could, and the
    // trailing pair does.
    assert_eq!(methods(&calls), ["sendPhoto", "sendDocument", "sendMediaGroup"]);
    assert_eq!(fields(&calls[0].1), field_pairs(&[("caption", "hello"), ("show_caption_above_media", "True"), ("parse_mode", "HTML"), ("chat_id", "12345")]));
    assert_eq!(fields(&calls[1].1), field_pairs(&[("chat_id", "12345")]));
    assert_eq!(media(&calls[2].1), json!([{"type": "photo", "media": "attach://file0"}, {"type": "photo", "media": "attach://file1"}]));
    assert_eq!(fields(&calls[2].1)[1], ("disable_notification".to_string(), "False".to_string()));
  }

  #[tokio::test]
  async fn test_album_batches_and_size_budget() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let photos: Vec<Attachment> = (0..12).map(|i| attachment(&format!("p{i}.jpg"), "image/jpeg", 4)).collect();
    assert!(send_album(&server, "tgram://123456789:abcdefg_hijklmnop/12345?album=yes", photos).await);
    let calls_ = calls(&server).await;
    assert_eq!(methods(&calls_), ["sendMediaGroup", "sendMediaGroup"]);
    assert_eq!(media(&calls_[0].1).as_array().unwrap().len(), 10);
    assert_eq!(media(&calls_[1].1).as_array().unwrap().len(), 2);

    // Albums stay within 50 MB of uploads.
    server.reset().await;
    mount_ok(&server).await;
    let videos: Vec<Attachment> = (0..3).map(|i| attachment(&format!("v{i}.mp4"), "video/mp4", 20_000_000)).collect();
    assert!(send_album(&server, "tgram://123456789:abcdefg_hijklmnop/12345?album=yes", videos).await);
    assert_eq!(methods(&calls(&server).await), ["sendMediaGroup", "sendVideo"]);
  }

  #[tokio::test]
  async fn test_album_retry_and_fallbacks() {
    // A refused album (400) goes out item by item.
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path_regex("/sendMediaGroup$")).respond_with(ResponseTemplate::new(400)).mount(&server).await;
    mount_ok(&server).await;
    let files = vec![attachment("a.jpeg", "image/jpeg", 4), attachment("b.png", "image/png", 4)];
    assert!(send_album(&server, "tgram://123456789:abcdefg_hijklmnop/12345?album=yes", files.clone()).await);
    let calls_ = calls(&server).await;
    assert_eq!(methods(&calls_), ["sendMediaGroup", "sendPhoto", "sendPhoto"]);
    assert_eq!(fields(&calls_[1].1)[0], ("caption".to_string(), "hello".to_string()));
    assert_eq!(fields(&calls_[2].1), field_pairs(&[("chat_id", "12345")]));

    // Any other album error fails without a fallback.
    server.reset().await;
    Mock::given(method("POST")).and(path_regex("/sendMediaGroup$")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    mount_ok(&server).await;
    assert!(!send_album(&server, "tgram://123456789:abcdefg_hijklmnop/12345?album=yes", files.clone()).await);
    assert_eq!(methods(&calls(&server).await), ["sendMediaGroup"]);

    // A refused photo is retried as a document, keeping its caption.
    server.reset().await;
    Mock::given(method("POST")).and(path_regex("/sendPhoto$")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    mount_ok(&server).await;
    assert!(send_album(&server, "tgram://123456789:abcdefg_hijklmnop/12345?album=yes", files[..1].to_vec()).await);
    let calls_ = calls(&server).await;
    assert_eq!(methods(&calls_), ["sendPhoto", "sendDocument"]);
    assert_eq!(fields(&calls_[1].1)[0], ("caption".to_string(), "hello".to_string()));

    // A failed document has nothing to fall back to.
    server.reset().await;
    Mock::given(method("POST")).and(path_regex("/sendDocument$")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    mount_ok(&server).await;
    assert!(!send_album(&server, "tgram://123456789:abcdefg_hijklmnop/12345?album=yes", vec![attachment("a.zip", "application/zip", 4)]).await);
    assert_eq!(methods(&calls(&server).await), ["sendDocument"]);

    // Without album mode every attachment is its own message.
    server.reset().await;
    mount_ok(&server).await;
    assert!(send_album(&server, "tgram://123456789:abcdefg_hijklmnop/12345", files).await);
    assert_eq!(methods(&calls(&server).await), ["sendPhoto", "sendPhoto"]);
  }

  #[tokio::test]
  async fn test_album_delivery_tracking() {
    // The album is refused, the first photo arrives on its own, and the
    // second photo fails along with its document fallback. The retry skips
    // the delivered photo (and its caption); the rest still form an album.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path_regex("/sendMediaGroup$"))
      .respond_with(ResponseTemplate::new(400))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path_regex("/sendPhoto$")).respond_with(ResponseTemplate::new(200)).up_to_n_times(1).with_priority(1).mount(&server).await;
    Mock::given(method("POST")).and(path_regex("/sendPhoto$")).respond_with(ResponseTemplate::new(500)).with_priority(2).mount(&server).await;
    Mock::given(method("POST")).and(path_regex("/sendDocument$")).respond_with(ResponseTemplate::new(500)).with_priority(2).mount(&server).await;
    Mock::given(method("POST")).and(path_regex("/sendMediaGroup$")).respond_with(ResponseTemplate::new(200)).with_priority(2).mount(&server).await;

    let a = apprise("tgram://123456789:abcdefg_hijklmnop/12345?album=yes&retry=1&wait=0");
    let mut c = ctx("", "hello", NotifyFormat::Text);
    c.attachments = vec![attachment("a.jpeg", "image/jpeg", 4), attachment("b.png", "image/png", 4), attachment("c.jpeg", "image/jpeg", 4)];
    assert!(notify(&a, &server, &c).await);

    let calls = calls(&server).await;
    assert_eq!(methods(&calls), ["sendMediaGroup", "sendPhoto", "sendPhoto", "sendDocument", "sendMediaGroup"]);
    assert_eq!(media(&calls[4].1), json!([{"type": "photo", "media": "attach://file0"}, {"type": "photo", "media": "attach://file1"}]));
  }

  // ── Delivery, owner detection and failures ────────────────────────────

  #[tokio::test]
  async fn test_retry_skips_delivered_chats() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path_regex("/sendMessage$"))
      .and(wiremock::matchers::body_partial_json(json!({"chat_id": 67890})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    mount_ok(&server).await;
    let a = apprise("tgram://123456789:abcdefg_hijklmnop/12345/67890?retry=1&wait=0");
    assert!(notify(&a, &server, &ctx("T", "B", NotifyFormat::Text)).await);
    let ids: Vec<Value> = calls(&server).await.iter().map(|(_, r)| body_json(r)["chat_id"].clone()).collect();
    assert_eq!(ids, vec![json!(12345), json!(67890), json!(67890)]);
  }

  #[tokio::test]
  async fn test_failures_continue_with_other_chats() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path_regex("/sendMessage$"))
      .and(wiremock::matchers::body_partial_json(json!({"chat_id": 12345})))
      .respond_with(ResponseTemplate::new(400).set_body_json(json!({"ok": false, "description": "Bad Request: chat not found"})))
      .with_priority(1)
      .mount(&server)
      .await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/12345/67890");
    assert!(!crate::notify::with_test_base(server.uri(), obj.send(&ctx("", "B", NotifyFormat::Html))).await.unwrap());
    assert_eq!(calls(&server).await.len(), 2);

    // Odd status codes and unreadable bodies fail too.
    for status in [500, 999, 201] {
      server.reset().await;
      Mock::given(method("POST")).and(path_regex("/sendMessage$")).respond_with(ResponseTemplate::new(status).set_body_string("}")).mount(&server).await;
      assert!(!crate::notify::with_test_base(server.uri(), obj.send(&ctx("", "B", NotifyFormat::Html))).await.unwrap(), "{status}");
    }

    // A connection failure is a failed send, not an error.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    assert!(!crate::notify::with_test_base(base, obj.send(&ctx("", "B", NotifyFormat::Html))).await.unwrap());
  }

  #[tokio::test]
  async fn test_owner_detection() {
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/?thread=1234");
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&ctx("", "B", NotifyFormat::Html))).await.unwrap());
    let calls_ = calls(&server).await;
    assert_eq!(methods(&calls_), ["getUpdates", "sendMessage"]);
    assert!(calls_[0].1.body.is_empty());
    assert_eq!(calls_[0].1.headers.get("content-type").unwrap(), "application/json");
    // The detected owner is sent as text, with the default topic.
    let sent = body_json(&calls_[1].1);
    assert_eq!((sent["chat_id"].clone(), sent["message_thread_id"].clone()), (json!("532389719"), json!(1234)));

    // The owner is kept, so the next notification skips the lookup.
    server.reset().await;
    mount_ok(&server).await;
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&ctx("", "B", NotifyFormat::Html))).await.unwrap());
    assert_eq!(methods(&calls(&server).await), ["sendMessage"]);
  }

  #[tokio::test]
  async fn test_owner_detection_failures() {
    for response in [
      ResponseTemplate::new(500).set_body_json(json!({"description": "Failure Message"})),
      ResponseTemplate::new(999),
      ResponseTemplate::new(200).set_body_string("}"),
      ResponseTemplate::new(200).set_body_json(json!({"ok": true, "result": []})),
      ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
      ResponseTemplate::new(200).set_body_json(json!({})),
      ResponseTemplate::new(200).set_body_json(json!(["not", "an", "object"])),
      ResponseTemplate::new(200).set_body_json(json!({"ok": false, "result": owner_response()["result"]})),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path_regex("/getUpdates$")).respond_with(response).mount(&server).await;
      let obj = tg("tgram://123456789:abcdefg_hijklmnop/");
      assert!(!crate::notify::with_test_base(server.uri(), obj.send(&ctx("", "B", NotifyFormat::Html))).await.unwrap());
      assert_eq!(methods(&calls(&server).await), ["getUpdates"]);
    }

    // No chats and no detection: nothing is sent.
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let obj = tg("tgram://123456789:abcdefg_hijklmnop/?detect=no");
    assert!(!crate::notify::with_test_base(server.uri(), obj.send(&ctx("", "B", NotifyFormat::Html))).await.unwrap());
    assert!(calls(&server).await.is_empty());
  }

  #[tokio::test]
  async fn test_owner_kept_in_store() {
    // Two identical URLs share a store: the second reads the stored owner.
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let mut a = Apprise::with_asset(crate::asset::AppriseAsset { async_mode: false, ..Default::default() });
    assert!(a.add("tgram://123456789:abcdefg_hijklmnop/"));
    assert!(a.add("tgram://123456789:abcdefg_hijklmnop/"));
    assert!(notify(&a, &server, &ctx("", "B", NotifyFormat::Text)).await);
    assert_eq!(methods(&calls(&server).await), ["getUpdates", "sendMessage", "sendMessage"]);
  }

  // ── Rich Messages ─────────────────────────────────────────────────────

  fn template_file(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().into_owned()
  }

  const BLOCKS: &str = r#"{"blocks": [{"type": "section_heading", "text": "{{app_title}}"}, {"type": "paragraph", "text": "{{app_body}} {{mykey}} {{app_type}} {{app_color}} {{app_image_url}} Café"}]}"#;

  #[tokio::test]
  async fn test_rich_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = template_file(&dir, "blocks.json", BLOCKS);
    let server = MockServer::start().await;
    mount_ok(&server).await;

    // Through Apprise: the body and title are converted, but never merged or split.
    let a = apprise(&format!("tgram://123456789:abcdefg_hijklmnop/lead2gold/1234:55?template={path}&:mykey=my%20val&overflow=split"));
    assert!(notify(&a, &server, &ctx("Ti & <b>tle</b>", "a & b\n**c**  ", NotifyFormat::Text)).await);
    let calls_ = calls(&server).await;
    assert_eq!(methods(&calls_), ["sendRichMessage", "sendRichMessage"]);
    let rich = json!({"blocks": [
      {"type": "section_heading", "text": "Ti&nbsp;&amp;&nbsp;&lt;b&gt;tle&lt;/b&gt;"},
      {"type": "paragraph", "text": "a&nbsp;&amp;&nbsp;b<br/>**c**&nbsp;&nbsp; my val info #3AA3E3  Café"}
    ]});
    assert_eq!(body_json(&calls_[0].1), json!({"chat_id": 1234, "rich_message": rich, "message_thread_id": 55, "link_preview_options": {"is_disabled": true}}));
    assert_eq!(body_json(&calls_[1].1), json!({"chat_id": "@lead2gold", "rich_message": rich, "link_preview_options": {"is_disabled": true}}));

    // Markdown input stays as written; the image URL and preview follow the URL.
    server.reset().await;
    mount_ok(&server).await;
    let a = apprise(&format!("tgram://123456789:abcdefg_hijklmnop/12345?template={path}&image=yes&preview=yes"));
    assert!(notify(&a, &server, &ctx("", "**x**", NotifyFormat::Markdown)).await);
    let sent = body_json(&calls(&server).await[0].1);
    assert!(sent.get("link_preview_options").is_none());
    assert_eq!(
      sent["rich_message"]["blocks"][1]["text"],
      json!("**x** {{mykey}} info #3AA3E3 https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-256x256.png Café")
    );
  }

  #[tokio::test]
  async fn test_rich_message_with_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let path = template_file(&dir, "blocks.json", BLOCKS);
    let server = MockServer::start().await;
    mount_ok(&server).await;
    let obj = tg(&format!("tgram://123456789:abcdefg_hijklmnop/12345?template={path}"));
    let mut c = ctx("t", "b", NotifyFormat::Html);
    c.attachments = vec![attachment("a.txt", "text/plain", 3)];
    assert!(crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
    let calls_ = calls(&server).await;
    assert_eq!(methods(&calls_), ["sendRichMessage", "sendDocument"]);
    assert_eq!(fields(&calls_[1].1), field_pairs(&[("title", "a.txt"), ("chat_id", "12345")]));

    // A failed attachment fails the notification.
    server.reset().await;
    Mock::given(method("POST")).and(path_regex("/sendDocument$")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    mount_ok(&server).await;
    assert!(!crate::notify::with_test_base(server.uri(), obj.send(&c)).await.unwrap());
  }

  #[tokio::test]
  async fn test_rich_message_failures() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    mount_ok(&server).await;
    for (name, content) in [
      ("bad.json", "{ not valid json }".to_string()),
      ("list.json", "[1, 2]".to_string()),
      ("noblocks.json", r#"{"blocks": {}}"#.to_string()),
      ("empty.json", r#"{"blocks": []}"#.to_string()),
      ("notype.json", r#"{"blocks": [{"text": "x"}]}"#.to_string()),
      ("notdict.json", r#"{"blocks": ["x"]}"#.to_string()),
      ("big.json", format!(r#"{{"blocks": [{{"type": "p", "text": "{}"}}]}}"#, "x".repeat(TELEGRAM_TEMPLATE_MAX_BYTES))),
    ] {
      let path = template_file(&dir, name, &content);
      let obj = tg(&format!("tgram://123456789:abcdefg_hijklmnop/12345?template={path}"));
      assert!(!crate::notify::with_test_base(server.uri(), obj.send(&ctx("t", "b", NotifyFormat::Html))).await.unwrap(), "{name}");
    }
    // A missing template file.
    let obj = tg(&format!("tgram://123456789:abcdefg_hijklmnop/12345?template={}", dir.path().join("missing.json").display()));
    assert!(!crate::notify::with_test_base(server.uri(), obj.send(&ctx("t", "b", NotifyFormat::Html))).await.unwrap());
    assert!(calls(&server).await.is_empty());

    // Telegram refusing the message.
    let path = template_file(&dir, "ok.json", BLOCKS);
    server.reset().await;
    Mock::given(method("POST"))
      .and(path_regex("/sendRichMessage$"))
      .respond_with(ResponseTemplate::new(400).set_body_json(json!({"description": "bad"})))
      .mount(&server)
      .await;
    let obj = tg(&format!("tgram://123456789:abcdefg_hijklmnop/12345/67890?template={path}"));
    assert!(!crate::notify::with_test_base(server.uri(), obj.send(&ctx("t", "b", NotifyFormat::Html))).await.unwrap());
    assert_eq!(methods(&calls(&server).await), ["sendRichMessage", "sendRichMessage"]);
  }
}
