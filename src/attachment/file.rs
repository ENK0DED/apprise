use crate::error::AttachError;
use crate::notify::Attachment;
use std::path::Path;
use tokio::fs;

pub async fn load_from_file(path: &str) -> Result<Attachment, AttachError> {
    let data = fs::read(path).await.map_err(|e| AttachError::Other(e.to_string()))?;
    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("attachment").to_string();
    let mime_type = mime_guess::from_path(path).first_or_octet_stream().to_string();
    Ok(Attachment { name, data, mime_type })
}
