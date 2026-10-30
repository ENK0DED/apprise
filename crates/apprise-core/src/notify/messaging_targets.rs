//! Target list parsers shared by the messaging plugins, ported from
//! upstream `apprise/utils/parse.py` (`is_phone_no`, `parse_phone_no`,
//! `parse_list`, the default `validate_regex` token check) and
//! `URLBase.unquote`.

/// Longest phone number (in digits) upstream accepts.
const PHONE_MAX_LEN: usize = 14;

/// Upstream `is_phone_no(phone, min_len)`: return the phone number's digits
/// (upstream's `full` value) when `phone` looks like a phone number with
/// `min_len` to 14 digits.
pub(super) fn is_phone_no(phone: &str, min_len: usize) -> Option<String> {
  // ^\+?[0-9\s)(+-]+\s*$
  let rest = phone.strip_prefix('+').unwrap_or(phone);
  if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, ')' | '(' | '+' | '-')) {
    return None;
  }
  let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
  if digits.len() > PHONE_MAX_LEN || digits.len() < min_len {
    return None;
  }
  Some(digits)
}

/// Upstream `parse_phone_no()` (with `store_unparseable=True`): find every
/// phone-number-like run in each entry. An entry with none is split on the
/// usual delimiters and kept as-is so that callers can report it.
pub(super) fn parse_phone_no<S: AsRef<str>>(entries: &[S]) -> Vec<String> {
  let mut result = Vec::new();
  for entry in entries {
    let entry = entry.as_ref();
    if entry.is_empty() {
      continue;
    }
    let found = find_phone_numbers(entry);
    if found.is_empty() {
      result.extend(split_delimited(entry));
    } else {
      result.extend(found);
    }
  }
  result
}

/// Upstream `parse_list()`: split each entry on whitespace, commas,
/// semicolons and brackets, then drop duplicates and sort.
pub(super) fn parse_list<S: AsRef<str>>(entries: &[S]) -> Vec<String> {
  let mut result: Vec<String> = entries.iter().flat_map(|e| split_delimited(e.as_ref())).collect();
  result.sort();
  result.dedup();
  result
}

/// Upstream `validate_regex(value)` with its default `[^\s]+` pattern: the
/// value must start with a non-whitespace character; it is returned trimmed.
pub(super) fn validate_token(value: &str) -> Option<String> {
  match value.chars().next() {
    Some(c) if !c.is_whitespace() => Some(value.trim().to_string()),
    _ => None,
  }
}

/// Upstream `URLBase.unquote()`: percent-decode `value`, replacing invalid
/// UTF-8 sequences.
pub(super) fn unquote(value: &str) -> String {
  String::from_utf8_lossy(&urlencoding::decode_binary(value.as_bytes())).into_owned()
}

/// Split on upstream's `STRING_DELIMITERS` (`[\[\]\;,\s]+`).
fn split_delimited(value: &str) -> Vec<String> {
  value.split(|c: char| c.is_whitespace() || matches!(c, '[' | ']' | ';' | ',')).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// `findall()` over upstream's `PHONE_NO_DETECTION_RE`:
///
/// ```text
/// ((?:[+(][+(\s]{0,32})?[0-9][0-9()\s-]{1,32}[0-9])(?=$|[\s,+(]+[0-9])
/// ```
fn find_phone_numbers(value: &str) -> Vec<String> {
  let chars: Vec<char> = value.chars().collect();
  let mut found = Vec::new();
  let mut start = 0;
  while start < chars.len() {
    match match_phone_at(&chars, start) {
      Some(end) => {
        found.push(chars[start..end].iter().collect());
        start = end;
      }
      None => start += 1,
    }
  }
  found
}

/// Try `PHONE_NO_DETECTION_RE` at `start`, returning the end of the match.
fn match_phone_at(chars: &[char], start: usize) -> Option<usize> {
  let is_digit = |i: usize| chars.get(i).is_some_and(|c| c.is_ascii_digit());
  let is_middle = |c: char| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '(' | ')' | '-');
  let is_gap = |c: char| c.is_whitespace() || matches!(c, ',' | '+' | '(');

  // Optional prefix: [+(][+(\s]{0,32}. None of its characters are digits and
  // a digit must follow, so only the longest prefix can lead to a match.
  let mut pos = start;
  if matches!(chars[pos], '+' | '(') {
    pos += 1;
    let mut taken = 0;
    while taken < 32 && chars.get(pos).is_some_and(|&c| c == '+' || c == '(' || c.is_whitespace()) {
      pos += 1;
      taken += 1;
    }
  }

  // [0-9]
  if !is_digit(pos) {
    return None;
  }
  let first = pos + 1;

  // [0-9()\s-]{1,32}[0-9], backtracking from the longest middle run
  let run = chars[first..].iter().take_while(|&&c| is_middle(c)).count();
  for middle in (1..=run.min(32)).rev() {
    let last = first + middle;
    if !is_digit(last) {
      continue;
    }
    let end = last + 1;
    // (?=$|[\s,+(]+[0-9])
    let gap = chars[end..].iter().take_while(|&&c| is_gap(c)).count();
    if end == chars.len() || (gap > 0 && is_digit(end + gap)) {
      return Some(end);
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_is_phone_no() {
    assert_eq!(is_phone_no("+1 (555) 123-4567", 10).as_deref(), Some("15551234567"));
    assert_eq!(is_phone_no("15551234567", 10).as_deref(), Some("15551234567"));
    assert_eq!(is_phone_no("12345678901234", 10).as_deref(), Some("12345678901234"));
    // Too long or too short
    assert!(is_phone_no("123456789012345", 10).is_none());
    assert!(is_phone_no("123456789", 10).is_none());
    assert_eq!(is_phone_no("411", 3).as_deref(), Some("411"));
    // Not phone-like content
    assert!(is_phone_no("", 10).is_none());
    assert!(is_phone_no("+", 10).is_none());
    assert!(is_phone_no("abcd", 10).is_none());
    assert!(is_phone_no("@12", 1).is_none());
    assert!(is_phone_no("1555123456a", 10).is_none());
  }

  #[test]
  fn test_parse_phone_no() {
    // Expected values captured from upstream parse_phone_no()
    let cases: &[(&str, &[&str])] = &[
      ("+1(555) 123-1234", &["+1(555) 123-1234"]),
      ("1555 5555555", &["1555 5555555"]),
      ("12", &["12"]),
      ("@12", &["@12"]),
      ("abcd", &["abcd"]),
      ("a b,c", &["a", "b", "c"]),
      ("15551234567,15551234568", &["15551234567", "15551234568"]),
      ("15551234567 +15551234568", &["15551234567", "+15551234568"]),
      ("1 (405) 123 1234", &["1 (405) 123 1234"]),
      ("+48123456789", &["+48123456789"]),
      ("#123", &["123"]),
      ("1", &["1"]),
      ("1-", &["1-"]),
      ("12a34", &["12a34"]),
      ("555-1234x", &["555-1234x"]),
      ("(555)1234", &["(555)1234"]),
      ("++ ( 12 34", &["++ ( 12 34"]),
      ("12 ab 34", &["12", "ab", "34"]),
      ("12;34", &["12", "34"]),
    ];
    for (input, expected) in cases {
      assert_eq!(parse_phone_no(&[*input]), *expected, "input: {:?}", input);
    }
    // Empty entries are skipped
    assert!(parse_phone_no(&[""]).is_empty());
  }

  #[test]
  fn test_parse_list() {
    assert_eq!(parse_list(&["b,a", "c a", "[d];e"]), vec!["a", "b", "c", "d", "e"]);
    assert!(parse_list::<&str>(&[]).is_empty());
  }

  #[test]
  fn test_unquote() {
    assert_eq!(unquote("123456789%3Aabc"), "123456789:abc");
    assert_eq!(unquote("%21%21"), "!!");
    assert_eq!(unquote("plain"), "plain");
    assert_eq!(unquote("%FF"), "\u{FFFD}");
  }

  #[test]
  fn test_validate_token() {
    assert_eq!(validate_token("abc ").as_deref(), Some("abc"));
    assert_eq!(validate_token("a b").as_deref(), Some("a b"));
    assert!(validate_token(" abc").is_none());
    assert!(validate_token("").is_none());
    assert!(validate_token("   ").is_none());
  }
}
