use crate::error::NotifyError;
use crate::notify::custom_json::{basic_auth, custom_args, header_map};
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client};
use crate::utils::parse::{ParsedUrl, parse_list, quote, unquote};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeSet;

/// Nextcloud version assumed unless `?version=` says otherwise. Version 21
/// moved admin notifications into the notifications app.
const DEFAULT_VERSION: i64 = 21;

/// Group ID meaning every user on the server.
const ALL_GROUP: &str = "all";

/// How long a group's members stay in the persistent store (upstream
/// `group_discovery_cache_length_sec`).
const GROUP_CACHE: std::time::Duration = std::time::Duration::from_secs(86400);

/// A `[a-z0-9_-]+` name.
fn is_name(s: &str) -> bool {
  !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Upstream `IS_GROUP`: `#group`, or `all`/`everyone`/`*` with an optional `#`.
fn parse_group(target: &str) -> Option<String> {
  let t = target.trim();
  let bare = t.strip_prefix('#').or_else(|| t.strip_prefix("%23"));
  if let Some(g) = bare
    && is_name(g)
    && !["all", "everyone"].contains(&g.to_lowercase().as_str())
  {
    return Some(g.to_string());
  }
  let name = bare.unwrap_or(t);
  (["all", "everyone", "*"].contains(&name.to_lowercase().as_str())).then(|| ALL_GROUP.to_string())
}

/// Upstream `IS_USER`: a name with an optional `@`.
fn parse_user(target: &str) -> Option<String> {
  let t = target.trim();
  let name = t.strip_prefix('@').or_else(|| t.strip_prefix("%40")).unwrap_or(t);
  is_name(name).then(|| name.to_string())
}

pub struct Nextcloud {
  host: String,
  port: Option<u16>,
  users: Vec<String>,
  groups: BTreeSet<String>,
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  version: i64,
  url_prefix: String,
  headers: Vec<(String, String)>,
  verify_certificate: bool,
  tags: Vec<String>,
}

impl Nextcloud {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    let host = url.host.clone().filter(|h| !h.is_empty())?;

    let mut targets: Vec<String> = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      targets.extend(parse_list(to));
    }
    let mut users = Vec::new();
    let mut groups = BTreeSet::new();
    for target in parse_list(&targets.join(" ")) {
      if let Some(g) = parse_group(&target) {
        groups.insert(g);
      } else if let Some(u) = parse_user(&target) {
        users.push(u);
      } else {
        tracing::warn!("Ignored invalid Nextcloud user/group '{}'", target);
      }
    }

    // Upstream unquotes these a second time
    let version = match url.get("version").filter(|v| !v.is_empty()) {
      Some(v) => match unquote(v).trim().parse::<i64>() {
        Ok(n) if n >= 1 => n,
        _ => {
          tracing::warn!("At invalid Nextcloud version ({}) was specified.", v);
          return None;
        }
      },
      None => DEFAULT_VERSION,
    };

    let url_prefix = url.get("url_prefix").map(unquote).filter(|p| !p.is_empty()).map(|p| format!("/{}", p.trim_matches('/'))).unwrap_or_default();

    Some(Self {
      host,
      port: url.port,
      users,
      groups,
      secure: url.schema == "nclouds",
      user: url.user.clone(),
      password: url.password.clone(),
      version,
      url_prefix,
      headers: custom_args(url, '+'),
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Nextcloud",
      service_url: Some("https://nextcloud.com/"),
      setup_url: Some("https://appriseit.com/services/nextcloud/"),
      protocols: vec!["ncloud", "nclouds"],
      description: "Send Nextcloud notifications.",
      attachment_support: false,
    }
  }

  fn base_url(&self) -> String {
    let schema = if self.secure { "https" } else { "http" };
    let port_str = self.port.map(|p| format!(":{}", p)).unwrap_or_default();
    format!("{}://{}{}{}", schema, self.host, port_str, self.url_prefix)
  }

  fn request(&self, client: &reqwest::Client, method: reqwest::Method, url: &str) -> Result<reqwest::RequestBuilder, NotifyError> {
    let headers = header_map(&[("User-Agent", APP_ID), ("OCS-APIREQUEST", "true"), ("Accept", "application/json")], &self.headers)?;
    let mut req = client.request(method, url).headers(headers);
    if let Some((user, password)) = basic_auth(&self.user, &self.password) {
      req = req.basic_auth(user, Some(password));
    }
    Ok(req)
  }

  /// The users in `group` (or every user for [`ALL_GROUP`]) from the OCS
  /// provisioning API, kept in the persistent store for a day as upstream
  /// does. `Err` when the lookup itself failed.
  async fn group_users(&self, client: &reqwest::Client, group: &str) -> Result<BTreeSet<String>, NotifyError> {
    let cached = crate::storage::with_service_store(|s| s.get(group)).flatten().and_then(|v| serde_json::from_value::<BTreeSet<String>>(v).ok());
    if let Some(users) = cached {
      tracing::trace!("Using Nextcloud cached response for group '{}' query", group);
      return Ok(users);
    }

    let url = if group == ALL_GROUP {
      format!("{}/ocs/v1.php/cloud/users", self.base_url())
    } else {
      format!("{}/ocs/v1.php/cloud/groups/{}", self.base_url(), quote(group, "/"))
    };
    tracing::debug!("Nextcloud group GET URL: {} (cert_verify={})", url, self.verify_certificate);
    crate::notify::throttle().await;
    let resp = self.request(client, reqwest::Method::GET, &url)?.query(&[("format", "json")]).send().await?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status != 200 {
      tracing::warn!("Failed to send Nextcloud Group \"{}\": error={}.", group, status);
      return Err(NotifyError::ServiceError { status, body: text });
    }
    let content: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
      tracing::debug!("Failed to parse Nextcloud JSON response");
      Value::Null
    });
    let users: BTreeSet<String> = match content.pointer("/ocs/data/users") {
      Some(Value::Array(list)) => {
        list.iter().map(|u| u.as_str().map(String::from).unwrap_or_else(|| u.to_string())).map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).collect()
      }
      _ => BTreeSet::new(),
    };
    if users.is_empty() && group == ALL_GROUP {
      // Nothing worth keeping
      tracing::warn!("Failed to retrieve all users from Nextcloud");
      return Ok(users);
    }
    if users.is_empty() {
      tracing::warn!("No users associated with Nextcloud group '{}'", group);
    }
    crate::storage::with_service_store(|s| s.set(group, serde_json::json!(users), Some(GROUP_CACHE), true));
    Ok(users)
  }
}

#[async_trait]
impl Notify for Nextcloud {
  fn schemas(&self) -> &[&str] {
    &["ncloud", "nclouds"]
  }
  fn service_name(&self) -> &str {
    "Nextcloud"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn title_maxlen(&self) -> usize {
    255
  }
  fn body_maxlen(&self) -> usize {
    4000
  }
  /// The server, not its users, decides what the group cache holds
  fn url_identifier(&self) -> Option<Vec<String>> {
    let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "\0".to_string());
    let schema = if self.secure { "nclouds" } else { "ncloud" };
    Some(vec![
      schema.to_string(),
      opt(&self.user),
      opt(&self.password),
      self.host.clone(),
      self.port.map(|p| p.to_string()).unwrap_or_else(|| "\0".to_string()),
      self.url_prefix.clone(),
    ])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let client = build_client(self.verify_certificate)?;

    let mut targets: BTreeSet<String> = self.users.iter().cloned().collect();
    for group in &self.groups {
      match self.group_users(&client, group).await {
        Ok(users) => targets.extend(users),
        Err(e) => {
          tracing::debug!("Nextcloud group discovery failed: {}", e);
          return Ok(false);
        }
      }
    }
    if targets.is_empty() {
      tracing::warn!("There were no Nextcloud targets to notify.");
      return Ok(false);
    }

    let short = if ctx.title.is_empty() { ctx.asset.app_desc.clone() } else { ctx.title.clone() };
    let mut payload = vec![("shortMessage", short)];
    if !ctx.body.is_empty() {
      payload.push(("longMessage", ctx.body.clone()));
    }

    let mut has_error = false;
    for target in &targets {
      // Skip a target that already accepted this message so a retry does
      // not deliver it twice
      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      let target_esc = quote(target, "/");
      let url = if self.version < 21 {
        format!("{}/ocs/v2.php/apps/admin_notifications/api/v1/notifications/{}", self.base_url(), target_esc)
      } else {
        format!("{}/ocs/v2.php/apps/notifications/api/v2/admin_notifications/{}", self.base_url(), target_esc)
      };
      tracing::debug!("Nextcloud v{} POST URL: {} (cert_verify={})", self.version, url, self.verify_certificate);
      crate::notify::throttle().await;
      match self.request(&client, reqwest::Method::POST, &url)?.form(&payload).send().await {
        Ok(r) if r.status().as_u16() == 200 => {
          tracing::info!("Sent Nextcloud v{} Notify \"{}\"", self.version, target);
          crate::delivery::mark_delivered(target, false);
        }
        Ok(r) => {
          tracing::warn!("Failed to send Nextcloud v{} Notify \"{}\": error={}.", self.version, target, r.status().as_u16());
          has_error = true;
        }
        Err(e) => {
          tracing::warn!("A Connection error occurred with Nextcloud v{} Notify \"{}\"", self.version, target);
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
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
  use serde_json::json;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      "ncloud://localhost",
      "ncloud://localhost/admin",
      "ncloud://user@localhost/admin",
      "ncloud://user@localhost?to=user1,user2",
      "ncloud://user@localhost?to=user1,user2&version=20",
      "ncloud://user@localhost?to=user1,user2&version=21",
      "ncloud://user@localhost?to=user1&version=20&url_prefix=/abcd",
      "ncloud://user@localhost?to=user1&version=21&url_prefix=/abcd",
      "ncloud://user:pass@localhost/user1/user2",
      "ncloud://user:pass@localhost/#group1/#group2/#group1",
      "ncloud://user:pass@localhost:8080/admin",
      "nclouds://user:pass@localhost/admin",
      "nclouds://user:pass@localhost:8080/admin/",
      "nclouds://user:pass@localhost:8080/#group/",
      "ncloud://localhost:8080/admin?+HeaderKey=HeaderValue",
      "ncloud://user:pass@localhost:8083/user1/user2/user3",
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "ncloud://:@/",
      "ncloud://",
      "nclouds://",
      "ncloud://user@localhost?to=user1,user2&version=invalid",
      "ncloud://user@localhost?to=user1,user2&version=0",
      "ncloud://user@localhost?to=user1,user2&version=-23",
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  fn nc(url: &str) -> Nextcloud {
    Nextcloud::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_targets() {
    let n = nc("ncloud://user:pass@localhost/#group1/#group2/#group1/@bob/alice/*/bad!?to=%23devs,everyone,carol");
    assert_eq!(n.users, vec!["bob", "alice", "carol"]);
    assert_eq!(n.groups.iter().cloned().collect::<Vec<_>>(), vec!["all", "devs", "group1", "group2"]);
    assert_eq!(parse_group("#all"), Some("all".to_string()));
    assert_eq!(parse_group("%23Dev"), Some("Dev".to_string()));
    assert_eq!(parse_group("dev"), None);
    assert_eq!(parse_user("%40bob"), Some("bob".to_string()));
    assert_eq!(nc("ncloud://localhost").version, 21);
    assert_eq!(nc("ncloud://localhost?url_prefix=abcd/").url_prefix, "/abcd");
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  #[tokio::test]
  async fn test_send_v21_and_headers() {
    let server = MockServer::start().await;
    let p = "/ocs/v2.php/apps/notifications/api/v2/admin_notifications/admin";
    Mock::given(method("POST"))
      .and(path(p))
      .and(header("OCS-APIREQUEST", "true"))
      .and(header("Accept", "application/json"))
      .and(header("HeaderKey", "Header+Value"))
      .and(header("Authorization", "Basic dXNlcjpwYXNz"))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let n = nc(&format!("ncloud://user:pass@localhost:{}/admin?+HeaderKey=Header+Value", server.address().port()));
    assert!(n.send(&ctx("title", "body")).await.unwrap());
    let got: Vec<(String, String)> = url::form_urlencoded::parse(&requests_to(&server, "POST", p).await[0].body).into_owned().collect();
    assert_eq!(got, vec![("shortMessage".to_string(), "title".to_string()), ("longMessage".to_string(), "body".to_string())]);
  }

  #[tokio::test]
  async fn test_send_v20_prefix_and_defaults() {
    let server = MockServer::start().await;
    let p = "/abcd/ocs/v2.php/apps/admin_notifications/api/v1/notifications/admin";
    Mock::given(method("POST")).and(path(p)).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;

    let n = nc(&format!("ncloud://localhost:{}/admin/?version=20&url_prefix=/abcd", server.address().port()));
    // An empty title falls back to the app description; an empty body is left out
    assert!(n.send(&ctx("", "")).await.unwrap());
    let requests = requests_to(&server, "POST", p).await;
    assert!(requests[0].headers.get("Authorization").is_none());
    let got: Vec<(String, String)> = url::form_urlencoded::parse(&requests[0].body).into_owned().collect();
    assert_eq!(got, vec![("shortMessage".to_string(), "Apprise Notifications".to_string())]);
  }

  #[tokio::test]
  async fn test_groups_and_all() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/ocs/v1.php/cloud/groups/devs"))
      .and(query_param("format", "json"))
      .and(header("OCS-APIREQUEST", "true"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ocs": {"meta": {"status": "ok"}, "data": {"users": ["user1", "user2"]}}})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path("/ocs/v1.php/cloud/users"))
      .and(query_param("format", "json"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ocs": {"data": {"users": ["user1", " user3 ", ""]}}})))
      .expect(1)
      .mount(&server)
      .await;
    for u in ["user1", "user2", "user3", "user4"] {
      Mock::given(method("POST"))
        .and(path(format!("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/{}", u)))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    }
    let n = nc(&format!("ncloud://admin:pass@localhost:{}/%23devs/all/user4", server.address().port()));
    assert!(n.send(&ctx("title", "body")).await.unwrap());
  }

  #[tokio::test]
  async fn test_failures() {
    // A failed group lookup fails the send before anything is posted
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/ocs/v1.php/cloud/groups/devs")).respond_with(ResponseTemplate::new(500)).expect(1).mount(&server).await;
    Mock::given(method("POST"))
      .and(path("/ocs/v2.php/apps/notifications/api/v2/admin_notifications/bob"))
      .respond_with(ResponseTemplate::new(200))
      .expect(0)
      .mount(&server)
      .await;
    assert!(!nc(&format!("ncloud://localhost:{}/%23devs/bob", server.address().port())).send(&ctx("t", "b")).await.unwrap());

    // An empty or unparseable group leaves nobody to notify
    let server = MockServer::start().await;
    Mock::given(method("GET"))
      .and(path("/ocs/v1.php/cloud/users"))
      .respond_with(ResponseTemplate::new(200).set_body_string("{"))
      .expect(1)
      .mount(&server)
      .await;
    assert!(!nc(&format!("ncloud://localhost:{}/all", server.address().port())).send(&ctx("t", "b")).await.unwrap());
    assert!(!nc("ncloud://localhost").send(&ctx("t", "b")).await.unwrap());

    // One user failing still notifies the others; only 200 is success
    let server = MockServer::start().await;
    let base = "/ocs/v2.php/apps/notifications/api/v2/admin_notifications";
    Mock::given(method("POST")).and(path(format!("{}/a", base))).respond_with(ResponseTemplate::new(204)).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path(format!("{}/b", base))).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    assert!(!nc(&format!("ncloud://localhost:{}/a/b", server.address().port())).send(&ctx("t", "b")).await.unwrap());
  }

  #[tokio::test]
  async fn test_group_members_come_from_the_store() {
    let server = MockServer::start().await;
    // One lookup across both runs
    Mock::given(method("GET"))
      .and(path("/ocs/v1.php/cloud/groups/devs"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ocs": {"data": {"users": ["user1"]}}})))
      .expect(1)
      .mount(&server)
      .await;
    let base = "/ocs/v2.php/apps/notifications/api/v2/admin_notifications";
    Mock::given(method("POST")).and(path(format!("{}/user1", base))).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;

    let tmp = tempfile::tempdir().unwrap();
    for _ in 0..2 {
      // A fresh instance each time, as a new process would have
      let asset = crate::AppriseAsset { storage_path: Some(tmp.path().to_path_buf()), ..Default::default() };
      let mut apprise = crate::Apprise::with_asset(asset);
      assert!(apprise.add(&format!("ncloud://admin:pass@localhost:{}/%23devs", server.address().port())));
      assert!(apprise.notify(&ctx("t", "b")).await.is_success());
      apprise.flush_stores();
    }
  }

  #[tokio::test]
  async fn test_retry_skips_users_that_accepted() {
    let server = MockServer::start().await;
    let base = "/ocs/v2.php/apps/notifications/api/v2/admin_notifications";
    Mock::given(method("POST")).and(path(format!("{}/a", base))).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    // The second user fails once, then accepts the retry
    Mock::given(method("POST")).and(path(format!("{}/b", base))).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path(format!("{}/b", base))).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    let mut apprise = crate::Apprise::new();
    assert!(apprise.add(&format!("ncloud://localhost:{}/a/b?retry=1&wait=0", server.address().port())));
    assert!(apprise.notify(&ctx("t", "b")).await.is_success());
  }
}
