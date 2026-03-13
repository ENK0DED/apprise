use std::collections::HashMap;
use url::Url;

/// A parsed Apprise URL with all components accessible
#[derive(Debug, Clone)]
pub struct ParsedUrl {
    pub schema: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Full path (without leading slash)
    pub path: String,
    /// Individual path components
    pub path_parts: Vec<String>,
    /// Query string as key→value map
    pub qsd: HashMap<String, String>,
    /// Original raw URL string
    pub raw: String,
}

impl ParsedUrl {
    /// Parse an Apprise-style URL
    pub fn parse(raw: &str) -> Option<Self> {
        // Handle URLs that may not have //: e.g., "growl://host"
        let normalized = if raw.contains("://") {
            raw.to_string()
        } else {
            return None;
        };

        let url = Url::parse(&normalized).ok()?;

        let schema = url.scheme().to_lowercase();
        let host = url.host_str().map(|h| h.to_string());
        let port = url.port();

        let user = if url.username().is_empty() {
            None
        } else {
            Some(urlencoding::decode(url.username()).unwrap_or_default().into_owned())
        };
        let password = url
            .password()
            .map(|p| urlencoding::decode(p).unwrap_or_default().into_owned());

        let path = url.path().trim_start_matches('/').to_string();
        let path_parts: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| urlencoding::decode(s).unwrap_or_default().into_owned())
            .collect();

        let mut qsd = HashMap::new();
        for (key, value) in url.query_pairs() {
            qsd.insert(key.into_owned(), value.into_owned());
        }

        Some(ParsedUrl {
            schema,
            user,
            password,
            host,
            port,
            path,
            path_parts,
            qsd,
            raw: raw.to_string(),
        })
    }

    /// Get query string value by key (case-insensitive)
    pub fn get(&self, key: &str) -> Option<&str> {
        // Try exact match first
        if let Some(v) = self.qsd.get(key) {
            return Some(v.as_str());
        }
        // Case-insensitive fallback
        let lower = key.to_lowercase();
        self.qsd
            .iter()
            .find(|(k, _)| k.to_lowercase() == lower)
            .map(|(_, v)| v.as_str())
    }

    /// Get the host, or return a default
    pub fn host_or<'a>(&'a self, default: &'a str) -> &'a str {
        self.host.as_deref().unwrap_or(default)
    }

    /// Build http(s) URL from parts
    pub fn base_url(&self, secure: bool) -> String {
        let schema = if secure { "https" } else { "http" };
        match (&self.host, self.port) {
            (Some(h), Some(p)) => format!("{}://{}:{}", schema, h, p),
            (Some(h), None) => format!("{}://{}", schema, h),
            _ => format!("{}://localhost", schema),
        }
    }

    /// Determine if this URL uses a secure variant (ends with 's' convention)
    pub fn is_secure(&self) -> bool {
        self.schema.ends_with('s')
    }

    /// Parse tags from qsd "tags" or "tag" key (comma-separated)
    pub fn tags(&self) -> Vec<String> {
        self.get("tags")
            .or_else(|| self.get("tag"))
            .map(|t| {
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get "verify" ssl flag (default true)
    pub fn verify_certificate(&self) -> bool {
        self.get("verify")
            .map(|v| !matches!(v.to_lowercase().as_str(), "no" | "false" | "0"))
            .unwrap_or(true)
    }
}

/// Try to parse multiple space/newline-separated URLs from a string
pub fn extract_urls(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                vec![]
            } else {
                // Remove inline comments
                let without_comment = trimmed
                    .find(" #")
                    .or_else(|| trimmed.find(" ;"))
                    .map(|i| &trimmed[..i])
                    .unwrap_or(trimmed);
                without_comment
                    .split_whitespace()
                    .filter(|s| s.contains("://"))
                    .map(|s| s.to_string())
                    .collect()
            }
        })
        .collect()
}

/// Parse a "bool-like" query string value
pub fn parse_bool(s: &str) -> bool {
    matches!(s.to_lowercase().as_str(), "yes" | "true" | "1" | "on" | "enable" | "enabled")
}

/// Parse targets from path parts + optional query key
pub fn parse_targets(path_parts: &[String], qsd: &HashMap<String, String>) -> Vec<String> {
    let mut targets: Vec<String> = path_parts.to_vec();
    if let Some(t) = qsd.get("to") {
        targets.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    targets
}

/// Normalize phone number (strip non-digit characters except leading +)
pub fn normalize_phone(phone: &str) -> String {
    let phone = phone.trim();
    if phone.starts_with('+') {
        format!("+{}", phone[1..].chars().filter(|c| c.is_ascii_digit()).collect::<String>())
    } else {
        phone.chars().filter(|c| c.is_ascii_digit()).collect()
    }
}
