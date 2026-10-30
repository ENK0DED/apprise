//! Upstream `parse_qsd` handling of prefixed query arguments.
//!
//! Upstream stores every argument whose key starts with `+` (HTTP headers),
//! `-` (GET parameters) or `:` (payload extras) in the `qsd+`, `qsd-` and
//! `qsd:` dictionaries, with the prefix removed and the key's case kept.
//! [`ParsedUrl::qsd`] decodes the query as form data instead, where a leading
//! `+` turns into a space and a `+` inside a value is lost, so plugins that
//! accept these overrides read them from the raw URL through [`prefixed`].

use crate::utils::parse::ParsedUrl;

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

/// Percent-decode `s` like Python's `urllib.parse.unquote`: invalid UTF-8 is
/// replaced rather than rejected and `+` is left alone.
pub fn unquote(s: &str) -> String {
  String::from_utf8_lossy(&urlencoding::decode_binary(s.as_bytes())).into_owned()
}

#[cfg(test)]
mod tests {
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
  fn test_unquote() {
    assert_eq!(unquote("a%20b+c"), "a b+c");
    assert_eq!(unquote("%E2%9C%93"), "\u{2713}");
    assert_eq!(unquote("%FF"), "\u{FFFD}");
  }
}
