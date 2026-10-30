use crate::error::NotifyError;
use crate::notify::ntfy::image_url;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::qsd;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Upstream `CHANNEL_REGEX`: an optional `#` then the channel's digits.
fn parse_channel(target: &str) -> Option<u64> {
  let t = target.trim_start();
  let t = t.strip_prefix('#').or_else(|| t.strip_prefix("%35")).unwrap_or(t);
  let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
  digits.parse().ok()
}

/// Discord mentions found in the body (upstream `USER_ROLE_DETECTION_RE`):
/// `<@123>` users, `<@&123>` roles and `@name` mentions.
#[derive(Debug, Default, PartialEq)]
struct Mentions {
  users: Vec<String>,
  roles: Vec<String>,
  content: Vec<String>,
}

fn find_mentions(body: &str) -> Mentions {
  let b = body.as_bytes();
  let mut m = Mentions::default();
  let mut pos = 0;
  while pos < b.len() {
    let mut q = pos;
    while q < b.len() && (b[q] as char).is_ascii_whitespace() {
      q += 1;
    }
    // <@123>, <@&123>, @123 or @&123
    let mut r = q;
    if r < b.len() && b[r] == b'<' {
      r += 1;
    }
    if r < b.len() && b[r] == b'@' {
      r += 1;
      let role = r < b.len() && b[r] == b'&';
      if role {
        r += 1;
      }
      let start = r;
      while r < b.len() && b[r].is_ascii_digit() {
        r += 1;
      }
      if r > start {
        let id = body[start..r].to_string();
        if role {
          m.content.push(format!("<@&{}>", id));
          m.roles.push(id);
        } else {
          m.content.push(format!("<@{}>", id));
          m.users.push(id);
        }
        pos = if r < b.len() && b[r] == b'>' { r + 1 } else { r };
        continue;
      }
    }
    // @everyone, @here, ...
    if q < b.len() && b[q] == b'@' {
      let mut r = q + 1;
      while r < b.len() && b[r].is_ascii_alphanumeric() {
        r += 1;
      }
      if r > q + 1 {
        m.content.push(body[q..r].to_string());
        pos = r;
        continue;
      }
    }
    pos += 1;
    while pos < b.len() && !body.is_char_boundary(pos) {
      pos += 1;
    }
  }
  m
}

pub struct Notifiarr {
  apikey: String,
  channels: Vec<u64>,
  include_image: bool,
  event: i64,
  source: Option<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Notifiarr {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().unwrap_or_default();
    let mut targets: Vec<String> = url.path_parts.clone();

    // With ?apikey= (or ?key=) the host is a channel instead of the key
    let apikey = match qsd::get(url, "apikey").filter(|v| !v.is_empty()).or_else(|| qsd::get(url, "key").filter(|v| !v.is_empty())) {
      Some(k) => {
        if !host.is_empty() {
          targets.push(host.clone());
        }
        qsd::unquote(&k)
      }
      None => host,
    };
    if apikey.is_empty() {
      tracing::warn!("An invalid Notifiarr APIKey was specified.");
      return None;
    }

    if let Some(to) = qsd::get(url, "to").filter(|v| !v.is_empty()) {
      targets.extend(qsd::unquote(&to).split(|c: char| " \t\r\n,#\\/".contains(c)).filter(|s| !s.is_empty()).map(String::from));
    }
    let mut channels = Vec::new();
    for target in qsd::parse_list(&targets.join(" ")) {
      match parse_channel(&target) {
        Some(c) => channels.push(c),
        None => tracing::warn!("Dropped invalid channel ({}) specified.", target),
      }
    }

    let event = match qsd::get(url, "event").filter(|v| !v.is_empty()) {
      Some(e) => match qsd::unquote(&e).trim().parse::<i64>() {
        Ok(n) => n,
        Err(_) => {
          tracing::warn!("An invalid Notifiarr Discord Event ID ({}) was specified.", e);
          return None;
        }
      },
      None => 0,
    };

    let source = qsd::get(url, "source")
      .filter(|v| !v.is_empty())
      .or_else(|| qsd::get(url, "from").filter(|v| !v.is_empty()))
      .map(|s| qsd::unquote(&s).trim().to_string())
      .filter(|s| !s.is_empty());

    Some(Self {
      apikey,
      channels,
      include_image: qsd::get(url, "image").is_some_and(|v| parse_bool(&v)),
      event,
      source,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Notifiarr",
      service_url: Some("https://notifiarr.com/"),
      setup_url: Some("https://appriseit.com/services/notifiarr/"),
      protocols: vec!["notifiarr"],
      description: "Send notifications via Notifiarr.",
      attachment_support: false,
    }
  }

  fn payload(&self, ctx: &NotifyContext, channel: u64, mentions: &Mentions) -> Value {
    let app_id = ctx.asset.app_id.as_str();
    let ping = |ids: &Vec<String>| ids.first().map(|i| json!(i)).unwrap_or(json!(0));
    let mut payload = json!({
      "source": self.source.as_deref().unwrap_or(app_id),
      "type": ctx.notify_type.as_str(),
      "notification": {
        "update": self.event != 0,
        "name": app_id,
        "event": if self.event != 0 { self.event.to_string() } else { String::new() },
      },
      "discord": {
        "color": ctx.notify_type.color_hex(),
        "ping": {"pingUser": ping(&mentions.users), "pingRole": ping(&mentions.roles)},
        "text": {
          "title": ctx.title,
          "content": if mentions.content.is_empty() { String::new() } else { format!("\u{1F449} {}", mentions.content.join(" ")) },
          "description": ctx.body,
          "footer": ctx.asset.app_desc,
        },
        "ids": {"channel": channel},
      },
    });
    if self.include_image {
      let icon = image_url(&ctx.asset, &ctx.notify_type, "256x256");
      payload["discord"]["text"]["icon"] = json!(icon);
      payload["discord"]["images"] = json!({"thumbnail": icon});
    }
    payload
  }
}

#[async_trait]
impl Notify for Notifiarr {
  fn schemas(&self) -> &[&str] {
    &["notifiarr"]
  }
  fn service_name(&self) -> &str {
    "Notifiarr"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    4096
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.04
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.channels.is_empty() {
      tracing::warn!("There were no Notifiarr channels to notify.");
      return Ok(false);
    }
    let mentions = find_mentions(&ctx.body);
    let url = format!("{}/api/v1/notification/apprise", service_base("https://notifiarr.com"));
    let client = build_client(self.verify_certificate)?;

    let mut has_error = false;
    for channel in &self.channels {
      tracing::debug!("Notifiarr POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let resp = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/json")
        .header("Accept", "text/plain")
        .header("X-api-Key", &self.apikey)
        .body(self.payload(ctx, *channel, &mentions).to_string())
        .send()
        .await;
      match resp {
        Ok(r) if r.status().is_success() => tracing::info!("Sent Notifiarr notification."),
        Ok(r) => {
          tracing::warn!("Failed to send Notifiarr notification: error={}.", r.status().as_u16());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Notifiarr Chat notification.");
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_urls() {
    for url in ["notifiarr://:@/", "notifiarr://", "notifiarr://apikey/1234/?event=invalid"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
    for url in [
      "notifiarr://apikey",
      "notifiarr://apikey/%%invalid%%",
      "notifiarr://apikey/#123",
      "notifiarr://apikey/123?image=No",
      "notifiarr://apikey/?to=123,432&event=1234",
      "notifiarr://123/?apikey=myapikey",
      "notifiarr://123/?key=myapikey",
      "notifiarr://123/?apikey=myapikey&source=My%20System",
      "notifiarr://123/?apikey=myapikey&from=My%20System",
      "notifiarr://?apikey=myapikey",
      "notifiarr://invalid?apikey=myapikey",
      "notifiarr://123/325/?apikey=myapikey",
      "notifiarr://apikey/123/",
    ] {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  fn arr(url: &str) -> Notifiarr {
    Notifiarr::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_parse() {
    let n = arr("notifiarr://apikey/123/%2345/bad/123abc?image=yes&event=12&source=Me");
    assert_eq!((n.apikey.as_str(), n.channels.clone(), n.event, n.source.as_deref(), n.include_image), ("apikey", vec![45, 123, 123], 12, Some("Me"), true));
    let n = arr("notifiarr://123/325/?apikey=myapikey&to=1,%232&from=Sys");
    assert_eq!((n.apikey.as_str(), n.channels.clone(), n.source.as_deref()), ("myapikey", vec![1, 123, 2, 325], Some("Sys")));
    assert!(arr("notifiarr://?apikey=myapikey").channels.is_empty());
    assert!(!arr("notifiarr://apikey/1").include_image);
  }

  #[test]
  fn test_mentions() {
    let m = find_mentions("hi <@123> <@&456> @here x@everyone <@abc> @&x @77y");
    assert_eq!(m.users, vec!["123", "77"]);
    assert_eq!(m.roles, vec!["456"]);
    assert_eq!(m.content, vec!["<@123>", "<@&456>", "@here", "@everyone", "@abc", "<@77>"]);
    assert_eq!(find_mentions("plain \u{e9} text"), Mentions::default());
  }

  #[tokio::test]
  async fn test_send() {
    let server = MockServer::start().await;
    let p = "/api/v1/notification/apprise";
    Mock::given(method("POST"))
      .and(path(p))
      .and(header("X-api-Key", "apikey"))
      .and(header("Accept", "text/plain"))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(200))
      .expect(2)
      .mount(&server)
      .await;

    let n = arr("notifiarr://apikey/123/%2345?image=yes&event=12&source=Me");
    let ctx = NotifyContext { title: "title".into(), body: "hi <@123> <@&456> @here".into(), notify_type: NotifyType::Warning, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), n.send(&ctx)).await.unwrap());

    // Matches pycapture.py
    let icon = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-warning-256x256.png";
    let bodies: Vec<Value> = requests_to(&server, "POST", p).await.iter().map(|r| serde_json::from_slice(&r.body).unwrap()).collect();
    assert_eq!(
      bodies[0],
      json!({
        "source": "Me", "type": "warning",
        "notification": {"update": true, "name": "Apprise", "event": "12"},
        "discord": {
          "color": "#CACF29",
          "ping": {"pingUser": "123", "pingRole": "456"},
          "text": {"title": "title", "content": "\u{1F449} <@123> <@&456> @here", "description": "hi <@123> <@&456> @here", "footer": "Apprise Notifications", "icon": icon},
          "ids": {"channel": 45},
          "images": {"thumbnail": icon},
        },
      })
    );
    assert_eq!(bodies[1]["discord"]["ids"]["channel"], 123);
  }

  #[tokio::test]
  async fn test_defaults_and_failures() {
    let server = MockServer::start().await;
    let p = "/api/v1/notification/apprise";
    Mock::given(method("POST")).and(path(p)).respond_with(ResponseTemplate::new(500)).expect(2).mount(&server).await;
    let n = arr("notifiarr://apikey/1/2");
    let ctx = NotifyContext { title: "t".into(), body: "b".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), n.send(&ctx)).await.unwrap());
    let body: Value = serde_json::from_slice(&requests_to(&server, "POST", p).await[0].body).unwrap();
    assert_eq!(body["source"], "Apprise");
    assert_eq!(body["notification"], json!({"update": false, "name": "Apprise", "event": ""}));
    assert_eq!(body["discord"]["ping"], json!({"pingUser": 0, "pingRole": 0}));
    assert_eq!(body["discord"]["text"]["content"], "");
    assert!(body["discord"].get("images").is_none());

    assert!(!arr("notifiarr://apikey").send(&ctx).await.unwrap());
  }
}
