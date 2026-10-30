//! Better Stack, either raising incidents for whoever is on call or adding
//! entries to a log source:
//!
//!   betterstack://{api_token}[/{email}][/{team}/{team2}...]
//!   betterstack://{source_token}@{ingesting_host}[:{port}]
//!
//! See <https://betterstack.com/docs/uptime/api/create-a-new-incident/> and
//! <https://betterstack.com/docs/logs/ingesting-data/http/logs/>.

use crate::error::NotifyError;
use crate::notify::incident::{self, arg};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyType;
use crate::utils::email::is_email;
use crate::utils::parse::ParsedUrl;
use crate::utils::plugin_args::parse_list;
use crate::utils::qsd::validate_token;
use async_trait::async_trait;
use serde_json::{Value, json};

/// The ways Better Stack can alert the on-call person, with the payload
/// field that switches each one on (upstream `BETTERSTACK_ALERT_METHODS`).
const ALERT_METHODS: &[(&str, &str)] = &[("call", "call"), ("sms", "sms"), ("email", "email"), ("push", "push"), ("critical", "critical_alert")];

/// Upstream `BETTERSTACK_HTTP_ERROR_MAP`.
const HTTP_ERRORS: &[(u16, &str)] = &[
  (402, "Quota or spending limit reached."),
  (403, "Invalid source token."),
  (404, "Authentication failed."),
  (406, "Payload could not be parsed."),
  (413, "Payload too large."),
];

/// Upstream `BETTERSTACK_LOG_LEVELS`.
fn log_level(notify_type: &NotifyType) -> &'static str {
  match notify_type {
    NotifyType::Info | NotifyType::Success => "info",
    NotifyType::Warning => "warn",
    NotifyType::Failure => "error",
  }
}

/// Where the notification goes.
#[derive(Debug, Clone, PartialEq)]
enum Mode {
  /// Raise an incident that alerts the on-call person.
  Incident,
  /// Add an entry to the log source at this ingesting host and port.
  Logs { host: String, port: Option<u16> },
}

pub struct BetterStack {
  mode: Mode,
  token: String,
  from_addr: Option<String>,
  /// The teams to raise an incident for; team names may hold spaces.
  targets: Vec<String>,
  policy: Option<String>,
  /// The alert methods to switch on; `None` leaves it to the team settings.
  alert: Option<Vec<String>>,
  team_wait: Option<i64>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl BetterStack {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Logs mode keeps the source token in the user field and names an
    // ingesting host; incident mode keeps the API token in the host
    let mut host = url.host.clone();
    let mut token = url.user.clone();
    if token.is_none() && arg(url, "token").is_none() {
      token = host.take();
    }
    if let Some(t) = arg(url, "token") {
      token = Some(t);
    }
    let Some(token) = token.and_then(|t| validate_token(&t)) else {
      tracing::warn!("An invalid Better Stack token was specified.");
      return None;
    };
    let mode = match host {
      Some(host) => Mode::Logs { host, port: url.port },
      None => Mode::Incident,
    };

    // Our path holds an optional requester e-mail followed by teams
    let mut from_addr = arg(url, "from");
    let mut targets: Vec<String> = Vec::new();
    for entry in &url.path_parts {
      if from_addr.is_none() && is_email(entry).is_some() {
        from_addr = Some(entry.clone());
        continue;
      }
      targets.push(entry.clone());
    }
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.extend(parse_list([to], false, true));
    }
    let from_addr = match from_addr {
      Some(addr) => match is_email(&addr) {
        Some(email) => Some(email.full_email),
        None => {
          tracing::warn!("An invalid Better Stack From Email ({}) was specified.", addr);
          return None;
        }
      },
      None => None,
    };

    let alert = match arg(url, "alert") {
      Some(list) => {
        let mut methods: Vec<String> = Vec::new();
        for method in parse_list([list], true, true) {
          let method = method.to_lowercase();
          if !ALERT_METHODS.iter().any(|(name, _)| *name == method) {
            tracing::warn!("An invalid Better Stack alert method ({}) was specified.", method);
            return None;
          }
          // Store each method only once
          if !methods.contains(&method) {
            methods.push(method);
          }
        }
        Some(methods)
      }
      None => None,
    };

    // Seconds to wait before escalating to the whole team
    let team_wait = match arg(url, "team_wait") {
      Some(wait) => match wait.trim().parse::<i64>() {
        Ok(n) if n >= 0 => Some(n),
        _ => {
          tracing::warn!("An invalid Better Stack team wait ({}) was specified.", wait);
          return None;
        }
      },
      None => None,
    };

    Some(Self {
      mode,
      token,
      from_addr,
      targets: parse_list(targets, false, true),
      policy: arg(url, "policy").map(|p| p.trim().to_string()).filter(|p| !p.is_empty()),
      alert,
      team_wait,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Better Stack",
      service_url: Some("https://betterstack.com/"),
      setup_url: Some("https://appriseit.com/services/betterstack/"),
      protocols: vec!["betterstack"],
      description: "Raise Better Stack incidents or add entries to a Better Stack log source.",
      attachment_support: false,
    }
  }

  fn incident_payload(&self, ctx: &NotifyContext) -> Value {
    let mut payload = json!({ "summary": if ctx.title.is_empty() { &ctx.body } else { &ctx.title } });
    // The body becomes the full description when a title is used
    if !ctx.title.is_empty() {
      payload["name"] = json!(ctx.title);
      payload["description"] = json!(ctx.body);
    }
    if let Some(from) = &self.from_addr {
      payload["requester_email"] = json!(from);
    }
    if let Some(policy) = &self.policy {
      payload["policy_id"] = json!(policy);
    }
    if let Some(wait) = self.team_wait {
      payload["team_wait"] = json!(wait);
    }
    if let Some(alert) = &self.alert {
      // Switch on the methods asked for and switch off the rest
      for (method, field) in ALERT_METHODS {
        payload[*field] = json!(alert.iter().any(|m| m == method));
      }
    }
    payload
  }

  fn logs_payload(ctx: &NotifyContext) -> Value {
    let mut payload = json!({
      "dt": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string(),
      "level": log_level(&ctx.notify_type),
      "message": ctx.body,
    });
    // Only include a title when one was given
    if !ctx.title.is_empty() {
      payload["title"] = json!(ctx.title);
    }
    payload
  }

  fn post(&self, client: &reqwest::Client, url: &str, payload: &Value) -> reqwest::RequestBuilder {
    client.post(url).header("User-Agent", APP_ID).header("Authorization", format!("Bearer {}", self.token)).json(payload)
  }
}

#[async_trait]
impl Notify for BetterStack {
  fn schemas(&self) -> &[&str] {
    &["betterstack"]
  }
  fn service_name(&self) -> &str {
    "Better Stack"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(match &self.mode {
      Mode::Incident => vec!["betterstack".into(), "incident".into(), self.token.clone(), String::new(), "443".into()],
      Mode::Logs { host, port } => vec!["betterstack".into(), "logs".into(), self.token.clone(), host.clone(), port.unwrap_or(443).to_string()],
    })
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;
    let accepted = [200, 201, 202, 204];

    if let Mode::Logs { host, port } = &self.mode {
      let url = service_base(&format!("https://{}{}", host, port.map(|p| format!(":{}", p)).unwrap_or_default()));
      let request = self.post(&client, &url, &Self::logs_payload(ctx));
      return incident::exchange(request, "Better Stack logs", &accepted, HTTP_ERRORS).await?.into_result();
    }

    let url = format!("{}/api/v3/incidents", service_base("https://incidents.betterstack.com"));
    let mut payload = self.incident_payload(ctx);
    // Without teams, the team that owns the token gets the incident
    let teams: Vec<Option<&String>> = if self.targets.is_empty() { vec![None] } else { self.targets.iter().map(Some).collect() };
    let mut has_error = false;
    for team in teams {
      // Skip a team that already has this incident so a retry does not
      // raise it twice
      let key = team.map(String::as_str).unwrap_or_default();
      if crate::delivery::is_delivered(key, false) {
        continue;
      }
      if let Some(team) = team {
        payload["team_name"] = json!(team);
      }
      match incident::exchange(self.post(&client, &url, &payload), "Better Stack incident", &accepted, HTTP_ERRORS).await {
        Ok(reply) if reply.delivered => crate::delivery::mark_delivered(key, false),
        Ok(_) => has_error = true,
        Err(e) => {
          incident::log_connection_error("Better Stack incident", &e);
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
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  fn betterstack(url: &str) -> BetterStack {
    BetterStack::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  fn ctx(title: &str, body: &str, notify_type: NotifyType) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), notify_type, ..Default::default() }
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      "betterstack://token",
      "betterstack://token/user@example.com",
      "betterstack://token/user@example.com/Team%20A/team2",
      "betterstack://?token=token&from=user@example.com&to=Ops",
      "betterstack://token/user@example.com?alert=sms,call&policy=123&team_wait=60",
      "betterstack://token@s1234.eu-nbg-2.betterstackdata.com",
      "betterstack://in.logs.betterstack.com:8443?token=token",
    ];
    for url in urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "betterstack://",
      "betterstack://token/?from=invalid",
      "betterstack://token/?alert=fax",
      "betterstack://token/?team_wait=-1",
      "betterstack://token/?team_wait=abc",
    ];
    for url in urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let obj = betterstack("betterstack://abc123/user@example.com/Ops%20Team?alert=SMS,sms,Call&policy=%2042%20&team_wait=0");
    assert_eq!(obj.mode, Mode::Incident);
    assert_eq!(obj.token, "abc123");
    assert_eq!(obj.from_addr.as_deref(), Some("user@example.com"));
    assert_eq!(obj.targets, vec!["Ops Team"]);
    assert_eq!(obj.policy.as_deref(), Some("42"));
    assert_eq!(obj.team_wait, Some(0));
    // Matched without case and stored once (sorted before lower-casing)
    assert_eq!(obj.alert, Some(vec!["call".to_string(), "sms".to_string()]));

    let obj = betterstack("betterstack://?token=token&from=user@example.com&to=Ops,Dev%20Team");
    assert_eq!(obj.mode, Mode::Incident);
    assert_eq!(obj.targets, vec!["Dev Team", "Ops"]);
    assert!(obj.alert.is_none() && obj.team_wait.is_none());

    // A user and host, or a host with ?token=, mean logs mode
    let obj = betterstack("betterstack://srctoken@s1234.eu-nbg-2.betterstackdata.com");
    assert_eq!(obj.mode, Mode::Logs { host: "s1234.eu-nbg-2.betterstackdata.com".into(), port: None });
    assert_eq!(obj.token, "srctoken");
    let obj = betterstack("betterstack://in.logs.betterstack.com:8443?token=token");
    assert_eq!(obj.mode, Mode::Logs { host: "in.logs.betterstack.com".into(), port: Some(8443) });
    assert_eq!(obj.token, "token");

    // Incidents and logs are different connections
    assert_ne!(betterstack("betterstack://abc@in.logs.betterstack.com").url_identifier(), betterstack("betterstack://abc").url_identifier());
  }

  #[tokio::test]
  async fn test_send_incident() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v3/incidents"))
      .and(header("authorization", "Bearer abc123"))
      .and(header("content-type", "application/json"))
      .and(body_json(json!({
        "summary": "Disk <full> & more",
        "name": "Disk <full> & more",
        "description": "Free some _space_ now!",
        "requester_email": "user@example.com",
        "policy_id": "42",
        "team_wait": 0,
        "call": false,
        "sms": true,
        "email": false,
        "push": true,
        "critical_alert": false,
        "team_name": "Ops Team",
      })))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("betterstack://abc123/user@example.com/Ops%20Team?alert=sms,push&policy=42&team_wait=0").unwrap();
    let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("Disk <full> & more", "Free some _space_ now!", NotifyType::Info))).await;
    assert!(result.unwrap());
  }

  #[tokio::test]
  async fn test_send_incident_without_title() {
    // The body is the summary and team settings decide the alerting
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/api/v3/incidents"))
      .and(body_json(json!({"summary": "Something broke"})))
      .respond_with(ResponseTemplate::new(201))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("betterstack://abc123").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "Something broke", NotifyType::Info))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_teams() {
    // One incident per team; a failing team fails the notification but
    // the other is still reached
    let server = MockServer::start().await;
    for (team, status) in [("team1", 404), ("team2", 201)] {
      Mock::given(method("POST"))
        .and(path("/api/v3/incidents"))
        .and(body_json(json!({"summary": "body", "requester_email": "user@example.com", "team_name": team})))
        .respond_with(ResponseTemplate::new(status))
        .expect(1)
        .mount(&server)
        .await;
    }
    let svc = from_url("betterstack://abc123/user@example.com/team2/team1").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "body", NotifyType::Info))).await.unwrap());
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_team() {
    let server = MockServer::start().await;
    let team = |name: &str| json!({"summary": "body", "team_name": name});
    Mock::given(method("POST")).and(path("/api/v3/incidents")).and(body_json(team("team1"))).respond_with(ResponseTemplate::new(201)).mount(&server).await;
    Mock::given(method("POST")).and(path("/api/v3/incidents")).and(body_json(team("team2"))).respond_with(ResponseTemplate::new(500)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("betterstack://abc123/team1/team2?retry=1&wait=0"));
    assert!(!crate::notify::with_test_base(server.uri(), a.notify(&ctx("", "body", NotifyType::Info))).await.is_success());
    let requests = server.received_requests().await.unwrap();
    let teams: Vec<Value> =
      requests.iter().filter(|r| r.url.path() == "/api/v3/incidents").map(|r| serde_json::from_slice::<Value>(&r.body).unwrap()["team_name"].clone()).collect();
    assert_eq!(teams.iter().filter(|t| **t == "team1").count(), 1);
    assert_eq!(teams.iter().filter(|t| **t == "team2").count(), 2);
  }

  #[tokio::test]
  async fn test_send_logs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/"))
      .and(header("authorization", "Bearer srctoken"))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;
    let svc = from_url("betterstack://srctoken@s1234.eu-nbg-2.betterstackdata.com").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("Title", "Body", NotifyType::Failure))).await.unwrap());
    let requests = server.received_requests().await.unwrap();
    let request = requests.iter().find(|r| r.headers.contains_key("authorization")).unwrap();
    let sent: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(sent["message"], "Body");
    assert_eq!(sent["title"], "Title");
    assert_eq!(sent["level"], "error");
    let dt = sent["dt"].as_str().unwrap();
    assert!(dt.ends_with("+00:00"));
    assert!(chrono::DateTime::parse_from_rfc3339(dt).is_ok());

    // No title, and a warning
    let payload = BetterStack::logs_payload(&ctx("", "Body", NotifyType::Warning));
    assert!(payload.get("title").is_none());
    assert_eq!(payload["level"], "warn");
  }

  #[tokio::test]
  async fn test_send_failure() {
    for (url, route) in [("betterstack://token/user@example.com", "/api/v3/incidents"), ("betterstack://token@s1234.eu-nbg-2.betterstackdata.com", "/")] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(route)).respond_with(ResponseTemplate::new(999)).expect(1).mount(&server).await;
      let svc = from_url(url).unwrap();
      let result = crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "body", NotifyType::Info))).await;
      assert!(!matches!(result, Ok(true)), "{}", url);
    }
  }

  #[test]
  fn test_service_details() {
    let details = BetterStack::static_details();
    assert_eq!(details.service_name, "Better Stack");
    assert_eq!(details.protocols, vec!["betterstack"]);
  }
}
