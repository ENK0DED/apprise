//! Slack (upstream `NotifySlack`).
//!
//! Five modes:
//! - `hook`: an incoming webhook,
//!   `https://hooks.slack.com/services/TOKEN_A/TOKEN_B/TOKEN_C`, written
//!   `slack://TOKEN_A/TOKEN_B/TOKEN_C[/#channel...]`;
//! - `gov-hook`: the same on `hooks.slack-gov.com` (`?mode=gov-hook`);
//! - `bot`: a bot or user OAuth token, `slack://xoxb-.../#channel/@user/email`,
//!   the only mode that can upload attachments;
//! - `workflow` and `trigger`: Workflow Builder webhooks
//!   (`https://hooks.slack.com/workflows/T/F/X/Y`, `.../triggers/T/X/Y`),
//!   written `slack://T/F/X/Y?mode=workflow` and `slack://T/X/Y?mode=trigger`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::asset::NotifyImageSize;
use crate::conversion::{
  commonmark_emphasis_run, commonmark_escape_link_url, commonmark_find_backtick_run, commonmark_headings_to_bold, commonmark_index_backtick_runs,
  commonmark_new_scan_budget, commonmark_pick_emphasis_sentinel, commonmark_render_emphasis_markers, commonmark_scan_angle_dest, commonmark_scan_autolink_dest,
  commonmark_scan_paren_dest, pystr,
};
use crate::delivery::{is_delivered, mark_delivered, recall, remember};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::email::is_email;
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::plugin_args::{parse_list, qsd_bool, split_path};
use crate::utils::templates::{TemplateFile, TemplateType, app_tokens, apply_template};

/// Largest template accepted, in bytes (upstream `max_slack_template_size`).
const MAX_TEMPLATE_SIZE: usize = 35000;

/// Bots have no channel of their own (upstream `default_notification_channel`).
const DEFAULT_CHANNEL: &str = "#general";

/// Upstream `SlackMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  Webhook,
  WebhookGov,
  Bot,
  Workflow,
  Trigger,
}

/// Upstream `SLACK_MODES`, in order: `?mode=` takes the first one it starts.
const MODES: [(&str, Mode); 5] =
  [("hook", Mode::Webhook), ("gov-hook", Mode::WebhookGov), ("bot", Mode::Bot), ("workflow", Mode::Workflow), ("trigger", Mode::Trigger)];

impl Mode {
  fn from_prefix(prefix: &str) -> Option<Self> {
    MODES.iter().find(|(name, _)| name.starts_with(prefix)).map(|(_, m)| *m)
  }

  fn as_str(self) -> &'static str {
    MODES.iter().find(|(_, m)| *m == self).map(|(name, _)| *name).unwrap_or("hook")
  }

  fn is_workflow(self) -> bool {
    matches!(self, Mode::Workflow | Mode::Trigger)
  }
}

/// Upstream `CHANNEL_LIST_DELIM`: `[ \t\r\n,#\\/]+`.
fn split_channels(value: &str) -> impl Iterator<Item = String> + '_ {
  value.split([' ', '\t', '\r', '\n', ',', '#', '\\', '/']).filter(|s| !s.is_empty()).map(str::to_string)
}

/// Upstream `validate_regex()` (after strip) for the webhook tokens:
/// `^[A-Z0-9]+$` in any case.
fn webhook_token(value: Option<&str>) -> Option<String> {
  let value = pystr::strip(value?);
  (!value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric())).then(|| value.to_string())
}

/// `^(?:xoxe\.)?xox[abp]-[A-Z0-9-]+$` in any case.
fn access_token(value: Option<&str>) -> Option<String> {
  let value = pystr::strip(value?);
  let lower = value.to_ascii_lowercase();
  let rest = lower.strip_prefix("xoxe.").unwrap_or(&lower);
  let rest = rest.strip_prefix("xox")?;
  let mut chars = rest.chars();
  let ok = matches!(chars.next(), Some('a' | 'b' | 'p'))
    && chars.next() == Some('-')
    && !chars.as_str().is_empty()
    && chars.all(|c| c.is_ascii_alphanumeric() || c == '-');
  ok.then(|| value.to_string())
}

/// Upstream `CHANNEL_RE`: `^(?P<channel>[+#@]?[A-Z0-9_-]{1,32})(:(?P<thread_ts>[0-9.]+))?$`.
fn parse_channel(target: &str) -> Option<(&str, Option<&str>)> {
  let (channel, thread_ts) = match target.split_once(':') {
    Some((c, t)) => (c, Some(t)),
    None => (target, None),
  };
  let name = channel.strip_prefix(['+', '#', '@']).unwrap_or(channel);
  let name_ok = (1..=32).contains(&name.chars().count()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
  let ts_ok = thread_ts.is_none_or(|t| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || c == '.'));
  (name_ok && ts_ok).then_some((channel, thread_ts))
}

/// Python truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
  match v {
    None | Some(Value::Null) => false,
    Some(Value::Bool(b)) => *b,
    Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
    Some(Value::String(s)) => !s.is_empty(),
    Some(Value::Array(a)) => !a.is_empty(),
    Some(Value::Object(o)) => !o.is_empty(),
  }
}

/// Python `html.escape(text, quote=False)`.
fn escape_html(text: &str) -> String {
  text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub struct Slack {
  mode: Mode,
  access_token: Option<String>,
  token_a: Option<String>,
  token_b: Option<String>,
  token_c: Option<String>,
  workflow_path: Vec<String>,
  user: Option<String>,
  /// `None` is the webhook's own channel.
  channels: Vec<Option<String>>,
  include_image: bool,
  include_footer: bool,
  include_timestamp: bool,
  use_blocks: bool,
  template: Option<TemplateFile>,
  tokens: IndexMap<String, String>,
  /// Email addresses already resolved to user IDs.
  lookup_users: Mutex<HashMap<String, String>>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Slack {
  /// Upstream `parse_url()` and `__init__()`.
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The first token is the host
    let token = url.host.clone().unwrap_or_default();
    let mut entries = split_path(url);

    // ?mode= decides how the path is read
    let mode_arg = url.get("mode").filter(|m| !m.is_empty());
    let explicit = match mode_arg {
      Some(m) => match Mode::from_prefix(m) {
        Some(mode) => Some(mode),
        None => {
          tracing::warn!("The Slack mode specified ({}) is invalid.", m);
          return None;
        }
      },
      None => None,
    };

    let (mut access, mut token_a, mut token_b, mut token_c) = (None, None, None, None);
    let mut workflow_path = None;
    let mut targets: Vec<String> = Vec::new();
    if explicit.is_some_and(Mode::is_workflow) {
      // Every path segment is part of the workflow
      workflow_path = Some(std::iter::once(token).chain(entries).collect::<Vec<_>>().join("/"));
    } else if token.starts_with("xo") {
      access = Some(token);
      targets = entries;
    } else {
      token_a = Some(token);
      let mut rest = entries.drain(..);
      token_b = rest.next();
      token_c = rest.next();
      targets = rest.collect();
    }

    // ?token= holds the bot token or the webhook tokens
    if let Some(t) = url.get("token").filter(|t| !t.is_empty()) {
      let parts: Vec<String> = split_channels(t).collect();
      if parts.first().is_some_and(|p| p.starts_with("xo")) {
        access = parts.into_iter().next();
        (token_a, token_b, token_c) = (None, None, None);
      } else {
        access = None;
        let mut parts = parts.into_iter();
        (token_a, token_b, token_c) = (parts.next(), parts.next(), parts.next());
      }
    }

    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.extend(split_channels(to));
    }

    let mode = explicit.unwrap_or(if access.as_deref().is_some_and(|a| !a.is_empty()) { Mode::Bot } else { Mode::Webhook });

    let mut path = Vec::new();
    match mode {
      Mode::Workflow | Mode::Trigger => {
        // Slack workflows use four segments and triggers three
        path = workflow_path.unwrap_or_default().split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
        let expected = if mode == Mode::Workflow { 4 } else { 3 };
        if path.len() != expected {
          tracing::warn!("A Slack '{}' URL requires exactly {} path segments ({} given).", mode.as_str(), expected, path.len());
          return None;
        }
        (access, token_a, token_b, token_c) = (None, None, None, None);
      }
      Mode::Webhook | Mode::WebhookGov => {
        access = None;
        token_a = webhook_token(token_a.as_deref());
        token_b = webhook_token(token_b.as_deref());
        token_c = webhook_token(token_c.as_deref());
        for (which, value) in [("first", &token_a), ("second", &token_b), ("third", &token_c)] {
          if value.is_none() {
            tracing::warn!("An invalid Slack ({}) Token was specified.", which);
            return None;
          }
        }
      }
      Mode::Bot => {
        (token_a, token_b, token_c) = (None, None, None);
        access = access_token(access.as_deref());
        if access.is_none() {
          tracing::warn!("An invalid Slack OAuth Access Token was specified.");
          return None;
        }
      }
    }

    let template = match url.get("template").filter(|t| !t.is_empty()) {
      Some(t) => match TemplateFile::parse(t, MAX_TEMPLATE_SIZE) {
        Some(file) => Some(file),
        None => {
          tracing::warn!("The Slack template could not be loaded.");
          return None;
        }
      },
      None => None,
    };

    let mut channels: Vec<Option<String>> = parse_list(&targets, true, true).into_iter().map(Some).collect();
    if channels.is_empty() {
      // A webhook (or workflow) has its own channel; a bot does not
      channels.push((mode == Mode::Bot).then(|| DEFAULT_CHANNEL.to_string()));
    }

    // A template always means Block Kit
    let use_blocks = template.is_some() || url.get("blocks").filter(|b| !b.is_empty()).is_some_and(parse_bool);

    Some(Self {
      mode,
      access_token: access,
      token_a,
      token_b,
      token_c,
      workflow_path: path,
      user: url.user.clone(),
      channels,
      include_image: qsd_bool(url, "image", true),
      include_footer: qsd_bool(url, "footer", true),
      include_timestamp: qsd_bool(url, "timestamp", true),
      use_blocks,
      template,
      tokens: url.qsd_colon.clone(),
      lookup_users: Mutex::new(HashMap::new()),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Slack",
      service_url: Some("https://slack.com/"),
      setup_url: Some("https://appriseit.com/services/slack/"),
      protocols: vec!["slack"],
      description: "Send Slack messages via webhooks, bots or Workflow Builder.",
      attachment_support: true,
    }
  }

  fn api_url(method: &str) -> String {
    format!("{}/api/{}", service_base("https://slack.com"), method)
  }

  /// The Block Kit content `?template=` builds (upstream `gen_payload()`).
  /// `None` (already logged) when it cannot be used.
  async fn gen_payload(&self, template: &TemplateFile, ctx: &NotifyContext) -> Option<Value> {
    let text = template.read("Slack").await?;
    let image_url = if self.include_image { ctx.image_url(NotifyImageSize::Xy72) } else { None };
    let tokens = app_tokens(&self.tokens, &ctx.asset, &ctx.notify_type, &ctx.title, &ctx.body, image_url.as_deref());

    let content: Value = match serde_json::from_str(&apply_template(&text, TemplateType::Json, &tokens)) {
      Ok(v) => v,
      Err(e) => {
        tracing::error!("Slack template {} contains invalid JSON.", template.loggable());
        tracing::debug!("JSONDecodeError: {}", e);
        return None;
      }
    };
    if !content.is_object() {
      tracing::error!("Slack template {} must be a JSON object.", template.loggable());
      return None;
    }
    // Block Kit needs a non-empty list of typed blocks
    let Some(blocks) = content.get("blocks").and_then(Value::as_array).filter(|b| !b.is_empty()) else {
      tracing::error!("Slack template {} must contain a non-empty 'blocks' list.", template.loggable());
      return None;
    };
    if !blocks.iter().all(|b| b.get("type").is_some_and(Value::is_string)) {
      tracing::error!("Slack template {} contains a block missing a 'type' string.", template.loggable());
      return None;
    }
    Some(content)
  }

  /// One request to Slack (upstream `_send()`). Returns the JSON response
  /// (`{"ok": false}` when the body is not JSON) when Slack accepted it.
  async fn request(
    &self,
    client: &reqwest::Client,
    url: &str,
    payload: &Value,
    attach: Option<&Attachment>,
    get: bool,
    params: &[(&str, String)],
  ) -> Option<Value> {
    tracing::debug!("Slack Payload: {}", payload);
    let mut req = if get { client.get(url) } else { client.post(url) };
    req = req.header("User-Agent", APP_ID).header("Accept", "application/json");
    if let Some(token) = self.access_token.as_deref().filter(|_| self.mode == Mode::Bot) {
      req = req.header("Authorization", format!("Bearer {token}"));
    }
    if !params.is_empty() {
      req = req.query(params);
    }
    req = match attach {
      Some(a) => {
        let part = reqwest::multipart::Part::bytes(a.data.clone()).file_name(a.name.clone());
        req.multipart(reqwest::multipart::Form::new().part("file", part))
      }
      None => req.header("Content-Type", "application/json; charset=utf-8").body(payload.to_string()),
    };

    throttle().await;
    let what = attach.map(|a| format!(" {}", a.name)).unwrap_or_default();
    let resp = match req.send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred posting{} to Slack.", what);
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return None;
      }
    };
    let status = resp.status();
    let content = resp.bytes().await.unwrap_or_default();
    let response = serde_json::from_slice::<Value>(&content).ok().filter(Value::is_object).unwrap_or_else(|| json!({ "ok": false }));

    let okay = match self.mode {
      // File uploads answer "OK - <length>"
      Mode::Bot => truthy(response.get("ok")) || content.windows(2).any(|w| w == b"OK"),
      // Workflow webhooks confirm with the status alone
      Mode::Workflow | Mode::Trigger => status.as_u16() == 200,
      // Webhooks answer "ok"
      Mode::Webhook | Mode::WebhookGov => content.as_ref() == b"ok",
    };
    if status.as_u16() != 200 || !okay {
      let reason = if status.as_u16() == 401 { "Unauthorized - Invalid Token." } else { status.canonical_reason().unwrap_or("") };
      tracing::warn!("Failed to send{} to Slack: {}{}error={}.", what, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&content));
      return None;
    }
    Some(response)
  }

  /// The user ID an email address belongs to (upstream `lookup_userid()`);
  /// only a bot can look it up.
  async fn lookup_userid(&self, client: &reqwest::Client, email: &str) -> Option<String> {
    if let Some(id) = self.lookup_users.lock().unwrap_or_else(|e| e.into_inner()).get(email) {
      return Some(id.clone());
    }
    let Some(token) = self.access_token.as_deref().filter(|_| self.mode == Mode::Bot) else {
      tracing::warn!("Emails can not be resolved to Slack User IDs unless you have a bot configured.");
      return None;
    };

    throttle().await;
    let resp = client
      .get(Self::api_url("users.lookupByEmail"))
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/x-www-form-urlencoded")
      .header("Authorization", format!("Bearer {token}"))
      .query(&[("email", email)])
      .send()
      .await;
    let resp = match resp {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred looking up Slack User.");
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return None;
      }
    };
    let status = resp.status();
    let content = resp.bytes().await.unwrap_or_default();
    let response: Value = serde_json::from_slice(&content).unwrap_or(Value::Null);
    // A 200 can still fail, e.g. {"ok": false, "error": "missing_scope"}
    let user_id = response.pointer("/user/id").and_then(Value::as_str).filter(|_| status.as_u16() == 200 && truthy(response.get("ok")));
    let Some(user_id) = user_id else {
      let reason = if status.as_u16() == 401 { "Unauthorized - Invalid Token." } else { status.canonical_reason().unwrap_or("") };
      tracing::warn!("Failed to send Slack User Lookup:{}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&content));
      return None;
    };
    self.lookup_users.lock().unwrap_or_else(|e| e.into_inner()).insert(email.to_string(), user_id.to_string());
    tracing::info!("Email {} resolves to the Slack User ID: {}.", email, user_id);
    Some(user_id.to_string())
  }

  /// The message for webhook and bot modes, without its channel.
  async fn message_payload(&self, ctx: &NotifyContext) -> Option<Value> {
    let image_url = if self.include_image { ctx.image_url(NotifyImageSize::Xy72) } else { None };
    let color = ctx.asset.color(&ctx.notify_type);

    if self.use_blocks {
      if let Some(template) = &self.template {
        return Some(json!({ "attachments": [self.gen_payload(template, ctx).await?] }));
      }
      let slack_format = if ctx.body_format == NotifyFormat::Markdown { "mrkdwn" } else { "plain_text" };
      let mut blocks = vec![json!({ "type": "section", "text": { "type": slack_format, "text": ctx.body } })];
      // Slack only accepts non-empty headers; they are plain text
      if !ctx.title.is_empty() {
        blocks.insert(0, json!({ "type": "header", "text": { "type": "plain_text", "text": escape_html(&ctx.title), "emoji": true } }));
      }
      let mut payload = json!({});
      if self.include_footer {
        let mut elements = vec![json!({ "type": slack_format, "text": ctx.asset.app_id })];
        if let Some(image) = &image_url {
          payload["icon_url"] = json!(image);
          elements.insert(0, json!({ "type": "image", "image_url": image, "alt_text": ctx.notify_type.as_str() }));
        }
        blocks.push(json!({ "type": "context", "elements": elements }));
      }
      payload["attachments"] = json!([{ "blocks": blocks, "color": color }]);
      return Some(payload);
    }

    // The legacy attachment format; the title skips dialect_convert()
    let mut attachment = json!({ "title": escape_html(&ctx.title), "text": ctx.body, "color": color });
    let mut payload = json!({ "mrkdwn": ctx.body_format == NotifyFormat::Markdown });
    if let Some(image) = &image_url {
      payload["icon_url"] = json!(image);
    }
    if self.include_footer {
      if let Some(image) = &image_url {
        attachment["footer_icon"] = json!(image);
      }
      attachment["footer"] = json!(ctx.asset.app_id);
      if self.include_timestamp {
        attachment["ts"] = json!(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or_default());
      }
    }
    payload["attachments"] = json!([attachment]);
    Some(payload)
  }
}

/// Upstream `_commonmark_to_slack()`: CommonMark to Slack `mrkdwn`.
///
/// ```text
/// # heading           *heading* (Slack has no headings)
/// **bold**            *bold*
/// *italic*            _italic_
/// [label](<url>)      <url|label>
/// &, and <, > in code HTML entities
/// ```
fn commonmark_to_slack(body: &str) -> String {
  // Slack shows headings as bold text
  let body = commonmark_headings_to_bold(body);
  let s: Vec<char> = body.chars().collect();
  let n = s.len();
  let starts = |i: usize, pat: &str| pat.chars().enumerate().all(|(k, c)| s.get(i + k) == Some(&c));

  let mut out = String::with_capacity(body.len());
  // `*` runs, matched once the whole body is scanned
  let mut delimiters = Vec::new();
  // Where each pending link's "[" sits in `out`, innermost last
  let mut link_stack: Vec<usize> = Vec::new();
  let backtick_runs = commonmark_index_backtick_runs(&s);
  let sentinel = commonmark_pick_emphasis_sentinel(&body);
  let mut budget = commonmark_new_scan_budget(&s);

  let mut i = 0;
  while i < n {
    let ch = s[i];

    // Backslash escapes, in the form Slack's parser expects
    if ch == '\\' && i + 1 < n {
      let next = s[i + 1];
      match next {
        // < and > would open and close a Slack anchor
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        // Plain characters to Slack
        '(' | ')' | '[' | ']' | '!' | '#' => out.push(next),
        // Valid Slack escapes such as \* and \_
        _ => {
          out.push(ch);
          out.push(next);
        }
      }
      i += 2;
      continue;
    }

    if ch == '&' {
      out.push_str("&amp;");
      i += 1;
      continue;
    }

    // Code spans keep their delimiters; HTML inside is escaped
    if ch == '`' {
      let mut j = i;
      while j < n && s[j] == '`' {
        j += 1;
      }
      let run = j - i;
      if let Some(close) = commonmark_find_backtick_run(&backtick_runs, j, run) {
        let delim: String = s[i..j].iter().collect();
        let content: String = s[j..close].iter().collect();
        out.push_str(&delim);
        out.push_str(&escape_html(&content));
        out.push_str(&delim);
        i = close + run;
        continue;
      }
      // Not a code span; Slack has no escape for backticks
      out.extend(&s[i..j]);
      i = j;
      continue;
    }

    if ch == '[' {
      link_stack.push(out.len());
      out.push('[');
      i += 1;
      continue;
    }

    // [label](<url>)
    if starts(i, "](<")
      && !link_stack.is_empty()
      && let Some(close) = commonmark_scan_angle_dest(&s, i, n, Some(&mut budget))
    {
      let url = commonmark_escape_link_url(&s[i + 3..close].iter().collect::<String>());
      let open = link_stack.pop().unwrap_or_default();
      let text = out[open + 1..].to_string();
      out.truncate(open);
      out.push_str(&format!("<{url}|{text}>"));
      i = close + 2;
      continue;
    }

    // [label](url), without reading the URL as emphasis
    if starts(i, "](") && !link_stack.is_empty() {
      if let Some(close) = commonmark_scan_paren_dest(&s, i + 1, n, Some(&mut budget)) {
        let url = commonmark_escape_link_url(&s[i + 2..close].iter().collect::<String>());
        let open = link_stack.pop().unwrap_or_default();
        let text = out[open + 1..].to_string();
        out.truncate(open);
        out.push_str(&format!("<{url}|{text}>"));
        i = close + 1;
        continue;
      }
      link_stack.pop();
    }

    // A "]" that never forms "](" cannot close a link
    if ch == ']' && !link_stack.is_empty() && !starts(i, "](") {
      link_stack.pop();
    }

    // Complete autolinks; any other "<" stays literal
    if ch == '<'
      && let (Some(close), _) = commonmark_scan_autolink_dest(&s, i, n)
    {
      let url = commonmark_escape_link_url(&s[i + 1..close].iter().collect::<String>());
      out.push_str(&format!("<{url}>"));
      i = close + 1;
      continue;
    }

    if ch == '*' {
      i = commonmark_emphasis_run(&s, i, n, &mut delimiters, &mut out, &sentinel);
      continue;
    }

    out.push(ch);
    i += 1;
  }

  commonmark_render_emphasis_markers(&out, &mut delimiters, ("*", "*"), ("_", "_"), &sentinel)
}

#[async_trait]
impl Notify for Slack {
  fn schemas(&self) -> &[&str] {
    &["slack"]
  }
  fn service_name(&self) -> &str {
    "Slack"
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
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Markdown, NotifyFormat::Text]
  }
  fn body_maxlen(&self) -> usize {
    35000
  }
  fn request_rate_per_sec(&self) -> f64 {
    // 50 requests a minute (Tier 2)
    1.2
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    match body_format {
      // Slack uses &, < and > for links and mentions
      NotifyFormat::Text => escape_html(body),
      NotifyFormat::Markdown => commonmark_to_slack(body),
      NotifyFormat::Html => body.to_string(),
    }
  }
  fn has_dialect(&self) -> bool {
    true
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    if self.mode.is_workflow() {
      return Some(["slack", self.mode.as_str()].into_iter().map(str::to_string).chain(self.workflow_path.iter().cloned()).collect());
    }
    // Upstream hashes str() of each value, so a missing one is "None"
    let part = |v: &Option<String>| v.clone().unwrap_or_else(|| "None".to_string());
    Some(vec!["slack".into(), part(&self.token_a), part(&self.token_b), part(&self.token_c), part(&self.access_token)])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;

    // Workflow Builder: one fixed endpoint, no channels
    if self.mode.is_workflow() {
      let payload = match &self.template {
        Some(template) => match self.gen_payload(template, ctx).await {
          Some(p) => p,
          None => return Ok(false),
        },
        // The workflow must accept this variable
        None => json!({ "text": if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}: {}", ctx.title, ctx.body) } }),
      };
      let kind = if self.mode == Mode::Trigger { "triggers" } else { "workflows" };
      let url = format!("{}/{}/{}", service_base("https://hooks.slack.com"), kind, self.workflow_path.join("/"));
      let sent = self.request(&client, &url, &payload, None, false, &[]).await.is_some();
      if sent {
        tracing::info!("Sent Slack notification.");
      }
      return Ok(sent);
    }

    let Some(mut payload) = self.message_payload(ctx).await else { return Ok(false) };

    if !ctx.attachments.is_empty() && matches!(self.mode, Mode::Webhook | Mode::WebhookGov) {
      tracing::warn!("Slack Webhooks do not support attachments.");
    }
    if let Some(user) = &self.user {
      payload["username"] = json!(user);
    }

    let tokens = || format!("{}/{}/{}", self.token_a.as_deref().unwrap_or(""), self.token_b.as_deref().unwrap_or(""), self.token_c.as_deref().unwrap_or(""));
    let url = match self.mode {
      Mode::Webhook => format!("{}/services/{}", service_base("https://hooks.slack.com"), tokens()),
      Mode::WebhookGov => format!("{}/services/{}", service_base("https://hooks.slack-gov.com"), tokens()),
      _ => Self::api_url("chat.postMessage"),
    };

    let mut has_error = false;
    let track_attachments = !ctx.attachments.is_empty() && self.mode == Mode::Bot;
    // (message key, channel ID) of every message an attachment can follow
    let mut attach_channels: Vec<(String, String)> = Vec::new();

    for channel in &self.channels {
      let message_key = format!("message:{}", channel.as_deref().unwrap_or(""));

      if is_delivered(&message_key, false) {
        // The message is up; only its attachments may still be missing
        match recall(&message_key, false).as_ref().and_then(Value::as_str) {
          Some(id) => attach_channels.push((message_key, id.to_string())),
          None if track_attachments => has_error = true,
          None => {}
        }
        continue;
      }

      if let Some(target) = channel {
        if let Some(email) = is_email(target) {
          match self.lookup_userid(&client, &email.full_email).await {
            Some(id) => payload["channel"] = json!(id),
            None => {
              has_error = true;
              continue;
            }
          }
        } else {
          let Some((name, thread_ts)) = parse_channel(target) else {
            tracing::warn!("The specified Slack target {} is invalid;skipping.", target);
            has_error = true;
            continue;
          };
          // A thread of one channel must not carry over to the next
          match thread_ts {
            Some(ts) => payload["thread_ts"] = json!(ts),
            None => {
              if let Some(object) = payload.as_object_mut() {
                object.remove("thread_ts");
              }
            }
          }
          payload["channel"] = json!(match name.chars().next() {
            // An encoded ID
            Some('+') => name[1..].to_string(),
            Some('@') | Some('#') => name.to_string(),
            _ => format!("#{name}"),
          });
        }
      }

      let Some(response) = self.request(&client, &url, &payload, None, false, &[]).await else {
        has_error = true;
        continue;
      };
      match channel {
        Some(c) => tracing::info!("Sent Slack notification to {}.", c),
        None => tracing::info!("Sent Slack notification."),
      }

      // Keep the channel or DM ID for the attachment upload
      let channel_id = response.get("channel").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string);
      if let Some(id) = &channel_id {
        attach_channels.push((message_key.clone(), id.clone()));
        remember(&message_key, json!(id), false);
      }
      // The message is visible now, even if a later upload fails
      mark_delivered(&message_key, false);

      if track_attachments && channel_id.is_none() {
        tracing::error!("Slack did not identify the attachment channel.");
        has_error = true;
      }
    }

    if track_attachments && !attach_channels.is_empty() {
      for (no, attachment) in ctx.attachments.iter().enumerate().map(|(i, a)| (i + 1, a)) {
        let pending: Vec<&(String, String)> = attach_channels.iter().filter(|(key, _)| !is_delivered(&format!("attachment:{no}:{key}"), false)).collect();
        if pending.is_empty() {
          continue;
        }
        tracing::debug!("Posting Slack attachment {}", attachment.name);

        // https://api.slack.com/methods/files.getUploadURLExternal
        let name = if attachment.name.is_empty() { format!("file{no:03}.dat") } else { attachment.name.clone() };
        let params = [("filename", name), ("length", attachment.data.len().to_string())];
        let response = self.request(&client, &Self::api_url("files.getUploadURLExternal"), &json!({}), None, true, &params).await;
        let upload = response.as_ref().and_then(|r| Some((r.get("file_id").filter(|v| truthy(Some(v)))?.clone(), r.get("upload_url")?.as_str()?.to_string())));
        let Some((file_id, upload_url)) = upload.filter(|(_, u)| !u.is_empty()) else {
          tracing::error!("Could retrieve file upload URL.");
          return Ok(false);
        };

        if self.request(&client, &upload_url, &json!({}), Some(attachment), false, &[]).await.is_none() {
          return Ok(false);
        }

        // https://api.slack.com/methods/files.completeUploadExternal
        for (key, channel_id) in pending {
          let payload = json!({ "files": [{ "id": file_id, "title": attachment.name }], "channel_id": channel_id });
          let response = self.request(&client, &Self::api_url("files.completeUploadExternal"), &payload, None, false, &[]).await;
          if !truthy(response.as_ref().and_then(|r| r.get("files"))) {
            tracing::error!("Failed to send file to channel.");
            return Ok(false);
          }
          mark_delivered(&format!("attachment:{no}:{key}"), false);
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
  use crate::notify::with_test_base;
  use wiremock::matchers::{method, path, query_param};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  const IMAGE: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-72x72.png";
  const HOOK: &str = "/services/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ";
  const WEBHOOK: &str = "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ";

  fn slack(url: &str) -> Slack {
    Slack::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, format: NotifyFormat) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), body_format: format, ..Default::default() }
  }

  async fn mock_at(server: &MockServer, verb: &str, route: &str, status: u16, body: &str) {
    Mock::given(method(verb)).and(path(route)).respond_with(ResponseTemplate::new(status).set_body_string(body)).mount(server).await;
  }

  async fn hook(body: &str) -> MockServer {
    let server = MockServer::start().await;
    mock_at(&server, "POST", HOOK, 200, body).await;
    server
  }

  async fn requests_to(server: &MockServer, route: &str) -> Vec<Request> {
    server.received_requests().await.unwrap().into_iter().filter(|r| r.url.path() == route).collect()
  }

  fn json_of(r: &Request) -> Value {
    serde_json::from_slice(&r.body).unwrap()
  }

  /// The payload without its (time-dependent) attachment timestamp.
  fn without_ts(mut v: Value) -> Value {
    if let Some(a) = v.pointer_mut("/attachments/0").and_then(Value::as_object_mut) {
      a.remove("ts");
    }
    v
  }

  #[test]
  fn test_storage_id_matches_upstream() {
    // uid: lines of upstream's apprise --dry-run; a missing value hashes as "None"
    crate::notify::custom_json::assert_upstream_uids(&[
      ("slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#channel", "ef4fa19c"),
      ("slack://username@xoxe.xoxb-1234-1234-abc124/#nuxref", "054ce864"),
    ]);
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#hmm/#-invalid-",
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#channel",
      "slack://username@xoxe.xoxb-1234-1234-abc124/#nuxref?footer=no&timestamp=yes",
      "slack://username@xoxe.xoxp-1234-1234-abc124/#nuxref?footer=yes&timestamp=no",
      "slack://?token=xoxe.xoxb-1234-1234-abc124&to=#nuxref&footer=no&user=test",
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/+id/@id/",
      "slack://username@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?to=#nuxref",
      "slack://username@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#nuxref",
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnl/user@gmail.com",
      "slack://bot@_/#nuxref?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnadfdajkjkfl/",
      "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan",
      "slack://username@xoxb-1234-1234-abc124/#nuxref?footer=no&timestamp=yes",
      "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&mode=hook",
      "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&footer=yes&timestamp=no",
      "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&footer=no&timestamp=no",
      "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&footer=yes&image=no",
      "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=yes&format=text",
      "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&blocks=no&format=text",
      "slack://?token=xoxb-1234-1234-abc124&to=#nuxref&footer=no&user=test",
      "slack://?token=xoxb-1234-1234-abc124&to=#nuxref,#$,#-&footer=no",
      "slack://username@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ",
      "https://hooks.slack.com/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc",
      "https://hooks.slack-gov.com/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc",
      "https://hooks.slack.com/services/AAAAAAAAA/BBBBBBBBB/cccccccccccccccccccccccc?format=text",
      "slack://notify@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#b:100",
      "slack://notify@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/+124:100/@chan",
      "slack://notify@T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#b:bad",
      "slack://T1JJ3T3L2/Ft07XXXX/XXXXXXXX/YYYYYYYY/?mode=workflow",
      "slack://T1JJ3T3L2/XXXXXXXX/YYYYYYYY/?mode=trigger",
      "https://hooks.slack.com/workflows/T1JJ3T3L2/Ft07XXXX/XXXXXXXX/YYYYYYYY",
      "https://hooks.slack.com/triggers/T1JJ3T3L2/XXXXXXXX/YYYYYYYY",
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?mode=h",
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?mode=gov",
    ];
    for url in urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "slack://",
      "slack://:@/",
      "slack://T1JJ3T3L2",
      "slack://T1JJ3T3L2/A1BRTD4JD/",
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?mode=invalid",
      // Upstream modes are matched by prefix, case and all
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?mode=webhook",
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?mode=Hook",
      "slack://?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/&to=#chan&mode=bot",
      "slack://xoxb-1234-1234-abc124/#chan?mode=hook",
      "slack://xoxq-1234-1234-abc124/#chan",
      "slack://username@-INVALID-/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#cool",
      "slack://username@T1JJ3T3L2/-INVALID-/TIiajkdnlazkcOXrIdevi7FQ/#great",
      "slack://username@T1JJ3T3L2/A1BRTD4JD/-INVALID-/#channel",
      "slack://T1JJ3T3L2/XXXXXXXX/?mode=workflow",
      "slack://T1JJ3T3L2/XXXXXXXX/YYYYYYYY/?mode=workflow",
      "slack://T1JJ3T3L2/Ft07XXXX/XXXXXXXX/YYYYYYYY/?mode=trigger",
      "slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/?template=invalid://x.json",
      "https://hooks.slack.com/workflows/T1JJ3T3L2/XXXXXXXX/YYYYYYYY",
    ];
    for url in urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_options() {
    let s = slack("slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#chan/+id:1.5/@user/#chan");
    assert_eq!(s.mode, Mode::Webhook);
    // parse_list() sorts and de-duplicates the targets
    assert_eq!(s.channels, vec![Some("#chan".to_string()), Some("+id:1.5".to_string()), Some("@user".to_string())]);
    assert!(s.include_image && s.include_footer && s.include_timestamp && !s.use_blocks);
    assert_eq!(slack(WEBHOOK).channels, vec![None]);

    let s = slack("slack://?token=xoxe.xoxb-1234-1234-abc124&to=#nuxref&footer=no&user=test&image=no&timestamp=no&blocks=yes");
    assert_eq!(s.mode, Mode::Bot);
    assert_eq!(s.access_token.as_deref(), Some("xoxe.xoxb-1234-1234-abc124"));
    assert_eq!(s.user.as_deref(), Some("test"));
    assert_eq!(s.channels, vec![Some("nuxref".to_string())]);
    assert!(!s.include_image && !s.include_footer && !s.include_timestamp && s.use_blocks);
    // A bot without targets posts to #general
    assert_eq!(slack("slack://xoxb-1234-1234-abc124").channels, vec![Some("#general".to_string())]);

    // ?token= replaces the host token; the path before it was token_b
    let s = slack("slack://bot@_/#nuxref?token=T1JJ3T3L2/A1BRTD4JD/TIiajkdnadfdajkjkfl/");
    assert_eq!((s.token_a.as_deref(), s.token_c.as_deref()), (Some("T1JJ3T3L2"), Some("TIiajkdnadfdajkjkfl")));
    assert_eq!(s.channels, vec![None]);

    assert_eq!(slack(&format!("{WEBHOOK}?mode=gov")).mode, Mode::WebhookGov);
    let s = slack("slack://T1/F2/X3/Y4?mode=work");
    assert_eq!((s.mode, s.workflow_path.clone()), (Mode::Workflow, vec!["T1".to_string(), "F2".into(), "X3".into(), "Y4".into()]));
    assert_eq!(s.channels, vec![None]);
    assert_eq!(s.url_identifier(), Some(vec!["slack".into(), "workflow".into(), "T1".into(), "F2".into(), "X3".into(), "Y4".into()]));
    assert_eq!(slack("slack://T1/X3/Y4?mode=tri").mode, Mode::Trigger);
    assert_eq!(
      slack(WEBHOOK).url_identifier(),
      Some(vec!["slack".into(), "T1JJ3T3L2".into(), "A1BRTD4JD".into(), "TIiajkdnlazkcOXrIdevi7FQ".into(), "None".into()])
    );

    // The native gov URL keeps its own arguments (upstream drops them)
    let parsed =
      crate::notify::registry::parse_service_url("https://hooks.slack-gov.com/services/T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ?footer=no").unwrap();
    let s = Slack::from_url(&parsed).unwrap();
    assert_eq!((s.mode, s.include_footer, s.channels.clone()), (Mode::WebhookGov, false, vec![None]));
  }

  #[test]
  fn test_dialect() {
    let s = slack(WEBHOOK);
    assert!(s.has_dialect());
    assert_eq!(s.dialect_convert("a <b> & c", &NotifyFormat::Text), "a &lt;b&gt; &amp; c");
    assert_eq!(s.dialect_convert("<b>x</b>", &NotifyFormat::Html), "<b>x</b>");
    let cases = [
      (
        "# Heading\n**Bold** [link](https://example.com?a=1&b=2) & `code <b>`",
        "*Heading*\n*Bold* <https://example.com?a=1&amp;b=2|link> &amp; `code &lt;b&gt;`",
      ),
      ("***both*** and *one* __x__", "_*both*_ and _one_ __x__"),
      ("[a [b](u) c](v)", "<v|a <u|b> c>"),
      ("[x](<a b>)", "<a b|x>"),
      ("see <https://a.b/c?d=1&e=2>", "see <https://a.b/c?d=1&amp;e=2>"),
      ("\\< \\> \\( \\* \\_", "&lt; &gt; ( \\* \\_"),
      ("**hello** &amp; <world>", "*hello* &amp;amp; <world>"),
      ("``unclosed `", "``unclosed `"),
      ("] stray [x](y)", "] stray <y|x>"),
    ];
    for (input, expected) in cases {
      assert_eq!(s.dialect_convert(input, &NotifyFormat::Markdown), expected, "{input:?}");
    }
  }

  #[test]
  fn test_parse_channel() {
    assert_eq!(parse_channel("#b:100"), Some(("#b", Some("100"))));
    assert_eq!(parse_channel("+124:1.5"), Some(("+124", Some("1.5"))));
    assert_eq!(parse_channel("chan"), Some(("chan", None)));
    for bad in ["#b:bad", "$", "+", "++a", "a:", &"x".repeat(33), "a b"] {
      assert_eq!(parse_channel(bad), None, "{bad}");
    }
  }

  /// Legacy attachments over a webhook, one request per channel (pycapture).
  #[tokio::test]
  async fn test_webhook_legacy_markdown() {
    let server = hook("ok").await;
    let s = slack(&format!("{WEBHOOK}/#chan/+id:1.5/@user?footer=yes"));
    let body = s.dialect_convert("# Head\n**bold** *it* [l](https://a.b?x=1&y=2) & `c <b>`", &NotifyFormat::Markdown);
    assert!(with_test_base(server.uri(), s.send(&ctx("T <&>", &body, NotifyFormat::Markdown))).await.unwrap());

    let requests = requests_to(&server, HOOK).await;
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].headers.get("Content-Type").unwrap(), "application/json; charset=utf-8");
    assert_eq!(requests[0].headers.get("Accept").unwrap(), "application/json");
    assert!(requests[0].headers.get("Authorization").is_none());
    let expected = |channel: Value| {
      let mut v = json!({
        "mrkdwn": true,
        "attachments": [{
          "title": "T &lt;&amp;&gt;",
          "text": "*Head*\n*bold* _it_ <https://a.b?x=1&amp;y=2|l> &amp; `c &lt;b&gt;`",
          "color": "#3AA3E3",
          "footer_icon": IMAGE,
          "footer": "Apprise",
        }],
        "icon_url": IMAGE,
      });
      v.as_object_mut().unwrap().extend(channel.as_object().unwrap().clone());
      v
    };
    assert!(json_of(&requests[0])["attachments"][0]["ts"].is_f64());
    assert_eq!(without_ts(json_of(&requests[0])), expected(json!({"channel": "#chan"})));
    assert_eq!(without_ts(json_of(&requests[1])), expected(json!({"channel": "id", "thread_ts": "1.5"})));
    assert_eq!(without_ts(json_of(&requests[2])), expected(json!({"channel": "@user"})));
  }

  #[tokio::test]
  async fn test_webhook_legacy_text() {
    let server = hook("ok").await;
    let s = slack(&format!("slack://bob@{}?image=no&timestamp=no", &WEBHOOK[8..]));
    let body = s.dialect_convert("a <b> & c", &NotifyFormat::Text);
    assert!(with_test_base(server.uri(), s.send(&ctx("T", &body, NotifyFormat::Text))).await.unwrap());
    assert_eq!(
      json_of(&requests_to(&server, HOOK).await[0]),
      json!({"mrkdwn": false, "attachments": [{"title": "T", "text": "a &lt;b&gt; &amp; c", "color": "#3AA3E3", "footer": "Apprise"}], "username": "bob"})
    );
  }

  /// Block Kit, with and without a title and footer (pycapture).
  #[tokio::test]
  async fn test_webhook_blocks() {
    let server = hook("ok").await;
    let s = slack(&format!("{WEBHOOK}?blocks=yes"));
    assert!(with_test_base(server.uri(), s.send(&ctx("T <&>", "*bold*", NotifyFormat::Markdown))).await.unwrap());
    assert_eq!(
      json_of(&requests_to(&server, HOOK).await[0]),
      json!({
        "attachments": [{
          "blocks": [
            {"type": "header", "text": {"type": "plain_text", "text": "T &lt;&amp;&gt;", "emoji": true}},
            {"type": "section", "text": {"type": "mrkdwn", "text": "*bold*"}},
            {"type": "context", "elements": [{"type": "image", "image_url": IMAGE, "alt_text": "info"}, {"type": "mrkdwn", "text": "Apprise"}]},
          ],
          "color": "#3AA3E3",
        }],
        "icon_url": IMAGE,
      })
    );

    let server = hook("ok").await;
    let s = slack(&format!("{WEBHOOK}?blocks=yes&footer=no&format=text"));
    assert!(with_test_base(server.uri(), s.send(&ctx("", "a&lt;b", NotifyFormat::Text))).await.unwrap());
    assert_eq!(
      json_of(&requests_to(&server, HOOK).await[0]),
      json!({"attachments": [{"blocks": [{"type": "section", "text": {"type": "plain_text", "text": "a&lt;b"}}], "color": "#3AA3E3"}]})
    );
  }

  /// A webhook keeps its own identity unless a bot name is given.
  #[tokio::test]
  async fn test_username_by_mode() {
    for (url, username) in [(WEBHOOK.to_string(), None), (format!("slack://Custom@{}", &WEBHOOK[8..]), Some("Custom")), (format!("{WEBHOOK}?blocks=yes"), None)]
    {
      let server = hook("ok").await;
      assert!(with_test_base(server.uri(), slack(&url).send(&ctx("title", "body", NotifyFormat::Markdown))).await.unwrap());
      assert_eq!(json_of(&requests_to(&server, HOOK).await[0]).get("username").and_then(Value::as_str), username, "{url}");
    }

    let server = MockServer::start().await;
    mock_at(&server, "POST", HOOK, 200, "ok").await;
    let s = slack(&format!("{WEBHOOK}?mode=gov-hook"));
    assert!(with_test_base(server.uri(), s.send(&ctx("t", "b", NotifyFormat::Text))).await.unwrap());
    assert!(json_of(&requests_to(&server, HOOK).await[0]).get("username").is_none());
  }

  /// Webhooks only accept "ok"; bad channels and errors fail the send.
  #[tokio::test]
  async fn test_webhook_failures() {
    for (status, body) in [(200, "fail"), (500, "ok"), (999, "ok")] {
      let server = MockServer::start().await;
      mock_at(&server, "POST", HOOK, status, body).await;
      assert!(!with_test_base(server.uri(), slack(&format!("{WEBHOOK}/#a")).send(&ctx("t", "b", NotifyFormat::Text))).await.unwrap(), "{status} {body}");
    }

    // An invalid channel is skipped, the others still go out
    let server = hook("ok").await;
    let s = slack(&format!("{WEBHOOK}/#hmm/#b:bad"));
    assert!(!with_test_base(server.uri(), s.send(&ctx("t", "b", NotifyFormat::Text))).await.unwrap());
    assert_eq!(requests_to(&server, HOOK).await.len(), 1);

    // Emails need a bot to look them up
    let server = hook("ok").await;
    assert!(!with_test_base(server.uri(), slack(&format!("{WEBHOOK}/user@gmail.com")).send(&ctx("t", "b", NotifyFormat::Text))).await.unwrap());
    assert!(requests_to(&server, HOOK).await.is_empty());

    let s = slack(WEBHOOK);
    assert!(!with_test_base("http://127.0.0.1:1", s.send(&ctx("t", "b", NotifyFormat::Text))).await.unwrap());
  }

  /// Workflow Builder and trigger webhooks (pycapture).
  #[tokio::test]
  async fn test_workflows() {
    let server = MockServer::start().await;
    mock_at(&server, "POST", "/workflows/T111/F222/X333/Y444", 200, "").await;
    mock_at(&server, "POST", "/triggers/T111/X333/Y444", 200, "x").await;

    let s = slack("slack://bob@T111/F222/X333/Y444?mode=workflow");
    let body = s.dialect_convert("body *b*", &NotifyFormat::Markdown);
    assert!(with_test_base(server.uri(), s.send(&ctx("the title", &body, NotifyFormat::Markdown))).await.unwrap());
    let request = &requests_to(&server, "/workflows/T111/F222/X333/Y444").await[0];
    assert_eq!(json_of(request), json!({"text": "the title: body _b_"}));
    assert_eq!(request.headers.get("Content-Type").unwrap(), "application/json; charset=utf-8");

    let s = Slack::from_url(&crate::notify::registry::parse_service_url("https://hooks.slack.com/triggers/T111/X333/Y444").unwrap()).unwrap();
    assert!(with_test_base(server.uri(), s.send(&ctx("", "body", NotifyFormat::Text))).await.unwrap());
    assert_eq!(json_of(&requests_to(&server, "/triggers/T111/X333/Y444").await[0]), json!({"text": "body"}));

    // Only the status counts
    let server = MockServer::start().await;
    mock_at(&server, "POST", "/workflows/T111/F222/X333/Y444", 500, "").await;
    assert!(!with_test_base(server.uri(), slack("slack://T111/F222/X333/Y444?mode=workflow").send(&ctx("", "b", NotifyFormat::Text))).await.unwrap());
  }

  /// A template is the workflow payload, or the Block Kit attachment of a
  /// message (pycapture).
  #[tokio::test]
  async fn test_templates() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("sl.json");
    std::fs::write(
      &file,
      r#"{"blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "{{app_title}}: {{app_body}} {{tok}} {{app_image_url}} {{app_color_hex}}"}}], "color": "{{app_color}}"}"#,
    )
    .unwrap();
    let server = MockServer::start().await;
    mock_at(&server, "POST", "/workflows/T111/F222/X333/Y444", 200, "").await;
    mock_at(&server, "POST", HOOK, 200, "ok").await;

    let s = slack(&format!("slack://T111/F222/X333/Y444?mode=work&template={}&:tok=hi&image=no", file.display()));
    assert!(with_test_base(server.uri(), s.send(&ctx("the \"t\"", "b", NotifyFormat::Text))).await.unwrap());
    assert_eq!(
      json_of(&requests_to(&server, "/workflows/T111/F222/X333/Y444").await[0]),
      json!({"blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "the \"t\": b hi  #3AA3E3"}}], "color": "#3AA3E3"})
    );

    // A template implies blocks
    let s = slack(&format!("slack://bob@{}/#c?template={}&:tok=hi", &WEBHOOK[8..], file.display()));
    assert!(s.use_blocks);
    assert!(with_test_base(server.uri(), s.send(&ctx("the \"t\"", "b", NotifyFormat::Text))).await.unwrap());
    assert_eq!(
      json_of(&requests_to(&server, HOOK).await[0]),
      json!({
        "attachments": [{"blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": format!("the \"t\": b hi {IMAGE} #3AA3E3")}}], "color": "#3AA3E3"}],
        "username": "bob",
        "channel": "#c",
      })
    );

    let bad = [
      ("invalid.json", "{"),
      ("list.json", "[]"),
      ("no-blocks.json", r#"{"color": "x"}"#),
      ("empty.json", r#"{"blocks": []}"#),
      ("not-list.json", r#"{"blocks": {"type": "section"}}"#),
      ("no-type.json", r#"{"blocks": [{"text": "x"}]}"#),
      ("not-dict.json", r#"{"blocks": ["section"]}"#),
    ];
    for (name, content) in bad {
      let file = dir.path().join(name);
      std::fs::write(&file, content).unwrap();
      for url in [format!("{WEBHOOK}?template={}", file.display()), format!("slack://T111/F222/X333/Y444?mode=workflow&template={}", file.display())] {
        assert!(!with_test_base(server.uri(), slack(&url).send(&ctx("t", "b", NotifyFormat::Text))).await.unwrap(), "{name}");
      }
    }
    let missing = format!("{WEBHOOK}?template={}", dir.path().join("missing.json").display());
    assert!(!with_test_base(server.uri(), slack(&missing).send(&ctx("t", "b", NotifyFormat::Text))).await.unwrap());
    assert_eq!(requests_to(&server, HOOK).await.len(), 1);
  }

  /// A bot posts to each channel, resolves emails, then uploads each file
  /// once and shares it with every channel (pycapture).
  #[tokio::test]
  async fn test_bot_attachments() {
    let server = MockServer::start().await;
    mock_at(&server, "GET", "/api/users.lookupByEmail", 200, r#"{"ok": true, "user": {"id": "U123"}}"#).await;
    Mock::given(method("POST"))
      .and(path("/api/chat.postMessage"))
      .and(wiremock::matchers::body_partial_json(json!({"channel": "#general"})))
      .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok": true, "channel": "C222"}"#))
      .mount(&server)
      .await;
    mock_at(&server, "POST", "/api/chat.postMessage", 200, r#"{"ok": true, "channel": "D111"}"#).await;
    for (name, id) in [("a.txt", "F1"), ("b.png", "F2")] {
      Mock::given(method("GET"))
        .and(path("/api/files.getUploadURLExternal"))
        .and(query_param("filename", name))
        .respond_with(
          ResponseTemplate::new(200).set_body_string(json!({"ok": true, "file_id": id, "upload_url": format!("{}/upload/{id}", server.uri())}).to_string()),
        )
        .mount(&server)
        .await;
    }
    mock_at(&server, "POST", "/upload/F1", 200, "OK - 5").await;
    mock_at(&server, "POST", "/upload/F2", 200, "OK - 7").await;
    mock_at(&server, "POST", "/api/files.completeUploadExternal", 200, r#"{"ok": true, "files": [{"id": "F"}]}"#).await;

    let s = slack("slack://xoxb-1234-1234-abc124/user@example.com/#general?blocks=no&footer=no");
    let mut c = ctx("T", "B", NotifyFormat::Text);
    c.attachments = vec![
      Attachment { name: "a.txt".into(), data: b"hello".to_vec(), mime_type: "text/plain".into() },
      Attachment { name: "b.png".into(), data: b"PNGDATA".to_vec(), mime_type: "image/png".into() },
    ];
    assert!(with_test_base(server.uri(), s.send(&c)).await.unwrap());

    let all = server.received_requests().await.unwrap();
    let order: Vec<String> = all.iter().map(|r| format!("{} {}", r.method, r.url.path())).collect();
    assert_eq!(
      order,
      [
        "POST /api/chat.postMessage",
        "GET /api/users.lookupByEmail",
        "POST /api/chat.postMessage",
        "GET /api/files.getUploadURLExternal",
        "POST /upload/F1",
        "POST /api/files.completeUploadExternal",
        "POST /api/files.completeUploadExternal",
        "GET /api/files.getUploadURLExternal",
        "POST /upload/F2",
        "POST /api/files.completeUploadExternal",
        "POST /api/files.completeUploadExternal",
      ]
    );
    for r in &all {
      assert_eq!(r.headers.get("Authorization").unwrap(), "Bearer xoxb-1234-1234-abc124");
    }
    let message =
      |channel: &str| json!({"mrkdwn": false, "attachments": [{"title": "T", "text": "B", "color": "#3AA3E3"}], "icon_url": IMAGE, "channel": channel});
    assert_eq!(json_of(&all[0]), message("#general"));
    assert_eq!(all[1].url.query(), Some("email=user%40example.com"));
    assert_eq!(all[1].headers.get("Content-Type").unwrap(), "application/x-www-form-urlencoded");
    assert_eq!(json_of(&all[2]), message("U123"));
    assert_eq!(all[3].url.query(), Some("filename=a.txt&length=5"));
    assert_eq!(json_of(&all[3]), json!({}));
    let upload = String::from_utf8_lossy(&all[4].body).to_string();
    assert!(upload.contains("name=\"file\"; filename=\"a.txt\"\r\n\r\nhello"), "{upload}");
    assert!(all[4].headers.get("Content-Type").unwrap().to_str().unwrap().starts_with("multipart/form-data"));
    assert_eq!(json_of(&all[5]), json!({"files": [{"id": "F1", "title": "a.txt"}], "channel_id": "C222"}));
    assert_eq!(json_of(&all[6]), json!({"files": [{"id": "F1", "title": "a.txt"}], "channel_id": "D111"}));
    assert_eq!(json_of(&all[10]), json!({"files": [{"id": "F2", "title": "b.png"}], "channel_id": "D111"}));
  }

  #[tokio::test]
  async fn test_bot_failures() {
    let attach = || vec![Attachment { name: "a.gif".into(), data: b"GIF".to_vec(), mime_type: "image/gif".into() }];
    let run = |responses: Vec<(&'static str, &'static str, u16, String)>, with_attach: bool| async move {
      let server = MockServer::start().await;
      for (verb, route, status, body) in responses {
        let body = body.replace("{uri}", &server.uri());
        mock_at(&server, verb, route, status, &body).await;
      }
      let mut c = ctx("t", "b", NotifyFormat::Text);
      if with_attach {
        c.attachments = attach();
      }
      with_test_base(server.uri(), slack("slack://xoxb-1234-1234-abc124/#general").send(&c)).await.unwrap()
    };
    let post = |body: &str| ("POST", "/api/chat.postMessage", 200, body.to_string());
    let upload_url = ("GET", "/api/files.getUploadURLExternal", 200, r#"{"ok": true, "file_id": "F1", "upload_url": "{uri}/upload"}"#.to_string());

    // A bot needs "ok": true
    assert!(run(vec![post(r#"{"ok": true, "channel": "C1"}"#)], false).await);
    assert!(!run(vec![post(r#"{"ok": false, "error": "not_in_channel"}"#)], false).await);
    assert!(!run(vec![("POST", "/api/chat.postMessage", 401, r#"{"ok": true}"#.into())], false).await);
    // Without a channel ID attachments have nowhere to go
    assert!(!run(vec![post(r#"{"ok": true}"#)], true).await);
    // Each step of the upload must succeed
    assert!(!run(vec![post(r#"{"ok": true, "channel": "C1"}"#), ("GET", "/api/files.getUploadURLExternal", 200, r#"{"ok": true}"#.into())], true).await);
    assert!(!run(vec![post(r#"{"ok": true, "channel": "C1"}"#), upload_url.clone(), ("POST", "/upload", 500, "".into())], true).await);
    assert!(
      !run(
        vec![
          post(r#"{"ok": true, "channel": "C1"}"#),
          upload_url.clone(),
          ("POST", "/upload", 200, "OK".into()),
          ("POST", "/api/files.completeUploadExternal", 200, r#"{"ok": true, "files": []}"#.into()),
        ],
        true
      )
      .await
    );
    assert!(
      run(
        vec![
          post(r#"{"ok": true, "channel": "C1"}"#),
          upload_url,
          ("POST", "/upload", 200, "OK".into()),
          ("POST", "/api/files.completeUploadExternal", 200, r#"{"ok": true, "files": [{"id": "F1"}]}"#.into()),
        ],
        true
      )
      .await
    );

    // A failed email lookup skips that target only
    let server = MockServer::start().await;
    mock_at(&server, "GET", "/api/users.lookupByEmail", 200, r#"{"ok": false, "error": "missing_scope"}"#).await;
    mock_at(&server, "POST", "/api/chat.postMessage", 200, r#"{"ok": true, "channel": "C1"}"#).await;
    let s = slack("slack://xoxb-1234-1234-abc124/#general/user@example.com");
    assert!(!with_test_base(server.uri(), s.send(&ctx("t", "b", NotifyFormat::Text))).await.unwrap());
    assert_eq!(requests_to(&server, "/api/chat.postMessage").await.len(), 1);
  }

  /// A resolved email is looked up once.
  #[tokio::test]
  async fn test_email_lookup_cached() {
    let server = MockServer::start().await;
    mock_at(&server, "GET", "/api/users.lookupByEmail", 200, r#"{"ok": true, "user": {"id": "U9"}}"#).await;
    mock_at(&server, "POST", "/api/chat.postMessage", 200, r#"{"ok": true, "channel": "D9"}"#).await;
    let s = slack("slack://xoxb-1234-1234-abc124/user@example.com");
    for _ in 0..2 {
      assert!(with_test_base(server.uri(), s.send(&ctx("t", "b", NotifyFormat::Text))).await.unwrap());
    }
    assert_eq!(requests_to(&server, "/api/users.lookupByEmail").await.len(), 1);
    assert!(requests_to(&server, "/api/chat.postMessage").await.iter().all(|r| json_of(r)["channel"] == "U9"));
  }

  /// A retry shares the file without posting the message again.
  #[tokio::test]
  async fn test_retry_finishes_attachment() {
    let server = MockServer::start().await;
    mock_at(&server, "POST", "/api/chat.postMessage", 200, r#"{"ok": true, "channel": "C123456"}"#).await;
    let upload = json!({"ok": true, "file_id": "F123ABC456", "upload_url": format!("{}/upload", server.uri())}).to_string();
    mock_at(&server, "GET", "/api/files.getUploadURLExternal", 200, &upload).await;
    mock_at(&server, "POST", "/upload", 200, "OK").await;
    Mock::given(method("POST"))
      .and(path("/api/files.completeUploadExternal"))
      .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok": false}"#))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    mock_at(&server, "POST", "/api/files.completeUploadExternal", 200, r#"{"ok": true, "files": [{"id": "F123ABC456"}]}"#).await;

    let mut a = crate::Apprise::new();
    assert!(a.add("slack://xoxb-1234-1234-abc124/#general?retry=1&wait=0"));
    let mut c = ctx("", "body", NotifyFormat::Text);
    c.attachments = vec![Attachment { name: "a.gif".into(), data: b"GIF".to_vec(), mime_type: "image/gif".into() }];
    assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
    assert_eq!(requests_to(&server, "/api/chat.postMessage").await.len(), 1);
    assert_eq!(requests_to(&server, "/api/files.completeUploadExternal").await.len(), 2);
  }

  /// Through Apprise: the format follows the input, HTML becomes mrkdwn.
  #[tokio::test]
  async fn test_notify_formats() {
    async fn text_of(url: &str, body: &str, format: NotifyFormat) -> Value {
      let server = hook("ok").await;
      let mut a = crate::Apprise::new();
      assert!(a.add(url));
      assert!(with_test_base(server.uri(), a.notify(&ctx("", body, format))).await.is_success(), "{url}");
      json_of(&requests_to(&server, HOOK).await[0])
    }
    let url = format!("{WEBHOOK}/#general");
    assert_eq!(text_of(&url, "<b>hello</b> <i>world</i>", NotifyFormat::Html).await["attachments"][0]["text"], "*hello* _world_");
    assert_eq!(
      text_of(&url, "<a href=\"https://example.com/x\">click here</a>", NotifyFormat::Html).await["attachments"][0]["text"],
      "<https://example.com/x|click here>"
    );
    assert_eq!(text_of(&url, "<p>2 &lt; 3 &amp; 4</p>", NotifyFormat::Html).await["attachments"][0]["text"], "2 &lt; 3 &amp; 4");

    let payload = text_of(&url, "*literal*", NotifyFormat::Text).await;
    assert_eq!((payload["mrkdwn"].clone(), payload["attachments"][0]["text"].clone()), (json!(false), json!("*literal*")));

    let blocks = format!("{url}?blocks=yes");
    let block_text = |p: Value| p["attachments"][0]["blocks"][0]["text"].clone();
    assert_eq!(
      block_text(text_of(&blocks, "**hello** &amp; <world>", NotifyFormat::Markdown).await),
      json!({"type": "mrkdwn", "text": "*hello* &amp;amp; <world>"})
    );
    assert_eq!(
      block_text(text_of(&blocks, "**hello** &amp; <world>", NotifyFormat::Text).await),
      json!({"type": "plain_text", "text": "**hello** &amp;amp; &lt;world&gt;"})
    );
  }
}
