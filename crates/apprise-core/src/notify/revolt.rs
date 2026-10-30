use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::asset::NotifyImageSize;
use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::parse_list;
use crate::utils::parse::{ParsedUrl, unquote};

const REVOLT_API: &str = "https://api.revolt.chat";

/// Added to the server's rate limit reset time.
const CLOCK_SKEW: Duration = Duration::from_secs(2);

const TITLE_MAXLEN: usize = 100;

struct RateLimit {
  remaining: i64,
  reset: Instant,
}

pub struct Revolt {
  bot_token: String,
  targets: Vec<String>,
  icon_url: Option<String>,
  link: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
  ratelimit: Mutex<RateLimit>,
  clock_skew: Duration,
}

impl Revolt {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // revolt://{bot_token}/{targets}; ?bot_token= overrides the host
    let bot_token = match url.get("bot_token") {
      Some(t) => unquote(t),
      None => url.host.clone().unwrap_or_default(),
    };
    let bot_token = bot_token.trim().to_string();
    if bot_token.is_empty() {
      tracing::warn!("An invalid Revolt Bot Token was specified.");
      return None;
    }

    let mut raw: Vec<String> = url.path_parts.clone();
    // Upstream's URLBase.parse_list() also unquotes each entry
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      raw.extend(parse_list(&[to]).iter().map(|t| unquote(t)));
    }
    if let Some(channel) = url.get("channel") {
      raw.extend(parse_list(&[channel]).iter().map(|t| unquote(t)));
    }
    let mut targets = Vec::new();
    for target in parse_list(&raw) {
      if target.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        targets.push(target);
      } else {
        tracing::warn!("Dropped invalid Revolt channel ({}) specified.", target);
      }
    }

    Some(Self {
      bot_token,
      targets,
      icon_url: url.get("icon_url").map(unquote),
      link: url.get("url").map(unquote),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
      ratelimit: Mutex::new(RateLimit { remaining: 1, reset: Instant::now() }),
      clock_skew: CLOCK_SKEW,
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Revolt",
      service_url: Some("https://revolt.chat/"),
      setup_url: Some("https://appriseit.com/services/revolt/"),
      protocols: vec!["revolt"],
      description: "Send messages to Revolt channels through a bot.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext) -> Value {
    if ctx.body_format != NotifyFormat::Markdown {
      let content = if ctx.title.is_empty() { ctx.body.clone() } else { format!("{}\n{}", ctx.title, ctx.body) };
      return json!({ "content": content });
    }
    let title = if ctx.title.is_empty() { Value::Null } else { json!(ctx.title.chars().take(TITLE_MAXLEN).collect::<String>()) };
    let mut embed = json!({
      "title": title,
      "description": ctx.body,
      "colour": ctx.notify_type.color_hex(),
      "replies": null,
    });
    if let Some(icon) = self.icon_url.clone().or_else(|| ctx.image_url(NotifyImageSize::Xy256)) {
      embed["icon_url"] = json!(icon);
    }
    if let Some(link) = &self.link {
      embed["url"] = json!(link);
    }
    json!({ "embeds": [embed] })
  }

  /// Post to one channel, retrying once when told to slow down (HTTP 429).
  async fn post(&self, client: &reqwest::Client, channel_id: &str, payload: &str) -> bool {
    let url = format!("{}/channels/{}/messages", service_base(REVOLT_API), channel_id);
    let mut retries = 1;
    loop {
      let now = Instant::now();
      // Wait out an announced rate limit instead of the usual interval
      let wait = {
        let rl = self.ratelimit.lock().unwrap_or_else(|e| e.into_inner());
        (rl.remaining <= 0 && now < rl.reset).then(|| rl.reset - now + self.clock_skew)
      };
      crate::notify::throttle_for(wait).await;

      tracing::debug!("Revolt POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let resp = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("X-Bot-Token", &self.bot_token)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Accept", "application/json; charset=utf-8")
        .body(payload.to_string())
        .send()
        .await;
      let r = match resp {
        Ok(r) => r,
        Err(e) => {
          tracing::warn!("A Connection error occurred posting to Revolt.");
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
          return false;
        }
      };

      let status = r.status().as_u16();
      let header_int = |name: &str| r.headers().get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<i64>().ok());
      if let Some(remaining) = header_int("X-RateLimit-Remaining") {
        let mut rl = self.ratelimit.lock().unwrap_or_else(|e| e.into_inner());
        rl.remaining = remaining;
        if let Some(after_ms) = header_int("X-RateLimit-Reset-After") {
          rl.reset = now + Duration::from_millis(after_ms.max(0) as u64);
        }
      }

      if status == 200 || status == 204 {
        tracing::info!("Sent Revolt notification.");
        return true;
      }

      let reason = r.status().canonical_reason().unwrap_or("");
      tracing::debug!("Response Details:\r\n{}", r.text().await.unwrap_or_default().chars().take(2000).collect::<String>());
      if status == 429 && retries > 0 {
        retries -= 1;
        let reset = self.ratelimit.lock().unwrap_or_else(|e| e.into_inner()).reset;
        tracing::warn!(
          "Revolt request limit reached; instructed to throttle for {:.3}s",
          (reset.saturating_duration_since(now) + self.clock_skew).as_secs_f64()
        );
        continue;
      }
      tracing::warn!("Failed to send to Revolt notification: {}{}error={}.", reason, if reason.is_empty() { "" } else { ", " }, status);
      return false;
    }
  }
}

#[async_trait]
impl Notify for Revolt {
  fn schemas(&self) -> &[&str] {
    &["revolt"]
  }
  fn service_name(&self) -> &str {
    "Revolt"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn notify_format(&self) -> NotifyFormat {
    // Upstream parse_url() selects Markdown (embeds) unless ?format= is given
    NotifyFormat::Markdown
  }
  fn body_maxlen(&self) -> usize {
    2000
  }
  fn title_maxlen(&self) -> usize {
    TITLE_MAXLEN
  }
  fn request_rate_per_sec(&self) -> f64 {
    3.0
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There were not Revolt channels to notify.");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let payload = self.payload(ctx).to_string();
    let mut has_error = false;
    for channel_id in &self.targets {
      // A retry skips channels that already accepted the message
      if crate::delivery::is_delivered(channel_id, false) {
        continue;
      }
      if !self.post(&client, channel_id, &payload).await {
        has_error = true;
        continue;
      }
      crate::delivery::mark_delivered(channel_id, false);
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse_revolt(url: &str) -> Revolt {
    Revolt::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_invalid_urls() {
    let no_token = format!("revolt://?channel={}", "i".repeat(24));
    for url in ["revolt://", "revolt://:@/", no_token.as_str()] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let (bot, chan, i) = ("i".repeat(24), "t".repeat(64), "i".repeat(24));
    let urls = vec![
      // No channel: loads, but cannot notify
      format!("revolt://{}", bot),
      format!("revolt://{}/?channel={}", bot, i),
      format!("revolt://{}/?to={}", bot, i),
      format!("revolt://{}/?channel={},%20", bot, i),
      format!("revolt://{}/{}", bot, chan),
      format!("revolt://_?bot_token={}&channel={}", bot, chan),
      format!("revolt://{}/{}?format=markdown", bot, chan),
      format!("revolt://{}/{}?format=text", bot, chan),
      format!("revolt://{}/{}?url=http://localhost", bot, chan),
      format!("revolt://{}/{}?icon_url=http://localhost/test.jpg", bot, chan),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let r = parse_revolt(&format!("revolt://{}/{}", "A".repeat(24), "B".repeat(64)));
    assert_eq!(r.bot_token, "A".repeat(24));
    assert_eq!(r.targets, vec!["B".repeat(64)]);

    let r = parse_revolt(&format!("revolt://_?bot_token={}&channel={}", "i".repeat(24), "t".repeat(64)));
    assert_eq!(r.bot_token, "i".repeat(24));

    // Channels from the path, ?to= and ?channel= are merged, sorted and
    // invalid ones dropped
    let r = parse_revolt("revolt://bot/zzz/b@d?to=bbb&channel=aaa,ccc&icon_url=http://localhost/i.png&url=http://example.com");
    assert_eq!(r.targets, vec!["aaa", "bbb", "ccc", "zzz"]);
    assert_eq!(r.icon_url.as_deref(), Some("http://localhost/i.png"));
    assert_eq!(r.link.as_deref(), Some("http://example.com"));
  }

  #[test]
  fn test_payloads() {
    let r = parse_revolt("revolt://bot/chan?url=https://ex.com");
    let ctx =
      NotifyContext { title: "T".into(), body: "B".into(), notify_type: NotifyType::Warning, body_format: NotifyFormat::Markdown, ..Default::default() };
    assert_eq!(
      r.payload(&ctx),
      json!({"embeds": [{
        "title": "T",
        "description": "B",
        "colour": "#CACF29",
        "replies": null,
        "icon_url": "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-warning-256x256.png",
        "url": "https://ex.com",
      }]})
    );

    // No title: null in the embed
    let ctx = NotifyContext { body: "B".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
    assert_eq!(parse_revolt("revolt://bot/chan?icon_url=http://x/i.png").payload(&ctx)["embeds"][0]["title"], Value::Null);
    assert_eq!(parse_revolt("revolt://bot/chan?icon_url=http://x/i.png").payload(&ctx)["embeds"][0]["icon_url"], "http://x/i.png");

    // Text format: plain content
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), body_format: NotifyFormat::Text, ..Default::default() };
    assert_eq!(r.payload(&ctx), json!({"content": "T\nB"}));
  }

  #[tokio::test]
  async fn test_send_each_channel() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/channels/chan1/messages"))
      .and(header("X-Bot-Token", "bottoken"))
      .and(header("Content-Type", "application/json; charset=utf-8"))
      .and(header("Accept", "application/json; charset=utf-8"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/channels/chan2/messages")).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;

    let svc = from_url("revolt://bottoken/chan2/chan1").unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), body_format: NotifyFormat::Markdown, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_retries_once_on_429() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/channels/chan/messages"))
      .respond_with(ResponseTemplate::new(429).insert_header("X-RateLimit-Remaining", "0").insert_header("X-RateLimit-Reset-After", "10"))
      .expect(2)
      .mount(&server)
      .await;
    let svc = from_url("revolt://bottoken/chan").unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    // No channels
    assert!(!from_url("revolt://bottoken").unwrap().send(&ctx).await.unwrap());

    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/channels/a/messages")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/channels/b/messages")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let svc = from_url("revolt://bottoken/a/b").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[test]
  fn test_static_details() {
    let details = Revolt::static_details();
    assert_eq!(details.service_name, "Revolt");
    assert!(details.protocols.contains(&"revolt"));
    assert!(!details.attachment_support);
    assert_eq!(parse_revolt("revolt://bot/chan").request_rate_per_sec(), 3.0);
  }

  #[tokio::test]
  async fn test_rate_limit_forces_a_wait() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/channels/chan/messages"))
      .respond_with(ResponseTemplate::new(429).insert_header("X-RateLimit-Remaining", "0").insert_header("X-RateLimit-Reset-After", "300"))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/channels/chan/messages")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut svc = parse_revolt("revolt://bottoken/chan");
    svc.clock_skew = Duration::ZERO;
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    let started = Instant::now();
    // Throttling itself is off; only the announced reset is waited for
    let sent = crate::notify::with_throttling_in_tests(crate::notify::with_throttle(
      0.0,
      Default::default(),
      crate::notify::with_test_base(server.uri(), svc.send(&ctx)),
    ))
    .await;
    assert!(sent.unwrap());
    assert_eq!(crate::notify::custom_json::requests_to(&server, "POST", "/channels/chan/messages").await.len(), 2);
    assert!(started.elapsed() >= Duration::from_millis(250), "{:?}", started.elapsed());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/channels/chan1/messages")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    // The second channel fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path("/channels/chan2/messages"))
      .and(header("X-Bot-Token", "bottoken"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/channels/chan2/messages")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("revolt://bottoken/chan1/chan2?retry=1&wait=0"));
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&NotifyContext { body: "B".into(), ..Default::default() })).await.is_success());
    let sent: Vec<String> =
      crate::notify::custom_json::requests_to(&server, "POST", "/channels/chan2/messages").await.iter().map(|r| r.url.path().to_string()).collect();
    assert_eq!(sent, vec!["/channels/chan2/messages", "/channels/chan2/messages"]);
  }
}
