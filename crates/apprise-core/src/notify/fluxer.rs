//! Fluxer webhooks (upstream `NotifyFluxer`), a Discord-style webhook API.
//!
//! `https://api.fluxer.app/webhooks/WEBHOOK_ID/WEBHOOK_TOKEN` is written
//! `fluxer://WEBHOOK_ID/WEBHOOK_TOKEN`. A self-hosted server is reached on
//! `/api` unless the URL gives another path:
//! `fluxers://host[:port][/path]/WEBHOOK_ID/WEBHOOK_TOKEN` (`?path=/` for
//! the host root).

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::asset::NotifyImageSize;
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::discord::{extract_markdown_sections, ping_payload};
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base, throttle_for};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_list, qsd_bool, split_path, unquote};
use crate::utils::validate::{is_hostname, is_ipaddr};

/// Embed fields per message (upstream `fluxer_max_fields`).
const MAX_FIELDS: usize = 10;

/// Seconds to wait after a 429 without a usable `Retry-After`, and the
/// shortest wait honoured (upstream `default_delay_sec`).
const DEFAULT_DELAY_SEC: f64 = 1.0;

/// API path of a self-hosted server when the URL gives none (upstream
/// `default_private_notify_path`).
const DEFAULT_PRIVATE_PATH: &str = "/api/";

/// Where the webhooks live (upstream `FluxerMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  /// `https://api.fluxer.app`.
  Cloud,
  /// A self-hosted server.
  Private,
}

/// What a 429 response told us (upstream `ratelimit_remaining` and
/// `ratelimit_reset`).
#[derive(Debug)]
struct RateLimit {
  remaining: f64,
  reset: Instant,
}

pub struct Fluxer {
  webhook_id: String,
  webhook_token: String,
  mode: Mode,
  host: String,
  port: Option<u16>,
  secure: bool,
  /// The API path, wrapped in slashes (`/` in cloud mode).
  fullpath: String,
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
  thread_name: Option<String>,
  /// `?thread=`, `?href=` or `?url=` forced Markdown whatever `?format=` says.
  markdown_implied: bool,
  /// Embeds are only used when the message is sent as Markdown.
  markdown: bool,
  ping: Vec<String>,
  ratelimit: Mutex<RateLimit>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `validate_regex()` for the webhook ID (`^[0-9]{10,}$`).
fn is_webhook_id(value: &str) -> bool {
  value.len() >= 10 && value.chars().all(|c| c.is_ascii_digit())
}

/// Upstream `validate_regex()` for the webhook token (`^[A-Za-z0-9_\-]{16,}$`).
fn is_webhook_token(value: &str) -> bool {
  value.len() >= 16 && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl Fluxer {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().unwrap_or_default();

    // The host and every path entry; the webhook details are the last two
    let mut tokens: Vec<String> = std::iter::once(host.clone()).chain(split_path(url)).collect();

    // Without ?mode=, a valid host followed by the webhook details means a
    // self-hosted server
    let mode = match url.get("mode").filter(|m| !m.is_empty()) {
      Some(m) => match unquote(&m.trim().to_lowercase()).as_str() {
        "cloud" => Mode::Cloud,
        "private" => Mode::Private,
        _ => {
          tracing::warn!("An invalid Fluxer Mode ({}) was specified.", m);
          return None;
        }
      },
      None if (is_hostname(&host) || is_ipaddr(&host)) && tokens.len() > 2 => Mode::Private,
      None => Mode::Cloud,
    };

    let webhook_token = tokens.pop().unwrap_or_default();
    let webhook_id = tokens.pop().unwrap_or_default();
    // YAML `webhook_id:` and `webhook_token:` settings replace what the URL
    // gives (upstream passes them to the constructor); the path and mode
    // stay as the URL sets them
    let webhook_id = url.kwarg("webhook_id").map_or(webhook_id, str::to_string);
    let webhook_token = url.kwarg("webhook_token").map_or(webhook_token, str::to_string);
    if !is_webhook_id(&webhook_id) {
      tracing::warn!("An invalid Fluxer Webhook ID was specified.");
      return None;
    }
    if !is_webhook_token(&webhook_token) {
      tracing::warn!("An invalid Fluxer Webhook Token was specified.");
      return None;
    }

    if host.is_empty() && mode == Mode::Private {
      tracing::warn!("An invalid Fluxer Hostname ({}) was provided.", host);
      return None;
    }
    let mode = if mode == Mode::Private && host.to_lowercase().contains("fluxer.app") {
      tracing::warn!("Fluxer mode changed to cloud mode because fluxer.app found in {}", host);
      Mode::Cloud
    } else {
      mode
    };

    // Path entries between the host and the webhook details; ?path= wins
    let mut path = if tokens.len() <= 1 { String::new() } else { format!("/{}/", tokens[1..].join("/")) };
    if let Some(p) = url.get("path").filter(|p| !p.is_empty()) {
      path = unquote(p);
    }
    let path = path.trim().to_string();
    let path = if mode != Mode::Private {
      if !path.trim_matches('/').is_empty() {
        tracing::warn!("Ignoring the Fluxer path ({}) specified; cloud mode always posts to https://api.fluxer.app", path);
      }
      "/".to_string()
    } else if path.is_empty() {
      DEFAULT_PRIVATE_PATH.to_string()
    } else {
      path
    };
    let mut fullpath = format!("/{}", path.trim_matches('/'));
    if !fullpath.ends_with('/') {
      fullpath.push('/');
    }

    // ?botname= and ?name= set the bot name
    let user = match url.get("botname").or_else(|| url.get("name")) {
      Some(name) => Some(unquote(name)),
      None => url.user.clone(),
    }
    .filter(|u| !u.is_empty());

    if let Some(flags) = url.get("flags").filter(|f| !f.is_empty())
      && !unquote(flags).trim().parse::<i64>().is_ok_and(|v| v >= 0)
    {
      tracing::warn!("An invalid Fluxer flags setting ({}) was specified.", flags);
      return None;
    }

    // ?url= is an alias of ?href=; it and ?thread= imply Markdown
    let href = url.get("href").or_else(|| url.get("url")).map(unquote);
    let thread = url.get("thread").map(unquote);
    let markdown_implied = href.is_some() || thread.is_some();
    let markdown = markdown_implied || url.get("format").and_then(|f| f.parse::<NotifyFormat>().ok()) == Some(NotifyFormat::Markdown);

    Some(Self {
      webhook_id,
      webhook_token,
      mode,
      host,
      port: url.port,
      secure: url.schema == "fluxers",
      fullpath,
      user,
      tts: qsd_bool(url, "tts", false),
      avatar: qsd_bool(url, "avatar", true),
      footer: qsd_bool(url, "footer", false),
      footer_logo: qsd_bool(url, "footer_logo", true),
      include_image: qsd_bool(url, "image", false),
      fields: qsd_bool(url, "fields", true),
      avatar_url: url.get("avatar_url").map(unquote).filter(|a| !a.is_empty()),
      href: href.filter(|h| !h.is_empty()),
      thread_id: thread.filter(|t| !t.is_empty()),
      thread_name: url.get("thread_name").map(unquote).filter(|t| !t.is_empty()),
      markdown_implied,
      markdown,
      ping: url.get("ping").map(|p| parse_list([unquote(p)], true, true)).unwrap_or_default(),
      ratelimit: Mutex::new(RateLimit { remaining: DEFAULT_DELAY_SEC, reset: Instant::now() }),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Fluxer",
      service_url: Some("https://fluxer.app/"),
      setup_url: Some("https://appriseit.com/services/fluxer/"),
      protocols: vec!["fluxer", "fluxers"],
      description: "Send via Fluxer webhooks.",
      attachment_support: true,
    }
  }

  /// The webhook URL (upstream `notify_url` with its `prefix`).
  fn webhook_url(&self) -> String {
    format!("{}/webhooks/{}/{}", self.prefix(), self.webhook_id, self.webhook_token)
  }

  /// [`Self::webhook_url`] with the webhook details masked, for logs.
  fn loggable_url(&self) -> String {
    let mask = |s: &str| format!("{}...{}", &s[..1], &s[s.len() - 1..]);
    format!("{}/webhooks/{}/{}", self.prefix(), mask(&self.webhook_id), mask(&self.webhook_token))
  }

  /// Where the webhooks live: the cloud API, or the self-hosted server and
  /// its API path.
  fn prefix(&self) -> String {
    match self.mode {
      Mode::Cloud => service_base("https://api.fluxer.app"),
      Mode::Private => {
        let schema = if self.secure { "https" } else { "http" };
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        format!("{schema}://{}{port}{}", self.host, self.fullpath.trim_end_matches('/'))
      }
    }
  }

  /// How long to wait before the next request when an earlier 429 has
  /// blocked us.
  fn rate_limit_wait(&self) -> Option<Duration> {
    let mut rl = self.ratelimit.lock().unwrap_or_else(|e| e.into_inner());
    if rl.remaining > 0.0 {
      return None;
    }
    let now = Instant::now();
    if now >= rl.reset {
      // The block has passed
      rl.remaining = 1.0;
      return None;
    }
    Some(rl.reset - now)
  }

  /// Block until `delay` seconds from now.
  fn block_for(&self, delay: f64) {
    let mut rl = self.ratelimit.lock().unwrap_or_else(|e| e.into_inner());
    rl.remaining = 0.0;
    rl.reset = Instant::now() + Duration::try_from_secs_f64(delay).unwrap_or(Duration::from_secs_f64(DEFAULT_DELAY_SEC));
  }

  /// Post one message (upstream `_send()`), with an attachment as the
  /// multipart `files[0]` part beside `payload_json`. A 429 is retried once
  /// after its `Retry-After` delay.
  async fn post(&self, client: &reqwest::Client, payload: &mut Map<String, Value>, attach: Option<&Attachment>) -> bool {
    let url = self.webhook_url();
    tracing::debug!("Fluxer POST URL: {} (cert_verify={})", self.loggable_url(), self.verify_certificate);

    if let Some(a) = attach {
      // Fluxer requires content with an attachment
      payload.insert("content".into(), json!(a.name));
      payload.insert("attachments".into(), json!([{ "id": 0, "filename": a.name }]));
    }
    let body = Value::Object(payload.clone()).to_string();
    tracing::debug!("Fluxer Payload: {}", body);

    let mut rate_limit = 1;
    loop {
      throttle_for(self.rate_limit_wait()).await;

      let mut req = client.post(&url).header("User-Agent", APP_ID);
      if let Some(thread) = &self.thread_id {
        req = req.query(&[("thread_id", thread)]);
      }
      req = match attach {
        None => req.header("Content-Type", "application/json; charset=utf-8").body(body.clone()),
        Some(a) => {
          let part = || reqwest::multipart::Part::bytes(a.data.clone()).file_name(a.name.clone());
          let part = part().mime_str(&a.mime_type).unwrap_or_else(|_| part());
          req.multipart(reqwest::multipart::Form::new().text("payload_json", body.clone()).part("files[0]", part))
        }
      };

      let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Fluxer notification.");
          tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
          return false;
        }
      };

      let status = resp.status().as_u16();
      if status == 429 {
        let delay = resp
          .headers()
          .get("Retry-After")
          .and_then(|v| v.to_str().ok())
          .and_then(|v| v.trim().parse::<f64>().ok())
          .filter(|d| d.is_finite())
          .unwrap_or(DEFAULT_DELAY_SEC)
          .max(DEFAULT_DELAY_SEC);
        self.block_for(delay);
        tracing::warn!("Fluxer rate limiting in effect; blocking for {:.2} second(s)", delay);
        if rate_limit > 0 {
          rate_limit -= 1;
          continue;
        }
        return false;
      }

      if status != 200 && status != 204 {
        let reason = resp.status().canonical_reason().unwrap_or("");
        tracing::warn!("Failed to send Fluxer notification: {}, error={}.", reason, status);
        let details = resp.text().await.unwrap_or_default();
        tracing::debug!("Response Details:\r\n{}", details.chars().take(2000).collect::<String>());
        return false;
      }

      tracing::info!("Sent Fluxer notification.");
      let mut rl = self.ratelimit.lock().unwrap_or_else(|e| e.into_inner());
      rl.reset = Instant::now();
      rl.remaining = DEFAULT_DELAY_SEC;
      return true;
    }
  }
}

#[async_trait]
impl Notify for Fluxer {
  fn schemas(&self) -> &[&str] {
    &["fluxer", "fluxers"]
  }
  fn service_name(&self) -> &str {
    "Fluxer"
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
    if self.markdown_implied { NotifyFormat::Markdown } else { NotifyFormat::Text }
  }
  fn notify_format_overridable(&self) -> bool {
    // ?thread=, ?href= and ?url= replace whatever ?format= asked for
    !self.markdown_implied
  }
  fn body_maxlen(&self) -> usize {
    2000
  }
  fn overflow_amalgamate_title(&self) -> bool {
    true
  }
  fn request_rate_per_sec(&self) -> f64 {
    // Fluxer's Retry-After decides when to wait
    0.0
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    let cloud = self.mode == Mode::Cloud;
    let schema = if cloud || self.secure { "fluxers" } else { "fluxer" };
    let host = if cloud { String::new() } else { self.host.clone() };
    let port = if cloud { String::new() } else { self.port.unwrap_or(if self.secure { 443 } else { 80 }).to_string() };
    let path = if cloud { String::new() } else { self.fullpath.trim_end_matches('/').to_string() };
    Some(vec![schema.into(), host, port, path, self.webhook_id.clone(), self.webhook_token.clone()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let body = ctx.body.as_str();
    let title = ctx.title.as_str();

    let mut payload = Map::new();
    payload.insert("tts".into(), json!(self.tts));
    // Text-to-speech does not wait for the whole message
    payload.insert("wait".into(), json!(!self.tts));

    let image_url = ctx.image_url(NotifyImageSize::Xy256);
    if self.avatar
      && let Some(avatar) = self.avatar_url.clone().or_else(|| image_url.clone())
    {
      payload.insert("avatar_url".into(), json!(avatar));
    }
    if let Some(user) = &self.user {
      payload.insert("username".into(), json!(user));
    }
    if let Some(thread_name) = &self.thread_name {
      payload.insert("thread_name".into(), json!(thread_name));
    }

    // Markdown picks up the body's mentions (ping= adds to them); other
    // formats only ping what ping= lists
    let pings = self.ping.join(" ");
    let found = if self.markdown {
      if self.ping.is_empty() { ping_payload(&[body]) } else { ping_payload(&[body, pings.as_str()]) }
    } else if !self.ping.is_empty() {
      ping_payload(&[pings.as_str()])
    } else {
      Map::new()
    };
    payload.extend(found);

    if !body.is_empty() {
      // Fields that did not fit in the first message
      let mut extra: Vec<Value> = Vec::new();

      if self.markdown {
        let mut embed = Map::new();
        embed.insert("author".into(), json!({ "name": ctx.asset.app_id, "url": ctx.asset.app_url }));
        embed.insert("description".into(), json!(body));
        embed.insert("color".into(), json!(ctx.asset.color_int(&ctx.notify_type)));
        // Fluxer strictly validates fields, so an empty title is left out
        if !title.is_empty() {
          embed.insert("title".into(), json!(title));
        }
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
            extra = fields.split_off(fields.len().min(MAX_FIELDS));
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

      // Skip the message if an earlier attempt already posted it
      if !is_delivered("message", false) {
        if !self.post(&client, &mut payload, None).await {
          return Ok(false);
        }
        mark_delivered("message", false);
      }

      if !extra.is_empty() {
        if let Some(embed) = payload.get_mut("embeds").and_then(|e| e.get_mut(0)) {
          embed["description"] = json!("");
        }
        for (n, chunk) in extra.chunks(MAX_FIELDS).enumerate() {
          // Each extra message of fields is tracked on its own
          let key = format!("fields:{}", n * MAX_FIELDS);
          if is_delivered(&key, false) {
            continue;
          }
          if let Some(embed) = payload.get_mut("embeds").and_then(|e| e.get_mut(0)) {
            embed["fields"] = Value::Array(chunk.to_vec());
          }
          if !self.post(&client, &mut payload, None).await {
            return Ok(false);
          }
          mark_delivered(&key, false);
        }
      }
    }

    if !ctx.attachments.is_empty() {
      // Keep the other settings; nothing is read aloud or waited for
      payload.insert("tts".into(), json!(false));
      payload.insert("wait".into(), json!(false));
      payload.remove("embeds");
      payload.remove("allow_mentions");

      // One attachment per message
      for (index, attachment) in ctx.attachments.iter().enumerate() {
        let key = format!("attach:{index}");
        if is_delivered(&key, false) {
          continue;
        }
        tracing::info!("Posting Fluxer Attachment {}", attachment.name);
        if !self.post(&client, &mut payload, Some(attachment)).await {
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
  use crate::Apprise;
  use crate::notify::registry::{from_url, parse_service_url};
  use crate::notify::with_test_base;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, Request, ResponseTemplate};

  const ID: &str = "0000000000";
  const TOKEN: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
  const HOOK: &str = "/webhooks/0000000000/BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
  const IMAGE: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-256x256.png";
  const LOGO: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-logo.png";

  fn fluxer(url: &str) -> Fluxer {
    Fluxer::from_url(&parse_service_url(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  async fn mock(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(HOOK)).respond_with(ResponseTemplate::new(status)).mount(&server).await;
    server
  }

  /// The POST requests the webhook received, in order.
  async fn posts(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default().into_iter().filter(|r| r.method.as_str() == "POST" && r.url.path() == HOOK).collect()
  }

  fn json_body(r: &Request) -> Value {
    serde_json::from_slice(&r.body).unwrap()
  }

  /// The `payload_json` part and the file name of a multipart request.
  fn multipart(r: &Request) -> (Value, String) {
    let text = String::from_utf8_lossy(&r.body).to_string();
    let start = text.find("name=\"payload_json\"").unwrap();
    let json_start = text[start..].find("\r\n\r\n").unwrap() + start + 4;
    let json_end = text[json_start..].find("\r\n--").unwrap() + json_start;
    let file = text.split("name=\"files[0]\"; filename=\"").nth(1).unwrap().split('"').next().unwrap().to_string();
    (serde_json::from_str(&text[json_start..json_end]).unwrap(), file)
  }

  async fn send(url: &str, c: &NotifyContext, server: &MockServer) -> bool {
    let svc = fluxer(url);
    with_test_base(server.uri(), svc.send(c)).await.unwrap()
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "fluxer://".to_string(),
      "fluxer://:@/".to_string(),
      format!("fluxer://{ID}"),
      format!("fluxer://jack@{ID}/{TOKEN}?mode=invalid"),
      format!("fluxer://{ID}/{TOKEN}?flags=-1"),
      format!("fluxer://{ID}/{TOKEN}?flags=invalid"),
      // The ID needs 10 digits, the token 16 characters
      format!("fluxer://123456789/{TOKEN}"),
      format!("fluxer://{ID}/short"),
      format!("fluxer://{ID}/{}!", &TOKEN[..20]),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      format!("fluxer://{ID}/{TOKEN}"),
      format!("fluxer://l2g@{ID}/{TOKEN}"),
      format!("fluxer://api.fluxer.app/{ID}/{TOKEN}?mode=private"),
      format!("fluxer://{ID}/{TOKEN}?format=markdown&footer=Yes&image=Yes&ping=Joe"),
      format!("fluxer://{ID}/{TOKEN}?format=markdown&footer=Yes&image=No&fields=no"),
      format!("fluxer://jack@{ID}/{TOKEN}?mode=private&host=example.ca"),
      format!("fluxer://{ID}/{TOKEN}?mode=private&host=example.ca&name=jack"),
      format!("fluxer://example.ca:123/{ID}/{TOKEN}"),
      format!("fluxers://example.ca/api/{ID}/{TOKEN}"),
      format!("fluxers://example.ca:8443/custom/api/{ID}/{TOKEN}"),
      format!("https://api.fluxer.app/webhooks/{ID}/{TOKEN}"),
      format!("https://api.fluxer.app/v1/webhooks/{ID}/{TOKEN}?footer=yes"),
      format!("https://api.fluxer.app/v1/webhooks/{ID}/{TOKEN}?footer=yes&botname=joe"),
      format!("fluxer://{ID}/{TOKEN}?flags=1"),
      format!("fluxer://{ID}/{TOKEN}?format=markdown&thread=abc123"),
      format!("fluxer://{ID}/{TOKEN}?format=text"),
      format!("fluxer://{ID}/{TOKEN}?hmarkdown=true&ref=http://localhost"),
      format!("fluxer://{ID}/{TOKEN}?markdown=true&url=http://localhost"),
      format!("fluxer://{ID}/{TOKEN}?avatar_url=http://localhost/test.jpg"),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
    assert_eq!(fluxer(&format!("https://api.fluxer.app/v1/webhooks/{ID}/{TOKEN}?footer=yes&botname=joe")).user.as_deref(), Some("joe"));
    assert_eq!(fluxer(&format!("fluxer://{ID}/{TOKEN}?mode=private&host=example.ca&name=jack")).user.as_deref(), Some("jack"));
    // The ping list is split, sorted and made unique
    assert_eq!(fluxer(&format!("fluxer://{ID}/{TOKEN}?ping=@joe,<@321>,<@%26654>,@joe")).ping, vec!["<@&654>", "<@321>", "@joe"]);
  }

  #[test]
  fn test_modes_and_paths() {
    let cases = [
      (format!("fluxer://{ID}/{TOKEN}"), Mode::Cloud, "/", format!("https://api.fluxer.app/webhooks/{ID}/{TOKEN}")),
      (format!("fluxers://example.ca/{ID}/{TOKEN}"), Mode::Private, "/api/", format!("https://example.ca/api/webhooks/{ID}/{TOKEN}")),
      (format!("fluxers://example.ca/api/{ID}/{TOKEN}"), Mode::Private, "/api/", format!("https://example.ca/api/webhooks/{ID}/{TOKEN}")),
      (
        format!("fluxers://example.ca:8443/custom/api/{ID}/{TOKEN}"),
        Mode::Private,
        "/custom/api/",
        format!("https://example.ca:8443/custom/api/webhooks/{ID}/{TOKEN}"),
      ),
      (format!("fluxers://example.ca/{ID}/{TOKEN}?path=/"), Mode::Private, "/", format!("https://example.ca/webhooks/{ID}/{TOKEN}")),
      (format!("fluxers://example.ca/api/{ID}/{TOKEN}?path=/other"), Mode::Private, "/other/", format!("https://example.ca/other/webhooks/{ID}/{TOKEN}")),
      (format!("fluxer://example.ca:123/{ID}/{TOKEN}"), Mode::Private, "/api/", format!("http://example.ca:123/api/webhooks/{ID}/{TOKEN}")),
      // A fluxer.app host forces cloud mode and drops its path
      (format!("fluxers://api.fluxer.app/anything/{ID}/{TOKEN}?mode=private"), Mode::Cloud, "/", format!("https://api.fluxer.app/webhooks/{ID}/{TOKEN}")),
      // The ID itself is used as the host when private mode is forced
      (format!("fluxer://jack@{ID}/{TOKEN}?mode=private"), Mode::Private, "/api/", format!("http://{ID}/api/webhooks/{ID}/{TOKEN}")),
      (format!("https://api.fluxer.app/v1/webhooks/{ID}/{TOKEN}"), Mode::Cloud, "/", format!("https://api.fluxer.app/webhooks/{ID}/{TOKEN}")),
    ];
    for (url, mode, fullpath, endpoint) in cases {
      let f = fluxer(&url);
      assert_eq!(f.mode, mode, "{}", url);
      assert_eq!(f.fullpath, fullpath, "{}", url);
      assert_eq!(f.webhook_url(), endpoint, "{}", url);
      assert_eq!(f.loggable_url(), endpoint.replace(&format!("{ID}/{TOKEN}"), "0...0/B...B"), "{}", url);
    }

    // Leading and trailing slashes are normalized
    for entry in ["api", "/api", "api/", "/api/"] {
      assert_eq!(fluxer(&format!("fluxers://example.ca/{ID}/{TOKEN}?path={entry}")).fullpath, "/api/");
    }

    // Different paths are different connections; the implicit and
    // explicit /api are the same one
    let id = |url: String| fluxer(&url).url_identifier();
    let under_api = id(format!("fluxers://example.ca/api/{ID}/{TOKEN}"));
    assert_ne!(id(format!("fluxers://example.ca/{ID}/{TOKEN}?path=/")), under_api);
    assert_eq!(id(format!("fluxers://example.ca/{ID}/{TOKEN}")), under_api);
    assert_eq!(under_api, Some(vec!["fluxers".into(), "example.ca".into(), "443".into(), "/api".into(), ID.into(), TOKEN.into()]));
    assert_eq!(id(format!("fluxer://{ID}/{TOKEN}")), Some(vec!["fluxers".into(), "".into(), "".into(), "".into(), ID.into(), TOKEN.into()]));
  }

  #[test]
  fn test_details_and_limits() {
    let svc = from_url(&format!("fluxer://{ID}/{TOKEN}")).unwrap();
    assert_eq!(svc.body_maxlen(), 2000);
    assert_eq!(svc.title_maxlen(), 250);
    assert!(svc.overflow_amalgamate_title());
    assert_eq!(svc.request_rate_per_sec(), 0.0);
    assert!(svc.attachment_support());
    assert_eq!(svc.notify_format(), NotifyFormat::Text);
    // ?thread= forces Markdown over ?format=
    let svc = from_url(&format!("fluxer://{ID}/{TOKEN}?format=text&thread=1")).unwrap();
    assert_eq!(svc.notify_format(), NotifyFormat::Markdown);
    assert!(!svc.notify_format_overridable());
  }

  #[tokio::test]
  async fn test_send_text() {
    // Matches pycapture.py and the apprise-go fluxer golden
    let server = mock(204).await;
    assert!(send(&format!("fluxer://bot@{ID}/{TOKEN}?tts=yes&flags=4"), &ctx("t", "body"), &server).await);
    let reqs = posts(&server).await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].headers.get("content-type").unwrap(), "application/json; charset=utf-8");
    assert_eq!(json_body(&reqs[0]), json!({ "tts": true, "wait": false, "avatar_url": IMAGE, "username": "bot", "content": "t\r\nbody" }));

    let server = mock(200).await;
    assert!(send(&format!("fluxer://{ID}/{TOKEN}?avatar=no"), &ctx("", "body"), &server).await);
    assert_eq!(json_body(&posts(&server).await[0]), json!({ "tts": false, "wait": true, "content": "body" }));
  }

  #[tokio::test]
  async fn test_send_markdown_embed() {
    let server = mock(204).await;
    let url = format!("fluxer://{ID}/{TOKEN}?href=https%3A%2F%2Fexample.com&footer=yes&image=yes&thread=999&thread_name=chat");
    let c = NotifyContext { notify_type: crate::types::NotifyType::Info, ..ctx("t", "body") };
    assert!(send(&url, &c, &server).await);
    let reqs = posts(&server).await;
    assert_eq!(reqs[0].url.query(), Some("thread_id=999"));
    assert_eq!(
      json_body(&reqs[0]),
      json!({
        "tts": false, "wait": true, "avatar_url": IMAGE, "thread_name": "chat",
        "embeds": [{
          "author": { "name": "Apprise", "url": "https://github.com/caronc/apprise" },
          "description": "body", "color": 3843043, "title": "t", "url": "https://example.com",
          "footer": { "text": "Apprise Notifications", "icon_url": LOGO },
          "thumbnail": { "url": IMAGE, "height": 256, "width": 256 },
        }],
      })
    );

    // An empty title is left out of the embed, and there is no content
    let server = mock(204).await;
    assert!(send(&format!("fluxer://{ID}/{TOKEN}?format=markdown"), &ctx("", "Hello world"), &server).await);
    let payload = json_body(&posts(&server).await[0]);
    assert!(payload["embeds"][0].get("title").is_none());
    assert!(payload.get("content").is_none() && payload.get("allow_mentions").is_none());
  }

  #[tokio::test]
  async fn test_pings() {
    let mentions = |p: &Value, key: &str| {
      let mut v: Vec<String> = p["allow_mentions"][key].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
      v.sort();
      v
    };
    let body = "\n    # Heading\n    @everyone and @admin, wake and meet our new user <@123>; <@&456>\"\n    ";

    // Markdown finds mentions in the body
    let server = mock(200).await;
    assert!(send(&format!("fluxer://{ID}/{TOKEN}/?format=markdown"), &ctx("", body), &server).await);
    let p = json_body(&posts(&server).await[0]);
    assert_eq!(
      (mentions(&p, "users"), mentions(&p, "roles"), mentions(&p, "parse")),
      (vec!["123".into()], vec!["456".into()], vec!["admin".into(), "everyone".into()])
    );
    assert!(p["content"].as_str().unwrap().starts_with("👉 "));

    // Text does not
    let server = mock(200).await;
    assert!(send(&format!("fluxer://{ID}/{TOKEN}/?format=text"), &ctx("", body), &server).await);
    assert!(json_body(&posts(&server).await[0]).get("allow_mentions").is_none());

    // ping= is used whatever the format; Markdown adds the body's mentions
    for (format, users, roles, parse) in [
      ("text", vec!["321"], vec!["654"], vec!["joe"]),
      ("html", vec!["321"], vec!["654"], vec!["joe"]),
      ("markdown", vec!["123", "321"], vec!["456", "654"], vec!["admin", "everyone", "joe"]),
    ] {
      let server = mock(200).await;
      assert!(send(&format!("fluxer://{ID}/{TOKEN}/?ping=@joe,<@321>,<@%26654>&format={format}"), &ctx("", body), &server).await);
      let p = json_body(&posts(&server).await[0]);
      let s = |v: Vec<&str>| v.into_iter().map(String::from).collect::<Vec<_>>();
      assert_eq!((mentions(&p, "users"), mentions(&p, "roles"), mentions(&p, "parse")), (s(users), s(roles), s(parse)), "{}", format);
      if format != "markdown" {
        let content = p["content"].as_str().unwrap();
        assert!(content.starts_with(body) && content.contains("<@321>") && content.contains("<@&654>") && content.contains("@joe"));
      }
    }
  }

  #[tokio::test]
  async fn test_fields_are_batched() {
    // Ten fields per message; the rest follow with an empty description
    let server = mock(200).await;
    let body: String = (0..25).map(|i| format!("# Heading {i}\nvalue {i}\n")).collect();
    assert!(send(&format!("fluxer://{ID}/{TOKEN}/?format=markdown"), &ctx("title", &body), &server).await);
    let reqs = posts(&server).await;
    assert_eq!(reqs.len(), 3);
    let fields: Vec<usize> = reqs.iter().map(|r| json_body(r)["embeds"][0]["fields"].as_array().unwrap().len()).collect();
    assert_eq!(fields, vec![10, 10, 5]);
    assert_eq!(json_body(&reqs[0])["embeds"][0]["fields"][0], json!({ "name": "Heading 0", "value": "```md\nvalue 0\n```" }));
    assert_eq!(json_body(&reqs[1])["embeds"][0]["description"], "");

    // fields=no keeps the body as the description
    let server = mock(200).await;
    assert!(send(&format!("fluxer://{ID}/{TOKEN}/?format=markdown&fields=no"), &ctx("", "# H1\nv1"), &server).await);
    assert_eq!(json_body(&posts(&server).await[0])["embeds"][0]["description"], "# H1\nv1");
  }

  #[tokio::test]
  async fn test_attachments() {
    let server = mock(200).await;
    let mut c = ctx("title", "body");
    c.attachments = vec![
      Attachment { name: "report.pdf".into(), data: b"PDFDATA".to_vec(), mime_type: "application/pdf".into() },
      Attachment { name: "b.gif".into(), data: b"GIF".to_vec(), mime_type: "image/gif".into() },
    ];
    assert!(send(&format!("fluxer://{ID}/{TOKEN}"), &c, &server).await);
    let reqs = posts(&server).await;
    assert_eq!(reqs.len(), 3);
    assert_eq!(json_body(&reqs[0]), json!({ "tts": false, "wait": true, "avatar_url": IMAGE, "content": "title\r\nbody" }));
    let (payload, file) = multipart(&reqs[1]);
    assert!(reqs[1].headers.get("content-type").unwrap().to_str().unwrap().starts_with("multipart/form-data"));
    assert_eq!(file, "report.pdf");
    assert_eq!(
      payload,
      json!({ "tts": false, "wait": false, "avatar_url": IMAGE, "content": "report.pdf", "attachments": [{ "id": 0, "filename": "report.pdf" }] })
    );
    assert!(String::from_utf8_lossy(&reqs[1].body).contains("Content-Type: application/pdf\r\n\r\nPDFDATA"));
    let (payload, file) = multipart(&reqs[2]);
    assert_eq!(file, "b.gif");
    assert_eq!(payload["content"], "b.gif");

    // Attachments without a body send only the attachments; embeds and
    // mentions are dropped from them
    let server = mock(200).await;
    let mut c = ctx("", "");
    c.attachments = vec![Attachment { name: "a.txt".into(), data: b"x".to_vec(), mime_type: "text/plain".into() }];
    assert!(send(&format!("fluxer://{ID}/{TOKEN}?format=markdown&ping=<@1>"), &c, &server).await);
    let reqs = posts(&server).await;
    assert_eq!(reqs.len(), 1);
    let (payload, _) = multipart(&reqs[0]);
    assert!(payload.get("embeds").is_none() && payload.get("allow_mentions").is_none());
  }

  #[tokio::test]
  async fn test_failures() {
    for status in [500, 999, 201] {
      let server = mock(status).await;
      assert!(!send(&format!("fluxer://{ID}/{TOKEN}"), &ctx("t", "b"), &server).await, "{}", status);
    }
    // A failed message stops before the attachments
    let server = mock(500).await;
    let mut c = ctx("t", "b");
    c.attachments = vec![Attachment { name: "a.txt".into(), data: b"x".to_vec(), mime_type: "text/plain".into() }];
    assert!(!send(&format!("fluxer://{ID}/{TOKEN}"), &c, &server).await);
    assert_eq!(posts(&server).await.len(), 1);

    let svc = fluxer(&format!("fluxer://{ID}/{TOKEN}"));
    assert!(!with_test_base("http://127.0.0.1:1", svc.send(&ctx("t", "b"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_rate_limit_retry() {
    // A 429 is retried once; a second one fails
    for (responses, ok, count) in [(vec![429, 204], true, 2), (vec![429, 429], false, 2)] {
      let server = MockServer::start().await;
      for status in responses {
        Mock::given(method("POST"))
          .and(path(HOOK))
          .respond_with(ResponseTemplate::new(status).append_header("Retry-After", "0"))
          .up_to_n_times(1)
          .mount(&server)
          .await;
      }
      let svc = fluxer(&format!("fluxer://{ID}/{TOKEN}"));
      assert_eq!(with_test_base(server.uri(), svc.send(&ctx("t", "b"))).await.unwrap(), ok);
      assert_eq!(posts(&server).await.len(), count);
    }
  }

  #[test]
  fn test_rate_limit_wait() {
    let f = fluxer(&format!("fluxer://{ID}/{TOKEN}"));
    assert_eq!(f.rate_limit_wait(), None);
    // A block in the past is cleared
    f.ratelimit.lock().unwrap().remaining = 0.0;
    assert_eq!(f.rate_limit_wait(), None);
    assert_eq!(f.ratelimit.lock().unwrap().remaining, 1.0);
    // A block in the future is waited out
    f.block_for(2.0);
    let wait = f.rate_limit_wait().unwrap();
    assert!(wait <= Duration::from_secs(2) && wait > Duration::from_millis(1900));
    // Unusable delays fall back to the default
    f.block_for(f64::MAX);
    assert!(f.rate_limit_wait().unwrap() <= Duration::from_secs(1));
  }

  #[tokio::test]
  async fn test_rate_limit_waits_retry_after() {
    // Retry-After below the minimum still waits a second before the retry
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(HOOK))
      .respond_with(ResponseTemplate::new(429).append_header("Retry-After", "0.25"))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(HOOK)).respond_with(ResponseTemplate::new(204)).mount(&server).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("fluxer://{ID}/{TOKEN}")));
    let start = std::time::Instant::now();
    let result = crate::notify::with_throttling_in_tests(with_test_base(server.uri(), a.notify(&ctx("t", "b")))).await;
    assert!(result.is_success());
    assert!(start.elapsed() >= Duration::from_millis(950));
    assert_eq!(posts(&server).await.len(), 2);
  }

  #[tokio::test]
  async fn test_retry_skips_sent() {
    // Messages arrive; only the png upload fails, so a retry re-posts it alone
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(HOOK))
      .and(|r: &Request| String::from_utf8_lossy(&r.body).contains("filename=\"b.png\""))
      .respond_with(ResponseTemplate::new(500))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(HOOK)).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("fluxer://{ID}/{TOKEN}/?retry=2&wait=0")));
    let mut c = ctx("", "hello");
    c.attachments = vec![
      Attachment { name: "a.gif".into(), data: b"GIF".to_vec(), mime_type: "image/gif".into() },
      Attachment { name: "b.png".into(), data: b"PNG".to_vec(), mime_type: "image/png".into() },
    ];
    assert!(!with_test_base(server.uri(), a.notify(&c)).await.is_success());
    let names: Vec<String> = posts(&server).await.iter().map(|r| if r.body.starts_with(b"{") { "message".to_string() } else { multipart(r).1 }).collect();
    assert_eq!(names, vec!["message", "a.gif", "b.png", "b.png", "b.png"]);
  }

  #[tokio::test]
  async fn test_formats() {
    // HTML input reaches a Markdown embed as Markdown
    let server = mock(200).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("fluxer://{ID}/{TOKEN}/?format=markdown&fields=no")));
    let c = NotifyContext { body: "<b>hello</b> <i>world</i>".into(), body_format: NotifyFormat::Html, ..Default::default() };
    assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
    assert_eq!(json_body(&posts(&server).await[0])["embeds"][0]["description"], "**hello** *world*");

    // Text keeps the title on its own line of the content
    let server = mock(200).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("fluxer://{ID}/{TOKEN}")));
    let c = NotifyContext { title: "T".into(), body: "<b>hello</b>".into(), body_format: NotifyFormat::Html, ..Default::default() };
    assert!(with_test_base(server.uri(), a.notify(&c)).await.is_success());
    assert_eq!(json_body(&posts(&server).await[0])["content"], "T\r\nhello");
  }

  #[tokio::test]
  async fn test_overflow_split_counts_title() {
    let server = mock(200).await;
    let mut a = Apprise::new();
    assert!(a.add(&format!("fluxer://{ID}/{TOKEN}/?overflow=split")));
    let body: String = (0..400).map(|i| format!("line {i:04} of text\r\n")).collect();
    let title = "T".repeat(240);
    assert!(with_test_base(server.uri(), a.notify(&ctx(&title, &body))).await.is_success());
    let reqs = posts(&server).await;
    assert!(reqs.len() > 1);
    for r in reqs {
      assert!(json_body(&r)["content"].as_str().unwrap().chars().count() <= 2002);
    }
  }

  /// YAML `webhook_id:` and `webhook_token:` settings replace what the URL
  /// gives, as upstream's constructor arguments do; the URL still decides
  /// the mode and path (upstream: POST .../webhooks/9876543210987/zyxw...,
  /// uid 18577019; private: POST /sub/webhooks/1234567890123/zyxw...).
  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_yaml_webhook_settings_replace_the_url_values() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(204)).mount(&server).await;
    let doc = format!(
      concat!(
        "urls:\n",
        "  - fluxer://1234567890123/abcdefghijklmnopqrst:\n      webhook_id: '9876543210987'\n      webhook_token: zyxwvutsrqponmlkjihg\n",
        "  - fluxer://{}/sub/1234567890123/abcdefghijklmnopqrst:\n      webhook_token: zyxwvutsrqponmlkjihg\n",
      ),
      server.address()
    );
    let (a, uids) = crate::config::yaml::load_for_test(&doc).await;
    assert_eq!(uids[0], "18577019");
    assert!(with_test_base(server.uri(), a.notify(&ctx("title", "body"))).await.is_success());
    let mut paths: Vec<String> =
      server.received_requests().await.unwrap().iter().filter(|r| r.method.as_str() == "POST").map(|r| r.url.path().to_string()).collect();
    paths.sort();
    assert_eq!(paths, ["/sub/webhooks/1234567890123/zyxwvutsrqponmlkjihg", "/webhooks/9876543210987/zyxwvutsrqponmlkjihg"]);
  }
}
