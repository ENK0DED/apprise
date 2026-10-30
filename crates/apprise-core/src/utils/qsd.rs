//! Query string parsing that follows upstream `parse_qsd()` exactly.
//!
//! [`ParsedUrl::qsd`] comes from the `url` crate, which turns `+` into a
//! space and forgets the argument order. Upstream keeps `+` literal (it is
//! common in tokens, labels such as `user+tag@example.com` and display names)
//! and keeps the order of `+header` and `:token` arguments, which some
//! services pass straight through. This re-parses the raw URL instead.

use crate::utils::parse::ParsedUrl;

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

/// The query arguments of an Apprise URL, in the order given.
#[derive(Debug, Clone, Default)]
pub struct Qsd {
  /// `(key, value)` with the key decoded but not lower-cased.
  entries: Vec<(String, String)>,
}

impl Qsd {
  /// Parse the query string of `url` (everything after the first `?`).
  pub fn from_url(url: &ParsedUrl) -> Self {
    match url.raw.split_once('?') {
      Some((_, qs)) => Self::parse(qs.trim()),
      None => Self::default(),
    }
  }

  /// Upstream `parse_qsd()`: pairs split on `&` and `;`; a `+` in a key
  /// (other than its first character) means a space; values are decoded
  /// and trimmed but `+` stays literal.
  pub fn parse(qs: &str) -> Self {
    let mut entries = Vec::new();
    for pair in qs.split(['&', ';']) {
      let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
      let mut chars = k.chars();
      let key = match chars.next() {
        Some(first) => format!("{}{}", first, chars.as_str().replace('+', " ")),
        None => String::new(),
      };
      entries.push((unquote(&key), unquote(v).trim().to_string()));
    }
    Self { entries }
  }

  /// Upstream `results["qsd"][key]`: case-insensitive, the last value wins.
  /// Empty values are returned as `Some("")`.
  pub fn get(&self, key: &str) -> Option<&str> {
    self.entries.iter().rev().find(|(k, _)| k.trim().to_lowercase() == key).map(|(_, v)| v.as_str())
  }

  /// Like [`Qsd::get`], but `None` for an empty value (upstream's
  /// `if key in qsd and len(qsd[key])`).
  pub fn value(&self, key: &str) -> Option<&str> {
    self.get(key).filter(|v| !v.is_empty())
  }

  /// Upstream `results["qsd+"]` (prefix `'+'`) or `results["qsd:"]`
  /// (prefix `':'`): arguments whose key starts with the prefix, with the
  /// prefix removed, in first-seen order with the last value winning. A
  /// `+` prefix also matches a leading space, which is what `+` decodes to.
  pub fn prefixed(&self, prefix: char) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in &self.entries {
      let Some(first) = k.chars().next() else { continue };
      if first != prefix && !(prefix == '+' && first == ' ') {
        continue;
      }
      let name = &k[first.len_utf8()..];
      match out.iter_mut().find(|(existing, _)| existing == name) {
        Some(entry) => entry.1 = v.clone(),
        None => out.push((name.to_string(), v.clone())),
      }
    }
    out
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_unquote() {
    assert_eq!(unquote("a%20b+c"), "a b+c");
    assert_eq!(unquote("garbage%"), "garbage%");
    assert_eq!(unquote("%zz%4"), "%zz%4");
    assert_eq!(unquote("J%C3%B6rg"), "Jörg");
    assert_eq!(unquote("%FF"), "\u{FFFD}");
  }

  #[test]
  fn test_get_keeps_plus_and_last_value() {
    let q = Qsd::parse("Name=John+Doe&to=a%2Bb@x.com&TO=c@x.com&empty=&flag");
    assert_eq!(q.get("name"), Some("John+Doe"));
    assert_eq!(q.get("to"), Some("c@x.com"));
    assert_eq!(q.get("empty"), Some(""));
    assert_eq!(q.value("empty"), None);
    assert_eq!(q.get("flag"), Some(""));
    assert_eq!(q.get("missing"), None);
  }

  #[test]
  fn test_prefixed_keeps_order() {
    let q = Qsd::parse(":zulu=z&:alpha=a&+X-One=1&%20X-Two=2&:zulu=last&+X+Spaced=s");
    assert_eq!(q.prefixed(':'), vec![("zulu".to_string(), "last".to_string()), ("alpha".to_string(), "a".to_string())]);
    assert_eq!(
      q.prefixed('+'),
      vec![("X-One".to_string(), "1".to_string()), ("X-Two".to_string(), "2".to_string()), ("X Spaced".to_string(), "s".to_string())]
    );
  }

  #[test]
  fn test_from_url() {
    let url = ParsedUrl::parse("mailgun://user@example.com/key?name=A+B&+X-H=v").unwrap();
    let q = Qsd::from_url(&url);
    assert_eq!(q.get("name"), Some("A+B"));
    assert_eq!(q.prefixed('+'), vec![("X-H".to_string(), "v".to_string())]);
    assert!(Qsd::from_url(&ParsedUrl::parse("mailgun://user@example.com/key").unwrap()).get("name").is_none());
  }
}
