//! Attachments (upstream `apprise/attachment/` and `apprise_attachment.py`).
//!
//! A source is a local path, a `file://` URL or an `http(s)://` URL. URLs
//! accept `?name=` (the file name services see; directories are dropped)
//! and `?mime=` (a known MIME type). HTTP sources also accept `+Header=value`
//! (and `-Header=value`) request headers, `user:pass@` basic auth,
//! `?verify=`, `?redirect=`, `?cto=` and `?rto=`; other query arguments are
//! passed on to the server.
//!
//! Loading happens in two steps, as upstream's instantiate/download split:
//! [`parse_attachment`] rejects sources that can never work (an unknown
//! schema, an invalid `?mime=`), and [`AttachSource::load`] reads the
//! content, which may fail for that one attachment (missing file, HTTP
//! error, too large).

pub mod file;
pub mod http;
pub mod memory;

use std::collections::HashMap;

use crate::error::AttachError;
use crate::notify::Attachment;
use crate::utils::cwe312::cwe312_loggable;

/// Largest attachment accepted, in bytes (upstream `max_file_size`).
pub const MAX_FILE_SIZE: u64 = 1_048_576_000;

/// MIME type used when none can be detected.
pub const UNKNOWN_MIMETYPE: &str = "application/octet-stream";

/// Base file name used when the source does not provide one.
pub const UNKNOWN_FILENAME: &str = "apprise-attachment";

/// Where Apprise runs, which limits where attachments may come from
/// (upstream `ContentLocation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentLocation {
  /// Local use: files and web content are both allowed.
  #[default]
  Local,
  /// A hosted service: only web (`http(s)://`) content is allowed.
  Hosted,
  /// Attachments are disabled.
  Inaccessible,
}

/// A parsed attachment source, ready to load.
#[derive(Debug, Clone)]
pub enum AttachSource {
  File(file::FileSource),
  Http(http::HttpSource),
}

impl AttachSource {
  /// Read the attachment's content.
  pub async fn load(&self) -> Result<Attachment, AttachError> {
    match self {
      Self::File(f) => f.load().await,
      Self::Http(h) => h.load().await,
    }
  }

  /// The source with secrets masked, for logs.
  pub fn loggable(&self) -> &str {
    match self {
      Self::File(f) => &f.display,
      Self::Http(h) => &h.display,
    }
  }

  fn location(&self) -> ContentLocation {
    match self {
      Self::File(_) => ContentLocation::Local,
      Self::Http(_) => ContentLocation::Hosted,
    }
  }
}

/// Parse an attachment source; see the module docs for the forms accepted.
pub fn parse_attachment(source: &str) -> Result<AttachSource, AttachError> {
  parse_attachment_from(source, ContentLocation::Local)
}

/// [`parse_attachment`] for an Apprise running at `location`: a hosted
/// instance only accepts web content, and an inaccessible one nothing.
pub fn parse_attachment_from(source: &str, location: ContentLocation) -> Result<AttachSource, AttachError> {
  let loggable = cwe312_loggable(source, true);
  if location == ContentLocation::Inaccessible {
    tracing::warn!("Attachments are disabled; ignoring {}", loggable);
    return Err(AttachError::Disallowed(loggable));
  }
  let parsed = match schema_of(source) {
    None => AttachSource::File(file::FileSource::from_path(source)),
    Some(schema) => match schema.as_str() {
      "file" => AttachSource::File(file::FileSource::from_url(source)?),
      "http" | "https" => AttachSource::Http(http::HttpSource::from_url(source)?),
      other => {
        tracing::warn!("Unsupported schema {}.", other);
        return Err(AttachError::UnsupportedSchema(other.to_string()));
      }
    },
  };
  if location == ContentLocation::Hosted && parsed.location() != ContentLocation::Hosted {
    tracing::warn!("Attachment was disallowed due to accessibility restrictions (hosted->local): {}", loggable);
    return Err(AttachError::Disallowed(loggable));
  }
  Ok(parsed)
}

/// Load an attachment from a path or URL.
pub async fn load_attachment(source: &str) -> Result<Attachment, AttachError> {
  parse_attachment(source)?.load().await
}

/// The lowercase schema of `value` (upstream `GET_SCHEMA_RE`), if it has one.
pub fn schema_of(value: &str) -> Option<String> {
  let trimmed = value.trim_start();
  let (schema, _) = trimmed.split_once("://")?;
  (!schema.is_empty() && schema.len() <= 32 && schema.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| schema.to_lowercase())
}

/// `?name=` and `?mime=`, common to every attachment URL.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CommonOptions {
  pub name: Option<String>,
  pub mime: Option<String>,
}

impl CommonOptions {
  pub fn from_query(qsd: &HashMap<String, String>) -> Result<Self, AttachError> {
    // Only the file name is kept, so `?name=../../etc/passwd` is `passwd`.
    let name = qsd.get("name").map(|n| basename(n).trim().to_string()).filter(|n| !n.is_empty());
    let mime = qsd.get("mime").map(|m| m.trim().to_lowercase());
    if let Some(m) = &mime
      && !is_known_mime(m)
    {
      tracing::warn!("An invalid mime-type ({}) was specified.", m);
      return Err(AttachError::InvalidMime(m.clone()));
    }
    Ok(Self { name, mime })
  }
}

/// Split a query string into decoded key/value pairs (last one wins).
pub(crate) fn parse_query(query: &str) -> Vec<(String, String)> {
  query
    .split('&')
    .filter(|p| !p.is_empty())
    .map(|pair| {
      let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
      let decode = |s: &str| urlencoding::decode(&s.replace('+', "%2B")).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string());
      (decode(k), decode(v))
    })
    .collect()
}

/// The last path component of `path`, for either separator.
pub(crate) fn basename(path: &str) -> &str {
  path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Whether `mime` is a MIME type Apprise knows.
pub fn is_known_mime(mime: &str) -> bool {
  mime_guess::get_mime_extensions_str(mime).is_some()
}

/// The MIME type of a file name, if its extension is known.
pub fn guess_mime(name: &str) -> Option<String> {
  mime_guess::from_path(name).first_raw().map(str::to_string)
}

/// The usual extension (with the dot) for `mime`, if known.
pub fn guess_extension(mime: &str) -> Option<String> {
  let essence = mime.split(';').next().unwrap_or(mime).trim().to_lowercase();
  // Prefer the extension Python's mimetypes picks for common types.
  let preferred = match essence.as_str() {
    "application/octet-stream" => Some("bin"),
    "image/jpeg" => Some("jpg"),
    "text/plain" => Some("txt"),
    "text/html" => Some("html"),
    "audio/mpeg" => Some("mp3"),
    "video/mp4" => Some("mp4"),
    "image/tiff" => Some("tiff"),
    "text/markdown" => Some("md"),
    "application/xml" | "text/xml" => Some("xml"),
    _ => None,
  };
  preferred.or_else(|| mime_guess::get_mime_extensions_str(&essence).and_then(|e| e.first().copied())).map(|e| format!(".{e}"))
}

/// The name a service sees: the forced name, else the detected one, else
/// `apprise-attachment` with an extension for the MIME type.
pub(crate) fn final_name(forced: Option<&str>, detected: Option<&str>, mime: &str) -> String {
  forced
    .or(detected)
    .filter(|n| !n.is_empty())
    .map(str::to_string)
    .unwrap_or_else(|| format!("{}{}", UNKNOWN_FILENAME, guess_extension(mime).unwrap_or_else(|| ".obj".to_string())))
}

#[cfg(test)]
mod tests;
