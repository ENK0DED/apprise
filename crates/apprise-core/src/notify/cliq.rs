//! Zoho Cliq channel and bot messages through a Webhook Token (upstream
//! `NotifyCliq`): `cliq://{token}/#{channel}/@{bot}`, with `?region=` for
//! accounts outside the US data center.

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::asset::NotifyImageSize;
use crate::conversion::{
  commonmark_emphasis_run, commonmark_find_backtick_run, commonmark_index_backtick_runs, commonmark_new_scan_budget, commonmark_pick_emphasis_sentinel,
  commonmark_render_emphasis_markers, commonmark_scan_angle_dest, commonmark_scan_autolink_dest, commonmark_scan_paren_dest,
};
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_bool_or, parse_list, unquote};

/// Each region and the Cliq domain it is reached on (upstream
/// `CLIQ_REGION_DOMAINS`).
const REGION_DOMAINS: &[(&str, &str)] = &[
  ("us", "cliq.zoho.com"),
  ("eu", "cliq.zoho.eu"),
  ("in", "cliq.zoho.in"),
  ("au", "cliq.zoho.com.au"),
  ("cn", "cliq.zoho.com.cn"),
  ("jp", "cliq.zoho.jp"),
  ("sa", "cliq.zoho.sa"),
  ("uk", "cliq.zoho.uk"),
  ("ca", "cliq.zohocloud.ca"),
];

pub struct Cliq {
  token: String,
  region: &'static str,
  company: Option<String>,
  botname: Option<String>,
  include_image: bool,
  broadcast: bool,
  /// Channels (`#name`) and bots (`@name`).
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream's token pattern, `^[A-Z0-9]+(\.[A-Z0-9]+)+$` (any case).
fn is_token(value: &str) -> bool {
  let parts: Vec<&str> = value.split('.').collect();
  parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// A channel or bot name (`[A-Z0-9_.-]+`, any case).
fn is_name(value: &str) -> bool {
  !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Upstream `CLIQ_HTTP_ERROR_MAP`, else the standard reason.
fn status_reason(status: u16) -> &'static str {
  match status {
    400 => "Bad Request - Check the channel or bot name.",
    401 => "Unauthorized - Invalid Webhook Token.",
    404 => "The channel or bot could not be found.",
    429 => "Too many requests were made.",
    _ => reqwest::StatusCode::from_u16(status).ok().and_then(|s| s.canonical_reason()).unwrap_or(""),
  }
}

/// Whether `text` holds `pat` starting at character index `i`.
fn starts_with_at(text: &[char], i: usize, pat: &str) -> bool {
  let mut k = i;
  for c in pat.chars() {
    if text.get(k) != Some(&c) {
      return false;
    }
    k += 1;
  }
  true
}

fn collect(chars: &[char]) -> String {
  chars.iter().collect()
}

/// Upstream `_commonmark_to_cliq`: translate CommonMark to Zoho Cliq
/// formatting.
///
/// ```text
/// CommonMark          Zoho Cliq
/// **bold**            *bold*
/// *italic*            _italic_
/// [label](<url>)      [label](url)
/// <url>               url
/// \*                  *
/// ```
fn commonmark_to_cliq(body: &str) -> String {
  let chars: Vec<char> = body.chars().collect();
  let n = chars.len();
  let mut out = String::with_capacity(body.len());
  let mut delimiters = Vec::new();
  // Possible link-label openings still waiting for their "](".
  let mut open_links = 0usize;
  let backtick_runs = commonmark_index_backtick_runs(&chars);
  let sentinel = commonmark_pick_emphasis_sentinel(body);
  let mut scan_budget = commonmark_new_scan_budget(&chars);

  let mut i = 0;
  while i < n {
    let ch = chars[i];

    // Cliq shows escapes literally, so they are dropped.
    if ch == '\\' && i + 1 < n {
      out.push(chars[i + 1]);
      i += 2;
      continue;
    }

    // Code spans are kept as they are; unmatched backticks are text.
    if ch == '`' {
      let mut j = i;
      while j < n && chars[j] == '`' {
        j += 1;
      }
      let end = commonmark_find_backtick_run(&backtick_runs, j, j - i).map_or(j, |close| close + j - i);
      out.push_str(&collect(&chars[i..end]));
      i = end;
      continue;
    }

    if ch == '[' {
      open_links += 1;
      out.push('[');
      i += 1;
      continue;
    }

    // `[label](<url>)` becomes Cliq's `[label](url)`.
    if starts_with_at(&chars, i, "](<")
      && open_links > 0
      && let Some(close) = commonmark_scan_angle_dest(&chars, i, n, Some(&mut scan_budget))
    {
      // Protect the wrapping parenthesis from the URL.
      let url = collect(&chars[i + 3..close]).replace(')', "%29");
      open_links -= 1;
      out.push_str(&format!("]({url})"));
      i = close + 2;
      continue;
    }

    // Bare destinations are copied without being scanned for emphasis.
    if starts_with_at(&chars, i, "](") && open_links > 0 {
      // Whether or not it finishes a link, this "](" retires its opener.
      open_links -= 1;
      if let Some(close) = commonmark_scan_paren_dest(&chars, i + 1, n, Some(&mut scan_budget)) {
        out.push_str(&collect(&chars[i..=close]));
        i = close + 1;
        continue;
      }
    }

    // A "]" that never forms "](" cannot close a link.
    if ch == ']' && open_links > 0 && !starts_with_at(&chars, i, "](") {
      open_links -= 1;
    }

    // Autolinks lose their brackets; Cliq links the URL itself.
    if ch == '<'
      && let (Some(close), _) = commonmark_scan_autolink_dest(&chars, i, n)
    {
      out.push_str(&collect(&chars[i + 1..close]));
      i = close + 1;
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

impl Cliq {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The token is the host unless ?token= says otherwise
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => unquote(t),
      None => url.host.as_deref().map(unquote).unwrap_or_default(),
    };
    let token = token.trim().to_string();
    if !is_token(&token) {
      tracing::warn!("An invalid Zoho Cliq Webhook Token ({}) was specified.", token);
      return None;
    }

    let region = url.get("region").map(unquote).map(|r| r.trim().to_lowercase()).filter(|r| !r.is_empty()).unwrap_or_else(|| "us".into());
    let Some(&(region, _)) = REGION_DOMAINS.iter().find(|(r, _)| *r == region) else {
      tracing::warn!("An invalid Zoho Cliq region ({}) was specified.", region);
      return None;
    };

    // Users in more than one company pick which one
    let company = match url.get("company").filter(|c| !c.is_empty()).map(unquote) {
      Some(c) if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-')) => Some(c),
      Some(c) => {
        tracing::warn!("An invalid Zoho Cliq Company ID ({}) was specified.", c);
        return None;
      }
      None => None,
    };

    // The bot name sits in front of the token; ?botname= overrides it
    let botname = match url.get("botname").filter(|b| !b.is_empty()) {
      Some(b) => Some(unquote(b)),
      None => url.user.clone(),
    }
    .map(|b| b.trim().to_string())
    .filter(|b| !b.is_empty());

    let mut entries = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      entries.extend(parse_list([to], true, true));
    }
    let mut targets = Vec::new();
    for target in parse_list(entries, true, true) {
      // Bots first, since the "#" of a channel is optional
      if let Some(name) = target.strip_prefix('@').filter(|n| is_name(n)) {
        targets.push(format!("@{name}"));
      } else if let Some(name) = Some(target.strip_prefix('#').unwrap_or(&target)).filter(|n| is_name(n)) {
        targets.push(format!("#{name}"));
      } else {
        tracing::warn!("Dropped invalid Zoho Cliq target ({}) specified.", target);
      }
    }

    Some(Self {
      token,
      region,
      company,
      botname,
      include_image: url.get("image").map(|v| parse_bool_or(Some(v), false)).unwrap_or(true),
      broadcast: url.get("broadcast").map(|v| parse_bool_or(Some(v), false)).unwrap_or(false),
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Zoho Cliq",
      service_url: Some("https://www.zoho.com/cliq/"),
      setup_url: Some("https://appriseit.com/services/cliq/"),
      protocols: vec!["cliq", "zcliq", "zohocliq"],
      description: "Send messages to Zoho Cliq channels and bots with a Webhook Token.",
      attachment_support: false,
    }
  }

  /// The message URL for a `#channel` or `@bot` target.
  fn message_url(&self, target: &str) -> String {
    let domain = REGION_DOMAINS.iter().find(|(r, _)| *r == self.region).map_or("cliq.zoho.com", |(_, d)| *d);
    let company = self.company.as_ref().map(|c| format!("/company/{c}")).unwrap_or_default();
    let kind = if target.starts_with('@') { "bots" } else { "channelsbyname" };
    format!("{}{company}/api/v2/{kind}/{}/message", service_base(&format!("https://{domain}")), &target[1..])
  }
}

#[async_trait]
impl Notify for Cliq {
  fn schemas(&self) -> &[&str] {
    &["cliq", "zcliq", "zohocliq"]
  }
  fn service_name(&self) -> &str {
    "Zoho Cliq"
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
  fn body_maxlen(&self) -> usize {
    5000
  }
  fn title_maxlen(&self) -> usize {
    // The title is shown in a card above the message
    200
  }
  fn dialect_convert(&self, body: &str, body_format: &NotifyFormat) -> String {
    if *body_format != NotifyFormat::Markdown {
      return body.to_string();
    }
    commonmark_to_cliq(body)
  }
  fn has_dialect(&self) -> bool {
    true
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    // The region and company decide which account the token belongs to
    Some(vec!["cliq".into(), self.region.into(), self.company.clone().unwrap_or_else(|| "None".into()), self.token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Zoho Cliq channels or bots to notify.");
      return Ok(false);
    }

    // Cliq needs text, so a title on its own becomes the text; otherwise
    // the title is shown in a card above the message
    let mut payload = Map::new();
    payload.insert("text".into(), json!(if ctx.body.is_empty() { &ctx.title } else { &ctx.body }));
    if !ctx.title.is_empty() && !ctx.body.is_empty() {
      payload.insert("card".into(), json!({ "title": ctx.title, "theme": "modern-inline" }));
    }

    // Cliq needs a name whenever the sender is changed
    let image_url = if self.include_image { ctx.image_url(NotifyImageSize::Xy72) } else { None };
    if self.botname.is_some() || image_url.is_some() {
      let mut bot = Map::new();
      bot.insert("name".into(), json!(self.botname.as_deref().unwrap_or(&ctx.asset.app_id)));
      if let Some(image) = &image_url {
        bot.insert("image".into(), json!(image));
      }
      payload.insert("bot".into(), Value::Object(bot));
    }

    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    for target in &self.targets {
      // Skip a target that already accepted this message
      if is_delivered(target, false) {
        continue;
      }
      let url = self.message_url(target);

      // Bot messages can reach every subscriber instead of just us
      let mut body = payload.clone();
      if target.starts_with('@') && self.broadcast {
        body.insert("broadcast".into(), json!(true));
      }
      let body = Value::Object(body);
      tracing::debug!("Zoho Cliq POST URL: {} (cert_verify={})", url, self.verify_certificate);
      tracing::debug!("Zoho Cliq Payload: {}", body);

      throttle().await;
      let resp = client
        .post(&url)
        .query(&[("zapikey", &self.token)])
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json; charset=utf-8")
        .body(body.to_string())
        .send()
        .await;
      let resp = match resp {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Zoho Cliq notification to {}.", target);
          tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
          has_error = true;
          continue;
        }
      };
      let status = resp.status().as_u16();
      if status != 200 && status != 204 {
        let reason = status_reason(status);
        tracing::warn!("Failed to send Zoho Cliq notification to {}: {}{}error={}.", target, reason, if reason.is_empty() { "" } else { ", " }, status);
        let details = resp.text().await.unwrap_or_default();
        tracing::debug!("Response Details:\r\n{}", details.chars().take(2000).collect::<String>());
        has_error = true;
        continue;
      }
      tracing::info!("Sent Zoho Cliq notification to {}.", target);
      mark_delivered(target, false);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::Apprise;
  use crate::notify::registry::{from_url, parse_service_url};
  use crate::notify::with_test_base;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  const TOKEN: &str = "1001.2798931fdf4905dc5d37806b47d8253a.2d73063dae2ccf4a04810ddb0a1b2c3d";
  const IMAGE_72: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-72x72.png";

  fn cliq(url: &str) -> Cliq {
    Cliq::from_url(&parse_service_url(url).unwrap()).unwrap()
  }

  fn f(body: &str) -> String {
    commonmark_to_cliq(body)
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    server
  }

  /// The POST requests made, in order, as (path, JSON body).
  async fn posts(server: &MockServer) -> Vec<(String, Value)> {
    server
      .received_requests()
      .await
      .unwrap_or_default()
      .into_iter()
      .filter(|r: &Request| r.method.as_str() == "POST" && r.url.path().contains("/api/v2/"))
      .map(|r| (r.url.path().to_string(), serde_json::from_slice(&r.body).unwrap()))
      .collect()
  }

  #[test]
  fn test_urls() {
    for url in [
      "cliq://".to_string(),
      "cliq://invalid!token/general".to_string(),
      "cliq://1001/general".to_string(),
      format!("cliq://{TOKEN}/general?region=mars"),
      format!("cliq://{TOKEN}/general?company=bad!id"),
      format!("https://cliq.zoho.xyz/api/v2/channelsbyname/devops/message?zapikey={TOKEN}"),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {}", url);
    }
    for url in [
      format!("cliq://{TOKEN}"),
      format!("cliq://{TOKEN}/general"),
      format!("zcliq://Alerts@{TOKEN}/%23general/@mybot"),
      format!("zohocliq://?token={TOKEN}&to=general,@mybot&botname=Alerts&region=eu&company=1234&image=no&broadcast=yes"),
      format!("https://cliq.zoho.com/api/v2/channelsbyname/general/message?zapikey={TOKEN}"),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {}", url);
    }

    let c = cliq(&format!("zohocliq://?token={TOKEN}&to=general,@mybot&botname=Alerts&region=EU&company=1234&image=no&broadcast=yes"));
    assert_eq!(c.targets, vec!["@mybot", "#general"]);
    assert_eq!((c.botname.as_deref(), c.region, c.company.as_deref(), c.include_image, c.broadcast), (Some("Alerts"), "eu", Some("1234"), false, true));

    // Defaults
    let c = cliq(&format!("cliq://{TOKEN}"));
    assert_eq!((c.region, c.company.as_deref(), c.include_image, c.broadcast, c.botname.as_deref()), ("us", None, true, false, None));

    // Invalid targets are dropped; a blank bot name is none
    let c = cliq(&format!("cliq://%20%20@{TOKEN}/%23general/ops/@mybot/bad!channel?region=%20EU%20"));
    assert_eq!(c.targets, vec!["#general", "@mybot", "#ops"]);
    assert_eq!((c.botname.as_deref(), c.region), (None, "eu"));
    assert_eq!(cliq(&format!("zcliq://My%20Bot@{TOKEN}/general")).botname.as_deref(), Some("My Bot"));
  }

  #[test]
  fn test_native_urls() {
    let c = cliq(&format!("https://cliq.zoho.com.au/company/777/api/v2/channelsbyname/devops/message?zapikey={TOKEN}"));
    assert_eq!((c.token.as_str(), c.targets.clone(), c.region, c.company.as_deref()), (TOKEN, vec!["#devops".to_string()], "au", Some("777")));
    let c = cliq(&format!("https://cliq.zohocloud.ca/api/v2/bots/abot/message?a=b&zapikey={TOKEN}&c=d"));
    assert_eq!((c.token.as_str(), c.targets.clone(), c.region, c.company.as_deref()), (TOKEN, vec!["@abot".to_string()], "ca", None));
  }

  #[test]
  fn test_details() {
    let c = cliq(&format!("zcliq://My%20Bot@{TOKEN}/general/@mybot?region=jp&company=99"));
    assert_eq!(c.url_identifier(), Some(vec!["cliq".into(), "jp".into(), "99".into(), TOKEN.into()]));
    assert_ne!(cliq(&format!("cliq://{TOKEN}/general?region=eu&company=99")).url_identifier(), c.url_identifier());
    assert_eq!(cliq(&format!("cliq://{TOKEN}")).url_identifier(), Some(vec!["cliq".into(), "us".into(), "None".into(), TOKEN.into()]));
    assert_eq!((c.body_maxlen(), c.title_maxlen(), c.request_rate_per_sec(), c.notify_format()), (5000, 200, 5.5, NotifyFormat::Markdown));
    assert!(!c.attachment_support());
    assert_eq!(c.message_url("#general"), "https://cliq.zoho.jp/company/99/api/v2/channelsbyname/general/message");
    assert_eq!(cliq(&format!("cliq://{TOKEN}")).message_url("@b"), "https://cliq.zoho.com/api/v2/bots/b/message");
    assert_eq!(c.dialect_convert("**x**", &NotifyFormat::Text), "**x**");
    assert_eq!(c.dialect_convert("**x**", &NotifyFormat::Markdown), "*x*");
  }

  #[test]
  fn test_dialect() {
    let cases = [
      ("**bold** and *it*", "*bold* and _it_"),
      ("`c*d` and ```x**y```", "`c*d` and ```x**y```"),
      ("a ` b *c*", "a ` b _c_"),
      ("[a](<https://x.com/(b)>)", "[a](https://x.com/(b%29)"),
      ("[a](https://x.com/b_*c*)", "[a](https://x.com/b_*c*)"),
      ("see <https://y.com/a*b> now", "see https://y.com/a*b now"),
      ("a < b", "a < b"),
      ("[a](<unterminated", "[a](<unterminated"),
      ("[a](has space)", "[a](has space)"),
      ("[a] then (b)", "[a] then (b)"),
      ("5 \\* 3 \\_", "5 * 3 _"),
      (
        "**bold** *italic* [link](<https://example.com>) `**literal**` <https://example.org>",
        "*bold* _italic_ [link](https://example.com) `**literal**` https://example.org",
      ),
    ];
    for (input, expected) in cases {
      assert_eq!(f(input), expected, "input: {:?}", input);
    }
  }

  #[tokio::test]
  async fn test_payload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(query_param("zapikey", TOKEN))
      .and(header("content-type", "application/json; charset=utf-8"))
      .respond_with(ResponseTemplate::new(204))
      .mount(&server)
      .await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("cliq://Alerts@{TOKEN}/general/@mybot?region=eu&company=1234&broadcast=yes")));
    let ctx = NotifyContext {
      title: "Disk & <db01>".into(),
      body: "Usage at **95%**".into(),
      body_format: NotifyFormat::Markdown,
      notify_type: crate::types::NotifyType::Warning,
      ..Default::default()
    };
    assert!(with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
    let image = IMAGE_72.replace("info", "warning");
    let card = json!({ "title": "Disk & <db01>", "theme": "modern-inline" });
    assert_eq!(
      posts(&server).await,
      vec![
        (
          "/company/1234/api/v2/bots/mybot/message".to_string(),
          json!({ "text": "Usage at *95%*", "card": card, "bot": { "name": "Alerts", "image": image }, "broadcast": true })
        ),
        (
          "/company/1234/api/v2/channelsbyname/general/message".to_string(),
          json!({ "text": "Usage at *95%*", "card": card, "bot": { "name": "Alerts", "image": image } })
        ),
      ]
    );

    // Without an image or bot name the sender is left alone
    let server = mock(200).await;
    let svc = cliq(&format!("cliq://{TOKEN}/general?image=no"));
    assert!(with_test_base(server.uri(), svc.send(&NotifyContext { body: "Hello".into(), ..Default::default() })).await.unwrap());
    assert_eq!(posts(&server).await, vec![("/api/v2/channelsbyname/general/message".to_string(), json!({ "text": "Hello" }))]);

    // A title on its own is the text; a bot name alone changes the sender
    let server = mock(200).await;
    let svc = cliq(&format!("cliq://Ops@{TOKEN}/general?image=no"));
    assert!(with_test_base(server.uri(), svc.send(&NotifyContext { title: "Only a title".into(), ..Default::default() })).await.unwrap());
    assert_eq!(posts(&server).await[0].1, json!({ "text": "Only a title", "bot": { "name": "Ops" } }));

    // The image alone uses the Apprise name; broadcast is off by default
    let server = mock(200).await;
    let svc = cliq(&format!("cliq://{TOKEN}/@mybot"));
    assert!(with_test_base(server.uri(), svc.send(&NotifyContext { body: "Hello".into(), ..Default::default() })).await.unwrap());
    assert_eq!(posts(&server).await[0].1, json!({ "text": "Hello", "bot": { "name": "Apprise", "image": IMAGE_72 } }));
  }

  #[tokio::test]
  async fn test_failures() {
    // No targets
    let server = mock(200).await;
    assert!(!with_test_base(server.uri(), cliq(&format!("cliq://{TOKEN}")).send(&NotifyContext { body: "b".into(), ..Default::default() })).await.unwrap());
    assert!(posts(&server).await.is_empty());

    // One target failing does not stop the others
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/v2/channelsbyname/gone/message")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let svc = cliq(&format!("cliq://{TOKEN}/%23gone/%23general"));
    assert!(!with_test_base(server.uri(), svc.send(&NotifyContext { body: "Hello".into(), ..Default::default() })).await.unwrap());
    assert_eq!(posts(&server).await.len(), 2);

    for status in [201, 500, 999] {
      let server = mock(status).await;
      let svc = cliq(&format!("cliq://{TOKEN}/general"));
      assert!(!with_test_base(server.uri(), svc.send(&NotifyContext { body: "b".into(), ..Default::default() })).await.unwrap(), "{}", status);
    }
    let svc = cliq(&format!("cliq://{TOKEN}/general"));
    assert!(!with_test_base("http://127.0.0.1:1", svc.send(&NotifyContext { body: "b".into(), ..Default::default() })).await.unwrap());
  }

  #[tokio::test]
  async fn test_formats() {
    let notify = |body: &'static str, format: NotifyFormat| async move {
      let server = mock(200).await;
      let mut a = Apprise::new();
      assert!(a.add(&format!("cliq://{TOKEN}/general?image=no")));
      let c = NotifyContext { body: body.into(), body_format: format, ..Default::default() };
      assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
      posts(&server).await[0].1["text"].as_str().unwrap().to_string()
    };
    assert_eq!(notify("<b>bold</b> <i>it</i> <a href=\"https://x.com/a_b\">l_ink</a>", NotifyFormat::Html).await, "*bold* _it_ [l_ink](https://x.com/a_b)");
    // Plain text reaches Cliq exactly as typed
    assert_eq!(notify("5 * 3 = a_b <tag> [x](y)", NotifyFormat::Text).await, "5 * 3 = a_b <tag> [x](y)");
    assert_eq!(notify("**literal** &amp; stuff.", NotifyFormat::Text).await, "**literal** &amp; stuff.");
  }

  #[tokio::test]
  async fn test_retry_skips_delivered() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/v2/channelsbyname/b/message")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("cliq://{TOKEN}/a/b?retry=1&wait=0")));
    assert!(with_test_base(server.uri(), a.notify(&NotifyContext { body: "x".into(), ..Default::default() })).await.is_success());
    let paths: Vec<String> = posts(&server).await.into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths, vec!["/api/v2/channelsbyname/a/message", "/api/v2/channelsbyname/b/message", "/api/v2/channelsbyname/b/message"]);
  }
}
