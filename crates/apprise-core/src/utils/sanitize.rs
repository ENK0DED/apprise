//! Payload sanitization for safe debug/trace logging.
//!
//! Matches Python's `apprise.utils.sanitize.sanitize_payload()`.
//! Recursively walks JSON values, summarizing large strings and binary blobs
//! so they're safe to print in logs without dumping megabytes of base64.

use sha2::{Sha256, Digest};

/// Keys that commonly contain large binary-like values.
const BLOB_KEYWORDS: &[&str] = &[
    "base64", "attachment", "attachments", "base64_attachments",
    "contentbytes", "blob", "file", "data", "image", "media", "document",
];

/// Options controlling payload sanitization.
pub struct SanitizeOptions {
    /// Maximum recursion depth before truncation.
    pub max_depth: usize,
    /// Global upper bound on visited items.
    pub max_items: usize,
    /// Strings longer than this are summarized.
    pub max_str_len: usize,
    /// Characters shown at start/end of summaries.
    pub preview: usize,
    /// Maximum bytes hashed for sha256 preview.
    pub hash_sample_size: usize,
    /// Summarize values under blob-like keys aggressively.
    pub aggressive_blob_keys: bool,
}

impl Default for SanitizeOptions {
    fn default() -> Self {
        Self {
            max_depth: 10,
            max_items: 100,
            max_str_len: 512,
            preview: 32,
            hash_sample_size: 8192,
            aggressive_blob_keys: true,
        }
    }
}

/// Sanitize a JSON value for safe logging output.
///
/// Recursively walks the structure, summarizing large strings and bytes
/// so logs remain readable without leaking massive payloads.
pub fn sanitize_payload(value: &serde_json::Value) -> serde_json::Value {
    sanitize_payload_with_options(value, &SanitizeOptions::default())
}

/// Sanitize with custom options.
pub fn sanitize_payload_with_options(
    value: &serde_json::Value,
    opts: &SanitizeOptions,
) -> serde_json::Value {
    let mut items_seen: usize = 0;
    walk(value, 0, false, opts, &mut items_seen)
}

fn hash_preview(s: &str, sample_size: usize) -> String {
    let bytes = s.as_bytes();
    let to_hash = if bytes.len() > sample_size { &bytes[..sample_size] } else { bytes };
    let digest = Sha256::digest(to_hash);
    format!("{:x}", digest).chars().take(12).collect()
}

fn summarize_str(s: &str, opts: &SanitizeOptions, blob_mode: bool) -> serde_json::Value {
    let len = s.len();

    if blob_mode && opts.aggressive_blob_keys {
        let head: String = s.chars().take(opts.preview).collect();
        let tail: String = if len >= opts.preview {
            s.chars().skip(len.saturating_sub(opts.preview)).collect()
        } else {
            s.to_string()
        };
        return serde_json::Value::String(
            format!("<string len={} blob head={:?} tail={:?}>", len, head, tail)
        );
    }

    if len <= opts.max_str_len {
        return serde_json::Value::String(s.to_string());
    }

    let head: String = s.chars().take(opts.preview).collect();
    let tail: String = if len >= opts.preview {
        s.chars().skip(len.saturating_sub(opts.preview)).collect()
    } else {
        s.to_string()
    };
    serde_json::Value::String(
        format!("<string len={} head={:?} tail={:?}>", len, head, tail)
    )
}

fn is_blob_key(k: &str) -> bool {
    let lk = k.to_lowercase();
    BLOB_KEYWORDS.contains(&lk.as_str())
}

fn walk(
    value: &serde_json::Value,
    depth: usize,
    blob_mode: bool,
    opts: &SanitizeOptions,
    items_seen: &mut usize,
) -> serde_json::Value {
    if *items_seen >= opts.max_items {
        return serde_json::Value::String("<truncated: global item limit reached>".to_string());
    }
    if depth > opts.max_depth {
        return serde_json::Value::String("<truncated: max depth reached>".to_string());
    }

    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            *items_seen += 1;
            value.clone()
        }
        serde_json::Value::String(s) => {
            *items_seen += 1;
            summarize_str(s, opts, blob_mode)
        }
        serde_json::Value::Array(arr) => {
            let mut out = Vec::new();
            for entry in arr {
                if *items_seen >= opts.max_items {
                    out.push(serde_json::Value::String("<truncated: limit reached>".to_string()));
                    break;
                }
                out.push(walk(entry, depth + 1, blob_mode, opts, items_seen));
            }
            serde_json::Value::Array(out)
        }
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                if *items_seen >= opts.max_items {
                    out.insert("<truncated>".to_string(), serde_json::Value::String("...".to_string()));
                    break;
                }
                let child_blob = blob_mode || is_blob_key(k);
                out.insert(k.clone(), walk(v, depth + 1, child_blob, opts, items_seen));
            }
            serde_json::Value::Object(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_small_payload_unchanged() {
        let payload = json!({"title": "hello", "body": "world"});
        let result = sanitize_payload(&payload);
        assert_eq!(result, payload);
    }

    #[test]
    fn test_large_string_summarized() {
        let long_str = "x".repeat(1000);
        let payload = json!({"message": long_str});
        let result = sanitize_payload(&payload);
        let msg = result["message"].as_str().unwrap();
        assert!(msg.starts_with("<string len=1000"));
        assert!(msg.contains("head="));
        assert!(msg.contains("tail="));
    }

    #[test]
    fn test_blob_key_always_summarized() {
        let payload = json!({"base64": "short"});
        let result = sanitize_payload(&payload);
        let val = result["base64"].as_str().unwrap();
        assert!(val.contains("blob"), "blob key should be marked: {}", val);
    }

    #[test]
    fn test_nested_structure() {
        let payload = json!({
            "notification": {
                "title": "Test",
                "attachment": "base64datahere"
            }
        });
        let result = sanitize_payload(&payload);
        assert_eq!(result["notification"]["title"], json!("Test"));
        let att = result["notification"]["attachment"].as_str().unwrap();
        assert!(att.contains("blob"));
    }

    #[test]
    fn test_array_handling() {
        let payload = json!(["a", "b", "c"]);
        let result = sanitize_payload(&payload);
        assert_eq!(result, json!(["a", "b", "c"]));
    }

    #[test]
    fn test_max_depth() {
        let opts = SanitizeOptions { max_depth: 1, ..Default::default() };
        let payload = json!({"a": {"b": {"c": "deep"}}});
        let result = sanitize_payload_with_options(&payload, &opts);
        // At depth 2, it should truncate
        let inner = &result["a"]["b"];
        assert!(inner.as_str().unwrap().contains("max depth"));
    }

    #[test]
    fn test_max_items() {
        let opts = SanitizeOptions { max_items: 3, ..Default::default() };
        let payload = json!({"a": 1, "b": 2, "c": 3, "d": 4, "e": 5});
        let result = sanitize_payload_with_options(&payload, &opts);
        // Should have truncation marker
        let obj = result.as_object().unwrap();
        let has_truncated = obj.keys().any(|k| k.contains("truncated"));
        assert!(has_truncated || obj.len() < 5);
    }

    #[test]
    fn test_null_bool_number_passthrough() {
        let payload = json!({"n": null, "b": true, "i": 42, "f": 3.14});
        let result = sanitize_payload(&payload);
        assert_eq!(result, payload);
    }
}
