//! Argument parsing with the exact semantics of upstream `apprise.utils.parse`
//! (`parse_bool`, `parse_list`) and `URLBase.split_path`, for plugins whose
//! requests depend on them (target order, duplicate removal, defaults).

use crate::utils::parse::ParsedUrl;

/// Upstream `parse_bool(arg, default)`: only the first two characters are
/// compared, and anything unrecognised returns `default`.
pub fn parse_bool_or(value: Option<&str>, default: bool) -> bool {
  let Some(value) = value else { return default };
  let lower = value.to_lowercase();
  let head: String = lower.chars().take(2).collect();
  match head.as_str() {
    "de" | "di" | "ne" | "f" | "n" | "no" | "of" | "0" | "fa" => false,
    "en" | "al" | "t" | "y" | "ye" | "on" | "1" | "tr" => true,
    _ => default,
  }
}

fn is_list_delim(c: char, allow_whitespace: bool) -> bool {
  matches!(c, '[' | ']' | ';' | ',') || (allow_whitespace && c.is_whitespace())
}

/// Upstream `parse_list()`: split every entry on `[\[\];,\s]+` (whitespace
/// only when `allow_whitespace`). With `sort`, the unique entries are
/// returned sorted, otherwise in their original order with duplicates kept.
pub fn parse_list<I, S>(entries: I, allow_whitespace: bool, sort: bool) -> Vec<String>
where
  I: IntoIterator<Item = S>,
  S: AsRef<str>,
{
  let mut result: Vec<String> = entries
    .into_iter()
    .flat_map(|e| e.as_ref().split(|c| is_list_delim(c, allow_whitespace)).map(str::to_string).collect::<Vec<_>>())
    .map(|s| if allow_whitespace { s } else { s.trim().to_string() })
    .filter(|s| !s.is_empty())
    .collect();
  if sort {
    result.sort();
    result.dedup();
  }
  result
}

/// Upstream `URLBase.split_path()`: the raw URL path split on
/// `[ \t\r\n,\\/]+`, each entry percent-decoded afterwards (so an encoded
/// `%2F` or `%2C` stays inside its entry).
pub fn split_path(url: &ParsedUrl) -> Vec<String> {
  url.path.split(['/', ' ', '\t', '\r', '\n', ',', '\\']).filter(|s| !s.is_empty()).map(unquote).collect()
}

/// Percent-decode like Python's `urllib.parse.unquote` (invalid UTF-8 is
/// replaced and `+` is left alone).
pub fn unquote(s: &str) -> String {
  String::from_utf8_lossy(&urlencoding::decode_binary(s.as_bytes())).into_owned()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_bool_or() {
    assert!(parse_bool_or(Some("yes"), false));
    assert!(parse_bool_or(Some("Yeah"), false));
    assert!(parse_bool_or(Some("1"), false));
    assert!(!parse_bool_or(Some("no"), true));
    assert!(!parse_bool_or(Some("False"), true));
    assert!(!parse_bool_or(Some("0"), true));
    assert!(parse_bool_or(Some("random"), true));
    assert!(!parse_bool_or(Some("random"), false));
    assert!(parse_bool_or(Some(""), true));
    assert!(parse_bool_or(None, true));
  }

  #[test]
  fn test_parse_list() {
    assert_eq!(parse_list(["c, a;b", "a"], true, true), vec!["a", "b", "c"]);
    assert_eq!(parse_list(["c a,c"], true, false), vec!["c", "a", "c"]);
    assert_eq!(parse_list(["a b, c"], false, false), vec!["a b", "c"]);
    assert!(parse_list(Vec::<String>::new(), true, true).is_empty());
  }

  #[test]
  fn test_split_path() {
    let url = ParsedUrl::parse("json://host/a,b/c%2Fd//e%20f").unwrap();
    assert_eq!(split_path(&url), vec!["a", "b", "c/d", "e f"]);
  }
}
