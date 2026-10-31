use async_trait::async_trait;
use serde_json::json;

use crate::conversion::pystr;
use crate::error::NotifyError;
use crate::notify::incident;
use crate::notify::{APP_ID, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::utils::parse::{ParsedUrl, unquote};
use crate::utils::plugin_args::parse_list;

/// The domain added to a host that is only a company name.
const XMATTERS_DOMAIN: &str = "xmatters.com";

/// Upstream `XMATTERS_HTTP_ERROR_MAP`.
const XMATTERS_HTTP_ERROR_MAP: &[(u16, &str)] = &[
  (400, "Bad Request - Invalid payload."),
  (401, "Unauthorized - Invalid credentials."),
  (403, "Forbidden - The user may not trigger this workflow."),
  (404, "Not Found - Invalid integration ID."),
  (429, "Too many requests were made."),
];

/// How a trigger request authenticates.
#[derive(Debug, Clone, PartialEq)]
enum Auth {
  /// URL Authentication: the key goes in `?apiKey=`
  ApiKey(String),
  /// Basic (or API Key) Authentication
  Basic { user: String, password: String },
}

/// xMatters (by Everbridge) alerts raised through a workflow's HTTP trigger.
///
/// `xmatters://{apikey}@{host}/{integration_id}[/{recipients}...]` or
/// `xmatters://{user}:{password}@{host}/{integration_id}[/{recipients}...]`.
/// A host without a domain is a company name (`acmeco` becomes
/// `acmeco.xmatters.com`). The trigger URL copied from xMatters works as is.
pub struct XMatters {
  host: String,
  port: Option<u16>,
  integration_id: String,
  auth: Auth,
  /// xMatters user, group or dynamic team names; empty lets the workflow
  /// decide who is notified
  targets: Vec<String>,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// Upstream `^[a-z0-9][a-z0-9-]*$` (case-insensitive; Python's `$` also
/// accepts one trailing newline), returned stripped.
fn validate_integration_id(value: &str) -> Option<String> {
  let core = value.strip_suffix('\n').unwrap_or(value);
  let mut chars = core.chars();
  let first = chars.next()?;
  (first.is_ascii_alphanumeric() && chars.all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| pystr::strip(value).to_string())
}

/// Upstream `validate_regex(value)`: the value must start with something
/// other than whitespace, and is returned stripped.
fn validate_key(value: &str) -> Option<String> {
  value.chars().next().filter(|c| !pystr::is_space(*c)).map(|_| pystr::strip(value).to_string())
}

impl XMatters {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // Upstream parses the URL with host verification
    let Some(host) = url.verified_host() else {
      tracing::warn!("A valid xMatters host must be specified.");
      return None;
    };

    // The integration ID comes first, followed by our recipients
    let mut entries = url.path_parts.clone();
    let integration_id = if entries.is_empty() { String::new() } else { entries.remove(0) };
    let Some(integration_id) = validate_integration_id(&integration_id) else {
      tracing::warn!("An invalid xMatters integration ID ({}) was specified.", integration_id);
      return None;
    };

    // Support ?to= and ?recipients=; recipient names may contain spaces, so
    // only commas split them
    for key in ["to", "recipients"] {
      if let Some(value) = url.get(key).filter(|v| !v.is_empty()) {
        entries.extend(parse_list([unquote(value)], false, true));
      }
    }
    let targets = parse_list(&entries, false, true);

    // Without a password, the user field holds our API key; ?apikey= wins
    let mut apikey = if url.password.is_none() { url.user.clone() } else { None };
    if let Some(value) = url.get("apikey").filter(|v| !v.is_empty()) {
      apikey = Some(unquote(value));
    }

    // A username and password means Basic Authentication, so any API key is
    // not needed
    let auth = match (&url.user, &url.password) {
      (Some(user), Some(password)) => Auth::Basic { user: user.clone(), password: password.clone() },
      _ => match apikey.as_deref().and_then(validate_key) {
        Some(key) => Auth::ApiKey(key),
        None => {
          tracing::warn!("An xMatters API key, or a username and password, must be specified.");
          return None;
        }
      },
    };

    Some(Self {
      host,
      // Upstream leaves a port of 0 out of the request
      port: url.port.filter(|p| *p != 0),
      integration_id,
      auth,
      targets,
      verify_certificate: url.verify_certificate(),
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "xMatters",
      service_url: Some("https://www.xmatters.com/"),
      setup_url: Some("https://appriseit.com/services/xmatters/"),
      protocols: vec!["xmatters"],
      description: "Raise xMatters alerts through a workflow's HTTP trigger.",
      attachment_support: false,
    }
  }

  /// `https://{host}[:{port}]`, with `.xmatters.com` added to a bare company
  /// name.
  fn origin(&self) -> String {
    let host = if self.host.contains('.') { self.host.clone() } else { format!("{}.{}", self.host, XMATTERS_DOMAIN) };
    match self.port {
      Some(port) => format!("https://{}:{}", host, port),
      None => format!("https://{}", host),
    }
  }
}

#[async_trait]
impl Notify for XMatters {
  fn schemas(&self) -> &[&str] {
    &["xmatters"]
  }
  fn service_name(&self) -> &str {
    "xMatters"
  }
  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }
  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }
  /// The host, its trigger and our credentials identify the connection
  fn url_identifier(&self) -> Option<Vec<String>> {
    let none = || "None".to_string();
    let (apikey, user, password) = match &self.auth {
      Auth::ApiKey(key) => (key.clone(), none(), none()),
      Auth::Basic { user, password } => (none(), user.clone(), password.clone()),
    };
    Some(vec!["xmatters".into(), self.host.clone(), self.port.map_or_else(none, |p| p.to_string()), self.integration_id.clone(), apikey, user, password])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    // The summary is the alert subject. Without a title, the start of the
    // body stands in for it.
    let summary: String = if ctx.title.is_empty() { ctx.body.chars().take(self.title_maxlen()).collect() } else { ctx.title.clone() };
    let mut payload = json!({ "summary": summary, "message": ctx.body });
    // Without recipients, the workflow decides who is notified
    if !self.targets.is_empty() {
      payload["recipients"] = json!(self.targets.join(","));
    }

    let url = format!("{}/api/integration/1/functions/{}/triggers", service_base(&self.origin()), self.integration_id);
    tracing::debug!("xMatters POST URL: {} (cert_verify={})", url, self.verify_certificate);

    let client = build_client(self.verify_certificate)?;
    let request = client.post(url).header("User-Agent", APP_ID).json(&payload);
    // URL Authentication passes the API key as a parameter, while everything
    // else uses Basic Authentication
    let request = match &self.auth {
      Auth::ApiKey(key) => request.query(&[("apiKey", key)]),
      Auth::Basic { user, password } => request.basic_auth(user, Some(password)),
    };
    incident::exchange(request, "xMatters notification", &[200, 201, 202], XMATTERS_HTTP_ERROR_MAP).await?.into_result()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::custom_json::requests_to;
  use crate::notify::registry::from_url;
  use crate::types::NotifyType;
  use serde_json::Value;
  use wiremock::matchers::{header, method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  // An integration ID and API key we can use
  const IID: &str = "ba601cb1-3513-4320-b48a-05cb501bb5af";
  const APIKEY: &str = "ffcd4dec-8a49-4a67-9e59-ed9fabcb002d";

  fn trigger() -> String {
    format!("/api/integration/1/functions/{IID}/triggers")
  }

  fn xmatters(url: &str) -> XMatters {
    XMatters::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  #[test]
  fn test_urls() {
    // Upstream apprise_url_tests
    for url in [
      // No hostname
      "xmatters://".to_string(),
      // No integration ID
      format!("xmatters://{APIKEY}@acmeco"),
      // Invalid integration ID
      format!("xmatters://{APIKEY}@acmeco/%20invalid%3F"),
      format!("xmatters://{APIKEY}@acmeco/-starts-with-a-dash"),
      // No credentials at all
      format!("xmatters://acmeco/{IID}"),
      // A username without a password is an API key; an empty one is not
      format!("xmatters://:pass@acmeco/{IID}"),
      format!("xmatters://acmeco/{IID}?apikey=%20%20"),
      // A native URL without its API key
      format!("https://acmeco.xmatters.com/api/integration/1/functions/{IID}/triggers"),
      // A host upstream's host verification refuses
      format!("xmatters://{APIKEY}@acme_co!/{IID}"),
    ] {
      assert!(from_url(&url).is_none(), "Should not parse: {url}");
    }
    for url in [
      format!("xmatters://{APIKEY}@acmeco/{IID}"),
      format!("xmatters://{APIKEY}@acmeco/{IID}/jdoe/First%20Aid"),
      format!("xmatters://user:pass@acmeco.na5.xmatters.com/{IID}/?to=jdoe"),
      format!("xmatters://acmeco/{IID}/?apikey={APIKEY}&recipients=jdoe"),
      format!("https://acmeco.xmatters.com/api/integration/1/functions/{IID}/triggers?apiKey={APIKEY}&recipients=jdoe,ops"),
    ] {
      assert!(from_url(&url).is_some(), "Should parse: {url}");
    }
  }

  #[test]
  fn test_url_parsing() {
    let obj = xmatters(&format!("xmatters://{APIKEY}@acmeco/{IID}/jdoe/First%20Aid/?to=jane|Work"));
    assert_eq!(obj.host, "acmeco");
    assert_eq!(obj.integration_id, IID);
    assert_eq!(obj.auth, Auth::ApiKey(APIKEY.into()));
    // Recipients are sorted and only commas split them
    assert_eq!(obj.targets, vec!["First Aid", "jane|Work", "jdoe"]);

    // Basic authentication ignores any API key
    let obj = xmatters(&format!("xmatters://user:pass@acmeco.na5.xmatters.com:8443/{IID}?apikey={APIKEY}"));
    assert_eq!(obj.auth, Auth::Basic { user: "user".into(), password: "pass".into() });
    assert_eq!(obj.port, Some(8443));
    assert!(obj.targets.is_empty());

    // A user without a password is our API key, which ?apikey= replaces;
    // query values are unquoted once more, as upstream does
    assert_eq!(xmatters(&format!("xmatters://user@acmeco/{IID}?apikey=k%2541")).auth, Auth::ApiKey("kA".into()));
    assert_eq!(xmatters(&format!("xmatters://:pass@acmeco/{IID}?apikey=k")).auth, Auth::ApiKey("k".into()));
    // ?user= and ?password= give Basic Authentication
    assert_eq!(xmatters(&format!("xmatters://acmeco:8443/{IID}/?user=u&password=p")).auth, Auth::Basic { user: "u".into(), password: "p".into() });

    // Each recipient argument is split on commas and semicolons; entries are
    // stripped and duplicates removed
    let obj = xmatters(&format!("xmatters://{APIKEY}@acmeco/{IID}/jdoe?recipients=a%2541,%20b%20c%20|d&to=jdoe,[ops]"));
    assert_eq!(obj.targets, vec!["aA", "b c |d", "jdoe", "ops"]);
  }

  #[test]
  fn test_service_details() {
    let details = XMatters::static_details();
    assert_eq!(details.service_name, "xMatters");
    assert_eq!(details.protocols, vec!["xmatters"]);
    assert!(!details.attachment_support);
    let svc = from_url(&format!("xmatters://{APIKEY}@acmeco/{IID}")).unwrap();
    assert_eq!(svc.body_maxlen(), 32768);
    assert_eq!(svc.title_maxlen(), 250);
    assert_eq!(svc.notify_format(), crate::types::NotifyFormat::Text);
  }

  #[test]
  fn test_origin() {
    // A company name becomes a full xMatters host; a full host and port are
    // kept as given
    assert_eq!(xmatters(&format!("xmatters://{APIKEY}@acmeco/{IID}")).origin(), "https://acmeco.xmatters.com");
    assert_eq!(xmatters(&format!("xmatters://{APIKEY}@acmeco:8443/{IID}")).origin(), "https://acmeco.xmatters.com:8443");
    assert_eq!(xmatters(&format!("xmatters://user:pass@acmeco.na5.xmatters.com:8443/{IID}")).origin(), "https://acmeco.na5.xmatters.com:8443");
  }

  #[test]
  fn test_storage_id_matches_upstream() {
    // uid: lines of upstream's apprise --dry-run; the recipients do not count
    crate::notify::custom_json::assert_upstream_uids(&[
      (&format!("xmatters://{APIKEY}@acmeco/{IID}/jdoe"), "3a38998a"),
      (&format!("xmatters://{APIKEY}@acmeco/{IID}/ops"), "3a38998a"),
      (&format!("xmatters://user:pass@acmeco.na5.xmatters.com:8443/{IID}"), "d3f012db"),
      (&format!("https://acmeco.xmatters.com/api/integration/1/functions/{IID}/triggers?apiKey={APIKEY}&recipients=jdoe"), "3d0370e4"),
    ]);
    // Upstream test_plugin_xmatters_url_identifier
    let id = |url: String| xmatters(&url).url_identifier();
    assert_ne!(id(format!("xmatters://{APIKEY}@acmeco/{IID}/jdoe")), id(format!("xmatters://other@acmeco/{IID}/jdoe")));
    assert_ne!(id(format!("xmatters://{APIKEY}@acmeco/{IID}/jdoe")), id(format!("xmatters://{APIKEY}@acmeco/abcd1234/jdoe")));
  }

  #[test]
  fn test_native_url() {
    let native = from_url(&format!("https://acmeco.xmatters.com/api/integration/1/functions/{IID}/triggers?apiKey={APIKEY}&recipients=jdoe")).unwrap();
    // The same connection as its xmatters:// form
    let plain = from_url(&format!("xmatters://{APIKEY}@acmeco.xmatters.com/{IID}/jdoe")).unwrap();
    assert_eq!(native.url_identifier(), plain.url_identifier());
  }

  #[tokio::test]
  async fn test_send_apikey() {
    // Upstream test_plugin_xmatters_apikey_payload; matches pycapture.py
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(trigger()))
      .and(query_param("apiKey", APIKEY))
      .and(header("user-agent", APP_ID))
      .and(header("content-type", "application/json"))
      .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"requestId": "abc"}"#))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("xmatters://{APIKEY}@acmeco/{IID}/jdoe/First%20Aid/?to=jane|Work")).unwrap();
    let ctx = NotifyContext {
      title: "Disk <full> & failing".into(),
      body: "Some _plain_ text\n\\ \"quoted\"".into(),
      notify_type: NotifyType::Failure,
      ..Default::default()
    };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let sent = &requests_to(&server, "POST", &trigger()).await[0];
    // URL authentication uses the apiKey parameter and nothing else
    assert_eq!(sent.url.query(), Some(format!("apiKey={APIKEY}").as_str()));
    assert!(sent.headers.get("authorization").is_none());
    // The title and body are sent untouched
    assert_eq!(
      serde_json::from_slice::<Value>(&sent.body).unwrap(),
      json!({
        "summary": "Disk <full> & failing",
        "message": "Some _plain_ text\n\\ \"quoted\"",
        "recipients": "First Aid,jane|Work,jdoe",
      })
    );
  }

  #[tokio::test]
  async fn test_send_basic_auth() {
    // Upstream test_plugin_xmatters_basic_auth
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path(trigger()))
      // "user:pass"
      .and(header("authorization", "Basic dXNlcjpwYXNz"))
      .respond_with(ResponseTemplate::new(200))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url(&format!("xmatters://user:pass@acmeco.na5.xmatters.com:8443/{IID}?apikey={APIKEY}")).unwrap();
    // Without a title, the start of the body is our summary
    let ctx = NotifyContext { body: "A".repeat(300), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let sent = &requests_to(&server, "POST", &trigger()).await[0];
    assert_eq!(sent.url.query(), None);
    // No recipients are sent when none were given
    assert_eq!(serde_json::from_slice::<Value>(&sent.body).unwrap(), json!({"summary": "A".repeat(250), "message": "A".repeat(300)}));
  }

  #[tokio::test]
  async fn test_summary_counts_characters() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path(trigger())).respond_with(ResponseTemplate::new(201)).expect(1).mount(&server).await;
    let svc = from_url(&format!("xmatters://{APIKEY}@acmeco/{IID}")).unwrap();
    let ctx = NotifyContext { body: "é".repeat(260), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let sent = &requests_to(&server, "POST", &trigger()).await[0];
    assert_eq!(serde_json::from_slice::<Value>(&sent.body).unwrap()["summary"], json!("é".repeat(250)));
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { title: "T".into(), body: "B".into(), ..Default::default() };
    let url = format!("xmatters://{APIKEY}@acmeco/{IID}/jdoe");
    // A failure, and a bizarre code we cannot look up
    for status in [500, 999, 404, 401] {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path(trigger())).respond_with(ResponseTemplate::new(status)).expect(1).mount(&server).await;
      let err = crate::notify::with_test_base(server.uri(), from_url(&url).unwrap().send(&ctx)).await.unwrap_err();
      assert!(matches!(err, NotifyError::ServiceError { status: s, .. } if s == status), "{status}");
    }

    // A connection error
    let server = MockServer::start().await;
    let base = server.uri();
    drop(server);
    assert!(crate::notify::with_test_base(base, from_url(&url).unwrap().send(&ctx)).await.is_err());
  }
}
