//! Web Key Directory (WKD) lookups (upstream `apprise/utils/wkd.py`).
//!
//! WKD lets a sender fetch a recipient's OpenPGP public key from a
//! well-known HTTPS location on the recipient's mail domain (RFC 9080,
//! draft-koch-openpgp-webkey-service). Two forms are tried, in order:
//!
//! - advanced: `https://openpgpkey.{domain}/.well-known/openpgpkey/{domain}/hu/{hash}?l={local}`
//! - direct: `https://{domain}/.well-known/openpgpkey/hu/{hash}?l={local}`
//!
//! `{hash}` is the z-base32 encoded SHA-1 of the lowercased local part.
//! Requests (and every redirect) must stay on HTTPS and reach public IP
//! addresses only, as upstream's `HTTPPolicySession` enforces. Keys are
//! returned as raw bytes; parsing them is left to [`crate::utils::pgp`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};

use crate::utils::http::{is_public_ip_address, is_secure_http_url, read_bounded_response};
use crate::utils::parse::quote;
use crate::utils::validate::verify_hostname;

/// The z-base32 alphabet (RFC 6189).
const ZB32: &[u8; 32] = b"ybndrfg8ejkmcpqxot1uwisza345h769";

/// Largest WKD response accepted (64 KiB).
pub const MAX_RESPONSE_SIZE: usize = 65536;

/// How long fetched keys stay cached (24 hours).
pub const CACHE_EXPIRY: Duration = Duration::from_secs(60 * 60 * 24);

/// Most redirects followed for one lookup.
const MAX_REDIRECTS: usize = 30;

/// Key material by lowercased address, with its expiry.
type Cache = Arc<Mutex<HashMap<String, (Vec<u8>, Instant)>>>;

/// Fetches OpenPGP key material over WKD and caches it in memory (upstream
/// `AppriseWKDController`). Clones share the cache.
#[derive(Debug, Clone)]
pub struct WkdController {
  verify_certificate: bool,
  cache: Cache,
  /// Require HTTPS and public addresses (always on outside tests).
  strict: bool,
  /// Tests send every lookup to this base instead (`{base}/{host}/path`).
  #[cfg(test)]
  base: Option<String>,
}

impl WkdController {
  /// A controller that verifies certificates unless `verify_certificate`
  /// is false (`?verify=no`). Timeouts and redirect handling follow the
  /// service's `?cto=`, `?rto=` and `?redirect=` options.
  pub fn new(verify_certificate: bool) -> Self {
    Self {
      verify_certificate,
      cache: Arc::new(Mutex::new(HashMap::new())),
      strict: true,
      #[cfg(test)]
      base: None,
    }
  }

  /// A controller that sends its requests to a local mock server.
  #[cfg(test)]
  pub(crate) fn for_test(base: &str) -> Self {
    Self { strict: false, base: Some(base.trim_end_matches('/').to_string()), ..Self::new(true) }
  }

  /// The z-base32 encoding of `data` (no padding; 32 characters for a
  /// SHA-1 digest).
  pub fn zb32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 8 / 5 + 1);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &byte in data {
      acc = (acc << 8) | u32::from(byte);
      bits += 8;
      while bits >= 5 {
        bits -= 5;
        out.push(ZB32[((acc >> bits) & 0x1f) as usize] as char);
      }
      acc &= (1 << bits) - 1;
    }
    if bits > 0 {
      out.push(ZB32[((acc << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
  }

  /// The advanced and direct lookup URLs for `email`, or `None` when it is
  /// not a usable address. The local part keeps its case in `?l=`; only
  /// the hash uses the lowercased form.
  pub fn wkd_urls(email: &str) -> Option<(String, String)> {
    let (local_orig, domain) = email.split_once('@')?;
    let local = local_orig.to_lowercase();
    let domain = domain.to_lowercase();
    if local.is_empty() || domain.is_empty() {
      return None;
    }
    // A crafted user@legit.com@attacker.test must not reach attacker.test
    // with legit.com as credentials
    verify_hostname(&domain, false, false, false)?;
    let hash = Self::zb32_encode(&Sha1::digest(local.as_bytes()));
    let l = quote(local_orig, "/");
    Some((
      format!("https://openpgpkey.{domain}/.well-known/openpgpkey/{domain}/hu/{hash}?l={l}"),
      format!("https://{domain}/.well-known/openpgpkey/hu/{hash}?l={l}"),
    ))
  }

  /// The key material published for `email`: the advanced method first,
  /// then the direct one. Results are cached for [`CACHE_EXPIRY`].
  pub async fn fetch(&self, email: &str) -> Option<Vec<u8>> {
    if email.is_empty() || !email.contains('@') {
      return None;
    }
    let key = email.trim().to_lowercase();
    if let Some((data, expires)) = self.lock().get(&key)
      && *expires > Instant::now()
    {
      tracing::debug!("WKD cache hit for {}", key);
      return Some(data.clone());
    }
    let (advanced, direct) = Self::wkd_urls(email.trim())?;
    for url in [advanced, direct] {
      if let Some(data) = self.get(&url).await {
        tracing::debug!("WKD key fetched for {} ({} bytes)", key, data.len());
        self.lock().insert(key, (data.clone(), Instant::now() + CACHE_EXPIRY));
        return Some(data);
      }
    }
    tracing::debug!("No WKD key found for {}", key);
    None
  }

  /// One GET: the body of a 200 response that looks like OpenPGP data and
  /// fits [`MAX_RESPONSE_SIZE`], otherwise `None`.
  async fn get(&self, url: &str) -> Option<Vec<u8>> {
    tracing::debug!("WKD GET {}", url);
    let target = self.request_url(url);
    let client = match self.client().build() {
      Ok(c) => c,
      Err(e) => {
        tracing::debug!("WKD request failed for {}: {}", url, crate::error::loggable_http_error(&e));
        return None;
      }
    };
    let response = match client.get(&target).header("User-Agent", crate::notify::APP_ID).send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::debug!("WKD request failed for {}: {}", url, crate::error::loggable_http_error(&e));
        return None;
      }
    };
    if response.status() != reqwest::StatusCode::OK {
      tracing::debug!("WKD returned HTTP {} for {}", response.status().as_u16(), url);
      return None;
    }
    // A redirect must not end on plain HTTP: the key would be exposed to
    // interception and the WKD trust model broken
    if self.strict && response.url().scheme() != "https" {
      tracing::debug!("WKD redirect ended on non-HTTPS URL for {}; skipping", url);
      return None;
    }
    let content = match read_bounded_response(response, MAX_RESPONSE_SIZE).await {
      Ok(Some(content)) => content,
      Ok(None) => {
        tracing::warn!("WKD response too large for {}; skipping", url);
        return None;
      }
      Err(e) => {
        tracing::debug!("WKD request failed for {}: {}", url, crate::error::loggable_http_error(&e));
        return None;
      }
    };
    if content.is_empty() {
      tracing::debug!("WKD returned empty body for {}", url);
      return None;
    }
    // Every binary OpenPGP packet has bit 7 set in its first byte and an
    // armored key starts with "-----"; an HTML error page served with 200
    // must not hide the direct-method fallback
    if content[0] & 0x80 == 0 && !content.starts_with(b"-----") {
      tracing::debug!("WKD response for {} is not PGP data; skipping", url);
      return None;
    }
    Some(content)
  }

  fn client(&self) -> reqwest::ClientBuilder {
    let follow = crate::notify::current_http_options().redirect.unwrap_or(true);
    let strict = self.strict;
    let redirect = if !follow {
      reqwest::redirect::Policy::none()
    } else {
      reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
          attempt.error("too many redirects")
        } else if strict && !redirect_allowed(attempt.url()) {
          attempt.error("WKD redirect left public HTTPS")
        } else {
          attempt.follow()
        }
      })
    };
    let builder = crate::notify::client_builder(self.verify_certificate, crate::notify::ClientDefaults::default()).redirect(redirect);
    if strict { builder.dns_resolver(PublicResolver) } else { builder }
  }

  #[cfg(test)]
  fn request_url(&self, url: &str) -> String {
    match &self.base {
      Some(base) => format!("{}/{}", base, url.trim_start_matches("https://")),
      None => url.to_string(),
    }
  }

  #[cfg(not(test))]
  fn request_url(&self, url: &str) -> String {
    url.to_string()
  }

  /// Drop expired cache entries.
  pub fn prune(&self) {
    let now = Instant::now();
    self.lock().retain(|_, (_, expires)| *expires > now);
  }

  fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Vec<u8>, Instant)>> {
    self.cache.lock().unwrap_or_else(|e| e.into_inner())
  }
}

/// A redirect target WKD may follow: HTTPS, and a public address when the
/// host is an IP literal (names are checked by [`PublicResolver`]).
fn redirect_allowed(url: &url::Url) -> bool {
  if !is_secure_http_url(url.as_str()) {
    return false;
  }
  match url.host() {
    Some(url::Host::Ipv4(ip)) => is_public_ip_address(&ip.to_string()),
    Some(url::Host::Ipv6(ip)) => is_public_ip_address(&ip.to_string()),
    Some(url::Host::Domain(_)) => true,
    None => false,
  }
}

/// Resolves names to their public addresses only, so a WKD host (or a
/// redirect) cannot point the lookup at an internal service.
struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
  fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
    let host = name.as_str().to_string();
    Box::pin(async move {
      let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.filter(|a| is_public_ip_address(&a.ip().to_string())).collect();
      if addrs.is_empty() {
        return Err(format!("{} has no public address", host).into());
      }
      Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use wiremock::matchers::{method, path, query_param};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  const KEY: &[u8] = b"\x99\x01\x0dbinary-key";

  #[test]
  fn test_zb32_encode() {
    // Expected values from upstream AppriseWKDController.zb32_encode()
    assert_eq!(WkdController::zb32_encode(b""), "");
    assert_eq!(WkdController::zb32_encode(&[0u8; 5]), "yyyyyyyy");
    assert_eq!(WkdController::zb32_encode(&[0xffu8; 5]), "99999999");
    assert_eq!(WkdController::zb32_encode(b"\x00"), "yy");
    assert_eq!(WkdController::zb32_encode(b"\xf0"), "6y");
    assert_eq!(WkdController::zb32_encode(&Sha1::digest(b"joe.doe")), "iy9q119eutrkn8s1mk4r39qejnbu3n5q");
    assert_eq!(WkdController::zb32_encode(&Sha1::digest(b"")).len(), 32);
  }

  #[test]
  fn test_wkd_urls() {
    // The RFC 9080 example
    let (advanced, direct) = WkdController::wkd_urls("Joe.Doe@Example.ORG").unwrap();
    assert_eq!(advanced, "https://openpgpkey.example.org/.well-known/openpgpkey/example.org/hu/iy9q119eutrkn8s1mk4r39qejnbu3n5q?l=Joe.Doe");
    assert_eq!(direct, "https://example.org/.well-known/openpgpkey/hu/iy9q119eutrkn8s1mk4r39qejnbu3n5q?l=Joe.Doe");

    // The local part is percent-encoded in ?l=
    let (advanced, _) = WkdController::wkd_urls("a b+c@example.com").unwrap();
    assert!(advanced.ends_with("?l=a%20b%2Bc"));

    for bad in ["", "user", "@example.com", "user@", "user@1.2.3.4", "user@legit.com@attacker.test", "user@exa mple.com", "user@-bad.com"] {
      assert!(WkdController::wkd_urls(bad).is_none(), "{}", bad);
    }
  }

  #[tokio::test]
  async fn test_fetch_advanced_then_direct() {
    let server = MockServer::start().await;
    let hash = "iy9q119eutrkn8s1mk4r39qejnbu3n5q";
    Mock::given(method("GET"))
      .and(path(format!("/openpgpkey.example.org/.well-known/openpgpkey/example.org/hu/{}", hash)))
      .respond_with(ResponseTemplate::new(404))
      .expect(1)
      .mount(&server)
      .await;
    Mock::given(method("GET"))
      .and(path(format!("/example.org/.well-known/openpgpkey/hu/{}", hash)))
      .and(query_param("l", "Joe.Doe"))
      .respond_with(ResponseTemplate::new(200).set_body_bytes(KEY))
      .expect(1)
      .mount(&server)
      .await;
    let wkd = WkdController::for_test(&server.uri());
    assert_eq!(wkd.fetch("Joe.Doe@example.org").await.as_deref(), Some(KEY));
    // Cached, case-insensitively: no further requests
    assert_eq!(wkd.fetch("joe.doe@EXAMPLE.org").await.as_deref(), Some(KEY));

    // An expired entry is fetched again; prune drops it
    wkd.lock().values_mut().for_each(|(_, expires)| *expires = Instant::now() - Duration::from_secs(1));
    wkd.prune();
    assert!(wkd.lock().is_empty());
  }

  #[tokio::test]
  async fn test_fetch_rejects_bad_responses() {
    let server = MockServer::start().await;
    let respond = |host: &str, response: ResponseTemplate| {
      Mock::given(method("GET")).and(wiremock::matchers::path_regex(format!("^/{}/.*", regex_lite::escape(host)))).respond_with(response)
    };
    // HTML served with 200, an empty body and an oversized key
    respond("openpgpkey.html.test", ResponseTemplate::new(200).set_body_string("<html>parked</html>")).mount(&server).await;
    respond("html.test", ResponseTemplate::new(200).set_body_string("{}")).mount(&server).await;
    respond("openpgpkey.empty.test", ResponseTemplate::new(200)).mount(&server).await;
    respond("empty.test", ResponseTemplate::new(500)).mount(&server).await;
    respond("openpgpkey.large.test", ResponseTemplate::new(200).set_body_bytes(vec![0x99u8; MAX_RESPONSE_SIZE + 1])).mount(&server).await;
    respond("large.test", ResponseTemplate::new(200).set_body_string("-----BEGIN PGP PUBLIC KEY BLOCK-----\n")).mount(&server).await;

    let wkd = WkdController::for_test(&server.uri());
    assert!(wkd.fetch("user@html.test").await.is_none());
    assert!(wkd.fetch("user@empty.test").await.is_none());
    // The armored direct-method answer is accepted after the oversized one
    assert_eq!(wkd.fetch("user@large.test").await.as_deref(), Some(&b"-----BEGIN PGP PUBLIC KEY BLOCK-----\n"[..]));
    assert!(wkd.fetch("no-at-sign").await.is_none());
    assert!(wkd.fetch("").await.is_none());
  }

  #[tokio::test]
  async fn test_fetch_refuses_non_public_and_plain_http() {
    // The production policy: the mock server is on 127.0.0.1, so neither
    // a direct request nor a redirect to it may succeed
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/key")).respond_with(ResponseTemplate::new(200).set_body_bytes(KEY)).mount(&server).await;
    let wkd = WkdController::new(true);
    assert!(wkd.get(&format!("{}/key", server.uri())).await.is_none());
    assert!(wkd.get("https://127.0.0.1:9/key").await.is_none());

    let Ok(base) = url::Url::parse("https://example.com/") else { panic!() };
    assert!(redirect_allowed(&base));
    for bad in ["http://example.com/", "https://127.0.0.1/", "https://[::1]/", "https://10.1.2.3/", "https://user@example.com/"] {
      assert!(!redirect_allowed(&url::Url::parse(bad).unwrap()), "{}", bad);
    }
    assert!(redirect_allowed(&url::Url::parse("https://8.8.8.8/").unwrap()));
  }

  #[tokio::test]
  async fn test_resolver_drops_private_addresses() {
    use reqwest::dns::Resolve;
    let name: reqwest::dns::Name = "localhost".parse().unwrap();
    assert!(PublicResolver.resolve(name).await.is_err());
  }
}
