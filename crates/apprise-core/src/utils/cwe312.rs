//! CWE-312 secure logging utilities.
//!
//! Masks sensitive information (passwords, tokens, API keys) in URLs and text
//! before they're written to logs, preventing cleartext storage of sensitive data.
//!
//! See: https://cwe.mitre.org/data/definitions/312.html

/// Mask a word that may contain sensitive information.
///
/// Matching Python's `cwe312_word()`:
/// - If `force` is true, always masks (first char...last char)
/// - If the word is not a hostname and > 1 char, masks it
/// - If the word is >= 16 chars, masks it (likely a token/key)
/// - If `advanced` is true, checks character class variance (mixed case + digits + special)
///   and masks if obscurity threshold is reached
pub fn cwe312_word(word: &str, force: bool, advanced: bool) -> String {
    let trimmed = word.trim();
    if trimmed.is_empty() {
        return trimmed.to_string();
    }

    if force {
        return mask_word(trimmed);
    }

    // If > 1 char and doesn't look like a hostname, mask it
    if trimmed.len() > 1 && !looks_like_hostname(trimmed) {
        return mask_word(trimmed);
    }

    // Very long words are likely tokens/passwords
    if trimmed.len() >= 16 {
        return mask_word(trimmed);
    }

    if advanced {
        // Check character class variance — mixed types suggest a password/token
        let mut last_class = None;
        let mut obscurity = 0u32;
        let threshold = 5;

        for c in trimmed.chars() {
            let class = if c.is_ascii_digit() {
                'n'
            } else if c.is_ascii_uppercase() {
                '+'
            } else if c.is_ascii_lowercase() {
                '-'
            } else {
                's' // special
            };

            if last_class != Some(class) || class == 's' {
                obscurity += 1;
                if obscurity >= threshold {
                    return mask_word(trimmed);
                }
            }
            last_class = Some(class);
        }
    }

    trimmed.to_string()
}

/// Mask a URL for secure logging, matching Python's `cwe312_url()`.
///
/// Masks passwords, path segments that look like tokens, and sensitive
/// query parameters (password, secret, pass, token, key, id, apikey, to).
pub fn cwe312_url(url: &str) -> String {
    // Try to parse with our URL parser
    let Some(parsed) = crate::utils::parse::ParsedUrl::parse(url) else {
        return url.to_string();
    };

    let is_http = parsed.schema.starts_with("http");

    // Mask password
    let password = parsed.password.as_deref()
        .map(|p| cwe312_word(p, true, true))
        .unwrap_or_default();

    // Mask user (less aggressively for HTTP)
    let user = parsed.user.as_deref()
        .map(|u| cwe312_word(u, false, is_http))
        .unwrap_or_default();

    // Mask host (less aggressively for HTTP)
    let host = parsed.host.as_deref()
        .map(|h| cwe312_word(h, false, !is_http))
        .unwrap_or_default();

    // Mask path segments
    let masked_path = if parsed.path.is_empty() {
        String::new()
    } else {
        let segments: Vec<String> = parsed.path.split('/')
            .map(|seg| {
                if seg.is_empty() { String::new() }
                else { cwe312_word(seg, false, true) }
            })
            .collect();
        format!("/{}", segments.join("/"))
    };

    // Build auth part
    let auth = if !user.is_empty() && !password.is_empty() {
        format!("{}:{}@", user, password)
    } else if !user.is_empty() {
        format!("{}@", user)
    } else {
        String::new()
    };

    // Mask sensitive query parameters
    let sensitive_keys = ["password", "secret", "pass", "token", "key", "id", "apikey", "to"];
    let params = if parsed.qsd.is_empty() {
        String::new()
    } else {
        let pairs: Vec<String> = parsed.qsd.iter().map(|(k, v)| {
            let masked_v = if sensitive_keys.contains(&k.to_lowercase().as_str()) {
                cwe312_word(v, true, true)
            } else {
                cwe312_word(v, false, true)
            };
            format!("{}={}", k, masked_v)
        }).collect();
        format!("?{}", pairs.join("&"))
    };

    let port = parsed.port.map(|p| format!(":{}", p)).unwrap_or_default();

    format!("{}://{}{}{}{}{}",
        parsed.schema, auth, host, port, masked_path, params)
}

fn mask_word(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = word.chars().collect();
    let first = chars.first().map(|c| c.to_string()).unwrap_or_default();
    let last = chars.last().map(|c| c.to_string()).unwrap_or_default();
    format!("{}...{}", first, last)
}

fn looks_like_hostname(s: &str) -> bool {
    // Simple check: contains dots and all segments are alphanumeric/hyphens
    if s.contains('.') {
        return s.split('.').all(|seg| {
            !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    }
    // Single word: check if it's all alphanumeric (like "localhost")
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cwe312_word_force() {
        assert_eq!(cwe312_word("mysecret", true, true), "m...t");
        assert_eq!(cwe312_word("ab", true, true), "a...b");
    }

    #[test]
    fn test_cwe312_word_hostname() {
        // Hostnames should not be masked
        assert_eq!(cwe312_word("localhost", false, true), "localhost");
        assert_eq!(cwe312_word("example.com", false, true), "example.com");
    }

    #[test]
    fn test_cwe312_word_long_string() {
        let long = "a".repeat(20);
        let result = cwe312_word(&long, false, true);
        assert!(result.contains("..."), "Long string should be masked: {}", result);
    }

    #[test]
    fn test_cwe312_word_obscure() {
        // Mixed case + digits + special chars → high obscurity
        let result = cwe312_word("aB1$cD2!", false, true);
        assert!(result.contains("..."), "Obscure string should be masked: {}", result);
    }

    #[test]
    fn test_cwe312_word_empty() {
        assert_eq!(cwe312_word("", false, true), "");
        assert_eq!(cwe312_word("  ", false, true), "");
    }

    #[test]
    fn test_cwe312_url_masks_password() {
        let result = cwe312_url("http://user:secret@localhost/path");
        assert!(result.contains("s...t"), "Password should be masked: {}", result);
        assert!(!result.contains("secret"));
    }

    #[test]
    fn test_cwe312_url_preserves_structure() {
        let result = cwe312_url("http://user:pass@localhost:8080/path");
        assert!(result.starts_with("http://"));
        assert!(result.contains("localhost"));
        assert!(result.contains(":8080"));
    }

    #[test]
    fn test_cwe312_url_masks_token_param() {
        let result = cwe312_url("http://localhost?token=mytoken123");
        assert!(!result.contains("mytoken123"), "Token should be masked: {}", result);
    }

    #[test]
    fn test_cwe312_url_invalid_url() {
        // Invalid URLs are returned unchanged
        assert_eq!(cwe312_url("not a url"), "not a url");
    }
}
