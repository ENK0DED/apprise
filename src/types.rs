use serde::{Deserialize, Serialize};
use std::fmt;

/// Notification type / severity
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyType {
    Info,
    Success,
    Warning,
    Failure,
}

impl NotifyType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Failure => "failure",
        }
    }

    /// Return a color hex code (for services that support color)
    pub fn color(&self) -> u32 {
        match self {
            Self::Info => 0x3498DB,
            Self::Success => 0x2ECC71,
            Self::Warning => 0xE67E22,
            Self::Failure => 0xE74C3C,
        }
    }

    /// Return color as hex string e.g. "#3498DB"
    pub fn color_hex(&self) -> String {
        format!("#{:06X}", self.color())
    }

    pub fn all() -> &'static [&'static str] {
        &["info", "success", "warning", "failure"]
    }
}

impl fmt::Display for NotifyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for NotifyType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "info" => Ok(Self::Info),
            "success" => Ok(Self::Success),
            "warning" | "warn" => Ok(Self::Warning),
            "failure" | "fail" | "error" => Ok(Self::Failure),
            other => Err(format!("Unknown notification type: {}", other)),
        }
    }
}

impl Default for NotifyType {
    fn default() -> Self {
        Self::Info
    }
}

/// Message body format
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyFormat {
    Text,
    Html,
    Markdown,
}

impl NotifyFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Html => "html",
            Self::Markdown => "markdown",
        }
    }

    pub fn all() -> &'static [&'static str] {
        &["text", "html", "markdown"]
    }
}

impl fmt::Display for NotifyFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for NotifyFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "html" => Ok(Self::Html),
            "markdown" | "md" => Ok(Self::Markdown),
            other => Err(format!("Unknown format: {}", other)),
        }
    }
}

impl Default for NotifyFormat {
    fn default() -> Self {
        Self::Text
    }
}

/// Storage operation mode
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageMode {
    Auto,
    Flush,
    Memory,
}

impl std::str::FromStr for StorageMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "flush" => Ok(Self::Flush),
            "memory" => Ok(Self::Memory),
            other => Err(format!("Unknown storage mode: {}", other)),
        }
    }
}

impl Default for StorageMode {
    fn default() -> Self {
        Self::Auto
    }
}

impl fmt::Display for StorageMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Flush => write!(f, "flush"),
            Self::Memory => write!(f, "memory"),
        }
    }
}
