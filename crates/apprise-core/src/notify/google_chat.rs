use async_trait::async_trait;
use serde_json::json;

use crate::conversion::{
  commonmark_emphasis_run, commonmark_escape_link_url, commonmark_find_backtick_run, commonmark_headings_to_bold, commonmark_index_backtick_runs,
  commonmark_new_scan_budget, commonmark_pick_emphasis_sentinel, commonmark_render_emphasis_markers, commonmark_scan_angle_dest, commonmark_scan_autolink_dest,
  commonmark_scan_paren_dest, pystr,
};
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::unquote;
use crate::utils::qsd::validate_token;

/// Google Chat incoming webhooks
/// (`https://chat.googleapis.com/v1/spaces/{workspace}/messages?key=...&token=...`).
pub struct GoogleChat {
  workspace: String,
  webhook_key: String,
  webhook_token: String,
  thread_key: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl GoogleChat {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // gchat://workspace/webhook_key/webhook_token[/thread_key]
    let mut workspace = url.host.clone().unwrap_or_default();
    let mut tokens = url.path_parts.iter();
    let mut webhook_key = tokens.next().cloned().unwrap_or_default();
    let mut webhook_token = tokens.next().cloned().unwrap_or_default();
    let mut thread_key = tokens.next().cloned().unwrap_or_default();

    // Arguments override the path, even when empty
    if let Some(v) = url.get("workspace") {
      workspace = unquote(v);
    }
    if let Some(v) = url.get("key") {
      webhook_key = unquote(v);
    }
    if let Some(v) = url.get("token") {
      webhook_token = unquote(v);
    }
    if let Some(v) = url.get("thread").or_else(|| url.get("threadkey")) {
      thread_key = unquote(v);
    }

    let Some(workspace) = validate_token(&workspace) else {
      tracing::warn!("An invalid Google Chat Workspace was specified.");
      return None;
    };
    let Some(webhook_key) = validate_token(&webhook_key) else {
      tracing::warn!("An invalid Google Chat Webhook Key was specified.");
      return None;
    };
    let Some(webhook_token) = validate_token(&webhook_token) else {
      tracing::warn!("An invalid Google Chat Webhook Token was specified.");
      return None;
    };
    let thread_key = if thread_key.is_empty() {
      None
    } else {
      let Some(key) = validate_token(&thread_key) else {
        tracing::warn!("An invalid Google Chat Thread Key was specified.");
        return None;
      };
      Some(key)
    };

    Some(Self { workspace, webhook_key, webhook_token, thread_key, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Google Chat",
      service_url: Some("https://chat.google.com/"),
      setup_url: Some("https://appriseit.com/services/googlechat/"),
      protocols: vec!["gchat"],
      description: "Send via Google Chat webhooks.",
      attachment_support: false,
    }
  }
}

/// Whether `text` holds `pat` starting at character index `i`.
fn starts_with_at(text: &[char], i: usize, pat: &str) -> bool {
  let mut rest = text.get(i..).unwrap_or_default().iter();
  pat.chars().all(|c| rest.next() == Some(&c))
}

fn collect(chars: &[char]) -> String {
  chars.iter().collect()
}

/// Upstream `_commonmark_to_google_chat`: translate CommonMark to Google
/// Chat formatting.
///
/// ```text
/// CommonMark          Google Chat
/// # heading           *heading* (Chat has no heading syntax)
/// **bold**            *bold*
/// *italic*            _italic_
/// `code`              `code`
/// [label](<url>)      <url|label>
/// &, <, >             HTML entities
/// ```
fn commonmark_to_google_chat(body: &str) -> String {
  // Google Chat represents headings as bold text.
  let headed = commonmark_headings_to_bold(body);
  let body: Vec<char> = headed.chars().collect();

  let mut out = String::with_capacity(headed.len());
  let mut delimiters = Vec::new();
  // Byte offsets in `out` of possible link-label openings, innermost last.
  let mut link_stack: Vec<usize> = Vec::new();

  let n = body.len();
  let mut i = 0;
  let backtick_runs = commonmark_index_backtick_runs(&body);
  let sentinel = commonmark_pick_emphasis_sentinel(&headed);
  let mut scan_budget = commonmark_new_scan_budget(&body);

  while i < n {
    let ch = body[i];

    // Decode CommonMark escapes and protect Chat anchor delimiters.
    if ch == '\\' && i + 1 < n {
      match body[i + 1] {
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '&' => out.push_str("&amp;"),
        // Chat formatting characters stay escaped.
        nxt @ ('*' | '_' | '~' | '`') => {
          out.push('\\');
          out.push(nxt);
        }
        nxt => out.push(nxt),
      }
      i += 2;
      continue;
    }

    // Escape ampersands before Chat interprets them as entities.
    if ch == '&' {
      out.push_str("&amp;");
      i += 1;
      continue;
    }

    // Keep code spans, escaping HTML-sensitive content.
    if ch == '`' {
      let mut j = i;
      while j < n && body[j] == '`' {
        j += 1;
      }
      let run = j - i;
      if let Some(close) = commonmark_find_backtick_run(&backtick_runs, j, run) {
        let content = collect(&body[j..close]).replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let delim = collect(&body[i..j]);
        out.push_str(&delim);
        out.push_str(&content);
        out.push_str(&delim);
        i = close + run;
        continue;
      }
      // Unmatched backticks are literal text.
      out.push_str(&collect(&body[i..j]));
      i = j;
      continue;
    }

    // A possible link-label opening.
    if ch == '[' {
      link_stack.push(out.len());
      out.push('[');
      i += 1;
      continue;
    }

    // A complete `[label](<url>)` becomes `<url|label>`.
    if starts_with_at(&body, i, "](<")
      && !link_stack.is_empty()
      && let Some(close) = commonmark_scan_angle_dest(&body, i, n, Some(&mut scan_budget))
    {
      let url = commonmark_escape_link_url(&collect(&body[i + 3..close]));
      let open = link_stack.pop().unwrap_or_default();
      let text = out[open + 1..].to_string();
      out.truncate(open);
      out.push_str(&format!("<{url}|{text}>"));
      i = close + 2;
      continue;
    }

    // Bare destinations convert without scanning their URL as emphasis.
    if starts_with_at(&body, i, "](") && !link_stack.is_empty() {
      if let Some(close) = commonmark_scan_paren_dest(&body, i + 1, n, Some(&mut scan_budget)) {
        let url = commonmark_escape_link_url(&collect(&body[i + 2..close]));
        let open = link_stack.pop().unwrap_or_default();
        let text = out[open + 1..].to_string();
        out.truncate(open);
        out.push_str(&format!("<{url}|{text}>"));
        i = close + 1;
        continue;
      }
      // This "](" does not finish a link, so retire its opener.
      link_stack.pop();
    }

    // A "]" that never forms "](" cannot close a link; retire the
    // innermost "[" so a later, unrelated "](" cannot reuse it.
    if ch == ']' && !link_stack.is_empty() && !starts_with_at(&body, i, "](") {
      link_stack.pop();
    }

    // Complete autolinks become Chat anchors; other "<" stay literal.
    if ch == '<'
      && let (Some(close), _) = commonmark_scan_autolink_dest(&body, i, n)
    {
      let url = commonmark_escape_link_url(&collect(&body[i + 1..close]));
      out.push_str(&format!("<{url}>"));
      i = close + 1;
      continue;
    }

    if ch == '*' {
      i = commonmark_emphasis_run(&body, i, n, &mut delimiters, &mut out, &sentinel);
      continue;
    }

    out.push(ch);
    i += 1;
  }

  let text = commonmark_render_emphasis_markers(&out, &mut delimiters, ("*", "*"), ("_", "_"), &sentinel);

  // Chat indents nested lists by four spaces where the generated
  // CommonMark uses two, so leading indentation is doubled outside of
  // fenced code blocks.
  let mut result: Vec<String> = Vec::new();
  // The opening fence width; zero when no fence is open.
  let mut fence_run = 0;
  for line in text.split('\n') {
    let stripped = line.trim_start_matches(' ');
    let run = stripped.len() - stripped.trim_start_matches('`').len();
    let mut is_boundary = false;
    if fence_run == 0 {
      if run >= 3 {
        fence_run = run;
        is_boundary = true;
      }
    } else if run >= fence_run && pystr::strip(&stripped[run..]).is_empty() {
      fence_run = 0;
      is_boundary = true;
    }
    if is_boundary || fence_run != 0 {
      result.push(line.to_string());
    } else {
      let spaces = line.len() - stripped.len();
      result.push(format!("{}{}", "  ".repeat(spaces), stripped));
    }
  }
  result.join("\n")
}

#[async_trait]
impl Notify for GoogleChat {
  fn schemas(&self) -> &[&str] {
    &["gchat"]
  }
  fn service_name(&self) -> &str {
    "Google Chat"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn title_maxlen(&self) -> usize {
    // The title is placed in the body
    0
  }
  fn body_maxlen(&self) -> usize {
    4000
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    if *body_format != NotifyFormat::Markdown {
      return body.to_string();
    }
    commonmark_to_google_chat(body)
  }
  fn has_dialect(&self) -> bool {
    true
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["gchat".into(), self.workspace.clone(), self.webhook_key.clone(), self.webhook_token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let url = format!("{}/v1/spaces/{}/messages", service_base("https://chat.googleapis.com"), self.workspace);
    let mut params = vec![("token", self.webhook_token.as_str()), ("key", self.webhook_key.as_str())];
    let mut payload = json!({ "text": ctx.body });
    if let Some(thread_key) = &self.thread_key {
      params.push(("messageReplyOption", "REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD"));
      payload["thread"] = json!({ "thread_key": thread_key });
    }

    tracing::debug!("Google Chat POST URL: {} (cert_verify={})", url, self.verify_certificate);
    tracing::debug!("Google Chat Payload: {}", payload);

    let client = build_client(self.verify_certificate)?;
    throttle().await;
    let resp = client
      .post(&url)
      .query(&params)
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/json; charset=utf-8")
      .body(payload.to_string())
      .send()
      .await?;

    let status = resp.status().as_u16();
    if status == 200 || status == 204 {
      tracing::info!("Sent Google Chat notification.");
      return Ok(true);
    }
    let reason = resp.status().canonical_reason().unwrap_or("");
    tracing::warn!("Failed to send Google Chat notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status);
    Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Apprise;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path, query_param, query_param_is_missing};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn gchat(url: &str) -> GoogleChat {
    GoogleChat::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn f(body: &str) -> String {
    commonmark_to_google_chat(body)
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["gchat://", "gchat://:@/", "gchat://workspace", "gchat://workspace/key/", "gchat://?workspace=&key=k&token=t"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let cases = [
      ("gchat://workspace/key/token", ("workspace", "key", "token", None)),
      ("gchat://?workspace=ws&key=mykey&token=mytoken", ("ws", "mykey", "mytoken", None)),
      ("gchat://?workspace=ws&key=mykey&token=mytoken&thread=abc123", ("ws", "mykey", "mytoken", Some("abc123"))),
      ("gchat://?workspace=ws&key=mykey&token=mytoken&threadKey=abc345", ("ws", "mykey", "mytoken", Some("abc345"))),
      ("gchat://ws/key/token/thread", ("ws", "key", "token", Some("thread"))),
      // Google's own webhook URL
      ("https://chat.googleapis.com/v1/spaces/myworkspace/messages?key=mykey&token=mytoken", ("myworkspace", "mykey", "mytoken", None)),
      (
        "https://chat.googleapis.com/v1/spaces/myworkspace/messages?key=mykey&token=mytoken&threadKey=mythreadkey",
        ("myworkspace", "mykey", "mytoken", Some("mythreadkey")),
      ),
    ];
    for (url, (ws, key, token, thread)) in cases {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
      let parsed = crate::notify::registry::parse_service_url(url).unwrap();
      let g = GoogleChat::from_url(&parsed).unwrap();
      assert_eq!((g.workspace.as_str(), g.webhook_key.as_str(), g.webhook_token.as_str(), g.thread_key.as_deref()), (ws, key, token, thread), "{}", url);
    }
  }

  #[test]
  fn test_details_and_limits() {
    let svc = from_url("gchat://workspace/key/token").unwrap();
    assert_eq!(svc.body_maxlen(), 4000);
    assert_eq!(svc.title_maxlen(), 0);
    assert_eq!(svc.notify_format(), NotifyFormat::Markdown);
    assert_eq!(svc.request_rate_per_sec(), 5.5);
    assert!(!svc.attachment_support());
    let g = gchat("gchat://workspace/key/token/thread");
    assert_eq!(g.url_identifier(), Some(vec!["gchat".into(), "workspace".into(), "key".into(), "token".into()]));
    // Formats other than Markdown are left unchanged
    assert_eq!(g.dialect_convert("*bold*", &NotifyFormat::Html), "*bold*");
  }

  #[test]
  fn test_dialect_conversion() {
    let marker = '\u{e000}';
    let attack = format!("before {marker}0{marker} after");
    let cases: Vec<(String, String)> = vec![
      ("*bold* [click here](<https://example.com/x>)".into(), "_bold_ <https://example.com/x|click here>".into()),
      ("text ``unterminated".into(), "text ``unterminated".into()),
      ("****x".into(), "****x".into()),
      ("\\>literal".into(), "&gt;literal".into()),
      ("\\!text".into(), "!text".into()),
      ("````\n``` nested\n  indented\nlast\n````".into(), "````\n``` nested\n  indented\nlast\n````".into()),
      ("\\*text\\*".into(), "\\*text\\*".into()),
      ("\\_text\\_".into(), "\\_text\\_".into()),
      ("\\~text\\~".into(), "\\~text\\~".into()),
      ("\\`text\\`".into(), "\\`text\\`".into()),
      ("[text](<https://example.com/unterminated".into(), "[text](<https://example.com/unterminated".into()),
      ("**unterminated".into(), "**unterminated".into()),
      ("**".into(), "**".into()),
      (attack.clone(), attack.clone()),
      (format!("*bold* {attack}"), format!("_bold_ {attack}")),
      ("[click here](https://a*b)".into(), "<https://a*b|click here>".into()),
      ("see <https://a*b> now".into(), "see <https://a*b> now".into()),
      ("a < b".into(), "a < b".into()),
      ("[label](has space)".into(), "[label](has space)".into()),
      ("[label](unterminated".into(), "[label](unterminated".into()),
      ("[OUTER [INNER](<badstuff)](https://example.com)".into(), "<https://example.com|OUTER [INNER](<badstuff)>".into()),
      ("[label] and ](https://example.com) end".into(), "[label] and ](https://example.com) end".into()),
      ("# Head\n\n- a\n  - b\n```\n  code\n```".into(), "*Head*\n\n- a\n    - b\n```\n  code\n```".into()),
      ("`a<b & c>`".into(), "`a&lt;b &amp; c&gt;`".into()),
    ];
    for (input, expected) in cases {
      assert_eq!(f(&input), expected, "input: {:?}", input);
    }
  }

  /// Send `body` in `format` through the whole pipeline and return the
  /// `text` of every request made.
  async fn notify_texts(url: &str, title: &str, body: &str, format: NotifyFormat) -> Vec<String> {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/v1/spaces/workspace/messages")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = Apprise::new();
    assert!(a.add(url));
    let ctx = NotifyContext { title: title.into(), body: body.into(), body_format: format, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
    let mut texts = Vec::new();
    for r in server.received_requests().await.unwrap_or_default() {
      if r.method.as_str() == "POST" && r.url.path() == "/v1/spaces/workspace/messages" {
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        texts.push(v["text"].as_str().unwrap().to_string());
      }
    }
    texts
  }

  #[tokio::test]
  async fn test_formats_match_upstream() {
    // pycapture.py for each input format of the same message
    let body = "# Head & <x>\n\n**bold** *it* `a<b`\n\n- a\n  - b\n\n[l](https://e.com/a_b)";
    let cases = [
      (NotifyFormat::Text, "*My Title*\n# Head &amp; <x&gt;\n\n\\*\\*bold\\*\\* \\*it\\* \\`a<b\\`\n\n- a\n    - b\n\n[l](https://e.com/a\\_b)"),
      (NotifyFormat::Markdown, "*My Title*\n*Head &amp; <x>*\n\n*bold* _it_ `a&lt;b`\n\n- a\n    - b\n\n<https://e.com/a_b|l>"),
    ];
    for (format, expected) in cases {
      assert_eq!(notify_texts("gchat://workspace/key/token", "My Title", body, format.clone()).await, vec![expected.to_string()], "{:?}", format);
    }

    let html = [
      ("<b>hello</b> <i>world</i>", "*hello* _world_"),
      ("<a href=\"https://example.com/x\">click here</a>", "<https://example.com/x|click here>"),
      ("<p>2 &lt; 3 &amp; 4</p>", "2 &lt; 3 &amp; 4"),
      ("<a href=\"https://e/x>TAIL\">click</a>", "<https://e/x&gt;TAIL|click>"),
      ("<a href=\"https://e/x|evil\">click</a>", "<https://e/x%7Cevil|click>"),
      ("<code>a &lt; b &amp; c</code>", "`a &lt; b &amp; c`"),
      ("<b><i>x</i></b>", "_*x*_"),
      ("<b>bold <i>italic</i> still bold</b>", "*bold _italic_ still bold*"),
      ("<b>A</b><b>B</b>", "*A****B*"),
    ];
    for (body, expected) in html {
      assert_eq!(notify_texts("gchat://workspace/key/token", "", body, NotifyFormat::Html).await, vec![expected.to_string()], "{}", body);
    }

    // The merged title heading is shown in bold
    assert_eq!(notify_texts("gchat://workspace/key/token", "My Title", "<b>hello</b>", NotifyFormat::Html).await, vec!["*My Title*\n*hello*"]);
    // A title that converts to nothing is dropped
    assert_eq!(notify_texts("gchat://workspace/key/token", "  - ", "<b>hello</b>", NotifyFormat::Html).await, vec!["*hello*"]);
    // Plain text ampersands are escaped once
    assert_eq!(notify_texts("gchat://workspace/key/token", "", "A & B and AT&amp;T", NotifyFormat::Text).await, vec!["A &amp; B and AT&amp;amp;T"]);
  }

  #[tokio::test]
  async fn test_apprise_go_goldens() {
    let unit = "<b>bold</b> and <a href='https://example.com'>link</a> plus <code>code</code>.";
    let expected = format!("*over*\n{}", vec!["*bold* and <https://example.com|link> plus `code`."; 30].join(" "));
    let body = vec![unit; 30].join(" ");
    assert_eq!(notify_texts("gchat://workspace/key/token?overflow=split", "over", &body, NotifyFormat::Html).await, vec![expected]);

    // ?format=html sends HTML as given, with the title in bold
    let unit = "<b>bold</b> and <a href='https://example.com'>a link</a> plus <code>code</code>.";
    let body = vec![unit; 40].join(" ");
    let expected = format!("<b>over</b><br />\r\n{body}");
    assert_eq!(notify_texts("gchat://workspace/key/token?overflow=split&format=html", "over", &body, NotifyFormat::Html).await, vec![expected]);

    let body = "# Heading\n**Bold** [link](https://example.com?a=1&b=2)";
    let expected = "*Title*\n*Heading*\n*Bold* <https://example.com?a=1&amp;b=2|link>";
    assert_eq!(notify_texts("gchat://workspace/key/token", "Title", body, NotifyFormat::Markdown).await, vec![expected]);

    let expected = "_*Title*_\nBody &amp; text";
    assert_eq!(notify_texts("gchat://workspace/key/token", "<i>Title</i>", "<p>Body &amp; text</p>", NotifyFormat::Html).await, vec![expected]);
  }

  #[tokio::test]
  async fn test_dialect_overflow() {
    // Entity escaping makes this body exceed the limit after conversion
    let body = "& ".repeat(10000);
    let texts = notify_texts("gchat://workspace/key/token?overflow=upstream", "", &body, NotifyFormat::Markdown).await;
    assert_eq!(texts.len(), 1);
    assert!(texts[0].chars().count() > 4000);

    let texts = notify_texts("gchat://workspace/key/token?overflow=truncate", "", &body, NotifyFormat::Markdown).await;
    assert_eq!(texts.len(), 1);
    assert!(texts[0].chars().count() <= 4000);

    let texts = notify_texts("gchat://workspace/key/token?overflow=split", "", &body, NotifyFormat::Markdown).await;
    assert!(texts.len() > 1);
    assert!(texts.iter().all(|t| t.chars().count() <= 4000));
  }

  #[tokio::test]
  async fn test_send_request() {
    // Matches pycapture.py and the apprise-go gchat golden
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/v1/spaces/workspace123/messages"))
      .and(query_param("token", "token789"))
      .and(query_param("key", "key456"))
      .and(query_param_is_missing("messageReplyOption"))
      .and(header("content-type", "application/json; charset=utf-8"))
      .and(body_json(json!({ "text": "hello from python" })))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("gchat://workspace123/key456/token789").unwrap();
    let ctx = NotifyContext { body: "hello from python".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_thread() {
    let server = MockServer::builder().start().await;
    Mock::given(method("POST"))
      .and(path("/v1/spaces/workspace123/messages"))
      .and(query_param("token", "token789"))
      .and(query_param("key", "key456"))
      .and(query_param("messageReplyOption", "REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD"))
      .and(body_json(json!({ "text": "title\r\nbody", "thread": { "thread_key": "thread123" } })))
      .respond_with(ResponseTemplate::new(204))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("gchat://workspace123/key456/token789/thread123").unwrap();
    let ctx = NotifyContext { body: "title\r\nbody".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [201, 500, 999] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).and(path("/v1/spaces/workspace/messages")).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let svc = from_url("gchat://workspace/key/token").unwrap();
      let ctx = NotifyContext { body: "b".into(), ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.is_err(), "status {}", status);
    }
  }

  #[tokio::test]
  async fn test_connection_error() {
    let svc = from_url("gchat://workspace/key/token").unwrap();
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(crate::notify::with_test_base("http://127.0.0.1:1", svc.send(&ctx)).await.is_err());
  }
}
