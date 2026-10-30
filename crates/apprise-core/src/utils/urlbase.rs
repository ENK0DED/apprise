//! URL post-processing upstream `URLBase.parse_url()` applies before a
//! plugin sees the results.

use crate::utils::parse::ParsedUrl;

/// The user and password a URL carries once upstream's query overrides are
/// applied: `?password=` and then `?pass=` replace the password, `?user=`
/// replaces the user, and when that leaves no password the user written in
/// the URL becomes the password (`PASSWORD@host?user=name`).
pub fn url_credentials(url: &ParsedUrl) -> (Option<String>, Option<String>) {
  let mut password = url.password.clone();
  if let Some(p) = url.get("password") {
    password = Some(p.to_string());
  }
  if let Some(p) = url.get("pass") {
    password = Some(p.to_string());
  }

  let mut user = url.user.clone();
  if let Some(u) = url.get("user") {
    if password.is_none() {
      password = url.user.clone();
    }
    user = Some(u.to_string());
  }
  (user, password)
}

/// The host, percent-decoded as upstream's `unquote(results["host"])`
/// returns it (empty when the URL has none).
pub fn url_host(url: &ParsedUrl) -> String {
  let host = url.host.as_deref().unwrap_or_default();
  urlencoding::decode(host).map(|h| h.into_owned()).unwrap_or_else(|_| host.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn creds(url: &str) -> (Option<String>, Option<String>) {
    url_credentials(&ParsedUrl::parse(url).unwrap())
  }

  #[test]
  fn test_url_credentials() {
    let s = |v: &str| Some(v.to_string());
    assert_eq!(creds("bulksms://a:b@host"), (s("a"), s("b")));
    assert_eq!(creds("bulksms://host?user=a&password=b"), (s("a"), s("b")));
    assert_eq!(creds("bulksms://host?user=a&pass=b"), (s("a"), s("b")));
    assert_eq!(creds("bulksms://a@host?pass=b"), (s("a"), s("b")));
    // The URL user becomes the password when ?user= overrides it
    assert_eq!(creds("bulksms://b@host?user=a"), (s("a"), s("b")));
    assert_eq!(creds("bulksms://x:b@host?user=a"), (s("a"), s("b")));
    assert_eq!(creds("bulksms://host"), (None, None));
    assert_eq!(creds("bulksms://host?user=a"), (s("a"), None));
  }

  #[test]
  fn test_url_host() {
    assert_eq!(url_host(&ParsedUrl::parse("burstsms://a:b@%20").unwrap()), " ");
    assert_eq!(url_host(&ParsedUrl::parse("burstsms://a:b@My%20Sender/123").unwrap()), "My Sender");
    assert_eq!(url_host(&ParsedUrl::parse("burstsms://?key=a").unwrap()), "");
  }
}
