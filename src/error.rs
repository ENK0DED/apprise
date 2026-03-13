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

    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

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

    #[error("File not found: {0}")]
    NotFound(String),

    #[error("{0}")]
    Other(String),
}
