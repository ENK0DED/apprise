//! Reading key, credential and subscription files the way upstream plugins
//! read them through `AppriseAttachment`: from a local path, a `file://`
//! URL or an `http(s)://` URL, with a size limit.

use base64::Engine;
use std::path::PathBuf;

use crate::asset::AppriseAsset;

/// Read `source` for an Apprise using `asset`, refusing anything larger
/// than `max_size` bytes. A URL without `?redirect=` follows the asset's
/// `http_redirects`, as upstream's attachment does. Returns the content
/// and, for a local file, its path.
pub async fn read_source(source: &str, max_size: usize, asset: &AppriseAsset) -> Option<(Vec<u8>, Option<PathBuf>)> {
  if source.starts_with("http://") || source.starts_with("https://") {
    // The limit applies while downloading, not after.
    let mut http = crate::attachment::http::HttpSource::from_url(source).ok()?;
    http.max_file_size = max_size as u64;
    let attachment = match http.load_for(asset).await {
      Ok(attachment) => attachment,
      Err(crate::error::AttachError::TooLarge { .. }) => {
        tracing::warn!("Remote file exceeds the {} byte limit", max_size);
        return None;
      }
      Err(_) => return None,
    };
    return Some((attachment.data, None));
  }
  let path = PathBuf::from(source.strip_prefix("file://").unwrap_or(source));
  let size = tokio::fs::metadata(&path).await.ok()?.len();
  if size > max_size as u64 {
    tracing::warn!("File exceeds the {} byte limit", max_size);
    return None;
  }
  let data = tokio::fs::read(&path).await.ok()?;
  Some((data, Some(path)))
}

/// Every PEM block in `data` as (label, DER bytes); undecodable blocks and
/// RFC 1421 header lines are skipped.
pub fn pem_blocks(data: &str) -> Vec<(String, Vec<u8>)> {
  let mut blocks = Vec::new();
  let mut current: Option<(String, String)> = None;
  for line in data.lines().map(str::trim) {
    if let Some(label) = line.strip_prefix("-----BEGIN ").and_then(|l| l.strip_suffix("-----")) {
      current = Some((label.to_string(), String::new()));
    } else if line.starts_with("-----END ") {
      if let Some((label, body)) = current.take()
        && let Ok(der) = base64::engine::general_purpose::STANDARD.decode(body)
      {
        blocks.push((label, der));
      }
    } else if let Some((_, body)) = current.as_mut()
      && !line.contains(':')
    {
      body.push_str(line);
    }
  }
  blocks
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_pem_blocks() {
    let pem = "junk\n-----BEGIN EC PARAMETERS-----\nBggqhkjOPQMBBw==\n-----END EC PARAMETERS-----\n-----BEGIN PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nAAEC\nAw==\n-----END PRIVATE KEY-----\n";
    let blocks = pem_blocks(pem);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].0, "EC PARAMETERS");
    assert_eq!(blocks[1], ("PRIVATE KEY".to_string(), vec![0, 1, 2, 3]));
    assert!(pem_blocks("-----BEGIN X-----\n!!\n-----END X-----").is_empty());
  }

  #[tokio::test]
  async fn test_read_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.json");
    std::fs::write(&path, b"12345").unwrap();
    let p = path.to_string_lossy().into_owned();
    let asset = AppriseAsset::default();
    assert_eq!(read_source(&p, 5, &asset).await.unwrap(), (b"12345".to_vec(), Some(path.clone())));
    assert_eq!(read_source(&format!("file://{}", p), 5, &asset).await.unwrap().0, b"12345");
    assert!(read_source(&p, 4, &asset).await.is_none());
    assert!(read_source("/invalid/path", 5, &asset).await.is_none());
  }
}
