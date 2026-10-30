//! Email address helpers shared by the email plugins.
//!
//! Ports of upstream's `is_email()`, `parse_emails()` and `parse_list()`
//! (`apprise/utils/parse.py`) and of Python's `email.utils.formataddr()`,
//! so display names, labels and recipient lists parse exactly as upstream.

use regex_lite::Regex;
use std::sync::LazyLock;

/// Python's `\s` for str patterns: every character `str.isspace()` accepts.
const WS: &str = r"\t\n\x0B\x0C\r\x1C-\x20\x{85}\x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}";

/// `[a-z]` under Python's `re.IGNORECASE`, which also folds these four
/// non-ASCII letters onto ASCII ones.
const AZ: &str = r"a-z\x{130}\x{131}\x{17F}\x{212A}";

/// Upstream `GET_EMAIL_RE`, anchored the way `re.match()` anchors it.
static GET_EMAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
  let pattern = format!(
    concat!(
      r#"(?i)^(([{ws}"']{{0,32}})?(?P<name>[^:<'"]{{0,128}})?[:<{ws}'"]{{1,32}})?"#,
      r"(?P<full_email>((?P<label>[^+{ws}]{{1,128}})\+)?",
      r"(?P<email>(?P<userid>[{az}0-9_!#$%&*/=?%`{{|}}~^-]+",
      r"(?:\.[{az}0-9_!#$%&'*/=?%`{{|}}~^-]+)*)@(?P<domain>(",
      r"(?:[{az}0-9](?:[{az}0-9_-]*[{az}0-9])?\.)+",
      r"[{az}0-9](?:[{az}0-9_-]*[{az}0-9]))|",
      r"[{az}0-9][{az}0-9_-]{{5,}})))",
      r"[{ws}]*>?",
    ),
    ws = WS,
    az = AZ
  );
  Regex::new(&pattern).expect("GET_EMAIL_RE compiles")
});

/// Upstream `EMAIL_DETECTION_RE`. regex-lite has no look-ahead, so the
/// leading `(?=[^\s,;])` is folded into the first display-name character,
/// and the ReDoS guards (`{0,256}`) become plain repetitions because the
/// engine runs in linear time anyway.
static EMAIL_DETECTION_RE: LazyLock<Regex> = LazyLock::new(|| {
  let pattern = format!(
    concat!(
      r#"(?i)(?:(?:"(?:[^"\\]|\\.)*"|[^<>"@,;{ws}])(?:"(?:[^"\\]|\\.)*"|[^<>"@,;\r\n])*?)??"#,
      r"<[^@>\r\n]{{1,64}}@(?:[{az}0-9][{az}0-9_-]{{0,62}}\.){{1,10}}[{az}]{{2,63}}>",
      r"|",
      r"[^{ws}@,;\r\n][^@,;\r\n]*@(?:[{az}0-9][{az}0-9_-]{{0,62}}\.){{1,10}}[{az}]{{2,63}}",
    ),
    ws = WS,
    az = AZ
  );
  Regex::new(&pattern).expect("EMAIL_DETECTION_RE compiles")
});

/// Python's `str.isspace()`.
pub fn is_space(c: char) -> bool {
  c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

/// Upstream `STRING_DELIMITERS` (`[\[\];,\s]+`).
fn is_delimiter(c: char) -> bool {
  matches!(c, '[' | ']' | ';' | ',') || is_space(c)
}

/// An address accepted by [`is_email`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAddress {
  /// Display name (empty when none was given).
  pub name: String,
  /// The address without its label (`user@example.com`).
  pub email: String,
  /// The address including its label (`label+user@example.com`).
  pub full_email: String,
  /// The `label+` prefix, if any.
  pub label: String,
  /// The local part without the label.
  pub user: String,
  /// The domain.
  pub domain: String,
}

impl EmailAddress {
  /// The display name, or `None` when there is none (upstream stores `False`).
  pub fn display_name(&self) -> Option<String> {
    if self.name.is_empty() { None } else { Some(self.name.clone()) }
  }
}

/// Upstream `is_email()`: parse `Name <user@example.com>`, `Name:user@...`
/// and bare addresses.
pub fn is_email(address: &str) -> Option<EmailAddress> {
  let caps = GET_EMAIL_RE.captures(address)?;
  let group = |name: &str| caps.name(name).map(|m| m.as_str()).unwrap_or("");
  // Upstream strips " \t\r\n\x0b\x0c+" from these
  const STRIP: [char; 7] = [' ', '\t', '\r', '\n', '\x0b', '\x0c', '+'];
  Some(EmailAddress {
    name: group("name").trim_end_matches(STRIP).to_string(),
    email: group("email").to_string(),
    full_email: group("full_email").to_string(),
    label: group("label").trim_matches(STRIP).to_string(),
    user: group("userid").to_string(),
    domain: group("domain").to_string(),
  })
}

/// Upstream `parse_emails()` for one string: pull every address (with its
/// display name) out of a comma, semicolon or space separated list. Content
/// that holds no address is split on the usual delimiters instead, so callers
/// can report it as invalid.
pub fn parse_emails(arg: &str) -> Vec<String> {
  if arg.is_empty() {
    return Vec::new();
  }
  if arg.contains('@') {
    let found: Vec<String> = EMAIL_DETECTION_RE.find_iter(arg).map(|m| m.as_str().to_string()).collect();
    if !found.is_empty() {
      return found;
    }
  }
  arg.split(is_delimiter).filter(|s| !s.is_empty()).map(str::to_string).collect()
}

/// [`parse_emails`] over several strings (upstream passes lists through).
pub fn parse_emails_all<S: AsRef<str>>(args: &[S]) -> Vec<String> {
  args.iter().flat_map(|a| parse_emails(a.as_ref())).collect()
}

/// Run each entry through [`is_email`], dropping (and logging) the ones that
/// are not addresses. `kind` names the field in the log ("To", "Carbon Copy").
pub fn valid_addresses<S: AsRef<str>>(entries: &[S], kind: &str) -> Vec<EmailAddress> {
  entries
    .iter()
    .filter_map(|entry| {
      let parsed = is_email(entry.as_ref());
      if parsed.is_none() {
        tracing::warn!("Dropped invalid {} email ({}) specified.", kind, entry.as_ref());
      }
      parsed
    })
    .collect()
}

/// Upstream `parse_list()`: split on the usual delimiters and return the
/// unique entries, sorted.
pub fn parse_list<S: AsRef<str>>(args: &[S]) -> Vec<String> {
  let mut out: Vec<String> = args.iter().flat_map(|a| a.as_ref().split(is_delimiter)).filter(|s| !s.is_empty()).map(str::to_string).collect();
  out.sort();
  out.dedup();
  out
}

/// RFC 2047 `Q` encoding of a header value, as Python's email package
/// produces it once Apprise registers quoted-printable for UTF-8.
pub fn header_encode(value: &str) -> String {
  if value.is_empty() {
    return String::new();
  }
  let mut out = String::from("=?utf-8?q?");
  for b in value.bytes() {
    match b {
      b' ' => out.push('_'),
      b'-' | b'!' | b'*' | b'+' | b'/' | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => out.push(b as char),
      _ => out.push_str(&format!("={:02X}", b)),
    }
  }
  out.push_str("?=");
  out
}

/// Python's `email.utils.formataddr((name, address), charset="utf-8")`.
pub fn formataddr(name: Option<&str>, address: &str) -> String {
  let Some(name) = name.filter(|n| !n.is_empty()) else { return address.to_string() };
  if !name.is_ascii() {
    return format!("{} <{}>", header_encode(name), address);
  }
  let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
  if name.chars().any(|c| "[]\\()<>@,:;\".".contains(c)) { format!("\"{}\" <{}>", escaped, address) } else { format!("{} <{}>", escaped, address) }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_is_email_forms() {
    let e = is_email("user@example.com").unwrap();
    assert_eq!((e.name.as_str(), e.full_email.as_str(), e.domain.as_str()), ("", "user@example.com", "example.com"));

    let e = is_email("Chris Smith <chris@example.com>").unwrap();
    assert_eq!(e.name, "Chris Smith");
    assert_eq!(e.full_email, "chris@example.com");

    let e = is_email("Da:user5@example.com").unwrap();
    assert_eq!(e.name, "Da");
    assert_eq!(e.full_email, "user5@example.com");

    let e = is_email("\"Frodo\" <frodo@shire.me>").unwrap();
    assert_eq!(e.name, "Frodo");

    let e = is_email("label+user@example.com").unwrap();
    assert_eq!(e.label, "label");
    assert_eq!(e.email, "user@example.com");
    assert_eq!(e.full_email, "label+user@example.com");
    assert_eq!(e.user, "user");

    // A single-label domain needs at least six characters
    assert!(is_email("user@localhost").is_some());
    assert!(is_email("abcd@host").is_none());
    assert!(is_email("invalid").is_none());
    assert!(is_email("!invalid").is_none());
    assert!(is_email("\"@localhost.localdomain").is_none());
    assert!(is_email("").is_none());
  }

  #[test]
  fn test_parse_emails() {
    // Expected values come from upstream parse_emails()
    let cases: &[(&str, &[&str])] = &[
      ("user4@example.com,g@r@b,Da:user5@example.com", &["user4@example.com", "Da:user5@example.com"]),
      ("user3@example.com,i@v,User1:user1@example.com", &["user3@example.com", "User1:user1@example.com"]),
      ("Chris<l2g@nuxref.com>,plain@example.com", &["Chris<l2g@nuxref.com>", "plain@example.com"]),
      ("No One <noreply@yahoo.ca>", &["No One <noreply@yahoo.ca>"]),
      ("a@example.com b@example.com", &["a@example.com", "b@example.com"]),
      ("Chris Smith <l2g@nuxref.com>; \"Doe, Jane\" <jane@x.com> , bob@x.org", &["Chris Smith <l2g@nuxref.com>", "\"Doe, Jane\" <jane@x.com>", "bob@x.org"]),
      ("user1@example.com/invalid", &["user1@example.com"]),
      (" lead@x.com", &["lead@x.com"]),
      ("a@b.c d@e.fg", &["b.c d@e.fg"]),
      ("invalid-email, another", &["invalid-email", "another"]),
      ("garbage%", &["garbage%"]),
    ];
    for (input, expected) in cases {
      assert_eq!(parse_emails(input), expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "input: {}", input);
    }
    assert!(parse_emails("").is_empty());
  }

  #[test]
  fn test_is_email_matches_upstream() {
    // (input, name, full_email) from upstream is_email()
    let cases: &[(&str, &str, &str)] = &[
      ("b,Da:user5@example.com", "b,Da", "user5@example.com"),
      ("a@example.com b@example.com", "a@example.com", "b@example.com"),
      ("Name+ <a@b.com>", "Name", "a@b.com"),
      ("Reply+Guy<reply@example.com>", "Reply+Guy", "reply@example.com"),
      ("user@example.com garbage", "", "user@example.com"),
      ("=?utf-8?q?J=C3=B6rg?= <a@b.com>", "=?utf-8?q?J=C3=B6rg?=", "a@b.com"),
    ];
    for (input, name, full) in cases {
      let e = is_email(input).unwrap_or_else(|| panic!("should parse: {}", input));
      assert_eq!((e.name.as_str(), e.full_email.as_str()), (*name, *full), "input: {}", input);
    }
    for bad in ["g@r", "i@v", "\"\\\"Frodo\\\"\" <a@b.com>"] {
      assert!(is_email(bad).is_none(), "should not parse: {}", bad);
    }
  }

  #[test]
  fn test_parse_list_sorts_and_dedups() {
    assert_eq!(parse_list(&["b@x.com, a@x.com", "b@x.com;c@x.com"]), vec!["a@x.com", "b@x.com", "c@x.com"]);
    assert!(parse_list::<&str>(&[]).is_empty());
  }

  #[test]
  fn test_formataddr() {
    assert_eq!(formataddr(None, "a@b.com"), "a@b.com");
    assert_eq!(formataddr(Some(""), "a@b.com"), "a@b.com");
    assert_eq!(formataddr(Some("Chris Smith"), "a@b.com"), "Chris Smith <a@b.com>");
    assert_eq!(formataddr(Some("Dr. Who"), "a@b.com"), "\"Dr. Who\" <a@b.com>");
    assert_eq!(formataddr(Some("Jo \"J\" Doe"), "a@b.com"), "\"Jo \\\"J\\\" Doe\" <a@b.com>");
    assert_eq!(formataddr(Some("back\\slash"), "a@b.com"), "\"back\\\\slash\" <a@b.com>");
    assert_eq!(formataddr(Some("x y_z="), "a@b.com"), "x y_z= <a@b.com>");
    assert_eq!(formataddr(Some("Jörg ~ = ? _"), "a@b.com"), "=?utf-8?q?J=C3=B6rg_=7E_=3D_=3F_=5F?= <a@b.com>");
    assert_eq!(formataddr(Some("日本 語"), "a@b.com"), "=?utf-8?q?=E6=97=A5=E6=9C=AC_=E8=AA=9E?= <a@b.com>");
  }
}
