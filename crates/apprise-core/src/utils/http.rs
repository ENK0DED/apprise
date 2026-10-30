//! HTTP helpers for plugins (upstream `apprise/utils/http.py`): checks for
//! URLs taken from untrusted input, and response reads with a size cap.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Whether `value` is an ordinary public unicast IP address.
///
/// Private, loopback, link-local, shared (CGN), documentation, reserved,
/// unspecified and multicast addresses are refused. IPv6 addresses that
/// carry an IPv4 address (mapped, NAT64, 6to4) are judged by it, as upstream
/// does with Python's `ipaddress` rules.
pub fn is_public_ip_address(value: &str) -> bool {
  match value.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>() {
    Ok(IpAddr::V4(v4)) => is_public_v4(v4),
    Ok(IpAddr::V6(v6)) => is_public_v6(v6),
    Err(_) => false,
  }
}

fn in_v4(addr: u32, net: [u8; 4], bits: u32) -> bool {
  let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
  addr & mask == u32::from_be_bytes(net) & mask
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
  let a = u32::from(ip);
  // 192.0.0.9 and 192.0.0.10 are globally reachable despite their block.
  if a == u32::from_be_bytes([192, 0, 0, 9]) || a == u32::from_be_bytes([192, 0, 0, 10]) {
    return true;
  }
  const BLOCKED: &[([u8; 4], u32)] = &[
    ([0, 0, 0, 0], 8),
    ([10, 0, 0, 0], 8),
    ([100, 64, 0, 0], 10),
    ([127, 0, 0, 0], 8),
    ([169, 254, 0, 0], 16),
    ([172, 16, 0, 0], 12),
    ([192, 0, 0, 0], 24),
    ([192, 0, 2, 0], 24),
    ([192, 168, 0, 0], 16),
    ([198, 18, 0, 0], 15),
    ([198, 51, 100, 0], 24),
    ([203, 0, 113, 0], 24),
    ([224, 0, 0, 0], 4),
    ([240, 0, 0, 0], 4),
  ];
  !BLOCKED.iter().any(|(net, bits)| in_v4(a, *net, *bits))
}

fn in_v6(addr: u128, net: u128, bits: u32) -> bool {
  let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
  addr & mask == net & mask
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
  let a = u128::from(ip);
  // NAT64 and IPv4-mapped addresses inherit their IPv4 address's status.
  if in_v6(a, 0x0064_ff9b_0000_0000_0000_0000_0000_0000, 96) || in_v6(a, 0x0064_ff9b_0001_0000_0000_0000_0000_0000, 48) {
    return is_public_v4(Ipv4Addr::from(a as u32));
  }
  if let Some(v4) = ip.to_ipv4_mapped() {
    return is_public_v4(v4);
  }
  // Only global unicast (2000::/3) can be public at all.
  if !in_v6(a, 0x2000 << 112, 3) {
    return false;
  }
  // Globally reachable exceptions inside 2001::/23.
  const EXCEPTIONS: &[(u128, u32)] = &[
    (0x2001_0001_0000_0000_0000_0000_0000_0001, 128),
    (0x2001_0001_0000_0000_0000_0000_0000_0002, 128),
    (0x2001_0003 << 96, 32),
    (0x2001_0004_0112 << 80, 48),
    (0x2001_0020 << 96, 28),
    (0x2001_0030 << 96, 28),
  ];
  if EXCEPTIONS.iter().any(|(net, bits)| in_v6(a, *net, *bits)) {
    return true;
  }
  const BLOCKED: &[(u128, u32)] = &[
    (0x2001 << 112, 23),     // IETF protocol assignments, Teredo
    (0x2001_0db8 << 96, 32), // documentation
    (0x2002 << 112, 16),     // 6to4
    (0x3fff << 112, 20),     // documentation
  ];
  !BLOCKED.iter().any(|(net, bits)| in_v6(a, *net, *bits))
}

/// Whether `url` is safe to use as a request target: HTTPS, a valid host
/// name or IP address, no user name or password (even an empty one), a
/// usable port and no fragment. Private hosts are allowed, for self-hosted
/// services.
pub fn is_secure_http_url(url: &str) -> bool {
  // A fragment is never sent to the server; an empty one counts too.
  if url.contains('#') {
    return false;
  }
  let Some((scheme, rest)) = url.split_once("://") else { return false };
  if !scheme.eq_ignore_ascii_case("https") {
    return false;
  }
  let authority = rest.split(['/', '?']).next().unwrap_or("");
  if authority.is_empty() || authority.contains('@') {
    return false;
  }
  let Ok(parsed) = url::Url::parse(url) else { return false };
  if parsed.port() == Some(0) {
    return false;
  }
  match parsed.host() {
    Some(url::Host::Domain(host)) => is_hostname(host),
    Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_)) => true,
    None => false,
  }
}

/// Upstream `is_hostname()` with underscores allowed.
fn is_hostname(host: &str) -> bool {
  if host.is_empty() || host.len() > 253 {
    return false;
  }
  let host = host.strip_suffix('.').unwrap_or(host);
  host.split('.').all(|label| {
    let b = label.as_bytes();
    match b.len() {
      0 => false,
      1 => b[0].is_ascii_alphabetic(),
      n if n > 63 => false,
      _ => {
        b[0].is_ascii_alphanumeric() && b[1..].iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')) && !matches!(b[b.len() - 1], b'-' | b'_')
      }
    }
  })
}

/// Read a response body, giving up once it passes `max_bytes`.
///
/// Returns `Ok(None)` when the body is too large; nothing past the limit is
/// buffered.
pub async fn read_bounded_response(mut response: reqwest::Response, max_bytes: usize) -> Result<Option<Vec<u8>>, reqwest::Error> {
  if response.content_length().is_some_and(|len| len > max_bytes as u64) {
    return Ok(None);
  }
  let mut body = Vec::new();
  while let Some(chunk) = response.chunk().await? {
    if body.len() + chunk.len() > max_bytes {
      return Ok(None);
    }
    body.extend_from_slice(&chunk);
  }
  Ok(Some(body))
}

#[cfg(test)]
mod tests {
  use super::*;

  // Expected values come from upstream's helpers on Python 3.13.
  #[test]
  fn test_is_public_ip_address() {
    let cases = [
      ("8.8.8.8", true),
      ("10.0.0.1", false),
      ("127.0.0.1", false),
      ("169.254.1.1", false),
      ("100.64.0.1", false),
      ("100.128.0.1", true),
      ("192.0.2.1", false),
      ("198.18.0.1", false),
      ("224.0.0.1", false),
      ("240.0.0.1", false),
      ("255.255.255.255", false),
      ("0.0.0.0", false),
      ("192.168.1.1", false),
      ("172.16.0.1", false),
      ("172.32.0.1", true),
      ("::1", false),
      ("::", false),
      ("2001:4860:4860::8888", true),
      ("fe80::1", false),
      ("fc00::1", false),
      ("fec0::1", false),
      ("2001:db8::1", false),
      ("::ffff:8.8.8.8", true),
      ("::ffff:10.0.0.1", false),
      ("64:ff9b::808:808", true),
      ("64:ff9b::a00:1", false),
      ("2002:808:808::1", false),
      ("2001:0:4136:e378:8000:63bf:3fff:fdd2", false),
      ("ff02::1", false),
      ("3fff::1", false),
      ("2001:20::1", true),
      ("abc", false),
    ];
    for (ip, expected) in cases {
      assert_eq!(is_public_ip_address(ip), expected, "{ip}");
    }
  }

  #[test]
  fn test_is_secure_http_url() {
    let cases = [
      ("https://example.com/x", true),
      ("http://example.com", false),
      ("https://user@example.com", false),
      ("https://@example.com", false),
      ("https://:p@example.com", false),
      ("https://example.com:0/", false),
      ("https://example.com:99999/", false),
      ("https://example.com/#", false),
      ("https://ex_ample.com", true),
      ("https://exa mple.com", false),
      ("https://[::1]/", true),
      ("https://10.0.0.1:8443/x?y=1", true),
      ("HTTPS://Example.COM", true),
      ("https:///path", false),
      ("https://-bad.com", false),
    ];
    for (url, expected) in cases {
      assert_eq!(is_secure_http_url(url), expected, "{url}");
    }
  }

  #[tokio::test]
  async fn test_read_bounded_response() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    crate::ensure_crypto_provider();
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string("0123456789")).mount(&server).await;
    let get = || async { reqwest::get(server.uri()).await.unwrap() };
    assert_eq!(read_bounded_response(get().await, 10).await.unwrap(), Some(b"0123456789".to_vec()));
    assert_eq!(read_bounded_response(get().await, 9).await.unwrap(), None);
  }
}
