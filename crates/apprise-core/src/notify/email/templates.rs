//! Known email providers (upstream `apprise/plugins/email/templates.py`).
//!
//! When a URL does not name its SMTP server, the sender address is matched
//! against this table, in order, to fill in the provider's server, port and
//! security mode. The last two entries (local mail server and catch-all)
//! keep the URL's host as the server.

use std::sync::LazyLock;

use regex_lite::Regex;

use super::smtp::SecureMode;

/// The login a provider expects (upstream `WebBaseLogin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebBaseLogin {
  /// The login must be the full email address.
  Email,
  /// The login must be the user ID (the part before the `@`).
  UserId,
}

/// One provider's defaults.
#[derive(Debug)]
pub(crate) struct EmailTemplate {
  pub name: &'static str,
  pattern: &'static str,
  /// The SMTP port, unless the URL gives one.
  pub port: Option<u16>,
  /// The SMTP server; `None` keeps the URL's host.
  pub smtp_host: Option<&'static str>,
  /// Whether the provider requires a secure connection.
  pub secure: Option<bool>,
  /// The security mode, unless the URL gives `?mode=`.
  pub secure_mode: Option<SecureMode>,
  /// Accepted login forms; empty accepts whatever the URL gives.
  pub login_type: &'static [WebBaseLogin],
  /// The sender's user name when the URL has none.
  pub from_user: Option<&'static str>,
}

const EMAIL_ONLY: &[WebBaseLogin] = &[WebBaseLogin::Email];
const USERID_ONLY: &[WebBaseLogin] = &[WebBaseLogin::UserId];

/// A provider with the usual `label+id@domain` sender pattern.
const fn provider(
  name: &'static str,
  pattern: &'static str,
  port: u16,
  smtp_host: &'static str,
  secure_mode: SecureMode,
  login_type: &'static [WebBaseLogin],
) -> EmailTemplate {
  EmailTemplate { name, pattern, port: Some(port), smtp_host: Some(smtp_host), secure: Some(true), secure_mode: Some(secure_mode), login_type, from_user: None }
}

/// Upstream `EMAIL_TEMPLATES`, in the same order.
const EMAIL_TEMPLATES: &[EmailTemplate] = &[
  provider("Google Mail", r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>gmail\.com)$", 587, "smtp.gmail.com", SecureMode::StartTls, EMAIL_ONLY),
  provider(
    "Yandex",
    r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>yandex\.(com|ru|ua|by|kz|uz|tr|fr))$",
    465,
    "smtp.yandex.ru",
    SecureMode::Ssl,
    USERID_ONLY,
  ),
  provider(
    "Microsoft Hotmail",
    r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>(hotmail|live)\.com(\.au)?)$",
    587,
    "smtp-mail.outlook.com",
    SecureMode::StartTls,
    EMAIL_ONLY,
  ),
  provider(
    "Microsoft Outlook",
    r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>(smtp\.)?outlook\.com(\.au)?)$",
    587,
    "smtp.outlook.com",
    SecureMode::StartTls,
    EMAIL_ONLY,
  ),
  // An authenticated sender (from=) and a recipient (to=) are required
  provider("Microsoft Office 365", r"^[^@]+@(?P<domain>(smtp\.)?office365\.com)$", 587, "smtp.office365.com", SecureMode::StartTls, &[]),
  provider("Yahoo Mail", r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>yahoo\.(ca|com))$", 465, "smtp.mail.yahoo.com", SecureMode::Ssl, EMAIL_ONLY),
  provider("GMX Mail", r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>gmx\.(com|net|de|at|ch|fr))$", 587, "mail.gmx.com", SecureMode::StartTls, EMAIL_ONLY),
  provider(
    "Fast Mail",
    r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>fastmail\.(com|cn|co\.uk|com\.au|de|es|fm|fr|im|in|jp|mx|net|nl|org|se|to|tw|uk|us))$",
    465,
    "smtp.fastmail.com",
    SecureMode::Ssl,
    EMAIL_ONLY,
  ),
  provider(
    "Fast Mail Extended Addresses",
    concat!(
      r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@",
      r"(?P<domain>123mail\.org|airpost\.net|eml\.cc|fmail\.co\.uk|",
      r"fmgirl\.com|fmguy\.com|mailbolt\.com|mailcan\.com|",
      r"mailhaven\.com|mailmight\.com|ml1\.net|mm\.st|myfastmail\.com|",
      r"proinbox\.com|promessage\.com|rushpost\.com|sent\.(as|at|com)|",
      r"speedymail\.org|warpmail\.net|xsmail\.com|150mail\.com|",
      r"150ml\.com|16mail\.com|2-mail\.com|4email\.net|50mail\.com|",
      r"allmail\.net|bestmail\.us|cluemail\.com|elitemail\.org|",
      r"emailcorner\.net|emailengine\.(net|org)|emailgroups\.net|",
      r"emailplus\.org|emailuser\.net|f-m\.fm|fast-email\.com|",
      r"fast-mail\.org|fastem\.com|fastemail\.us|fastemailer\.com|",
      r"fastest\.cc|fastimap\.com|fastmailbox\.net|fastmessaging\.com|",
      r"fea\.st|fmailbox\.com|ftml\.net|h-mail\.us|hailmail\.net|",
      r"imap-mail\.com|imap\.cc|imapmail\.org|inoutbox\.com|",
      r"internet-e-mail\.com|internet-mail\.org|internetemails\.net|",
      r"internetmailing\.net|jetemail\.net|justemail\.net|",
      r"letterboxes\.org|mail-central\.com|mail-page\.com|",
      r"mailandftp\.com|mailas\.com|mailc\.net|mailforce\.net|",
      r"mailftp\.com|mailingaddress\.org|mailite\.com|mailnew\.com|",
      r"mailsent\.net|mailservice\.ms|mailup\.net|mailworks\.org|",
      r"mymacmail\.com|nospammail\.net|ownmail\.net|petml\.com|",
      r"postinbox\.com|postpro\.net|realemail\.net|reallyfast\.biz|",
      r"reallyfast\.info|speedpost\.net|ssl-mail\.com|swift-mail\.com|",
      r"the-fastest\.net|the-quickest\.com|theinternetemail\.com|",
      r"veryfast\.biz|veryspeedy\.net|yepmail\.net)$",
    ),
    465,
    "smtp.fastmail.com",
    SecureMode::Ssl,
    EMAIL_ONLY,
  ),
  provider("Zoho Mail", r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>zoho(mail)?\.com)$", 587, "smtp.zoho.com", SecureMode::StartTls, EMAIL_ONLY),
  // An authenticated sender (from=) and a recipient (to=) are required
  provider(
    "SendGrid",
    r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>(\.smtp)?sendgrid\.(com|net))$",
    465,
    "smtp.sendgrid.net",
    SecureMode::Ssl,
    USERID_ONLY,
  ),
  provider("163.com", r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>163\.com)$", 465, "smtp.163.com", SecureMode::Ssl, EMAIL_ONLY),
  provider("Foxmail.com", r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>(foxmail|qq)\.com)$", 587, "smtp.qq.com", SecureMode::StartTls, EMAIL_ONLY),
  provider("Comcast.net", r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>(comcast)\.net)$", 465, "smtp.comcast.net", SecureMode::Ssl, EMAIL_ONLY),
  EmailTemplate {
    name: "Local Mail Server",
    pattern: r"^(((?P<label>[^+]+)\+)?(?P<id>[^@]+)@)?(?P<domain>localhost(\.localdomain)?)$",
    port: None,
    smtp_host: None,
    secure: None,
    secure_mode: None,
    login_type: &[],
    // A default sender when the URL has no user
    from_user: Some("root"),
  },
  // Catch-all: the URL's host is the SMTP server
  EmailTemplate {
    name: "Custom",
    pattern: r"^((?P<label>[^+]+)\+)?(?P<id>[^@]+)@(?P<domain>.+)$",
    port: None,
    smtp_host: None,
    secure: None,
    secure_mode: None,
    login_type: &[],
    from_user: None,
  },
];

static COMPILED: LazyLock<Vec<Regex>> =
  LazyLock::new(|| EMAIL_TEMPLATES.iter().map(|t| Regex::new(&format!("(?i){}", t.pattern)).expect("email template compiles")).collect());

/// The first template matching `address`, with the matched `id` group.
pub(crate) fn find_template(address: &str) -> Option<(&'static EmailTemplate, Option<String>)> {
  EMAIL_TEMPLATES.iter().zip(COMPILED.iter()).find_map(|(template, re)| {
    tracing::trace!("Scanning {} against {}", address, template.name);
    re.captures(address).map(|caps| (template, caps.name("id").map(|m| m.as_str().to_string())))
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_every_template_compiles() {
    assert_eq!(COMPILED.len(), EMAIL_TEMPLATES.len());
  }

  #[test]
  fn test_template_lookup() {
    let name = |a: &str| find_template(a).map(|(t, _)| t.name);
    assert_eq!(name("user@gmail.com"), Some("Google Mail"));
    assert_eq!(name("USER@GMAIL.COM"), Some("Google Mail"));
    assert_eq!(name("label+user@gmail.com"), Some("Google Mail"));
    assert_eq!(name("user@yahoo.com"), Some("Yahoo Mail"));
    assert_eq!(name("user@yahoo.co.uk"), Some("Custom"));
    assert_eq!(name("user@live.com.au"), Some("Microsoft Hotmail"));
    assert_eq!(name("user@smtp.outlook.com"), Some("Microsoft Outlook"));
    assert_eq!(name("user@sent.at"), Some("Fast Mail Extended Addresses"));
    assert_eq!(name("user@gmx.ch"), Some("GMX Mail"));
    assert_eq!(name("localhost"), Some("Local Mail Server"));
    assert_eq!(name("user@localhost.localdomain"), Some("Local Mail Server"));
    assert_eq!(name("user@example.com"), Some("Custom"));
    assert_eq!(name("example.com"), None);

    let (_, id) = find_template("label+user@gmail.com").unwrap();
    assert_eq!(id.as_deref(), Some("user"));
  }
}
