//! Email over SMTP (upstream `apprise/plugins/email/`).
//!
//! `mailto://user:pass@example.com/to@example.org` sends through the SMTP
//! server of the sender's provider when it is a known one ([`templates`]),
//! else through the URL's host (`?smtp=` names another). Messages are HTML
//! with a plain-text alternative, or plain text, built as Python's `email`
//! package builds them ([`mime`]); `?pgp=sign|encrypt` adds PGP/MIME
//! (RFC 3156) with keys from `pgppub=`/`pgpprv=`, the service's storage
//! directory, or WKD (`?wkd=yes`), and `?inline=yes` shows image
//! attachments inline (RFC 2387).

mod message;
pub(crate) mod mime;
pub(crate) mod smtp;
pub(crate) mod templates;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;

use self::smtp::{SecureMode, SmtpConfig, SmtpConnection};
use self::templates::WebBaseLogin;
use crate::error::NotifyError;
use crate::notify::{Notify, NotifyContext, ServiceDetails};
use crate::types::NotifyFormat;
use crate::utils::email::{EmailAddress, formataddr, is_email, is_space, parse_emails};
use crate::utils::parse::{ParsedUrl, parse_bool_or};
use crate::utils::pgp::PgpController;
use crate::utils::wkd::WkdController;

/// Upstream `NotifyEmail.socket_connect_timeout` (seconds).
const SOCKET_CONNECT_TIMEOUT: f64 = 15.0;

/// The PGP treatment of outgoing mail (upstream `PGPMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgpMode {
  /// No PGP (default).
  None,
  /// Sign with the sender's key; also encrypt when the recipient's key is
  /// known.
  Sign,
  /// Encrypt to the recipient's key; fail without one.
  Encrypt,
}

impl PgpMode {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::None => "no",
      Self::Sign => "sign",
      Self::Encrypt => "encrypt",
    }
  }

  /// Upstream's prefix match: `e`, `en`, ... select `encrypt`; `none`
  /// means `no`.
  fn parse(value: &str) -> Option<Self> {
    let value = value.to_lowercase();
    if value == "none" {
      return Some(Self::None);
    }
    [Self::None, Self::Sign, Self::Encrypt].into_iter().find(|m| m.as_str().starts_with(&value))
  }
}

/// The display name kept for an address (upstream stores the name, `False`,
/// or the asset's `app_id`, which is only known when sending).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromName {
  /// The application name (`AppriseAsset.app_id`).
  AppId,
  Name(String),
  None,
}

impl FromName {
  fn from_parsed(email: &EmailAddress) -> Self {
    if email.name.is_empty() { Self::None } else { Self::Name(email.name.clone()) }
  }
}

pub struct Email {
  /// `mailtos://` (or a provider that requires security).
  secure: bool,
  user: Option<String>,
  password: Option<String>,
  host: String,
  smtp_host: String,
  port: u16,
  secure_mode: SecureMode,
  from_name: FromName,
  from_addr: String,
  targets: Vec<(Option<String>, String)>,
  cc: Vec<String>,
  bcc: Vec<String>,
  reply_to: Vec<String>,
  names: HashMap<String, FromName>,
  /// The URL's `+name=value` headers.
  headers: IndexMap<String, String>,
  pgp_mode: PgpMode,
  pgp: PgpController,
  inline: bool,
  tz: Option<crate::utils::time::Timezone>,
  timeout: Duration,
  verify_certificate: bool,
  tags: Vec<String>,
}

/// What a subclass of Email (Delta Chat) changes when sending.
pub(crate) struct Protocol<'a> {
  /// The subject to use instead of the title.
  pub subject: Option<String>,
  /// Headers added after the URL's own (upstream `_protocol_headers`).
  pub headers: &'a [(&'a str, &'a str)],
}

impl Email {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    Self::parse(url, url.schema == "mailtos")
  }

  /// Upstream `NotifyEmail.parse_url()` followed by `__init__()`; `secure`
  /// is whether the URL used the secure schema.
  pub(crate) fn parse(url: &ParsedUrl, mut secure: bool) -> Option<Self> {
    // ── parse_url() ──
    let mut targets: Vec<String> = Vec::new();
    let mut host = url.host.clone().unwrap_or_default();
    if !crate::utils::validate::is_ipaddr(&host) && crate::utils::validate::verify_hostname(&host, false, false, false).is_none() {
      // An address in the host position is kept as a recipient
      if is_email(&host).is_some() {
        targets.push(host.clone());
      }
      host.clear();
    }
    let pgp_raw = url.get("pgp").filter(|v| !v.is_empty());
    let use_wkd = url.get("wkd").filter(|v| !v.is_empty()).is_some_and(|v| parse_bool_or(v, false));
    let pgp_key = url.get("pgppub").filter(|v| !v.is_empty()).map(str::to_string);
    let pgp_privkey = url.get("pgpprv").filter(|v| !v.is_empty()).map(str::to_string);
    let inline = url.get("inline").is_some_and(|v| parse_bool_or(v, false));
    targets.extend(url.path_parts.iter().cloned());
    if let Some(to) = url.get("to").filter(|v| !v.is_empty()) {
      targets.push(to.to_string());
    }
    let name = url.get("name").filter(|v| !v.is_empty());
    let from_arg = match url.get("from").filter(|v| !v.is_empty()) {
      Some(from) => Some(match name {
        Some(name) => formataddr(Some(name), from),
        None => from.to_string(),
      }),
      None => name.map(str::to_string),
    };
    let smtp_arg = url.get("smtp").filter(|v| !v.is_empty()).map(str::to_string);
    let mode_arg = url.get("mode").filter(|v| !v.is_empty()).map(str::to_lowercase);

    // ── __init__() ──
    let mut user = url.user.clone();
    let password = url.password.clone();
    let mut port = url.port.filter(|p| *p != 0);
    let mut names = HashMap::new();
    let mut secure_mode = match &mode_arg {
      Some(mode) => match SecureMode::parse(mode) {
        Some(m) => m,
        None => {
          tracing::warn!("The secure mode specified ({}) is invalid.", mode);
          return None;
        }
      },
      None if secure => SecureMode::StartTls,
      None => SecureMode::Insecure,
    };

    let mut collect = |value: Option<&str>, kind: &str| -> Vec<String> {
      let mut out: Vec<String> = Vec::new();
      for entry in parse_emails(value.unwrap_or_default()) {
        match is_email(&entry) {
          Some(email) => {
            names.insert(email.full_email.clone(), FromName::from_parsed(&email));
            if !out.contains(&email.full_email) {
              out.push(email.full_email);
            }
          }
          None => tracing::warn!("Dropped invalid {} email ({}) specified.", kind, entry),
        }
      }
      out
    };
    let cc = collect(url.get("cc"), "Carbon Copy");
    let bcc = collect(url.get("bcc"), "Blind Carbon Copy");
    let reply_to = collect(url.get("reply"), "Reply To");

    // apply_email_defaults(): the provider's server, port and mode
    let mut smtp_host = smtp_arg.clone();
    let mut from_addr = (FromName::None, String::new());
    if smtp_host.is_none() {
      let first_word = |u: &str| u.split(|c: char| c == '@' || is_space(c)).next().unwrap_or_default().to_string();
      let lookup = match &user {
        Some(u) => format!("{}@{}", first_word(u), host),
        None => host.clone(),
      };
      if let Some((template, id)) = templates::find_template(&lookup) {
        tracing::debug!("Applying {} Defaults", template.name);
        secure = template.secure.unwrap_or(secure);
        smtp_host = template.smtp_host.map(str::to_string);
        if port.is_none() {
          port = template.port;
        }
        if mode_arg.is_none()
          && let Some(mode) = template.secure_mode
        {
          secure_mode = mode;
        }
        if !template.login_type.is_empty()
          && let Some(u) = user.clone()
        {
          if is_email(&u).is_some() {
            if !template.login_type.contains(&WebBaseLogin::Email) {
              // The provider wants the user ID, not the address
              user = id;
            } else {
              host = u.split('@').nth(1).unwrap_or_default().to_string();
            }
          } else if !template.login_type.contains(&WebBaseLogin::UserId) {
            user = Some(format!("{}@{}", u, host));
          }
        }
        if let Some(from_user) = template.from_user {
          from_addr.1 = format!("{}@{}", from_user, host);
        }
      }
    }

    if let Some(u) = &user {
      if !host.is_empty() {
        let local = u.split(|c: char| c == '@' || is_space(c)).next().unwrap_or_default();
        from_addr = (FromName::AppId, format!("{}@{}", local, host));
      } else if let Some(email) = is_email(u) {
        host = email.domain;
        from_addr = (FromName::AppId, u.clone());
      }
    }
    if let Some(from) = &from_arg {
      match is_email(from) {
        Some(email) => from_addr = (FromName::from_parsed(&email), email.full_email),
        // Only a name: keep the detected address
        None => from_addr.0 = FromName::Name(from.clone()),
      }
    }
    if is_email(&from_addr.1).is_none() {
      tracing::warn!("Invalid ~From~ email specified: {}", from_addr.1);
      return None;
    }
    names.insert(from_addr.1.clone(), from_addr.0.clone());

    let mut to = Vec::new();
    if targets.is_empty() {
      // Without recipients, the sender notifies itself
      to.push((None, from_addr.1.clone()));
    } else {
      for entry in targets.iter().flat_map(|t| parse_emails(t)) {
        match is_email(&entry) {
          Some(email) => to.push(((!email.name.is_empty()).then(|| email.name.clone()), email.full_email)),
          None => tracing::warn!("Dropped invalid To email ({}) specified.", entry),
        }
      }
    }

    if !secure && secure_mode != SecureMode::Insecure {
      secure = true;
    }
    let port = port.unwrap_or(secure_mode.default_port());
    let smtp_host = smtp_host.filter(|h| !h.is_empty()).unwrap_or_else(|| host.clone());

    // An explicit pgp= wins over the encrypt mode wkd=yes implies
    let pgp_mode = match pgp_raw {
      None if use_wkd => PgpMode::Encrypt,
      None => PgpMode::None,
      Some(raw) => match PgpMode::parse(raw) {
        Some(mode) => mode,
        None => {
          tracing::warn!("The Email PGP mode specified ({}) is invalid.", raw);
          return None;
        }
      },
    };
    let verify_certificate = url.verify_certificate();
    let wkd = use_wkd.then(|| WkdController::new(verify_certificate));
    let pgp = PgpController::new(pgp_key.as_deref(), pgp_privkey.as_deref(), Some(&from_addr.1), "Apprise", wkd);

    let headers: IndexMap<String, String> = url.qsd_add.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let timeout = url.get("cto").and_then(|v| v.trim().parse::<f64>().ok()).filter(|t| t.is_finite() && *t > 0.0).unwrap_or(SOCKET_CONNECT_TIMEOUT);

    Some(Self {
      secure,
      user,
      password,
      host,
      smtp_host,
      port,
      secure_mode,
      from_name: from_addr.0,
      from_addr: from_addr.1,
      targets: to,
      cc,
      bcc,
      reply_to,
      names,
      headers,
      pgp_mode,
      pgp,
      inline,
      tz: url.get("tz").and_then(crate::utils::time::zoneinfo),
      timeout: Duration::from_secs_f64(timeout),
      verify_certificate,
      tags: url.tags(),
    })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "E-Mail",
      service_url: None,
      setup_url: Some("https://appriseit.com/services/email/"),
      protocols: vec!["mailto", "mailtos"],
      description: "Send notifications via SMTP email.",
      attachment_support: true,
    }
  }

  /// The PGP mode (`?pgp=`, or `encrypt` implied by `?wkd=yes`).
  pub fn pgp_mode(&self) -> PgpMode {
    self.pgp_mode
  }

  /// The values that make this service distinct (upstream
  /// `url_identifier`): schema, credentials, host, SMTP server and port.
  pub(crate) fn identifier(&self, schema: &str) -> Vec<String> {
    vec![
      schema.to_string(),
      self.user.clone().unwrap_or_default(),
      self.password.clone().unwrap_or_default(),
      self.host.clone(),
      self.smtp_host.clone(),
      self.port.to_string(),
    ]
  }

  /// Upstream `NotifyEmail.send()`: one SMTP session for every recipient.
  pub(crate) async fn deliver(&self, ctx: &NotifyContext, protocol: Protocol<'_>) -> Result<bool, NotifyError> {
    if self.targets.is_empty() {
      tracing::warn!("There are no Email recipients to notify");
      return Ok(false);
    }
    crate::notify::throttle().await;
    let tls = self.tls_connector();
    let config = SmtpConfig {
      host: &self.smtp_host,
      port: self.port,
      mode: self.secure_mode,
      user: self.user.as_deref(),
      password: self.password.as_deref(),
      tls,
      timeout: self.timeout,
    };
    let mut conn = match SmtpConnection::open(config).await {
      Ok(conn) => conn,
      Err(e) => {
        tracing::warn!("Connection error while submitting email to \"{}\"", self.smtp_host);
        tracing::debug!("Socket Exception: {}", e);
        return Ok(false);
      }
    };

    let mut headers: IndexMap<String, String> = IndexMap::new();
    headers.insert("X-Application".to_string(), ctx.asset.app_id.clone());
    headers.extend(self.headers.iter().map(|(k, v)| (k.clone(), v.clone())));
    headers.extend(protocol.headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let headers: Vec<(String, String)> = headers.into_iter().collect();

    let store_path = crate::storage::with_service_store(|s| s.path().map(std::path::Path::to_path_buf)).flatten();
    let pgp = self.pgp.with_path(store_path).with_app_id(&ctx.asset.app_id);
    let title = protocol.subject.unwrap_or_else(|| ctx.title.clone());
    // A subject is one header line (RFC 5322 section 2.2.3)
    let subject = title.split(is_space).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    let format = if ctx.body_format == NotifyFormat::Html { NotifyFormat::Html } else { NotifyFormat::Text };
    let smtp_host = if self.smtp_host.is_empty() { self.from_addr.split('@').nth(1).unwrap_or_default() } else { self.smtp_host.as_str() };
    tracing::debug!("SMTP Host: {}", smtp_host);
    let spec = message::MessageSpec {
      subject: &subject,
      body: &ctx.body,
      format,
      from_name: &self.from_name,
      from_addr: &self.from_addr,
      cc: &self.cc,
      bcc: &self.bcc,
      reply_to: &self.reply_to,
      smtp_host,
      attachments: &ctx.attachments,
      headers: &headers,
      names: &self.names,
      pgp: (self.pgp_mode != PgpMode::None).then_some(&pgp),
      pgp_mode: self.pgp_mode,
      inline: self.inline,
      tz: self.tz,
      app_id: &ctx.asset.app_id,
    };

    let mut has_error = false;
    for (to_name, to_addr) in &self.targets {
      let email = match message::prepare_email(&spec, to_name.as_deref(), to_addr).await {
        Ok(email) => email,
        Err(e) => {
          // As upstream's generator raising: later recipients are not sent
          tracing::debug!("Socket Exception: {}", e.0);
          has_error = true;
          break;
        }
      };
      // A retry skips recipients that already accepted the message
      if crate::delivery::is_delivered(&email.recipient, false) {
        continue;
      }
      if conn.sendmail(&self.from_addr, &email.to_addrs, &email.body).await {
        tracing::info!("Sent Email to {}", email.recipient);
        crate::delivery::mark_delivered(&email.recipient, false);
      } else {
        tracing::warn!("Sending email to \"{}\" failed.", email.recipient);
        has_error = true;
      }
    }
    conn.quit().await;
    pgp.prune();
    Ok(!has_error)
  }

  fn tls_connector(&self) -> tokio_rustls::TlsConnector {
    #[cfg(test)]
    if let Ok(connector) = tests::TEST_TLS.try_with(|c| c.clone()) {
      return connector;
    }
    crate::utils::socket::tls_connector(self.verify_certificate)
  }
}

#[async_trait]
impl Notify for Email {
  fn schemas(&self) -> &[&str] {
    &["mailto", "mailtos"]
  }
  fn service_name(&self) -> &str {
    "E-Mail"
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
  fn notify_formats(&self) -> Vec<NotifyFormat> {
    vec![NotifyFormat::Html, NotifyFormat::Text]
  }
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(self.identifier(if self.secure { "mailtos" } else { "mailto" }))
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    self.deliver(ctx, Protocol { subject: None, headers: &[] }).await
  }
}
