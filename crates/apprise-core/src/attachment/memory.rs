//! In-memory attachments (upstream `AttachMemory`).

use std::collections::HashMap;

use super::{CommonOptions, MAX_FILE_SIZE, UNKNOWN_MIMETYPE, basename, exceeds, parse_query};
use crate::error::AttachError;
use crate::notify::Attachment;

/// An attachment from bytes already in memory. An empty name becomes
/// `<uuid>.dat` and an empty MIME type `application/octet-stream`.
pub fn from_memory(name: &str, data: Vec<u8>, mime_type: &str) -> Attachment {
  let name = if name.trim().is_empty() { format!("{}.dat", uuid4()) } else { super::basename(name.trim()).to_string() };
  let mime_type = if mime_type.trim().is_empty() { UNKNOWN_MIMETYPE.to_string() } else { mime_type.trim().to_string() };
  Attachment { name, data, mime_type }
}

/// A text attachment: UTF-8 bytes, `text/plain`, named `<uuid>.txt`.
pub fn from_text(content: &str) -> Attachment {
  Attachment { name: format!("{}.txt", uuid4()), data: content.as_bytes().to_vec(), mime_type: "text/plain".to_string() }
}

/// Bytes in memory as an attachment source (upstream `AttachMemory`), so
/// they can carry their own size limit like file and web sources. As
/// upstream, a memory attachment with no content, or with more than its
/// limit, cannot be attached.
#[derive(Debug, Clone)]
pub struct MemorySource {
  pub attachment: Attachment,
  /// The most it may hold, in bytes (upstream `max_file_size`); `0`
  /// disables the check.
  pub max_file_size: u64,
  /// The source for logs, as upstream shows it:
  /// `memory://{name}?mime={mime}`.
  pub display: String,
}

impl MemorySource {
  /// `data` named `name` (see [`from_memory`] for the defaults).
  pub fn new(name: &str, data: Vec<u8>, mime_type: &str) -> Self {
    Self::from_attachment(from_memory(name, data, mime_type))
  }

  /// UTF-8 text (see [`from_text`]).
  pub fn text(content: &str) -> Self {
    Self::from_attachment(from_text(content))
  }

  fn from_attachment(attachment: Attachment) -> Self {
    let display = loggable(&attachment);
    Self { attachment, max_file_size: MAX_FILE_SIZE, display }
  }

  /// A `memory://name[?name=...&mime=...]` URL. It holds no content, so it
  /// never loads; upstream accepts the URL all the same, and only the
  /// services that take attachments fail.
  pub fn from_url(url: &str) -> Result<Self, AttachError> {
    let rest = &url.trim_start()["memory://".len()..];
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let qsd: HashMap<String, String> = parse_query(query).into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
    let opts = CommonOptions::from_query(&qsd)?;
    let path = urlencoding::decode(path).map(|p| p.into_owned()).unwrap_or_else(|_| path.to_string());
    let name = opts.name.unwrap_or_else(|| basename(&path).to_string());
    Ok(Self::new(&name, Vec::new(), opts.mime.as_deref().unwrap_or_default()))
  }

  /// The attachment, unless it is empty or over its limit.
  pub fn load(&self) -> Result<Attachment, AttachError> {
    if self.attachment.data.is_empty() {
      return Err(AttachError::NotFound(self.display.clone()));
    }
    if exceeds(self.attachment.data.len() as u64, self.max_file_size) {
      tracing::error!("Content exceeds allowable maximum memory size ({}KB): {}", self.max_file_size / 1024, self.display);
      return Err(AttachError::TooLarge { source_url: self.display.clone(), limit: self.max_file_size });
    }
    Ok(self.attachment.clone())
  }
}

/// How upstream shows a memory attachment: `memory://{name}?mime={mime}`.
pub(crate) fn loggable(attachment: &Attachment) -> String {
  format!("memory://{}?mime={}", urlencoding::encode(&attachment.name), urlencoding::encode(&attachment.mime_type).replace("%2F", "/"))
}

/// A random (version 4) UUID.
fn uuid4() -> String {
  let mut b: [u8; 16] = rand::random();
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  let h = hex::encode(b);
  format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_from_memory_basic() {
    let data = b"hello world".to_vec();
    let att = from_memory("test.txt", data.clone(), "text/plain");
    assert_eq!(att.name, "test.txt");
    assert_eq!(att.data, data);
    assert_eq!(att.mime_type, "text/plain");
  }

  #[test]
  fn test_from_memory_binary() {
    let data = vec![0x89, 0x50, 0x4E, 0x47]; // PNG magic bytes
    let att = from_memory("image.png", data.clone(), "image/png");
    assert_eq!(att.name, "image.png");
    assert_eq!(att.data, data);
    assert_eq!(att.mime_type, "image/png");
  }

  #[test]
  fn test_from_memory_defaults() {
    let att = from_memory("", Vec::new(), "");
    assert!(att.name.ends_with(".dat"), "{}", att.name);
    assert_eq!(att.name.len(), 36 + 4);
    assert!(att.data.is_empty());
    assert_eq!(att.mime_type, "application/octet-stream");
    assert_eq!(from_memory("../x/y.bin", vec![1], "").name, "y.bin");
  }

  #[test]
  fn test_from_text() {
    let att = from_text("hello");
    assert!(att.name.ends_with(".txt"));
    assert_eq!(att.data, b"hello");
    assert_eq!(att.mime_type, "text/plain");
  }

  // Upstream AttachMemory.exists(): content, within max_file_size (0 = any)
  #[test]
  fn test_memory_source_limits() {
    let mut source = MemorySource::new("big.bin", vec![b'x'; 2048], "");
    assert_eq!(source.display, "memory://big.bin?mime=application/octet-stream");
    assert_eq!(source.max_file_size, MAX_FILE_SIZE);
    assert_eq!(source.load().unwrap().data.len(), 2048);
    source.max_file_size = 1024;
    assert!(matches!(source.load(), Err(AttachError::TooLarge { limit: 1024, .. })));
    source.max_file_size = 2048;
    assert!(source.load().is_ok());
    source.max_file_size = 0;
    assert!(source.load().is_ok());
    assert!(
      matches!(MemorySource::new("empty.bin", Vec::new(), "").load(), Err(AttachError::NotFound(s)) if s == "memory://empty.bin?mime=application/octet-stream")
    );
    assert_eq!(MemorySource::text("hi").load().unwrap().mime_type, "text/plain");
  }

  #[test]
  fn test_memory_url() {
    let source = MemorySource::from_url("memory://dir/x%20y.txt?mime=text/plain").unwrap();
    assert_eq!((source.attachment.name.as_str(), source.display.as_str()), ("x y.txt", "memory://x%20y.txt?mime=text/plain"));
    assert!(source.load().is_err());
    assert_eq!(MemorySource::from_url("memory://a.bin?name=b.txt").unwrap().attachment.name, "b.txt");
    assert!(matches!(MemorySource::from_url("memory://a?mime=bogus/x"), Err(AttachError::InvalidMime(_))));
  }
}
