//! Local file configuration (upstream `apprise/config/file.py`).

use std::path::Path;
use std::sync::OnceLock;

use regex_lite::Regex;

use super::{ConfigFormat, MAX_BUFFER_SIZE};
use crate::error::ConfigError;

/// Read a configuration file. Returns its content and, when the name ends
/// in `.yml`/`.yaml`, the YAML format.
pub async fn read_file(path: &Path, encoding: &str) -> Result<(String, Option<ConfigFormat>), ConfigError> {
  let meta = tokio::fs::metadata(path).await.map_err(|_| ConfigError::Other(format!("File is not accessible: {}", path.display())))?;
  if meta.len() as usize > MAX_BUFFER_SIZE {
    return Err(ConfigError::Other(format!("File size exceeds maximum allowable buffer length ({}KB).", MAX_BUFFER_SIZE / 1024)));
  }

  let bytes = tokio::fs::read(path).await.map_err(|_| ConfigError::Other(format!("File can not be opened for read: {}", path.display())))?;
  let content = decode(&bytes, encoding).ok_or_else(|| ConfigError::Other(format!("File not using expected encoding ({}) : {}", encoding, path.display())))?;

  Ok((content, yaml_by_name(&path.to_string_lossy()).then_some(ConfigFormat::Yaml)))
}

/// Upstream `^.*\.ya?ml\s*$` on the file name.
fn yaml_by_name(name: &str) -> bool {
  static RE: OnceLock<Regex> = OnceLock::new();
  RE.get_or_init(|| Regex::new(r"(?i)^.*\.ya?ml\s*$").unwrap()).is_match(name)
}

/// Decode bytes with the `?encoding=` a source names. UTF-8 (the default),
/// ASCII and Latin-1 are understood.
pub(crate) fn decode(bytes: &[u8], encoding: &str) -> Option<String> {
  match encoding.trim().to_lowercase().replace('_', "-").as_str() {
    "utf-8" | "utf8" | "u8" => String::from_utf8(bytes.to_vec()).ok(),
    "ascii" | "us-ascii" => bytes.is_ascii().then(|| String::from_utf8(bytes.to_vec()).unwrap()),
    "latin-1" | "latin1" | "iso-8859-1" | "iso8859-1" | "l1" => Some(bytes.iter().map(|b| *b as char).collect()),
    other => {
      tracing::warn!("Unsupported configuration encoding {}", other);
      None
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_yaml_by_name() {
    assert!(yaml_by_name("/etc/apprise.yml"));
    assert!(yaml_by_name("/etc/apprise.YAML"));
    assert!(!yaml_by_name("/etc/apprise.conf"));
    assert!(!yaml_by_name("/etc/apprise.yml.bak"));
  }

  #[test]
  fn test_decode() {
    assert_eq!(decode("é".as_bytes(), "utf-8").as_deref(), Some("é"));
    assert_eq!(decode(&[0xe9], "latin-1").as_deref(), Some("é"));
    assert!(decode(&[0xe9], "utf-8").is_none());
    assert!(decode(b"x", "klingon").is_none());
  }

  #[tokio::test]
  async fn test_read_file_limits() {
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.conf");
    std::fs::write(&big, "x".repeat(MAX_BUFFER_SIZE + 1)).unwrap();
    assert!(read_file(&big, "utf-8").await.is_err());
    assert!(read_file(&dir.path().join("missing.conf"), "utf-8").await.is_err());

    let yml = dir.path().join("a.yml");
    std::fs::write(&yml, "urls: []").unwrap();
    assert_eq!(read_file(&yml, "utf-8").await.unwrap().1, Some(ConfigFormat::Yaml));
  }
}
