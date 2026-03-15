//! URL-safe Base64 encoding/decoding utilities matching Python's apprise base64 module.
//!
//! Used by VAPID (WebPush) and OneSignal plugins.

use base64::Engine;
use std::collections::HashMap;

/// URL-safe Base64 encode (no padding), matching Python's `base64.urlsafe_b64encode().rstrip(b"=")`.
pub fn base64_urlencode(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// URL-safe Base64 decode (handles missing padding), matching Python's `base64.urlsafe_b64decode()`.
pub fn base64_urldecode(data: &str) -> Option<Vec<u8>> {
    // Add padding back
    let padded = match data.len() % 4 {
        2 => format!("{}==", data),
        3 => format!("{}=", data),
        _ => data.to_string(),
    };
    base64::engine::general_purpose::URL_SAFE.decode(&padded).ok()
}

/// Decode a dict where string values prefixed with `b64:` are base64-decoded
/// and parsed as JSON, matching Python's `decode_b64_dict()`.
pub fn decode_b64_dict(di: &HashMap<String, serde_json::Value>) -> HashMap<String, serde_json::Value> {
    let mut result = di.clone();
    for (k, v) in result.iter_mut() {
        if let Some(s) = v.as_str() {
            if let Some(encoded) = s.strip_prefix("b64:") {
                if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                    if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&decoded) {
                        *v = parsed;
                        continue;
                    }
                }
                // Decoding failed — leave the original value
            }
        }
        let _ = k; // suppress unused warning
    }
    result
}

/// Encode a dict, converting non-string values to `b64:` prefixed base64 strings.
/// Returns `(encoded_dict, needs_decoding)` matching Python's `encode_b64_dict()`.
pub fn encode_b64_dict(
    di: &HashMap<String, serde_json::Value>,
) -> (HashMap<String, serde_json::Value>, bool) {
    let mut result = di.clone();
    let mut needs_decoding = false;

    for (_k, v) in result.iter_mut() {
        if v.is_string() {
            continue;
        }
        // Encode non-string values as b64: prefixed JSON
        if let Ok(json_bytes) = serde_json::to_vec(v) {
            let encoded = base64::engine::general_purpose::URL_SAFE.encode(&json_bytes);
            *v = serde_json::Value::String(format!("b64:{}", encoded));
            needs_decoding = true;
        } else {
            *v = serde_json::Value::String(v.to_string());
        }
    }

    (result, needs_decoding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_urlencode() {
        assert_eq!(base64_urlencode(b"hello"), "aGVsbG8");
        assert_eq!(base64_urlencode(b""), "");
        // Ensure no padding
        assert!(!base64_urlencode(b"a").contains('='));
    }

    #[test]
    fn test_base64_urldecode() {
        assert_eq!(base64_urldecode("aGVsbG8"), Some(b"hello".to_vec()));
        assert_eq!(base64_urldecode(""), Some(b"".to_vec()));
        // Should handle missing padding
        assert_eq!(base64_urldecode("YQ"), Some(b"a".to_vec()));
    }

    #[test]
    fn test_roundtrip() {
        let data = b"test data with special chars: \x00\xff";
        let encoded = base64_urlencode(data);
        let decoded = base64_urldecode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_decode_b64_dict_with_prefix() {
        let mut di = HashMap::new();
        // "hello" as JSON string, base64 encoded
        let b64_val = base64::engine::general_purpose::STANDARD.encode(b"\"hello\"");
        di.insert("key".to_string(), serde_json::json!(format!("b64:{}", b64_val)));
        di.insert("normal".to_string(), serde_json::json!("plain"));

        let result = decode_b64_dict(&di);
        assert_eq!(result["key"], serde_json::json!("hello"));
        assert_eq!(result["normal"], serde_json::json!("plain"));
    }

    #[test]
    fn test_decode_b64_dict_invalid_b64() {
        let mut di = HashMap::new();
        di.insert("key".to_string(), serde_json::json!("b64:!!!invalid!!!"));
        let result = decode_b64_dict(&di);
        // Should leave unchanged on decode failure
        assert_eq!(result["key"], serde_json::json!("b64:!!!invalid!!!"));
    }

    #[test]
    fn test_encode_b64_dict() {
        let mut di = HashMap::new();
        di.insert("str_val".to_string(), serde_json::json!("already a string"));
        di.insert("num_val".to_string(), serde_json::json!(42));

        let (result, needs_decoding) = encode_b64_dict(&di);
        assert!(needs_decoding);
        assert_eq!(result["str_val"], serde_json::json!("already a string"));
        assert!(result["num_val"].as_str().unwrap().starts_with("b64:"));
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let mut di = HashMap::new();
        di.insert("text".to_string(), serde_json::json!("hello"));
        di.insert("number".to_string(), serde_json::json!(123));
        di.insert("bool".to_string(), serde_json::json!(true));

        let (encoded, _) = encode_b64_dict(&di);
        let decoded = decode_b64_dict(&encoded);

        assert_eq!(decoded["text"], serde_json::json!("hello"));
        assert_eq!(decoded["number"], serde_json::json!(123));
        assert_eq!(decoded["bool"], serde_json::json!(true));
    }
}
