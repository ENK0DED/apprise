use crate::asset::NotifyImageSize;
use crate::delivery::{is_delivered, mark_delivered};
use crate::error::{NotifyError, loggable_http_error};
use crate::notify::{Notify, NotifyContext, ServiceDetails, build_client, throttle};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_list, qsd_bool, split_path};
use crate::utils::qsd;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

/// How long a resolved channel ID stays cached (upstream
/// `default_cache_expiry_sec`, 20 days).
const CHANNEL_CACHE_EXPIRY: Duration = Duration::from_secs(60 * 60 * 24 * 20);

/// Upstream `MATTERMOST_MODES`, the default first.
const MODES: &[&str] = &["webhook", "bot"];

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
  /// Post to an incoming webhook (`/hooks/{token}`).
  Webhook,
  /// Post through the REST API (`/api/v4/posts`) with a bearer token.
  Bot,
}

/// A notification target: a channel name (`#`) or a channel ID (`+`).
#[derive(Debug, Clone, PartialEq)]
enum Target {
  Channel(String),
  ChannelId(String),
}

impl Target {
  /// The delivery-tracking key; a name and an ID that read the same are
  /// still different targets.
  fn key(&self) -> String {
    match self {
      Self::Channel(name) => format!("#{}", name),
      Self::ChannelId(id) => format!("+{}", id),
    }
  }
}

fn is_channel_char(c: char) -> bool {
  c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Upstream `IS_CHANNEL`: `#name` (or `%23name`).
fn channel_name(target: &str) -> Option<&str> {
  let name = target.strip_prefix('#').or_else(|| target.strip_prefix("%23"))?;
  (!name.is_empty() && name.chars().all(is_channel_char)).then_some(name)
}

/// Upstream `IS_CHANNEL_ID`: `id`, `+id` (or `%2Bid`).
fn channel_id(target: &str) -> Option<&str> {
  let id = target.strip_prefix('+').or_else(|| target.strip_prefix("%2B")).unwrap_or(target);
  (!id.is_empty() && id.chars().all(is_channel_char)).then_some(id)
}

pub struct Mattermost {
  host: String,
  port: Option<u16>,
  user: Option<String>,
  token: String,
  fullpath: String,
  mode: Mode,
  targets: Vec<Target>,
  include_image: bool,
  icon_url: Option<String>,
  secure: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Mattermost {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.verified_host()?;

    // The last path entry is the token; everything before it is the path
    let mut tokens = split_path(url);
    let token = tokens.pop();
    let fullpath = if tokens.is_empty() { String::new() } else { format!("/{}", tokens.join("/")) };

    let mut targets: Vec<String> = Vec::new();
    for key in ["to", "channel", "channels"] {
      if let Some(value) = url.get(key).filter(|v| !v.is_empty()) {
        targets.extend(qsd::parse_list(value).iter().map(|t| qsd::unquote(t)).filter(|t| !t.is_empty()));
      }
    }

    let mut user = url.user.clone();
    let mut mode = url.get("mode").filter(|m| !m.is_empty()).map(qsd::unquote);
    if let Some(team) = url.get("team").filter(|t| !t.is_empty()) {
      user = Some(qsd::unquote(team));
      // A team implies bot mode unless a mode was given
      mode.get_or_insert_with(|| "bot".to_string());
    } else if let Some(botname) = url.get("botname").filter(|b| !b.is_empty()) {
      user = Some(qsd::unquote(botname));
    }
    let user = user.filter(|u| !u.is_empty());

    let mode = match mode.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
      Some(m) => {
        let m = m.to_lowercase();
        match MODES.iter().find(|mode| mode.starts_with(&m)) {
          Some(&"bot") => Mode::Bot,
          Some(_) => Mode::Webhook,
          None => {
            tracing::warn!("The Mattermost mode specified ({}) is invalid.", m);
            return None;
          }
        }
      }
      None => Mode::Webhook,
    };

    let Some(token) = token.as_deref().and_then(qsd::validate_token) else {
      tracing::warn!("An invalid Mattermost Token was specified.");
      return None;
    };

    let mut parsed = Vec::new();
    for target in parse_list(&targets, true, true) {
      if let Some(name) = channel_name(&target) {
        if mode == Mode::Bot && user.is_none() {
          tracing::warn!("Mattermost bot mode requires a team to resolve {}, dropping it.", target);
          continue;
        }
        parsed.push(Target::Channel(name.to_string()));
      } else if let Some(id) = channel_id(&target) {
        parsed.push(if mode == Mode::Webhook { Target::Channel(id.to_string()) } else { Target::ChannelId(id.to_string()) });
      } else {
        tracing::warn!("Dropping invalid Mattermost target {}", target);
      }
    }

    Some(Self {
      host,
      port: url.port,
      user,
      token,
      fullpath: fullpath.trim().to_string(),
      mode,
      targets: parsed,
      include_image: qsd_bool(url, "image", true),
      icon_url: url.get("icon_url").map(qsd::unquote).filter(|i| !i.is_empty()),
      secure: url.schema == "mmosts",
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Mattermost",
      service_url: Some("https://mattermost.com/"),
      setup_url: Some("https://appriseit.com/services/mattermost/"),
      protocols: vec!["mmost", "mmosts"],
      description: "Send via Mattermost webhooks or bot API.",
      attachment_support: true,
    }
  }

  /// `{schema}://{host}{port}{fullpath}` without a trailing slash.
  fn base_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    let port = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    format!("{}://{}{}{}", schema, self.host, port, self.fullpath.trim_end_matches('/'))
  }

  /// Resolve a channel name to its ID through the team (bot mode). Results
  /// are kept in the persistent store.
  async fn channel_lookup(&self, client: &reqwest::Client, channel: &str) -> Option<String> {
    let key = format!("c:{}", channel);
    if let Some(cached) = crate::storage::with_service_store(|s| s.get(&key)).flatten().and_then(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string)) {
      return Some(cached);
    }

    let url = format!("{}/api/v4/teams/name/{}/channels/name/{}", self.base_url(), qsd::quote(self.user.as_deref().unwrap_or(""), ""), qsd::quote(channel, ""));
    tracing::debug!("Mattermost channel lookup URL: {} (cert_verify={})", url, self.verify_certificate);

    throttle().await;
    let resp = match client
      .get(&url)
      .header("User-Agent", crate::notify::APP_ID)
      .header("Accept", "application/json")
      .header("Authorization", format!("Bearer {}", self.token))
      .send()
      .await
    {
      Ok(resp) => resp,
      Err(e) => {
        tracing::warn!("A Connection error occurred performing Mattermost channel lookup for {}.", channel);
        tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
        return None;
      }
    };

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() != 200 {
      tracing::warn!("Mattermost channel lookup failed for {}: {}, error={}.", channel, status.canonical_reason().unwrap_or(""), status.as_u16());
      tracing::debug!("Response Details:\r\n{}", text);
      return None;
    }
    let Ok(data) = serde_json::from_str::<Value>(&text) else {
      tracing::debug!("Mattermost channel lookup response was not JSON:\r\n{}", text);
      return None;
    };
    let id = data.get("id").and_then(Value::as_str).filter(|id| !id.trim().is_empty())?.to_string();

    crate::storage::with_service_store(|s| s.set(&key, json!(id), Some(CHANNEL_CACHE_EXPIRY), true));
    Some(id)
  }

  /// Upload every attachment for `channel_id`; `None` when one fails.
  async fn upload(&self, client: &reqwest::Client, channel_id: &str, ctx: &NotifyContext) -> Option<Vec<Value>> {
    let url = format!("{}/api/v4/files", self.base_url());
    let mut file_ids = Vec::new();
    for (no, attachment) in ctx.attachments.iter().enumerate() {
      let name = if attachment.name.is_empty() { format!("file{:03}.dat", no + 1) } else { attachment.name.clone() };
      tracing::debug!("Posting Mattermost attachment {}", name);

      let part = reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(name.clone());
      let part = part.mime_str(&attachment.mime_type).unwrap_or_else(|_| reqwest::multipart::Part::bytes(attachment.data.clone()).file_name(name.clone()));
      let form = reqwest::multipart::Form::new().text("channel_id", channel_id.to_string()).part("files", part);

      throttle().await;
      let resp = match client
        .post(&url)
        .header("User-Agent", crate::notify::APP_ID)
        .header("Authorization", format!("Bearer {}", self.token))
        .multipart(form)
        .send()
        .await
      {
        Ok(resp) => resp,
        Err(e) => {
          tracing::warn!("A Connection error occurred uploading Mattermost attachment.");
          tracing::debug!("Socket Exception: {}", loggable_http_error(&e));
          return None;
        }
      };

      let status = resp.status();
      let text = resp.text().await.unwrap_or_default();
      if !matches!(status.as_u16(), 200 | 201) {
        tracing::warn!("Failed to upload Mattermost attachment {}: {}, error={}.", name, status.canonical_reason().unwrap_or(""), status.as_u16());
        tracing::debug!("Response Details:\r\n{}", text);
        return None;
      }

      // Upstream: data.get("file_infos", [{}])[0].get("id", "")
      let file_id = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(data)) => match data.get("file_infos") {
          None => Some(Value::Null),
          Some(Value::Array(infos)) => match infos.first() {
            Some(Value::Object(info)) => Some(info.get("id").cloned().unwrap_or(Value::Null)),
            _ => None,
          },
          Some(_) => None,
        },
        _ => None,
      };
      let Some(file_id) = file_id else {
        tracing::warn!("Failed to parse Mattermost file upload response.");
        return None;
      };
      let empty = match &file_id {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
      };
      if empty {
        tracing::warn!("Mattermost file upload returned no file ID.");
        return None;
      }
      file_ids.push(file_id);
    }
    Some(file_ids)
  }
}

#[async_trait]
impl Notify for Mattermost {
  fn schemas(&self) -> &[&str] {
    &["mmost", "mmosts"]
  }
  fn service_name(&self) -> &str {
    "Mattermost"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    4000
  }
  fn title_maxlen(&self) -> usize {
    0
  }
  fn attachment_support(&self) -> bool {
    // Only the bot API takes uploads
    self.mode == Mode::Bot
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.02
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    // Upstream hashes str() of each value, so a missing one is "None"
    let none = || "None".to_string();
    let schema = if self.secure { "mmosts" } else { "mmost" };
    let mode = if self.mode == Mode::Bot { "bot" } else { "webhook" };
    let team = if self.mode == Mode::Bot { self.user.clone().unwrap_or_else(none) } else { none() };
    Some(vec![schema.into(), mode.into(), self.token.clone(), self.host.clone(), self.port.map_or_else(none, |p| p.to_string()), self.fullpath.clone(), team])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.mode == Mode::Bot && self.targets.is_empty() {
      tracing::warn!("Mattermost BOT mode has no valid channels to notify, aborting.");
      return Ok(false);
    }

    let client = build_client(self.verify_certificate)?;
    let mode = if self.mode == Mode::Bot { "bot" } else { "webhook" };
    let (url, expected): (String, &[u16]) = match self.mode {
      Mode::Bot => (format!("{}/api/v4/posts", self.base_url()), &[201, 200]),
      Mode::Webhook => (format!("{}/hooks/{}", self.base_url(), self.token), &[200]),
    };

    // A webhook without targets posts to its default channel
    let targets: Vec<Option<&Target>> =
      if self.mode == Mode::Webhook && self.targets.is_empty() { vec![None] } else { self.targets.iter().map(Some).collect() };

    let mut has_error = false;
    for target in targets {
      let key = target.map(Target::key).unwrap_or_default();
      // Skip a target that already accepted this message
      if is_delivered(&key, false) {
        continue;
      }

      let payload = match self.mode {
        Mode::Bot => {
          let channel_id = match target {
            Some(Target::Channel(name)) => match self.channel_lookup(&client, name).await {
              Some(id) => id,
              None => {
                has_error = true;
                continue;
              }
            },
            Some(Target::ChannelId(id)) => id.clone(),
            None => continue,
          };
          let Some(file_ids) = self.upload(&client, &channel_id, ctx).await else {
            has_error = true;
            continue;
          };
          let mut payload = json!({"channel_id": channel_id, "message": ctx.body});
          if !file_ids.is_empty() {
            payload["file_ids"] = json!(file_ids);
          }
          payload
        }
        Mode::Webhook => {
          let mut payload = json!({"text": ctx.body});
          let image = self.icon_url.clone().or_else(|| if self.include_image { ctx.image_url(NotifyImageSize::Xy72) } else { None });
          if let Some(image) = image {
            payload["icon_url"] = json!(image);
          }
          payload["username"] = json!(self.user.clone().unwrap_or_else(|| ctx.asset.app_id.clone()));
          if let Some(Target::Channel(channel) | Target::ChannelId(channel)) = target {
            payload["channel"] = json!(channel);
          }
          payload
        }
      };
      let shown = match (self.mode, &payload) {
        (Mode::Bot, p) => format!("channel_id {}", p["channel_id"].as_str().unwrap_or("")),
        (Mode::Webhook, p) => format!("channel {}", p["channel"].as_str().unwrap_or("None")),
      };

      tracing::debug!(
        "Mattermost {} POST URL: {} (cert_verify={})",
        mode,
        if self.mode == Mode::Bot { url.as_str() } else { "<webhook>" },
        self.verify_certificate
      );
      tracing::debug!("Mattermost {} Payload: {}", mode, payload);

      throttle().await;
      let mut request = client.post(&url).header("User-Agent", crate::notify::APP_ID).header("Content-Type", "application/json");
      if self.mode == Mode::Bot {
        request = request.header("Authorization", format!("Bearer {}", self.token));
      }
      match request.body(payload.to_string()).send().await {
        Ok(resp) => {
          let status = resp.status();
          if !expected.contains(&status.as_u16()) {
            let text = resp.text().await.unwrap_or_default();
            tracing::warn!("Failed to send Mattermost notification to {}: {}, error={}.", shown, status.canonical_reason().unwrap_or(""), status.as_u16());
            tracing::debug!("Response Details:\r\n{}", text);
            has_error = true;
            continue;
          }
          tracing::info!("Sent Mattermost {} notification to {}.", mode, shown);
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Mattermost {} notification to {}.", mode, shown);
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
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use crate::notify::{APP_ID, Attachment};
  use crate::types::NotifyFormat;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn mm(url: &str) -> Mattermost {
    Mattermost::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  /// The orchestrator folds the title into the body (`title\r\nbody`).
  fn ctx(body: &str) -> NotifyContext {
    NotifyContext { body: body.into(), body_format: NotifyFormat::Text, ..Default::default() }
  }

  fn gif() -> Attachment {
    Attachment { name: "apprise-test.gif".into(), data: b"GIF89a".to_vec(), mime_type: "image/gif".into() }
  }

  fn json_body(req: &wiremock::Request) -> Value {
    serde_json::from_slice(&req.body).unwrap()
  }

  fn port(server: &MockServer) -> u16 {
    server.address().port()
  }

  #[test]
  fn test_storage_id_matches_upstream() {
    // uid: lines of upstream's apprise --dry-run; a missing value hashes as "None"
    crate::notify::custom_json::assert_upstream_uids(&[
      ("mmost://localhost/3ccdd113474722377935511fc85d3dd4", "4eb68637"),
      ("mmosts://localhost:8443/path/3ccdd113474722377935511fc85d3dd4", "760fbd89"),
      ("mmost://localhost/3ccdd113474722377935511fc85d3dd4?mode=bot&channel=abc", "858226a0"),
      ("mmost://team@localhost/3ccdd113474722377935511fc85d3dd4?mode=bot&channel=abc", "b4575e1a"),
    ]);
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "mmost://",
      "mmosts://",
      "mmost://:@/",
      // No token
      "mmosts://localhost",
      "mmost://localhost:invalid-port/3ccdd113474722377935511fc85d3dd4",
      "mmost://localhost/token?mode=invalid",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    for url in [
      "mmost://localhost/3ccdd113474722377935511fc85d3dd4",
      "mmost://localhost/3ccdd113474722377935511fc85d3dd4?icon_url=http://localhost/test.png",
      "mmost://user@localhost/3ccdd113474722377935511fc85d3dd4?channel=test",
      "mmost://user@localhost/3ccdd113474722377935511fc85d3dd4?channels=test",
      "mmost://user@localhost/3ccdd113474722377935511fc85d3dd4?to=test",
      "mmost://localhost/3ccdd113474722377935511fc85d3dd4?to=test&image=True",
      "mmost://localhost/3ccdd113474722377935511fc85d3dd4?to=test&team=chester",
      "mmost://localhost/3ccdd113474722377935511fc85d3dd4?to=general&botname=foobar",
      "mmost://team@localhost/3ccdd113474722377935511fc85d3dd4?channel=$!garbag3^&mode=bot",
      "mmost://localhost/3ccdd113474722377935511fc85d3dd4?to=test&image=False",
      "mmost://localhost:8080/3ccdd113474722377935511fc85d3dd4",
      "mmosts://localhost/3ccdd113474722377935511fc85d3dd4",
      "https://mattermost.example.com/hooks/3ccdd113474722377935511fc85d3dd4",
      "mmosts://localhost/a/path/3ccdd113474722377935511fc85d3dd4",
      "mmosts://localhost/////3ccdd113474722377935511fc85d3dd4///",
      "mmost://localhost/token?mode=w",
      "mmost://localhost/token?mode=b&to=channel-id-1",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_url_fields() {
    let m = mm("mmosts://localhost/a/path/3ccdd113474722377935511fc85d3dd4");
    assert_eq!((m.token.as_str(), m.fullpath.as_str(), m.mode), ("3ccdd113474722377935511fc85d3dd4", "/a/path", Mode::Webhook));
    assert!(m.secure);
    assert!(m.include_image);
    assert_eq!(mm("mmosts://localhost/////tok///").fullpath, "");

    // Webhook targets are channel names, sorted and without duplicates
    let m = mm("mmost://localhost/token?channels=c,%23a,b&to=b");
    assert_eq!(m.targets, vec![Target::Channel("a".into()), Target::Channel("b".into()), Target::Channel("c".into())]);

    // A team implies bot mode; names need a team in bot mode
    let m = mm("mmost://localhost/token?to=test&team=chester");
    assert_eq!((m.mode, m.user.as_deref()), (Mode::Bot, Some("chester")));
    assert_eq!(m.targets, vec![Target::ChannelId("test".into())]);
    let m = mm("mmost://localhost/token?mode=bot&to=%23chan,id1");
    assert_eq!(m.targets, vec![Target::ChannelId("id1".into())]);
    let m = mm("mmost://team@localhost/token?mode=bot&to=%23chan,%2Bid1");
    assert_eq!(m.targets, vec![Target::Channel("chan".into()), Target::ChannelId("id1".into())]);
    // ?mode= wins over the mode a team implies
    assert_eq!(mm("mmost://localhost/token?team=t&mode=webhook").mode, Mode::Webhook);
    // ?botname= sets the webhook user
    let m = mm("mmost://localhost/token?botname=mybot&to=general");
    assert_eq!((m.mode, m.user.as_deref()), (Mode::Webhook, Some("mybot")));
    assert!(mm("mmost://team@localhost/3ccdd113474722377935511fc85d3dd4?channel=$!garbag3^&mode=bot").targets.is_empty());

    assert!(!mm("mmost://localhost/token").attachment_support());
    assert!(mm("mmost://localhost/token?mode=bot").attachment_support());
  }

  #[tokio::test]
  async fn test_webhook_channels() {
    // Verified against upstream with pycapture.py
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/hooks/token")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let m = mm(&format!("mmost://user1@127.0.0.1:{}/token?channels=%23one,two", port(&server)));
    assert!(m.send(&ctx("title\r\nbody")).await.unwrap());

    let posts = requests_to(&server, "POST", "/hooks/token").await;
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].headers.get("content-type").unwrap(), "application/json");
    assert_eq!(posts[0].headers.get("user-agent").unwrap(), APP_ID);
    assert!(posts[0].headers.get("authorization").is_none());
    let icon = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-72x72.png";
    assert_eq!(json_body(&posts[0]), json!({"text": "title\r\nbody", "icon_url": icon, "username": "user1", "channel": "one"}));
    assert_eq!(json_body(&posts[1]), json!({"text": "title\r\nbody", "icon_url": icon, "username": "user1", "channel": "two"}));
  }

  #[tokio::test]
  async fn test_webhook_payload_variants() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/a/path/hooks/token")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let base = format!("mmost://127.0.0.1:{}/a/path/token", port(&server));

    // No user, no channel and no image: the app ID is the username
    assert!(mm(&format!("{}?image=no", base)).send(&ctx("body")).await.unwrap());
    // An icon URL wins over the image
    assert!(mm(&format!("{}?icon_url=http://x/icon.png", base)).send(&ctx("body")).await.unwrap());

    let posts = requests_to(&server, "POST", "/a/path/hooks/token").await;
    assert_eq!(json_body(&posts[0]), json!({"text": "body", "username": "Apprise"}));
    assert_eq!(json_body(&posts[1]), json!({"text": "body", "icon_url": "http://x/icon.png", "username": "Apprise"}));
  }

  #[tokio::test]
  async fn test_webhook_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/hooks/token")).respond_with(ResponseTemplate::new(400)).mount(&server).await;
    assert!(!mm(&format!("mmost://127.0.0.1:{}/token?to=test", port(&server))).send(&ctx("body")).await.unwrap());
    // A 201 is not a webhook success
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/hooks/token")).respond_with(ResponseTemplate::new(201)).mount(&server).await;
    assert!(!mm(&format!("mmost://127.0.0.1:{}/token", port(&server))).send(&ctx("body")).await.unwrap());
    // A connection failure
    assert!(!mm("mmost://127.0.0.1:1/token").send(&ctx("body")).await.unwrap());
    // Webhooks ignore attachments
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/hooks/token")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut c = ctx("body");
    c.attachments.push(gif());
    assert!(mm(&format!("mmost://127.0.0.1:{}/token", port(&server))).send(&c).await.unwrap());
    assert!(requests_to(&server, "POST", "/api/v4/files").await.is_empty());
  }

  #[tokio::test]
  async fn test_bot_mode() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/v4/posts")).respond_with(ResponseTemplate::new(201)).mount(&server).await;
    let m = mm(&format!("mmost://127.0.0.1:{}/bearerToken?mode=bot&to=channel-id-123", port(&server)));
    assert!(m.send(&ctx("title\r\nbody")).await.unwrap());
    let posts = requests_to(&server, "POST", "/api/v4/posts").await;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].headers.get("authorization").unwrap(), "Bearer bearerToken");
    assert_eq!(json_body(&posts[0]), json!({"channel_id": "channel-id-123", "message": "title\r\nbody"}));

    // No channel to post to
    assert!(!mm(&format!("mmost://127.0.0.1:{}/bearerToken?mode=bot", port(&server))).send(&ctx("body")).await.unwrap());
    assert_eq!(requests_to(&server, "POST", "/api/v4/posts").await.len(), 1);

    // A refused post
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/v4/posts")).respond_with(ResponseTemplate::new(401)).mount(&server).await;
    assert!(!mm(&format!("mmost://127.0.0.1:{}/bearerToken?mode=bot&to=id1", port(&server))).send(&ctx("body")).await.unwrap());
    assert!(!mm("mmost://127.0.0.1:1/bearerToken?mode=bot&to=id1").send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_bot_channel_lookup() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/api/v4/teams/name/myteam/channels/name/good"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "cid-good"})))
      .mount(&server)
      .await;
    Mock::given(method("GET")).and(path("/api/v4/teams/name/myteam/channels/name/bad")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    Mock::given(method("GET"))
      .and(path("/api/v4/teams/name/myteam/channels/name/nojson"))
      .respond_with(ResponseTemplate::new(200).set_body_string("}"))
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/api/v4/teams/name/myteam/channels/name/noid"))
      .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v4/posts")).respond_with(ResponseTemplate::new(201)).mount(&server).await;

    let m = mm(&format!("mmost://127.0.0.1:{}/bearerToken?mode=bot&team=myteam&to=%23good", port(&server)));
    assert!(m.send(&ctx("title\r\nbody")).await.unwrap());
    let get = &requests_to(&server, "GET", "/api/v4/teams/name/myteam/channels/name/good").await[0];
    assert_eq!(get.headers.get("authorization").unwrap(), "Bearer bearerToken");
    assert_eq!(get.headers.get("accept").unwrap(), "application/json");
    assert_eq!(json_body(&requests_to(&server, "POST", "/api/v4/posts").await[0]), json!({"channel_id": "cid-good", "message": "title\r\nbody"}));

    // One lookup fails, the other channel still gets the post
    for bad in ["bad", "nojson", "noid"] {
      let m = mm(&format!("mmost://myteam@127.0.0.1:{}/bearerToken?mode=bot&to=%23good,%23{}", port(&server), bad));
      assert!(!m.send(&ctx("body")).await.unwrap(), "{}", bad);
    }
    assert_eq!(requests_to(&server, "POST", "/api/v4/posts").await.len(), 4);
    assert!(!mm("mmost://myteam@127.0.0.1:1/bearerToken?mode=bot&to=%23good").send(&ctx("body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_channel_lookup_is_cached() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/api/v4/teams/name/myteam/channels/name/chan"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "cid9"})))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v4/posts")).respond_with(ResponseTemplate::new(201)).mount(&server).await;

    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("mmost://myteam@127.0.0.1:{}/tok?mode=bot&to=%23chan&store=memory", port(&server))));
    assert!(apprise.notify(&ctx("one")).await.is_success());
    assert!(apprise.notify(&ctx("two")).await.is_success());
    assert_eq!(requests_to(&server, "GET", "/api/v4/teams/name/myteam/channels/name/chan").await.len(), 1);
    assert_eq!(requests_to(&server, "POST", "/api/v4/posts").await.len(), 2);
  }

  #[tokio::test]
  async fn test_bot_attachments() {
    // Every target gets its own uploads (verified against upstream)
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v4/files"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"file_infos": [{"id": "fid-001"}]})))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v4/posts")).respond_with(ResponseTemplate::new(201).set_body_string("{}")).mount(&server).await;

    let m = mm(&format!("mmost://127.0.0.1:{}/bearerToken?mode=bot&to=channel-001,channel-002", port(&server)));
    let mut c = ctx("title\r\nbody");
    c.attachments = vec![gif(), Attachment { name: String::new(), data: b"PNG".to_vec(), mime_type: "image/png".into() }];
    assert!(m.send(&c).await.unwrap());

    let uploads = requests_to(&server, "POST", "/api/v4/files").await;
    assert_eq!(uploads.len(), 4);
    assert_eq!(uploads[0].headers.get("authorization").unwrap(), "Bearer bearerToken");
    let body = String::from_utf8_lossy(&uploads[0].body).to_string();
    assert!(body.contains("name=\"channel_id\"\r\n\r\nchannel-001"), "{}", body);
    assert!(body.contains("name=\"files\"; filename=\"apprise-test.gif\""), "{}", body);
    assert!(body.contains("Content-Type: image/gif"), "{}", body);
    // An unnamed attachment gets a generated name
    assert!(String::from_utf8_lossy(&uploads[1].body).contains("filename=\"file002.dat\""));
    assert!(String::from_utf8_lossy(&uploads[2].body).contains("channel-002"));

    let posts = requests_to(&server, "POST", "/api/v4/posts").await;
    assert_eq!(json_body(&posts[0]), json!({"channel_id": "channel-001", "message": "title\r\nbody", "file_ids": ["fid-001", "fid-001"]}));
    assert_eq!(json_body(&posts[1])["channel_id"], "channel-002");
  }

  #[tokio::test]
  async fn test_bot_attachment_failures() {
    for upload in [
      ResponseTemplate::new(500),
      ResponseTemplate::new(200).set_body_string("not-json{{{"),
      ResponseTemplate::new(200).set_body_json(json!({"file_infos": [{"id": ""}]})),
      ResponseTemplate::new(200).set_body_json(json!({"file_infos": []})),
      ResponseTemplate::new(200).set_body_json(json!({})),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/api/v4/files")).respond_with(upload).mount(&server).await;
      Mock::given(method("POST")).and(path("/api/v4/posts")).respond_with(ResponseTemplate::new(201)).mount(&server).await;
      let m = mm(&format!("mmost://127.0.0.1:{}/bearerToken?mode=bot&to=channel-id-123", port(&server)));
      let mut c = ctx("body");
      c.attachments.push(gif());
      assert!(!m.send(&c).await.unwrap());
      assert_eq!(requests_to(&server, "POST", "/api/v4/files").await.len(), 1);
      assert!(requests_to(&server, "POST", "/api/v4/posts").await.is_empty());
    }
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_targets() {
    let server = MockServer::start().await;
    // id2 fails once, then succeeds
    Mock::given(method("POST"))
      .and(path("/api/v4/posts"))
      .and(wiremock::matchers::body_string_contains("id2"))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/api/v4/posts")).respond_with(ResponseTemplate::new(201)).mount(&server).await;

    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("mmost://127.0.0.1:{}/tok?mode=bot&to=id1,id2&retry=1&wait=0", port(&server))));
    assert!(apprise.notify(&ctx("body")).await.is_success());
    let ids: Vec<Value> = requests_to(&server, "POST", "/api/v4/posts").await.iter().map(|r| json_body(r)["channel_id"].clone()).collect();
    assert_eq!(ids, vec![json!("id1"), json!("id2"), json!("id2")]);
  }
}
