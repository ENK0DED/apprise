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

// ── Prefixed arguments ──────────────────────────────────────────────────────
//
// Upstream `parse_qsd` handling of prefixed query arguments.
//
// Upstream stores every argument whose key starts with `+` (HTTP headers),
// `-` (GET parameters) or `:` (payload extras) in the `qsd+`, `qsd-` and
// `qsd:` dictionaries, with the prefix removed and the key's case kept.
// [`ParsedUrl::qsd`] decodes the query as form data instead, where a leading
// `+` turns into a space and a `+` inside a value is lost, so plugins that
// accept these overrides read them from the raw URL through [`prefixed`].

/// The arguments of `url` whose key starts with `prefix` (`+`, `-` or `:`),
/// in URL order with the prefix removed.
///
/// Matches upstream: keys and values are percent-decoded once, a `+` stays a
/// literal `+` (except inside a key, where it is a space), values are trimmed,
/// and a repeated key keeps its first position but takes the last value. A
/// `+` key also matches a leading space (`%20Key`), as upstream does.
pub fn prefixed(url: &ParsedUrl, prefix: char) -> Vec<(String, String)> {
  let mut out: Vec<(String, String)> = Vec::new();
  for (key, value) in pairs(url) {
    let stripped = match prefix {
      '+' => key.strip_prefix('+').or_else(|| key.strip_prefix(' ')),
      p => key.strip_prefix(p),
    };
    let Some(key) = stripped else { continue };
    match out.iter_mut().find(|(k, _)| k == key) {
      Some(entry) => entry.1 = value,
      None => out.push((key.to_string(), value)),
    }
  }
  out
}

/// The value of argument `key` as upstream's `qsd` dictionary holds it: the
/// key is matched case-insensitively and the value keeps any literal `+`
/// (which [`ParsedUrl::get`] turns into a space).
pub fn get(url: &ParsedUrl, key: &str) -> Option<String> {
  let key = key.to_lowercase();
  pairs(url).filter(|(k, _)| k.trim().to_lowercase() == key).map(|(_, v)| v).last()
}

/// Every `key=value` pair of the query, decoded as upstream `parse_qsd`
/// does. Keys are not trimmed or lowercased; values are trimmed.
fn pairs(url: &ParsedUrl) -> impl Iterator<Item = (String, String)> + '_ {
  let query = url.raw.split_once('?').map(|(_, q)| q.trim()).unwrap_or("");
  query.split('&').flat_map(|s| s.split(';')).filter(|_| !query.is_empty()).map(|pair| {
    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
    // The first character is kept as-is so a leading `+` survives
    let mut chars = name.chars();
    let key = match chars.next() {
      Some(first) => unquote(&format!("{}{}", first, chars.as_str().replace('+', " "))),
      None => String::new(),
    };
    (key, unquote(value).trim().to_string())
  })
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
mod prefixed_tests {
  use super::*;

  fn parse(raw: &str) -> ParsedUrl {
    ParsedUrl::parse(raw).unwrap()
  }

  #[test]
  fn test_prefixed_keeps_plus_and_case() {
    let url = parse("json://localhost/?+X-Head=v+1&-Param=a+b&:Field=x%2By&method=get&+X-Head2=%2B1");
    assert_eq!(prefixed(&url, '+'), vec![("X-Head".to_string(), "v+1".to_string()), ("X-Head2".to_string(), "+1".to_string())]);
    assert_eq!(prefixed(&url, '-'), vec![("Param".to_string(), "a+b".to_string())]);
    assert_eq!(prefixed(&url, ':'), vec![("Field".to_string(), "x+y".to_string())]);
  }

  #[test]
  fn test_prefixed_edge_cases() {
    // Encoded prefixes, a space-prefixed header, valueless keys, semicolons,
    // repeated keys and trimmed values
    let url = parse("json://localhost/?%2BA=1;%3Ab;%20C=%20x%20&:b=2&+A=3&-");
    assert_eq!(prefixed(&url, '+'), vec![("A".to_string(), "3".to_string()), ("C".to_string(), "x".to_string())]);
    assert_eq!(prefixed(&url, ':'), vec![("b".to_string(), "2".to_string())]);
    assert_eq!(prefixed(&url, '-'), vec![(String::new(), String::new())]);

    // A `+` inside a key is a space
    let url = parse("json://localhost/?+My+Header=1");
    assert_eq!(prefixed(&url, '+'), vec![("My Header".to_string(), "1".to_string())]);

    assert!(prefixed(&parse("json://localhost/"), '+').is_empty());
  }

  #[test]
  fn test_get_keeps_plus() {
    let url = parse("form://localhost/?attach-as=file+&Method=put&x=1&x=2");
    assert_eq!(get(&url, "attach-as").as_deref(), Some("file+"));
    assert_eq!(get(&url, "method").as_deref(), Some("put"));
    assert_eq!(get(&url, "x").as_deref(), Some("2"));
    assert_eq!(get(&url, "missing"), None);
    assert_eq!(get(&parse("form://localhost"), "x"), None);
  }

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
    assert_eq!(unquote("%E2%9C%93"), "\u{2713}");
    assert_eq!(unquote("%FF"), "\u{FFFD}");
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
