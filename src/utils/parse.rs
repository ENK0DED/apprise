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
        if !raw.contains("://") {
            return None;
        }

        // Pre-process: encode '#' chars in query string values so the url crate
        // doesn't treat them as fragment delimiters. In Apprise URLs, '#' in
        // query values denotes channel names (e.g., ?to=#channel).
        let normalized = Self::encode_hash_in_query(raw);

        match Url::parse(&normalized) {
            Ok(url) => {
                // If the url crate interpreted path '/' chars as part of userinfo
                // (e.g., napi://a/b/c@d/e), the username will contain '/'.
                // Fall back to our manual parser which handles this correctly.
                if url.username().contains('/') {
                    Self::fallback_parse(raw)
                } else {
                    Self::from_url_crate(url, raw)
                }
            }
            Err(_) => Self::fallback_parse(raw),
        }
    }

    /// Encode special characters that appear in the URL so the `url` crate
    /// doesn't reject or misinterpret them.
    fn encode_hash_in_query(raw: &str) -> String {
        // Find the query string start
        if let Some(qmark) = raw.find('?') {
            let (before, after) = raw.split_at(qmark);
            // Also handle '#' in path parts (e.g., /path/#channel/)
            let before_encoded = before.replace('#', "%23");
            let after_encoded = after.replace('#', "%23")
                .replace('<', "%3C")
                .replace('>', "%3E");
            format!("{}{}", before_encoded, after_encoded)
        } else {
            // Also handle '#' in path parts
            raw.replace('#', "%23")
        }
    }

    /// Parse using the `url` crate result
    fn from_url_crate(url: Url, raw: &str) -> Option<Self> {
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

    /// Fallback manual parser for URLs that the `url` crate rejects
    /// (e.g., `tgram://123456789:abcdefg_hijklmnop/lead2gold/` where the
    /// authority contains a colon that isn't a valid port).
    fn fallback_parse(raw: &str) -> Option<Self> {
        let scheme_end = raw.find("://")?;
        let schema = raw[..scheme_end].to_lowercase();
        if schema.is_empty() { return None; }
        let after_scheme = &raw[scheme_end + 3..];

        // Split into authority+path and query
        let (before_query, query_str) = match after_scheme.find('?') {
            Some(i) => (&after_scheme[..i], Some(&after_scheme[i + 1..])),
            None => (after_scheme, None),
        };

        // Split authority from path at the first '/'
        let (authority, path_str) = match before_query.find('/') {
            Some(i) => (&before_query[..i], &before_query[i + 1..]),
            None => (before_query, ""),
        };

        // Split user info from host at '@'
        let (user_info, host_part) = match authority.rfind('@') {
            Some(i) => (Some(&authority[..i]), &authority[i + 1..]),
            None => (None, authority),
        };

        let (user, password) = if let Some(ui) = user_info {
            match ui.find(':') {
                Some(i) => {
                    let u = urlencoding::decode(&ui[..i]).unwrap_or_default().into_owned();
                    let p = urlencoding::decode(&ui[i + 1..]).unwrap_or_default().into_owned();
                    (if u.is_empty() { None } else { Some(u) }, Some(p))
                }
                None => {
                    let u = urlencoding::decode(ui).unwrap_or_default().into_owned();
                    (if u.is_empty() { None } else { Some(u) }, None)
                }
            }
        } else {
            (None, None)
        };

        // The host_part may contain a colon that is NOT a valid port,
        // which is exactly why we're in the fallback. Treat the entire
        // host_part as the host (including any colon).
        let host = if host_part.is_empty() {
            None
        } else {
            Some(urlencoding::decode(host_part).unwrap_or_default().into_owned())
        };

        let path = path_str.trim_end_matches('/').to_string();
        let path_parts: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| urlencoding::decode(s).unwrap_or_default().into_owned())
            .collect();

        let mut qsd = HashMap::new();
        if let Some(qs) = query_str {
            for pair in qs.split('&') {
                if let Some(eq) = pair.find('=') {
                    let key = urlencoding::decode(&pair[..eq]).unwrap_or_default().into_owned();
                    let value = urlencoding::decode(&pair[eq + 1..]).unwrap_or_default().into_owned();
                    qsd.insert(key, value);
                } else if !pair.is_empty() {
                    let key = urlencoding::decode(pair).unwrap_or_default().into_owned();
                    qsd.insert(key, String::new());
                }
            }
        }

        Some(ParsedUrl {
            schema,
            user,
            password,
            host,
            port: None,
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
/// Parse a boolean value from a string. Matches the same values as Python apprise:
/// Truthy: yes, y, true, t, on, 1, enable, enabled, active, al, en, tr, ye
/// Everything else is false.
pub fn parse_bool(s: &str) -> bool {
    let lower = s.to_lowercase();
    matches!(lower.as_str(),
        "yes" | "y" | "true" | "t" | "on" | "1" | "enable" | "enabled" | "active"
        | "al" | "en" | "tr" | "ye"
    )
}

/// Parse targets from path parts + optional query key
pub fn parse_targets(path_parts: &[String], qsd: &HashMap<String, String>) -> Vec<String> {
    let mut targets: Vec<String> = path_parts.to_vec();
    if let Some(t) = qsd.get("to") {
        targets.extend(t.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }
    targets
}

/// Mask sensitive parts of a URL for safe logging (CWE-312 compliant).
/// Replaces passwords with `***` and masks query params containing
/// "key", "token", "secret", or "password" in their names.
pub fn mask_url(raw: &str) -> String {
    // Apprise URLs use custom schemes; prepend http:// so url::Url can parse
    let (prefix, parse_input) = if raw.contains("://") {
        let scheme_end = raw.find("://").unwrap();
        let scheme = &raw[..scheme_end];
        // url::Url only accepts known schemes; swap temporarily
        (Some(scheme.to_string()), format!("http://{}", &raw[scheme_end + 3..]))
    } else {
        (None, raw.to_string())
    };

    let Ok(mut url) = Url::parse(&parse_input) else { return raw.to_string() };

    if url.password().is_some() {
        let _ = url.set_password(Some("***"));
    }

    let sensitive = ["key", "token", "secret", "password", "pass", "apikey"];
    let pairs: Vec<(String, String)> = url.query_pairs()
        .map(|(k, v)| {
            let kl = k.to_lowercase();
            if sensitive.iter().any(|s| kl.contains(s)) {
                (k.into_owned(), "***".to_string())
            } else {
                (k.into_owned(), v.into_owned())
            }
        })
        .collect();

    if !pairs.is_empty() {
        url.set_query(None);
        { let mut qs = url.query_pairs_mut(); for (k, v) in &pairs { qs.append_pair(k, v); } }
    }

    let result = url.to_string();
    // Restore original scheme
    if let Some(scheme) = prefix {
        format!("{}://{}", scheme, &result["http://".len()..])
    } else {
        result
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── ParsedUrl basic tests ──────────────────────────────────────

    #[test]
    fn test_parse_basic_http() {
        let p = ParsedUrl::parse("http://hostname").unwrap();
        assert_eq!(p.schema, "http");
        assert_eq!(p.host.as_deref(), Some("hostname"));
        assert_eq!(p.port, None);
        assert_eq!(p.user, None);
        assert_eq!(p.password, None);
        assert!(p.qsd.is_empty());
    }

    #[test]
    fn test_parse_https() {
        let p = ParsedUrl::parse("https://example.com").unwrap();
        assert_eq!(p.schema, "https");
        assert_eq!(p.host.as_deref(), Some("example.com"));
        assert!(p.is_secure());
    }

    #[test]
    fn test_parse_with_port() {
        let p = ParsedUrl::parse("http://hostname:8080").unwrap();
        assert_eq!(p.host.as_deref(), Some("hostname"));
        assert_eq!(p.port, Some(8080));
    }

    #[test]
    fn test_parse_max_port() {
        let p = ParsedUrl::parse("http://hostname:65535").unwrap();
        assert_eq!(p.port, Some(65535));
    }

    #[test]
    fn test_parse_with_user_password() {
        let p = ParsedUrl::parse("http://user:pass@hostname").unwrap();
        assert_eq!(p.user.as_deref(), Some("user"));
        assert_eq!(p.password.as_deref(), Some("pass"));
        assert_eq!(p.host.as_deref(), Some("hostname"));
    }

    #[test]
    fn test_parse_encoded_user_password() {
        let p = ParsedUrl::parse("http://us%40er:p%40ss@hostname").unwrap();
        assert_eq!(p.user.as_deref(), Some("us@er"));
        assert_eq!(p.password.as_deref(), Some("p@ss"));
    }

    #[test]
    fn test_parse_with_path() {
        let p = ParsedUrl::parse("http://hostname/path/to/resource").unwrap();
        assert_eq!(p.path, "path/to/resource");
        assert_eq!(p.path_parts, vec!["path", "to", "resource"]);
    }

    #[test]
    fn test_parse_trailing_slashes() {
        let p = ParsedUrl::parse("http://hostname////").unwrap();
        assert_eq!(p.host.as_deref(), Some("hostname"));
        // path_parts should be empty since all components are empty after filtering
        assert!(p.path_parts.is_empty());
    }

    #[test]
    fn test_parse_with_port_and_path() {
        let p = ParsedUrl::parse("http://hostname:40/some/path").unwrap();
        assert_eq!(p.port, Some(40));
        assert_eq!(p.path_parts, vec!["some", "path"]);
    }

    // ── Query string tests ─────────────────────────────────────────

    #[test]
    fn test_parse_query_string() {
        let p = ParsedUrl::parse("http://hostname/path?key=value&foo=bar").unwrap();
        assert_eq!(p.qsd.get("key").map(|s| s.as_str()), Some("value"));
        assert_eq!(p.qsd.get("foo").map(|s| s.as_str()), Some("bar"));
    }

    #[test]
    fn test_get_case_insensitive() {
        let p = ParsedUrl::parse("http://host?MyKey=val").unwrap();
        assert_eq!(p.get("MyKey"), Some("val"));
        assert_eq!(p.get("mykey"), Some("val"));
        assert_eq!(p.get("MYKEY"), Some("val"));
    }

    #[test]
    fn test_tags_from_qsd() {
        let p = ParsedUrl::parse("http://host?tag=a,b,c").unwrap();
        assert_eq!(p.tags(), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_tags_from_qsd_tags() {
        let p = ParsedUrl::parse("http://host?tags=x,y").unwrap();
        assert_eq!(p.tags(), vec!["x", "y"]);
    }

    #[test]
    fn test_tags_empty() {
        let p = ParsedUrl::parse("http://host").unwrap();
        assert!(p.tags().is_empty());
    }

    #[test]
    fn test_verify_certificate_default() {
        let p = ParsedUrl::parse("http://host").unwrap();
        assert!(p.verify_certificate());
    }

    #[test]
    fn test_verify_certificate_false() {
        let p = ParsedUrl::parse("http://host?verify=no").unwrap();
        assert!(!p.verify_certificate());

        let p = ParsedUrl::parse("http://host?verify=false").unwrap();
        assert!(!p.verify_certificate());

        let p = ParsedUrl::parse("http://host?verify=0").unwrap();
        assert!(!p.verify_certificate());
    }

    // ── Special characters ─────────────────────────────────────────

    #[test]
    fn test_parse_ipv6_host() {
        let p = ParsedUrl::parse("http://[2001:db8::1]:8080/path").unwrap();
        assert_eq!(p.host.as_deref(), Some("[2001:db8::1]"));
        assert_eq!(p.port, Some(8080));
    }

    #[test]
    fn test_parse_devtunnel_host() {
        let p = ParsedUrl::parse("http://5t4m59hl-34343.euw.devtunnels.ms").unwrap();
        assert_eq!(p.host.as_deref(), Some("5t4m59hl-34343.euw.devtunnels.ms"));
    }

    #[test]
    fn test_parse_custom_schema() {
        let p = ParsedUrl::parse("slack://token_a/token_b/token_c").unwrap();
        assert_eq!(p.schema, "slack");
    }

    #[test]
    fn test_parse_none_for_invalid() {
        assert!(ParsedUrl::parse("").is_none());
        assert!(ParsedUrl::parse("notaurl").is_none());
        assert!(ParsedUrl::parse("://missing_schema").is_none());
    }

    // ── base_url / is_secure / host_or ─────────────────────────────

    #[test]
    fn test_base_url() {
        let p = ParsedUrl::parse("slack://host:443/path").unwrap();
        assert_eq!(p.base_url(true), "https://host:443");
        assert_eq!(p.base_url(false), "http://host:443");
    }

    #[test]
    fn test_host_or_default() {
        let p = ParsedUrl::parse("http://myhost").unwrap();
        assert_eq!(p.host_or("fallback"), "myhost");
    }

    // ── parse_bool ─────────────────────────────────────────────────

    #[test]
    fn test_parse_bool_truthy() {
        for val in &["yes", "Yes", "YES", "y", "Y", "true", "True", "TRUE",
                     "t", "T", "on", "ON", "1", "enable", "enabled", "active"] {
            assert!(parse_bool(val), "Expected true for '{}'", val);
        }
    }

    #[test]
    fn test_parse_bool_falsy() {
        for val in &["no", "No", "NO", "false", "False", "0", "off", "disable",
                     "disabled", "inactive", "f", "n", "never", "NEVER",
                     "OhYeah", "random", ""] {
            assert!(!parse_bool(val), "Expected false for '{}'", val);
        }
    }

    #[test]
    fn test_parse_bool_partial_matches() {
        // These partial prefixes are recognized as truthy
        assert!(parse_bool("al"));  // active-like
        assert!(parse_bool("en"));  // enable-like
        assert!(parse_bool("tr"));  // true-like
        assert!(parse_bool("ye"));  // yes-like
    }

    // ── extract_urls ───────────────────────────────────────────────

    #[test]
    fn test_extract_urls_basic() {
        let urls = extract_urls("json://localhost\nxml://localhost");
        assert_eq!(urls, vec!["json://localhost", "xml://localhost"]);
    }

    #[test]
    fn test_extract_urls_with_comments() {
        let text = "# This is a comment\njson://localhost\n; another comment\nxml://localhost";
        let urls = extract_urls(text);
        assert_eq!(urls, vec!["json://localhost", "xml://localhost"]);
    }

    #[test]
    fn test_extract_urls_inline_comments() {
        let text = "json://localhost # my json service";
        let urls = extract_urls(text);
        assert_eq!(urls, vec!["json://localhost"]);
    }

    #[test]
    fn test_extract_urls_inline_semicolon_comments() {
        let text = "json://localhost ; my json service";
        let urls = extract_urls(text);
        assert_eq!(urls, vec!["json://localhost"]);
    }

    #[test]
    fn test_extract_urls_empty_lines() {
        let text = "\n\njson://localhost\n\n\nxml://localhost\n\n";
        let urls = extract_urls(text);
        assert_eq!(urls, vec!["json://localhost", "xml://localhost"]);
    }

    #[test]
    fn test_extract_urls_whitespace() {
        let text = "   json://localhost   ";
        let urls = extract_urls(text);
        assert_eq!(urls, vec!["json://localhost"]);
    }

    #[test]
    fn test_extract_urls_multiple_on_one_line() {
        let text = "json://host1 xml://host2";
        let urls = extract_urls(text);
        assert_eq!(urls, vec!["json://host1", "xml://host2"]);
    }

    #[test]
    fn test_extract_urls_no_urls() {
        let text = "# just a comment\nnot a url\n";
        let urls = extract_urls(text);
        assert!(urls.is_empty());
    }

    #[test]
    fn test_extract_urls_empty() {
        let urls = extract_urls("");
        assert!(urls.is_empty());
    }

    // ── normalize_phone ────────────────────────────────────────────

    #[test]
    fn test_normalize_phone_plain() {
        assert_eq!(normalize_phone("1234567890"), "1234567890");
    }

    #[test]
    fn test_normalize_phone_with_plus() {
        assert_eq!(normalize_phone("+1 (234) 567-8901"), "+12345678901");
    }

    #[test]
    fn test_normalize_phone_no_plus() {
        assert_eq!(normalize_phone("(234) 567-8901"), "2345678901");
    }

    #[test]
    fn test_normalize_phone_whitespace() {
        assert_eq!(normalize_phone("  +44 123 456  "), "+44123456");
    }

    #[test]
    fn test_normalize_phone_dashes_dots() {
        assert_eq!(normalize_phone("123-456.7890"), "1234567890");
    }

    // ── mask_url ───────────────────────────────────────────────────

    #[test]
    fn test_mask_url_password() {
        let masked = mask_url("http://user:secret@host/path");
        assert!(masked.contains("***"), "password should be masked");
        assert!(!masked.contains("secret"), "raw password should not appear");
    }

    #[test]
    fn test_mask_url_no_password() {
        let masked = mask_url("http://host/path");
        assert_eq!(masked, "http://host/path");
    }

    #[test]
    fn test_mask_url_sensitive_query_params() {
        let masked = mask_url("http://host/path?apikey=ABC123&normal=ok");
        assert!(!masked.contains("ABC123"), "apikey value should be masked");
        assert!(masked.contains("normal=ok"), "non-sensitive params preserved");
    }

    #[test]
    fn test_mask_url_token_query_param() {
        let masked = mask_url("http://host?token=mytoken");
        assert!(!masked.contains("mytoken"), "token should be masked");
    }

    #[test]
    fn test_mask_url_secret_query_param() {
        let masked = mask_url("http://host?secret=xyz");
        assert!(!masked.contains("xyz"), "secret should be masked");
    }

    #[test]
    fn test_mask_url_password_query_param() {
        let masked = mask_url("http://host?password=hunter2");
        assert!(!masked.contains("hunter2"), "password param should be masked");
    }

    #[test]
    fn test_mask_url_custom_scheme() {
        let masked = mask_url("slack://user:pass@host/path");
        assert!(masked.starts_with("slack://"), "scheme should be preserved");
        assert!(!masked.contains("pass"), "password should be masked in custom scheme");
    }

    #[test]
    fn test_mask_url_invalid() {
        // Invalid URL should be returned as-is
        let masked = mask_url("not a url at all");
        assert_eq!(masked, "not a url at all");
    }

    // ── parse_targets ──────────────────────────────────────────────

    #[test]
    fn test_parse_targets_from_path() {
        let parts = vec!["a".to_string(), "b".to_string()];
        let qsd = HashMap::new();
        let targets = parse_targets(&parts, &qsd);
        assert_eq!(targets, vec!["a", "b"]);
    }

    #[test]
    fn test_parse_targets_with_to_param() {
        let parts = vec!["a".to_string()];
        let mut qsd = HashMap::new();
        qsd.insert("to".to_string(), "c,d".to_string());
        let targets = parse_targets(&parts, &qsd);
        assert_eq!(targets, vec!["a", "c", "d"]);
    }

    #[test]
    fn test_parse_targets_empty() {
        let parts: Vec<String> = vec![];
        let qsd = HashMap::new();
        let targets = parse_targets(&parts, &qsd);
        assert!(targets.is_empty());
    }
}
