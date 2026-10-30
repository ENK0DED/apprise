use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::{parse_list, qsd_bool, split_path, unquote};
use async_trait::async_trait;

/// Upstream `JOIN_PRIORITY_MAP`, checked in order against the start of the
/// lower-cased `?priority=` value.
const JOIN_PRIORITY_MAP: &[(&str, i32)] = &[("l", -2), ("m", -1), ("n", 0), ("h", 1), ("e", 2), ("-2", -2), ("-1", -1), ("0", 0), ("1", 1), ("2", 2)];

/// Device groups (`IS_GROUP_RE`), in the order upstream tries them.
const JOIN_GROUPS: &[&str] = &["all", "android", "chrome", "windows10", "phone", "tablet", "pc"];

/// Every target is sent to all devices when none is given.
const DEFAULT_JOIN_GROUP: &str = "group.all";

/// API keys and device IDs are 32 alphanumeric characters.
fn is_join_id(value: &str) -> bool {
  value.len() == 32 && value.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The group a target names, matched like upstream's unanchored
/// `(group\.)?(all|android|...)` (so `Allison` is `all`).
fn group_name(target: &str) -> Option<&'static str> {
  let lower = target.to_lowercase();
  let rest = lower.strip_prefix("group.").unwrap_or(&lower);
  JOIN_GROUPS.iter().copied().find(|g| rest.starts_with(g)).or_else(|| JOIN_GROUPS.iter().copied().find(|g| lower.starts_with(g)))
}

pub struct Join {
  apikey: String,
  targets: Vec<String>,
  priority: i32,
  include_image: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Join {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // join://{apikey}/{targets} or join://{apikey}@{target}/{targets}
    let host = url.host.as_deref().map(unquote).unwrap_or_default();
    let mut targets = Vec::new();
    let apikey = match url.user.as_deref().filter(|u| !u.is_empty()) {
      Some(user) => {
        targets.push(host);
        unquote(user)
      }
      None => host,
    };
    let apikey = apikey.trim().to_string();
    if !is_join_id(&apikey) {
      tracing::warn!("An invalid Join API Key was specified.");
      return None;
    }

    targets.extend(split_path(url));
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.push(to.to_string());
    }
    let mut targets: Vec<String> = parse_list(targets, true, true).into_iter().map(|t| group_name(&t).map(|g| format!("group.{}", g)).unwrap_or(t)).collect();
    if targets.is_empty() {
      targets.push(DEFAULT_JOIN_GROUP.to_string());
    }

    let priority = url
      .get("priority")
      .filter(|p| !p.is_empty())
      .and_then(|p| {
        let p = p.to_lowercase();
        JOIN_PRIORITY_MAP.iter().find(|(k, _)| p.starts_with(k)).map(|(_, v)| *v)
      })
      .unwrap_or(0);

    Some(Self { apikey, targets, priority, include_image: qsd_bool(url, "image", true), verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Join",
      service_url: Some("https://joaoapps.com/join/"),
      setup_url: Some("https://appriseit.com/services/join/"),
      protocols: vec!["join"],
      description: "Send notifications via Join.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Join {
  fn schemas(&self) -> &[&str] {
    &["join"]
  }
  fn service_name(&self) -> &str {
    "Join"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn body_maxlen(&self) -> usize {
    1000
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let base = format!("{}/_ah/api/messaging/v1/sendPush", service_base("https://joinjoaomgcd.appspot.com"));
    let icon = if self.include_image { ctx.asset.image_url(&ctx.notify_type, "72x72") } else { None };
    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;

    for target in &self.targets {
      let priority = self.priority.to_string();
      let mut args = vec![("apikey", self.apikey.as_str()), ("priority", priority.as_str()), ("title", ctx.title.as_str()), ("text", ctx.body.as_str())];
      // Groups and 32 character IDs are devices; anything else a device name
      args.push((if target.starts_with("group.") || is_join_id(target) { "deviceId" } else { "deviceNames" }, target.as_str()));
      if let Some(icon) = &icon {
        args.push(("icon", icon.as_str()));
      }
      // Upstream quotes spaces as %20 rather than '+'
      let query: Vec<String> = args.iter().map(|(k, v)| format!("{}={}", k, urlencoding::encode(v))).collect();
      let url = format!("{}?{}", base, query.join("&"));

      let result = client.post(&url).header("User-Agent", APP_ID).header("Content-Type", "application/x-www-form-urlencoded").send().await;
      match result {
        Ok(resp) if resp.status().as_u16() == 200 => tracing::info!("Sent Join notification to {}.", target),
        Ok(resp) => {
          let status = resp.status().as_u16();
          let reason = if status == 401 { "Unauthorized - Invalid Token." } else { resp.status().canonical_reason().unwrap_or("") };
          tracing::warn!("Failed to send Join notification to {}: {}{}error={}.", target, reason, if reason.is_empty() { "" } else { ", " }, status);
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Join:{} notification.", target);
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
  use crate::notify::registry::from_url;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn join(url: &str) -> Join {
    Join::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_urls() {
    let (a, d, e) = ("a".repeat(32), "d".repeat(32), "e".repeat(32));
    let valid = [
      format!("join://{}", a),
      format!("join://{}?to={}", a, d),
      format!("join://{}?priority=high", a),
      format!("join://{}?priority=invalid", a),
      format!("join://{}?priority=", a),
      format!("join://{}@{}?image=True", a, d),
      format!("join://{}@{}?image=False", a, d),
      format!("join://{}/{}", a, "My%20Device"),
      format!("join://{}/{}/{}", a, d, e),
      format!("join://{}/{}/{}", a, d, "group.chrome"),
    ];
    for url in &valid {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
    for url in ["join://", "join://:@/", "join://short", "join://bad!key0000000000000000000000000"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parsing() {
    let (a, d) = ("a".repeat(32), "d".repeat(32));
    assert_eq!(join(&format!("join://{}", a)).targets, vec!["group.all"]);
    // The host becomes a target when the API key is the user
    let obj = join(&format!("join://{}@{}/Allison/PHONE/group.Tablet/My%20Device", a, d));
    assert_eq!(obj.apikey, a);
    // Matches upstream, which also splits "My Device" on its space
    assert_eq!(obj.targets, vec!["group.all", "Device", "My", "group.phone", &d, "group.tablet"]);
    for (value, expected) in [("high", 1), ("low", -2), ("e", 2), ("-1", -1), ("invalid", 0)] {
      assert_eq!(join(&format!("join://{}?priority={}", a, value)).priority, expected);
    }
    assert!(join(&format!("join://{}", a)).include_image);
    assert!(!join(&format!("join://{}?image=no", a)).include_image);
  }

  #[tokio::test]
  async fn test_send() {
    // Matches pycapture.py: one POST per target with everything in the query
    let (a, d) = ("a".repeat(32), "d".repeat(32));
    let server = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/_ah/api/messaging/v1/sendPush")).respond_with(ResponseTemplate::new(200)).expect(3).mount(&server).await;
    let svc = from_url(&format!("join://{}/{}/Allison/myphone?priority=high", a, d)).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B ü".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let icon = "icon=https%3A%2F%2Fgithub.com%2Fcaronc%2Fapprise%2Fraw%2Fmaster%2Fapprise%2Fassets%2Fthemes%2Fdefault%2Fapprise-info-72x72.png";
    let requests = server.received_requests().await.unwrap();
    let queries: Vec<String> = requests.iter().map(|r| r.url.query().unwrap().to_string()).collect();
    assert_eq!(
      queries,
      vec![
        format!("apikey={}&priority=1&title=T&text=B%20%C3%BC&deviceId=group.all&{}", a, icon),
        format!("apikey={}&priority=1&title=T&text=B%20%C3%BC&deviceId={}&{}", a, d, icon),
        format!("apikey={}&priority=1&title=T&text=B%20%C3%BC&deviceNames=myphone&{}", a, icon),
      ]
    );
    assert_eq!(requests[0].headers.get("content-type").unwrap(), "application/x-www-form-urlencoded");
    assert!(requests[0].body.is_empty());
  }

  #[tokio::test]
  async fn test_send_failure() {
    for status in [401, 500, 999] {
      let server = MockServer::builder().start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(status)).expect(2).mount(&server).await;
      let svc = from_url(&format!("join://{}/a1/a2?image=no", "a".repeat(32))).unwrap();
      let ctx = NotifyContext { body: "B".into(), ..Default::default() };
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
  }

  #[test]
  fn test_service_details() {
    let details = Join::static_details();
    assert_eq!(details.service_name, "Join");
    assert_eq!(details.protocols, vec!["join"]);
  }
}
