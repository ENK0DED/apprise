//! Per-recipient message preparation (upstream `NotifyEmail.prepare_emails`).

use std::sync::LazyLock;

use regex_lite::Regex;

use super::mime::Part;
use super::{FromName, PgpMode};
use crate::notify::Attachment;
use crate::types::NotifyFormat;
use crate::utils::email::formataddr;
#[cfg(feature = "pgp")]
use crate::utils::pgp::PgpController;
use crate::utils::time::Timezone;

/// `cid:` references in an HTML body.
static CID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"cid:([^\s"'>)]+)"#).expect("cid pattern compiles"));

/// Why a message could not be prepared (upstream `AppriseEmailException`).
#[derive(Debug)]
pub(crate) struct PrepareError(pub String);

/// One prepared message.
pub(crate) struct EmailMessage {
  /// The `To` address, used to track delivery.
  pub recipient: String,
  /// Every envelope recipient: To, then Cc, then Bcc.
  pub to_addrs: Vec<String>,
  pub body: String,
}

/// Everything a notification's messages share.
pub(crate) struct MessageSpec<'a> {
  pub subject: &'a str,
  pub body: &'a str,
  pub format: NotifyFormat,
  pub from_name: &'a FromName,
  pub from_addr: &'a str,
  pub cc: &'a [String],
  pub bcc: &'a [String],
  pub reply_to: &'a [String],
  /// Domain of the Message-ID.
  pub smtp_host: &'a str,
  pub attachments: &'a [Attachment],
  /// Extra headers, in order (X-Application, the URL's `+` headers, then
  /// the protocol's own).
  pub headers: &'a [(String, String)],
  /// Display names by address.
  pub names: &'a std::collections::HashMap<String, FromName>,
  #[cfg(feature = "pgp")]
  pub pgp: Option<&'a PgpController>,
  pub pgp_mode: PgpMode,
  pub inline: bool,
  pub tz: Option<Timezone>,
  pub app_id: &'a str,
}

impl FromName {
  fn resolve<'a>(&'a self, app_id: &'a str) -> Option<&'a str> {
    match self {
      FromName::AppId => Some(app_id),
      FromName::Name(name) => Some(name.as_str()),
      FromName::None => None,
    }
  }
}

/// Python's `formataddr()` raises on a non-ASCII address.
fn address(name: Option<&str>, addr: &str) -> Result<String, PrepareError> {
  if !addr.is_ascii() {
    return Err(PrepareError(format!("Non-ASCII email address: {}", addr)));
  }
  Ok(formataddr(name, addr))
}

fn attachment_name(attachment: &Attachment, no: usize) -> String {
  if attachment.name.is_empty() { format!("file{:03}.dat", no) } else { attachment.name.clone() }
}

fn attachment_mime(attachment: &Attachment) -> &str {
  if attachment.mime_type.matches('/').count() == 1 { attachment.mime_type.as_str() } else { crate::attachment::UNKNOWN_MIMETYPE }
}

fn is_image(attachment: &Attachment) -> bool {
  attachment.mime_type.to_lowercase().starts_with("image/")
}

/// `email.utils.format_datetime()`: `Fri, 30 Oct 2026 06:01:00 -0400`.
fn format_date(tz: Option<Timezone>) -> String {
  const FORMAT: &str = "%a, %d %b %Y %H:%M:%S %z";
  match tz {
    Some(Timezone::Utc) => chrono::Utc::now().with_timezone(&chrono::FixedOffset::east_opt(0).expect("zero offset")).format(FORMAT).to_string(),
    Some(Timezone::Fixed(offset)) => chrono::Utc::now().with_timezone(&offset).format(FORMAT).to_string(),
    None => chrono::Local::now().format(FORMAT).to_string(),
  }
}

/// `email.utils.make_msgid(domain=...)`.
fn make_msgid(domain: &str) -> String {
  let centis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() / 10).unwrap_or_default();
  format!("<{}.{}.{}@{}>", centis, std::process::id(), rand::random::<u64>(), domain)
}

/// Build the message for one `To` recipient.
pub(crate) async fn prepare_email(spec: &MessageSpec<'_>, to_name: Option<&str>, to_addr: &str) -> Result<EmailMessage, PrepareError> {
  #[cfg(not(feature = "pgp"))]
  if spec.pgp_mode != PgpMode::None {
    let msg = "PGP Support unavailable; build Apprise with the pgp feature";
    tracing::warn!("{}", msg);
    return Err(PrepareError(msg.to_string()));
  }

  let cc: Vec<&String> = spec.cc.iter().filter(|a| !spec.bcc.contains(a) && a.as_str() != to_addr).collect();
  let bcc: Vec<&String> = spec.bcc.iter().filter(|a| a.as_str() != to_addr).collect();
  let reply_to: Vec<&String> = spec.reply_to.iter().filter(|a| a.as_str() != to_addr).collect();
  let named = |addr: &str| address(spec.names.get(addr).and_then(|n| n.resolve(spec.app_id)), addr);
  let cc_hdr = cc.iter().map(|a| named(a)).collect::<Result<Vec<_>, _>>()?;
  let bcc_hdr = bcc.iter().map(|a| named(a)).collect::<Result<Vec<_>, _>>()?;
  let reply_hdr = reply_to.iter().map(|a| named(a)).collect::<Result<Vec<_>, _>>()?;
  let from_hdr = address(spec.from_name.resolve(spec.app_id), spec.from_addr)?;

  tracing::debug!("Email From: {}", from_hdr);
  tracing::debug!("Email To: {}", to_addr);
  if !cc_hdr.is_empty() {
    tracing::debug!("Email Cc: {}", cc_hdr.join(", "));
  }
  if !bcc_hdr.is_empty() {
    tracing::debug!("Email Bcc: {}", bcc_hdr.join(", "));
  }
  if !reply_hdr.is_empty() {
    tracing::debug!("Email Reply-To: {}", reply_hdr.join(", "));
  }

  // Inline attachments (RFC 2387): HTML gains <img> references for images
  // it does not already name; plain text gets an [Image: name] marker
  let mut body = spec.body.to_string();
  let mut cid_refs: Vec<String> = Vec::new();
  if spec.inline && !spec.attachments.is_empty() {
    let names: Vec<String> = spec.attachments.iter().enumerate().map(|(i, a)| attachment_name(a, i + 1)).collect();
    if spec.format == NotifyFormat::Html {
      // A %20 in the reference matches a space in the file name
      let mut refs: Vec<String> = CID_RE.captures_iter(&body).map(|c| c[1].replace("%20", " ")).collect();
      refs.sort();
      refs.dedup();
      for missing in refs.iter().filter(|r| !names.contains(r)) {
        tracing::warn!("Email inline: no attachment matches cid:{} -- check the filename.", missing);
      }
      cid_refs = refs.into_iter().filter(|r| names.contains(r)).collect();
      let mut appended = Vec::new();
      for (attachment, name) in spec.attachments.iter().zip(&names) {
        if is_image(attachment) && !cid_refs.contains(name) {
          appended.push(name.clone());
          cid_refs.push(name.clone());
        }
      }
      for name in appended {
        body.push_str(&format!("<br/><img src=\"cid:{}\">", name.replace(' ', "%20")));
      }
    } else {
      let images: Vec<&String> = spec.attachments.iter().zip(&names).filter(|(a, _)| is_image(a)).map(|(_, n)| n).collect();
      if !images.is_empty() {
        body.push('\n');
        body.push_str(&images.iter().map(|n| format!("[Image: {}]", n)).collect::<Vec<_>>().join("\n"));
      }
    }
  }

  let mut base = if spec.format == NotifyFormat::Html {
    let mut alternative = Part::multipart("alternative", &[]);
    let text = crate::conversion::convert_between(&NotifyFormat::Html, &NotifyFormat::Text, &body);
    alternative.attach(Part::text_utf8(&text, "plain"));
    alternative.attach(Part::text_utf8(&body, "html"));
    alternative
  } else {
    Part::text_utf8(&body, "plain")
  };

  if !spec.attachments.is_empty() {
    // multipart/related when an image is shown inline, else mixed
    let mut mixed = Part::multipart(if cid_refs.is_empty() { "mixed" } else { "related" }, &[]);
    mixed.attach(base);
    for (no, attachment) in spec.attachments.iter().enumerate() {
      let filename = attachment_name(attachment, no + 1);
      let mut app = Part::application(&attachment.data);
      app.set_type(attachment_mime(attachment));
      if cid_refs.contains(&filename) {
        app.add_header("Content-Disposition", &format!("inline; filename=\"{}\"", filename));
        // The Content-ID matches the (space-encoded) cid: reference
        app.add_header("Content-ID", &format!("<{}>", filename.replace(' ', "%20")));
      } else {
        app.add_header("Content-Disposition", &format!("attachment; filename=\"{}\"", filename));
      }
      mixed.attach(app);
    }
    base = mixed;
  }

  // A custom Autocrypt header is dropped when the controller supplies one
  #[cfg(feature = "pgp")]
  let (mut base, autocrypt_added) = secure(spec, base, to_addr).await?;
  #[cfg(not(feature = "pgp"))]
  let autocrypt_added = false;

  // Header names are case-insensitive: keep the first Autocrypt only
  let mut autocrypt_seen = autocrypt_added;
  for (name, value) in spec.headers {
    if name.trim().eq_ignore_ascii_case("autocrypt") {
      if autocrypt_seen {
        continue;
      }
      autocrypt_seen = true;
    }
    base.add_header_utf8(name, value);
  }
  base.add_header_utf8("Subject", spec.subject);
  base.add_header("From", &from_hdr);
  base.add_header("To", &address(to_name, to_addr)?);
  base.add_header("Message-ID", &make_msgid(spec.smtp_host));
  base.add_header("Date", &format_date(spec.tz));
  if !cc_hdr.is_empty() {
    base.add_header("Cc", &cc_hdr.join(","));
  }
  if !reply_hdr.is_empty() {
    base.add_header("Reply-To", &reply_hdr.join(","));
  }

  let mut to_addrs = vec![to_addr.to_string()];
  to_addrs.extend(cc.into_iter().cloned());
  to_addrs.extend(bcc.into_iter().cloned());
  Ok(EmailMessage { recipient: to_addr.to_string(), to_addrs, body: base.as_string() })
}

/// Sign or encrypt `base` for `to_addr` as the PGP mode asks; also whether
/// an Autocrypt header was added.
#[cfg(feature = "pgp")]
async fn secure(spec: &MessageSpec<'_>, mut base: Part, to_addr: &str) -> Result<(Part, bool), PrepareError> {
  let mut autocrypt_added = false;
  match (spec.pgp, spec.pgp_mode) {
    (Some(pgp), PgpMode::Sign) => {
      tracing::debug!("Securing Email with PGP Signature");
      // RFC 3156: the signature covers the CRLF form of the part
      let canonical = super::smtp::to_crlf(&base.as_string());
      let Some((signature, micalg)) = pgp.sign(canonical.as_bytes()).await else {
        let msg = "Unable to sign email via PGP; supply a private key via pgpprv= or place one in the cache directory";
        tracing::warn!("{}", msg);
        return Err(PrepareError(msg.to_string()));
      };
      let mut signed = Part::multipart("signed", &[("micalg", &micalg), ("protocol", "application/pgp-signature")]);
      signed.attach(base);
      let mut sig_part = Part::base("application/pgp-signature", &signature);
      sig_part.add_header("Content-Disposition", "attachment; filename=\"signature.asc\"");
      signed.attach(sig_part);
      base = signed;

      // Encrypt as well when the recipient already has a key; sign mode
      // still works without one
      if let Some(encrypted) = pgp.encrypt(base.as_string().as_bytes(), &[to_addr], Some(false)).await {
        tracing::debug!("PGP public key found; adding encryption");
        base = encrypted_container(&encrypted);
      }
      if let Some(autocrypt) = pgp.autocrypt_header().await {
        base.add_header("Autocrypt", &autocrypt);
        autocrypt_added = true;
      }
    }
    (Some(pgp), PgpMode::Encrypt) => {
      tracing::debug!("Securing Email with PGP Encryption");
      // These travel inside the encrypted part
      base.add_header("From", &address(None, spec.from_addr)?);
      base.add_header("To", &address(None, to_addr)?);
      base.add_header_utf8("Subject", spec.subject);
      // Only a self-send may generate the (sender's) key
      let autogen = if to_addr.eq_ignore_ascii_case(spec.from_addr) { None } else { Some(false) };
      let Some(encrypted) = pgp.encrypt(base.as_string().as_bytes(), &[to_addr], autogen).await else {
        let msg = "Unable to encrypt email via PGP; supply a public key via pgppub= or place one in the cache directory";
        tracing::warn!("{}", msg);
        return Err(PrepareError(msg.to_string()));
      };
      let mut container = Part::multipart("encrypted", &[("protocol", "application/pgp-encrypted")]);
      if let Some(autocrypt) = pgp.autocrypt_header().await {
        container.add_header("Autocrypt", &autocrypt);
        autocrypt_added = true;
      }
      container.attach(version_part());
      container.attach(Part::base("application/octet-stream", &encrypted));
      base = container;
    }
    _ => {}
  }
  Ok((base, autocrypt_added))
}

/// The RFC 3156 version part: `MIMEText("Version: 1")` retyped.
#[cfg(feature = "pgp")]
fn version_part() -> Part {
  let mut part = Part::text_ascii("Version: 1", "plain");
  part.set_type("application/pgp-encrypted");
  part
}

/// A `multipart/encrypted` container around armored ciphertext.
#[cfg(feature = "pgp")]
fn encrypted_container(encrypted: &str) -> Part {
  let mut container = Part::multipart("encrypted", &[("protocol", "application/pgp-encrypted")]);
  container.attach(version_part());
  container.attach(Part::base("application/octet-stream", encrypted));
  container
}
