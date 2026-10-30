//! Host and address checks, ported from upstream `apprise/utils/parse.py`
//! (`is_hostname()` and `is_ipaddr()`).

use std::sync::OnceLock;

use regex_lite::Regex;

/// Upstream's IPv4 expression; it accepts leading zeros (`010.0.0.1`).
fn ipv4_re() -> &'static Regex {
  static RE: OnceLock<Regex> = OnceLock::new();
  RE.get_or_init(|| Regex::new(r"^((25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)$").unwrap())
}

/// Upstream's IPv6 expression, anchored at both ends (upstream 4f69f00a).
fn ipv6_re() -> &'static Regex {
  static RE: OnceLock<Regex> = OnceLock::new();
  RE.get_or_init(|| {
    Regex::new(concat!(
      r"(?i)^(([0-9a-f]{1,4}:){7,7}[0-9a-f]{1,4}|([0-9a-f]{1,4}:)",
      r"{1,7}:|([0-9a-f]{1,4}:){1,6}:[0-9a-f]{1,4}|([0-9a-f]{1,4}:){1,5}",
      r"(:[0-9a-f]{1,4}){1,2}|([0-9a-f]{1,4}:){1,4}",
      r"(:[0-9a-f]{1,4}){1,3}|([0-9a-f]{1,4}:){1,3}",
      r"(:[0-9a-f]{1,4}){1,4}|([0-9a-f]{1,4}:){1,2}",
      r"(:[0-9a-f]{1,4}){1,5}|[0-9a-f]{1,4}:",
      r"((:[0-9a-f]{1,4}){1,6})|:((:[0-9a-f]{1,4}){1,7}|:)|",
      r"fe80:(:[0-9a-f]{0,4}){0,4}%[0-9a-z]{1,}|::",
      r"(ffff(:0{1,4}){0,1}:){0,1}((25[0-5]",
      r"|(2[0-4]|1{0,1}[0-9]){0,1}[0-9])\.){3,3}(25[0-5]|(2[0-4]|",
      r"1{0,1}[0-9]){0,1}[0-9])|([0-9a-f]{1,4}:){1,4}:((25[0-5]|",
      r"(2[0-4]|1{0,1}[0-9]){0,1}[0-9])\.){3,3}(25[0-5]|(2[0-4]|",
      r"1{0,1}[0-9]){0,1}[0-9]))$",
    ))
    .unwrap()
  })
}

/// Validate an IPv4 or IPv6 address.
///
/// Returns the address on success; an IPv6 address is always returned in
/// square brackets, as a URL needs it (RFC 2732). Brackets on the input
/// must come in pairs.
pub fn verify_ipaddr(addr: &str, ipv4: bool, ipv6: bool) -> Option<String> {
  if ipv4 && ipv4_re().is_match(addr) {
    return Some(addr.to_string());
  }

  if ipv6 {
    let ip = match (addr.starts_with('['), addr.ends_with(']')) {
      (true, true) if addr.len() >= 2 => &addr[1..addr.len() - 1],
      (false, false) => addr,
      // Brackets have to come in pairs
      _ => return None,
    };
    if ipv6_re().is_match(ip) {
      return Some(format!("[{}]", ip));
    }
  }

  None
}

/// Check if a string is a valid IPv4 or IPv6 address.
pub fn is_ipaddr(addr: &str) -> bool {
  verify_ipaddr(addr, true, true).is_some()
}

/// Validate a hostname the way upstream `is_hostname()` does.
///
/// Labels may start with a digit and may contain underscores when
/// `underscore` is set (the Docker workaround upstream keeps). A host that
/// is not a valid name may still be a valid IP address. Returns the host
/// without a trailing dot, or the bracketed form of an IPv6 address.
pub fn verify_hostname(hostname: &str, ipv4: bool, ipv6: bool, underscore: bool) -> Option<String> {
  if hostname.is_empty() || hostname.len() > 253 {
    return None;
  }

  let hostname = hostname.strip_suffix('.').unwrap_or(hostname);
  let labels: Vec<&str> = hostname.split('.').collect();

  if labels.len() == 4 && !hostname.is_empty() && hostname.chars().all(|c| c.is_ascii_digit() || c == '.') {
    return verify_ipaddr(hostname, ipv4, false);
  }

  let valid_label = |label: &str| -> bool {
    let extra = |c: char| c.is_ascii_alphanumeric() || c == '-' || (underscore && c == '_');
    let bytes = label.as_bytes();
    match bytes.len() {
      // A single character must be a letter (or "_"/"-", which the trailing
      // check below then rejects anyway).
      1 => bytes[0].is_ascii_alphabetic(),
      2..=63 => {
        let first = label.chars().next().unwrap();
        let last = label.chars().last().unwrap();
        first.is_ascii_alphanumeric() && label.chars().all(extra) && last != '-' && last != '_'
      }
      _ => false,
    }
  };

  if !labels.iter().all(|l| valid_label(l)) {
    return verify_ipaddr(hostname, ipv4, ipv6);
  }

  Some(hostname.to_string())
}

/// Check if a string is a valid hostname or IP address (upstream defaults:
/// IPv4, IPv6 and underscores allowed).
pub fn is_hostname(hostname: &str) -> bool {
  verify_hostname(hostname, true, true, true).is_some()
}

/// Check if a string is a valid email address (simple check).
///
/// Format: `local@domain` where local is non-empty and domain is a valid hostname.
pub fn is_email(address: &str) -> bool {
  if let Some(at_pos) = address.rfind('@') {
    let local = &address[..at_pos];
    let domain = &address[at_pos + 1..];
    !local.is_empty() && is_hostname(domain)
  } else {
    false
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // ── is_hostname (upstream tests/test_apprise_utils.py::test_is_hostname) ──

  #[test]
  fn test_hostname_valid() {
    assert_eq!(verify_hostname("yahoo.ca", true, true, true).as_deref(), Some("yahoo.ca"));
    assert_eq!(verify_hostname("yahoo.ca.", true, true, true).as_deref(), Some("yahoo.ca"));
    assert_eq!(verify_hostname("valid-dashes-in-host.ca", true, true, true).as_deref(), Some("valid-dashes-in-host.ca"));
    assert!(is_hostname("my-host.example.co.uk"));
    assert!(is_hostname("a"));
    assert!(is_hostname("localhost"));
    // Underscores are allowed by default (docker compose workaround)
    assert_eq!(verify_hostname("valid_underscores_in_host.ca", true, true, true).as_deref(), Some("valid_underscores_in_host.ca"));
    assert!(verify_hostname("valid_underscores_in_host.ca", true, true, false).is_none());
    // RFC 1123 permits labels to start with digits
    assert!(is_hostname("123abc.com"));
    assert!(is_hostname("123.456"));
    assert!(is_hostname("5t4m59hl-34343.euw.devtunnels.ms"));
  }

  #[test]
  fn test_hostname_invalid() {
    assert!(!is_hostname(""));
    assert!(!is_hostname("-example.com"));
    assert!(!is_hostname("example-.com"));
    assert!(!is_hostname("_example.com"));
    assert!(!is_hostname("example_.com"));
    assert!(!is_hostname("exam ple.com"));
    assert!(!is_hostname("invalid-characters-#.ca"));
    assert!(!is_hostname("1.example.com"));
    assert!(!is_hostname("a..b"));
    assert!(!is_hostname(&"a".repeat(254)));
  }

  #[test]
  fn test_hostname_label_lengths() {
    assert!(!is_hostname(&format!("{}.com", "a".repeat(64))));
    assert!(is_hostname(&format!("{}.com", "a".repeat(63))));
  }

  #[test]
  fn test_hostname_ip_fallback() {
    assert_eq!(verify_hostname("127.0.0.1", true, true, true).as_deref(), Some("127.0.0.1"));
    assert!(verify_hostname("127.0.0.1", false, true, true).is_none());
    assert!(verify_hostname("0.0.0.256", true, true, true).is_none());
    assert_eq!(verify_hostname("[2001:db8::1]", true, true, true).as_deref(), Some("[2001:db8::1]"));
    assert_eq!(verify_hostname("::1", true, true, true).as_deref(), Some("[::1]"));
    assert!(verify_hostname("[2001:db8::1]", true, false, true).is_none());
  }

  // ── is_ipaddr (upstream test_is_ipaddr / compressed IPv6, 4f69f00a) ──

  #[test]
  fn test_ipaddr_ipv4() {
    for ip in ["127.0.0.1", "0.0.0.0", "255.255.255.255", "192.168.1.1", "010.0.0.1"] {
      assert_eq!(verify_ipaddr(ip, true, true).as_deref(), Some(ip), "{}", ip);
    }
    for ip in ["", "example.com", "999.999.999.999", "192.168.1", "1.2.3.4.5", "256.0.0.1"] {
      assert!(!is_ipaddr(ip), "{}", ip);
    }
  }

  #[test]
  fn test_ipaddr_ipv6() {
    for ip in ["::1", "fe80::1", "2001:db8::1", "::", "2001:0db8:85a3:0000:0000:8a2e:0370:7334", "::ffff:192.0.2.128", "fe80::7:8%eth0"] {
      assert_eq!(verify_ipaddr(ip, true, true), Some(format!("[{}]", ip)), "{}", ip);
    }
    assert_eq!(verify_ipaddr("[2001:db8::1]", true, true).as_deref(), Some("[2001:db8::1]"));
    // A compressed address must never come back shortened
    assert!(verify_ipaddr("2001:db8::1::2", true, true).is_none());
    assert!(verify_ipaddr("[2001:db8::1", true, true).is_none());
    assert!(verify_ipaddr("2001:db8::1]", true, true).is_none());
    assert!(verify_ipaddr("12345::", true, true).is_none());
    assert!(verify_ipaddr("::1", true, false).is_none());
  }

  // ── is_email ─────────────────────────────────────────────────

  #[test]
  fn test_email_valid() {
    assert!(is_email("user@example.com"));
    assert!(is_email("user.name@example.com"));
    assert!(is_email("user+tag@example.co.uk"));
    assert!(is_email("a@b.c"));
  }

  #[test]
  fn test_email_invalid() {
    assert!(!is_email(""));
    assert!(!is_email("user"));
    assert!(!is_email("@example.com"));
    assert!(!is_email("user@"));
    assert!(!is_email("user@-example.com"));
  }
}
