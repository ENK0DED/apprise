use crate::error::AttachError;
use crate::notify::Attachment;

pub async fn load_from_http(url: &str) -> Result<Attachment, AttachError> {
  crate::ensure_crypto_provider();
  let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| AttachError::Other(e.to_string()))?;
  let resp = client.get(url).send().await.map_err(|e| AttachError::Other(e.to_string()))?;
  let content_type = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream").to_string();
  let data = resp.bytes().await.map_err(|e| AttachError::Other(e.to_string()))?.to_vec();
  // Derive filename from URL
  let name = url.split('/').next_back().unwrap_or("attachment").split('?').next().unwrap_or("attachment").to_string();
  Ok(Attachment { name, data, mime_type: content_type })
}
