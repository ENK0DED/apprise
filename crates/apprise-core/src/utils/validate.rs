use std::net::IpAddr;

/// Check if a string is a valid hostname.
///
/// Rules:
/// - Only alphanumeric, hyphens, and dots allowed
/// - Labels separated by dots, each 1-63 chars
/// - No leading or trailing hyphens per label
/// - Not all digits (that would be ambiguous with an IP)
pub fn is_hostname(hostname: &str) -> bool {
  if hostname.is_empty() {
    return false;
  }

  let labels: Vec<&str> = hostname.split('.').collect();
  if labels.is_empty() {
    return false;
  }

  // Must have at least one label
  for label in &labels {
    if label.is_empty() || label.len() > 63 {
      return false;
    }
    if label.starts_with('-') || label.ends_with('-') {
      return false;
    }
    if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
      return false;
    }
  }

  // Not all digits (to avoid confusion with IP addresses)
  let all_digits = hostname.chars().filter(|c| *c != '.').all(|c| c.is_ascii_digit());
  if all_digits {
    return false;
  }

  true
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

/// Check if a string is a valid IPv4 or IPv6 address.
pub fn is_ipaddr(addr: &str) -> bool {
  addr.parse::<IpAddr>().is_ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  // ── is_hostname ──────────────────────────────────────────────

  #[test]
  fn test_hostname_valid() {
    assert!(is_hostname("example.com"));
    assert!(is_hostname("sub.example.com"));
    assert!(is_hostname("my-host"));
    assert!(is_hostname("a"));
    assert!(is_hostname("localhost"));
    assert!(is_hostname("my-host.example.co.uk"));
  }

  #[test]
  fn test_hostname_invalid() {
    assert!(!is_hostname(""));
    assert!(!is_hostname("-example.com"));
    assert!(!is_hostname("example-.com"));
    assert!(!is_hostname("exam ple.com"));
    assert!(!is_hostname("exam_ple.com"));
    assert!(!is_hostname("123.456")); // all digits
    assert!(!is_hostname("123"));
  }

  #[test]
  fn test_hostname_label_too_long() {
    let long_label = "a".repeat(64);
    assert!(!is_hostname(&format!("{}.com", long_label)));
  }

  #[test]
  fn test_hostname_max_label_length() {
    let label = "a".repeat(63);
    assert!(is_hostname(&format!("{}.com", label)));
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
    assert!(!is_email("user@123.456")); // domain is all digits
    assert!(!is_email("user@-example.com"));
  }

  // ── is_ipaddr ────────────────────────────────────────────────

  #[test]
  fn test_ipaddr_valid_ipv4() {
    assert!(is_ipaddr("127.0.0.1"));
    assert!(is_ipaddr("0.0.0.0"));
    assert!(is_ipaddr("255.255.255.255"));
    assert!(is_ipaddr("192.168.1.1"));
  }

  #[test]
  fn test_ipaddr_valid_ipv6() {
    assert!(is_ipaddr("::1"));
    assert!(is_ipaddr("fe80::1"));
    assert!(is_ipaddr("2001:db8::1"));
    assert!(is_ipaddr("::"));
  }

  #[test]
  fn test_ipaddr_invalid() {
    assert!(!is_ipaddr(""));
    assert!(!is_ipaddr("example.com"));
    assert!(!is_ipaddr("999.999.999.999"));
    assert!(!is_ipaddr("not-an-ip"));
    assert!(!is_ipaddr("192.168.1"));
  }
}
