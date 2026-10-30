//! Matrix event sizing (upstream `apprise/plugins/matrix/sizing.py`).
//!
//! A Matrix event must stay below 65536 bytes once serialized, so the body
//! limit of one notification depends on what it holds: JSON escapes, a
//! second HTML representation, the title wrappers and, in encrypted rooms,
//! the Megolm growth and envelope. [`body_char_limit`] finds the largest
//! character count that keeps every chunk of the body inside that budget.

use crate::types::NotifyFormat;

/// Matrix's hard event-size ceiling.
pub(super) const MATRIX_EVENT_BYTE_LIMIT: usize = 65536;

/// Room for fields Apprise does not control (homeserver metadata,
/// signatures once federated, JSON punctuation slop).
pub(super) const MATRIX_EVENT_SAFETY_MARGIN: usize = 4000;

/// The JSON fields around plaintext or encrypted message content.
const MATRIX_CONTENT_JSON_OVERHEAD: usize = 200;

/// `sender_key`/`session_id` are always the unpadded base64 of a 32-byte key.
const MATRIX_E2EE_KEY_LEN: usize = 43;

/// Budget for a device ID that is not known yet (fresh logins).
const MATRIX_DEVICE_ID_FALLBACK_BYTES: usize = 512;

/// JSON overhead of an `m.room.encrypted` payload's fixed fields, without
/// the ciphertext and device ID values (upstream measures `json.dumps()`).
fn e2ee_envelope_structural_bytes() -> usize {
  let key = "x".repeat(MATRIX_E2EE_KEY_LEN);
  format!(r#"{{"algorithm": "m.megolm.v1.aes-sha2", "ciphertext": "", "sender_key": "{key}", "session_id": "{key}", "device_id": ""}}"#).len()
}

/// The unpadded base64 length of `byte_count` bytes.
fn b64_unpadded_len(byte_count: usize) -> usize {
  let (full, rem) = (byte_count / 3, byte_count % 3);
  if rem == 0 { full * 4 } else { full * 4 + rem + 1 }
}

/// The length of `n` as a protobuf varint.
fn varint_len(n: usize) -> usize {
  if n == 0 { 1 } else { (usize::BITS - n.leading_zeros()).div_ceil(7) as usize }
}

/// The base64 Megolm ciphertext length for a plaintext of `plaintext_len`
/// bytes at message index `counter`, computed without encrypting (upstream
/// `predict_megolm_ciphertext_len`).
pub(super) fn predict_megolm_ciphertext_len(plaintext_len: usize, counter: usize) -> usize {
  // AES-256-CBC with PKCS7 padding always adds 1-16 bytes.
  let padded_len = (plaintext_len / 16 + 1) * 16;
  let counter_field_len = 1 + varint_len(counter);
  let ciphertext_field_len = 1 + varint_len(padded_len) + padded_len;
  // Version byte, both fields, the truncated HMAC and the Ed25519 signature.
  let total = 1 + counter_field_len + ciphertext_field_len + 8 + 64;
  b64_unpadded_len(total)
}

fn utf8_char_len(ch: char) -> usize {
  ch.len_utf8()
}

/// A character's UTF-8 cost after JSON string escaping.
fn json_char_bytes(ch: char) -> usize {
  match ch {
    '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
    c if (c as u32) < 0x20 => 6,
    c => utf8_char_len(c),
  }
}

fn json_str_bytes(s: &str) -> usize {
  s.chars().map(json_char_bytes).sum()
}

/// The largest input accepted by a non-decreasing cost function.
fn max_byte_budget(cost_fn: impl Fn(usize) -> usize, byte_budget: usize, ceiling: usize) -> usize {
  if cost_fn(0) > byte_budget {
    return 0;
  }
  let (mut low, mut high) = (0, ceiling);
  while low < high {
    let middle = (low + high).div_ceil(2);
    if cost_fn(middle) <= byte_budget {
      low = middle;
    } else {
      high = middle - 1;
    }
  }
  low
}

/// A character limit safe for every chunk of `body`: a sliding window
/// finds the densest region instead of relying on an average.
fn fit_char_limit(body: &[char], byte_budget: usize, cost_fn: impl Fn(usize) -> usize) -> usize {
  if body.is_empty() {
    return 1;
  }
  let ceiling = byte_budget * 16 + 1024;
  let raw_byte_target = max_byte_budget(cost_fn, byte_budget, ceiling);

  let mut window_sum = 0;
  let mut left = 0;
  let mut shortest_over: Option<usize> = None;
  for (right, &ch) in body.iter().enumerate() {
    window_sum += json_char_bytes(ch);
    while window_sum > raw_byte_target {
      let window_length = right + 1 - left;
      if shortest_over.is_none_or(|s| window_length < s) {
        shortest_over = Some(window_length);
      }
      window_sum -= json_char_bytes(body[left]);
      left += 1;
    }
  }
  match shortest_over {
    None => body.len(),
    Some(s) => s.saturating_sub(1).max(1),
  }
}

fn device_id_overhead_bytes(device_id: Option<&str>) -> usize {
  match device_id {
    Some(d) if !d.is_empty() => json_str_bytes(d),
    _ => MATRIX_DEVICE_ID_FALLBACK_BYTES,
  }
}

fn is_rich(format: &NotifyFormat) -> bool {
  matches!(format, NotifyFormat::Html | NotifyFormat::Markdown)
}

/// The JSON bytes the title wrappers add (`# title\r\n`, and for rich
/// messages `<h1>title</h1><br/>` too).
fn title_overhead_bytes(title: &str, body_format: &NotifyFormat) -> usize {
  if title.is_empty() {
    return 0;
  }
  let plain = json_str_bytes(&format!("# {title}\r\n"));
  if !is_rich(body_format) {
    return plain;
  }
  plain + json_str_bytes(&format!("<h1>{}</h1><br/>", super::escape_html(title, false, false)))
}

/// A chunk's estimated final event size.
fn wire_cost(content_bytes: usize, body_format: &NotifyFormat, e2ee_capable: bool, title_bytes: usize, device_id: Option<&str>) -> usize {
  // Rich messages carry a plain and a formatted representation.
  let multiplier = if is_rich(body_format) { 2 } else { 1 };
  let inner_bytes = content_bytes * multiplier + title_bytes + MATRIX_CONTENT_JSON_OVERHEAD;
  if !e2ee_capable {
    return inner_bytes;
  }
  predict_megolm_ciphertext_len(inner_bytes, 0) + e2ee_envelope_structural_bytes() + device_id_overhead_bytes(device_id)
}

/// The safe Matrix body limit, in characters, for one notification
/// (upstream `body_char_limit`). `body` and `title` are the bounded preview
/// (`body.rstrip()`, `title.strip()[:title_maxlen]`).
pub(super) fn body_char_limit(body: &str, title: &str, body_format: &NotifyFormat, e2ee_capable: bool, device_id: Option<&str>) -> usize {
  let title_bytes = title_overhead_bytes(title, body_format);
  let byte_budget = MATRIX_EVENT_BYTE_LIMIT - MATRIX_EVENT_SAFETY_MARGIN;
  let chars: Vec<char> = body.chars().collect();
  fit_char_limit(&chars, byte_budget, |content_bytes| wire_cost(content_bytes, body_format, e2ee_capable, title_bytes, device_id))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_envelope_overhead_matches_upstream() {
    // len(json.dumps({...}, ensure_ascii=False)) from upstream sizing.py.
    assert_eq!(e2ee_envelope_structural_bytes(), 194);
  }

  #[test]
  fn test_predicted_ciphertext_lengths() {
    // Values from upstream predict_megolm_ciphertext_len().
    assert_eq!(predict_megolm_ciphertext_len(0, 0), 124);
    assert_eq!(predict_megolm_ciphertext_len(15, 0), 124);
    assert_eq!(predict_megolm_ciphertext_len(16, 0), 146);
    assert_eq!(predict_megolm_ciphertext_len(1000, 300), 1450);
  }

  #[test]
  fn test_body_char_limit_matches_upstream() {
    // Values from upstream sizing.body_char_limit() on the same inputs.
    let long = "a".repeat(70000);
    assert_eq!(body_char_limit(&long, "", &NotifyFormat::Text, false, None), 61336);
    assert_eq!(body_char_limit(&long, "Title", &NotifyFormat::Html, false, None), 30653);
    assert_eq!(body_char_limit(&long, "Title", &NotifyFormat::Markdown, true, None), 22652);
    assert_eq!(body_char_limit(&long, "Title", &NotifyFormat::Text, true, Some("DEVICEID")), 45708);
    let dense = "\"\\\n\u{1}é😀".repeat(20000);
    assert_eq!(body_char_limit(&dense, "T<i>", &NotifyFormat::Html, true, Some("DEV")), 7614);
    assert_eq!(body_char_limit("short", "", &NotifyFormat::Text, false, None), 5);
    assert_eq!(body_char_limit("", "", &NotifyFormat::Text, false, None), 1);
  }

  #[test]
  fn test_dense_region_sets_the_limit() {
    // A short run of escape-heavy text bounds the limit for the whole body.
    let body = format!("{}{}{}", "a".repeat(40000), "\u{1}".repeat(20000), "a".repeat(40000));
    let limit = body_char_limit(&body, "", &NotifyFormat::Text, false, None);
    let budget = MATRIX_EVENT_BYTE_LIMIT - MATRIX_EVENT_SAFETY_MARGIN - MATRIX_CONTENT_JSON_OVERHEAD;
    // Any window of `limit` characters fits, wherever it lands.
    let costs: Vec<usize> = body.chars().map(json_char_bytes).collect();
    let mut sum: usize = costs[..limit].iter().sum();
    let mut worst = sum;
    for i in limit..costs.len() {
      sum = sum + costs[i] - costs[i - limit];
      worst = worst.max(sum);
    }
    assert!(worst <= budget, "{worst} > {budget}");
    assert!(limit < 61336);
  }
}
