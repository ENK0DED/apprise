#[cfg(not(target_arch = "wasm32"))]
pub mod file;
pub mod http;
pub mod memory;

use crate::error::AttachError;
use crate::notify::Attachment;

/// Load an attachment from a path or URL
pub async fn load_attachment(source: &str) -> Result<Attachment, AttachError> {
  if source.starts_with("http://") || source.starts_with("https://") {
    http::load_from_http(source).await
  } else {
    #[cfg(not(target_arch = "wasm32"))]
    {
      file::load_from_file(source).await
    }
    #[cfg(target_arch = "wasm32")]
    {
      Err(AttachError::Other("File attachments are not supported on WASM".to_string()))
    }
  }
}
