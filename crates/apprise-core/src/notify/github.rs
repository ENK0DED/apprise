use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::parse::ParsedUrl;

/// Supported GitHub REST API version
const API_VERSION: &str = "2022-11-28";

/// Default category for new discussions
const DEFAULT_CATEGORY: &str = "General";

/// Finds a discussion's internal ID from its number
const QUERY_DISCUSSION: &str =
  "query($owner: String!, $repo: String!, $number: Int!) { repository(owner: $owner, name: $repo) { discussion(number: $number) { id } } }";

/// Finds a repository's internal ID and its discussion categories
const QUERY_CATEGORIES: &str =
  "query($owner: String!, $repo: String!) { repository(owner: $owner, name: $repo) { id discussionCategories(first: 100) { nodes { id name slug } } } }";

/// Adds a comment to a discussion
const MUTATION_COMMENT: &str = "mutation($id: ID!, $body: String!) { addDiscussionComment(input: {discussionId: $id, body: $body}) { comment { id } } }";

/// Starts a new discussion
const MUTATION_DISCUSSION: &str = "mutation($repo: ID!, $category: ID!, $title: String!, $body: String!) { createDiscussion(input: {repositoryId: $repo, categoryId: $category, title: $title, body: $body}) { discussion { number } } }";

/// GitHub notification targets
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
  /// An issue (opened new, or commented on)
  Issue,
  /// A pull request (commented on only)
  Pull,
  /// A discussion (started new, or commented on)
  Discussion,
}

impl Kind {
  /// Accept singular and plural target names from GitHub URLs
  fn parse(s: &str) -> Option<Self> {
    match s.to_lowercase().as_str() {
      "issues" | "issue" => Some(Self::Issue),
      "pull" | "pulls" => Some(Self::Pull),
      "discussions" | "discussion" => Some(Self::Discussion),
      _ => None,
    }
  }
}

/// GitHub issues, pull request comments and discussions.
///
/// `github://{token}@{owner}/{repo}[/{kind}[/{number}]]`; the path matches the
/// browser URL, so `/pull/12/files` and `/issues/new` are accepted.
pub struct GitHub {
  token: String,
  owner: String,
  repo: String,
  kind: Kind,
  number: Option<u64>,
  labels: Vec<String>,
  assignees: Vec<String>,
  category: String,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Issue, pull request, or discussion number (`^\s*#?[1-9][0-9]{0,9}\s*$`)
fn parse_number(value: &str) -> Option<u64> {
  let value = value.trim();
  let digits = value.strip_prefix('#').unwrap_or(value);
  if digits.is_empty() || digits.len() > 10 || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
    return None;
  }
  digits.parse().ok()
}

/// Upstream `parse_list()`: split on whitespace, commas, semicolons and
/// brackets, then de-duplicate and sort.
fn parse_list(value: &str) -> Vec<String> {
  let mut items: Vec<String> =
    value.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '[' | ']')).filter(|s| !s.is_empty()).map(String::from).collect();
  items.sort();
  items.dedup();
  items
}

/// Every character of a non-empty `value` matches `allowed`
fn matches_all(value: &str, allowed: impl Fn(char) -> bool) -> bool {
  !value.is_empty() && value.chars().all(allowed)
}

/// Python truthiness for a JSON value
fn truthy(value: &Value) -> bool {
  match value {
    Value::Null => false,
    Value::Bool(b) => *b,
    Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
    Value::String(s) => !s.is_empty(),
    Value::Array(a) => !a.is_empty(),
    Value::Object(o) => !o.is_empty(),
  }
}

impl GitHub {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The owner is stored in the host field
    let owner = url.host.clone().filter(|h| !h.is_empty())?;

    // The token is stored in the user field; ?token= overrides it
    let mut token = url.user.clone();
    if let Some(t) = url.get("token").filter(|t| !t.is_empty()) {
      token = Some(t.to_string());
    }
    let token = token.filter(|t| matches_all(t, |c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))?;
    if !matches_all(&owner, |c| c.is_ascii_alphanumeric() || c == '-') {
      tracing::warn!("An invalid GitHub owner ({}) was specified.", owner);
      return None;
    }

    // The remaining path follows a GitHub browser URL
    let mut entries = url
      .path
      .split(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n' | ',' | '\\' | '/'))
      .filter(|s| !s.is_empty())
      .map(|s| urlencoding::decode(s).map(|d| d.into_owned()).unwrap_or_else(|_| s.to_string()));

    let repo = entries.next()?;
    if !matches_all(&repo, |c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')) {
      tracing::warn!("An invalid GitHub repository ({}) was specified.", repo);
      return None;
    }

    // Target type; issues are the default
    let kind = match entries.next() {
      Some(k) => match Kind::parse(&k) {
        Some(kind) => kind,
        None => {
          tracing::warn!("An unsupported GitHub path ({}) was specified.", k);
          return None;
        }
      },
      None => Kind::Issue,
    };

    // Target number; /new means create a new issue or discussion
    let rest: Vec<String> = entries.collect();
    let (number, suffix) = match rest.split_first() {
      Some((first, suffix)) if first.eq_ignore_ascii_case("new") => (None, suffix),
      Some((first, suffix)) => (Some(first.as_str()), suffix),
      None => (None, &rest[..]),
    };

    // Ignore browser suffixes such as /files, but reject another number
    if suffix.iter().any(|e| parse_number(e).is_some()) {
      tracing::warn!("An invalid GitHub number was specified.");
      return None;
    }

    let number = match number {
      Some(n) => match parse_number(n) {
        Some(n) => Some(n),
        None => {
          tracing::warn!("An invalid GitHub number ({}) was specified.", n);
          return None;
        }
      },
      None => None,
    };

    // A pull request can only be commented on, never created
    if kind == Kind::Pull && number.is_none() {
      tracing::warn!("A GitHub pull request number must be specified.");
      return None;
    }

    // Labels and assignees only apply when opening a new issue
    let labels = url.get("labels").map(parse_list).unwrap_or_default();
    let assignees = url.get("assignees").map(parse_list).unwrap_or_default();

    // Category for new discussions
    let category = url.get("category").map(str::trim).filter(|c| !c.is_empty()).unwrap_or(DEFAULT_CATEGORY).to_string();

    Some(Self { token, owner, repo, kind, number, labels, assignees, category, verify_certificate: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "GitHub",
      service_url: Some("https://github.com/"),
      setup_url: Some("https://appriseit.com/services/github/"),
      protocols: vec!["github"],
      description: "Open or comment on GitHub issues, pull requests and discussions.",
      attachment_support: false,
    }
  }

  fn status_str(status: u16) -> &'static str {
    match status {
      401 => "Unauthorized - Invalid Token.",
      403 => "Forbidden - Missing permission or rate limited.",
      404 => "Not Found - Unknown repository or issue, or no access to it.",
      410 => "Gone - Issues are disabled on this repository.",
      422 => "Validation Failed - GitHub rejected the request.",
      _ => "",
    }
  }

  /// POST to GitHub and return the parsed response object (empty when the
  /// reply is not a JSON object).
  async fn post(&self, client: &reqwest::Client, url: &str, payload: &Value) -> Result<Map<String, Value>, NotifyError> {
    tracing::debug!("GitHub POST URL: {} (cert_verify={})", url, self.verify_certificate);
    tracing::debug!("GitHub Payload: {}", payload);
    let resp = client
      .post(url)
      .header("User-Agent", APP_ID)
      .header("Accept", "application/vnd.github+json")
      .header("Authorization", format!("Bearer {}", self.token))
      .header("Content-Type", "application/json")
      .header("X-GitHub-Api-Version", API_VERSION)
      .body(payload.to_string())
      .send()
      .await?;

    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    // The HTTP status remains useful without a readable body
    let content = match serde_json::from_str::<Value>(&text) {
      Ok(Value::Object(map)) => map,
      _ => Map::new(),
    };

    if !matches!(status, 200 | 201) {
      let status_str = Self::status_str(status);
      tracing::warn!("Failed to send GitHub notification: {}{}error={}.", status_str, if status_str.is_empty() { "" } else { ", " }, status);
      return Err(NotifyError::ServiceError { status, body: text });
    }

    // GraphQL reports its errors inside a successful response
    if let Some(errors) = content.get("errors").filter(|e| truthy(e)) {
      let list = match errors {
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
      };
      let message = list
        .iter()
        .map(|e| match e {
          Value::Object(o) => match o.get("message") {
            Some(Value::String(s)) => s.clone(),
            Some(m) => m.to_string(),
            None => e.to_string(),
          },
          Value::String(s) => s.clone(),
          other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ");
      tracing::warn!("Failed to send GitHub notification: {}", message);
      return Err(NotifyError::ServiceError { status, body: message });
    }

    tracing::info!("Sent GitHub notification.");
    Ok(content)
  }

  /// Comment on a discussion, or start a new one.
  async fn send_discussion(&self, client: &reqwest::Client, body: &str, title: &str) -> Result<bool, NotifyError> {
    let graphql_url = format!("{}/graphql", service_base("https://api.github.com"));

    if let Some(number) = self.number {
      // Look up the discussion's internal ID
      let query = json!({
        "query": QUERY_DISCUSSION,
        "variables": { "owner": self.owner, "repo": self.repo, "number": number },
      });
      let content = self.post(client, &graphql_url, &query).await?;
      let discussion_id =
        content.get("data").and_then(|d| d.get("repository")).and_then(|r| r.get("discussion")).and_then(|d| d.get("id")).filter(|id| truthy(id));
      let Some(discussion_id) = discussion_id else {
        tracing::warn!("GitHub discussion {} was not found in {}/{}.", number, self.owner, self.repo);
        return Ok(false);
      };

      // Add the comment
      let mutation = json!({
        "query": MUTATION_COMMENT,
        "variables": { "id": discussion_id, "body": body },
      });
      self.post(client, &graphql_url, &mutation).await?;
      return Ok(true);
    }

    // Look up the repository's internal ID and its categories
    let query = json!({
      "query": QUERY_CATEGORIES,
      "variables": { "owner": self.owner, "repo": self.repo },
    });
    let content = self.post(client, &graphql_url, &query).await?;

    // Match the category by name or URL slug; a reply shaped unexpectedly
    // leaves nothing to match against
    let categories = Self::read_categories(&content);
    let ids = categories.as_ref().and_then(|(repository_id, category_ids, _)| {
      let category_id = category_ids.get(&self.category.to_lowercase())?;
      (truthy(repository_id) && truthy(category_id)).then(|| (repository_id.clone(), category_id.clone()))
    });
    let Some((repository_id, category_id)) = ids else {
      let names = categories.map(|(_, _, names)| names.join(", ")).filter(|n| !n.is_empty()).unwrap_or_else(|| "none".to_string());
      tracing::warn!("GitHub discussion category {} was not found in {}/{}; available: {}.", self.category, self.owner, self.repo, names);
      return Ok(false);
    };

    // Start the discussion
    let mutation = json!({
      "query": MUTATION_DISCUSSION,
      "variables": { "repo": repository_id, "category": category_id, "title": title, "body": body },
    });
    self.post(client, &graphql_url, &mutation).await?;
    Ok(true)
  }

  /// Read the repository ID, a lowercase name/slug → category ID map and the
  /// category names from a category lookup. `None` when the reply is shaped
  /// unexpectedly (missing keys, a non-list of nodes, non-string names).
  fn read_categories(content: &Map<String, Value>) -> Option<(Value, HashMap<String, Value>, Vec<String>)> {
    let repository = content.get("data")?.as_object()?.get("repository")?.as_object()?;
    let repository_id = repository.get("id")?.clone();
    let nodes = repository.get("discussionCategories")?.as_object()?.get("nodes")?.as_array()?;
    let mut category_ids = HashMap::new();
    let mut names = Vec::new();
    for node in nodes {
      let node = node.as_object()?;
      let name = node.get("name")?.as_str()?;
      let slug = node.get("slug")?.as_str()?;
      let id = node.get("id")?.clone();
      category_ids.insert(name.to_lowercase(), id.clone());
      category_ids.insert(slug.to_lowercase(), id);
      names.push(name.to_string());
    }
    Some((repository_id, category_ids, names))
  }
}

#[async_trait]
impl Notify for GitHub {
  fn schemas(&self) -> &[&str] {
    &["github"]
  }
  fn service_name(&self) -> &str {
    "GitHub"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Markdown
  }
  fn body_maxlen(&self) -> usize {
    65536
  }
  /// 256 for something new; a comment has no title field, so let Apprise
  /// merge the title into the body.
  fn title_maxlen(&self) -> usize {
    if self.number.is_some() { 0 } else { 256 }
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // New issues and discussions require a title
    let title = if ctx.title.is_empty() { ctx.asset.app_desc.as_str() } else { ctx.title.as_str() };
    let client = build_client(self.verify_certificate)?;

    if self.kind == Kind::Discussion {
      // Discussions are only reachable through GraphQL
      return self.send_discussion(&client, &ctx.body, title).await;
    }

    // Start with the repository's issues endpoint
    let mut url = format!("{}/repos/{}/{}/issues", service_base("https://api.github.com"), self.owner, self.repo);
    let payload = if let Some(number) = self.number {
      // Issues and pull requests share the same comment endpoint
      url.push_str(&format!("/{}/comments", number));
      json!({ "body": ctx.body })
    } else {
      // Create a new issue
      let mut payload = json!({ "title": title, "body": ctx.body });
      if !self.labels.is_empty() {
        payload["labels"] = json!(self.labels);
      }
      if !self.assignees.is_empty() {
        payload["assignees"] = json!(self.assignees);
      }
      payload
    };

    self.post(&client, &url, &payload).await?;
    Ok(true)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_json, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const TOKEN: &str = "github_pat_11ABCDEFG0123456789_abcdef";

  fn parse(url: &str) -> Option<GitHub> {
    GitHub::from_url(&ParsedUrl::parse(url).unwrap())
  }

  fn ctx(title: &str, body: &str) -> NotifyContext {
    NotifyContext { title: title.into(), body: body.into(), ..Default::default() }
  }

  fn graphql_ok() -> Value {
    json!({
      "data": {
        "repository": {
          "id": "R_1",
          "discussion": {"id": "D_1"},
          "discussionCategories": {
            "nodes": [
              {"id": "C_1", "name": "General", "slug": "general"},
              {"id": "C_2", "name": "Show and tell", "slug": "show-and-tell"},
            ]
          },
        }
      }
    })
  }

  #[test]
  fn test_valid_urls() {
    let urls = [
      format!("github://{}@caronc/apprise", TOKEN),
      format!("github://{}@caronc/apprise/issues/new?labels=bug,nightly&assignees=caronc", TOKEN),
      format!("github://caronc/apprise/issues/42?token={}", TOKEN),
      format!("github://{}@caronc/apprise/pull/1807/files", TOKEN),
      format!("github://{}@caronc/apprise/discussions/7", TOKEN),
      format!("github://{}@caronc/apprise/discussions/new?category=ideas", TOKEN),
      format!("github://{}@caronc/apprise/discussions", TOKEN),
      format!("github://{}@caronc/apprise/discussion/17", TOKEN),
      format!("github://{}@caronc/apprise/pulls/%2312", TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_invalid_urls() {
    let urls = [
      "github://".to_string(),
      // No token
      "github://caronc/apprise".to_string(),
      // Invalid token characters
      "github://bad%20token@caronc/apprise".to_string(),
      // No repository
      format!("github://{}@caronc", TOKEN),
      // Invalid owner
      format!("github://{}@caronc_bad/apprise", TOKEN),
      // Invalid repository
      format!("github://{}@caronc/bad%20repo", TOKEN),
      // Unsupported path
      format!("github://{}@caronc/apprise/commit/abc123", TOKEN),
      format!("github://{}@caronc/apprise/wiki", TOKEN),
      // Pull request without a number
      format!("github://{}@caronc/apprise/pull", TOKEN),
      format!("github://{}@caronc/apprise/pull/new", TOKEN),
      // Issue that is not a number
      format!("github://{}@caronc/apprise/issues/abc", TOKEN),
      // Two numbers would reach two places
      format!("github://{}@caronc/apprise/issues/12/23", TOKEN),
      // A number after /new is refused too
      format!("github://{}@caronc/apprise/issues/new/5", TOKEN),
      // Numbers too large for GitHub to hold
      format!("github://{}@caronc/apprise/issues/12345678901", TOKEN),
      format!("github://{}@caronc/apprise/issues/0", TOKEN),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let g = parse(&format!("github://{}@caronc/apprise?labels=nightly,bug,bug&assignees=caronc", TOKEN)).unwrap();
    assert_eq!(g.token, TOKEN);
    assert_eq!(g.owner, "caronc");
    assert_eq!(g.repo, "apprise");
    assert_eq!(g.kind, Kind::Issue);
    assert_eq!(g.number, None);
    assert_eq!(g.labels, vec!["bug", "nightly"]);
    assert_eq!(g.assignees, vec!["caronc"]);
    assert_eq!(g.category, "General");
    assert_eq!(g.title_maxlen(), 256);

    // ?token= overrides the user field
    let g = parse(&format!("github://other@caronc/apprise/issues/42?token={}", TOKEN)).unwrap();
    assert_eq!(g.token, TOKEN);
    assert_eq!(g.number, Some(42));
    assert_eq!(g.title_maxlen(), 0);

    let g = parse(&format!("github://{}@caronc/apprise/PULLS/%237/files", TOKEN)).unwrap();
    assert_eq!(g.kind, Kind::Pull);
    assert_eq!(g.number, Some(7));

    // The largest number GitHub can store is accepted
    let g = parse(&format!("github://{}@caronc/apprise/issues/2147483647", TOKEN)).unwrap();
    assert_eq!(g.number, Some(2147483647));

    // A blank category falls back to the default
    let g = parse(&format!("github://{}@caronc/apprise/discussion?category=%20%20", TOKEN)).unwrap();
    assert_eq!(g.kind, Kind::Discussion);
    assert_eq!(g.category, "General");
    assert_eq!(g.number, None);
  }

  #[test]
  fn test_parse_number() {
    assert_eq!(parse_number("#7"), Some(7));
    assert_eq!(parse_number(" 42 "), Some(42));
    assert_eq!(parse_number("9999999999"), Some(9999999999));
    assert_eq!(parse_number("12345678901"), None);
    assert_eq!(parse_number("07"), None);
    assert_eq!(parse_number("# 7"), None);
    assert_eq!(parse_number("abc"), None);
    assert_eq!(parse_number(""), None);
  }

  fn headers_ok() -> impl Fn(wiremock::MockBuilder) -> wiremock::MockBuilder {
    |m: wiremock::MockBuilder| {
      m.and(header("Accept", "application/vnd.github+json"))
        .and(header("Authorization", format!("Bearer {}", TOKEN).as_str()))
        .and(header("Content-Type", "application/json"))
        .and(header("X-GitHub-Api-Version", API_VERSION))
        .and(header("User-Agent", APP_ID))
    }
  }

  #[tokio::test]
  async fn test_send_new_issue() {
    let server = MockServer::start().await;
    let m = Mock::given(method("POST")).and(path("/repos/caronc/apprise/issues")).and(body_json(json!({
      "title": "Load <failed> & stopped",
      "body": "**step** _load_ failed!",
      "labels": ["bug", "nightly"],
      "assignees": ["caronc"],
    })));
    headers_ok()(m).respond_with(ResponseTemplate::new(201).set_body_json(json!({"number": 1}))).expect(1).mount(&server).await;

    let svc = from_url(&format!("github://{}@caronc/apprise?labels=bug,nightly&assignees=caronc", TOKEN)).unwrap();
    let ok = crate::notify::with_test_base(server.uri(), svc.send(&ctx("Load <failed> & stopped", "**step** _load_ failed!"))).await.unwrap();
    assert!(ok);
  }

  #[tokio::test]
  async fn test_send_new_issue_fallback_title() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/repos/owner/project/issues"))
      .and(body_json(json!({"title": "Apprise Notifications", "body": "Body"})))
      // An unreadable reply does not hide a successful status
      .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("github://token123@owner/project").unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "Body"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_comments() {
    for (kind, number) in [("issues", 42), ("pull", 1807)] {
      let server = MockServer::start().await;
      let m = Mock::given(method("POST"))
        .and(path(format!("/repos/caronc/apprise/issues/{}/comments", number)))
        .and(body_json(json!({"body": "# Nightly\n**load** failed"})));
      headers_ok()(m).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;

      let svc = from_url(&format!("github://{}@caronc/apprise/{}/{}/files", TOKEN, kind, number)).unwrap();
      // The title arrives already merged into the body (title_maxlen is 0)
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "# Nightly\n**load** failed"))).await.unwrap());
    }
  }

  #[tokio::test]
  async fn test_send_http_errors() {
    for code in [401u16, 403, 404, 410, 422, 500, 999] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(code).set_body_json(json!({"message": "Not Found"}))).expect(1).mount(&server).await;
      let svc = from_url(&format!("github://{}@caronc/apprise", TOKEN)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "test"))).await.is_err(), "code {}", code);
    }
  }

  #[tokio::test]
  async fn test_send_errors_in_body() {
    // A truthy errors value that is not a list still counts as a failure
    for reply in [json!({"errors": [{"message": "Resource not accessible by integration"}, "unexpected"]}), json!({"errors": true})] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(reply)).expect(1).mount(&server).await;
      let svc = from_url(&format!("github://{}@caronc/apprise", TOKEN)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "test"))).await.is_err());
    }

    // An empty errors list is not a failure
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(201).set_body_json(json!({"errors": []}))).expect(1).mount(&server).await;
    let svc = from_url(&format!("github://{}@caronc/apprise", TOKEN)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("t", "test"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_discussion_comment() {
    let server = MockServer::start().await;
    let lookup = Mock::given(method("POST")).and(path("/graphql")).and(body_json(json!({
      "query": QUERY_DISCUSSION,
      "variables": {"owner": "caronc", "repo": "apprise", "number": 7},
    })));
    headers_ok()(lookup)
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"repository": {"discussion": {"id": "D_1"}}}})))
      .expect(1)
      .mount(&server)
      .await;
    let comment = Mock::given(method("POST")).and(path("/graphql")).and(body_json(json!({
      "query": MUTATION_COMMENT,
      "variables": {"id": "D_1", "body": "Nightly\r\n**load** failed"},
    })));
    headers_ok()(comment)
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"addDiscussionComment": {"comment": {}}}})))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("github://{}@caronc/apprise/discussions/7", TOKEN)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "Nightly\r\n**load** failed"))).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_discussion_comment_failures() {
    // A missing discussion, or a lookup reply of the wrong shape, fails
    // without commenting
    for reply in [
      json!({"data": {"repository": {"discussion": null}}}),
      json!({"data": {"repository": {"discussion": "bad"}}}),
      json!({"data": {"repository": null}}),
      json!([]),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(reply.clone())).expect(1).mount(&server).await;
      let svc = from_url(&format!("github://{}@caronc/apprise/discussions/7", TOKEN)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "test"))).await.unwrap(), "reply {}", reply);
    }

    // A failed lookup fails without commenting
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(401).set_body_json(json!({}))).expect(1).mount(&server).await;
    let svc = from_url(&format!("github://{}@caronc/apprise/discussions/7", TOKEN)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "test"))).await.is_err());
  }

  #[tokio::test]
  async fn test_send_new_discussion() {
    for (query, category, title, expected_title) in [
      ("", "C_1", "Release <2.0> & more", "Release <2.0> & more"),
      ("?category=show-and-tell", "C_2", "", "Apprise Notifications"),
      ("?category=SHOW%20AND%20TELL", "C_2", "T", "T"),
    ] {
      let server = MockServer::start().await;
      let lookup = Mock::given(method("POST")).and(path("/graphql")).and(body_json(json!({
        "query": QUERY_CATEGORIES,
        "variables": {"owner": "caronc", "repo": "apprise"},
      })));
      headers_ok()(lookup).respond_with(ResponseTemplate::new(200).set_body_json(graphql_ok())).expect(1).mount(&server).await;
      let create = Mock::given(method("POST")).and(path("/graphql")).and(body_json(json!({
        "query": MUTATION_DISCUSSION,
        "variables": {"repo": "R_1", "category": category, "title": expected_title, "body": "**Out** now"},
      })));
      headers_ok()(create)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"createDiscussion": {"discussion": {}}}})))
        .expect(1)
        .mount(&server)
        .await;

      let svc = from_url(&format!("github://{}@caronc/apprise/discussions{}", TOKEN, query)).unwrap();
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx(title, "**Out** now"))).await.unwrap(), "query {}", query);
    }
  }

  #[tokio::test]
  async fn test_send_new_discussion_failures() {
    // An unknown category fails without creating anything
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(graphql_ok())).expect(1).mount(&server).await;
    let svc = from_url(&format!("github://{}@caronc/apprise/discussions?category=Polls", TOKEN)).unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "test"))).await.unwrap());

    // Replies GitHub would never send to a category lookup
    for reply in [
      json!([]),
      json!({"data": "bad"}),
      json!({"data": {"repository": {"id": "R_1"}}}),
      json!({"data": {"repository": {"id": "R_1", "discussionCategories": {}}}}),
      json!({"data": {"repository": {"id": "R_1", "discussionCategories": {"nodes": "bad"}}}}),
      json!({"data": {"repository": {"id": "R_1", "discussionCategories": {"nodes": [{"id": "C_1", "name": 5, "slug": "x"}]}}}}),
    ] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(reply.clone())).expect(1).mount(&server).await;
      let svc = from_url(&format!("github://{}@caronc/apprise/discussions", TOKEN)).unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "test"))).await.unwrap(), "reply {}", reply);
    }

    // A failed category lookup fails without creating anything
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(404).set_body_json(json!({}))).expect(1).mount(&server).await;
    let svc = from_url(&format!("github://{}@caronc/apprise/discussions", TOKEN)).unwrap();
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx("", "test"))).await.is_err());
  }

  #[test]
  fn test_service_details() {
    let details = GitHub::static_details();
    assert_eq!(details.service_name, "GitHub");
    assert_eq!(details.protocols, vec!["github"]);
    let svc = parse(&format!("github://{}@caronc/apprise", TOKEN)).unwrap();
    assert_eq!(svc.notify_format(), NotifyFormat::Markdown);
    assert_eq!(svc.body_maxlen(), 65536);
  }
}
