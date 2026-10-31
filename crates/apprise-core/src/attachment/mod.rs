//! Attachments (upstream `apprise/attachment/` and `apprise_attachment.py`).
//!
//! A source is a local path, a `file://` URL, an `http(s)://` URL or bytes
//! in memory ([`memory::MemorySource`]; a `memory://` URL names one with no
//! content). URLs accept `?name=` (the file name services see; directories
//! are dropped) and `?mime=` (a known MIME type). HTTP sources also accept
//! `+Header=value` (and `-Header=value`) request headers, `user:pass@`
//! basic auth, `?verify=`, `?redirect=`, `?cto=` and `?rto=`; other query
//! arguments are passed on to the server.
//!
//! Loading happens in two steps, as upstream's instantiate/download split:
//! [`parse_attachment`] rejects sources that can never work (an unknown
//! schema, an invalid `?mime=`), and [`AttachSource::load`] reads the
//! content, which may fail for that one attachment (missing file, HTTP
//! error, too large).
//!
//! Each source has its own size limit (upstream `max_file_size`),
//! [`MAX_FILE_SIZE`] unless changed with [`AttachSource::set_max_file_size`];
//! `0` disables it. Upstream's plugins lower it for their templates and key
//! files.

pub mod file;
pub mod http;
pub mod memory;

use std::collections::HashMap;

use crate::error::AttachError;
use crate::notify::Attachment;
use crate::utils::cwe312::cwe312_loggable;

/// Largest attachment accepted by default, in bytes (upstream
/// `AttachBase.max_file_size`).
pub const MAX_FILE_SIZE: u64 = 1_048_576_000;

/// Whether `size` bytes break the limit `limit` (`0`: no limit).
pub(crate) fn exceeds(size: u64, limit: u64) -> bool {
  limit > 0 && size > limit
}

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
  Memory(memory::MemorySource),
}

impl AttachSource {
  /// Read the attachment's content, refusing more than
  /// [`max_file_size`](Self::max_file_size) before reading past it.
  pub async fn load(&self) -> Result<Attachment, AttachError> {
    match self {
      Self::File(f) => f.load().await,
      Self::Http(h) => h.load().await,
      Self::Memory(m) => m.load(),
    }
  }

  /// The most this source may hold, in bytes; `0` means no limit.
  pub fn max_file_size(&self) -> u64 {
    match self {
      Self::File(f) => f.max_file_size,
      Self::Http(h) => h.max_file_size,
      Self::Memory(m) => m.max_file_size,
    }
  }

  /// Change this source's limit (upstream sets `max_file_size` on one
  /// attachment, as its plugins do for templates and key files). `0`
  /// disables the check.
  pub fn set_max_file_size(&mut self, limit: u64) {
    match self {
      Self::File(f) => f.max_file_size = limit,
      Self::Http(h) => h.max_file_size = limit,
      Self::Memory(m) => m.max_file_size = limit,
    }
  }

  /// [`set_max_file_size`](Self::set_max_file_size), returning the source.
  pub fn with_max_file_size(mut self, limit: u64) -> Self {
    self.set_max_file_size(limit);
    self
  }

  /// The source with secrets masked, for logs.
  pub fn loggable(&self) -> &str {
    match self {
      Self::File(f) => &f.display,
      Self::Http(h) => &h.display,
      Self::Memory(m) => &m.display,
    }
  }

  fn location(&self) -> ContentLocation {
    match self {
      Self::File(_) | Self::Memory(_) => ContentLocation::Local,
      Self::Http(_) => ContentLocation::Hosted,
    }
  }

  /// Refuse this source for an Apprise running at `location`: a hosted
  /// instance only accepts web content, and an inaccessible one nothing
  /// (upstream `AppriseAttachment.add()`).
  pub fn allowed_at(&self, location: ContentLocation) -> Result<(), AttachError> {
    let loggable = || cwe312_loggable(self.loggable(), true);
    if location == ContentLocation::Inaccessible {
      tracing::warn!("Attachments are disabled; ignoring {}", loggable());
      return Err(AttachError::Disallowed(loggable()));
    }
    if location == ContentLocation::Hosted && self.location() != ContentLocation::Hosted {
      tracing::warn!("Attachment was disallowed due to accessibility restrictions (hosted->local): {}", loggable());
      return Err(AttachError::Disallowed(loggable()));
    }
    Ok(())
  }
}

impl From<memory::MemorySource> for AttachSource {
  fn from(source: memory::MemorySource) -> Self {
    Self::Memory(source)
  }
}

/// Parse an attachment source; see the module docs for the forms accepted.
pub fn parse_attachment(source: &str) -> Result<AttachSource, AttachError> {
  parse_attachment_from(source, ContentLocation::Local)
}

/// [`parse_attachment`] for an Apprise running at `location`: a hosted
/// instance only accepts web content, and an inaccessible one nothing.
pub fn parse_attachment_from(source: &str, location: ContentLocation) -> Result<AttachSource, AttachError> {
  if location == ContentLocation::Inaccessible {
    let loggable = cwe312_loggable(source, true);
    tracing::warn!("Attachments are disabled; ignoring {}", loggable);
    return Err(AttachError::Disallowed(loggable));
  }
  let parsed = match schema_of(source) {
    None => AttachSource::File(file::FileSource::from_path(source)),
    Some(schema) => match schema.as_str() {
      "file" => AttachSource::File(file::FileSource::from_url(source)?),
      "http" | "https" => AttachSource::Http(http::HttpSource::from_url(source)?),
      "memory" => AttachSource::Memory(memory::MemorySource::from_url(source)?),
      other => {
        tracing::warn!("Unsupported schema {}.", other);
        return Err(AttachError::UnsupportedSchema(other.to_string()));
      }
    },
  };
  parsed.allowed_at(location)?;
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
    .map(safe_name)
    .filter(|n| !n.is_empty())
    .unwrap_or_else(|| format!("{}{}", UNKNOWN_FILENAME, guess_extension(mime).unwrap_or_else(|| ".obj".to_string())))
}

/// `name` without control characters. Names end up in MIME and multipart
/// headers, where a CR/LF (for instance from `%0d%0a` in an attachment URL)
/// would inject headers; upstream's email package refuses such names.
pub fn safe_name(name: &str) -> String {
  name.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests;
