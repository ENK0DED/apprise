//! Local file attachments (upstream `AttachFile`).

use std::collections::HashMap;
use std::path::PathBuf;

use super::{CommonOptions, MAX_FILE_SIZE, UNKNOWN_MIMETYPE, basename, final_name, guess_mime, parse_query};
use crate::error::AttachError;
use crate::notify::Attachment;
use crate::utils::disk::path_decode;

/// A file on the local disk.
#[derive(Debug, Clone)]
pub struct FileSource {
  pub path: PathBuf,
  pub name: Option<String>,
  pub mime: Option<String>,
  /// The source as given, for logs.
  pub display: String,
}

impl FileSource {
  /// A plain path. `?` and anything after it are part of the file name.
  pub fn from_path(path: &str) -> Self {
    Self { path: path_decode(path), name: None, mime: None, display: path.to_string() }
  }

  /// A `file://path[?name=...&mime=...]` URL.
  pub fn from_url(url: &str) -> Result<Self, AttachError> {
    let rest = &url.trim_start()["file://".len()..];
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    if path.is_empty() {
      return Err(AttachError::Other(format!("Unparseable URL {url}.")));
    }
    let path = urlencoding::decode(path).map(|p| p.into_owned()).unwrap_or_else(|_| path.to_string());
    let qsd: HashMap<String, String> = parse_query(query).into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect();
    let opts = CommonOptions::from_query(&qsd)?;
    Ok(Self { path: path_decode(&path), name: opts.name, mime: opts.mime, display: url.to_string() })
  }

  /// Read the file.
  pub async fn load(&self) -> Result<Attachment, AttachError> {
    self.load_limited(MAX_FILE_SIZE).await
  }

  /// [`load`](Self::load), refusing files over `limit` bytes (or
  /// [`MAX_FILE_SIZE`], if smaller) before reading them.
  pub async fn load_limited(&self, limit: u64) -> Result<Attachment, AttachError> {
    let limit = limit.min(MAX_FILE_SIZE);
    let meta = tokio::fs::metadata(&self.path).await.ok().filter(|m| m.is_file()).ok_or_else(|| AttachError::NotFound(self.display.clone()))?;
    if meta.len() > limit {
      tracing::error!("Content exceeds allowable maximum file length ({}KB): {}", limit / 1024, self.display);
      return Err(AttachError::TooLarge { source_url: self.display.clone(), limit });
    }
    let data = tokio::fs::read(&self.path).await.map_err(|_| AttachError::NotFound(self.display.clone()))?;
    let detected = self.path.file_name().map(|n| n.to_string_lossy().into_owned());
    let name = final_name(self.name.as_deref(), detected.as_deref().map(basename), UNKNOWN_MIMETYPE);
    let mime_type = self.mime.clone().or_else(|| guess_mime(&name)).unwrap_or_else(|| UNKNOWN_MIMETYPE.to_string());
    Ok(Attachment { name, data, mime_type })
  }
}
