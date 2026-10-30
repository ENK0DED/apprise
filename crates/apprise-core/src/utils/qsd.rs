//! Small string helpers with Python semantics that plugins use while
//! reading their URL: `urllib.parse.quote`/`unquote` (for the values upstream
//! decodes a second time), `validate_regex`'s default token rule, and
//! `parse_list`.
//!
//! Query arguments themselves come from [`crate::utils::parse::ParsedUrl`],
//! which already follows upstream `parse_qsd()`.

/// Python's `urllib.parse.unquote()`: decode `%XX` escapes as UTF-8
/// (invalid sequences become U+FFFD), leave `+` and malformed escapes alone.
pub fn unquote(s: &str) -> String {
  if !s.contains('%') {
    return s.to_string();
  }
  let bytes = s.as_bytes();
  let mut out = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%'
      && i + 2 < bytes.len()
      && let (Some(hi), Some(lo)) = ((bytes[i + 1] as char).to_digit(16), (bytes[i + 2] as char).to_digit(16))
    {
      out.push((hi * 16 + lo) as u8);
      i += 3;
      continue;
    }
    out.push(bytes[i]);
    i += 1;
  }
  String::from_utf8_lossy(&out).into_owned()
}

/// Upstream `validate_regex(value)` with its default pattern: the value must
/// start with a non-space character and is returned trimmed.
pub fn validate_token(value: &str) -> Option<String> {
  let first = value.chars().next()?;
  if crate::utils::email::is_space(first) { None } else { Some(value.trim().to_string()) }
}

/// Upstream `parse_list`: split on whitespace, commas and semicolons, then
/// sort and drop duplicates and empty entries.
pub fn parse_list(s: &str) -> Vec<String> {
  let mut list: Vec<String> = s.split(|c: char| c.is_whitespace() || c == ',' || c == ';').filter(|t| !t.is_empty()).map(String::from).collect();
  list.sort();
  list.dedup();
  list
}

/// Percent-encode `s` like Python's `urllib.parse.quote`: letters, digits,
/// `_.-~` and the characters in `safe` are kept, everything else is encoded
/// as UTF-8.
pub fn quote(s: &str, safe: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for c in s.chars() {
    if c.is_ascii_alphanumeric() || "_.-~".contains(c) || safe.contains(c) {
      out.push(c);
    } else {
      let mut buf = [0u8; 4];
      for b in c.encode_utf8(&mut buf).bytes() {
        out.push_str(&format!("%{:02X}", b));
      }
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_list() {
    assert_eq!(parse_list("b,a, c  d/e\\f,,a;g"), vec!["a", "b", "c", "d/e\\f", "g"]);
    assert!(parse_list(" , ").is_empty());
  }

  #[test]
  fn test_quote() {
    assert_eq!(quote("http://a.com/\u{e9} x?a=1&b=%", ":/?&="), "http://a.com/%C3%A9%20x?a=1&b=%25");
    assert_eq!(quote("a~b_c.d-e*", ""), "a~b_c.d-e%2A");
  }

  #[test]
  fn test_unquote() {
    assert_eq!(unquote("a%20b+c"), "a b+c");
    assert_eq!(unquote("garbage%"), "garbage%");
    assert_eq!(unquote("%zz%4"), "%zz%4");
    assert_eq!(unquote("J%C3%B6rg"), "Jörg");
    assert_eq!(unquote("%E2%9C%93"), "\u{2713}");
    assert_eq!(unquote("%FF"), "\u{FFFD}");
  }
}
