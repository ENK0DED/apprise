//! Discord webhooks (upstream `NotifyDiscord`). Guilded subclasses it
//! upstream and reuses everything here through a [`Flavor`].
//!
//! A webhook URL `https://discord.com/api/webhooks/WEBHOOK_ID/WEBHOOK_TOKEN`
//! is written `discord://WEBHOOK_ID/WEBHOOK_TOKEN`.

use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::asset::NotifyImageSize;
use crate::conversion::{commonmark_sections, pystr};
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle_for};
use crate::types::NotifyFormat;
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::plugin_args::{parse_list, qsd_bool, split_path};
use crate::utils::templates::{TemplateFile, TemplateType, app_tokens, apply_template};

/// Largest template accepted, in bytes (upstream `max_discord_template_size`).
const MAX_TEMPLATE_SIZE: usize = 35000;

/// Upstream `discord_field_name_maxlen` and `discord_field_value_maxlen`.
const FIELD_NAME_MAXLEN: usize = 256;
const FIELD_VALUE_MAXLEN: usize = 1024;

/// Added to a rate-limit wait in case our clock is behind (upstream `clock_skew`).
const CLOCK_SKEW: Duration = Duration::from_secs(10);

/// What a webhook service built on Discord's API changes (upstream
/// subclasses override these class attributes).
pub(crate) struct Flavor {
  pub schemas: &'static [&'static str],
  pub service_name: &'static str,
  /// Fixed service base (tests redirect it) and the webhook path under it
  /// (upstream `notify_url`).
  pub base: &'static str,
  pub path: &'static str,
  pub details: fn() -> ServiceDetails,
}

const DISCORD: Flavor =
  Flavor { schemas: &["discord"], service_name: "Discord", base: "https://discord.com", path: "/api/webhooks", details: Discord::static_details };

/// Per-message limits (upstream `discord_max_fields`,
/// `discord_max_attachments`, `discord_max_attach_bytes`).
#[derive(Debug, Clone, Copy)]
struct Limits {
  max_fields: usize,
  max_attachments: usize,
  max_attach_bytes: usize,
}

const LIMITS: Limits = Limits { max_fields: 10, max_attachments: 10, max_attach_bytes: 25 * 1024 * 1024 };

/// What Discord last told us about its rate limit.
#[derive(Debug)]
struct RateLimit {
  remaining: f64,
  reset: SystemTime,
}

pub struct Discord {
  flavor: &'static Flavor,
  webhook_id: String,
  webhook_token: String,
  user: Option<String>,
  tts: bool,
  avatar: bool,
  footer: bool,
  footer_logo: bool,
  include_image: bool,
  fields: bool,
  avatar_url: Option<String>,
  href: Option<String>,
  thread_id: Option<String>,
  flags: Option<i64>,
  ping: Vec<String>,
  template: Option<TemplateFile>,
  tokens: IndexMap<String, String>,
  batch: bool,
  /// `?format=`, or the Markdown `?url=` and `?thread=` imply (upstream
  /// `_format_override`). Embeds are only used when this is Markdown.
  format_override: Option<NotifyFormat>,
  /// `?thread=` or `?url=` forced Markdown whatever `?format=` says.
  markdown_implied: bool,
  /// `?emojis=no`: escape `:name:` so Discord shows it as text.
  escape_emojis: bool,
  limits: Limits,
  clock_skew: Duration,
  ratelimit: Mutex<RateLimit>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Discord {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    Self::from_url_with(url, &DISCORD)
  }

  /// Upstream `parse_url()` and `__init__()`, for Discord or a subclass.
  pub(crate) fn from_url_with(url: &ParsedUrl, flavor: &'static Flavor) -> Option<Self> {
    let name = flavor.service_name;

    let Some(webhook_id) = url.host.as_deref().map(pystr::strip).filter(|s| !s.is_empty()).map(str::to_string) else {
      tracing::warn!("An invalid {} Webhook ID was specified.", name);
      return None;
    };
    let Some(webhook_token) = split_path(url).first().map(|t| pystr::strip(t).to_string()).filter(|t| !t.is_empty()) else {
      tracing::warn!("An invalid {} Webhook Token was specified.", name);
      return None;
    };

    // ?botname= is an alias of the user
    let user = url.get("botname").map(str::to_string).or_else(|| url.user.clone()).filter(|u| !u.is_empty());

    let flags = match url.get("flags").filter(|f| !f.is_empty()) {
      None => None,
      Some(f) => match f.trim().parse::<i64>() {
        Ok(v) if v >= 0 => Some(v),
        _ => {
          tracing::warn!("An invalid {} flags setting ({}) was specified.", name, f);
          return None;
        }
      },
    };

    // ?url= is an alias of ?href=; it and ?thread= imply Markdown
    let (href, url_alias) = match url.get("href") {
      Some(h) => (Some(h), false),
      None => (url.get("url"), url.get("url").is_some()),
    };
    let thread_id = url.get("thread").map(str::to_string);
    let markdown_implied = url_alias || thread_id.is_some();
    let format_override = if markdown_implied { Some(NotifyFormat::Markdown) } else { url.get("format").and_then(|f| f.parse().ok()) };

    let template = match url.get("template").filter(|t| !t.is_empty()) {
      Some(t) => match TemplateFile::parse(t, MAX_TEMPLATE_SIZE) {
        Some(file) => Some(file),
        None => {
          tracing::warn!("The {} template specified could not be loaded.", name);
          return None;
        }
      },
      None => None,
    };

    Some(Self {
      flavor,
      webhook_id,
      webhook_token,
      user,
      tts: qsd_bool(url, "tts", false),
      avatar: qsd_bool(url, "avatar", true),
      footer: qsd_bool(url, "footer", false),
      footer_logo: qsd_bool(url, "footer_logo", true),
      include_image: qsd_bool(url, "image", false),
      fields: qsd_bool(url, "fields", true),
      avatar_url: url.get("avatar_url").filter(|a| !a.is_empty()).map(str::to_string),
      href: href.filter(|h| !h.is_empty()).map(str::to_string),
      thread_id: thread_id.filter(|t| !t.is_empty()),
      flags,
      ping: url.get("ping").map(|p| parse_list([p], true, true)).unwrap_or_default(),
      template,
      tokens: url.qsd_colon.clone(),
      batch: qsd_bool(url, "batch", true),
      format_override,
      markdown_implied,
      escape_emojis: url.get("emojis").map(parse_bool) == Some(false),
      limits: LIMITS,
      clock_skew: CLOCK_SKEW,
      ratelimit: Mutex::new(RateLimit { remaining: 1.0, reset: SystemTime::now() }),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Discord",
      service_url: Some("https://discord.com/"),
      setup_url: Some("https://appriseit.com/services/discord/"),
      protocols: vec!["discord"],
      description: "Send notifications via Discord webhooks.",
      attachment_support: true,
    }
  }

  /// Build the webhook payload from `?template=` (upstream `gen_payload()`).
  /// `None` (already logged) when it cannot be used.
  async fn gen_payload(&self, template: &TemplateFile, ctx: &NotifyContext, title: &str, image_url: Option<&str>) -> Option<Map<String, Value>> {
    let name = self.flavor.service_name;
    let text = template.read(name).await?;

    let mut tokens = app_tokens(&self.tokens, &ctx.asset, &ctx.notify_type, title, &ctx.body, image_url);
    // Embeds want the colour as a decimal integer
    tokens.insert("app_color_int".into(), ctx.asset.color_int(&ctx.notify_type).to_string());

    let content: Value = match serde_json::from_str(&apply_template(&text, TemplateType::Json, &tokens)) {
      Ok(v) => v,
      Err(e) => {
        tracing::error!("{} template {} contains invalid JSON.", name, template.loggable());
        tracing::debug!("JSONDecodeError: {}", e);
        return None;
      }
    };
    let Value::Object(content) = content else {
      tracing::error!("{} template {} must be a JSON object (got {}).", name, template.loggable(), json_type_name(&content));
      return None;
    };

    // A webhook payload needs some content or at least one embed
    let has_content = content.get("content").and_then(Value::as_str).is_some_and(|c| !c.is_empty());
    let embeds = content.get("embeds").and_then(Value::as_array).filter(|e| !e.is_empty());
    if !has_content && embeds.is_none() {
      tracing::error!("{} template {} must contain a non-empty 'content' string or a non-empty 'embeds' list.", name, template.loggable());
      return None;
    }
    if embeds.is_some_and(|e| !e.iter().all(Value::is_object)) {
      tracing::error!("{} template {} contains an embed entry that is not a JSON object.", name, template.loggable());
      return None;
    }
    Some(content)
  }

  /// How long to wait for Discord's rate limit to reset, if it is used up.
  fn rate_limit_wait(&self) -> Option<Duration> {
    let rl = self.ratelimit.lock().unwrap_or_else(|e| e.into_inner());
    if rl.remaining > 0.0 {
      return None;
    }
    rl.reset.duration_since(SystemTime::now()).ok().filter(|d| !d.is_zero()).map(|d| d + self.clock_skew)
  }

  /// Remember the `X-RateLimit-*` headers of a response.
  fn update_rate_limit(&self, headers: &reqwest::header::HeaderMap) {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);
    let Some(remaining) = header("X-RateLimit-Remaining").and_then(|v| v.parse::<f64>().ok()) else { return };
    let mut rl = self.ratelimit.lock().unwrap_or_else(|e| e.into_inner());
    rl.remaining = remaining;
    // Upstream keeps the remaining count even when the reset is unusable
    if let Some(reset) = header("X-RateLimit-Reset").and_then(|v| v.parse::<i64>().ok()) {
      let offset = Duration::from_secs(reset.unsigned_abs());
      rl.reset = if reset >= 0 { UNIX_EPOCH + offset } else { UNIX_EPOCH.checked_sub(offset).unwrap_or(UNIX_EPOCH) };
    }
  }

  /// Post one message (upstream `_send()`), with attachments as multipart
  /// `files[N]` parts beside `payload_json`. A 429 is retried once.
  async fn post(&self, client: &reqwest::Client, payload: &Map<String, Value>, attach: &[&Attachment]) -> bool {
    let name = self.flavor.service_name;
    let url = format!("{}{}/{}/{}", service_base(self.flavor.base), self.flavor.path, self.webhook_id, self.webhook_token);
    let what = if attach.is_empty() { "notification".to_string() } else { format!("{} attachment(s)", attach.len()) };
    let body = Value::Object(payload.clone()).to_string();
    tracing::debug!("{} Payload: {}", name, body);

    let mut rate_limit = 1;
    loop {
      throttle_for(self.rate_limit_wait()).await;

      let mut req = client.post(&url).header("User-Agent", APP_ID);
      if let Some(thread) = &self.thread_id {
        req = req.query(&[("thread_id", thread)]);
      }
      req = if attach.is_empty() {
        req.header("Content-Type", "application/json; charset=utf-8").body(body.clone())
      } else {
        let mut form = reqwest::multipart::Form::new().text("payload_json", body.clone());
        for (idx, a) in attach.iter().enumerate() {
          tracing::debug!("Posting {} attachment {}", name, a.name);
          form = form.part(format!("files[{idx}]"), file_part(a));
        }
        req.multipart(form)
      };

      let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred posting to {}.", name);
          tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
          return false;
        }
      };
      self.update_rate_limit(resp.headers());

      let status = resp.status();
      if matches!(status.as_u16(), 200 | 204) {
        tracing::info!("Sent {} {}.", name, what);
        return true;
      }
      if status.as_u16() == 429 && rate_limit > 0 {
        let remaining = self.ratelimit.lock().unwrap_or_else(|e| e.into_inner()).remaining;
        tracing::warn!("{} rate limiting in effect; blocking for {:.2} second(s)", name, remaining);
        rate_limit -= 1;
        continue;
      }

      let reason = status.canonical_reason().unwrap_or("");
      tracing::warn!("Failed to send {} {}: {}{}error={}.", name, what, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      let details = resp.text().await.unwrap_or_default();
      tracing::debug!("Response Details:\r\n{}", details.chars().take(2000).collect::<String>());
      return false;
    }
  }
}

/// A multipart file part for an attachment.
fn file_part(a: &Attachment) -> reqwest::multipart::Part {
  let part = || reqwest::multipart::Part::bytes(a.data.clone()).file_name(a.name.clone());
  part().mime_str(&a.mime_type).unwrap_or_else(|_| part())
}

/// Python's name for the type of a JSON value, for log messages.
fn json_type_name(v: &Value) -> &'static str {
  match v {
    Value::Null => "NoneType",
    Value::Bool(_) => "bool",
    Value::Number(n) if n.is_f64() => "float",
    Value::Number(_) => "int",
    Value::String(_) => "str",
    Value::Array(_) => "list",
    Value::Object(_) => "dict",
  }
}

/// Upstream `ping_payload()`: the users (`<@123>`), roles (`<@&456>`) and
/// `@name` mentions (`@everyone`, `@here`) found in `args`, as
/// `allow_mentions` plus a `content` line that pings them. Empty when
/// there are none.
pub(crate) fn ping_payload(args: &[&str]) -> Map<String, Value> {
  let mut parse: Vec<String> = Vec::new();
  let mut users: Vec<String> = Vec::new();
  let mut roles: Vec<String> = Vec::new();
  let add = |set: &mut Vec<String>, value: String| {
    if !set.contains(&value) {
      set.push(value);
    }
  };

  for arg in args {
    for mention in find_mentions(arg) {
      match mention {
        Mention::Name(v) => add(&mut parse, v),
        Mention::Role(v) => add(&mut roles, v),
        Mention::User(v) => add(&mut users, v),
      }
    }
  }

  let mut payload = Map::new();
  if parse.is_empty() && users.is_empty() && roles.is_empty() {
    return payload;
  }
  let content: Vec<String> =
    parse.iter().map(|v| format!("@{v}")).chain(roles.iter().map(|v| format!("<@&{v}>"))).chain(users.iter().map(|v| format!("<@{v}>"))).collect();
  payload.insert("allow_mentions".into(), json!({ "parse": parse, "users": users, "roles": roles }));
  payload.insert("content".into(), Value::String(format!("👉 {}", content.join(" "))));
  payload
}

enum Mention {
  Name(String),
  Role(String),
  User(String),
}

/// Upstream `USER_ROLE_DETECTION_RE.findall()`:
/// `\s*(?:<?@(?P<role>&?)(?P<id>[0-9]+)>?|@(?P<value>[a-z0-9]+))`, any case.
fn find_mentions(text: &str) -> Vec<Mention> {
  let s: Vec<char> = text.chars().collect();
  let n = s.len();
  // `[a-z0-9]` without case also takes these four letters in Python
  let is_name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '\u{130}' | '\u{131}' | '\u{17f}' | '\u{212a}');
  let mut out = Vec::new();
  let mut i = 0;
  while i < n {
    // <@123>, <@&456>, @123
    let mut j = if s[i] == '<' { i + 1 } else { i };
    if j < n && s[j] == '@' {
      j += 1;
      let role = j < n && s[j] == '&';
      if role {
        j += 1;
      }
      let digits = s[j..].iter().take_while(|c| c.is_ascii_digit()).count();
      if digits > 0 {
        let id: String = s[j..j + digits].iter().collect();
        j += digits;
        if j < n && s[j] == '>' {
          j += 1;
        }
        out.push(if role { Mention::Role(id) } else { Mention::User(id) });
        i = j;
        continue;
      }
    }
    // @everyone, @here, @name
    if s[i] == '@' {
      let len = s[i + 1..].iter().take_while(|c| is_name(**c)).count();
      if len > 0 {
        out.push(Mention::Name(s[i + 1..i + 1 + len].iter().collect()));
        i += 1 + len;
        continue;
      }
    }
    i += 1;
  }
  out
}

/// Upstream `extract_markdown_sections()`: the text before the first
/// heading, and each heading's section as an embed field shown as a code
/// block (an empty section stays empty).
pub(crate) fn extract_markdown_sections(markdown: &str) -> (String, Vec<Value>) {
  // Leave room for the code block markers around each section
  let (description, sections) = commonmark_sections(markdown, FIELD_NAME_MAXLEN, FIELD_VALUE_MAXLEN - 10);
  let fields = sections
    .into_iter()
    .map(|(name, value)| {
      let value = if value.is_empty() { "```\n```".to_string() } else { format!("```md\n{}\n```", pystr::rstrip(&value)) };
      json!({ "name": name, "value": value })
    })
    .collect();
  (description, fields)
}

/// Escape every colon that opens a `:name:` so Discord shows it as text
/// (upstream `DISCORD_EMOJI_ESCAPE_RE` substitution). Discord draws any
/// name it knows as an emoji, even inside `2001:db8:100::1`. Escapes,
/// code spans, Discord tokens (`<:party:123>`, `<t:1700000000:R>`) and web
/// links are left exactly as they are.
///
/// The upstream pattern is
/// ``(?P<skip>\\.|(?P<tick>`+).+?(?P=tick)|<[^<>\s]*>|(?i:https?)://\S+)|:(?=[a-z0-9_+-]+:)``
/// (dot matching newlines); this scanner reproduces its matches in
/// linear time.
pub(crate) fn escape_emoji_names(text: &str) -> String {
  let s: Vec<char> = text.chars().collect();
  let n = s.len();

  // Maximal backtick runs, and the longest run at or after each one
  let mut runs: Vec<(usize, usize)> = Vec::new();
  let mut i = 0;
  while i < n {
    if s[i] == '`' {
      let start = i;
      while i < n && s[i] == '`' {
        i += 1;
      }
      runs.push((start, i - start));
    } else {
      i += 1;
    }
  }
  let mut longest_from = vec![0; runs.len() + 1];
  for r in (0..runs.len()).rev() {
    longest_from[r] = longest_from[r + 1].max(runs[r].1);
  }

  let is_name = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '+' | '-');
  let mut out = String::with_capacity(text.len());
  let mut run_idx = 0;
  let mut i = 0;
  while i < n {
    let c = s[i];
    match c {
      // An escaped character
      '\\' if i + 1 < n => {
        out.push(c);
        out.push(s[i + 1]);
        i += 2;
      }
      // A code span: a run of k backticks, at least one character, then
      // the first k backticks after it; the longest k that closes wins.
      '`' => {
        while runs[run_idx].0 + runs[run_idx].1 <= i {
          run_idx += 1;
        }
        let (start, len) = runs[run_idx];
        let run = start + len - i;
        let later = longest_from[run_idx + 1];
        // A shorter opening closes inside its own run when 2k + 1 fits
        let k = run.min(later).max((run - 1) / 2);
        if k == 0 {
          out.push(c);
          i += 1;
          continue;
        }
        let end = if 2 * k < run {
          i + 2 * k + 1
        } else {
          let mut r = run_idx + 1;
          while runs[r].1 < k {
            r += 1;
          }
          runs[r].0 + k
        };
        out.extend(&s[i..end]);
        i = end;
      }
      // A Discord token such as <@123> or <:party:123>
      '<' => {
        let close = s[i + 1..].iter().position(|&c| c == '<' || c == '>' || pystr::is_space(c)).map(|p| i + 1 + p);
        match close {
          Some(j) if s[j] == '>' => {
            out.extend(&s[i..=j]);
            i = j + 1;
          }
          _ => {
            out.push(c);
            i += 1;
          }
        }
      }
      // A web link, up to the next whitespace
      'h' | 'H' if link_len(&s[i..]).is_some() => {
        let end = i + link_len(&s[i..]).unwrap_or(1);
        out.extend(&s[i..end]);
        i = end;
      }
      // A colon that opens a :name:
      ':' => {
        let name = s[i + 1..].iter().take_while(|c| is_name(**c)).count();
        if name > 0 && s.get(i + 1 + name) == Some(&':') {
          out.push_str("\\:");
        } else {
          out.push(c);
        }
        i += 1;
      }
      _ => {
        out.push(c);
        i += 1;
      }
    }
  }
  out
}

/// The length of a `(?i:https?)://\S+` match at the start of `s`.
fn link_len(s: &[char]) -> Option<usize> {
  let eq = |c: char, want: char| c.eq_ignore_ascii_case(&want) || (want == 's' && c == '\u{17f}');
  if s.len() < 4 || !"http".chars().zip(s).all(|(w, c)| eq(*c, w)) {
    return None;
  }
  let mut i = 4;
  if s.get(i).is_some_and(|c| eq(*c, 's')) {
    i += 1;
  }
  if s.get(i..i + 3) != Some(&[':', '/', '/']) {
    return None;
  }
  i += 3;
  let rest = s[i..].iter().take_while(|c| !pystr::is_space(**c)).count();
  (rest > 0).then_some(i + rest)
}

#[async_trait]
impl Notify for Discord {
  fn schemas(&self) -> &[&str] {
    self.flavor.schemas
  }
  fn service_name(&self) -> &str {
    self.flavor.service_name
  }
  fn details(&self) -> ServiceDetails {
    (self.flavor.details)()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn attachment_support(&self) -> bool {
    true
  }
  fn notify_format(&self) -> NotifyFormat {
    self.notify_formats()[0].clone()
  }
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    if self.markdown_implied { vec![NotifyFormat::Markdown] } else { vec![NotifyFormat::Text, NotifyFormat::Markdown] }
  }
  fn notify_format_overridable(&self) -> bool {
    // ?thread= and ?url= replace whatever ?format= asked for
    !self.markdown_implied
  }
  fn body_maxlen(&self) -> usize {
    2000
  }
  fn overflow_amalgamate_title(&self) -> bool {
    true
  }
  fn request_rate_per_sec(&self) -> f64 {
    // Discord's rate-limit headers decide when to wait
    0.0
  }
  fn dialect_convert(&self, body: &str, _body_format: &NotifyFormat) -> String {
    if self.escape_emojis { escape_emoji_names(body) } else { body.to_string() }
  }
  fn has_dialect(&self) -> bool {
    true
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec![self.flavor.schemas[0].to_string(), self.webhook_id.clone(), self.webhook_token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let body = ctx.body.as_str();
    // The title skips dialect_convert(), so protect it here too, unless the
    // content has no declared format and skipped it on purpose
    let title = if self.escape_emojis && !ctx.body_passthrough { escape_emoji_names(&ctx.title) } else { ctx.title.clone() };
    // Embeds only when Markdown was asked for; Discord renders Markdown
    // sent as regular content on its own
    let use_embeds = self.format_override == Some(NotifyFormat::Markdown);

    let mut payload = Map::new();
    payload.insert("tts".into(), json!(self.tts));
    // Text-to-speech does not wait for the whole message
    payload.insert("wait".into(), json!(!self.tts));
    if let Some(flags) = self.flags.filter(|f| *f != 0) {
      payload.insert("flags".into(), json!(flags));
    }

    let image_url = ctx.image_url(NotifyImageSize::Xy256);
    if self.avatar
      && let Some(avatar) = self.avatar_url.clone().or_else(|| image_url.clone())
    {
      payload.insert("avatar_url".into(), json!(avatar));
    }
    if let Some(user) = &self.user {
      payload.insert("username".into(), json!(user));
    }

    // A template defines the whole message; otherwise embeds pick up the
    // body's mentions (ping= adds to them) and regular content only pings
    // what ping= lists.
    if self.template.is_none() {
      let pings = self.ping.join(" ");
      let found = if use_embeds {
        if self.ping.is_empty() { ping_payload(&[body]) } else { ping_payload(&[body, pings.as_str()]) }
      } else if !self.ping.is_empty() {
        ping_payload(&[pings.as_str()])
      } else {
        Map::new()
      };
      payload.extend(found);
    }

    if let Some(template) = &self.template {
      let Some(content) = self.gen_payload(template, ctx, &title, image_url.as_deref()).await else { return Ok(false) };
      payload.extend(content);

      // Skip the message if an earlier attempt already posted it
      if !is_delivered("message", false) {
        if !self.post(&client, &payload, &[]).await {
          return Ok(false);
        }
        mark_delivered("message", false);
      }
    } else if !body.is_empty() {
      // Fields that did not fit in the first message
      let mut extra: Vec<Value> = Vec::new();

      if use_embeds {
        let mut embed = Map::new();
        embed.insert("author".into(), json!({ "name": ctx.asset.app_id, "url": ctx.asset.app_url }));
        embed.insert("title".into(), json!(title));
        embed.insert("description".into(), json!(body));
        embed.insert("color".into(), json!(ctx.asset.color_int(&ctx.notify_type)));
        if let Some(href) = &self.href {
          embed.insert("url".into(), json!(href));
        }
        if self.footer {
          let mut footer = Map::new();
          footer.insert("text".into(), json!(ctx.asset.app_desc));
          if self.footer_logo
            && let Some(logo) = ctx.logo_url()
          {
            footer.insert("icon_url".into(), json!(logo));
          }
          embed.insert("footer".into(), Value::Object(footer));
        }
        if self.include_image
          && let Some(image) = &image_url
        {
          embed.insert("thumbnail".into(), json!({ "url": image, "height": 256, "width": 256 }));
        }
        if self.fields {
          // Headings become fields; the text before them stays the description
          let (description, mut fields) = extract_markdown_sections(body);
          embed.insert("description".into(), json!(description));
          if !fields.is_empty() {
            extra = fields.split_off(fields.len().min(self.limits.max_fields));
            embed.insert("fields".into(), Value::Array(fields));
          }
        }
        payload.insert("embeds".into(), json!([embed]));
      } else {
        // ping= content (if any) follows the message
        let pings = payload.get("content").and_then(Value::as_str).unwrap_or_default();
        let content = if title.is_empty() { format!("{body}{pings}") } else { format!("{title}\r\n{body}{pings}") };
        payload.insert("content".into(), json!(content));
      }

      if !is_delivered("message", false) {
        if !self.post(&client, &payload, &[]).await {
          return Ok(false);
        }
        mark_delivered("message", false);
      }

      if !extra.is_empty() {
        if let Some(embed) = payload.get_mut("embeds").and_then(|e| e.get_mut(0)) {
          embed["description"] = json!("");
        }
        for (n, chunk) in extra.chunks(self.limits.max_fields).enumerate() {
          // Each extra message of fields is tracked on its own
          let key = format!("fields:{}", n * self.limits.max_fields);
          if is_delivered(&key, false) {
            continue;
          }
          if let Some(embed) = payload.get_mut("embeds").and_then(|e| e.get_mut(0)) {
            embed["fields"] = Value::Array(chunk.to_vec());
          }
          if !self.post(&client, &payload, &[]).await {
            return Ok(false);
          }
          mark_delivered(&key, false);
        }
      }
    }

    if !ctx.attachments.is_empty() {
      // Keep the other settings, drop the message itself
      payload.insert("tts".into(), json!(false));
      payload.insert("wait".into(), json!(true));
      payload.remove("embeds");
      payload.remove("content");
      payload.remove("allow_mentions");

      // Batches in order, each within the per-message count and size
      // limits; batch=no sends one attachment per message
      let max_per = if self.batch { self.limits.max_attachments } else { 1 };
      let mut batches: Vec<Vec<&Attachment>> = Vec::new();
      let mut current: Vec<&Attachment> = Vec::new();
      let mut current_size = 0;
      for attachment in &ctx.attachments {
        let size = if self.batch { attachment.data.len() } else { 0 };
        if !current.is_empty() && (current.len() >= max_per || (self.batch && current_size + size > self.limits.max_attach_bytes)) {
          batches.push(std::mem::take(&mut current));
          current_size = 0;
        }
        current.push(attachment);
        current_size += size;
      }
      batches.push(current);

      for (index, batch) in batches.iter().enumerate() {
        let key = format!("attach:{index}");
        if is_delivered(&key, false) {
          continue;
        }
        if !self.post(&client, &payload, batch).await {
          return Ok(false);
        }
        mark_delivered(&key, false);
      }
    }

    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::notify::with_test_base;
  use serde_json::json;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  const IMAGE: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-256x256.png";
  const LOGO: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-logo.png";
  const HOOK: &str = "/api/webhooks/AAAA/BBBB";

  fn discord(url: &str) -> Discord {
    Discord::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(HOOK)).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    server
  }

  /// The webhook requests received, in order.
  async fn posts(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.url.path() == HOOK).collect()
  }

  fn json_of(r: &Request) -> Value {
    serde_json::from_slice(&r.body).unwrap()
  }

  /// The `payload_json` part of a multipart request.
  fn payload_json(r: &Request) -> Value {
    let body = String::from_utf8_lossy(&r.body);
    let start = body.find("name=\"payload_json\"").unwrap();
    let rest = &body[start..];
    let rest = &rest[rest.find("\r\n\r\n").unwrap() + 4..];
    serde_json::from_str(&rest[..rest.find("\r\n--").unwrap()]).unwrap()
  }

  fn file_names(r: &Request) -> Vec<(String, String)> {
    let body = String::from_utf8_lossy(&r.body).to_string();
    body
      .split("Content-Disposition: form-data; name=\"")
      .skip(1)
      .filter_map(|part| {
        let (field, rest) = part.split_once('"')?;
        let name = rest.split_once("filename=\"")?.1.split_once('"')?.0;
        Some((field.to_string(), name.to_string()))
      })
      .collect()
  }

  fn attachment(name: &str, data: &[u8], mime: &str) -> Attachment {
    Attachment { name: name.into(), data: data.to_vec(), mime_type: mime.into() }
  }

  #[test]
  fn test_valid_urls() {
    let (i, t) = ("i".repeat(24), "t".repeat(64));
    let urls = [
      format!("discord://{i}/{t}"),
      format!("discord://l2g@{i}/{t}"),
      format!("discord://{i}/{t}?format=markdown&footer=Yes&image=Yes&ping=Joe"),
      format!("discord://{i}/{t}?format=markdown&footer=Yes&image=No&fields=no"),
      format!("discord://jack@{i}/{t}?format=markdown&footer=Yes&image=Yes"),
      format!("https://discord.com/api/webhooks/{}/{}", "0".repeat(10), "B".repeat(40)),
      format!("https://discordapp.com/api/webhooks/{}/{}", "0".repeat(10), "B".repeat(40)),
      format!("https://discordapp.com/api/webhooks/{}/{}?footer=yes", "0".repeat(10), "B".repeat(40)),
      format!("https://discordapp.com/api/webhooks/{}/{}?footer=yes&botname=joe", "0".repeat(10), "B".repeat(40)),
      format!("discord://{i}/{t}?format=markdown&avatar=No&footer=No"),
      format!("discord://{i}/{t}?flags=1"),
      format!("discord://{i}/{t}?flags=0"),
      format!("discord://{i}/{t}?format=markdown"),
      format!("discord://{i}/{t}?format=markdown&thread=abc123"),
      format!("discord://{i}/{t}?format=text"),
      format!("discord://{i}/{t}?hmarkdown=true&ref=http://localhost"),
      format!("discord://{i}/{t}?markdown=true&url=http://localhost"),
      format!("discord://{i}/{t}?avatar_url=http://localhost/test.jpg"),
      format!("discord://{i}/{t}?batch=yes"),
      format!("discord://{i}/{t}?batch=no"),
      format!("discord://{}/{}/", "a".repeat(24), "b".repeat(64)),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let (i, t) = ("i".repeat(24), "t".repeat(64));
    let urls = [
      "discord://".to_string(),
      "discord://:@/".to_string(),
      format!("discord://{i}"),
      format!("discord://%20%20/{t}"),
      format!("discord://{i}/%20%20"),
      format!("discord://{i}/{t}?flags=-1"),
      format!("discord://{i}/{t}?flags=invalid"),
      format!("discord://{i}/{t}?template=invalid://template.json"),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_options() {
    let d = discord("discord://bob@AAAA/BBBB/?botname=joe&flags=4&ping=@joe,<@321>,<@%26654>&tts=yes&batch=no&:mytoken=hi");
    assert_eq!(d.user.as_deref(), Some("joe"));
    assert_eq!(d.flags, Some(4));
    // parse_list() sorts and de-duplicates the ping targets
    assert_eq!(d.ping, vec!["<@&654>", "<@321>", "@joe"]);
    assert!(d.tts);
    assert!(!d.batch);
    assert_eq!(d.tokens.get("mytoken").map(String::as_str), Some("hi"));
    assert_eq!(d.format_override, None);
    assert_eq!(d.notify_formats(), vec![NotifyFormat::Text, NotifyFormat::Markdown]);
    assert!(d.notify_format_overridable());
    assert_eq!(d.url_identifier(), Some(vec!["discord".to_string(), "AAAA".to_string(), "BBBB".to_string()]));

    // ?url= (not ?href=) and ?thread= switch to Markdown, whatever ?format= says
    for url in ["discord://AAAA/BBBB/?url=https://a.ca&format=text", "discord://AAAA/BBBB/?thread=12345&format=html"] {
      let d = discord(url);
      assert_eq!(d.format_override, Some(NotifyFormat::Markdown), "{url}");
      assert_eq!(d.notify_formats(), vec![NotifyFormat::Markdown], "{url}");
      assert!(!d.notify_format_overridable(), "{url}");
    }
    let d = discord("discord://AAAA/BBBB/?href=https://a.ca&url=https://b.ca");
    assert_eq!(d.href.as_deref(), Some("https://a.ca"));
    assert_eq!(d.format_override, None);
    assert_eq!(discord("discord://AAAA/BBBB/?format=markdown").format_override, Some(NotifyFormat::Markdown));

    // Bot names are decoded
    for (quoted, name) in [("App%201", "App 1"), ("a%2Fb%3Ac", "a/b:c"), ("you%26me", "you&me")] {
      assert_eq!(discord(&format!("discord://{quoted}@AAAA/BBBB/")).user.as_deref(), Some(name));
    }

    // Only ?emojis=no turns escaping on
    assert!(discord("discord://AAAA/BBBB/?emojis=no").escape_emojis);
    assert!(!discord("discord://AAAA/BBBB/?emojis=yes").escape_emojis);
    assert!(!discord("discord://AAAA/BBBB/").escape_emojis);
    assert!(discord("discord://AAAA/BBBB/").has_dialect());
  }

  #[test]
  fn test_extract_markdown_sections() {
    let (desc, fields) = extract_markdown_sections("body");
    assert_eq!(desc, "body");
    assert!(fields.is_empty());

    let markdown = "## Heading one\nbody body\n\n# Heading 2 ##\n\nTest\n\nmore content\neven more content  \t\r\n\n\n# Heading 3 ##\n\n\nnormal content\n# heading 4\n#### Heading 5";
    let (desc, fields) = extract_markdown_sections(markdown);
    assert_eq!(desc, "");
    assert_eq!(
      fields,
      vec![
        json!({"name": "Heading one", "value": "```md\nbody body\n```"}),
        json!({"name": "Heading 2", "value": "```md\nTest\n\nmore content\neven more content\n```"}),
        json!({"name": "Heading 3", "value": "```md\nnormal content\n```"}),
        json!({"name": "heading 4", "value": "```\n```"}),
        json!({"name": "Heading 5", "value": "```\n```"}),
      ]
    );

    // Long sections are split to fit a field, long headings trimmed
    let (_, fields) = extract_markdown_sections(&format!("# {}\n{}", "n".repeat(300), "x".repeat(2500)));
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0]["name"], "n".repeat(FIELD_NAME_MAXLEN));
    assert!(fields.iter().all(|f| f["value"].as_str().unwrap().chars().count() <= FIELD_VALUE_MAXLEN));
    let joined: String = fields.iter().map(|f| f["value"].as_str().unwrap().trim_start_matches("```md\n").trim_end_matches("\n```").to_string()).collect();
    assert_eq!(joined, "x".repeat(2500));
  }

  #[test]
  fn test_ping_payload() {
    let found = ping_payload(&["@everyone and @admin, meet <@123>; <@&456>\" <@123>", "<@333> <@&444> @joe @123x"]);
    assert_eq!(
      Value::Object(found),
      json!({
        "allow_mentions": {"parse": ["everyone", "admin", "joe"], "users": ["123", "333"], "roles": ["456", "444"]},
        "content": "👉 @everyone @admin @joe <@&456> <@&444> <@123> <@333>",
      })
    );
    assert!(ping_payload(&["no pings here", " "]).is_empty());
    // <@joe> is a name mention, @&5 a role
    assert_eq!(
      Value::Object(ping_payload(&["<@joe> @&5x @&y"])),
      json!({"allow_mentions": {"parse": ["joe"], "users": [], "roles": ["5"]}, "content": "👉 @joe <@&5>"})
    );
  }

  #[test]
  fn test_emoji_escape_cases() {
    let cases = [
      ("2001:db8:100::1", "2001\\:db8\\:100::1"),
      ("00:ab:cd:ef:12:34", "00\\:ab\\:cd\\:ef\\:12:34"),
      ("12:30:45", "12\\:30:45"),
      (":smile:", "\\:smile:"),
      (":smile::smile:", "\\:smile:\\:smile:"),
      (":a:b:", "\\:a\\:b:"),
      ("ok:smile:ok", "ok\\:smile:ok"),
      (":+1: :-1:", "\\:+1: \\:-1:"),
      ("**:fire:**", "**\\:fire:**"),
      ("_:fire:_", "_\\:fire:_"),
      ("# Title :fire:", "# Title \\:fire:"),
      ("> quote :fire:", "> quote \\:fire:"),
      ("- item :fire:", "- item \\:fire:"),
      ("`2001:db8:100::1`", "`2001:db8:100::1`"),
      ("``a:b:c``", "``a:b:c``"),
      ("x `:a:` :b: `:c:`", "x `:a:` \\:b: `:c:`"),
      ("```\n:smile:\n```", "```\n:smile:\n```"),
      ("```py\nx = {'a:b:': 1}\n```\n:c:", "```py\nx = {'a:b:': 1}\n```\n\\:c:"),
      ("`:smile:", "`\\:smile:"),
      ("<:party:123>", "<:party:123>"),
      ("<a:wave:456>", "<a:wave:456>"),
      ("<t:1700000000:R>", "<t:1700000000:R>"),
      ("<@123> <@&456> <#789> </cmd:9>", "<@123> <@&456> <#789> </cmd:9>"),
      ("a < b :c: > d", "a < b \\:c: > d"),
      ("https://[fe80::1:2]/x:y:", "https://[fe80::1:2]/x:y:"),
      ("HTTP://a.ca/:b:", "HTTP://a.ca/:b:"),
      ("http://host:8080/", "http://host:8080/"),
      ("<https://a.ca/:b:>", "<https://a.ca/:b:>"),
      ("[link](https://a.ca/:b:) :c:", "[link](https://a.ca/:b:) \\:c:"),
      ("\\:smile:", "\\:smile:"),
      ("\\\\:smile:", "\\\\\\:smile:"),
      ("time: 5", "time: 5"),
      ("a::b", "a::b"),
      (":AB:", ":AB:"),
      (":", ":"),
      ("", ""),
      // Backtracking of the opening backtick run, as Python's regex does
      ("``a`:b:", "``a`\\:b:"),
      ("```:a:", "```\\:a:"),
      ("````:a:", "````\\:a:"),
      ("```:a:`", "```\\:a:`"),
      ("`````x``", "`````x``"),
      ("`` :a: `", "`` :a: `"),
      ("http:// :a:", "http:// \\:a:"),
    ];
    for (text, expected) in cases {
      let d = discord("discord://AAAA/BBBB/");
      assert_eq!(d.dialect_convert(text, &NotifyFormat::Text), text, "unset: {text:?}");
      let d = discord("discord://AAAA/BBBB/?emojis=yes");
      assert_eq!(d.dialect_convert(text, &NotifyFormat::Text), text, "on: {text:?}");
      let d = discord("discord://AAAA/BBBB/?emojis=no");
      assert_eq!(d.dialect_convert(text, &NotifyFormat::Markdown), expected, "off: {text:?}");
    }
  }

  #[test]
  fn test_emoji_escape_speed() {
    let cases: [(&str, &str, usize); 10] = [
      ("", "`x", 100000),
      ("", "``x", 70000),
      ("``", "x", 200000),
      ("", "<a:b:", 40000),
      ("http://", ":a", 100000),
      ("", "a:", 100000),
      ("", ":", 200000),
      ("", "\\", 200000),
      ("", "`a:b\\:<", 30000),
      ("", "`", 200000),
    ];
    for (prefix, piece, count) in cases {
      let text = format!("{}{}", prefix, piece.repeat(count));
      let start = std::time::Instant::now();
      escape_emoji_names(&text);
      assert!(start.elapsed() < Duration::from_secs(2), "{prefix:?} + {piece:?} * {count}");
    }
  }

  /// `?emojis=no` leaves a title with no declared format alone, as its body
  /// (upstream skips the title's dialect_convert() for passthrough content).
  #[tokio::test]
  async fn test_passthrough_title_keeps_emoji_names() {
    let server = mock(204).await;
    let (title, body) = ("Disk :fire: at 10:30", "Body :smile: `:x:` <b>");
    // What upstream sends for notify(body_format=None), then "markdown"
    for (query, passthrough, sent) in [
      ("?emojis=no", true, json!({"content": "Disk :fire: at 10:30\r\nBody :smile: `:x:` <b>"})),
      ("?emojis=no", false, json!({"content": "Disk \\:fire: at 10:30\r\nBody \\:smile: `:x:` <b>"})),
      ("?emojis=no&format=markdown", true, json!({"embeds": [{"title": "Disk :fire: at 10:30", "description": "Body :smile: `:x:` <b>"}]})),
      ("?emojis=no&format=markdown", false, json!({"embeds": [{"title": "Disk \\:fire: at 10:30", "description": "Body \\:smile: `:x:` <b>"}]})),
    ] {
      server.reset().await;
      Mock::given(method("POST")).and(path(HOOK)).respond_with(ResponseTemplate::new(204)).mount(&server).await;
      let mut a = crate::Apprise::new();
      assert!(a.add(&format!("discord://AAAA/BBBB/{query}")));
      let format = if passthrough { NotifyFormat::Text } else { NotifyFormat::Markdown };
      let c = NotifyContext { body_passthrough: passthrough, body_format: format, ..ctx(title, body) };
      assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
      let got = json_of(&posts(&server).await[0]);
      match sent.get("content") {
        Some(content) => assert_eq!(&got["content"], content, "{query} passthrough={passthrough}"),
        None => {
          let (embed, want) = (&got["embeds"][0], &sent["embeds"][0]);
          assert_eq!((&embed["title"], &embed["description"]), (&want["title"], &want["description"]), "{query} passthrough={passthrough}");
        }
      }
    }
  }

  /// Embeds, pings from the body and ping=, footer and thumbnail; checked
  /// against upstream (pycapture).
  #[tokio::test]
  async fn test_markdown_embed_payload() {
    let server = mock(204).await;
    let d = discord("discord://AAAA/BBBB?format=markdown&footer=yes&image=yes&ping=<@333>");
    let mut c = ctx("T", "# H1\nhello <@111> @everyone\n## H2\nv2");
    c.body_format = NotifyFormat::Markdown;
    assert!(with_test_base(server.uri(), d.send(&c)).await.unwrap());

    let requests = posts(&server).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers.get("Content-Type").unwrap(), "application/json; charset=utf-8");
    assert_eq!(requests[0].headers.get("User-Agent").unwrap(), APP_ID);
    assert_eq!(requests[0].url.query(), None);
    assert_eq!(
      json_of(&requests[0]),
      json!({
        "tts": false,
        "wait": true,
        "avatar_url": IMAGE,
        "allow_mentions": {"parse": ["everyone"], "users": ["111", "333"], "roles": []},
        "content": "👉 @everyone <@111> <@333>",
        "embeds": [{
          "author": {"name": "Apprise", "url": "https://github.com/caronc/apprise"},
          "title": "T",
          "description": "",
          "color": 3843043,
          "footer": {"text": "Apprise Notifications", "icon_url": LOGO},
          "thumbnail": {"url": IMAGE, "height": 256, "width": 256},
          "fields": [
            {"name": "H1", "value": "```md\nhello <@111> @everyone\n```"},
            {"name": "H2", "value": "```md\nv2\n```"},
          ],
        }],
      })
    );
  }

  /// Regular content only pings what ping= lists, after the message.
  #[tokio::test]
  async fn test_text_ping_payload() {
    let server = mock(200).await;
    let d = discord("discord://AAAA/BBBB/?ping=@joe,<@321>,<@%26654>&format=text&botname=bob&flags=4&tts=yes");
    assert!(with_test_base(server.uri(), d.send(&ctx("T", "hi <@999>"))).await.unwrap());
    assert_eq!(
      json_of(&posts(&server).await[0]),
      json!({
        "tts": true,
        "wait": false,
        "flags": 4,
        "avatar_url": IMAGE,
        "username": "bob",
        "allow_mentions": {"parse": ["joe"], "users": ["321"], "roles": ["654"]},
        "content": "T\r\nhi <@999>👉 @joe <@&654> <@321>",
      })
    );
  }

  /// ?thread= goes in the query and switches to embeds; no pings without ping=.
  #[tokio::test]
  async fn test_thread_and_href() {
    let server = mock(204).await;
    let d = discord("discord://AAAA/BBBB/?thread=12345&avatar=no&href=https://a.ca&fields=no&flags=0");
    assert!(with_test_base(server.uri(), d.send(&ctx("T", "line one <@1>"))).await.unwrap());
    let requests = posts(&server).await;
    assert_eq!(requests[0].url.query(), Some("thread_id=12345"));
    assert_eq!(
      json_of(&requests[0]),
      json!({
        "tts": false,
        "wait": true,
        "allow_mentions": {"parse": [], "users": ["1"], "roles": []},
        "content": "👉 <@1>",
        "embeds": [{
          "author": {"name": "Apprise", "url": "https://github.com/caronc/apprise"},
          "title": "T",
          "description": "line one <@1>",
          "color": 3843043,
          "url": "https://a.ca",
        }],
      })
    );
  }

  /// More fields than one embed takes continue in further messages with an
  /// empty description (pycapture).
  #[tokio::test]
  async fn test_fields_overflow() {
    let server = mock(200).await;
    let d = discord("discord://AAAA/BBBB/?format=markdown&avatar=no&ping=@here");
    let body = format!("intro\n{}", (0..12).map(|i| format!("# H{i}\nv{i}")).collect::<Vec<_>>().join("\n"));
    assert!(with_test_base(server.uri(), d.send(&ctx("T", &body))).await.unwrap());

    let requests = posts(&server).await;
    assert_eq!(requests.len(), 2);
    let field = |i: usize| json!({"name": format!("H{i}"), "value": format!("```md\nv{i}\n```")});
    let expected = |description: &str, fields: Vec<Value>| {
      json!({
        "tts": false,
        "wait": true,
        "allow_mentions": {"parse": ["here"], "users": [], "roles": []},
        "content": "👉 @here",
        "embeds": [{
          "author": {"name": "Apprise", "url": "https://github.com/caronc/apprise"},
          "title": "T",
          "description": description,
          "color": 3843043,
          "fields": fields,
        }],
      })
    };
    assert_eq!(json_of(&requests[0]), expected("intro", (0..10).map(field).collect()));
    assert_eq!(json_of(&requests[1]), expected("", (10..12).map(field).collect()));

    // One field per message
    let server = mock(200).await;
    let mut d = discord("discord://AAAA/BBBB/?format=markdown&fields=yes");
    d.limits.max_fields = 1;
    assert!(with_test_base(server.uri(), d.send(&ctx("", "# H1\nv1\n# H2\nv2\n# H3\nv3\n"))).await.unwrap());
    assert_eq!(posts(&server).await.len(), 3);

    // Without mentions there is no content or allow_mentions
    let server = mock(200).await;
    let d = discord("discord://AAAA/BBBB/?format=markdown");
    assert!(with_test_base(server.uri(), d.send(&ctx("", "Hello world"))).await.unwrap());
    let payload = json_of(&posts(&server).await[0]);
    assert!(payload.get("content").is_none() && payload.get("allow_mentions").is_none());
  }

  /// A template replaces the message (pycapture), with tokens JSON-escaped.
  #[tokio::test]
  async fn test_template_payload() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("discord.json");
    std::fs::write(
      &file,
      r#"{"content": "{{app_title}}: {{app_body}} {{mytoken}}", "embeds": [{"title": "{{ app_title }}", "color": {{app_color_int}}, "description": "{{app_type}} {{app_color}} {{app_image_url}} {{app_id}}"}]}"#,
    )
    .unwrap();

    let server = mock(200).await;
    let d = discord(&format!("discord://AAAA/BBBB/?template={}&:mytoken=hello&ping=<@1>", file.display()));
    let mut c = ctx("the \"title\"", "body\ntext é");
    c.notify_type = crate::types::NotifyType::Warning;
    assert!(with_test_base(server.uri(), d.send(&c)).await.unwrap());
    assert_eq!(
      json_of(&posts(&server).await[0]),
      json!({
        "tts": false,
        "wait": true,
        "avatar_url": IMAGE.replace("info", "warning"),
        "content": "the \"title\": body\ntext é hello",
        "embeds": [{
          "title": "the \"title\"",
          "color": 13291305,
          "description": format!("warning #CACF29 {} Apprise", IMAGE.replace("info", "warning")),
        }],
      })
    );
  }

  #[tokio::test]
  async fn test_template_failures() {
    let dir = tempfile::tempdir().unwrap();
    let server = mock(200).await;
    let cases = [
      ("invalid.json", "{"),
      ("list.json", "[1, 2]"),
      ("empty.json", r#"{"content": ""}"#),
      ("no-embeds.json", r#"{"embeds": []}"#),
      ("bad-embed.json", r#"{"embeds": ["text"]}"#),
      ("big.json", &format!(r#"{{"content": "{}"}}"#, "x".repeat(MAX_TEMPLATE_SIZE))),
    ];
    for (name, content) in cases {
      let file = dir.path().join(name);
      std::fs::write(&file, content).unwrap();
      let d = discord(&format!("discord://AAAA/BBBB/?template={}", file.display()));
      assert!(!with_test_base(server.uri(), d.send(&ctx("t", "b"))).await.unwrap(), "{name}");
    }
    // A missing template fails the send, not the URL
    let d = discord(&format!("discord://AAAA/BBBB/?template={}", dir.path().join("missing.json").display()));
    assert!(!with_test_base(server.uri(), d.send(&ctx("t", "b"))).await.unwrap());
    assert!(posts(&server).await.is_empty());

    // Embeds alone are enough; a template with content but odd embeds passes
    for content in [r#"{"embeds": [{"title": "x"}]}"#, r#"{"content": "x", "embeds": "odd"}"#] {
      let file = dir.path().join("ok.json");
      std::fs::write(&file, content).unwrap();
      let d = discord(&format!("discord://AAAA/BBBB/?template={}", file.display()));
      assert!(with_test_base(server.uri(), d.send(&ctx("t", "b"))).await.unwrap(), "{content}");
    }
  }

  /// Attachments go in one multipart message after the text (pycapture).
  #[tokio::test]
  async fn test_attachments() {
    let server = mock(200).await;
    let d = discord("discord://AAAA/BBBB?botname=bob&flags=4&format=markdown&ping=<@1>");
    let mut c = ctx("T", "B");
    c.attachments = vec![attachment("a.txt", b"hello", "text/plain"), attachment("b.png", b"PNGDATA", "image/png")];
    assert!(with_test_base(server.uri(), d.send(&c)).await.unwrap());

    let requests = posts(&server).await;
    assert_eq!(requests.len(), 2);
    assert!(json_of(&requests[0]).get("embeds").is_some());
    assert!(requests[1].headers.get("Content-Type").unwrap().to_str().unwrap().starts_with("multipart/form-data"));
    assert_eq!(payload_json(&requests[1]), json!({"tts": false, "wait": true, "flags": 4, "avatar_url": IMAGE, "username": "bob"}));
    assert_eq!(file_names(&requests[1]), vec![("files[0]".to_string(), "a.txt".to_string()), ("files[1]".to_string(), "b.png".to_string())]);
    let body = String::from_utf8_lossy(&requests[1].body).to_string();
    assert!(body.contains("Content-Type: image/png\r\n\r\nPNGDATA"));

    // Attachments only: no text message
    let server = mock(200).await;
    let mut c = ctx("", "");
    c.attachments = vec![attachment("a.txt", b"hello", "text/plain")];
    assert!(with_test_base(server.uri(), d.send(&c)).await.unwrap());
    assert_eq!(posts(&server).await.len(), 1);
  }

  #[tokio::test]
  async fn test_attachment_batches() {
    let files = || vec![attachment("a.gif", b"GIF", "image/gif"), attachment("b.png", b"PNG", "image/png"), attachment("c.jpeg", b"JPG", "image/jpeg")];
    let run = |d: Discord| async move {
      let server = mock(200).await;
      let mut c = ctx("", "test");
      c.attachments = files();
      assert!(with_test_base(server.uri(), d.send(&c)).await.unwrap());
      posts(&server).await.iter().skip(1).map(|r| file_names(r).into_iter().map(|(_, n)| n).collect::<Vec<_>>()).collect::<Vec<_>>()
    };

    assert_eq!(run(discord("discord://AAAA/BBBB/")).await, vec![vec!["a.gif", "b.png", "c.jpeg"]]);
    assert_eq!(run(discord("discord://AAAA/BBBB/?batch=no")).await, vec![vec!["a.gif"], vec!["b.png"], vec!["c.jpeg"]]);
    let mut d = discord("discord://AAAA/BBBB/");
    d.limits.max_attachments = 2;
    assert_eq!(run(d).await, vec![vec!["a.gif", "b.png"], vec!["c.jpeg"]]);
    let mut d = discord("discord://AAAA/BBBB/");
    d.limits.max_attach_bytes = 6;
    assert_eq!(run(d).await, vec![vec!["a.gif", "b.png"], vec!["c.jpeg"]]);
    let mut d = discord("discord://AAAA/BBBB/");
    d.limits.max_attach_bytes = 1;
    assert_eq!(run(d).await, vec![vec!["a.gif"], vec!["b.png"], vec!["c.jpeg"]]);
  }

  #[tokio::test]
  async fn test_failures() {
    for status in [500, 999, 400] {
      let server = mock(status).await;
      let d = discord("discord://AAAA/BBBB/");
      assert!(!with_test_base(server.uri(), d.send(&ctx("t", "b"))).await.unwrap(), "{status}");
    }

    // A failed attachment upload fails the send after the message went out
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(HOOK)).respond_with(ResponseTemplate::new(200)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).and(path(HOOK)).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    let mut c = ctx("t", "b");
    c.attachments = vec![attachment("a.txt", b"x", "text/plain")];
    assert!(!with_test_base(server.uri(), discord("discord://AAAA/BBBB/").send(&c)).await.unwrap());
    assert_eq!(posts(&server).await.len(), 2);

    // Connection errors are a failure, not an error
    let d = discord("discord://AAAA/BBBB/");
    assert!(!with_test_base("http://127.0.0.1:1", d.send(&ctx("t", "b"))).await.unwrap());
  }

  /// A 429 is retried once, after waiting for the reset Discord announced.
  #[tokio::test]
  async fn test_rate_limit() {
    let server = MockServer::start().await;
    let reset = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 1;
    Mock::given(method("POST"))
      .and(path(HOOK))
      .respond_with(ResponseTemplate::new(429).insert_header("X-RateLimit-Remaining", "0").insert_header("X-RateLimit-Reset", reset.to_string().as_str()))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(HOOK)).respond_with(ResponseTemplate::new(204).insert_header("X-RateLimit-Remaining", "4")).mount(&server).await;

    let mut d = discord("discord://AAAA/BBBB/");
    d.clock_skew = Duration::ZERO;
    let started = std::time::Instant::now();
    let sent = crate::notify::with_throttling_in_tests(crate::notify::with_throttle(
      0.0,
      Default::default(),
      with_test_base(server.uri(), async {
        // The first request of a service never waits
        assert!(d.send(&ctx("t", "first")).await.unwrap());
        assert_eq!(d.ratelimit.lock().unwrap().remaining, 4.0);
        true
      }),
    ))
    .await;
    assert!(sent);
    assert_eq!(posts(&server).await.len(), 2);
    assert!(started.elapsed() >= Duration::from_millis(1), "the retry waited");

    // A second 429 in a row gives up
    let server = mock(429).await;
    assert!(!with_test_base(server.uri(), discord("discord://AAAA/BBBB/").send(&ctx("t", "b"))).await.unwrap());
    assert_eq!(posts(&server).await.len(), 2);

    // Unusable headers are ignored; a usable remaining count is kept alone
    let d = discord("discord://AAAA/BBBB/");
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("X-RateLimit-Remaining", "x".parse().unwrap());
    d.update_rate_limit(&headers);
    assert_eq!(d.ratelimit.lock().unwrap().remaining, 1.0);
    headers.insert("X-RateLimit-Remaining", "0".parse().unwrap());
    headers.insert("X-RateLimit-Reset", "1.5".parse().unwrap());
    d.update_rate_limit(&headers);
    assert_eq!(d.ratelimit.lock().unwrap().remaining, 0.0);
    assert!(d.rate_limit_wait().is_none_or(|w| w <= CLOCK_SKEW + Duration::from_secs(1)));
  }

  /// Through Apprise: format selection, title escaping and emoji handling
  /// as upstream's tests check them.
  #[tokio::test]
  async fn test_notify_formats_and_emojis() {
    async fn notify(url: &str, title: &str, body: &str, format: NotifyFormat) -> Vec<Value> {
      let server = mock(200).await;
      let mut a = crate::Apprise::new();
      assert!(a.add(url));
      let c = NotifyContext { title: title.into(), body: body.into(), body_format: format, ..Default::default() };
      assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success(), "{url}");
      posts(&server).await.iter().map(json_of).collect()
    }

    // Markdown input goes out as regular content unless Markdown was asked for
    let body = "## Summary\nAll **good** <@123>";
    let payload = &notify("discord://AAAA/BBBB/", "Build", body, NotifyFormat::Markdown).await[0];
    assert_eq!(payload["content"], format!("Build\r\n{body}"));
    assert!(payload.get("embeds").is_none() && payload.get("allow_mentions").is_none());
    for extra in ["format=markdown", "url=https://a.ca", "thread=12345"] {
      let payload = &notify(&format!("discord://AAAA/BBBB/?{extra}"), "Build", body, NotifyFormat::Markdown).await[0];
      assert_eq!(payload["embeds"][0]["title"], "Build", "{extra}");
      assert_eq!(payload["embeds"][0]["fields"][0]["name"], "Summary", "{extra}");
      assert_eq!(payload["allow_mentions"]["users"], json!(["123"]), "{extra}");
    }

    // HTML becomes Markdown
    let payload = &notify("discord://AAAA/BBBB/?format=markdown&fields=no", "", "<b>hello</b> <i>world</i>", NotifyFormat::Html).await[0];
    assert_eq!(payload["embeds"][0]["description"], "**hello** *world*");

    let body = "Peer 2001:db8:100::1 :smile: `00:ab:cd` <t:1700000000:R> <:party:123> https://[fe80::1:2]/ a\\:b:";
    let escaped = "Peer 2001\\:db8\\:100::1 \\:smile: `00:ab:cd` <t:1700000000:R> <:party:123> https://[fe80::1:2]/ a\\:b:";
    for format in [NotifyFormat::Text, NotifyFormat::Markdown] {
      let payload = &notify("discord://AAAA/BBBB/?emojis=no", "MAC 00:ab:cd:", body, format).await[0];
      assert_eq!(payload["content"], format!("MAC 00\\:ab\\:cd:\r\n{escaped}"));
    }
    let payload = &notify("discord://AAAA/BBBB/?format=markdown&emojis=no", "MAC 00:ab:cd:", body, NotifyFormat::Markdown).await[0];
    assert_eq!(payload["embeds"][0]["title"], "MAC 00\\:ab\\:cd:");
    assert_eq!(payload["embeds"][0]["description"], escaped);
    let payload = &notify("discord://AAAA/BBBB/", "MAC 00:ab:cd:", body, NotifyFormat::Text).await[0];
    assert_eq!(payload["content"], format!("MAC 00:ab:cd:\r\n{body}"));
    let payload = &notify("discord://AAAA/BBBB/?emojis=yes", "", ":smile: :not_an_apprise_emoji:", NotifyFormat::Text).await[0];
    assert_eq!(payload["content"], "😄 :not_an_apprise_emoji:");

    // A message full of names still fits once its escapes are added
    let body = format!("{}:", ":a".repeat(1500));
    let contents: Vec<String> = notify("discord://AAAA/BBBB/?overflow=split&emojis=no", "", &body, NotifyFormat::Text)
      .await
      .iter()
      .map(|p| p["content"].as_str().unwrap().to_string())
      .collect();
    assert!(contents.len() > 1);
    assert!(contents.iter().all(|c| c.chars().count() <= 2000));
    assert_eq!(contents.concat().replace("\\:", ":"), body);
  }

  /// A retry only posts what did not arrive.
  #[tokio::test]
  async fn test_retry_skips_sent() {
    // Messages arrive, attachment uploads always fail
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(HOOK))
      .and(wiremock::matchers::header_regex("Content-Type", "^multipart/"))
      .respond_with(ResponseTemplate::new(500))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(HOOK)).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let mut a = crate::Apprise::new();
    assert!(a.add("discord://AAAA/BBBB/?retry=2&wait=0"));
    let mut c = ctx("", "hello");
    c.attachments = vec![attachment("a.gif", b"GIF", "image/gif")];
    assert!(!with_test_base(server.uri(), a.notify(&c)).await.is_success());
    let requests = posts(&server).await;
    let uploads = requests.iter().filter(|r| r.headers.get("Content-Type").unwrap().to_str().unwrap().starts_with("multipart/")).count();
    assert_eq!(requests.len() - uploads, 1);
    assert_eq!(uploads, 3);
  }
}
