use async_trait::async_trait;
use base64::Engine;
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, ServiceDetails, build_client, service_base};
use crate::types::NotifyFormat;
use crate::utils::email::{is_email, parse_emails};
use crate::utils::parse::{ParseOptions, ParsedUrl, parse_bool, unquote};

const GRAPH_URL: &str = "https://graph.microsoft.com";
const LOGIN_URL: &str = "https://login.microsoftonline.com";
const PERSONAL_SCOPE: &str = "https://graph.microsoft.com/Mail.Send offline_access";

/// Attachments larger than this go through an upload session.
const INLINE_ATTACHMENT_MAX: usize = 3_145_728;
/// Upload session chunk size (upstream `AttachBase.chunk()` default).
const UPLOAD_CHUNK: usize = 5_242_880;

/// Consumer domains that select personal (delegated) mode automatically.
const PERSONAL_DOMAINS: &[&str] = &[
  "outlook.com",
  "outlook.at",
  "outlook.be",
  "outlook.ca",
  "outlook.cl",
  "outlook.co.nz",
  "outlook.co.uk",
  "outlook.com.ar",
  "outlook.com.au",
  "outlook.com.br",
  "outlook.com.mx",
  "outlook.cz",
  "outlook.de",
  "outlook.dk",
  "outlook.es",
  "outlook.fi",
  "outlook.fr",
  "outlook.hu",
  "outlook.ie",
  "outlook.in",
  "outlook.it",
  "outlook.jp",
  "outlook.kr",
  "outlook.lv",
  "outlook.my",
  "outlook.nl",
  "outlook.ph",
  "outlook.pt",
  "outlook.rs",
  "outlook.sa",
  "outlook.sg",
  "outlook.sk",
  "hotmail.com",
  "hotmail.at",
  "hotmail.be",
  "hotmail.ca",
  "hotmail.cl",
  "hotmail.co.uk",
  "hotmail.com.ar",
  "hotmail.com.au",
  "hotmail.com.br",
  "hotmail.com.mx",
  "hotmail.cz",
  "hotmail.de",
  "hotmail.dk",
  "hotmail.es",
  "hotmail.fi",
  "hotmail.fr",
  "hotmail.gr",
  "hotmail.hu",
  "hotmail.ie",
  "hotmail.it",
  "hotmail.nl",
  "hotmail.no",
  "hotmail.pt",
  "hotmail.rs",
  "hotmail.se",
  "live.at",
  "live.be",
  "live.ca",
  "live.cl",
  "live.co.nz",
  "live.co.uk",
  "live.com",
  "live.com.ar",
  "live.com.au",
  "live.com.mx",
  "live.de",
  "live.dk",
  "live.es",
  "live.fi",
  "live.fr",
  "live.gr",
  "live.hu",
  "live.ie",
  "live.in",
  "live.it",
  "live.jp",
  "live.mx",
  "live.my",
  "live.nl",
  "live.no",
  "live.ph",
  "live.pt",
  "live.rs",
  "live.se",
  "live.sg",
  "msn.com",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  /// Application permissions (client credentials) in a tenant.
  Org,
  /// Delegated permissions for a consumer account (refresh token).
  Personal,
}

/// The sender shown in the From header.
#[derive(Debug, Clone, PartialEq)]
enum From {
  /// Not known yet; looked up from the directory (org mode).
  Unknown,
  /// The lookup gave nothing usable; send without a From.
  Unavailable,
  Known {
    email: String,
    name: Option<String>,
  },
}

/// Session state. The From address (org mode) and the rotated refresh
/// token (personal mode) are also kept in the persistent store, as upstream
/// does, so they survive between runs; without a store they live here only.
struct State {
  token: Option<String>,
  token_expiry: Instant,
  refresh_token: Option<String>,
  from: From,
  /// Whether the cached From address was read from the store yet.
  store_loaded: bool,
}

pub struct Office365 {
  mode: Mode,
  /// Account email or directory object id.
  source: String,
  source_is_object_id: bool,
  tenant: Option<String>,
  client_id: String,
  /// Client secret (org) or refresh token (personal).
  secret: String,
  /// (display name, address)
  targets: Vec<(Option<String>, String)>,
  cc: BTreeSet<String>,
  bcc: BTreeSet<String>,
  reply_to: BTreeSet<String>,
  names: HashMap<String, String>,
  save_sent: bool,
  verify_certificate: bool,
  tags: Vec<String>,
  state: Mutex<State>,
}

impl Office365 {
  pub fn from_url(original: &ParsedUrl) -> Option<Self> {
    // {schema}://{source}/{tenant}/{client_id}/{secret}/{targets}   (org)
    // {schema}://{source}/{client_id}/{secret}/{targets}            (personal)
    // Upstream reads this URL's query with `+` meaning a space.
    let reparsed = ParsedUrl::parse_with(&original.raw, ParseOptions { plus_to_space: true, ..Default::default() });
    let url = reparsed.as_ref().unwrap_or(original);
    let mut entries = url.path_parts.clone();
    let host = url.host.clone().unwrap_or_default();
    let qsd = |key: &str| url.get(key).filter(|v| !v.is_empty()).map(unquote);

    let source = match (qsd("from"), url.user.as_deref().filter(|u| !u.is_empty())) {
      (Some(from), _) => from,
      (None, Some(user)) => format!("{}@{}", user, host),
      (None, None) => host,
    };

    // An unknown ?mode= falls back to detection from the source domain
    let mode = match url.get("mode").map(|m| m.to_lowercase()).as_deref() {
      Some("org") => Mode::Org,
      Some("personal") => Mode::Personal,
      _ => match is_email(&source) {
        Some(e) if PERSONAL_DOMAINS.contains(&e.domain.to_lowercase().as_str()) => Mode::Personal,
        _ => Mode::Org,
      },
    };
    // YAML `source:` and `tenant:` settings replace what the URL gives
    // (upstream passes them to the constructor). The URL's own source still
    // decides the mode, and its tenant still takes its place in the path.
    let source = original.kwarg("source").map_or(source, str::to_string);

    let tenant = match mode {
      Mode::Org => {
        let written = qsd("tenant").or_else(|| (!entries.is_empty()).then(|| unquote(&entries.remove(0))));
        original.kwarg("tenant").map(str::to_string).or(written)
      }
      Mode::Personal => None,
    };
    let client_id = qsd("oauth_id").or_else(|| (!entries.is_empty()).then(|| unquote(&entries.remove(0)))).unwrap_or_default();

    // Trailing email addresses are targets; whatever remains is the secret
    let mut raw_targets: Vec<String> = Vec::new();
    while let Some(last) = entries.last() {
      let entry = unquote(last);
      if is_email(&entry).is_none() {
        break;
      }
      raw_targets.push(entry);
      entries.pop();
    }
    let secret = qsd("oauth_secret").unwrap_or_else(|| entries.iter().map(|e| unquote(e)).collect::<Vec<_>>().join("/"));
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      // Upstream's URLBase.parse_list() also unquotes each entry
      raw_targets.extend(crate::utils::parse::parse_list(to).iter().map(|t| unquote(t)));
    }

    let client_id = client_id.trim().to_string();
    if client_id.is_empty() || !client_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
      tracing::warn!("An invalid Office 365 Client OAuth2 ID ({}) was specified.", client_id);
      return None;
    }
    let secret = secret.trim().to_string();
    if secret.is_empty() {
      tracing::warn!("An invalid Office 365 Client OAuth2 Secret was specified.");
      return None;
    }
    let tenant = match mode {
      Mode::Org => {
        let tenant = tenant.unwrap_or_default().trim().to_string();
        if tenant.is_empty() || !tenant.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
          tracing::warn!("An invalid Office 365 Tenant ({}) was specified.", tenant);
          return None;
        }
        Some(tenant)
      }
      Mode::Personal => None,
    };

    let mut names = HashMap::new();
    let mut targets = Vec::new();
    if !raw_targets.is_empty() {
      for recipient in raw_targets.iter().flat_map(|t| parse_emails(t)) {
        match is_email(&recipient) {
          Some(e) => targets.push((e.display_name(), e.full_email)),
          None => tracing::warn!("Dropped invalid To email ({}) specified.", recipient),
        }
      }
    } else if is_email(&source).is_some() {
      targets.push((None, source.clone()));
    } else {
      tracing::warn!("No Target Office 365 Email Detected");
    }

    let mut collect = |key: &str, label: &str| {
      let mut set = BTreeSet::new();
      for recipient in url.get(key).map(parse_emails).unwrap_or_default() {
        match is_email(&recipient) {
          Some(e) => {
            if let Some(name) = e.display_name() {
              names.insert(e.full_email.clone(), name);
            } else {
              names.remove(&e.full_email);
            }
            set.insert(e.full_email);
          }
          None => tracing::warn!("Dropped invalid {} email ({}) specified.", label, recipient),
        }
      }
      set
    };
    let cc = collect("cc", "Carbon Copy");
    let bcc = collect("bcc", "Blind Carbon Copy");
    let reply_to = collect("reply_to", "Reply-To");

    let source_email = is_email(&source);
    let from = match (mode, source_email) {
      (Mode::Personal, None) => {
        tracing::warn!("A valid source email address is required for personal mode; got ({}).", source);
        return None;
      }
      (_, Some(e)) => From::Known { name: e.display_name(), email: e.full_email },
      (Mode::Org, None) => From::Unknown,
    };
    let source_is_object_id = matches!(from, From::Unknown) && is_object_id(&source);

    Some(Self {
      mode,
      source,
      source_is_object_id,
      tenant,
      client_id,
      secret,
      targets,
      cc,
      bcc,
      reply_to,
      names,
      save_sent: url.get("savesent").map(parse_bool).unwrap_or(true),
      verify_certificate: url.verify_certificate(),
      tags: original.tags(),
      state: Mutex::new(State { token: None, token_expiry: Instant::now(), refresh_token: None, from, store_loaded: false }),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Office 365",
      service_url: Some("https://office.com/"),
      setup_url: Some("https://appriseit.com/services/office365/"),
      protocols: vec!["azure", "o365"],
      description: "Send email through Microsoft Graph (Office 365 and Outlook.com).",
      attachment_support: true,
    }
  }

  /// `/v1.0/me` (personal) or `/v1.0/users/{source}` (org).
  fn user_path(&self) -> String {
    match self.mode {
      Mode::Personal => format!("{}/v1.0/me", service_base(GRAPH_URL)),
      Mode::Org => format!("{}/v1.0/users/{}", service_base(GRAPH_URL), self.source),
    }
  }

  /// Perform one request. Returns whether it succeeded (200/201/202) and the
  /// decoded JSON reply (`{}` when the reply is not JSON).
  async fn fetch(&self, client: &reqwest::Client, state: &State, method: reqwest::Method, url: &str, body: Body<'_>) -> (bool, Value) {
    tracing::debug!("Office 365 {} URL: {} (cert_verify={})", method, url, self.verify_certificate);
    let mut builder = client.request(method.clone(), url).header("User-Agent", APP_ID);
    builder = match body {
      Body::None => builder,
      Body::Json(v) => builder.header("Content-Type", "application/json").body(v.to_string()),
      Body::Form(pairs) => builder
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()),
      // No Content-Type: the chunk is a slice of the file (Content-Length
      // comes from the body)
      Body::Chunk(data, range) => builder.header("Content-Range", range).body(data),
    };
    if let Some(token) = &state.token {
      builder = builder.header("Authorization", format!("Bearer {}", token));
    }
    crate::notify::throttle().await;
    let r = match builder.send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("Exception received when sending Office 365 {} to {}: ", method, url);
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return (false, json!({}));
      }
    };
    let status = r.status();
    let raw = r.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
    if !matches!(status.as_u16(), 200..=202) {
      let reason = status.canonical_reason().unwrap_or("");
      tracing::warn!("Failed to send Office 365 {} to {}: {}{}error={}.", method, url, reason, if reason.is_empty() { "" } else { ", " }, status.as_u16());
      tracing::debug!("Response Details:\r\n{}", String::from_utf8_lossy(&raw[..raw.len().min(2000)]));
      return (false, json!({}));
    }
    let content = serde_json::from_slice::<Value>(&raw).unwrap_or_else(|_| {
      tracing::debug!("Failed to parse Office365 JSON response");
      json!({})
    });
    (true, content)
  }

  async fn authenticate(&self, client: &reqwest::Client, state: &mut State) -> bool {
    if state.token.is_some() && state.token_expiry > Instant::now() {
      return true;
    }
    let (url, grant): (String, Vec<(&str, String)>) = match self.mode {
      Mode::Org => (
        format!("{}/{}/oauth2/v2.0/token", service_base(LOGIN_URL), self.tenant.as_deref().unwrap_or_default()),
        vec![
          ("grant_type", "client_credentials".into()),
          ("client_id", self.client_id.clone()),
          ("client_secret", self.secret.clone()),
          ("scope", format!("{}/.default", GRAPH_URL)),
        ],
      ),
      Mode::Personal => (
        format!("{}/consumers/oauth2/v2.0/token", service_base(LOGIN_URL)),
        vec![
          ("grant_type", "refresh_token".into()),
          ("client_id", self.client_id.clone()),
          // The most recently rotated token wins over the one in the URL
          (
            "refresh_token",
            crate::storage::with_service_store(|s| s.get("refresh_token"))
              .flatten()
              .and_then(|v| v.as_str().filter(|t| !t.is_empty()).map(String::from))
              .or_else(|| state.refresh_token.clone())
              .unwrap_or_else(|| self.secret.clone()),
          ),
          ("scope", PERSONAL_SCOPE.into()),
        ],
      ),
    };
    let pairs: Vec<(&str, &str)> = grant.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (ok, response) = self.fetch(client, state, reqwest::Method::POST, &url, Body::Form(&pairs)).await;
    if !ok {
      return false;
    }
    state.token = None;
    let Some(expires_in) = response.get("expires_in").and_then(as_int) else {
      return false;
    };
    state.token_expiry = Instant::now() + Duration::from_secs((expires_in - 10).max(0) as u64);
    if self.mode == Mode::Personal
      && let Some(rotated) = response.get("refresh_token").and_then(Value::as_str).filter(|t| !t.is_empty())
    {
      state.refresh_token = Some(rotated.to_string());
      crate::storage::with_service_store(|s| s.set("refresh_token", json!(rotated), None, true));
    }
    state.token = response.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()).map(String::from);
    state.token.is_some()
  }

  /// Look up the From address of the source account (org mode).
  async fn resolve_from(&self, client: &reqwest::Client, state: &mut State) -> bool {
    if !self.authenticate(client, state).await {
      return false;
    }
    let (ok, response) = self.fetch(client, state, reqwest::Method::GET, &self.user_path(), Body::None).await;
    if !ok {
      if self.source_is_object_id {
        tracing::warn!(
          "The specified Object ID could not be resolved in this tenant. Verify the Object ID exists in the selected directory, or use a email address instead."
        );
        return false;
      }
      tracing::warn!("Could not acquire From email address; ensure \"User.Read.All\" Application scope is set!");
      return true;
    }
    let address = response.get("mail").and_then(Value::as_str).filter(|m| !m.is_empty()).or_else(|| response.get("userPrincipalName").and_then(Value::as_str));
    state.from = match address.and_then(is_email) {
      Some(e) => {
        // Remember the sender for future runs
        let name = response.get("displayName").and_then(Value::as_str).filter(|n| !n.is_empty()).map(String::from);
        crate::storage::with_service_store(|s| {
          s.set("from", json!(e.full_email), None, true);
          if let Some(name) = &name {
            s.set("name", json!(name), None, true);
          }
        });
        From::Known { email: e.full_email, name }
      }
      None => {
        tracing::warn!("Could not get From email from the Azure endpoint.");
        From::Unavailable
      }
    };
    true
  }

  fn address(&self, addr: &str) -> Value {
    let mut entry = Map::new();
    entry.insert("address".into(), json!(addr));
    if let Some(name) = self.names.get(addr) {
      entry.insert("name".into(), json!(name));
    }
    json!({ "emailAddress": entry })
  }

  /// Create an upload session on a draft and PUT the file in chunks.
  async fn upload_attachment(&self, client: &reqwest::Client, state: &mut State, attachment: &Attachment, message_id: &str, name: &str) -> bool {
    let url = format!("{}/message/{}/attachments/createUploadSession", self.user_path(), message_id);
    let payload = json!({
      "AttachmentItem": {
        "attachmentType": "file",
        "name": name,
        "contentType": attachment.mime_type,
        "size": attachment.data.len(),
      }
    });
    if !self.authenticate(client, state).await {
      return false;
    }
    let (ok, response) = self.fetch(client, state, reqwest::Method::POST, &url, Body::Json(payload)).await;
    if !ok {
      return false;
    }
    let Some(upload_url) = response.get("uploadUrl").and_then(Value::as_str).filter(|u| !u.is_empty()).map(String::from) else {
      return false;
    };

    let total = attachment.data.len();
    let mut ok = false;
    for (i, chunk) in attachment.data.chunks(UPLOAD_CHUNK).enumerate() {
      let start = i * UPLOAD_CHUNK;
      let range = format!("bytes {}-{}/{}", start, start + chunk.len() - 1, total);
      ok = self.fetch(client, state, reqwest::Method::PUT, &upload_url, Body::Chunk(chunk.to_vec(), range)).await.0;
      if !ok {
        return false;
      }
    }
    ok
  }
}

enum Body<'a> {
  None,
  Json(Value),
  Form(&'a [(&'a str, &'a str)]),
  Chunk(Vec<u8>, String),
}

#[async_trait]
impl Notify for Office365 {
  fn schemas(&self) -> &[&str] {
    &["azure", "o365"]
  }
  fn service_name(&self) -> &str {
    "Office 365"
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
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Html
  }
  fn request_rate_per_sec(&self) -> f64 {
    0.2
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    let mut id = vec!["azure".to_string(), self.source.clone()];
    id.extend(self.tenant.clone());
    id.extend([self.client_id.clone(), self.secret.clone()]);
    Some(id)
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Email recipients to notify");
      return Ok(false);
    }
    let client = build_client(self.verify_certificate)?;
    let mut state = self.state.lock().await;

    // Org mode reuses the sender found by an earlier run
    if self.mode == Mode::Org && !state.store_loaded {
      state.store_loaded = true;
      let text = |v: Option<Value>| v.and_then(|v| v.as_str().filter(|t| !t.is_empty()).map(String::from));
      if let Some((email, name)) = crate::storage::with_service_store(|s| (text(s.get("from")), text(s.get("name")))) {
        match &mut state.from {
          From::Unknown => {
            if let Some(email) = email {
              state.from = From::Known { email, name };
            }
          }
          From::Known { name: known @ None, .. } => *known = name,
          _ => {}
        }
      }
    }

    if self.mode == Mode::Org && state.from == From::Unknown && !self.resolve_from(&client, &mut state).await {
      return Ok(false);
    }

    let mut message = Map::new();
    message.insert("subject".into(), json!(ctx.title));
    message.insert("body".into(), json!({ "contentType": if ctx.body_format == NotifyFormat::Html { "HTML" } else { "Text" }, "content": ctx.body }));
    if let From::Known { email, name } = &state.from {
      message.insert("from".into(), json!({ "emailAddress": { "address": email, "name": name.as_deref().unwrap_or(&ctx.asset.app_id) } }));
    }
    if !self.reply_to.is_empty() {
      message.insert("replyTo".into(), Value::Array(self.reply_to.iter().map(|a| self.address(a)).collect()));
    }

    // Small attachments go inline; larger ones are uploaded to a draft
    let mut small = Vec::new();
    let mut large = Vec::new();
    for (no, attachment) in ctx.attachments.iter().enumerate() {
      let name = if attachment.name.is_empty() { format!("file{:03}.dat", no + 1) } else { attachment.name.clone() };
      if attachment.data.len() > INLINE_ATTACHMENT_MAX {
        large.push((attachment, name));
        continue;
      }
      tracing::debug!("Appending Office 365 attachment {}", name);
      small.push(json!({
        "@odata.type": "#microsoft.graph.fileAttachment",
        "name": name,
        "contentType": attachment.mime_type,
        "contentBytes": base64::engine::general_purpose::STANDARD.encode(&attachment.data),
      }));
    }
    if !small.is_empty() {
      message.insert("attachments".into(), Value::Array(small));
    }

    let user_path = self.user_path();
    let url = if large.is_empty() { format!("{}/sendMail", user_path) } else { format!("{}/messages", user_path) };
    let mut has_error = false;
    for (to_name, to_addr) in &self.targets {
      if !self.authenticate(&client, &mut state).await {
        return Ok(false);
      }
      // A retry skips recipients that already accepted the message
      if crate::delivery::is_delivered(to_addr, false) {
        continue;
      }

      let mut msg = message.clone();
      let mut to = Map::new();
      to.insert("address".into(), json!(to_addr));
      if let Some(name) = to_name {
        to.insert("name".into(), json!(name));
      }
      msg.insert("toRecipients".into(), json!([{ "emailAddress": to }]));
      let cc: Vec<Value> = self.cc.iter().filter(|a| !self.bcc.contains(*a) && *a != to_addr).map(|a| self.address(a)).collect();
      if !cc.is_empty() {
        msg.insert("ccRecipients".into(), Value::Array(cc));
      }
      let bcc: Vec<Value> = self.bcc.iter().filter(|a| *a != to_addr).map(|a| self.address(a)).collect();
      if !bcc.is_empty() {
        msg.insert("bccRecipients".into(), Value::Array(bcc));
      }
      tracing::debug!("{}Email To: {}", if large.is_empty() { "" } else { "Draft" }, to_addr);

      let payload = json!({ "message": msg, "saveToSentItems": if self.save_sent { "true" } else { "false" } });
      let (ok, response) = self.fetch(&client, &state, reqwest::Method::POST, &url, Body::Json(payload)).await;
      if !ok {
        has_error = true;
        continue;
      }
      if large.is_empty() {
        crate::delivery::mark_delivered(to_addr, false);
        continue;
      }

      let Some(message_id) = response.get("id").and_then(Value::as_str).filter(|i| !i.is_empty()).map(String::from) else {
        tracing::warn!("Email Draft ID could not be retrieved");
        has_error = true;
        continue;
      };
      tracing::debug!("Email Draft ID: {}", message_id);
      let mut uploaded = true;
      for (attachment, name) in &large {
        if !self.upload_attachment(&client, &mut state, attachment, &message_id, name).await {
          tracing::warn!("Could not prepare attachment session for {}", name);
          uploaded = false;
          break;
        }
      }
      if !uploaded {
        has_error = true;
        continue;
      }
      // Upstream posts the JSON literal null to send the draft
      let send_url = format!("{}/messages/{}/send", user_path, message_id);
      if !self.fetch(&client, &state, reqwest::Method::POST, &send_url, Body::Json(Value::Null)).await.0 {
        tracing::warn!("Could not send drafted email id: {}", message_id);
        has_error = true;
        continue;
      }
      crate::delivery::mark_delivered(to_addr, false);
    }
    Ok(!has_error)
  }
}

fn as_int(v: &Value) -> Option<i64> {
  match v {
    Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
    Value::String(s) => s.trim().parse().ok(),
    _ => None,
  }
}

/// `^[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}$` (case-insensitive).
fn is_object_id(s: &str) -> bool {
  let parts: Vec<&str> = s.split('-').collect();
  parts.len() == 5 && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| p.len() == n && p.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notify::registry::from_url;
  use wiremock::matchers::{body_partial_json, body_string, header, method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const SECRET_PATH: &str = "abcd/123/3343/@jack/test";

  fn parse_o365(url: &str) -> Office365 {
    Office365::from_url(&ParsedUrl::parse(url).unwrap()).unwrap()
  }

  async fn mount_token(server: &MockServer, tenant: &str) {
    Mock::given(method("POST"))
      .and(path(format!("/{}/oauth2/v2.0/token", tenant)))
      .and(header("Content-Type", "application/x-www-form-urlencoded"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"expires_in": 2000, "access_token": "abcd1234"})))
      .mount(server)
      .await;
  }

  fn json_bodies(reqs: &[wiremock::Request], p: &str) -> Vec<Value> {
    reqs.iter().filter(|r| r.url.path() == p).map(|r| serde_json::from_slice(&r.body).unwrap()).collect()
  }

  #[test]
  fn test_invalid_urls() {
    let urls = vec![
      "o365://".to_string(),
      "o365://:@/".to_string(),
      // Invalid tenant / client id
      format!("o365://user@example.com/,/ab-cd-ef-gh/{}/email1@test.ca", SECRET_PATH),
      format!("o365://user2@example.com/tenant/ab./{}/email1@test.ca", SECRET_PATH),
      // No secret
      "o365://user@example.com/tenant/ab-cd-ef-gh/email1@test.ca".to_string(),
      // Personal mode needs an email source
      "o365://hg-fe-dc-ba/client123/refresh?mode=personal".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_none(), "Should not parse: {}", url);
    }
  }

  #[test]
  fn test_valid_urls() {
    let urls = vec![
      format!("o365://tenant/ab-cd-ef-gh/{}/email1@test.ca", SECRET_PATH),
      format!("o365://user@example.edu/tenant/ab-cd-ef-gh/{}/email1@test.ca", SECRET_PATH),
      format!("o365://hg-fe-dc-ba/tenant/ab-cd-ef-gh/{}/email1@test.ca", SECRET_PATH),
      format!("o365://hg-fe-dc-ba/tenant/ab-cd-ef-gh/{}/", SECRET_PATH),
      format!("o365://_/?oauth_id=ab-cd-ef-gh&oauth_secret={}&tenant=tenant&to=email1@test.ca&from=user@example.ca", SECRET_PATH),
      format!("o365://user@example.com/tenant/ab-cd-ef-gh/{}", SECRET_PATH),
      "o365://tenant:user@example.com/01-12-23-34/abcd/321/4321/@test/test/email1@test.ca".to_string(),
      format!("o365://user@example.edu/tenant/ab-cd-ef-gh/{}/email1@test.ca?reply_to=reply@example.org", SECRET_PATH),
      format!("o365://user@example.edu/tenant/ab-cd-ef-gh/{}/email1@test.ca?savesent=no", SECRET_PATH),
      format!("azure://user@example.edu/tenant/ab-cd-ef-gh/{}/email1@test.ca", SECRET_PATH),
      "azure://sender@outlook.com/client123/refreshtoken123/target@example.com".to_string(),
    ];
    for url in &urls {
      assert!(from_url(url).is_some(), "Should parse: {}", url);
    }
  }

  #[test]
  fn test_from_url_fields() {
    let o = parse_o365(&format!("o365://user@example.edu/tenant/ab-cd-ef-gh/{}/email2@test.ca/email1@test.ca", SECRET_PATH));
    assert_eq!(o.mode, Mode::Org);
    assert_eq!(o.source, "user@example.edu");
    assert_eq!(o.tenant.as_deref(), Some("tenant"));
    assert_eq!(o.client_id, "ab-cd-ef-gh");
    assert_eq!(o.secret, SECRET_PATH);
    // Targets are collected from the end of the path
    assert_eq!(o.targets, vec![(None, "email1@test.ca".to_string()), (None, "email2@test.ca".to_string())]);
    assert!(o.save_sent);

    // No tenant needed when the source is a consumer account
    let o = parse_o365("azure://sender@outlook.com/client123/refreshtoken123/target@example.com");
    assert_eq!(o.mode, Mode::Personal);
    assert_eq!(o.tenant, None);
    assert_eq!(o.client_id, "client123");
    assert_eq!(o.secret, "refreshtoken123");

    // ?mode=org overrides detection; an unknown mode falls back to it
    assert_eq!(parse_o365("azure://sender@outlook.com/tenant123/client123/secret123/t@example.com?mode=org").mode, Mode::Org);
    assert_eq!(parse_o365("azure://sender@outlook.com/client123/secret123/t@example.com?mode=bogus").mode, Mode::Personal);
    assert_eq!(parse_o365("azure://sender@example.com/client123/refresh/t@example.com?mode=personal").mode, Mode::Personal);

    // tenant:user@host keeps user@host as the source
    let o = parse_o365("o365://tenant:user@example.com/01-12-23-34/abcd/321/4321/@test/test/email1@test.ca");
    assert_eq!(o.source, "tenant@example.com");
    assert_eq!(o.tenant.as_deref(), Some("01-12-23-34"));
    assert_eq!(o.secret, "321/4321/@test/test");

    // No targets: notify the source itself
    let o = parse_o365(&format!("o365://user@example.com/tenant/ab-cd-ef-gh/{}", SECRET_PATH));
    assert_eq!(o.targets, vec![(None, "user@example.com".to_string())]);
    // ... unless it is an object id
    let o = parse_o365(&format!("o365://hg-fe-dc-ba/tenant/ab-cd-ef-gh/{}/", SECRET_PATH));
    assert!(o.targets.is_empty());

    let o = parse_o365(&format!(
      "o365://user@example.com/tenant/ab-cd-ef-gh/{}/email1@test.ca?cc=Jane%20Doe%20<cc@test.com>,cc2@test.com&bcc=bcc@test.com&reply_to=Me:reply@test.com&savesent=no",
      SECRET_PATH
    ));
    assert_eq!(o.cc.iter().cloned().collect::<Vec<_>>(), vec!["cc2@test.com", "cc@test.com"]);
    assert_eq!(o.names.get("cc@test.com").map(String::as_str), Some("Jane Doe"));
    assert_eq!(o.names.get("reply@test.com").map(String::as_str), Some("Me"));
    assert_eq!(o.bcc.iter().cloned().collect::<Vec<_>>(), vec!["bcc@test.com"]);
    assert!(!o.save_sent);

    assert!(parse_o365(&format!("o365://01234567-89ab-cdef-0123-456789abcdef/tenant/ab-cd-ef-gh/{}/", SECRET_PATH)).source_is_object_id);
  }

  #[test]
  fn test_url_decoding() {
    // The query reads `+` as a space; user and host are decoded only once
    let o = parse_o365("azure://jo%2541@example.com/tenant/client123/secret/target@example.com?cc=Chris+Doe<cc@example.com>&to=x%2540y.com");
    assert_eq!(o.source, "jo%41@example.com");
    assert_eq!(o.names.get("cc@example.com").map(String::as_str), Some("Chris Doe"));
    // Entries of ?to= are unquoted once more, as upstream's parse_list() does
    assert!(o.targets.iter().any(|(_, a)| a == "x@y.com"), "{:?}", o.targets);
  }

  #[tokio::test]
  async fn test_send_org() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/tenant123/oauth2/v2.0/token"))
      .and(body_string("grant_type=client_credentials&client_id=client123&client_secret=secret123&scope=https%3A%2F%2Fgraph.microsoft.com%2F.default"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"expires_in": 2000, "access_token": "token"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/v1.0/users/sender@example.com/sendMail"))
      .and(header("Authorization", "Bearer token"))
      .and(header("Content-Type", "application/json"))
      .respond_with(ResponseTemplate::new(202))
      .expect(2)
      .mount(&server)
      .await;

    let svc = from_url("azure://sender@example.com/tenant123/client123/secret123/target@example.com?reply_to=reply@example.com&savesent=no").unwrap();
    let ctx = NotifyContext { title: "Test Subject".into(), body: "hello".into(), body_format: NotifyFormat::Html, ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // The token is reused until it expires
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
      json_bodies(&reqs, "/v1.0/users/sender@example.com/sendMail")[0],
      json!({
        "message": {
          "subject": "Test Subject",
          "body": {"contentType": "HTML", "content": "hello"},
          "from": {"emailAddress": {"address": "sender@example.com", "name": "Apprise"}},
          "replyTo": [{"emailAddress": {"address": "reply@example.com"}}],
          "toRecipients": [{"emailAddress": {"address": "target@example.com"}}],
        },
        "saveToSentItems": "false",
      })
    );
  }

  #[tokio::test]
  async fn test_send_personal_rotates_refresh_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(path("/consumers/oauth2/v2.0/token"))
      .and(body_string(
        "grant_type=refresh_token&client_id=client123&refresh_token=refreshtoken123&scope=https%3A%2F%2Fgraph.microsoft.com%2FMail.Send+offline_access",
      ))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"expires_in": 5, "access_token": "token", "refresh_token": "rotated"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path("/consumers/oauth2/v2.0/token"))
      .and(body_string("grant_type=refresh_token&client_id=client123&refresh_token=rotated&scope=https%3A%2F%2Fgraph.microsoft.com%2FMail.Send+offline_access"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"expires_in": 2000, "access_token": "token2"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1.0/me/sendMail")).respond_with(ResponseTemplate::new(202)).expect(2).mount(&server).await;

    let svc = from_url("azure://sender@outlook.com/client123/refreshtoken123/target@example.com").unwrap();
    let ctx = NotifyContext { title: "t".into(), body: "hello".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    // expires_in 5 leaves no usable lifetime, so the rotated token is used next
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let reqs = server.received_requests().await.unwrap();
    let mail = json_bodies(&reqs, "/v1.0/me/sendMail");
    assert_eq!(mail[0]["message"]["from"], json!({"emailAddress": {"address": "sender@outlook.com", "name": "Apprise"}}));
    assert_eq!(mail[0]["message"]["body"]["contentType"], "Text");
  }

  #[tokio::test]
  async fn test_send_resolves_from_for_object_id() {
    let server = MockServer::start().await;
    mount_token(&server, "tenant").await;
    Mock::given(method("GET"))
      .and(path("/v1.0/users/hg-fe-dc-ba"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"mail": "user@example.ca", "displayName": "John"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1.0/users/hg-fe-dc-ba/sendMail")).respond_with(ResponseTemplate::new(200)).expect(2).mount(&server).await;

    let svc = from_url(&format!("o365://hg-fe-dc-ba/tenant/ab-cd-ef-gh/{}/email1@test.ca?cc=cc@test.ca,email1@test.ca&bcc=bcc@test.ca", SECRET_PATH)).unwrap();
    let ctx = NotifyContext { title: "t".into(), body: "b".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let mail = json_bodies(&server.received_requests().await.unwrap(), "/v1.0/users/hg-fe-dc-ba/sendMail");
    assert_eq!(mail[0]["message"]["from"], json!({"emailAddress": {"address": "user@example.ca", "name": "John"}}));
    // The recipient is never copied to itself
    assert_eq!(mail[0]["message"]["ccRecipients"], json!([{"emailAddress": {"address": "cc@test.ca"}}]));
    assert_eq!(mail[0]["message"]["bccRecipients"], json!([{"emailAddress": {"address": "bcc@test.ca"}}]));
  }

  #[tokio::test]
  async fn test_send_object_id_lookup_failure() {
    let server = MockServer::start().await;
    mount_token(&server, "tenant").await;
    Mock::given(method("GET")).and(path("/v1.0/users/01234567-89ab-cdef-0123-456789abcdef")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    Mock::given(method("POST"))
      .and(path("/v1.0/users/01234567-89ab-cdef-0123-456789abcdef/sendMail"))
      .respond_with(ResponseTemplate::new(200))
      .expect(0)
      .mount(&server)
      .await;
    let svc = from_url(&format!("o365://01234567-89ab-cdef-0123-456789abcdef/tenant/ab-cd-ef-gh/{}/email1@test.ca", SECRET_PATH)).unwrap();
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[tokio::test]
  async fn test_send_inline_attachments() {
    let server = MockServer::start().await;
    mount_token(&server, "tenant123").await;
    Mock::given(method("POST")).and(path("/v1.0/users/sender@example.com/sendMail")).respond_with(ResponseTemplate::new(202)).expect(1).mount(&server).await;
    let svc = from_url("azure://sender@example.com/tenant123/client123/secret123/target@example.com").unwrap();
    let mut ctx = NotifyContext { title: "title".into(), body: "body".into(), body_format: NotifyFormat::Html, ..Default::default() };
    ctx.attachments.push(Attachment { name: "pixel.png".into(), data: b"PNG".to_vec(), mime_type: "image/png".into() });
    ctx.attachments.push(Attachment { name: String::new(), data: b"plain".to_vec(), mime_type: "text/plain".into() });
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    let mail = json_bodies(&server.received_requests().await.unwrap(), "/v1.0/users/sender@example.com/sendMail");
    assert_eq!(
      mail[0]["message"]["attachments"],
      json!([
        {"@odata.type": "#microsoft.graph.fileAttachment", "name": "pixel.png", "contentType": "image/png", "contentBytes": "UE5H"},
        {"@odata.type": "#microsoft.graph.fileAttachment", "name": "file002.dat", "contentType": "text/plain", "contentBytes": "cGxhaW4="},
      ])
    );
  }

  #[tokio::test]
  async fn test_send_large_attachment_through_draft() {
    let server = MockServer::start().await;
    mount_token(&server, "tenant123").await;
    let base = "/v1.0/users/sender@example.com";
    Mock::given(method("POST"))
      .and(path(format!("{}/messages", base)))
      .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": "draft1"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST"))
      .and(path(format!("{}/message/draft1/attachments/createUploadSession", base)))
      .respond_with(ResponseTemplate::new(201).set_body_json(json!({"uploadUrl": format!("{}/upload/session1", server.uri())})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("PUT")).and(path("/upload/session1")).respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
    Mock::given(method("POST"))
      .and(path(format!("{}/messages/draft1/send", base)))
      .and(body_string("null"))
      .respond_with(ResponseTemplate::new(202))
      .expect(1)
      .mount(&server)
      .await;

    let svc = from_url("azure://sender@example.com/tenant123/client123/secret123/target@example.com").unwrap();
    let mut ctx = NotifyContext { title: "t".into(), body: "b".into(), ..Default::default() };
    let size = INLINE_ATTACHMENT_MAX + 1;
    ctx.attachments.push(Attachment { name: "big.bin".into(), data: vec![7u8; size], mime_type: "application/octet-stream".into() });
    assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());

    let reqs = server.received_requests().await.unwrap();
    let session = json_bodies(&reqs, &format!("{}/message/draft1/attachments/createUploadSession", base));
    assert_eq!(session[0], json!({"AttachmentItem": {"attachmentType": "file", "name": "big.bin", "contentType": "application/octet-stream", "size": size}}));
    let put = reqs.iter().find(|r| r.method.as_str() == "PUT" && r.url.path() == "/upload/session1").unwrap();
    assert_eq!(put.headers.get("content-range").unwrap(), format!("bytes 0-{}/{}", size - 1, size).as_str());
    assert_eq!(put.headers.get("authorization").unwrap(), "Bearer abcd1234");
    assert!(put.headers.get("content-type").is_none());
    // The draft carries no inline attachments
    assert!(json_bodies(&reqs, &format!("{}/messages", base))[0]["message"].get("attachments").is_none());
  }

  #[tokio::test]
  async fn test_send_failures() {
    let ctx = NotifyContext { body: "b".into(), ..Default::default() };

    // No recipients
    let svc = from_url(&format!("o365://hg-fe-dc-ba/tenant/ab-cd-ef-gh/{}/", SECRET_PATH)).unwrap();
    assert!(!svc.send(&ctx).await.unwrap());

    // Token request rejected / malformed
    for reply in
      [ResponseTemplate::new(401), ResponseTemplate::new(200).set_body_string("{"), ResponseTemplate::new(200).set_body_json(json!({"access_token": "x"}))]
    {
      let server = MockServer::start().await;
      Mock::given(method("POST")).and(path("/tenant123/oauth2/v2.0/token")).respond_with(reply).mount(&server).await;
      Mock::given(method("POST")).and(path("/v1.0/users/sender@example.com/sendMail")).respond_with(ResponseTemplate::new(202)).expect(0).mount(&server).await;
      let svc = from_url("azure://sender@example.com/tenant123/client123/secret123/target@example.com").unwrap();
      assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }

    // Mail rejected
    let server = MockServer::start().await;
    mount_token(&server, "tenant123").await;
    Mock::given(method("POST")).and(path("/v1.0/users/sender@example.com/sendMail")).respond_with(ResponseTemplate::new(999)).mount(&server).await;
    let svc = from_url("azure://sender@example.com/tenant123/client123/secret123/target@example.com").unwrap();
    assert!(!crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
  }

  #[test]
  fn test_static_details() {
    let details = Office365::static_details();
    assert_eq!(details.service_name, "Office 365");
    assert_eq!(details.protocols, vec!["azure", "o365"]);
    assert!(details.attachment_support);
  }

  #[tokio::test]
  async fn test_store_keeps_sender_and_rotated_token() {
    let server = MockServer::start().await;
    mount_token(&server, "tenant").await;
    // Org mode looks the sender up once; the next run reads it from the store
    Mock::given(method("GET"))
      .and(path("/v1.0/users/hg-fe-dc-ba"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"mail": "user@example.ca", "displayName": "John"})))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path("/v1.0/users/hg-fe-dc-ba/sendMail")).respond_with(ResponseTemplate::new(202)).expect(2).mount(&server).await;
    // Personal mode sends the rotated refresh token on the next run
    for (sent, rotated) in [("seed", "rotated"), ("rotated", "rotated2")] {
      Mock::given(method("POST"))
        .and(path("/consumers/oauth2/v2.0/token"))
        .and(body_string(format!(
          "grant_type=refresh_token&client_id=client123&refresh_token={}&scope=https%3A%2F%2Fgraph.microsoft.com%2FMail.Send+offline_access",
          sent
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"expires_in": 2000, "access_token": "token", "refresh_token": rotated})))
        .expect(1)
        .mount(&server)
        .await;
    }
    Mock::given(method("POST")).and(path("/v1.0/me/sendMail")).respond_with(ResponseTemplate::new(202)).expect(2).mount(&server).await;

    let tmp = tempfile::tempdir().unwrap();
    let asset = crate::AppriseAsset { storage_path: Some(tmp.path().to_path_buf()), ..Default::default() };
    let mut a = crate::Apprise::with_asset(asset);
    let ctx = NotifyContext { title: "t".into(), body: "b".into(), ..Default::default() };
    for _ in 0..2 {
      // A fresh instance each time, as a new run would load it
      a.clear();
      assert!(a.add(&format!("o365://hg-fe-dc-ba/tenant/ab-cd-ef-gh/{}/email1@test.ca", SECRET_PATH)));
      assert!(a.add("azure://sender@outlook.com/client123/seed/target@example.com"));
      assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
    }
    let reqs = server.received_requests().await.unwrap();
    for mail in json_bodies(&reqs, "/v1.0/users/hg-fe-dc-ba/sendMail") {
      assert_eq!(mail["message"]["from"], json!({"emailAddress": {"address": "user@example.ca", "name": "John"}}));
    }
  }

  #[tokio::test]
  async fn test_retry_skips_delivered_recipients() {
    let server = MockServer::start().await;
    mount_token(&server, "tenant123").await;
    let mail = "/v1.0/users/sender@example.com/sendMail";
    // The second recipient fails once; the retry only sends to it again.
    Mock::given(method("POST"))
      .and(path(mail))
      .and(body_partial_json(json!({"message": {"toRecipients": [{"emailAddress": {"address": "b@example.com"}}]}})))
      .respond_with(ResponseTemplate::new(500))
      .up_to_n_times(1)
      .with_priority(1)
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(path(mail)).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let mut a = crate::Apprise::new();
    assert!(a.add("azure://sender@example.com/tenant123/client123/secret123/a@example.com/b@example.com?retry=1&wait=0"));
    let ctx = NotifyContext { title: "t".into(), body: "b".into(), ..Default::default() };
    assert!(crate::notify::with_test_base(server.uri(), a.notify(&ctx)).await.is_success());
    let to: Vec<Value> = json_bodies(&server.received_requests().await.unwrap(), mail)
      .iter()
      .map(|b| b["message"]["toRecipients"][0]["emailAddress"]["address"].clone())
      .collect();
    // Trailing path addresses are read from the end, as upstream does
    assert_eq!(to, vec![json!("b@example.com"), json!("a@example.com"), json!("b@example.com")]);
  }

  /// YAML `source:` and `tenant:` settings replace what the URL gives, as
  /// upstream's constructor arguments do. The URL's own source still
  /// decides the mode and its tenant still takes its place in the path; a
  /// personal account has no tenant. Upstream, for the same YAML: token
  /// from tenantB with secret `abcd/123`, mail from other@example.edu (uid
  /// 16b88c37); token from consumers, mail from other@outlook.com (uid
  /// 5caccf75); token from tenantA, mail as other@outlook.com (uid
  /// d70ea4a6).
  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_yaml_source_and_tenant_replace_the_urls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
      .and(wiremock::matchers::path_regex("/oauth2/v2.0/token$"))
      .respond_with(ResponseTemplate::new(200).set_body_json(json!({"expires_in": 2000, "access_token": "abcd1234", "refresh_token": "rt2"})))
      .mount(&server)
      .await;
    Mock::given(method("POST")).and(wiremock::matchers::path_regex("/sendMail$")).respond_with(ResponseTemplate::new(202)).mount(&server).await;
    let doc = concat!(
      "urls:\n",
      "  - azure://user@example.edu/tenantA/ab-cd-ef-gh/abcd/123/rcpt@example.com:\n      source: other@example.edu\n      tenant: tenantB\n",
      "  - o365://user@outlook.com/ab-cd-ef-gh/refreshtoken/rcpt@example.com:\n      source: other@outlook.com\n      tenant: tenantB\n",
      "  - o365://user@example.edu/tenantA/ab-cd-ef-gh/abcd/rcpt@example.com:\n      source: other@outlook.com\n",
    );
    let (a, uids) = crate::config::yaml::load_for_test(doc).await;
    assert_eq!(uids, ["16b88c37", "5caccf75", "d70ea4a6"]);
    for svc in a.services() {
      let ctx = NotifyContext { title: "title".into(), body: "body".into(), body_format: NotifyFormat::Html, ..Default::default() };
      assert!(crate::notify::with_test_base(server.uri(), svc.send(&ctx)).await.unwrap());
    }
    let sent: Vec<String> = server
      .received_requests()
      .await
      .unwrap()
      .iter()
      .filter(|r| r.method.as_str() == "POST")
      .map(|r| match r.url.path().ends_with("/token") {
        true => {
          let form: HashMap<String, String> = url::form_urlencoded::parse(&r.body).into_owned().collect();
          format!("{} {}", r.url.path(), form.get("client_secret").or(form.get("refresh_token")).unwrap())
        }
        false => {
          let body: Value = serde_json::from_slice(&r.body).unwrap();
          format!("{} {}", r.url.path(), body["message"]["from"]["emailAddress"]["address"].as_str().unwrap())
        }
      })
      .collect();
    assert_eq!(
      sent,
      [
        "/tenantB/oauth2/v2.0/token abcd/123",
        "/v1.0/users/other@example.edu/sendMail other@example.edu",
        "/consumers/oauth2/v2.0/token refreshtoken",
        "/v1.0/me/sendMail other@outlook.com",
        "/tenantA/oauth2/v2.0/token abcd",
        "/v1.0/users/other@outlook.com/sendMail other@outlook.com",
      ]
    );
  }
}
