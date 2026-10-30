use async_trait::async_trait;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::ParsedUrl;
use crate::utils::validate::is_email;

const DEFAULT_HOSTNAME: &str = "zulipchat.com";
const DEFAULT_STREAM: &str = "general";

pub struct Zulip {
  /// Bot name without its `-bot` suffix.
  botname: String,
  organization: String,
  hostname: String,
  token: String,
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Zulip {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // zulip://{botname}@{organization}[.{hostname}]/{token}/{targets}
    let is_name_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';

    // Upstream matches a 1-32 character name at the start of the value
    let raw_bot = url.user.as_deref().map(unquote).unwrap_or_default();
    let botname: String = raw_bot.trim().chars().take_while(|c| is_name_char(*c)).take(32).collect();
    if botname.is_empty() {
      tracing::warn!("The Zulip botname specified ({}) is invalid.", raw_bot);
      return None;
    }
    let botname = botname.strip_suffix("-bot").map(String::from).unwrap_or(botname);

    let raw_org = url.host.as_deref().map(unquote).unwrap_or_default();
    let raw_org = raw_org.trim();
    let organization: String = raw_org.chars().take_while(|c| is_name_char(*c)).take(32).collect();
    if organization.is_empty() {
      tracing::warn!("The Zulip organization specified ({}) is invalid.", raw_org);
      return None;
    }
    let hostname = raw_org[organization.len()..]
      .strip_prefix('.')
      .map(|h| h.split(char::is_whitespace).next().unwrap_or_default())
      .filter(|h| !h.is_empty())
      .unwrap_or(DEFAULT_HOSTNAME)
      .to_string();

    let mut targets = url.path_parts.clone();
    let token = match url.get("token").filter(|t| !t.is_empty()) {
      Some(t) => unquote(t),
      None if !targets.is_empty() => targets.remove(0),
      None => String::new(),
    };
    let token = token.trim().to_string();
    if token.len() != 32 || !token.chars().all(|c| c.is_ascii_alphanumeric()) {
      tracing::warn!("The Zulip token specified is invalid.");
      return None;
    }

    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      let to = unquote(to);
      targets.extend(to.split(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n' | ',' | '#' | '\\' | '/')).filter(|t| !t.is_empty()).map(String::from));
    }
    let mut targets = parse_list(&targets);
    if targets.is_empty() {
      targets.push(DEFAULT_STREAM.to_string());
    }

    Some(Self { botname, organization, hostname, token, targets, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Zulip",
      service_url: Some("https://zulipchat.com/"),
      setup_url: Some("https://appriseit.com/services/zulip/"),
      protocols: vec!["zulip"],
      description: "Send messages to Zulip streams and users.",
      attachment_support: false,
    }
  }
}

#[async_trait]
impl Notify for Zulip {
  fn schemas(&self) -> &[&str] {
    &["zulip"]
  }
  fn service_name(&self) -> &str {
    "Zulip"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    60
  }
  fn body_maxlen(&self) -> usize {
    10000
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let base = service_base(&format!("https://{}.{}", self.organization, self.hostname));
    let url = format!("{}/api/v1/messages", base);
    let auth_user = format!("{}-bot@{}.{}", self.botname, self.organization, self.hostname);
    let client = build_client(self.verify_certificate)?;

    let mut has_error = false;
    for target in &self.targets {
      // Email addresses get a private message; anything else is a stream
      let kind = if is_email(target) { "private" } else { "stream" };
      let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([("subject", ctx.title.as_str()), ("content", ctx.body.as_str()), ("type", kind), ("to", target.as_str())])
        .finish();
      tracing::debug!("Zulip POST URL: {} (cert_verify={})", url, self.verify_certificate);
      let resp = client
        .post(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8")
        .basic_auth(&auth_user, Some(&self.token))
        .body(body)
        .send()
        .await;
      match resp {
        Ok(r) if r.status().as_u16() == 200 => tracing::info!("Sent Zulip notification to {}.", target),
        Ok(r) => {
          let status = r.status();
          let reason = if status.as_u16() == 401 { "Unauthorized - Invalid Token." } else { status.canonical_reason().unwrap_or("") };
          tracing::warn!("Failed to send Zulip notification to {}: {}{}error={}.", target, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred sending Zulip notification to {}.", target);
          tracing::debug!("Socket Exception: {}", e);
          has_error = true;
        }
      }
    }
    Ok(!has_error)
  }
}

fn unquote(s: &str) -> String {
  urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string())
}

/// Upstream `parse_list()`: split on `[]`, `;`, `,` and whitespace, then
/// de-duplicate and sort.
fn parse_list(items: &[String]) -> Vec<String> {
  let mut out: Vec<String> = items
    .iter()
    .flat_map(|i| i.split(|c: char| c == '[' || c == ']' || c == ';' || c == ',' || c.is_whitespace()))
    .filter(|t| !t.is_empty())
    .map(String::from)
    .collect();
  out.sort();
  out.dedup();
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn parse_zulip(url: &str) -> Zulip {
    Zulip::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_valid_urls() {
    let token = "a".repeat(32);
    let urls = vec![
      format!("zulip://bot-name@apprise/{}", token),
      format!("zulip://botname@apprise/{}", token),
      format!("zulip://botname@apprise.zulipchat.com/{}", token),
      format!("zulip://botname@apprise/{}/channel1/channel2", token),
      format!("zulip://botname@apprise/{}/?to=channel1/channel2", token),
      format!("zulip://botname@apprise/?token={}&to=channel1", token),
      format!("zulip://botname@apprise/{}/user@example.com/user2@example.com", token),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let short_tok = format!("zulip://botname@apprise/{}", "a".repeat(24));
    let long_tok = format!("zulip://botname@apprise/{}", "a".repeat(33));
    let bad_bot = format!("zulip://....@apprise/{}", "a".repeat(32));
    let urls: Vec<&str> = vec!["zulip://", "zulip://:@/", "zulip://apprise", "zulip://botname@apprise", &short_tok, &long_tok, &bad_bot];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let token = "a".repeat(32);
    let z = parse_zulip(&format!("zulip://my-bot@apprise.example.org/{}/user@example.com/%23chan?to=y,x", token));
    assert_eq!(z.botname, "my");
    assert_eq!(z.organization, "apprise");
    assert_eq!(z.hostname, "example.org");
    assert_eq!(z.token, token);
    assert_eq!(z.targets, vec!["#chan", "user@example.com", "x", "y"]);

    // Defaults: zulipchat.com and the general stream
    let z = parse_zulip(&format!("zulip://botname@apprise/{}", token));
    assert_eq!(z.hostname, "zulipchat.com");
    assert_eq!(z.targets, vec!["general"]);

    // ?to= also splits on # and /
    let z = parse_zulip(&format!("zulip://botname@apprise/?token={}&to=%23a/b", token));
    assert_eq!(z.targets, vec!["a", "b"]);
  }

  #[tokio::test]
  async fn test_send_each_target() {
    use base64::Engine;
    let server = MockServer::start().await;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("my-bot@apprise.example.org:{}", "a".repeat(32)));
    Mock::given(method("POST"))
      .and(path("/api/v1/messages"))
      .and(header("Content-Type", "application/x-www-form-urlencoded; charset=utf-8"))
      .and(header("Authorization", format!("Basic {}", auth).as_str()))
      .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": "success"})))
      .expect(2)
      .mount(&server)
      .await;

    let svc = from_url(&format!("zulip://my-bot@apprise.example.org/{}/user@example.com/stream1", "a".repeat(32))).unwrap();
    let ctx = NotifyContext { title: "T".into(), body: "B b".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    let bodies: Vec<String> = reqs.iter().map(|r| String::from_utf8_lossy(&r.body).into_owned()).collect();
    // Targets are sorted: stream1 goes first
    assert_eq!(bodies, vec!["subject=T&content=B+b&type=stream&to=stream1", "subject=T&content=B+b&type=private&to=user%40example.com"]);
  }

  #[tokio::test]
  async fn test_send_failure_continues() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(401)).expect(2).mount(&server).await;
    let svc = from_url(&format!("zulip://botname@apprise/{}/a/b", "a".repeat(32))).unwrap();
    let ctx = NotifyContext { body: "B".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[test]
  fn test_limits_and_details() {
    let z = parse_zulip(&format!("zulip://botname@apprise/{}", "a".repeat(32)));
    assert_eq!(z.title_maxlen(), 60);
    assert_eq!(z.body_maxlen(), 10000);
    let details = Zulip::static_details();
    assert_eq!(details.service_name, "Zulip");
    assert_eq!(details.protocols, vec!["zulip"]);
    assert!(!details.attachment_support);
  }
}
