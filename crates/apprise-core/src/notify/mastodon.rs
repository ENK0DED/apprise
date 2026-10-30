use crate::conversion::commonmark::commonmark_decode_backslash_escapes;
use crate::conversion::pystr::{is_decimal, is_word};
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, throttle_for};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_list, qsd_bool, split_path};
use crate::utils::qsd;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Upstream `MASTODON_MESSAGE_VISIBILITIES`, matched by prefix in order.
const VISIBILITIES: &[&str] = &["default", "direct", "private", "unlisted", "public"];

/// Upstream `mastodon_body_maxlen`.
const MASTODON_BODY_MAXLEN: usize = 500;

/// Images per toot (one when a GIF or other media breaks the batch).
const TOOT_IMAGES_BATCH: usize = 4;

fn is_user_char(c: char) -> bool {
  c.is_ascii_alphanumeric() || c == '_'
}

fn is_host_char(c: char) -> bool {
  c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Upstream `IS_USER`: `[@]user[@host]`, returning `user[@host]`.
fn match_user(target: &str) -> Option<&str> {
  let s = target.trim_start();
  let s = s.strip_prefix('@').unwrap_or(s);
  let s = s.strip_suffix('\n').unwrap_or(s);
  let (user, host) = match s.split_once('@') {
    Some((user, host)) => (user, Some(host)),
    None => (s, None),
  };
  let ok = !user.is_empty() && user.chars().all(is_user_char) && host.is_none_or(|h| !h.is_empty() && h.chars().all(is_host_char));
  ok.then_some(s)
}

/// Upstream `MENTION_DETECTION_RE.findall()`: `@user[@host]` not preceded
/// by a word character or `@`, and followed by the end, whitespace or one
/// of `,.&()[]<>`.
fn find_mentions(text: &str) -> Vec<String> {
  let chars: Vec<char> = text.chars().collect();
  let ends_token = |pos: usize| pos == chars.len() || chars[pos].is_whitespace() || ",.&()[]<>".contains(chars[pos]);
  let mut found = Vec::new();
  let mut i = 0;
  while i < chars.len() {
    if chars[i] != '@' || (i > 0 && (is_word(chars[i - 1]) || chars[i - 1] == '@')) {
      i += 1;
      continue;
    }
    let user_end = (i + 1..chars.len()).find(|&j| !is_user_char(chars[j])).unwrap_or(chars.len());
    if user_end == i + 1 {
      i += 1;
      continue;
    }
    // The host is greedy but gives characters back until the token ends
    let mut end = None;
    if user_end < chars.len() && chars[user_end] == '@' {
      let host_end = (user_end + 1..chars.len()).find(|&j| !is_host_char(chars[j])).unwrap_or(chars.len());
      end = (user_end + 2..=host_end).rev().find(|&e| ends_token(e));
    }
    if end.is_none() && ends_token(user_end) {
      end = Some(user_end);
    }
    match end {
      Some(end) => {
        found.push(chars[i..end].iter().collect());
        i = end;
      }
      None => i += 1,
    }
  }
  found
}

/// Upstream `HASHTAG_DETECTION_RE.findall()`: `#tag` not preceded by `#`,
/// `%` or a word character and not followed by `#` or `%`.
fn find_hashtags(text: &str) -> Vec<String> {
  let chars: Vec<char> = text.chars().collect();
  let mut found = Vec::new();
  let mut i = 0;
  while i < chars.len() {
    let starts = chars[i] == '#' && (i == 0 || !(is_word(chars[i - 1]) || matches!(chars[i - 1], '#' | '%')));
    if !starts || i + 1 >= chars.len() || !is_word(chars[i + 1]) || chars[i + 1] == '_' {
      i += 1;
      continue;
    }
    let end = (i + 1..chars.len()).find(|&j| !is_word(chars[j])).unwrap_or(chars.len());
    if end < chars.len() && matches!(chars[end], '#' | '%') {
      i += 1;
      continue;
    }
    found.push(chars[i..end].iter().collect());
    i = end;
  }
  found
}

/// Upstream `valid_hashtag()`: word characters not starting with `_`, and
/// not only digits.
fn valid_hashtag(token: &str) -> bool {
  let value = token.strip_prefix('#').unwrap_or(token);
  let mut chars = value.chars();
  let Some(first) = chars.next() else { return false };
  if !is_word(first) || first == '_' || !chars.all(is_word) {
    return false;
  }
  let digits: Vec<char> = value.chars().filter(|c| *c != '_').collect();
  !(!digits.is_empty() && digits.iter().all(|c| is_decimal(*c)))
}

/// Upstream `normalize_ping_token()`: `@user[@host]` or a valid `#tag`.
fn normalize_ping_token(token: &str) -> Option<String> {
  let token = token.trim();
  if token.is_empty() {
    return None;
  }
  if token.starts_with('@')
    && let Some(user) = match_user(token)
  {
    return Some(format!("@{}", user));
  }
  (token.starts_with('#') && valid_hashtag(token)).then(|| token.to_string())
}

/// Upstream `ping_tokens(..., normalize=True)`: the configured tokens,
/// sorted, normalized and without case-insensitive duplicates.
fn normalized_tokens(entries: &[String], seen: &mut HashSet<String>) -> Vec<String> {
  let mut out = Vec::new();
  for token in parse_list(entries, true, true) {
    if let Some(normalized) = normalize_ping_token(&token)
      && seen.insert(normalized.to_lowercase())
    {
      out.push(normalized);
    }
  }
  out
}

/// Upstream `ping_tokens(text)`: the mentions, then the valid hashtags,
/// found in `text` and not seen before.
fn detected_tokens(text: &str, seen: &mut HashSet<String>) -> Vec<String> {
  let mentions = find_mentions(text);
  let hashtags = find_hashtags(text).into_iter().filter(|t| valid_hashtag(t));
  mentions.into_iter().chain(hashtags).filter(|t| seen.insert(t.to_lowercase())).collect()
}

/// Upstream `ping_payload()`: the status suffix for `tokens`.
fn ping_payload(tokens: &[String]) -> String {
  if tokens.is_empty() { String::new() } else { format!(" {}", tokens.join(" ")) }
}

/// Upstream `LEADING_HEADING_RE` (`\A {0,3}#{1,6}(?:[ \t]+(?P<title>.*))?(?=\r?\n|\Z)`)
/// substitution: a leading heading becomes its plain title, keeping a `\r`
/// before the line break.
fn strip_leading_heading(body: &str) -> String {
  let indent = body.len() - body.trim_start_matches(' ').len();
  let rest = &body[indent..];
  let hashes = rest.len() - rest.trim_start_matches('#').len();
  if indent > 3 || !(1..=6).contains(&hashes) {
    return body.to_string();
  }
  let after = &rest[hashes..];
  let line = &after[..after.find('\n').unwrap_or(after.len())];
  let (title, matched_end) = if line.starts_with([' ', '\t']) {
    (line.trim_start_matches([' ', '\t']), indent + hashes + line.len())
  } else if line.is_empty() || (line == "\r" && line.len() < after.len()) {
    // A bare `#` line
    ("", indent + hashes)
  } else {
    return body.to_string();
  };

  let (title, cr) = match title.strip_suffix('\r') {
    Some(title) if matched_end < body.len() => (title, "\r"),
    _ => (title, ""),
  };
  // Drop trailing spaces, then a closing `#` run separated by a space
  let mut title = title.trim_end_matches([' ', '\t']);
  let text = title.trim_end_matches('#');
  if text != title && text.ends_with([' ', '\t']) {
    title = text.trim_end_matches([' ', '\t']);
  }
  format!("{}{}{}", title, cr, &body[matched_end..])
}

/// The rate-limit state upstream keeps on the plugin instance, and the
/// looked-up account.
struct Session {
  ratelimit_remaining: i64,
  ratelimit_reset: SystemTime,
  whoami: Option<String>,
}

pub struct Mastodon {
  host: String,
  port: Option<u16>,
  token: String,
  secure: bool,
  visibility: &'static str,
  spoiler: Option<String>,
  idempotency_key: Option<String>,
  language: Option<String>,
  sensitive: bool,
  batch: bool,
  targets: Vec<String>,
  hashtags: Vec<String>,
  ping: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
  session: Mutex<Session>,
}

impl Mastodon {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.verified_host()?;

    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(token) => Some(qsd::unquote(token)),
      None if url.password.is_none() => url.user.as_deref().map(qsd::unquote),
      None => None,
    };
    let Some(token) = token.as_deref().and_then(qsd::validate_token) else {
      tracing::warn!("An invalid Mastodon Access Token was specified.");
      return None;
    };

    let visibility = match url.get("visibility").filter(|v| !v.is_empty()) {
      Some(v) => {
        let vis = qsd::unquote(v).trim().to_lowercase();
        let Some(found) = VISIBILITIES.iter().find(|candidate| candidate.starts_with(&vis)) else {
          tracing::warn!("The Mastodon visibility specified ({}) is invalid.", v);
          return None;
        };
        *found
      }
      None if url.schema.starts_with("toot") => "public",
      None => "default",
    };

    // Targets come from the path and ?to= (as apprise-go 04d2c71 does)
    let mut entries = split_path(url);
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.extend(qsd::parse_list(to).iter().map(|t| qsd::unquote(t)).filter(|t| !t.is_empty()));
    }

    let mut targets = Vec::new();
    let mut hashtags = Vec::new();
    let mut tag_seen = HashSet::new();
    let mut has_error = false;
    for target in parse_list(&entries, true, true) {
      if let Some(user) = match_user(&target) {
        targets.push(format!("@{}", user));
        continue;
      }
      match normalize_ping_token(&target) {
        Some(tag) if tag.starts_with('#') => {
          if tag_seen.insert(tag.to_lowercase()) {
            hashtags.push(tag);
          }
        }
        _ => {
          has_error = true;
          tracing::warn!("Dropped invalid Mastodon target ({}) specified.", target);
        }
      }
    }
    if has_error && targets.is_empty() && hashtags.is_empty() {
      tracing::warn!("No Mastodon targets to notify.");
      return None;
    }

    let ping = match url.get("ping") {
      Some(ping) => normalized_tokens(&[qsd::unquote(ping)], &mut HashSet::new()),
      None => Vec::new(),
    };

    let text_arg = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(qsd::unquote);
    Some(Self {
      host,
      port: url.port,
      token,
      secure: url.schema.ends_with('s'),
      visibility,
      spoiler: text_arg("spoiler"),
      idempotency_key: text_arg("key"),
      language: text_arg("language"),
      sensitive: qsd_bool(url, "sensitive", false),
      batch: qsd_bool(url, "batch", true),
      targets,
      hashtags,
      ping,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      session: Mutex::new(Session { ratelimit_remaining: 1, ratelimit_reset: SystemTime::now(), whoami: None }),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Mastodon",
      service_url: Some("https://joinmastodon.org"),
      setup_url: Some("https://appriseit.com/services/mastodon/"),
      protocols: vec!["mastodon", "toot", "mastodons", "toots"],
      description: "Post a toot on Mastodon.",
      attachment_support: true,
    }
  }

  fn session(&self) -> std::sync::MutexGuard<'_, Session> {
    self.session.lock().unwrap_or_else(|e| e.into_inner())
  }

  /// The configured mentions and hashtags, as one list.
  fn configured(&self) -> Vec<String> {
    vec![self.targets.iter().chain(&self.hashtags).chain(&self.ping).cloned().collect::<Vec<_>>().join(" ")]
  }

  fn api_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    match self.port {
      Some(port) => format!("{}://{}:{}", schema, self.host, port),
      None => format!("{}://{}", schema, self.host),
    }
  }

  /// Upstream `_request()`: a JSON payload, a multipart upload or a GET.
  /// Returns whether it succeeded and the decoded response (`{}` when it
  /// is not JSON).
  async fn request(&self, client: &reqwest::Client, path: &str, body: Request<'_>) -> (bool, Value) {
    let url = format!("{}{}", self.api_url(), path);
    let method = if matches!(body, Request::Get) { "GET" } else { "POST" };
    tracing::debug!("Mastodon {} URL: {} (cert_verify={})", method, url, self.verify_certificate);

    // Wait out an exhausted rate limit
    let wait = {
      let session = self.session();
      if session.ratelimit_remaining == 0 {
        session.ratelimit_reset.duration_since(SystemTime::now()).ok().map(|d| d + Duration::from_millis(500))
      } else {
        None
      }
    };
    throttle_for(wait).await;

    let request = match &body {
      Request::Get => client.get(&url).header("Content-Type", "application/json").body("null"),
      Request::Json(payload) => {
        tracing::debug!("Mastodon Payload: {}", payload);
        client.post(&url).header("Content-Type", "application/json").body(payload.to_string())
      }
      Request::Upload(attachment) => {
        let part = reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(attachment.name.clone()).mime_str("application/octet-stream");
        let Ok(part) = part else { return (false, json!({})) };
        client.post(&url).multipart(reqwest::multipart::Form::new().text("description", attachment.name.clone()).part("file", part))
      }
    };
    let resp = match request.header("User-Agent", APP_ID).bearer_auth(&self.token).send().await {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("Exception received when sending Mastodon {} to {}: ", method, url);
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return (false, json!({}));
      }
    };

    let status = resp.status();
    let headers = resp.headers().clone();
    let text = resp.text().await.unwrap_or_default();
    let content = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({}));
    if !(200..=202).contains(&status.as_u16()) {
      let reason = status.canonical_reason().unwrap_or("");
      tracing::warn!("Failed to send Mastodon {} to {}: {}{}error={}.", method, url, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      tracing::debug!("Response Details:\r\n{}", text);
      return (false, content);
    }

    // Upstream reads the reset time from X-RateLimit-Limit
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok());
    if let Some(remaining) = header("X-RateLimit-Remaining") {
      let mut session = self.session();
      session.ratelimit_remaining = remaining;
      if let Some(reset) = header("X-RateLimit-Limit") {
        session.ratelimit_reset =
          if reset >= 0 { UNIX_EPOCH + Duration::from_secs(reset as u64) } else { UNIX_EPOCH - Duration::from_secs(reset.unsigned_abs()) };
      }
    }
    (true, content)
  }

  /// The authenticated account's username (upstream `_whoami()`), looked
  /// up once per instance.
  async fn whoami(&self, client: &reqwest::Client) -> Option<String> {
    if let Some(user) = self.session().whoami.clone() {
      return Some(user);
    }
    let (ok, response) = self.request(client, "/api/v1/accounts/verify_credentials", Request::Get).await;
    if ok {
      let user = response.get("username").filter(|_| response.get("id").is_some()).map(|u| u.as_str().map(str::to_string).unwrap_or_else(|| u.to_string()));
      self.session().whoami = user.clone();
      user
    } else {
      if missing_scope(&response) {
        tracing::warn!("Failed to lookup Mastodon Auth details; missing scope: read:accounts");
      }
      None
    }
  }
}

/// What [`Mastodon::request`] sends.
enum Request<'a> {
  Get,
  Json(&'a Value),
  Upload(&'a crate::notify::Attachment),
}

/// Whether an error response names a missing OAuth scope.
fn missing_scope(response: &Value) -> bool {
  response.get("error").and_then(Value::as_str).is_some_and(|e| e.contains("authorized scopes"))
}

#[async_trait]
impl Notify for Mastodon {
  fn schemas(&self) -> &[&str] {
    &["mastodon", "toot", "mastodons", "toots"]
  }
  fn service_name(&self) -> &str {
    "Mastodon"
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
  fn title_maxlen(&self) -> usize {
    0
  }
  fn body_maxlen(&self) -> usize {
    // The configured mentions and hashtags are appended to every status
    let suffix = ping_payload(&normalized_tokens(&self.configured(), &mut HashSet::new()));
    MASTODON_BODY_MAXLEN.saturating_sub(suffix.chars().count())
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.0
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    let port = self.port.unwrap_or(if self.secure { 443 } else { 80 });
    Some(vec!["mastodons".into(), self.token.clone(), self.host.clone(), port.to_string()])
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    if *body_format != NotifyFormat::Markdown {
      return body.to_string();
    }
    // Statuses are plain text: a heading is just its line, and escapes
    // are dropped so mentions and hashtags stay intact
    commonmark_decode_backslash_escapes(&strip_leading_heading(body))
  }
  fn has_dialect(&self) -> bool {
    true
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let body = ctx.body.as_str();

    // Direct messages name their recipients, unless the body already does
    let mut targets: Vec<String> = Vec::new();
    if self.visibility == "direct" {
      let users: HashSet<String> = find_mentions(body).into_iter().collect();
      targets = self.targets.iter().filter(|t| !users.contains(*t)).cloned().collect();
      if self.targets.is_empty() {
        let Some(user) = self.whoami(&client).await else { return Ok(false) };
        let myself = format!("@{}", user);
        if !users.contains(&myself) {
          targets.push(myself);
        }
      }
    }

    // Upload the attachments first so the statuses can refer to them
    let mut media: Vec<(Value, String)> = Vec::new();
    for attachment in &ctx.attachments {
      let mime = attachment.mime_type.to_lowercase();
      if !(mime.starts_with("image/") || mime.starts_with("video/") || mime.starts_with("audio/")) {
        tracing::warn!("Ignoring unsupported Mastodon attachment {}.", attachment.name);
        continue;
      }
      tracing::debug!("Preparing Mastodon attachment {}", attachment.name);
      let (ok, response) = self.request(&client, "/api/v1/media", Request::Upload(attachment)).await;
      if !ok {
        if missing_scope(&response) {
          tracing::warn!("Failed to Send Attachment to Mastodon: missing scope: write:media");
        }
        return Ok(false);
      }
      match response.get("id").filter(|id| !matches!(id, Value::Null | Value::Bool(false)) && id.as_str() != Some("")) {
        Some(id) => media.push((id.clone(), mime)),
        None => tracing::debug!("Could not attach the file to Mastodon: {} (mime={})", attachment.name, attachment.mime_type),
      }
    }

    // Append the configured mentions and hashtags the body does not have
    let mut seen: HashSet<String> = targets.iter().map(|t| t.to_lowercase()).collect();
    let mut ping_tokens = Vec::new();
    let has_configured = !(self.targets.is_empty() && self.hashtags.is_empty() && self.ping.is_empty());
    if ctx.body_format == NotifyFormat::Markdown {
      // Markdown can carry visible mention and hashtag tokens
      ping_tokens.extend(detected_tokens(body, &mut seen));
    } else if ctx.body_format == NotifyFormat::Text {
      // Tokens already in a text body are not appended again
      detected_tokens(body, &mut seen);
    }
    if has_configured {
      ping_tokens.extend(normalized_tokens(&self.configured(), &mut seen));
    }

    let mut status = if targets.is_empty() { body.to_string() } else { format!("{} {}", targets.join(" "), body) };
    status.push_str(&ping_payload(&ping_tokens));

    let mut payload = json!({"status": status, "sensitive": self.sensitive});
    if self.visibility != "default" {
      payload["visibility"] = json!(self.visibility);
    }
    if let Some(spoiler) = &self.spoiler {
      payload["spoiler_text"] = json!(spoiler);
    }
    if let Some(key) = &self.idempotency_key {
      payload["Idempotency-Key"] = json!(key);
    }
    if let Some(language) = &self.language {
      payload["language"] = json!(language);
    }

    let payloads = if media.is_empty() {
      vec![payload]
    } else {
      // Up to four PNG/JPEG images share a toot; any other media closes
      // the batch it joins
      let batch_size = if self.batch { TOOT_IMAGES_BATCH } else { 1 };
      let mut batches: Vec<Vec<Value>> = Vec::new();
      let mut batch = Vec::new();
      for (id, mime) in media {
        batch.push(id);
        let groupable = mime.starts_with("image/png") || mime.starts_with("image/jpeg") || mime.starts_with("image/jpg");
        if !groupable || batch.len() >= batch_size {
          batches.push(std::mem::take(&mut batch));
        }
      }
      if !batch.is_empty() {
        batches.push(batch);
      }

      let total = batches.len();
      batches
        .into_iter()
        .enumerate()
        .map(|(no, media_ids)| {
          let mut p = payload.clone();
          p["media_ids"] = json!(media_ids);
          if no > 0 || body.is_empty() {
            // Later toots carry only their position
            p["status"] = json!(format!("{:02}/{:02}", no + 1, total));
            p["sensitive"] = json!(false);
            if let Some(key) = &self.idempotency_key {
              p["Idempotency-Key"] = json!(format!("{}-part{:02}", key, no));
            }
          }
          p
        })
        .collect()
    };

    let mut has_error = false;
    let total = payloads.len();
    for (no, payload) in payloads.iter().enumerate().map(|(i, p)| (i + 1, p)) {
      // Skip a post that already went out so a retry does not repeat it
      let key = no.to_string();
      if is_delivered(&key, false) {
        continue;
      }

      let (ok, response) = self.request(&client, "/api/v1/statuses", Request::Json(payload)).await;
      if !ok {
        has_error = true;
        if missing_scope(&response) {
          tracing::warn!("Failed to Send Status to Mastodon: missing scope: write:statuses");
        }
        continue;
      }

      let url =
        response.pointer("/account/username").and_then(Value::as_str).map(|u| format!("{}/web/@{}", self.api_url(), u)).unwrap_or_else(|| "unknown".into());
      let attached = payload.get("media_ids").and_then(Value::as_array).map_or(0, Vec::len);
      tracing::debug!("Mastodon [{:02}/{:02}] ({} attached) delivered to {}", no, total, attached, url);
      tracing::info!("Sent [{:02}/{:02}] Mastodon notification as public toot.", no, total);
      mark_delivered(&key, false);
    }

    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::Attachment;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const STATUSES: &str = "/api/v1/statuses";
  const MEDIA: &str = "/api/v1/media";
  const WHOAMI: &str = "/api/v1/accounts/verify_credentials";

  fn mastodon(url: &str) -> Mastodon {
    Mastodon::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str, format: NotifyFormat) -> NotifyContext {
    NotifyContext { body: body.into(), body_format: format, ..Default::default() }
  }

  fn attachment(name: &str, mime: &str) -> Attachment {
    Attachment { name: name.into(), data: b"data".to_vec(), mime_type: mime.into() }
  }

  async fn status_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(STATUSES)).respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).mount(&server).await;
    server
  }

  /// Send through `svc` and return the status payloads posted.
  async fn statuses(server: &MockServer, svc: &Mastodon, c: &NotifyContext) -> (bool, Vec<Value>) {
    let ok = svc.send(c).await.unwrap();
    (ok, requests_to(server, "POST", STATUSES).await.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect())
  }

  fn url(server: &MockServer, rest: &str) -> String {
    format!("mastodon://access_key@127.0.0.1:{}{}", server.address().port(), rest)
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "mastodon://",
      "mastodon://:@/",
      // No token
      "mastodon://hostname",
      // No valid target
      "mastodon://access_token@hostname/-/%/",
      "mastodon://access_token@hostname?visibility=invalid",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "toot://access_token@hostname",
      "toots://access_token@hostname",
      "mastodon://access_token@hostname/@user/@user2",
      "mastodon://hostname/@user/@user2?token=abcd123",
      "mastodon://access_token@hostname?to=@user, @user2",
      "mastodon://access_token@hostname/?cache=no",
      "mastodon://access_token@hostname/?spoiler=spoiler%20text",
      "mastodon://access_token@hostname/?language=en",
      "mastodons://access_token@hostname:8443",
      "mastodon://access_token@hostname/?key=My%20Idempotency%20Key",
      "mastodon://access_token@hostname?visibility=direct",
      // One invalid target is dropped when another is valid
      "mastodon://access_token@hostname/@user/-",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_url_fields() {
    let m = mastodon("mastodon://mytoken@nuxref.com/@user2/user1/%23Tag/%23tag?visibility=d&spoiler=test&language=en&sensitive=yes&key=k");
    assert_eq!((m.host.as_str(), m.token.as_str(), m.visibility), ("nuxref.com", "mytoken", "default"));
    assert_eq!((m.spoiler.as_deref(), m.language.as_deref(), m.idempotency_key.as_deref()), (Some("test"), Some("en"), Some("k")));
    assert!(m.sensitive && m.batch && !m.secure);
    // Sorted as upstream's parse_list() sorts them
    assert_eq!(m.targets, vec!["@user2", "@user1"]);
    assert_eq!(m.hashtags, vec!["#Tag"]);

    assert_eq!(mastodon("toots://t@host").visibility, "public");
    assert_eq!(mastodon("toots://t@host?visibility=un").visibility, "unlisted");
    assert_eq!(mastodon("mastodon://t@host?visibility=dir").visibility, "direct");
    assert_eq!(mastodon("mastodon://t@host").visibility, "default");
    assert_eq!(mastodon("mastodon://hostname/@user?token=abcd123").token, "abcd123");
    // A user with a password is not a token
    assert!(from_url("mastodon://user:pass@hostname").is_none());
    // ?to= adds targets
    assert_eq!(mastodon("mastodon://t@hostname?to=@user, @user2@other.host").targets, vec!["@user", "@user2@other.host"]);

    let m = mastodon("mastodon://t@host?ping=%23Media,%23media,@caronc,radarr,%23123,bad%25");
    assert_eq!(m.ping, vec!["#Media", "@caronc"]);
  }

  #[test]
  fn test_token_detection() {
    let body = "Body **#déjà_vu** and @abc, plus #再会. Ignore ##bad, #bad%, #123, email@miss, and duplicate @abc #déjà_vu.";
    assert_eq!(find_mentions(body), vec!["@abc", "@abc"]);
    assert_eq!(find_hashtags(body), vec!["#déjà_vu", "#再会", "#123", "#déjà_vu"]);
    assert!(!valid_hashtag("#123") && !valid_hashtag("#bad%") && !valid_hashtag("#_x") && valid_hashtag("#a_1"));
    assert_eq!(find_mentions("@a@b.c! @x@host.com. (@y) @z!"), vec!["@a@b", "@x@host.com.", "@y"]);
    assert_eq!(normalize_ping_token(" "), None);
    assert_eq!(normalize_ping_token("alerts"), None);
    assert_eq!(normalize_ping_token("@user@host"), Some("@user@host".into()));
  }

  #[test]
  fn test_dialect_convert() {
    let m = mastodon("mastodon://t@host");
    let md = NotifyFormat::Markdown;
    assert_eq!(m.dialect_convert("# My Title\nmy\\_var", &md), "My Title\nmy_var");
    assert_eq!(m.dialect_convert("## Title ##\r\nbody", &md), "Title\r\nbody");
    assert_eq!(m.dialect_convert("# Title#\nbody", &md), "Title#\nbody");
    assert_eq!(m.dialect_convert("#\nbody", &md), "\nbody");
    assert_eq!(m.dialect_convert("#\r\nbody", &md), "\r\nbody");
    assert_eq!(m.dialect_convert("# T \t## \r\nx", &md), "T\r\nx");
    assert_eq!(m.dialect_convert(" # sp", &md), "sp");
    assert_eq!(m.dialect_convert("#  \r", &md), "\r");
    assert_eq!(m.dialect_convert("#nospace\nbody", &md), "#nospace\nbody");
    assert_eq!(m.dialect_convert("    # indented", &md), "    # indented");
    assert_eq!(m.dialect_convert("####### seven", &md), "####### seven");
    // Only Markdown is converted
    assert_eq!(m.dialect_convert("# T\nmy\\_var", &NotifyFormat::Text), "# T\nmy\\_var");
  }

  #[tokio::test]
  async fn test_status_payloads() {
    // Verified against upstream with pycapture.py
    let server = status_server().await;
    let m = mastodon(&url(&server, "/@bob/%23tag?ping=@carol"));
    let (ok, posted) = statuses(&server, &m, &ctx("My *Title*\r\nHello **world** my\\_var @dave #x <b>b</b>", NotifyFormat::Text)).await;
    assert!(ok);
    assert_eq!(posted, vec![json!({"status": "My *Title*\r\nHello **world** my\\_var @dave #x <b>b</b> #tag @bob @carol", "sensitive": false})]);
    let req = &requests_to(&server, "POST", STATUSES).await[0];
    assert_eq!(req.headers.get("authorization").unwrap(), "Bearer access_key");
    assert_eq!(req.headers.get("content-type").unwrap(), "application/json");
    assert_eq!(req.headers.get("user-agent").unwrap(), APP_ID);

    // Markdown appends the tokens of the body (upstream behaviour)
    let server = status_server().await;
    let m = mastodon(&url(&server, "?format=markdown&ping=%23apprise"));
    let (_, posted) = statuses(&server, &m, &ctx("hello @bob and #tagged", NotifyFormat::Markdown)).await;
    assert_eq!(posted[0]["status"], "hello @bob and #tagged @bob #tagged #apprise");

    // HTML only gets the configured tokens
    let server = status_server().await;
    let m = mastodon(&url(&server, "/@bob?format=html"));
    let (_, posted) = statuses(&server, &m, &ctx("<b>T</b><br />\r\nHello&nbsp;@bob", NotifyFormat::Html)).await;
    assert_eq!(posted[0]["status"], "<b>T</b><br />\r\nHello&nbsp;@bob @bob");

    // Options
    let server = status_server().await;
    let m = mastodon(&url(&server, "?visibility=unlisted&spoiler=S&key=K&language=fr&sensitive=yes"));
    let (_, posted) = statuses(&server, &m, &ctx("body", NotifyFormat::Text)).await;
    assert_eq!(
      posted[0],
      json!({"status": "body", "sensitive": true, "visibility": "unlisted", "spoiler_text": "S", "Idempotency-Key": "K", "language": "fr"})
    );
  }

  #[tokio::test]
  async fn test_pings() {
    let body = "Body **#déjà_vu** and @abc, plus #再会. Ignore ##bad, #bad%, #123, email@miss, and duplicate @abc #déjà_vu.";
    let server = status_server().await;
    let m = mastodon(&url(&server, "?format=markdown&ping=@abc,%23déjà_vu,%23radarr,radarr,%23123,bad%25"));
    let (_, posted) = statuses(&server, &m, &ctx(body, NotifyFormat::Markdown)).await;
    assert_eq!(posted[0]["status"], format!("{} @abc #déjà_vu #再会 #radarr", body));

    let server = status_server().await;
    let (_, posted) = statuses(&server, &mastodon(&url(&server, "?format=markdown")), &ctx("Body with #solo only.", NotifyFormat::Markdown)).await;
    assert_eq!(posted[0]["status"], "Body with #solo only. #solo");

    let server = status_server().await;
    let body = "<p>No tags in this formatted body.</p>";
    let (_, posted) = statuses(&server, &mastodon(&url(&server, "?format=html")), &ctx(body, NotifyFormat::Html)).await;
    assert_eq!(posted[0]["status"], body);

    let server = status_server().await;
    let (_, posted) = statuses(&server, &mastodon(&url(&server, "/%23apprise?ping=%23apprise,%23media")), &ctx("Body", NotifyFormat::Text)).await;
    assert_eq!(posted[0]["status"], "Body #apprise #media");

    // Plain text keeps its tokens where they are; configured ones the body
    // already has are not repeated
    let body = "Plain text keeps #déjà_vu and @abc exactly where they are.";
    let server = status_server().await;
    let (_, posted) = statuses(&server, &mastodon(&url(&server, "")), &ctx(body, NotifyFormat::Text)).await;
    assert_eq!(posted[0]["status"], body);
    let server = status_server().await;
    let m = mastodon(&url(&server, "/@media/%23homeassistant?ping=radarr,@media,%23homeassistant,%23alerts,%23123,bad%25,@abc"));
    let (_, posted) = statuses(&server, &m, &ctx(body, NotifyFormat::Text)).await;
    assert_eq!(posted[0]["status"], format!("{} #alerts #homeassistant @media", body));
  }

  #[tokio::test]
  async fn test_body_maxlen_leaves_room_for_pings() {
    let m = mastodon("mastodon://t@host?ping=%23apprise");
    assert_eq!(m.body_maxlen(), 500 - " #apprise".len());
    assert_eq!(mastodon("mastodon://t@host").body_maxlen(), 500);
    let server = status_server().await;
    let m = mastodon(&url(&server, "?ping=%23apprise"));
    let (_, posted) = statuses(&server, &m, &ctx(&"a".repeat(m.body_maxlen()), NotifyFormat::Text)).await;
    let status = posted[0]["status"].as_str().unwrap();
    assert_eq!(status.chars().count(), 500);
    assert!(status.ends_with(" #apprise"));
  }

  #[tokio::test]
  async fn test_direct_messages() {
    let server = status_server().await;
    Mock::given(method("GET"))
      .and(path(WHOAMI))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"username": "me", "id": "42"})))
      .mount(&server)
      .await;

    // The body already names the target
    let m = mastodon(&url(&server, "/@target?visibility=direct"));
    let (_, posted) = statuses(&server, &m, &ctx("Direct body already contains @target.", NotifyFormat::Text)).await;
    assert_eq!(posted[0]["status"], "Direct body already contains @target.");
    // An email address is not a mention
    let (_, posted) = statuses(&server, &m, &ctx("Direct body contains email@target only.", NotifyFormat::Text)).await;
    assert_eq!(posted[1]["status"], "@target Direct body contains email@target only.");
    assert!(requests_to(&server, "GET", WHOAMI).await.is_empty());

    // Targets missing from the body lead the status
    let m = mastodon(&url(&server, "/@bob/@carol?visibility=direct"));
    let (_, posted) = statuses(&server, &m, &ctx("hi @bob", NotifyFormat::Text)).await;
    assert_eq!(posted[2], json!({"status": "@carol hi @bob", "sensitive": false, "visibility": "direct"}));

    // Without targets the message goes to the account itself, looked up once
    let m = mastodon(&url(&server, "?visibility=direct&ping=@zed"));
    let (_, posted) = statuses(&server, &m, &ctx("hi @me", NotifyFormat::Text)).await;
    assert_eq!(posted[3]["status"], "hi @me @zed");
    let (_, posted) = statuses(&server, &m, &ctx("hello", NotifyFormat::Text)).await;
    assert_eq!(posted[4]["status"], "@me hello @zed");
    let whoami = requests_to(&server, "GET", WHOAMI).await;
    assert_eq!(whoami.len(), 1);
    assert_eq!(whoami[0].headers.get("authorization").unwrap(), "Bearer access_key");

    // The account lookup fails
    for response in
      [ResponseTemplate::new(200).set_body_json(json!({})), ResponseTemplate::new(403).set_body_json(json!({"error": "outside the authorized scopes"}))]
    {
      let server = status_server().await;
      Mock::given(method("GET")).and(path(WHOAMI)).respond_with(response).mount(&server).await;
      let (ok, posted) = statuses(&server, &mastodon(&url(&server, "?visibility=direct")), &ctx("hi", NotifyFormat::Text)).await;
      assert!(!ok);
      assert!(posted.is_empty());
    }
  }

  #[tokio::test]
  async fn test_attachments_batches() {
    // Verified against upstream with pycapture.py
    let server = status_server().await;
    Mock::given(method("POST")).and(path(MEDIA)).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m1"}))).mount(&server).await;
    let m = mastodon(&url(&server, "?key=K&sensitive=yes"));
    let mut c = ctx("T\r\nB", NotifyFormat::Text);
    c.attachments =
      vec![attachment("a.png", "image/png"), attachment("b.gif", "image/gif"), attachment("c.png", "image/png"), attachment("notes.txt", "text/plain")];
    let (ok, posted) = statuses(&server, &m, &c).await;
    assert!(ok);
    assert_eq!(
      posted,
      vec![
        json!({"status": "T\r\nB", "sensitive": true, "Idempotency-Key": "K", "media_ids": ["m1", "m1"]}),
        json!({"status": "02/02", "sensitive": false, "Idempotency-Key": "K-part01", "media_ids": ["m1"]}),
      ]
    );
    // The text file is not uploaded
    let uploads = requests_to(&server, "POST", MEDIA).await;
    assert_eq!(uploads.len(), 3);
    let form = String::from_utf8_lossy(&uploads[0].body).to_string();
    assert!(form.contains("name=\"description\"\r\n\r\na.png"), "{}", form);
    assert!(form.contains("name=\"file\"; filename=\"a.png\"\r\nContent-Type: application/octet-stream"), "{}", form);
    assert_eq!(uploads[0].headers.get("authorization").unwrap(), "Bearer access_key");

    // Without batching (and without a body) every image is its own toot
    let server = status_server().await;
    Mock::given(method("POST")).and(path(MEDIA)).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m1"}))).mount(&server).await;
    let mut c = ctx("", NotifyFormat::Text);
    c.attachments = vec![attachment("a.png", "image/png"), attachment("c.jpg", "image/jpeg")];
    let (_, posted) = statuses(&server, &mastodon(&url(&server, "?batch=no")), &c).await;
    assert_eq!(
      posted,
      vec![json!({"status": "01/02", "sensitive": false, "media_ids": ["m1"]}), json!({"status": "02/02", "sensitive": false, "media_ids": ["m1"]})]
    );
  }

  #[tokio::test]
  async fn test_attachment_failures() {
    // A failed upload stops the notification
    let server = status_server().await;
    Mock::given(method("POST"))
      .and(path(MEDIA))
      .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "outside the authorized scopes"})))
      .mount(&server)
      .await;
    let mut c = ctx("body", NotifyFormat::Text);
    c.attachments = vec![attachment("a.png", "image/png")];
    let (ok, posted) = statuses(&server, &mastodon(&url(&server, "")), &c).await;
    assert!(!ok);
    assert!(posted.is_empty());

    // An upload without an ID is left out
    let server = status_server().await;
    Mock::given(method("POST")).and(path(MEDIA)).respond_with(ResponseTemplate::new(200).set_body_json(json!({}))).mount(&server).await;
    let (ok, posted) = statuses(&server, &mastodon(&url(&server, "")), &c).await;
    assert!(ok);
    assert_eq!(posted, vec![json!({"status": "body", "sensitive": false})]);
  }

  #[tokio::test]
  async fn test_failures() {
    for response in [ResponseTemplate::new(403), ResponseTemplate::new(500).set_body_string("not json"), ResponseTemplate::new(999)] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(STATUSES)).respond_with(response).mount(&server).await;
      assert!(!mastodon(&url(&server, "")).send(&ctx("test", NotifyFormat::Text)).await.unwrap());
    }
    // Any 2xx Mastodon uses is a success, even without JSON
    for code in [201, 202] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(STATUSES)).respond_with(ResponseTemplate::new(code)).mount(&server).await;
      assert!(mastodon(&url(&server, "")).send(&ctx("test", NotifyFormat::Text)).await.unwrap());
    }
    assert!(!mastodon("toots://access_token@127.0.0.1:1").send(&ctx("test", NotifyFormat::Text)).await.unwrap());
  }

  #[tokio::test]
  async fn test_rate_limit_headers() {
    let reset = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(STATUSES))
      .respond_with(ResponseTemplate::new(200).insert_header("X-RateLimit-Remaining", "0").insert_header("X-RateLimit-Limit", reset.to_string().as_str()))
      .mount(&server)
      .await;
    let m = mastodon(&url(&server, ""));
    assert!(m.send(&ctx("test", NotifyFormat::Text)).await.unwrap());
    assert_eq!(m.session().ratelimit_remaining, 0);
    assert_eq!(m.session().ratelimit_reset, UNIX_EPOCH + Duration::from_secs(reset));

    // A failed request keeps the last values
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(STATUSES)).respond_with(ResponseTemplate::new(403).insert_header("X-RateLimit-Remaining", "5")).mount(&server).await;
    let m = mastodon(&url(&server, ""));
    assert!(!m.send(&ctx("test", NotifyFormat::Text)).await.unwrap());
    assert_eq!(m.session().ratelimit_remaining, 1);
  }

  #[tokio::test]
  async fn test_markdown_through_apprise() {
    // The orchestrator turns the title into a heading, which the dialect
    // turns back into a plain line
    let server = status_server().await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&url(&server, "/@bob?format=markdown")));
    let c =
      NotifyContext { title: "My Title".into(), body: "Hello **world** my\\_var @dave".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
    assert!(apprise.notify(&c).await.is_success());
    let posted: Value = serde_json::from_slice(&requests_to(&server, "POST", STATUSES).await[0].body).unwrap();
    assert_eq!(posted["status"], "My Title\nHello **world** my_var @dave @dave @bob");
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_toots() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(MEDIA)).respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "m1"}))).mount(&server).await;
    // The second toot fails once
    Mock::given(method("POST"))
      .and(path(STATUSES))
      .and(wiremock::matchers::body_string_contains("02/02"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(STATUSES)).respond_with(ResponseTemplate::new(200)).mount(&server).await;

    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&url(&server, "?batch=no&retry=1&wait=0")));
    let mut c = ctx("body", NotifyFormat::Text);
    c.attachments = vec![attachment("a.png", "image/png"), attachment("c.png", "image/png")];
    assert!(apprise.notify(&c).await.is_success());
    let posted: Vec<String> = requests_to(&server, "POST", STATUSES)
      .await
      .iter()
      .map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["status"].as_str().unwrap().to_string())
      .collect();
    // The retry only sends the toot that failed
    assert_eq!(posted, vec!["body", "02/02", "02/02"]);
  }
}
