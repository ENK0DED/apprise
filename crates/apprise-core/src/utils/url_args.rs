//! Upstream's port check for host-verified URLs, which `ParsedUrl` does not
//! expose because it keeps a URL with an unreadable port loadable.

/// Whether the authority of the raw URL carries a port that upstream
/// `parse_url()` rejects for host-verified URLs, such as `host:` or
/// `host:garbage`. `ParsedUrl` keeps those as part of the host instead.
pub fn has_invalid_port(raw: &str) -> bool {
  let Some((_, rest)) = raw.split_once("://") else { return false };
  let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
  let host_port = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
  let after_host = match host_port.strip_prefix('[') {
    Some(v6) => v6.split_once(']').map(|(_, r)| r).unwrap_or(""),
    None => host_port.find(':').map(|i| &host_port[i..]).unwrap_or(""),
  };
  match after_host.strip_prefix(':') {
    Some(port) => !matches!(port.parse::<u32>(), Ok(1..=65535)),
    None => false,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_has_invalid_port() {
    assert!(!has_invalid_port("growl://localhost"));
    assert!(!has_invalid_port("growl://localhost:2000/path"));
    assert!(!has_invalid_port("xmpp://user:pass@localhost/target"));
    assert!(!has_invalid_port("xmpp://[::1]:5222"));
    assert!(has_invalid_port("growl://localhost:"));
    assert!(has_invalid_port("growl://localhost:garbage"));
    assert!(has_invalid_port("growl://localhost:0"));
    assert!(has_invalid_port("growl://localhost:70000"));
  }
}
