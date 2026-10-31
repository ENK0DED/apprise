use thiserror::Error;

#[derive(Debug, Error)]
pub enum NotifyError {
  #[error("HTTP request failed: {0}")]
  Http(#[from] reqwest::Error),

  #[error("JSON serialization error: {0}")]
  Json(#[from] serde_json::Error),

  #[error("URL parse error: {0}")]
  Url(#[from] url::ParseError),

  #[error("I/O error: {0}")]
  Io(#[from] std::io::Error),

  #[error("Invalid URL format: {0}")]
  InvalidUrl(String),

  #[error("Missing required parameter: {0}")]
  MissingParam(String),

  #[error("Invalid parameter value: {0}")]
  InvalidParam(String),

  #[error("Service returned error: {status} - {body}")]
  ServiceError { status: u16, body: String },

  #[error("Email error: {0}")]
  Email(String),

  #[error("MQTT error: {0}")]
  Mqtt(String),

  #[error("Authentication error: {0}")]
  Auth(String),

  #[error("Not supported on this platform")]
  NotSupported,

  #[error("{0}")]
  Other(String),
}

#[derive(Debug, Error)]
pub enum ConfigError {
  #[error("I/O error reading config: {0}")]
  Io(#[from] std::io::Error),

  #[error("HTTP error fetching config: {0}")]
  Http(#[from] reqwest::Error),

  #[error("Invalid config format: {0}")]
  InvalidFormat(String),

  #[error("Max recursion depth exceeded")]
  RecursionDepth,

  #[error("{0}")]
  Other(String),
}

#[derive(Debug, Error)]
pub enum AttachError {
  #[error("I/O error: {0}")]
  Io(#[from] std::io::Error),

  #[error("HTTP error: {0}")]
  Http(#[from] reqwest::Error),

  /// The attachment could not be found or fetched; the message is safe to
  /// log (secrets masked).
  #[error("File not found: {0}")]
  NotFound(String),

  /// The attachment exceeds the size limit (in bytes).
  #[error("Content exceeds allowable maximum file length ({}KB): {source_url}", limit / 1024)]
  TooLarge { source_url: String, limit: u64 },

  /// The attachment URL uses a schema no attachment type handles.
  #[error("Unsupported schema {0}.")]
  UnsupportedSchema(String),

  /// `?mime=` names an unknown MIME type.
  #[error("An invalid mime-type ({0}) was specified.")]
  InvalidMime(String),

  /// The attachment location is not allowed from where Apprise runs.
  #[error("Attachment was disallowed due to accessibility restrictions: {0}")]
  Disallowed(String),

  #[error("{0}")]
  Other(String),
}

/// A reqwest error as text, with the request URL (which may hold tokens)
/// masked.
pub fn loggable_http_error(err: &reqwest::Error) -> String {
  let text = err.to_string();
  let text = match err.url() {
    Some(url) => text.replace(url.as_str(), &crate::utils::cwe312::cwe312_loggable(url.as_str(), true)),
    None => text,
  };
  crate::utils::cwe312::cwe312_text(&text)
}
