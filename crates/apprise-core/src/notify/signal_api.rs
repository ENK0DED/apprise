use crate::conversion::commonmark::{
  EmphasisDelimiter, commonmark_can_close_emphasis, commonmark_can_open_emphasis, commonmark_decode_backslash_escapes, commonmark_emphasis_run,
  commonmark_find_backtick_run, commonmark_headings_to_bold, commonmark_index_backtick_runs, commonmark_match_emphasis, commonmark_new_scan_budget,
  commonmark_open_link_label, commonmark_pick_emphasis_sentinel, commonmark_scan_angle_dest, commonmark_scan_autolink_dest, commonmark_scan_paren_dest,
  commonmark_take_link_label,
};
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::custom_json::basic_auth;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, throttle};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::plugin_args::{qsd_bool, split_path};
use crate::utils::qsd;
use async_trait::async_trait;
use base64::Engine;
use serde_json::json;

/// Recipients per request in batch mode (upstream `default_batch_size`).
const BATCH_SIZE: usize = 10;

/// Signal markup characters that need a backslash to stay literal.
const SIGNAL_STYLE_CHARS: &str = "*`~|";

/// A zero-width space separates literal escapes from adjacent markup.
const SIGNAL_ZWSP: char = '\u{200b}';

/// Upstream `GROUP_REGEX` (`^\s*((@|%40)?(group\.)|@|%40)(?P<group>[a-z0-9_=-]+)`):
/// the group ID of `@id`, `group.id` or `@group.id`. Anything after the ID
/// is ignored.
fn group_id(target: &str) -> Option<&str> {
  let s = target.trim_start();
  let starts_ci = |s: &str, p: &str| s.len() >= p.len() && s.is_char_boundary(p.len()) && s[..p.len()].eq_ignore_ascii_case(p);
  let strip_at = |s: &'_ str| -> Option<usize> {
    if s.starts_with('@') {
      Some(1)
    } else if starts_ci(s, "%40") {
      Some(3)
    } else {
      None
    }
  };
  let id_len = |s: &str| s.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '=' | '-')).count();

  // `[@]group.` is tried first, then a bare `@`
  let mut candidates = Vec::new();
  let at = strip_at(s).unwrap_or(0);
  if starts_ci(&s[at..], "group.") {
    candidates.push(at + "group.".len());
  }
  if at > 0 {
    candidates.push(at);
  }
  candidates.into_iter().map(|start| &s[start..]).find_map(|rest| {
    let len = id_len(rest);
    (len > 0).then(|| &rest[..len])
  })
}

/// Signal's escaping of literal text: style characters get a backslash and
/// a literal backslash becomes `marker` until the final pass.
fn escape(text: &str, marker: &str) -> String {
  let mut out = String::with_capacity(text.len());
  for c in text.chars() {
    if c == '\\' {
      out.push_str(marker);
    } else {
      if SIGNAL_STYLE_CHARS.contains(c) {
        out.push('\\');
      }
      out.push(c);
    }
  }
  out
}

/// One `~` run: its length and whether it can open or close.
struct Tilde {
  len: usize,
  can_open: bool,
  can_close: bool,
}

/// Translate CommonMark to signal-cli-rest-api styled text (upstream
/// `_commonmark_to_signal`).
///
/// ```text
/// CommonMark          Signal
/// # heading           **heading** (Signal has no headings)
/// **bold**            **bold**
/// *italic*            *italic*
/// ~~strike~~          ~strike~
/// `code`              `code`
/// [label](<url>)      label (url)
/// \_                  _
/// ```
///
/// Unescaped `||spoiler||` is Signal's own syntax and is kept. Other
/// backslash escapes are dropped unless Signal needs them to keep a `*`,
/// backtick, `~` or `|` literal.
fn commonmark_to_signal(body: &str) -> String {
  // Signal shows headings as bold text
  let body = commonmark_headings_to_bold(body);
  let s: Vec<char> = body.chars().collect();
  let n = s.len();
  let starts = |i: usize, pat: &str| pat.chars().enumerate().all(|(k, c)| s.get(i + k) == Some(&c));

  let mut out = String::with_capacity(body.len());
  let mut delimiters: Vec<EmphasisDelimiter> = Vec::new();
  let mut tildes: Vec<Tilde> = Vec::new();
  // Where each pending link's "[" sits in `out`, innermost last
  let mut link_stack: Vec<usize> = Vec::new();
  let backtick_runs = commonmark_index_backtick_runs(&s);
  let sentinel = commonmark_pick_emphasis_sentinel(&body);
  // A literal backslash is marked until the final pass; the backslash
  // after the sentinel keeps a following digit from reading as an index
  let backslash = format!("{sentinel}\\");
  let mut budget = commonmark_new_scan_budget(&s);

  // `]` + "(" destination: replace the buffered label with `label (url)`
  let append_link = |out: &mut String, link_stack: &mut Vec<usize>, raw_url: &str| {
    let url = escape(&commonmark_decode_backslash_escapes(raw_url), &backslash);
    let text = commonmark_take_link_label(out, link_stack);
    if text.is_empty() {
      out.push_str(&url);
    } else {
      out.push_str(&format!("{text} ({url})"));
    }
  };

  let mut i = 0;
  while i < n {
    let ch = s[i];

    // CommonMark escapes, keeping Signal's own where needed
    if ch == '\\' && i + 1 < n && s[i + 1].is_ascii_punctuation() {
      out.push_str(&escape(&s[i + 1].to_string(), &backslash));
      i += 2;
      continue;
    }
    // A backslash before a line break is a hard break
    if ch == '\\' && i + 1 < n && s[i + 1] == '\n' {
      i += 1;
      continue;
    }
    if ch == '\\' {
      out.push_str(&backslash);
      i += 1;
      continue;
    }

    // Signal uses a single backtick for monospace text
    if ch == '`' {
      let mut j = i;
      while j < n && s[j] == '`' {
        j += 1;
      }
      let run = j - i;
      if let Some(close) = commonmark_find_backtick_run(&backtick_runs, j, run) {
        let mut content: String = s[j..close].iter().collect();
        if run >= 3 && content.contains('\n') {
          // Drop the language line of a fenced code block
          content = content.split_once('\n').map(|(_, rest)| rest.trim_end_matches('\n').to_string()).unwrap_or_default();
        }
        out.push('`');
        out.push_str(&escape(&content, &backslash));
        out.push('`');
        i = close + run;
        continue;
      }
      // Unmatched backticks stay literal
      out.push_str(&escape(&s[i..j].iter().collect::<String>(), &backslash));
      i = j;
      continue;
    }

    // A possible link-label opening; past the depth limit it is plain text
    if ch == '[' && commonmark_open_link_label(&mut out, &mut link_stack) {
      i += 1;
      continue;
    }

    // [label](<url>)
    if starts(i, "](<")
      && !link_stack.is_empty()
      && let Some(close) = commonmark_scan_angle_dest(&s, i, n, Some(&mut budget))
    {
      append_link(&mut out, &mut link_stack, &s[i + 3..close].iter().collect::<String>());
      i = close + 2;
      continue;
    }

    // [label](url), without reading the URL as markup
    if starts(i, "](") && !link_stack.is_empty() {
      if let Some(close) = commonmark_scan_paren_dest(&s, i + 1, n, Some(&mut budget)) {
        append_link(&mut out, &mut link_stack, &s[i + 2..close].iter().collect::<String>());
        i = close + 1;
        continue;
      }
      // Keep this label from matching an unrelated link
      link_stack.pop();
    }

    // A "]" that never forms "](" cannot close a link
    if ch == ']' && !link_stack.is_empty() && !starts(i, "](") {
      link_stack.pop();
    }

    // Autolink brackets are dropped; Signal recognizes the URL
    if ch == '<' {
      let (close, still_valid) = commonmark_scan_autolink_dest(&s, i, n);
      if let Some(close) = close {
        out.push_str(&escape(&s[i + 1..close].iter().collect::<String>(), &backslash));
        i = close + 1;
        continue;
      }
      if !still_valid {
        out.push(ch);
        i += 1;
        continue;
      }
      // An unfinished autolink is literal text
      out.push_str(&escape(&s[i..].iter().collect::<String>(), &backslash));
      i = n;
      continue;
    }

    if ch == '*' || ch == '_' {
      i = commonmark_emphasis_run(&s, i, n, &mut delimiters, &mut out, &sentinel);
      continue;
    }

    // A possible strikethrough opener or closer
    if ch == '~' {
      let mut j = i;
      while j < n && s[j] == '~' {
        j += 1;
      }
      let prev_ch = if i > 0 { Some(s[i - 1]) } else { None };
      let next_ch = s.get(j).copied();
      tildes.push(Tilde {
        len: j - i,
        can_open: commonmark_can_open_emphasis(ch, prev_ch, next_ch),
        can_close: commonmark_can_close_emphasis(ch, prev_ch, next_ch),
      });
      out.push_str(&format!("{sentinel}~{}{sentinel}", tildes.len() - 1));
      i = j;
      continue;
    }

    // Ordinary characters, a raw `||` included
    out.push(ch);
    i += 1;
  }

  // Pair tilde runs with the nearest opener of the same size
  let mut openers: [Vec<usize>; 3] = [Vec::new(), Vec::new(), Vec::new()];
  let mut matched = vec![false; tildes.len()];
  for (index, tilde) in tildes.iter().enumerate() {
    if tilde.len > 2 {
      continue;
    }
    if tilde.can_close
      && let Some(opener) = openers[tilde.len].pop()
    {
      matched[opener] = true;
      matched[index] = true;
      // Openers of the other size crossed by this pair are dropped
      let other = &mut openers[3 - tilde.len];
      while other.last().is_some_and(|o| *o > opener) {
        other.pop();
      }
    }
    if !matched[index] && tilde.can_open {
      openers[tilde.len].push(index);
    }
  }

  // Pair `*` and `_` runs by CommonMark's emphasis rules
  commonmark_match_emphasis(&mut delimiters);

  let render = |tilde: bool, index: usize, before: &str| -> String {
    if tilde {
      // Strikethrough uses a single `~` on either side
      return match tildes.get(index) {
        Some(_) if matched[index] => "~".to_string(),
        Some(t) => "\\~".repeat(t.len),
        None => String::new(),
      };
    }
    let Some(d) = delimiters.get(index) else { return String::new() };
    let marker = |strong: bool| if strong { "**" } else { "*" };
    // Closers, the unmatched (escaped) delimiters, then openers
    let mut pieces: Vec<String> = d.events.iter().filter(|e| !e.open).map(|e| marker(e.strong).to_string()).collect();
    pieces.push(escape(&d.ch.to_string().repeat(d.numdelims), &backslash));
    pieces.extend(d.events.iter().rev().filter(|e| e.open).map(|e| marker(e.strong).to_string()));

    // Separate adjacent star tokens so Signal does not merge them
    let mut after_star = before.ends_with("\\*");
    let mut result = String::new();
    for piece in pieces.iter().filter(|p| !p.is_empty()) {
      if piece.starts_with('*') && after_star {
        result.push(SIGNAL_ZWSP);
      }
      result.push_str(piece);
      after_star = piece.ends_with('*');
    }
    result
  };

  // Replace every `sentinel ~? digits sentinel` placeholder
  let mut rendered = String::with_capacity(out.len());
  let mut pos = 0;
  while pos < out.len() {
    let rest = &out[pos..];
    if let Some(after) = rest.strip_prefix(sentinel.as_str()) {
      let (tilde, digits_at) = if after.starts_with('~') { (true, 1) } else { (false, 0) };
      let digits = after[digits_at..].bytes().take_while(u8::is_ascii_digit).count();
      if digits > 0 && after[digits_at + digits..].starts_with(sentinel.as_str()) {
        let index = after[digits_at..digits_at + digits].parse().unwrap_or(usize::MAX);
        rendered.push_str(&render(tilde, index, &out[..pos]));
        pos += sentinel.len() + digits_at + digits + sentinel.len();
        continue;
      }
    }
    let c = rest.chars().next().unwrap_or_default();
    rendered.push(c);
    pos += c.len_utf8();
  }

  // Put literal backslashes back, separated from markup that follows
  let mut result = String::with_capacity(rendered.len());
  let mut rest = rendered.as_str();
  while let Some(p) = rest.find(backslash.as_str()) {
    result.push_str(&rest[..p]);
    result.push('\\');
    rest = &rest[p + backslash.len()..];
    if rest.starts_with(|c| SIGNAL_STYLE_CHARS.contains(c)) {
      result.push(SIGNAL_ZWSP);
    }
  }
  result.push_str(rest);
  result
}

pub struct SignalApi {
  host: String,
  port: Option<u16>,
  user: Option<String>,
  password: Option<String>,
  source: String,
  targets: Vec<String>,
  batch: bool,
  status: bool,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl SignalApi {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // signal://[user[:password]@]host[:port]/{from_phone}/{targets}
    let host = url.host.clone().filter(|h| !h.is_empty())?;
    let mut targets = split_path(url);
    let source = match url.get("from").filter(|f| !f.is_empty()) {
      Some(from) => Some(qsd::unquote(from)),
      None if !targets.is_empty() => Some(targets.remove(0)),
      None => None,
    };
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.extend(parse_phone_no(&[qsd::unquote(to)]));
    }

    let Some(source) = source.as_deref().and_then(|s| is_phone_no(s, 10)) else {
      tracing::warn!("An invalid Signal API Source Phone No was provided.");
      return None;
    };
    let source = format!("+{}", source.full);

    let mut parsed = Vec::new();
    if targets.is_empty() {
      // Send a message to ourselves
      parsed.push(source.clone());
    } else {
      for target in parse_phone_no(&targets) {
        if let Some(phone) = is_phone_no(&target, 10) {
          parsed.push(format!("+{}", phone.full));
        } else if let Some(group) = group_id(&target) {
          parsed.push(format!("group.{}", group));
        } else {
          tracing::warn!("Dropped invalid phone/group ({}) specified.", target);
        }
      }
    }

    Some(Self {
      host,
      port: url.port,
      user: url.user.clone(),
      password: url.password.clone(),
      source,
      targets: parsed,
      batch: qsd_bool(url, "batch", false),
      status: qsd_bool(url, "status", false),
      secure: url.schema == "signals",
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Signal API",
      service_url: Some("https://bbernhard.github.io/signal-cli-rest-api/"),
      setup_url: Some("https://appriseit.com/services/signal/"),
      protocols: vec!["signal", "signals"],
      description: "Send Signal messages via signal-cli REST API.",
      attachment_support: true,
    }
  }
}

#[async_trait]
impl Notify for SignalApi {
  fn schemas(&self) -> &[&str] {
    &["signal", "signals"]
  }
  fn service_name(&self) -> &str {
    "Signal API"
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
  fn url_identifier(&self) -> Option<Vec<String>> {
    // Upstream hashes str() of each value, so a missing one is "None"
    let none = || "None".to_string();
    Some(vec![
      if self.secure { "signals" } else { "signal" }.into(),
      self.user.clone().unwrap_or_else(none),
      self.password.clone().unwrap_or_else(none),
      self.host.clone(),
      self.port.map_or_else(none, |p| p.to_string()),
      self.source.clone(),
    ])
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    if *body_format == NotifyFormat::Markdown { commonmark_to_signal(body) } else { body.to_string() }
  }
  fn has_dialect(&self) -> bool {
    true
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There were no Signal API targets to notify.");
      return Ok(false);
    }

    let prefix = if self.status { format!("{} ", ctx.asset.ascii(&ctx.notify_type)) } else { String::new() };
    let mut payload = json!({
      "message": crate::conversion::pystr::rstrip(&format!("{}{}", prefix, ctx.body)),
      "number": self.source,
      // Markdown is sent as Signal styled text
      "text_mode": if ctx.body_format == NotifyFormat::Markdown { "styled" } else { "normal" },
      "recipients": [],
    });
    if !ctx.attachments.is_empty() {
      let encoded: Vec<String> = ctx.attachments.iter().map(|a| base64::engine::general_purpose::STANDARD.encode(&a.data)).collect();
      payload["base64_attachments"] = json!(encoded);
    }

    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    let url = format!("{}://{}{}/v2/send", schema, self.host, port);
    let auth = basic_auth(&self.user, &self.password);
    let client = build_client(self.verify_certificate)?;

    let batch_size = if self.batch { BATCH_SIZE } else { 1 };
    let mut has_error = false;
    for (index, recipients) in self.targets.chunks(batch_size).enumerate().map(|(i, r)| (i * batch_size, r)) {
      // Skip a batch that already went out so a retry does not repeat it
      let key = index.to_string();
      if is_delivered(&key, false) {
        continue;
      }
      payload["recipients"] = json!(recipients);
      let shown = if batch_size == 1 { format!(" to {}", recipients[0]) } else { "(s)".to_string() };

      tracing::debug!("Signal API POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let mut logged = payload.clone();
      if let Some(map) = logged.as_object_mut() {
        map.remove("recipients");
      }
      tracing::debug!("Signal API Payload: {}", crate::utils::sanitize::sanitize_payload(&logged));
      tracing::debug!("Signal API Recipients: {:?}", recipients);

      throttle().await;
      let mut request = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.to_string());
      if let Some((user, password)) = &auth {
        request = request.basic_auth(user, Some(password));
      }
      match request.send().await {
        Ok(resp) if matches!(resp.status().as_u16(), 200 | 201) => {
          tracing::info!("Sent {} Signal API notification{}.", recipients.len(), shown);
        }
        Ok(resp) => {
          let status = resp.status();
          let reason = status.canonical_reason().unwrap_or("");
          tracing::warn!(
            "Failed to send {} Signal API notification{}: {}{}error={}.",
            recipients.len(),
            shown,
            reason,
            if reason.is_empty() { "" } else { ", " },
            status.as_u16()
          );
          tracing::debug!("Response Details:\r\n{}", resp.text().await.unwrap_or_default());
          has_error = true;
          continue;
        }
        Err(e) => {
          tracing::warn!("A Connection error occured sending {} Signal API notification(s).", recipients.len());
          tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
          has_error = true;
          continue;
        }
      }

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
  use crate::types::NotifyType;
  use serde_json::Value;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn signal(url: &str) -> SignalApi {
    SignalApi::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(body: &str, format: NotifyFormat) -> NotifyContext {
    NotifyContext { body: body.into(), body_format: format, ..Default::default() }
  }

  async fn send_server(code: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v2/send")).respond_with(ResponseTemplate::new(code)).mount(&server).await;
    server
  }

  async fn payloads(server: &MockServer) -> Vec<Value> {
    requests_to(server, "POST", "/v2/send").await.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  fn url(server: &MockServer, rest: &str) -> String {
    format!("signal://127.0.0.1:{}/{}", server.address().port(), rest)
  }

  #[test]
  fn test_storage_id_matches_upstream() {
    // uid: lines of upstream's apprise --dry-run; a missing value hashes as "None"
    crate::notify::custom_json::assert_upstream_uids(&[
      ("signal://localhost/11111111111/123/", "5f4c42f0"),
      ("signals://user:pw@localhost:8080/11111111111/", "cf21f8f2"),
    ]);
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["signal://", "signal://:@/", "signal://localhost", "signal://localhost/123"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let (one, two, three, four) = ("1".repeat(11), "2".repeat(11), "3".repeat(11), "4".repeat(11));
    for url in [
      format!("signal://localhost/{}/123/", one),
      format!("signal://localhost:8080/{}/", one),
      format!("signal://localhost:8082/+{}/@group.abcd/", two),
      format!("signals://localhost/{}/{}?format=markdown", one, three),
      format!("signal://localhost:8080/+{}/group.abcd/", one),
      format!("signal://localhost:8080/?from={}&to={},{}", one, two, three),
      format!("signal://localhost:8080/?from={}&to={},{},{}", one, two, three, "5".repeat(3)),
      format!("signal://localhost:8080/{}/{}/?from={}", one, two, three),
      format!("signals://user@localhost/{}/{}", one, three),
      format!("signals://user:password@localhost/{}/{}", one, three),
      format!("signals://localhost/{}/{}/{}?batch=True", one, three, four),
      format!("signals://localhost/{}/{}/{}?status=True", one, three, four),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_targets() {
    let s = signal("signal://localhost:8082/+22222222222/@group.abcd/group.efg/@hij/%40klm!x/12514444444/123");
    assert_eq!(s.source, "+22222222222");
    assert_eq!(s.targets, vec!["group.abcd", "group.efg", "group.hij", "group.klm", "+12514444444"]);
    // No target sends to the source itself
    assert_eq!(signal("signal://localhost/11111111111").targets, vec!["+11111111111"]);
    // ?from= makes every path entry a target
    let s = signal("signal://localhost:8080/22222222222/?from=33333333333&to=+1 (555) 987-5432");
    assert_eq!(s.source, "+33333333333");
    assert_eq!(s.targets, vec!["+22222222222", "+15559875432"]);
    // Only invalid targets leaves none at all
    assert!(signal("signal://localhost/11111111111/123/").targets.is_empty());
  }

  #[test]
  fn test_markdown_dialect() {
    let cases = [
      ("# Alert", "**Alert**"),
      ("**bold** *it* _it_ __bold__", "**bold** *it* *it* **bold**"),
      ("~~gone~~ ~also~", "~gone~ ~also~"),
      ("~~~x~~~ a ~~b~ c~", "\\~\\~\\~x\\~\\~\\~ a \\~\\~b\\~ c\\~"),
      ("~a ~~b a~ b~~", "~a \\~\\~b a~ b\\~\\~"),
      ("a\\_b \\# 2\\*3 \\~ \\| \\&", "a_b # 2\\*3 \\~ \\| &"),
      ("2*3", "2\\*3"),
      ("a\\\nb", "a\nb"),
      ("C:\\temp x\\", "C:\\temp x\\"),
      ("\\\\**b**", "\\\u{200b}**b**"),
      ("**\\*T\\***", "**\\*T\\*\u{200b}**"),
      ("***bi***", "*\u{200b}**bi**\u{200b}*"),
      ("**a*", "\\*\u{200b}*a*"),
      ("`a*b` ``x`y``", "`a\\*b` `x\\`y`"),
      ("```python\nx*y\n```", "`x\\*y`"),
      ("a ` b", "a \\` b"),
      ("[lbl](<https://x.com/~u>)", "lbl (https://x.com/\\~u)"),
      ("[lbl](https://x.com/a\\_b)", "lbl (https://x.com/a_b)"),
      ("[](<https://x.com>)", "https://x.com"),
      ("[a](<b", "[a](<b"),
      ("[a](b c", "[a](b c"),
      ("[a] (b)", "[a] (b)"),
      ("<https://a.b/c~d>", "https://a.b/c\\~d"),
      ("a < b", "a < b"),
      ("<https://a.b/~c", "<https://a.b/\\~c"),
      ("||spoiler||", "||spoiler||"),
      // Backslash and digit sequences
      ("\\2~", "\\2\\~"),
      ("\\1~~", "\\1\\~\\~"),
      ("\\0*", "\\0\\*"),
      ("\\3_", "\\3_"),
      ("a\\12~b~", "a\\12~b~"),
      ("\\0~~a~~", "\\0~a~"),
    ];
    for (markdown, expected) in cases {
      assert_eq!(commonmark_to_signal(markdown), expected, "{:?}", markdown);
    }
    let s = signal("signal://localhost/11111111111");
    assert_eq!(s.dialect_convert("**b**", &NotifyFormat::Markdown), "**b**");
    assert_eq!(s.dialect_convert("2*3", &NotifyFormat::Text), "2*3");
  }

  #[test]
  fn test_unpaired_tildes_are_fast() {
    let start = std::time::Instant::now();
    let result = commonmark_to_signal(&("~~a ".repeat(25000) + &"a~ ".repeat(33333)));
    assert_eq!(result.matches('~').count(), 25000 * 2 + 33333);
    assert!(!result.replace("\\~", "").contains('~'));
    let result = commonmark_to_signal(&("~a ".repeat(33333) + &"a~~ ".repeat(25000)));
    assert_eq!(result.matches('~').count(), 33333 + 25000 * 2);
    assert!(!result.replace("\\~", "").contains('~'));
    assert!(start.elapsed().as_secs_f64() < 10.0);
  }

  #[test]
  fn test_backslash_markers_stay_small() {
    let every: String =
      [(0xE000u32, 0xF8FFu32), (0xF0000, 0xFFFFD), (0x100000, 0x10FFFD)].iter().flat_map(|&(a, b)| (a..=b).filter_map(char::from_u32)).collect();
    let flood = "\u{e000}".repeat(20000);
    for prefix in [every.clone(), flood.clone(), every.clone() + &flood] {
      let body = prefix.clone() + &"\\2~".repeat(20000);
      let start = std::time::Instant::now();
      assert_eq!(commonmark_to_signal(&body), prefix + &"\\2\\~".repeat(20000));
      assert!(start.elapsed().as_secs_f64() < 10.0);
    }
  }

  #[tokio::test]
  async fn test_send_payloads() {
    // Verified against upstream with pycapture.py
    let server = send_server(200).await;
    let s = signal(&format!("signal://user@127.0.0.1:{}/+15551233456/+15559875432/@group.abc/@grp2?status=yes&batch=yes", server.address().port()));
    let mut c = ctx("My Title\r\ntest body  ", NotifyFormat::Text);
    c.notify_type = NotifyType::Warning;
    assert!(s.send(&c).await.unwrap());
    let req = &requests_to(&server, "POST", "/v2/send").await[0];
    assert_eq!(req.headers.get("content-type").unwrap(), "application/json");
    assert_eq!(req.headers.get("user-agent").unwrap(), APP_ID);
    // A user without a password is sent as upstream sends it
    assert_eq!(req.headers.get("authorization").unwrap(), "Basic dXNlcjpOb25l");
    assert_eq!(
      payloads(&server).await,
      vec![json!({
        "message": "[~] My Title\r\ntest body",
        "number": "+15551233456",
        "text_mode": "normal",
        "recipients": ["+15559875432", "group.abc", "group.grp2"],
      })]
    );

    // Markdown is styled, attachments are base64
    let server = send_server(201).await;
    let s = signal(&url(&server, "+15551234567/+15557654321?format=markdown"));
    let mut c = ctx("**T**\n**bold** *it* a_b", NotifyFormat::Markdown);
    c.attachments.push(Attachment { name: "a.gif".into(), data: b"GIF89a".to_vec(), mime_type: "image/gif".into() });
    assert!(s.send(&c).await.unwrap());
    let req = &requests_to(&server, "POST", "/v2/send").await[0];
    assert!(req.headers.get("authorization").is_none());
    assert_eq!(
      payloads(&server).await[0],
      json!({"message": "**T**\n**bold** *it* a_b", "number": "+15551234567", "text_mode": "styled", "recipients": ["+15557654321"], "base64_attachments": ["R0lGODlh"]})
    );
  }

  #[tokio::test]
  async fn test_batches() {
    let server = send_server(200).await;
    let s = signal(&url(&server, "+12512222222/+12513333333/12514444444?batch=no"));
    assert!(s.send(&ctx("My Title\r\ntest body", NotifyFormat::Text)).await.unwrap());
    let posted = payloads(&server).await;
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[0]["recipients"], json!(["+12513333333"]));
    assert_eq!(posted[1]["recipients"], json!(["+12514444444"]));
    assert_eq!(posted[0]["number"], "+12512222222");

    // Batches of ten
    let server = send_server(200).await;
    let targets: Vec<String> = (0..12).map(|i| format!("+1555000{:04}", i)).collect();
    let s = signal(&url(&server, &format!("+15551234567/{}?batch=yes", targets.join("/"))));
    assert!(s.send(&ctx("body", NotifyFormat::Text)).await.unwrap());
    let posted = payloads(&server).await;
    assert_eq!(posted.len(), 2);
    assert_eq!(posted[0]["recipients"].as_array().unwrap().len(), 10);
    assert_eq!(posted[1]["recipients"], json!(["+15550000010", "+15550000011"]));
  }

  #[tokio::test]
  async fn test_failures() {
    for code in [500, 999, 202] {
      let server = send_server(code).await;
      assert!(!signal(&url(&server, "11111111111/44444444444")).send(&ctx("body", NotifyFormat::Text)).await.unwrap());
    }
    assert!(!signal("signal://127.0.0.1:1/11111111111/44444444444").send(&ctx("body", NotifyFormat::Text)).await.unwrap());
    // No valid target
    let server = send_server(200).await;
    assert!(!signal(&url(&server, "11111111111/123/")).send(&ctx("body", NotifyFormat::Text)).await.unwrap());
    assert!(payloads(&server).await.is_empty());
  }

  #[tokio::test]
  async fn test_styled_text_through_apprise() {
    let server = send_server(200).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&url(&server, "+15551234567/+15557654321?format=markdown")));
    let c = NotifyContext { title: "Build *failed*".into(), body: "my_app 2*3 ~user".into(), body_format: NotifyFormat::Text, ..Default::default() };
    assert!(apprise.notify(&c).await.is_success());
    let html = NotifyContext { body: "<b>bold</b> <i>it</i> a_b".into(), body_format: NotifyFormat::Html, ..Default::default() };
    assert!(apprise.notify(&html).await.is_success());
    let posted = payloads(&server).await;
    assert_eq!(posted[0]["text_mode"], "styled");
    assert_eq!(posted[0]["message"], "**Build \\*failed\\*\u{200b}**\nmy_app 2\\*3 \\~user");
    assert_eq!(posted[1]["message"], "**bold** *it* a_b");

    // Without ?format=markdown the text is sent as written
    let server = send_server(200).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&url(&server, "+15551234567/+15557654321")));
    assert!(apprise.notify(&ctx("**bold**", NotifyFormat::Markdown)).await.is_success());
    let posted = payloads(&server).await;
    assert_eq!(posted[0]["text_mode"], "normal");
    assert_eq!(posted[0]["message"], "**bold**");
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_batches() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/v2/send"))
      .and(wiremock::matchers::body_string_contains("+12514444444"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v2/send")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&url(&server, "+12512222222/+12513333333/12514444444?retry=1&wait=0")));
    assert!(apprise.notify(&ctx("body", NotifyFormat::Text)).await.is_success());
    let recipients: Vec<Value> = payloads(&server).await.iter().map(|p| p["recipients"][0].clone()).collect();
    assert_eq!(recipients, vec![json!("+12513333333"), json!("+12514444444"), json!("+12514444444")]);
  }

  #[test]
  fn test_deeply_nested_links() {
    // Upstream 27c4aa5f: deep nesting converts quickly and keeps its text
    let depth = 20000;
    let body = format!("{}x{}", "[".repeat(depth), "](u)".repeat(depth));
    let start = std::time::Instant::now();
    let result = commonmark_to_signal(&body);
    assert!(start.elapsed().as_secs_f64() < 15.0, "took {:?}", start.elapsed());
    // Only the outer labels become links; the rest stay as plain text
    assert_eq!(result.matches("](u)").count(), depth - crate::conversion::MAX_LINK_DEPTH);
  }
}
