use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};

use crate::asset::AppriseAsset;
use crate::error::NotifyError;
use crate::notify::apprise_api::uuid4;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::{NotifyFormat, NotifyType};
use crate::utils::parse::ParsedUrl;
use crate::utils::qsd;
use crate::utils::validate::is_ipaddr;

/// Upstream `AppriseAsset.image_url_mask`, used when the asset sets none.
const DEFAULT_IMAGE_URL_MASK: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/{THEME}/apprise-{TYPE}-{XY}{EXTENSION}";

/// The notification icon URL for `notify_type` at `size` (`"256x256"`),
/// built from the asset's URL mask like upstream `AppriseAsset.image_url()`.
pub(crate) fn image_url(asset: &AppriseAsset, notify_type: &NotifyType, size: &str) -> String {
  let mask = asset.image_url_mask.as_deref().unwrap_or(DEFAULT_IMAGE_URL_MASK);
  let mut out = String::with_capacity(mask.len());
  let mut rest = mask;
  // One pass, case-insensitive, so substituted values are never rescanned
  'outer: while !rest.is_empty() {
    for (key, value) in [("{THEME}", asset.theme.as_str()), ("{TYPE}", notify_type.as_str()), ("{XY}", size), ("{EXTENSION}", ".png")] {
      if rest.len() >= key.len() && rest.is_char_boundary(key.len()) && rest[..key.len()].eq_ignore_ascii_case(key) {
        out.push_str(value);
        rest = &rest[key.len()..];
        continue 'outer;
      }
    }
    let c = rest.chars().next().unwrap();
    out.push(c);
    rest = &rest[c.len_utf8()..];
  }
  out
}

/// Upstream `is_hostname()`: RFC 1123 labels (underscores allowed), an
/// optional trailing dot, or an IP address.
pub(crate) fn py_is_hostname(host: &str) -> bool {
  if host.is_empty() || host.len() > 253 {
    return false;
  }
  let h = host.strip_suffix('.').unwrap_or(host);
  let labels: Vec<&str> = h.split('.').collect();
  if labels.len() == 4 && h.chars().all(|c| c.is_ascii_digit() || c == '.') {
    return is_ipaddr(h);
  }
  let label_ok = |l: &str| {
    let ok_chars = l.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let first = l.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    let last_ok = !l.ends_with('_') && !l.ends_with('-');
    match l.len() {
      1 => l.chars().all(|c| c.is_ascii_alphabetic()),
      2..=63 => ok_chars && first && last_ok,
      _ => false,
    }
  };
  labels.iter().all(|l| label_ok(l)) || is_ipaddr(h.trim_start_matches('[').trim_end_matches(']'))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
  Cloud,
  Private,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Auth {
  Basic,
  Token,
}

/// Upstream `NTFY_PRIORITY_MAP`: the first key the value starts with wins.
const PRIORITY_MAP: &[(&str, &str)] = &[
  ("l", "low"),
  ("mo", "low"),
  ("n", "default"),
  ("h", "high"),
  ("e", "max"),
  ("mi", "min"),
  ("ma", "max"),
  ("d", "default"),
  ("1", "min"),
  ("2", "low"),
  ("3", "default"),
  ("4", "high"),
  ("5", "max"),
];

/// Serialised JSON above this size is uploaded as a file instead.
const JSON_UPSTREAM_SIZE_LIMIT: usize = 8000;

pub struct Ntfy {
  mode: Mode,
  host: String,
  port: Option<u16>,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  auth: Auth,
  token: Option<String>,
  topics: Vec<String>,
  priority: &'static str,
  /// `?attach=`: a URL ntfy attaches itself.
  attach: Option<String>,
  filename: Option<String>,
  click: Option<String>,
  delay: Option<String>,
  email: Option<String>,
  /// `?xtags=` (or the legacy `?tags=`): ntfy's own tags, sent as `X-Tags`.
  xtags: Vec<String>,
  actions: Option<String>,
  include_image: bool,
  avatar_url: Option<String>,
  verify_certificate: bool,
  /// Apprise routing tags: the ones configuration assigned (or `?tag=` on a
  /// URL built directly); `?tags=` belongs to ntfy.
  tags: Vec<String>,
}

/// A query value as upstream reads it: `qsd` value, unquoted once more.
fn arg(url: &ParsedUrl, key: &str) -> Option<String> {
  qsd::get(url, key).filter(|v| !v.is_empty()).map(|v| qsd::unquote(&v))
}

/// The part after the last `/`, or `None` when that is empty.
fn basename(path: &str) -> Option<String> {
  Some(path.rsplit('/').next().unwrap_or(path).to_string()).filter(|s| !s.is_empty())
}

/// Length of upstream's `json.dumps()` of a flat string map: ASCII-escaped
/// with `", "` and `": "` separators.
fn py_dumps_len(payload: &[(&str, &str)]) -> usize {
  let quoted = |s: &str| -> usize {
    2 + s
      .chars()
      .map(|c| match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c if c.is_ascii() => 1,
        c if (c as u32) > 0xFFFF => 12,
        _ => 6,
      })
      .sum::<usize>()
  };
  2 + payload.iter().map(|(k, v)| quoted(k) + 2 + quoted(v)).sum::<usize>() + payload.len().saturating_sub(1) * 2
}

impl Ntfy {
  const CLOUD_NOTIFY_URL: &'static str = "https://ntfy.sh";

  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Native https://ntfy.sh/topic URLs always use cloud mode
    let native = url.schema == "http" || url.schema == "https";
    if native && (!url.host.as_deref().is_some_and(|h| h.eq_ignore_ascii_case("ntfy.sh")) || url.user.is_some() || url.password.is_some() || url.port.is_some())
    {
      return None;
    }

    let host = if native { String::new() } else { url.host.clone().unwrap_or_default() };
    let user = url.user.clone().filter(|u| !u.is_empty());
    let password = url.password.clone().filter(|p| !p.is_empty());

    let mut targets: Vec<String> = url.path_parts.clone();
    if let Some(to) = qsd::get(url, "to").filter(|v| !v.is_empty()) {
      targets.extend(qsd::parse_list(&to));
    }

    // Authentication
    let mut auth: Option<String> = None;
    let mut token = None;
    if let Some(t) = arg(url, "token") {
      auth = Some("token".into());
      token = Some(t);
    }
    if let Some(a) = qsd::get(url, "auth").filter(|v| !v.is_empty()) {
      auth = Some(qsd::unquote(&a.trim().to_lowercase()));
    }
    if auth.is_none()
      && let Some(u) = &user
      && password.is_none()
    {
      let is_token = u.len() > 3 && u[..3].eq_ignore_ascii_case("tk_") && !u[3..].starts_with([' ', '\t']);
      auth = Some(if is_token { "token" } else { "basic" }.into());
    }
    if auth.as_deref() == Some("token") && token.is_none() {
      token = if user.is_some() && password.is_none() { user.clone() } else { password.clone() };
    }
    let auth = match auth.as_deref().unwrap_or("basic") {
      "basic" => Auth::Basic,
      "token" => Auth::Token,
      other => {
        tracing::warn!("An invalid ntfy Authentication type ({}) was specified.", other);
        return None;
      }
    };

    // Mode
    let mode = if native {
      "cloud".to_string()
    } else {
      match qsd::get(url, "mode").filter(|v| !v.is_empty()) {
        Some(m) => qsd::unquote(&m.trim().to_lowercase()),
        None if (py_is_hostname(&host) || is_ipaddr(&host)) && !targets.is_empty() => "private".into(),
        None => "cloud".into(),
      }
    };
    let mode = match mode.as_str() {
      "cloud" => Mode::Cloud,
      "private" => Mode::Private,
      other => {
        tracing::warn!("An invalid ntfy Mode ({}) was specified.", other);
        return None;
      }
    };
    match mode {
      // ntfy://topic: the host is the first topic
      Mode::Cloud if !host.to_lowercase().contains("ntfy.sh") => targets.insert(0, host.clone()),
      Mode::Private if !py_is_hostname(&host) => return None,
      _ => {}
    }

    let mut topics = Vec::new();
    for t in targets {
      let topic = t.trim();
      if (1..=64).contains(&topic.len()) && topic.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        topics.push(topic.to_string());
      } else {
        tracing::warn!("A specified ntfy topic ({}) is invalid and will be ignored", t);
      }
    }

    let priority = match qsd::get(url, "priority").filter(|v| !v.is_empty()) {
      Some(p) => {
        let p = qsd::unquote(&p).to_lowercase();
        PRIORITY_MAP.iter().find(|(k, _)| p.starts_with(k)).map(|(_, v)| *v).unwrap_or("default")
      }
      None => "default",
    };

    // ?attach= takes its filename from the URL unless ?filename= is given
    let attach = arg(url, "attach");
    let mut filename = None;
    if let Some(a) = &attach {
      if let Some(p) = ParsedUrl::parse(a).filter(|p| p.host.as_deref().is_some_and(py_is_hostname)) {
        filename = basename(&p.path);
      }
      if let Some(f) = arg(url, "filename") {
        filename = basename(&f);
      }
    }

    // Non-ASCII click URLs are encoded once here (and again when sent)
    let click = arg(url, "click").map(|c| if c.is_ascii() { c } else { qsd::quote(&c, ":/?&=[]") });

    let xtags =
      qsd::get(url, "xtags").filter(|v| !v.is_empty()).or_else(|| qsd::get(url, "tags")).map(|t| qsd::parse_list(&qsd::unquote(&t))).unwrap_or_default();

    // Upstream parse_bool() with a default of yes
    let include_image = qsd::get(url, "image").is_none_or(|v| {
      let head: String = v.to_lowercase().chars().take(2).collect();
      !["de", "di", "ne", "f", "n", "no", "of", "0", "fa"].contains(&head.as_str())
    });

    Some(Self {
      mode,
      host,
      port: url.port,
      secure: url.schema.ends_with('s'),
      user,
      password,
      auth,
      token,
      topics,
      priority,
      attach,
      filename,
      click,
      delay: arg(url, "delay"),
      email: arg(url, "email"),
      xtags,
      actions: arg(url, "actions"),
      include_image,
      avatar_url: qsd::get(url, "avatar_url").map(|v| qsd::unquote(&v)),
      verify_certificate: url.verify_certificate(),
      tags: url
        .assigned_tags
        .clone()
        .unwrap_or_else(|| url.get("tag").map(|t| t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "ntfy",
      service_url: Some("https://ntfy.sh/"),
      setup_url: Some("https://appriseit.com/services/ntfy/"),
      protocols: vec!["ntfy", "ntfys"],
      description: "Send notifications via ntfy.sh (self-hosted or cloud).",
      attachment_support: true,
    }
  }

  fn base_url(&self) -> String {
    match self.mode {
      Mode::Cloud => service_base(Self::CLOUD_NOTIFY_URL),
      Mode::Private => {
        let schema = if self.secure { "https" } else { "http" };
        match self.port {
          Some(p) => format!("{}://{}:{}", schema, self.host, p),
          None => format!("{}://{}", schema, self.host),
        }
      }
    }
  }

  fn headers(&self, ctx: &NotifyContext, icon: Option<&str>) -> Result<HeaderMap, NotifyError> {
    let mut list: Vec<(&str, String)> = vec![("User-Agent", APP_ID.to_string())];
    if self.mode == Mode::Private
      && self.auth == Auth::Token
      && let Some(t) = &self.token
    {
      list.push(("Authorization", format!("Bearer {}", t)));
    }
    if let Some(icon) = icon {
      list.push(("X-Icon", icon.to_string()));
    }
    if ctx.body_format == NotifyFormat::Markdown {
      list.push(("X-Markdown", "yes".into()));
    }
    if self.priority != "default" {
      list.push(("X-Priority", self.priority.into()));
    }
    if let Some(d) = &self.delay {
      list.push(("X-Delay", d.clone()));
    }
    if let Some(c) = &self.click {
      list.push(("X-Click", qsd::quote(c, ":/?@&=#")));
    }
    if let Some(e) = &self.email {
      list.push(("X-Email", e.clone()));
    }
    if !self.xtags.is_empty() {
      list.push(("X-Tags", self.xtags.join(",")));
    }
    if let Some(a) = &self.actions {
      list.push(("X-Actions", a.clone()));
    }
    let mut map = HeaderMap::new();
    for (k, v) in list {
      let value = HeaderValue::from_str(&v).map_err(|_| NotifyError::InvalidParam(format!("invalid value for header {}", k)))?;
      map.insert(HeaderName::from_bytes(k.as_bytes()).expect("valid header name"), value);
    }
    Ok(map)
  }

  /// Publish one message (or one attachment) to `topic` with the given
  /// title and body.
  async fn send_one(
    &self,
    client: &reqwest::Client,
    ctx: &NotifyContext,
    topic: &str,
    (title, body): (&str, &str),
    attach: Option<&Attachment>,
    icon: Option<&str>,
  ) -> Result<bool, NotifyError> {
    if self.mode == Mode::Private && self.auth == Auth::Token && self.token.is_none() {
      tracing::warn!("No Ntfy Token was specified");
      return Ok(false);
    }
    let mut headers = self.headers(ctx, icon)?;
    let mut url = self.base_url();

    let mut fields: Vec<(&str, &str)> = Vec::new();
    if !title.is_empty() {
      fields.push(("title", title));
    }
    if !body.is_empty() {
      fields.push(("message", body));
    }

    let mut req = match attach {
      None => {
        let mut payload: Vec<(&str, &str)> = vec![("topic", topic)];
        if let Some(a) = &self.attach {
          payload.push(("attach", a));
          if let Some(f) = &self.filename {
            payload.push(("filename", f));
          }
        }
        payload.extend(fields);

        if py_dumps_len(&payload) > JSON_UPSTREAM_SIZE_LIMIT {
          // Too big for a JSON message; upload it as a file instead
          let mime = match ctx.body_format {
            NotifyFormat::Markdown => "text/markdown",
            NotifyFormat::Text => "text/plain",
            NotifyFormat::Html => "text/html",
          };
          let content = if title.is_empty() { body.to_string() } else { format!("{}\n{}", title, body) };
          let att = Attachment { name: format!("{}.txt", uuid4()), data: content.into_bytes(), mime_type: mime.into() };
          return Box::pin(self.send_one(client, ctx, topic, ("", ""), Some(&att), icon)).await;
        }

        headers.insert(reqwest::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let json: Map<String, Value> = payload.into_iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect();
        client.post(&url).headers(headers).body(serde_json::to_string(&json)?)
      }
      Some(att) => {
        url = format!("{}/{}", url, topic);
        let mut params: Vec<(&str, &str)> = vec![("filename", &att.name)];
        params.extend(fields);
        client.post(&url).headers(headers).query(&params).body(att.data.clone())
      }
    };
    if self.mode == Mode::Private
      && self.auth == Auth::Basic
      && let Some(u) = &self.user
    {
      // `requests` sends a missing password as the string `None`
      req = req.basic_auth(u, Some(self.password.clone().unwrap_or_else(|| "None".into())));
    }

    tracing::debug!("ntfy POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let resp = match req.send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("A Connection error occurred sending ntfy:{} notification.", url);
        tracing::debug!("Socket Exception: {}", e);
        return Ok(false);
      }
    };
    let status = resp.status().as_u16();
    if status == 200 {
      tracing::info!("Sent ntfy notification to '{}'.", url);
      return Ok(true);
    }
    let text = resp.text().await.unwrap_or_default();
    let (error, code) = match serde_json::from_str::<Value>(&text) {
      Ok(v) => (v.get("error").and_then(|e| e.as_str()).unwrap_or("").to_string(), v.get("code").and_then(|c| c.as_i64()).unwrap_or(status as i64)),
      Err(_) => (String::new(), status as i64),
    };
    tracing::warn!("Failed to send ntfy notification to topic '{}': {}{}error={}.", topic, error, if error.is_empty() { "" } else { ", " }, code);
    Ok(false)
  }
}

#[async_trait]
impl Notify for Ntfy {
  fn schemas(&self) -> &[&str] {
    &["ntfy", "ntfys"]
  }
  fn service_name(&self) -> &str {
    "ntfy"
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
    200
  }
  fn body_maxlen(&self) -> usize {
    7800
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.topics.is_empty() {
      tracing::warn!("There are no ntfy topics to notify");
      return Ok(false);
    }

    let icon = if self.include_image {
      Some(self.avatar_url.clone().filter(|a| !a.is_empty()).unwrap_or_else(|| image_url(&ctx.asset, &ctx.notify_type, "256x256")))
    } else {
      None
    };

    let client = build_client(self.verify_certificate)?;
    let mut has_error = false;
    // Upstream pops topics off the end of its list
    for topic in self.topics.iter().rev() {
      if ctx.attachments.is_empty() {
        if !self.send_one(&client, ctx, topic, (&ctx.title, &ctx.body), None, icon.as_deref()).await? {
          has_error = true;
        }
        continue;
      }
      // One upload per attachment; the first carries the title and body
      for (no, att) in ctx.attachments.iter().enumerate() {
        let (title, body) = if no == 0 { (ctx.title.as_str(), ctx.body.as_str()) } else { ("", "") };
        if !self.send_one(&client, ctx, topic, (title, body), Some(att), icon.as_deref()).await? {
          return Ok(false);
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
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn ntfy(url: &str) -> Ntfy {
    Ntfy::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  const ICON: &str = "https://github.com/caronc/apprise/raw/master/apprise/assets/themes/default/apprise-info-256x256.png";

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "ntfy://",
      "ntfys://",
      "ntfy://:@/",
      "ntfy://user:pass@localhost?mode=private",
      "ntfy://user:pass@localhost/#/!/@",
      "ntfy://user@localhost/topic/",
      "ntfy://ntfy.sh/topic1/topic2/",
      "ntfy://localhost/topic1/topic2/",
      "ntfy://localhost/topic1/?email=user@gmail.com",
      "ntfy://localhost/topic1/?xtags=tag1,tag2,tag3",
      "ntfy://localhost/topic1/?tags=tag1,tag2,tag3",
      "ntfy://localhost/topic1/?actions=view%2CExample%2Chttp://www.example.com/%3Bview%2CTest%2Chttp://www.test.com/",
      "ntfy://localhost/topic1/?delay=3600",
      "ntfy://localhost/topic1/?title=A%20Great%20Title",
      "ntfy://localhost/topic1/?click=yes",
      "ntfy://localhost/topic1/?image=False",
      "ntfy://localhost/topic1/?avatar_url=ttp://localhost/test.jpg",
      "ntfy://localhost/topic1/?attach=http://example.com/file.jpg",
      "ntfy://localhost/topic1/?attach=http://example.com/file.jpg&filename=smoke.jpg",
      "ntfy://localhost/topic1/?attach=http://-%20",
      "ntfy://tk_abcd123456@localhost/topic1",
      "ntfy://abcd123456@localhost/topic1?auth=token",
      "ntfy://:abcd123456@localhost/topic1?auth=token",
      "ntfy://localhost/topic1?token=abc1234",
      "ntfy://user:token@localhost/topic1?auth=token",
      "ntfy://localhost/topic1?auth=token",
      "ntfy://localhost/topic1/?priority=default",
      "ntfy://localhost/topic1/?priority=high",
      "ntfy://user:pass@localhost:8080/topic/",
      "ntfys://user:pass@localhost?to=topic",
      "https://ntfy.sh?to=topic",
      "ntfy://user:pass@topic1/topic2/topic3/?mode=cloud",
      "ntfy://user:pass@ntfy.sh/topic1/topic2/?mode=cloud",
      "ntfy://user:pass@localhost:8083/topic1/topic2/",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in [
      "https://just/a/random/host/that/means/nothing",
      "https://sub.ntfy.sh/topic",
      "ntfys://user:web/token@localhost/topic/?mode=invalid",
      "ntfys://token@localhost/topic/?auth=invalid",
      "ntfys://user:web@-_/topic1/topic2/?mode=private",
    ] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_parse_modes_and_topics() {
    let n = ntfy("ntfy://topic1");
    assert_eq!((n.mode, n.topics.clone()), (Mode::Cloud, vec!["topic1".to_string()]));

    let n = ntfy("ntfy://user:pass@topic1/topic2/topic3/?mode=cloud");
    assert_eq!(n.topics, vec!["topic1", "topic2", "topic3"]);

    let n = ntfy("ntfy://user:pass@ntfy.sh/topic1/topic2/?mode=cloud");
    assert_eq!(n.topics, vec!["topic1", "topic2"]);

    // A host with topics is a private server, even ntfy.sh
    let n = ntfy("ntfy://ntfy.sh/topic1/topic2/");
    assert_eq!((n.mode, n.base_url()), (Mode::Private, "http://ntfy.sh".to_string()));

    let n = ntfy("https://ntfy.sh/t1/t2?mode=private");
    assert_eq!((n.mode, n.topics.clone(), n.base_url()), (Mode::Cloud, vec!["t1".to_string(), "t2".to_string()], "https://ntfy.sh".to_string()));

    let n = ntfy("ntfys://user:pass@localhost:8080?to=topic,abc---&priority=max");
    assert_eq!(
      (n.mode, n.topics.clone(), n.base_url()),
      (Mode::Private, vec!["abc---".to_string(), "topic".to_string()], "https://localhost:8080".to_string())
    );

    // Invalid topics are dropped, the URL still loads
    let n = ntfy("ntfy://user:pass@localhost/#/!/@");
    assert!(n.topics.is_empty());
    assert!(ntfy("ntfy://").topics.is_empty());
  }

  #[test]
  fn test_parse_auth() {
    let n = ntfy("ntfy://tk_abcd123456@localhost/topic1");
    assert_eq!((n.auth, n.token.as_deref()), (Auth::Token, Some("tk_abcd123456")));
    let n = ntfy("ntfy://abcd123456@localhost/topic1?auth=token");
    assert_eq!((n.auth, n.token.as_deref()), (Auth::Token, Some("abcd123456")));
    let n = ntfy("ntfy://:abcd123456@localhost/topic1?auth=token");
    assert_eq!((n.auth, n.token.as_deref()), (Auth::Token, Some("abcd123456")));
    let n = ntfy("ntfy://user:token@localhost/topic1?auth=token");
    assert_eq!((n.auth, n.token.as_deref()), (Auth::Token, Some("token")));
    let n = ntfy("ntfy://localhost/topic1?token=abc1234");
    assert_eq!((n.auth, n.token.as_deref()), (Auth::Token, Some("abc1234")));
    let n = ntfy("ntfy://user@localhost/topic1");
    assert_eq!((n.auth, n.token.as_deref()), (Auth::Basic, None));
    let n = ntfy("ntfy://localhost/topic1?auth=token");
    assert_eq!((n.auth, n.token.as_deref()), (Auth::Token, None));
  }

  #[test]
  fn test_parse_options() {
    for (value, expected) in [
      ("min", "min"),
      ("1", "min"),
      ("low", "low"),
      ("moderate", "low"),
      ("normal", "default"),
      ("high", "high"),
      ("4", "high"),
      ("max", "max"),
      ("emergency", "max"),
      ("urgent", "default"),
      ("", "default"),
    ] {
      assert_eq!(ntfy(&format!("ntfy://localhost/t?priority={}", value)).priority, expected, "{value}");
    }

    let n = ntfy("ntfy://localhost/t?attach=http://example.com/path/file.jpg");
    assert_eq!((n.attach.as_deref(), n.filename.as_deref()), (Some("http://example.com/path/file.jpg"), Some("file.jpg")));
    let n = ntfy("ntfy://localhost/t?attach=http://example.com/file.jpg&filename=/a/smoke.jpg");
    assert_eq!(n.filename.as_deref(), Some("smoke.jpg"));
    assert_eq!(ntfy("ntfy://localhost/t?attach=http://example.com/").filename, None);
    assert_eq!(ntfy("ntfy://localhost/t?attach=http://-%20").filename, None);
    // ?filename= alone means nothing
    assert_eq!(ntfy("ntfy://localhost/t?filename=a.jpg").filename, None);

    // xtags= wins over the legacy tags=; neither becomes a routing tag
    let n = ntfy("ntfy://localhost/t?xtags=b,a&tags=c&tag=route");
    assert_eq!((n.xtags.clone(), n.tags.clone()), (vec!["a".to_string(), "b".to_string()], vec!["route".to_string()]));
    let n = ntfy("ntfy://localhost/t?tags=smile,de");
    assert_eq!(n.xtags, vec!["de", "smile"]);
    assert!(n.tags().is_empty());

    assert!(!ntfy("ntfy://localhost/t?image=False").include_image);
    assert!(ntfy("ntfy://localhost/t?image=bogus").include_image);
    assert_eq!(ntfy("ntfy://localhost/t?click=http://a.com/%C3%A9%20x").click.as_deref(), Some("http://a.com/%C3%A9%20x"));
  }

  #[test]
  fn test_image_url_and_hostname() {
    let asset = AppriseAsset::default();
    assert_eq!(image_url(&asset, &NotifyType::Info, "256x256"), ICON);
    let custom = AppriseAsset { image_url_mask: Some("http://x/{theme}/{TYPE}-{xy}{EXTENSION}?{TYPE}".into()), ..AppriseAsset::default() };
    assert_eq!(image_url(&custom, &NotifyType::Warning, "72x72"), "http://x/default/warning-72x72.png?warning");

    for ok in ["localhost", "my_host", "a", "ntfy.sh.", "10.0.0.1", "a1.b-c"] {
      assert!(py_is_hostname(ok), "{ok}");
    }
    for bad in ["", "-_", "_a", "a-", "a..b", "1"] {
      assert!(!py_is_hostname(bad), "{bad}");
    }
  }

  #[test]
  fn test_py_dumps_len() {
    let payload = [("topic", "t"), ("message", "a\"\u{e9}\u{1F600}\n")];
    // len(json.dumps({"topic": "t", "message": "a\"\u00e9\U0001F600\n"})) in Python
    assert_eq!(py_dumps_len(&payload), 52);
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_private_json_publish() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(header("X-Icon", ICON))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .and(header("X-Priority", "max"))
      .and(header("X-Delay", "10"))
      .and(header("X-Click", "http://a.com/%25C3%25A9%2520x"))
      .and(header("X-Email", "a@b.c"))
      .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
      .expect(2)
      .mount(&server)
      .await;

    // Matches pycapture.py for the same options
    let n = ntfy(&format!(
      "ntfy://user:pass@localhost:{}/t1/t2?priority=max&xtags=b,a&click=http://a.com/\u{e9} x&delay=10&email=a@b.c&actions=view,x,y",
      server.address().port()
    ));
    assert!(n.send(&ctx("title", "body")).await.unwrap());

    // wiremock splits header values on commas, so check these directly
    let requests = requests_to(&server, "POST", "/").await;
    assert_eq!(requests[0].headers.get("X-Tags").unwrap(), "a,b");
    assert_eq!(requests[0].headers.get("X-Actions").unwrap(), "view,x,y");
    let topics: Vec<Value> = requests.iter().map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()).collect();
    assert_eq!(topics[0], serde_json::json!({"topic": "t2", "title": "title", "message": "body"}));
    assert_eq!(topics[1], serde_json::json!({"topic": "t1", "title": "title", "message": "body"}));
    assert!(requests[0].headers.get("X-Markdown").is_none());
  }

  #[tokio::test]
  async fn test_token_markdown_attach_url_and_no_image() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/"))
      .and(header("Authorization", "Bearer tk_abc"))
      .and(header("X-Markdown", "yes"))
      .and(body_json(serde_json::json!({"topic": "topic", "attach": "http://example.com/path/file.jpg", "filename": "file.jpg", "message": "body"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let n = ntfy(&format!("ntfy://tk_abc@localhost:{}/topic?attach=http://example.com/path/file.jpg&image=no", server.address().port()));
    let c = NotifyContext { body_format: NotifyFormat::Markdown, ..ctx("", "body") };
    assert!(n.send(&c).await.unwrap());
    let requests = requests_to(&server, "POST", "/").await;
    assert!(requests[0].headers.get("X-Icon").is_none());
  }

  #[tokio::test]
  async fn test_attachments_upload() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/topic")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;

    let n = ntfy(&format!("ntfy://localhost:{}/topic?image=no&avatar_url=http://x/y.png", server.address().port()));
    let mut c = ctx("title", "body");
    c.attachments.push(Attachment { name: "a.txt".into(), data: b"hello".to_vec(), mime_type: "text/plain".into() });
    c.attachments.push(Attachment { name: "b.png".into(), data: b"\x89PNG\r\n".to_vec(), mime_type: "image/png".into() });
    assert!(n.send(&c).await.unwrap());

    let requests = requests_to(&server, "POST", "/topic").await;
    let query = |i: usize| -> Vec<(String, String)> { requests[i].url.query_pairs().into_owned().collect() };
    assert_eq!(query(0), vec![("filename".into(), "a.txt".into()), ("title".into(), "title".into()), ("message".into(), "body".into())]);
    assert_eq!(query(1), vec![("filename".to_string(), "b.png".to_string())]);
    assert_eq!(requests[0].body, b"hello");
    assert_eq!(requests[1].body, b"\x89PNG\r\n");
    assert!(requests[0].headers.get("Content-Type").is_none());
  }

  #[tokio::test]
  async fn test_cloud_avatar_and_large_message_upload() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/topic"))
      .and(header("X-Icon", "http://x/y.png"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let n = ntfy("ntfy://topic?avatar_url=http://x/y.png");
    assert_eq!(n.mode, Mode::Cloud);
    let body = "x".repeat(8000);
    assert!(crate::notify::with_test_base(server.uri(), n.send(&ctx("Title", &body))).await.unwrap());

    let requests = requests_to(&server, "POST", "/topic").await;
    let query: Vec<(String, String)> = requests[0].url.query_pairs().into_owned().collect();
    assert_eq!(query.len(), 1);
    assert!(query[0].1.ends_with(".txt") && query[0].1.len() == 40);
    assert_eq!(requests[0].body, format!("Title\n{}", body).into_bytes());
  }

  #[tokio::test]
  async fn test_failures() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/"))
      .respond_with(ResponseTemplate::new(400).set_body_string(r#"{"code": 40001, "error": "bad"}"#))
      .expect(2)
      .mount(&server)
      .await;
    // Every topic is tried even after a failure
    let n = ntfy(&format!("ntfy://localhost:{}/t1/t2", server.address().port()));
    assert!(!n.send(&ctx("t", "b")).await.unwrap());

    // 2xx other than 200 is a failure too
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/")).respond_with(ResponseTemplate::new(202)).expect(1).mount(&server).await;
    assert!(!ntfy(&format!("ntfy://localhost:{}/t1", server.address().port())).send(&ctx("t", "b")).await.unwrap());

    // No topics, missing token, connection failure
    assert!(!ntfy("ntfy://").send(&ctx("t", "b")).await.unwrap());
    assert!(!ntfy("ntfy://localhost/topic1?auth=token").send(&ctx("t", "b")).await.unwrap());
    assert!(!ntfy("ntfy://127.0.0.1:1/topic1").send(&ctx("t", "b")).await.unwrap());
  }
}
