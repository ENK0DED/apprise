use crate::error::AttachError;
use crate::notify::Attachment;
use std::path::Path;
use tokio::fs;

fn guess_mime(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png"          => "image/png",
        "gif"          => "image/gif",
        "webp"         => "image/webp",
        "svg"          => "image/svg+xml",
        "pdf"          => "application/pdf",
        "txt"          => "text/plain",
        "html" | "htm" => "text/html",
        "json"         => "application/json",
        "xml"          => "application/xml",
        "zip"          => "application/zip",
        "mp4"          => "video/mp4",
        "mp3"          => "audio/mpeg",
        _              => "application/octet-stream",
    }
}

pub async fn load_from_file(path: &str) -> Result<Attachment, AttachError> {
    let data = fs::read(path).await.map_err(|e| AttachError::Other(e.to_string()))?;
    let name = Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or("attachment").to_string();
    let mime_type = guess_mime(path).to_string();
    Ok(Attachment { name, data, mime_type })
}
