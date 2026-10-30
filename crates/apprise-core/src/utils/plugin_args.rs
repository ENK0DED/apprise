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

/// A query argument read the way upstream `parse_qsd()` reads it: the key
/// matched case-insensitively and the value percent-decoded with `+` kept
/// as a literal `+` (`ParsedUrl::qsd` decodes it as a space), then trimmed.
/// The last occurrence wins.
pub fn qsd_raw(url: &ParsedUrl, key: &str) -> Option<String> {
  let (_, query) = url.raw.split_once('?')?;
  let wanted = key.to_lowercase();
  let mut found = None;
  for pair in query.split(['&', ';']) {
    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
    let mut chars = name.chars();
    let Some(first) = chars.next() else { continue };
    let name = unquote(&format!("{}{}", first, chars.as_str().replace('+', " ")));
    if name.trim().to_lowercase() == wanted {
      found = Some(unquote(value).trim().to_string());
    }
  }
  found
}

/// Upstream's `qsd+` / `qsd-` / `qsd:`: the query arguments whose key
/// starts with `prefix` (`+`, `-` or `:`), in URL order with the prefix
/// removed and the key's case kept. A `+` key also matches the space a
/// form-encoded `+` becomes. Values are percent-decoded with `+` kept
/// literal, and trimmed; a repeated key keeps its first position and its
/// last value.
pub fn prefixed_args(url: &ParsedUrl, prefix: char) -> Vec<(String, String)> {
  let Some((_, query)) = url.raw.split_once('?') else { return Vec::new() };
  let mut out: Vec<(String, String)> = Vec::new();
  for pair in query.split(['&', ';']) {
    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
    let mut chars = name.chars();
    let Some(first) = chars.next() else { continue };
    let key = unquote(&format!("{}{}", first, chars.as_str().replace('+', " ")));
    let stripped = match prefix {
      '+' => key.strip_prefix('+').or_else(|| key.strip_prefix(' ')),
      p => key.strip_prefix(p),
    };
    let Some(key) = stripped else { continue };
    let value = unquote(value).trim().to_string();
    match out.iter_mut().find(|(k, _)| k == key) {
      Some(entry) => entry.1 = value,
      None => out.push((key.to_string(), value)),
    }
  }
  out
}

/// The full email address in `address` (upstream `is_email()["full_email"]`).
///
/// Like upstream's (unanchored) regular expression, a leading display name
/// (`Name <user@example.com>`) and anything after the domain are dropped.
pub fn email_address(address: &str) -> Option<String> {
  let at = address.find('@')?;
  let is_sep = |c: char| matches!(c, ':' | '<' | '\'' | '"') || c.is_whitespace();

  // The display name is greedy, so the latest separator is tried first
  let mut starts: Vec<usize> = Vec::new();
  if let Some((i, c)) = address[..at].char_indices().rev().find(|(_, c)| is_sep(*c)) {
    starts.push(i + c.len_utf8());
  }
  starts.push(0);

  for start in starts {
    let s = &address[start..];
    // An optional "label+" prefix comes first
    if let Some(plus) = s.find('+')
      && (1..=128).contains(&plus)
      && !s[..plus].chars().any(char::is_whitespace)
      && let Some(len) = email_len(&s[plus + 1..])
    {
      return Some(s[..plus + 1 + len].to_string());
    }
    if let Some(len) = email_len(s) {
      return Some(s[..len].to_string());
    }
  }
  None
}

/// Length of the `userid@domain` match at the start of `s`, or `None`.
fn email_len(s: &str) -> Option<usize> {
  const USER: &str = "_!#$%&*/=?%`{|}~^-";
  let at = s.find('@')?;
  let user = &s[..at];
  let mut segments = user.split('.');
  let first = segments.next()?;
  let ok_char = |c: char, extra: &str| c.is_ascii_alphanumeric() || USER.contains(c) || extra.contains(c);
  if first.is_empty() || !first.chars().all(|c| ok_char(c, "")) {
    return None;
  }
  if !segments.all(|seg| !seg.is_empty() && seg.chars().all(|c| ok_char(c, "'"))) {
    return None;
  }
  Some(at + 1 + domain_len(&s[at + 1..])?)
}

/// Length of the longest domain match at the start of `s`: dotted labels
/// whose last label has at least 2 characters, or one label of 6+.
fn domain_len(s: &str) -> Option<usize> {
  let run: String = s.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
  let label_ok = |l: &str, min: usize| {
    l.len() >= min
      && l.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
      && l.chars().last().is_some_and(|c| c.is_ascii_alphanumeric())
      && l.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
  };
  // Dotted form: try the longest prefix first
  for end in (1..=run.len()).rev() {
    let candidate = &run[..end];
    let labels: Vec<&str> = candidate.split('.').collect();
    if labels.len() >= 2 && labels[..labels.len() - 1].iter().all(|l| label_ok(l, 1)) && label_ok(labels[labels.len() - 1], 2) {
      return Some(end);
    }
  }
  // Single label form
  let single: String = run.chars().take_while(|c| *c != '.').collect();
  if single.len() >= 6 && single.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()) {
    return Some(single.len());
  }
  None
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
  fn test_qsd_raw() {
    let url = ParsedUrl::parse("json://host?Key=a+b%2Bc&x=1&key=%20d+e%20&empty").unwrap();
    assert_eq!(qsd_raw(&url, "key").as_deref(), Some("d+e"));
    assert_eq!(qsd_raw(&url, "X").as_deref(), Some("1"));
    assert_eq!(qsd_raw(&url, "empty").as_deref(), Some(""));
    assert_eq!(qsd_raw(&url, "missing"), None);
    assert_eq!(qsd_raw(&ParsedUrl::parse("json://host/a").unwrap(), "a"), None);
  }

  #[test]
  fn test_prefixed_args() {
    let url = ParsedUrl::parse("json://host?+key1=val1&+Key2=Val+2&%2Bkey3=a%20b&-del=x&:c=1&:c=2&plain=y").unwrap();
    let pairs = |v: &[(&str, &str)]| v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
    assert_eq!(prefixed_args(&url, '+'), pairs(&[("key1", "val1"), ("Key2", "Val+2"), ("key3", "a b")]));
    assert_eq!(prefixed_args(&url, '-'), pairs(&[("del", "x")]));
    assert_eq!(prefixed_args(&url, ':'), pairs(&[("c", "2")]));
  }

  #[test]
  fn test_email_address() {
    assert_eq!(email_address("user@example.com").as_deref(), Some("user@example.com"));
    assert_eq!(email_address("label+user@example.com").as_deref(), Some("label+user@example.com"));
    assert_eq!(email_address("Name <user@example.com>").as_deref(), Some("user@example.com"));
    assert_eq!(email_address("user@localhost1").as_deref(), Some("user@localhost1"));
    assert_eq!(email_address("@invalid-subscriber"), None);
    assert_eq!(email_address("user@short"), None);
    assert_eq!(email_address("invalid"), None);
    assert_eq!(email_address("a+b+c@d.com"), None);
    // Matches upstream's unanchored expression
    assert_eq!(email_address("user@example.c").as_deref(), Some("user@example"));
    assert_eq!(email_address("user@example.com/x").as_deref(), Some("user@example.com"));
    assert_eq!(email_address("us.er@ex-ample.co.uk").as_deref(), Some("us.er@ex-ample.co.uk"));
    assert_eq!(email_address("o'k@x.com").as_deref(), Some("k@x.com"));
  }

  #[test]
  fn test_split_path() {
    let url = ParsedUrl::parse("json://host/a,b/c%2Fd//e%20f").unwrap();
    assert_eq!(split_path(&url), vec!["a", "b", "c/d", "e f"]);
  }
}
