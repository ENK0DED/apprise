//! In-memory attachments (upstream `AttachMemory`).

use super::UNKNOWN_MIMETYPE;
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
}
