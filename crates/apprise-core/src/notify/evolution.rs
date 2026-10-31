//! Evolution API, a self-hosted WhatsApp gateway (upstream
//! `NotifyEvolution`): `evolution[s]://{apikey}@{host}[:port]/{instance}/{phone}`.

use async_trait::async_trait;
use serde_json::json;

use crate::conversion::{
  commonmark_decode_backslash_escapes, commonmark_emphasis_run, commonmark_find_backtick_run, commonmark_headings_to_bold, commonmark_index_backtick_runs,
  commonmark_new_scan_budget, commonmark_open_link_label, commonmark_pick_emphasis_sentinel, commonmark_render_emphasis_markers, commonmark_scan_angle_dest,
  commonmark_scan_autolink_dest, commonmark_scan_paren_dest, commonmark_take_link_label,
};
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, throttle};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use crate::utils::phone::{is_phone_no, parse_phone_no};
use crate::utils::plugin_args::unquote;
use crate::utils::qsd::validate_token;

pub struct Evolution {
  apikey: String,
  host: String,
  port: Option<u16>,
  secure: bool,
  instance: String,
  /// Phone numbers, digits only.
  phone: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Whether `text` holds `pat` starting at character index `i`.
fn starts_with_at(text: &[char], i: usize, pat: &str) -> bool {
  let mut rest = text.get(i..).unwrap_or_default().iter();
  pat.chars().all(|c| rest.next() == Some(&c))
}

fn collect(chars: &[char]) -> String {
  chars.iter().collect()
}

/// Collapse runs of three or more backticks to two (`` re.sub(r"`{3,}", "``") ``).
fn collapse_backticks(text: &str) -> String {
  let mut out = String::with_capacity(text.len());
  let mut run = 0;
  let flush = |out: &mut String, run: usize| out.push_str(if run >= 3 { "``" } else { &"``"[..run] });
  for c in text.chars() {
    if c == '`' {
      run += 1;
      continue;
    }
    flush(&mut out, run);
    run = 0;
    out.push(c);
  }
  flush(&mut out, run);
  out
}

/// Upstream `_append_whatsapp_link`: replace the innermost buffered link
/// label in `out` with WhatsApp's `label (url)`.
fn append_whatsapp_link(out: &mut String, link_stack: &mut Vec<usize>, raw_url: &str) {
  let url = commonmark_decode_backslash_escapes(raw_url);
  let text = commonmark_take_link_label(out, link_stack);
  // Protect the wrapping parentheses from the URL.
  let safe_url = url.replace(')', "%29");
  if text.is_empty() {
    out.push_str(&safe_url);
  } else {
    out.push_str(&format!("{text} ({safe_url})"));
  }
}

/// Upstream `_commonmark_to_whatsapp`: translate CommonMark to WhatsApp
/// formatting. Whapi.Cloud uses it too: upstream gives `NotifyWhapi` an
/// identical copy.
///
/// ```text
/// CommonMark          WhatsApp
/// # heading           *heading* (WhatsApp has no heading syntax)
/// **bold**            *bold*
/// *italic*            _italic_
/// `code`              ```code```
/// [label](<url>)      label (url)
/// \*                  *
/// ```
pub(crate) fn commonmark_to_whatsapp(body: &str) -> String {
  // Headings become bold; code is left alone.
  let headed = commonmark_headings_to_bold(body);
  let chars: Vec<char> = headed.chars().collect();
  let n = chars.len();
  let mut out = String::with_capacity(headed.len());
  let mut delimiters = Vec::new();
  // Byte offsets in `out` of possible link-label openings, innermost last.
  let mut link_stack: Vec<usize> = Vec::new();
  let backtick_runs = commonmark_index_backtick_runs(&chars);
  let sentinel = commonmark_pick_emphasis_sentinel(&headed);
  let mut scan_budget = commonmark_new_scan_budget(&chars);

  let mut i = 0;
  while i < n {
    let ch = chars[i];

    // WhatsApp does not use escapes, so they are dropped.
    if ch == '\\' && i + 1 < n {
      out.push(chars[i + 1]);
      i += 2;
      continue;
    }

    // Code spans become triple-backtick monospace.
    if ch == '`' {
      let mut j = i;
      while j < n && chars[j] == '`' {
        j += 1;
      }
      let run = j - i;
      if let Some(close) = commonmark_find_backtick_run(&backtick_runs, j, run) {
        // Longer backtick runs inside would close the span early.
        out.push_str("```");
        out.push_str(&collapse_backticks(&collect(&chars[j..close])));
        out.push_str("```");
        i = close + run;
        continue;
      }
      out.push_str(&collect(&chars[i..j]));
      i = j;
      continue;
    }

    // A possible link-label opening; past the depth limit it is plain text.
    if ch == '[' && commonmark_open_link_label(&mut out, &mut link_stack) {
      i += 1;
      continue;
    }

    // `[label](<url>)` becomes `label (url)`.
    if starts_with_at(&chars, i, "](<")
      && !link_stack.is_empty()
      && let Some(close) = commonmark_scan_angle_dest(&chars, i, n, Some(&mut scan_budget))
    {
      append_whatsapp_link(&mut out, &mut link_stack, &collect(&chars[i + 3..close]));
      i = close + 2;
      continue;
    }

    // Bare destinations convert without being scanned for emphasis.
    if starts_with_at(&chars, i, "](") && !link_stack.is_empty() {
      if let Some(close) = commonmark_scan_paren_dest(&chars, i + 1, n, Some(&mut scan_budget)) {
        append_whatsapp_link(&mut out, &mut link_stack, &collect(&chars[i + 2..close]));
        i = close + 1;
        continue;
      }
      // This "](" does not finish a link, so retire its opener.
      link_stack.pop();
    }

    // A "]" that never forms "](" cannot close a link.
    if ch == ']' && !link_stack.is_empty() && !starts_with_at(&chars, i, "](") {
      link_stack.pop();
    }

    // Autolinks lose their brackets; WhatsApp links the URL itself.
    if ch == '<' {
      match commonmark_scan_autolink_dest(&chars, i, n) {
        (Some(close), _) => {
          out.push_str(&collect(&chars[i + 1..close]));
          i = close + 1;
        }
        // Not an autolink: a literal "<"
        (None, false) => {
          out.push(ch);
          i += 1;
        }
        // An unfinished autolink: the rest is kept as it is
        (None, true) => {
          out.push_str(&collect(&chars[i..]));
          i = n;
        }
      }
      continue;
    }

    if ch == '*' {
      i = commonmark_emphasis_run(&chars, i, n, &mut delimiters, &mut out, &sentinel);
      continue;
    }

    out.push(ch);
    i += 1;
  }

  commonmark_render_emphasis_markers(&out, &mut delimiters, ("*", "*"), ("_", "_"), &sentinel)
}

impl Evolution {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream parses with host verification
    let host = url.verified_host()?;

    let Some(apikey) = url.user.as_deref().map(unquote).and_then(|k| validate_token(&k)) else {
      tracing::warn!("An invalid Evolution API key was specified.");
      return None;
    };

    // The first path entry is the instance, the rest are phone numbers
    let mut entries = url.path_parts.clone().into_iter();
    let Some(instance) = entries.next().map(|i| unquote(&i)).and_then(|i| validate_token(&i)) else {
      tracing::warn!("An invalid Evolution API instance name was specified.");
      return None;
    };
    let mut targets: Vec<String> = entries.collect();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.extend(parse_phone_no(&[to]));
    }

    let mut phone = Vec::new();
    for target in parse_phone_no(&targets) {
      match is_phone_no(&target, 10) {
        // Evolution API expects digits only, without a leading "+"
        Some(p) => phone.push(p.full),
        None => tracing::warn!("Dropped invalid Evolution API phone # ({}) specified.", target),
      }
    }
    if phone.is_empty() {
      tracing::warn!("No valid Evolution API phone numbers were specified.");
      return None;
    }

    Some(Self {
      apikey,
      host,
      port: url.port,
      secure: url.schema == "evolutions",
      instance,
      phone,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Evolution API",
      service_url: Some("https://github.com/EvolutionAPI/evolution-api"),
      setup_url: Some("https://appriseit.com/services/evolution/"),
      protocols: vec!["evolution", "evolutions"],
      description: "Send WhatsApp messages through a self-hosted Evolution API server.",
      attachment_support: false,
    }
  }

  fn send_url(&self) -> String {
    let (schema, default_port) = if self.secure { ("https", 443) } else { ("http", 80) };
    let port = self.port.filter(|p| *p != default_port).map(|p| format!(":{p}")).unwrap_or_default();
    format!("{schema}://{}{port}/message/sendText/{}", self.host, self.instance)
  }
}

#[async_trait]
impl Notify for Evolution {
  fn schemas(&self) -> &[&str] {
    &["evolution", "evolutions"]
  }
  fn service_name(&self) -> &str {
    "Evolution API"
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
  fn request_rate_per_sec(&self) -> f64 {
    0.0
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    if *body_format != NotifyFormat::Markdown {
      return body.to_string();
    }
    commonmark_to_whatsapp(body)
  }
  fn has_dialect(&self) -> bool {
    true
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    let schema = if self.secure { "evolutions" } else { "evolution" };
    let port = self.port.map_or_else(|| "None".to_string(), |p| p.to_string());
    Some(vec![schema.into(), self.apikey.clone(), self.host.clone(), port, self.instance.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let url = self.send_url();
    let mut has_error = false;
    for number in &self.phone {
      // Skip a number that already accepted this message
      if is_delivered(number, false) {
        continue;
      }
      let payload = json!({ "number": number, "text": ctx.body });
      tracing::debug!("Evolution API POST URL: {} (cert_verify={})", url, self.verify_certificate);
      tracing::debug!("Evolution API Payload: {}", payload);

      throttle().await;
      let resp = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("apikey", &self.apikey)
        .body(payload.to_string())
        .send()
        .await;
      let resp = match resp {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Evolution API notification to {}.", number);
          tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };
      let status = resp.status().as_u16();
      if status != 200 && status != 201 {
        let reason = resp.status().canonical_reason().unwrap_or("");
        tracing::warn!("Failed to send Evolution API notification to {}: {}{}error={}.", number, reason, if reason.is_empty() { "" } else { ", " }, status);
        let details = resp.text().await.unwrap_or_default();
        tracing::debug!("Response Details:\r\n{}", details.chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent Evolution API notification to {}.", number);
      mark_delivered(number, false);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Apprise;
  use crate::notify::registry::from_url;
  use serde_json::Value;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  fn evolution(url: &str) -> Evolution {
    Evolution::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn f(body: &str) -> String {
    commonmark_to_whatsapp(body)
  }

  /// A server answering every POST with `status`, and the URL host for it.
  async fn mock(status: u16) -> (MockServer, String) {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/message/sendText/inst")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    let host = server.address().to_string();
    (server, host)
  }

  /// The JSON bodies of the messages sent.
  async fn posts(server: &MockServer) -> Vec<Value> {
    server
      .received_requests()
      .await
      .unwrap_or_default()
      .into_iter()
      .filter(|r: &Request| r.method.as_str() == "POST" && r.url.path() == "/message/sendText/inst")
      .map(|r| serde_json::from_slice(&r.body).unwrap())
      .collect()
  }

  #[test]
  fn test_urls() {
    for url in [
      "evolution://",
      "evolution://:@/",
      "evolution://hostname/myinstance/5511999999999",
      "evolution://myapikey@hostname/5511999999999",
      "evolution://myapikey@hostname/myinstance",
      "evolution://myapikey@hostname/myinstance/notaphone",
      "evolution://key@host",
      // The host is verified
      "evolution://myapikey@host!name/myinstance/5511999999999",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    for url in [
      "evolution://myapikey@hostname/myinstance/5511999999999",
      "evolutions://myapikey@hostname/myinstance/5511999999999",
      "evolution://myapikey@hostname:8080/myinstance/5511999999999",
      "evolution://myapikey@hostname/myinstance/5511999999999/5521888888888",
      "evolution://myapikey@hostname/myinstance/5511999999999?to=5521888888888",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
    let e = evolution("evolution://myapikey@hostname/myinstance/+55 (11) 999999999/bad?to=5521888888888");
    assert_eq!(e.phone, vec!["5511999999999", "5521888888888"]);
    assert_eq!(e.instance, "myinstance");
    let e = evolution("evolution://myapikey@hostname/myinstance/+5511999999999/bad?to=+1 555 123 4567,5521888888888");
    assert_eq!(e.phone, vec!["5511999999999", "15551234567", "5521888888888"]);
  }

  #[test]
  fn test_details() {
    let e = evolution("evolutions://key@secure.host:443/inst/5511999999999");
    assert_eq!(e.send_url(), "https://secure.host/message/sendText/inst");
    assert_eq!(evolution("evolution://key@host:8080/inst/5511999999999").send_url(), "http://host:8080/message/sendText/inst");
    assert_eq!(evolution("evolution://key@host:80/inst/5511999999999").send_url(), "http://host/message/sendText/inst");
    assert_eq!(e.url_identifier(), Some(vec!["evolutions".into(), "key".into(), "secure.host".into(), "443".into(), "inst".into()]));
    assert_eq!(evolution("evolution://key@host/inst/5511999999999").url_identifier().unwrap()[3], "None");
    assert_eq!((e.title_maxlen(), e.body_maxlen(), e.request_rate_per_sec(), e.notify_format()), (0, 32768, 0.0, NotifyFormat::Markdown));
    assert!(!e.attachment_support());
    assert_eq!(e.dialect_convert("*bold*", &NotifyFormat::Html), "*bold*");
  }

  #[test]
  fn test_dialect() {
    let marker = '\u{e000}';
    let attack = format!("before {marker}0{marker} after");
    let cases: Vec<(String, String)> = vec![
      ("text ``unterminated".into(), "text ``unterminated".into()),
      ("`` code```here ``".into(), "``` code``here ```".into()),
      ("````` content```` end `````".into(), "``` content`` end ```".into()),
      ("****x".into(), "****x".into()),
      ("**unterminated".into(), "**unterminated".into()),
      ("**".into(), "**".into()),
      (attack.clone(), attack.clone()),
      (format!("*bold* {attack}"), format!("_bold_ {attack}")),
      ("[text](<https://example.com/unterminated".into(), "[text](<https://example.com/unterminated".into()),
      ("[click](<https://e/x\\>y>)".into(), "click (https://e/x>y)".into()),
      ("[](<https://example.com/x>)".into(), "https://example.com/x".into()),
      ("# Heading\nbody".into(), "*Heading*\nbody".into()),
      ("intro\n## Real heading\nmore text".into(), "intro\n*Real heading*\nmore text".into()),
      ("intro\n```\n# not a heading\ncode\n```\nend".into(), "intro\n```\n# not a heading\ncode\n```\nend".into()),
      ("see `# not code` here".into(), "see ```# not code``` here".into()),
      ("`code` end\n# Heading\nmore".into(), "```code``` end\n*Heading*\nmore".into()),
      ("*bold* [click here](<https://example.com/x>)".into(), "_bold_ click here (https://example.com/x)".into()),
      ("[click here](https://a*b)".into(), "click here (https://a*b)".into()),
      ("[label](has space)".into(), "[label](has space)".into()),
      ("[label](unterminated".into(), "[label](unterminated".into()),
      ("see <https://a*b> now".into(), "see https://a*b now".into()),
      ("a < b".into(), "a < b".into()),
      ("see <https://a*b end".into(), "see <https://a*b end".into()),
      ("[OUTER [INNER](<badstuff)](https://example.com)".into(), "OUTER [INNER](<badstuff) (https://example.com)".into()),
      ("[label] and ](https://example.com) end".into(), "[label] and ](https://example.com) end".into()),
      ("# Heading\n**Bold** [link](https://example.com/a_(x)) & `code`".into(), "*Heading*\n*Bold* link (https://example.com/a_(x%29) & ```code```".into()),
    ];
    for (input, expected) in cases {
      assert_eq!(f(&input), expected, "input: {:?}", input);
    }
    assert_eq!(collapse_backticks("a```b````c``d`"), "a``b``c``d`");
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/message/sendText/myinstance"))
      .and(header("apikey", "MYAPIKEY"))
      .and(header("content-type", "application/json"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = evolution(&format!("evolution://MYAPIKEY@{}/myinstance/5511999999999", server.address()));
    let ctx = NotifyContext { body: "Test\r\nHello World".into(), ..Default::default() };
    assert!(svc.send(&ctx).await.unwrap());
    let reqs = server.received_requests().await.unwrap();
    let r = reqs.iter().find(|r| r.url.path() == "/message/sendText/myinstance").unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&r.body).unwrap(), json!({ "number": "5511999999999", "text": "Test\r\nHello World" }));
  }

  #[tokio::test]
  async fn test_multiple_targets_and_failures() {
    let (server, host) = mock(200).await;
    let svc = evolution(&format!("evolution://key@{host}/inst/5511111111111/5522222222222/5533333333333"));
    assert!(svc.send(&NotifyContext { body: "msg".into(), ..Default::default() }).await.unwrap());
    let numbers: Vec<Value> = posts(&server).await.into_iter().map(|p| p["number"].clone()).collect();
    assert_eq!(numbers, vec![json!("5511111111111"), json!("5522222222222"), json!("5533333333333")]);

    // One failing number fails the send, but the others are still tried
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(|r: &Request| String::from_utf8_lossy(&r.body).contains("5511111111111"))
      .respond_with(ResponseTemplate::new(500))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/message/sendText/inst")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let svc = evolution(&format!("evolution://key@{}/inst/5511111111111/5522222222222", server.address()));
    assert!(!svc.send(&NotifyContext { body: "msg".into(), ..Default::default() }).await.unwrap());
    assert_eq!(posts(&server).await.len(), 2);

    for status in [204, 500, 999] {
      let (_server, host) = mock(status).await;
      let svc = evolution(&format!("evolution://key@{host}/inst/5511999999999"));
      assert!(!svc.send(&NotifyContext { body: "msg".into(), ..Default::default() }).await.unwrap(), "{}", status);
    }
    let svc = evolution("evolution://key@127.0.0.1:1/inst/5511999999999");
    assert!(!svc.send(&NotifyContext { body: "msg".into(), ..Default::default() }).await.unwrap());
  }

  /// Send `body` in `format` through the whole pipeline; the texts sent.
  async fn notify_texts(query: &str, title: &str, body: &str, format: NotifyFormat) -> Vec<String> {
    let (server, host) = mock(200).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("evolution://key@{host}/inst/5511999999999{query}")));
    let c = NotifyContext { title: title.into(), body: body.into(), body_format: format, ..Default::default() };
    assert!(a.notify(&c).await.is_success());
    posts(&server).await.into_iter().map(|p| p["text"].as_str().unwrap().to_string()).collect()
  }

  #[tokio::test]
  async fn test_formats() {
    let html = [
      ("<b>hello</b> <i>world</i>", "*hello* _world_"),
      ("<b><i>x</i></b>", "_*x*_"),
      ("<b>bold <i>italic</i> still bold</b>", "*bold _italic_ still bold*"),
      ("<b>A</b><b>B</b>", "*A****B*"),
      ("<code>inline</code>", "```inline```"),
      ("<pre><code>block code</code></pre>", "```\nblock code\n```"),
      ("<a href=\"https://example.com/x\">click here</a>", "click here (https://example.com/x)"),
      ("<p>literal * asterisk</p>", "literal * asterisk"),
    ];
    for (body, expected) in html {
      assert_eq!(notify_texts("", "", body, NotifyFormat::Html).await, vec![expected], "{}", body);
    }
    assert_eq!(notify_texts("", "My Title", "<b>hello</b>", NotifyFormat::Html).await, vec!["*My Title*\n*hello*"]);
    assert_eq!(notify_texts("", "  - ", "<b>hello</b>", NotifyFormat::Html).await, vec!["*hello*"]);
    assert_eq!(
      notify_texts("", "", "*bold* [click here](<https://example.com/x>)", NotifyFormat::Markdown).await,
      vec!["_bold_ click here (https://example.com/x)"]
    );
    // apprise-go evolution golden
    assert_eq!(
      notify_texts("", "Title", "# Heading\n**Bold** [link](https://example.com/a_(x)) & `code`", NotifyFormat::Markdown).await,
      vec!["*Title*\n*Heading*\n*Bold* link (https://example.com/a_(x%29) & ```code```"]
    );
    let unit = "<b>bold</b> and <a href='https://example.com'>link</a> plus <code>code</code>.";
    let expected = format!("*over*\n{}", vec!["*bold* and link (https://example.com) plus ```code```."; 30].join(" "));
    assert_eq!(notify_texts("?overflow=split", "over", &vec![unit; 30].join(" "), NotifyFormat::Html).await, vec![expected]);
  }

  #[tokio::test]
  async fn test_overflow_split_code_fence() {
    // Split code never leaves a dangling WhatsApp fence
    let content = "line.with.dots-and-dashes_under ".repeat(2000);
    let texts = notify_texts("?overflow=split", "", &format!("<pre>{content}</pre>"), NotifyFormat::Html).await;
    assert!(texts.len() >= 2);
    for text in texts {
      assert!(!text.contains("```"));
    }
  }

  #[tokio::test]
  async fn test_retry_skips_delivered() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(|r: &Request| String::from_utf8_lossy(&r.body).contains("5522222222222"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/message/sendText/inst")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("evolution://key@{}/inst/5511111111111/5522222222222?retry=1&wait=0", server.address())));
    assert!(a.notify(&NotifyContext { body: "x".into(), ..Default::default() }).await.is_success());
    let numbers: Vec<String> = posts(&server).await.into_iter().map(|p| p["number"].as_str().unwrap().to_string()).collect();
    assert_eq!(numbers, vec!["5511111111111", "5522222222222", "5522222222222"]);
  }

  #[test]
  fn test_deeply_nested_links() {
    // Upstream 27c4aa5f: deep nesting converts quickly and keeps its text
    let depth = 20000;
    let body = format!("{}x{}", "[".repeat(depth), "](u)".repeat(depth));
    let start = std::time::Instant::now();
    let result = commonmark_to_whatsapp(&body);
    assert!(start.elapsed().as_secs_f64() < 15.0, "took {:?}", start.elapsed());
    // Only the outer labels become links; the rest stay as plain text
    assert_eq!(result.matches("](u)").count(), depth - crate::conversion::MAX_LINK_DEPTH);
  }
}
