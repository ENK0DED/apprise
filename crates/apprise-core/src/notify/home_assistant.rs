use crate::error::NotifyError;
use crate::notify::apprise_api::uuid4;
use crate::notify::custom_json::basic_auth;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::{ParsedUrl, parse_bool};
use crate::utils::qsd;
use async_trait::async_trait;
use serde_json::{Value, json};

/// Port used by `hassio://` when none is given.
const DEFAULT_INSECURE_PORT: u16 = 8123;

/// Service targets sent per call with `?batch=yes`.
const BATCH_SIZE: usize = 10;

/// Domain of a service entry that names none.
const DEFAULT_DOMAIN: &str = "notify";

/// A service call: `domain.service` plus the targets passed to it.
#[derive(Debug, Clone, PartialEq)]
struct Service {
  domain: String,
  service: String,
  targets: Vec<String>,
}

fn is_name_char(c: char) -> bool {
  c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Upstream `RE_IS_LONG_LIVED_TOKEN`: `8 hex . 64 hex` or a three-part JWT.
fn is_long_lived_token(t: &str) -> bool {
  let parts: Vec<&str> = t.split('.').collect();
  let hex = |s: &str, n: usize| s.len() == n && s.chars().all(|c| c.is_ascii_hexdigit());
  (parts.len() == 2 && hex(parts[0], 8) && hex(parts[1], 64)) || (parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(is_name_char)))
}

/// Upstream `is_domain_service_target`: match `[domain.]service[:t1,t2]` at
/// the start of `entry`; trailing text is ignored.
fn parse_service(entry: &str) -> Option<Service> {
  let s = entry.trim_start();
  let first: String = s.chars().take_while(|c| is_name_char(*c)).collect();
  if first.is_empty() {
    return None;
  }
  let mut rest = &s[first.len()..];
  let (domain, service) = match rest.strip_prefix('.') {
    Some(after) => {
      let service: String = after.chars().take_while(|c| is_name_char(*c)).collect();
      if service.is_empty() {
        // "name." is just a service
        (DEFAULT_DOMAIN.to_string(), first)
      } else {
        rest = &after[service.len()..];
        (first, service)
      }
    }
    None => (DEFAULT_DOMAIN.to_string(), first),
  };
  let targets = match rest.strip_prefix(':') {
    Some(after) => qsd::parse_list(&after.chars().take_while(|c| is_name_char(*c) || *c == ',').collect::<String>()),
    None => Vec::new(),
  };
  Some(Service { domain, service, targets })
}

/// Split raw entries into service calls. Whitespace and `;` separate
/// entries; a comma separates services unless it follows `service:`, where
/// it separates that service's targets.
///
/// Upstream's detection regex needs at least two targets after a colon, so
/// it turns `notify.mobile:device1` into a service named `device1` and drops
/// the third of three targets. That is a bug against its own documented
/// `[domain.]service[:target]` format; this parser keeps the targets.
fn parse_services(entries: &[String]) -> (Vec<Service>, Vec<String>) {
  let mut services = Vec::new();
  let mut invalid = Vec::new();
  for entry in entries {
    for chunk in entry.split(|c: char| c.is_whitespace() || ";[]".contains(c)).filter(|c| !c.is_empty()) {
      let candidates: Vec<&str> = if chunk.contains(':') { vec![chunk] } else { chunk.split(',').filter(|c| !c.is_empty()).collect() };
      for candidate in candidates {
        match parse_service(candidate) {
          Some(s) => services.push(s),
          None => {
            tracing::warn!("Dropped invalid [domain.]service[:target] entry ({}) specified.", candidate);
            invalid.push(candidate.to_string());
          }
        }
      }
    }
  }
  (services, invalid)
}

pub struct HomeAssistant {
  host: String,
  port: Option<u16>,
  user: Option<String>,
  password: Option<String>,
  access_token: String,
  secure: bool,
  nid: Option<String>,
  batch: bool,
  /// Path in front of `/api/services/...` for service calls.
  prefix: String,
  /// Service calls; empty means a persistent notification.
  services: Vec<Service>,
  /// Service entries were given (even if none of them were valid).
  has_targets: bool,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl HomeAssistant {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().unwrap_or_default();
    let tokens = &url.path_parts;

    // The token comes from ?accesstoken= or ?token= (the path then lists
    // targets), else the first path element that looks like a token (the
    // others are targets), else the last path element (and no targets)
    let mut targets: Vec<String> = Vec::new();
    let access_token = if let Some(t) = qsd::get(url, "accesstoken").filter(|t| !t.is_empty()).or_else(|| qsd::get(url, "token").filter(|t| !t.is_empty())) {
      targets = tokens.clone();
      Some(qsd::unquote(&t))
    } else if let Some(idx) = tokens.iter().position(|t| is_long_lived_token(t)) {
      targets.extend(tokens[idx + 1..].iter().rev().cloned());
      targets.extend(tokens[..idx].iter().rev().cloned());
      Some(tokens[idx].clone())
    } else {
      tokens.last().cloned()
    };

    if let Some(to) = qsd::get(url, "to").filter(|t| !t.is_empty()) {
      targets.extend(qsd::unquote(&to).split(|c: char| " \t\r\n,\\/".contains(c)).filter(|s| !s.is_empty()).map(String::from));
    }

    let Some(access_token) = access_token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) else {
      tracing::warn!("An invalid Home Assistant Long-Lived Access Token was specified.");
      return None;
    };

    let nid = match qsd::get(url, "nid").filter(|n| !n.is_empty()).map(|n| qsd::unquote(&n).trim().to_string()) {
      Some(n) if !n.is_empty() && n.chars().all(is_name_char) => Some(n),
      Some(n) => {
        tracing::warn!("An invalid Home Assistant Notification Identifier ({}) was specified.", n);
        return None;
      }
      None => None,
    };

    let (services, _) = parse_services(&targets);
    let secure = url.schema == "hassios";
    Some(Self {
      host,
      port: url.port.or(if secure { None } else { Some(DEFAULT_INSECURE_PORT) }),
      user: url.user.clone(),
      password: url.password.clone(),
      access_token,
      secure,
      nid,
      batch: qsd::get(url, "batch").is_some_and(|b| parse_bool(&b)),
      prefix: qsd::get(url, "prefix").filter(|p| !p.is_empty()).map(|p| qsd::unquote(&p)).unwrap_or_default(),
      has_targets: !targets.is_empty(),
      services,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "HomeAssistant",
      service_url: Some("https://www.home-assistant.io/"),
      setup_url: Some("https://appriseit.com/services/homeassistant/"),
      protocols: vec!["hassio", "hassios"],
      description: "Send persistent notifications or call notify services in Home Assistant.",
      attachment_support: false,
    }
  }

  fn base_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    match self.port {
      Some(p) => format!("{}://{}:{}", schema, self.host, p),
      None => format!("{}://{}", schema, self.host),
    }
  }

  async fn post(&self, client: &reqwest::Client, url: &str, payload: &Value) -> Result<bool, NotifyError> {
    tracing::debug!("Home Assistant POST URL: {} (cert_verify={})", url, self.verify_certificate);
    let req = client.post(url).header("User-Agent", APP_ID).header("Content-Type", "application/json").body(payload.to_string());
    // With a user, `requests` replaces the bearer token with basic auth
    let req = match basic_auth(&self.user, &self.password) {
      Some((user, password)) => req.basic_auth(user, Some(password)),
      None => req.header("Authorization", format!("Bearer {}", self.access_token)),
    };
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    if status != 200 {
      tracing::warn!("Failed to send Home Assistant notification: error={}.", status);
      return Err(NotifyError::ServiceError { status, body: resp.text().await.unwrap_or_default() });
    }
    tracing::info!("Sent Home Assistant notification.");
    Ok(true)
  }
}

#[async_trait]
impl Notify for HomeAssistant {
  fn schemas(&self) -> &[&str] {
    &["hassio", "hassios"]
  }
  fn service_name(&self) -> &str {
    "HomeAssistant"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let payload = json!({"title": ctx.title, "message": ctx.body});

    if !self.has_targets {
      // The prefix applies to service calls only, as upstream
      let url = format!("{}/api/services/persistent_notification/create", self.base_url());
      let mut payload = payload;
      payload["notification_id"] = json!(self.nid.clone().unwrap_or_else(uuid4));
      return self.post(&client, &url, &payload).await;
    }
    if self.services.is_empty() {
      tracing::warn!("No valid Home Assistant service targets to notify.");
      return Ok(false);
    }

    let batch_size = if self.batch { BATCH_SIZE } else { 1 };
    for s in &self.services {
      let url = format!("{}{}/api/services/{}/{}", self.base_url(), self.prefix.trim_end_matches('/'), s.domain, s.service);
      if s.targets.is_empty() {
        self.post(&client, &url, &payload).await?;
        continue;
      }
      for chunk in s.targets.chunks(batch_size) {
        let mut p = payload.clone();
        p["targets"] = json!(chunk);
        self.post(&client, &url, &p).await?;
      }
    }
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "hassio://localhost/long.lived.token",
      "hassio://localhost/prefix/path/long.lived.token",
      "hassio://localhost/long.lived.token?prefix=/ha",
      "hassio://localhost/service/?token=long.lived.token&prefix=/ha",
      "hassio://localhost/?token=long.lived.token&prefix=/ha&to=service",
      "hassio://localhost/service/$%/?token=long.lived.token&prefix=/ha",
      "hassio://localhost/%only%/%invalid%/?token=lng.lived.token&prefix=/ha",
      "hassio://localhost/service/?accesstoken=long.lived.token",
      "hassio://user:pass@localhost/long.lived.token/",
      "hassio://localhost:80/long.lived.token",
      "hassio://user@localhost:8123/long.lived.token",
      "hassios://localhost/long.lived.token?nid=abcd",
      "hassios://user:pass@localhost/long.lived.token",
      "hassios://localhost:8443/path/long.lived.token/",
      "hassio://localhost:8123/a/path?token=long.lived.token",
      "hassios://user:password@localhost:80/long.lived.token/",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    for url in ["hassio://:@/", "hassio://", "hassios://", "hassio://user@localhost", "hassios://localhost/long.lived.token?nid=!%"] {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn svc(domain: &str, service: &str, targets: &[&str]) -> Service {
    Service { domain: domain.into(), service: service.into(), targets: targets.iter().map(|t| t.to_string()).collect() }
  }

  fn ha(url: &str) -> HomeAssistant {
    HomeAssistant::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_service_parsing() {
    let parse = |s: &str| parse_services(&[s.to_string()]).0;
    assert_eq!(parse("mobile"), vec![svc("notify", "mobile", &[])]);
    assert_eq!(parse("notify.mobile"), vec![svc("notify", "mobile", &[])]);
    assert_eq!(parse("mobile:d2,d1"), vec![svc("notify", "mobile", &["d1", "d2"])]);
    assert_eq!(parse("notify.mobile:d1,d2 tts.say"), vec![svc("notify", "mobile", &["d1", "d2"]), svc("tts", "say", &[])]);
    assert_eq!(parse("a,b"), vec![svc("notify", "a", &[]), svc("notify", "b", &[])]);
    assert_eq!(parse("a b,c.d"), vec![svc("notify", "a", &[]), svc("notify", "b", &[]), svc("c", "d", &[])]);
    assert_eq!(parse("x.y:t1,,t2"), vec![svc("x", "y", &["t1", "t2"])]);
    assert_eq!(parse("bad!"), vec![svc("notify", "bad", &[])]);
    // Single and triple targets stay with their service (upstream bug)
    assert_eq!(parse("notify.mobile:device1"), vec![svc("notify", "mobile", &["device1"])]);
    assert_eq!(parse("domain.svc:t1,t2,t3"), vec![svc("domain", "svc", &["t1", "t2", "t3"])]);
    let (services, invalid) = parse_services(&["!!".to_string(), "$%".to_string()]);
    assert!(services.is_empty());
    assert_eq!(invalid, vec!["!!", "$%"]);
  }

  #[test]
  fn test_url_parsing() {
    // Path elements around the token are targets, nearest last
    let h = ha("hassio://localhost/long.lived.token/serviceA:target1,target2/service2/domain1.service3");
    assert_eq!(h.access_token, "long.lived.token");
    assert_eq!(h.services, vec![svc("domain1", "service3", &[]), svc("notify", "service2", &[]), svc("notify", "serviceA", &["target1", "target2"])]);
    let h = ha("hassio://localhost/prefix/path/long.lived.token");
    assert_eq!(h.services, vec![svc("notify", "path", &[]), svc("notify", "prefix", &[])]);

    // Without a token-shaped element the last one is the token
    let h = ha("hassio://localhost/p/tok");
    assert_eq!((h.access_token.as_str(), h.has_targets, h.port), ("tok", false, Some(8123)));

    let h = ha("hassio://localhost/service/?token=long.lived.token&prefix=/ha&to=x:t1,t2");
    assert_eq!(h.services, vec![svc("notify", "service", &[]), svc("notify", "x", &["t1"]), svc("notify", "t2", &[])]);
    assert_eq!(h.prefix, "/ha");

    let h = ha("hassio://localhost/%only%/%invalid%/?token=lng.lived.token");
    assert!(h.has_targets && h.services.is_empty());

    assert_eq!(ha("hassios://localhost/long.lived.token").port, None);
    assert_eq!(ha("hassio://localhost:80/long.lived.token").port, Some(80));
    assert!(is_long_lived_token(&format!("{}.{}", "a".repeat(8), "0".repeat(64))));
    assert!(!is_long_lived_token("a.b"));
  }

  fn ctx() -> NotifyContext {
    NotifyContext { title: "title".into(), body: "test-body".into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_persistent_notification() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .and(header("Authorization", "Bearer long.lived.token"))
      .and(header("Content-Type", "application/json"))
      .and(header("User-Agent", APP_ID))
      .and(body_json(json!({"title": "title", "message": "test-body", "notification_id": "abcd"})))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let h = ha(&format!("hassio://localhost:{}/long.lived.token?nid=abcd&prefix=/ha", server.address().port()));
    assert!(h.send(&ctx()).await.unwrap());

    // Without ?nid= a fresh UUID is used
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;
    let h = ha(&format!("hassio://user@localhost:{}/long.lived.token", server.address().port()));
    assert!(h.send(&ctx()).await.unwrap());
    let requests = requests_to(&server, "POST", "/api/services/persistent_notification/create").await;
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["notification_id"].as_str().unwrap().len(), 36);
    // "user:None": basic auth replaces the bearer token, as with `requests`
    assert_eq!(requests[0].headers.get_all("Authorization").iter().collect::<Vec<_>>(), vec!["Basic dXNlcjpOb25l"]);
  }

  #[tokio::test]
  async fn test_service_calls_unbatched_and_batched() {
    for (batch, expected) in [("no", vec![vec!["target1"], vec!["target2"]]), ("yes", vec![vec!["target1", "target2"]])] {
      let server = MockServer::start().await;
      Mock::given(method("POST"))
        .and(path("/ha/api/services/domain1/service3"))
        .and(body_json(json!({"title": "title", "message": "test-body"})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
      Mock::given(method("POST")).and(path("/ha/api/services/notify/service2")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
      Mock::given(method("POST"))
        .and(path("/ha/api/services/notify/serviceA"))
        .respond_with(ResponseTemplate::new(200))
        .expect(expected.len() as u64)
        .mount(&server)
        .await;

      let h = ha(&format!(
        "hassio://localhost:{}/long.lived.token/serviceA:target1,target2/service2/domain1.service3?batch={}&prefix=/ha/",
        server.address().port(),
        batch
      ));
      assert!(h.send(&ctx()).await.unwrap());
      // Only the service calls, in the order they were made
      let requests: Vec<wiremock::Request> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().starts_with("/ha/api/services/"))
        .collect();
      let paths: Vec<&str> = requests.iter().map(|r| r.url.path()).collect();
      assert_eq!(&paths[..2], &["/ha/api/services/domain1/service3", "/ha/api/services/notify/service2"]);
      let targets: Vec<Value> = requests[2..].iter().map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["targets"].clone()).collect();
      assert_eq!(targets, expected.iter().map(|t| json!(t)).collect::<Vec<_>>());
    }
  }

  #[tokio::test]
  async fn test_failures() {
    // A failed call stops the run
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/api/services/notify/serviceA")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/api/services/notify/service2")).respond_with(ResponseTemplate::new(404)).expect(1).mount(&server).await;
    let h = ha(&format!("hassio://localhost:{}/long.lived.token/service2:target3,target4/serviceA:target1", server.address().port()));
    assert!(matches!(h.send(&ctx()).await, Err(NotifyError::ServiceError { status: 404, .. })));

    // Only 200 is success
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/services/persistent_notification/create"))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    assert!(ha(&format!("hassio://localhost:{}/long.lived.token", server.address().port())).send(&ctx()).await.is_err());

    // Every service entry was invalid
    assert!(!ha("hassio://localhost/%only%/%invalid%/?token=lng.lived.token").send(&ctx()).await.unwrap());
    assert!(ha("hassio://127.0.0.1:1/long.lived.token").send(&ctx()).await.is_err());
  }
}
