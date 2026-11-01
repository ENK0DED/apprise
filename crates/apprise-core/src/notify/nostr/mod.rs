//! Nostr notifications (upstream `apprise/plugins/nostr/`).
//!
//! Nostr is an open, decentralized social network. Messages are signed by
//! your own key and published to one or more relays (servers) that anyone
//! can run.
//!
//! - `nostr://{nsec}` posts a public note (kind 1).
//! - `nostr://{nsec}/{npub}/{name@example.com}` sends a private direct
//!   message to each recipient: NIP-17 by default (sealed and gift wrapped,
//!   sent to the relays the recipient lists), or the older NIP-04 with
//!   `?mode=nip04`.
//!
//! Attachments are uploaded to a Blossom media server and linked in the
//! message; files sent in NIP-17 direct messages are encrypted first.
//!
//! - <https://nostr.com/>
//! - <https://github.com/nostr-protocol/nips>
//! - <https://github.com/hzrd149/blossom>

mod crypto;
mod relay;
#[cfg(test)]
mod tests;

use crate::error::NotifyError;
use crate::notify::{APP_ID, Attachment, Notify, NotifyContext, NotifyFormat, ServiceDetails, build_client};
use crate::utils::parse::ParsedUrl;
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crypto::{Event, NostrKey, bech32_encode, decode_key, gift_wrap, is_hex_key, is_public_key, nip04_encrypt, rumor, verify_event};
use relay::{Filter, NostrRelay, RelayError, RelayOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// How direct messages are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
  /// Private messages that hide who is talking to whom (NIP-17).
  Nip17,
  /// Older encrypted messages understood by more clients (NIP-04).
  Nip04,
}

/// Well known public relays used when none are specified.
const NOSTR_DEFAULT_RELAYS: [&str; 3] = ["wss://relay.damus.io", "wss://nos.lol", "wss://relay.primal.net"];

/// A public Blossom server that accepts uploads from any key.
const NOSTR_DEFAULT_BLOSSOM: &str = "https://nostr.download";

// Event kinds used by this plugin
const NOSTR_KIND_NOTE: i64 = 1;
const NOSTR_KIND_NIP04: i64 = 4;
const NOSTR_KIND_CHAT: i64 = 14;
const NOSTR_KIND_FILE: i64 = 15;
const NOSTR_KIND_INBOX_RELAYS: i64 = 10050;
const NOSTR_KIND_BLOSSOM_SERVERS: i64 = 10063;
const NOSTR_KIND_BLOSSOM_AUTH: i64 = 24242;

/// How long a Blossom upload permission stays valid (in seconds).
const NOSTR_UPLOAD_AUTH_EXPIRY: i64 = 300;

/// The most relays we use from a recipient's list; NIP-17 asks for 1 to 3.
const NOSTR_MAX_INBOX_RELAYS: usize = 3;

/// The most we read from a Nostr address or Blossom server reply (in bytes).
const NOSTR_MAX_RESPONSE: usize = 1_048_576;

/// The longest download link (or relay URL) we accept.
const NOSTR_MAX_URL_LENGTH: usize = 2048;

/// Relay lists, Blossom servers and Nostr addresses are looked up again
/// after 6 hours so changes are picked up (upstream
/// `default_cache_expiry_sec`).
const CACHE_EXPIRY: Duration = Duration::from_secs(60 * 60 * 6);

/// Upstream `parse_list(..., sort=False)`: split on commas, semicolons,
/// square brackets and whitespace, keeping order and duplicates.
fn split_list(text: &str) -> impl Iterator<Item = &str> {
  text.split(|c: char| matches!(c, '[' | ']' | ';' | ',') || c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)).filter(|s| !s.is_empty())
}

/// Python's `str.strip()`.
fn py_strip(s: &str) -> &str {
  s.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c))
}

/// A Nostr address (NIP-05) such as `name@example.com` (upstream
/// `IS_NIP05`, case-insensitive).
fn is_nip05(value: &str) -> bool {
  let Some((name, domain)) = value.split_once('@') else { return false };
  let name_ok = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
  let labels: Vec<&str> = domain.split('.').collect();
  name_ok && labels.len() >= 2 && labels.iter().all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

/// `value` as a relay URL, or `None` if it is not one (upstream
/// `normalize_relay()`); `wss://` is assumed when no schema is given.
fn normalize_relay(value: &str) -> Option<String> {
  if value.len() > NOSTR_MAX_URL_LENGTH {
    return None;
  }
  // Python's `$` also matches before one final newline
  let value = value.strip_suffix('\n').unwrap_or(value);
  let lower = value.to_ascii_lowercase();
  let (schema, rest) = if lower.starts_with("wss://") {
    ("wss://", &value[6..])
  } else if lower.starts_with("ws://") {
    ("ws://", &value[5..])
  } else {
    ("wss://", value)
  };
  // IS_RELAY: host[:port] then an optional /path without spaces, ? or #
  let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
  if path.chars().any(|c| c.is_whitespace() || c == '?' || c == '#') {
    return None;
  }
  let host = match authority.split_once(':') {
    Some((host, port)) => {
      // A port outside of 0-65535 can not exist, and port zero can not be
      // connected to
      if !port.bytes().all(|b| b.is_ascii_digit()) || !port.parse::<u32>().is_ok_and(|p| (1..=65535).contains(&p)) {
        return None;
      }
      host
    }
    None => authority,
  };
  if host.is_empty() || !host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'%' | b'-')) {
    return None;
  }
  // The host must be a real name or address
  if !crate::utils::validate::is_hostname(&host.to_lowercase()) {
    return None;
  }
  Some(format!("{}{}{}", schema, authority, path))
}

/// Python's `str.lower().startswith("wss://")` for a relay list entry.
fn is_secure_relay(entry: &str) -> bool {
  entry.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("wss://"))
}

/// One file uploaded to a Blossom server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Blob {
  url: String,
  #[serde(rename = "type")]
  mime: String,
  sha256: String,
  size: u64,
  /// NIP-17 files are encrypted; what the recipient needs to decrypt them
  #[serde(default, skip_serializing_if = "Option::is_none")]
  key: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  nonce: Option<String>,
  /// The SHA-256 of the file before it was encrypted
  #[serde(default, skip_serializing_if = "Option::is_none")]
  original: Option<String>,
}

/// The gift wraps of one NIP-17 message: the recipient's, and our own copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Wraps {
  to: Vec<Event>,
  #[serde(rename = "self")]
  own: Vec<Event>,
}

/// A connection to one relay (the real one, or a stand-in in tests).
// `#[async_trait]` marks each method `#[must_use]` although its boxed
// future already is.
#[allow(clippy::double_must_use)]
#[async_trait]
trait Relay: Send {
  async fn publish(&mut self, event: &Event) -> Result<(bool, String), RelayError>;
  async fn query(&mut self, filter: &Filter) -> Result<Vec<Value>, RelayError>;
  async fn close(&mut self);
}

#[async_trait]
impl Relay for NostrRelay {
  async fn publish(&mut self, event: &Event) -> Result<(bool, String), RelayError> {
    NostrRelay::publish(self, event).await
  }

  async fn query(&mut self, filter: &Filter) -> Result<Vec<Value>, RelayError> {
    NostrRelay::query(self, filter).await
  }

  async fn close(&mut self) {
    NostrRelay::close(self).await
  }
}

/// Keep `value` for a retry of this notification.
fn remember<T: Serialize>(key: &str, value: &T, per_message: bool) {
  if let Ok(value) = serde_json::to_value(value) {
    crate::delivery::remember(key, value, per_message);
  }
}

/// What [`remember`] kept for `key` during an earlier attempt.
fn recall<T: for<'de> Deserialize<'de>>(key: &str, per_message: bool) -> Option<T> {
  crate::delivery::recall(key, per_message).and_then(|v| serde_json::from_value(v).ok())
}

fn store_get(key: &str) -> Option<Value> {
  crate::storage::with_service_store(|s| s.get(key)).flatten()
}

fn store_set(key: &str, value: Value) {
  crate::storage::with_service_store(|s| s.set(key, value, Some(CACHE_EXPIRY), true));
}

fn npub(pubkey: &str) -> String {
  bech32_encode("npub", &hex::decode(pubkey).unwrap_or_default())
}

fn tag(values: &[&str]) -> Vec<String> {
  values.iter().map(|s| s.to_string()).collect()
}

/// Join the message text and file links, one per line, skipping blanks.
fn join_lines<'a>(body: &'a str, links: &'a [String]) -> String {
  std::iter::once(body).chain(links.iter().map(String::as_str)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n")
}

pub struct Nostr {
  key: NostrKey,
  mode: Mode,
  /// Relays we publish to and look lists up on.
  relays: Vec<String>,
  /// An explicit Blossom server for attachments.
  blossom: Option<String>,
  /// Public keys (hex) and Nostr addresses (lowercase).
  targets: Vec<String>,
  invalid_targets: Vec<String>,
  verify: bool,
  tags: Vec<String>,
}

impl Nostr {
  pub fn from_url(url: &ParsedUrl) -> Option<Self> {
    // The secret key is our host, unless ?key= is given; it may be an
    // nsec1... or 64 hex characters
    let secret = url.get("key").filter(|k| !k.is_empty()).map(str::to_string).or_else(|| url.host.clone()).unwrap_or_default();
    let Some(key) = decode_key(&secret, "nsec").and_then(|hex_key| NostrKey::from_secret(&hex::decode(hex_key).ok()?.try_into().ok()?)) else {
      tracing::warn!("An invalid Nostr secret key was specified.");
      return None;
    };

    // Pick how direct messages are sent
    let mode = match url.get("mode").map(py_strip).filter(|m| !m.is_empty()) {
      None => Mode::Nip17,
      Some(m) if m.eq_ignore_ascii_case("nip17") => Mode::Nip17,
      Some(m) if m.eq_ignore_ascii_case("nip04") => Mode::Nip04,
      Some(m) => {
        tracing::warn!("The Nostr mode specified ({}) is invalid.", m);
        return None;
      }
    };

    // Relays we publish to; wss:// is assumed when no schema is given
    let mut relays = Vec::new();
    for entry in split_list(url.get("relay").unwrap_or("")) {
      let Some(relay) = normalize_relay(entry) else {
        tracing::warn!("An invalid Nostr relay ({}) was specified.", entry);
        return None;
      };
      relays.push(relay);
    }
    if relays.is_empty() {
      // Fall back to some well known public relays
      relays = NOSTR_DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect();
    }

    // An optional Blossom server for attachments; https:// is assumed
    let blossom = url.get("blossom").map(py_strip).filter(|b| !b.is_empty()).map(|b| {
      let b = b.trim_end_matches('/');
      let lower = b.to_ascii_lowercase();
      if lower.starts_with("http://") || lower.starts_with("https://") { b.to_string() } else { format!("https://{}", b) }
    });

    // Recipients are found in the path, and in ?to=
    let mut given: Vec<String> = url.path_parts.clone();
    if let Some(to) = url.get("to").filter(|t| !t.is_empty()) {
      given.extend(crate::utils::parse::parse_list(to));
    }

    // Recipients are public keys (stored as hex) or Nostr addresses
    let mut targets = Vec::new();
    let mut invalid_targets = Vec::new();
    for target in given.iter().flat_map(|t| split_list(t)) {
      if let Some(pubkey) = decode_key(target, "npub").filter(|p| is_public_key(p)) {
        targets.push(pubkey);
      } else if is_nip05(target) {
        // Looked up when the first message is sent
        targets.push(target.to_lowercase());
      } else {
        tracing::warn!("Dropped invalid Nostr recipient ({}) specified.", target);
        invalid_targets.push(target.to_string());
      }
    }

    Some(Self { key, mode, relays, blossom, targets, invalid_targets, verify: url.verify_certificate(), tags: url.tags() })
  }

  pub fn static_details() -> ServiceDetails {
    ServiceDetails {
      service_name: "Nostr",
      service_url: Some("https://nostr.com/"),
      setup_url: Some("https://appriseit.com/services/nostr/"),
      protocols: vec!["nostr"],
      description: "Post public notes or send private direct messages (NIP-17 or NIP-04) over Nostr relays.",
      attachment_support: true,
    }
  }

  /// How relays are reached: the URL's `?cto=`, `?rto=` and `?verify=`.
  fn relay_options(&self, public_only: bool) -> RelayOptions {
    let options = crate::notify::current_http_options();
    let secs = |v: Option<f64>, default: f64| crate::utils::time::secs(v.filter(|t| t.is_finite() && *t > 0.0).unwrap_or(default));
    RelayOptions {
      verify: self.verify && options.verify.unwrap_or(true),
      connect_timeout: secs(options.connect_timeout, crate::notify::DEFAULT_CONNECT_TIMEOUT),
      timeout: secs(options.read_timeout, crate::notify::DEFAULT_READ_TIMEOUT),
      user_agent: APP_ID.to_string(),
      public_only,
    }
  }

  async fn open_relay(&self, url: &str, public_only: bool) -> Result<Box<dyn Relay>, RelayError> {
    #[cfg(test)]
    if let Some(fake) = tests::fake_relay(url, public_only) {
      return fake;
    }
    Ok(Box::new(NostrRelay::connect(url, &self.key, &self.relay_options(public_only)).await?))
  }

  /// Post a public note (kind 1).
  async fn send_note(&self, body: &str, attachments: &[Attachment]) -> bool {
    // A retry resends the same note so relays can spot the duplicate
    let event = match recall::<Event>("note", false) {
      Some(event) => event,
      None => {
        // Upload any attachments first, then link each file in the note
        // and describe it in an imeta tag
        let Some((urls, tags)) = self.upload_links(attachments).await else { return false };
        let event = self.key.sign(NOSTR_KIND_NOTE, &join_lines(body, &urls), tags, None);
        remember("note", &event, false);
        event
      }
    };
    if !self.publish(&self.relays, std::slice::from_ref(&event), false).await {
      tracing::warn!("Failed to send Nostr note.");
      return false;
    }
    tracing::info!("Sent Nostr note.");
    true
  }

  /// Send an older style NIP-04 direct message to `pubkey`.
  async fn send_nip04(&self, pubkey: &str, body: &str, links: &[String]) -> bool {
    // A retry resends the same message so relays can spot the duplicate
    let memo = format!("nip04|{}", pubkey);
    let event = match recall::<Event>(&memo, false) {
      Some(event) => event,
      None => {
        // Link each uploaded file in the message and encrypt it all
        let Some(shared) = self.key.shared_x(pubkey) else { return false };
        let event = self.key.sign(NOSTR_KIND_NIP04, &nip04_encrypt(&join_lines(body, links), &shared, None), vec![tag(&["p", pubkey])], None);
        remember(&memo, &event, false);
        event
      }
    };
    self.publish(&self.relays, std::slice::from_ref(&event), false).await
  }

  /// Send a private NIP-17 direct message to `pubkey`.
  async fn send_nip17(&self, pubkey: &str, body: &str, files: &[Blob]) -> bool {
    // NIP-17 messages may only go to relays the recipient listed
    let Some(relays) = self.inbox_relays(pubkey).await else {
      tracing::warn!("Could not look up the Nostr direct message relays of {}.", npub(pubkey));
      return false;
    };
    if relays.is_empty() {
      tracing::warn!("{} has not listed any relays for NIP-17 direct messages; ?mode=nip04 may reach them instead.", npub(pubkey));
      return false;
    }

    // A message to ourselves is already our own copy
    let own_key = self.key.public_key();
    let to_self = pubkey == own_key;

    // A retry resends the same messages so relays can spot duplicates
    let memo = format!("nip17|{}", pubkey);
    let wraps = match recall::<Wraps>(&memo, false) {
      Some(wraps) => wraps,
      None => {
        // The text itself is a chat message (kind 14), and each attachment
        // an encrypted file message (kind 15)
        let mut messages = Vec::new();
        if !body.is_empty() {
          messages.push(rumor(own_key, NOSTR_KIND_CHAT, body, vec![tag(&["p", pubkey])], None));
        }
        for blob in files {
          let tags = vec![
            tag(&["p", pubkey]),
            tag(&["file-type", &blob.mime]),
            tag(&["encryption-algorithm", "aes-gcm"]),
            tag(&["decryption-key", blob.key.as_deref().unwrap_or("")]),
            tag(&["decryption-nonce", blob.nonce.as_deref().unwrap_or("")]),
            tag(&["x", &blob.sha256]),
            tag(&["ox", blob.original.as_deref().unwrap_or("")]),
            tag(&["size", &blob.size.to_string()]),
          ];
          messages.push(rumor(own_key, NOSTR_KIND_FILE, &blob.url, tags, None));
        }

        // Seal and gift wrap each one for the recipient, and again for
        // ourselves so our other Nostr clients show what was sent
        let wrap_all = |recipient: &str| messages.iter().map(|m| gift_wrap(&self.key, recipient, m)).collect::<Option<Vec<_>>>();
        let Some(to) = wrap_all(pubkey) else { return false };
        let own = if to_self { Vec::new() } else { wrap_all(own_key).unwrap_or_default() };
        let wraps = Wraps { to, own };
        remember(&memo, &wraps, false);
        wraps
      }
    };

    // Only a list signed by someone else is kept to public addresses
    if !self.publish(&relays, &wraps.to, !to_self).await {
      return false;
    }

    // Our own copy is kept when we list relays of our own; losing it does
    // not undo the delivery. Our own list is signed by our own key, so
    // private relays are allowed there.
    if !wraps.own.is_empty()
      && let Some(own) = self.inbox_relays(own_key).await.filter(|r| !r.is_empty())
      && !self.publish(&own, &wraps.own, false).await
    {
      tracing::debug!("Nostr could not keep a copy of the message.");
    }
    true
  }

  /// Send `events` to each relay; true if each one reached at least one.
  /// Set `public_only` for relays taken from someone's relay list.
  async fn publish(&self, relays: &[String], events: &[Event], public_only: bool) -> bool {
    let mut accepted: Vec<&str> = Vec::new();
    for url in relays {
      // Always throttle before any remote server i/o is made
      crate::notify::throttle().await;
      tracing::debug!("Nostr publishing {} event(s) to {} (cert_verify={})", events.len(), url, self.verify);
      let mut relay = match self.open_relay(url, public_only).await {
        Ok(relay) => relay,
        Err(e) => {
          tracing::warn!("A connection error occurred with Nostr relay {}.", url);
          tracing::debug!("Socket Exception: {}", e);
          continue;
        }
      };
      for event in events {
        match relay.publish(event).await {
          Ok((true, _)) => {
            // One relay is enough, but more is safer
            if !accepted.contains(&event.id.as_str()) {
              accepted.push(&event.id);
            }
          }
          Ok((false, reason)) => tracing::warn!("Nostr relay {} rejected the message: {}", url, reason),
          Err(e) => {
            tracing::warn!("A connection error occurred with Nostr relay {}.", url);
            tracing::debug!("Socket Exception: {}", e);
            break;
          }
        }
      }
      relay.close().await;
    }
    // Every event must have reached at least one relay
    accepted.len() == events.len()
  }

  /// The newest correctly signed event of `kind` from `pubkey`, and whether
  /// any relay could be asked at all.
  async fn lookup(&self, kind: i64, pubkey: &str) -> (Option<Event>, bool) {
    let mut newest: Option<Event> = None;
    let mut answered = false;
    let filter = Filter { kinds: vec![kind], authors: Some(vec![pubkey.to_string()]), limit: Some(1) };
    for url in &self.relays {
      // Always throttle before any remote server i/o is made
      crate::notify::throttle().await;
      let events = match self.open_relay(url, false).await {
        Ok(mut relay) => {
          let events = relay.query(&filter).await;
          relay.close().await;
          events
        }
        Err(e) => Err(e),
      };
      let events = match events {
        Ok(events) => events,
        Err(e) => {
          // Another relay may still have it
          tracing::debug!("Nostr lookup on {} failed: {}", url, e);
          continue;
        }
      };

      // This relay answered, even if it had nothing
      answered = true;
      for event in events.iter().filter_map(verify_event) {
        // Only trust correctly signed events from the right author
        if event.kind == kind && event.pubkey == pubkey && newest.as_ref().is_none_or(|n| event.created_at > n.created_at) {
          newest = Some(event);
        }
      }
    }
    (newest, answered)
  }

  /// The values of each `name` tag in a looked up list, or `None` when no
  /// relay could be asked.
  async fn lookup_tags(&self, kind: i64, pubkey: &str, name: &str) -> Option<Vec<String>> {
    let (event, answered) = self.lookup(kind, pubkey).await;
    if !answered {
      return None;
    }
    Some(event.map(|e| e.tags.into_iter().filter(|t| t.len() > 1 && t[0] == name).map(|t| t[1].clone()).collect()).unwrap_or_default())
  }

  /// The relays `pubkey` reads direct messages from (kind 10050). An empty
  /// list means none are listed; `None` means the lookup failed.
  async fn inbox_relays(&self, pubkey: &str) -> Option<Vec<String>> {
    let cache_key = format!("inbox_{}", pubkey);
    if let Some(cached) = store_get(&cache_key).and_then(|v| serde_json::from_value::<Vec<String>>(v).ok()) {
      return Some(cached);
    }

    // Try again next time rather than remember a failure
    let listed = self.lookup_tags(NOSTR_KIND_INBOX_RELAYS, pubkey, "relay").await?;

    // Only a few well formed, secure relays are accepted from someone
    // else's list
    let mut relays: Vec<String> = Vec::new();
    for entry in listed {
      if let Some(url) = Some(&entry).filter(|e| is_secure_relay(e)).and_then(|e| normalize_relay(e))
        && !relays.contains(&url)
      {
        relays.push(url);
        if relays.len() >= NOSTR_MAX_INBOX_RELAYS {
          break;
        }
      }
    }
    store_set(&cache_key, json!(relays));
    Some(relays)
  }

  /// The Blossom server attachments are uploaded to: an explicit one, else
  /// the first secure server from our own list (kind 10063), else a public
  /// default.
  async fn blossom_server(&self) -> String {
    if let Some(blossom) = &self.blossom {
      return blossom.clone();
    }
    if let Some(server) = store_get("blossom").and_then(|v| v.as_str().map(str::to_string)) {
      return server;
    }
    let listed = self.lookup_tags(NOSTR_KIND_BLOSSOM_SERVERS, self.key.public_key(), "server").await;
    let server = listed
      .iter()
      .flatten()
      .find(|url| crate::utils::http::is_secure_http_url(url))
      .map(|url| url.trim_end_matches('/').to_string())
      .unwrap_or_else(|| NOSTR_DEFAULT_BLOSSOM.to_string());
    if listed.is_some() {
      // Only remember an answer that really came from a relay
      store_set("blossom", json!(server));
    }
    server
  }

  /// The public key (hex) of a recipient: a Nostr address is looked up at
  /// `https://<domain>/.well-known/nostr.json` (NIP-05) and cached.
  async fn resolve(&self, target: &str) -> Option<String> {
    // Public keys were already decoded when we loaded
    if is_hex_key(target) {
      return Some(target.to_string());
    }
    let cache_key = format!("nip05_{}", target);
    if let Some(pubkey) = store_get(&cache_key).and_then(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string)) {
      return Some(pubkey);
    }

    let (name, domain) = target.split_once('@')?;
    let url = format!("{}/.well-known/nostr.json", crate::notify::service_base(&format!("https://{}", domain)));
    tracing::debug!("Nostr address lookup: {} (cert_verify={})", url, self.verify);

    // Always throttle before any remote server i/o is made
    crate::notify::throttle().await;
    // NIP-05 requires that redirects are never followed
    let request = async {
      let client = crate::notify::client_builder(self.verify, Default::default()).redirect(reqwest::redirect::Policy::none()).build()?;
      let response = client.get(&url).query(&[("name", name)]).header("User-Agent", APP_ID).header("Accept", "application/json").send().await?;
      let status = response.status();
      // Never read more than a sensible amount
      Ok::<_, reqwest::Error>((status, crate::utils::http::read_bounded_response(response, NOSTR_MAX_RESPONSE).await?))
    };
    let (status, data) = match request.await {
      Ok(reply) => reply,
      Err(e) => {
        tracing::warn!("A connection error occurred looking up Nostr address {}.", target);
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return None;
      }
    };

    // The answer looks like {"names": {"<name>": "<hex pubkey>"}}
    let content: Value = data.and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or(Value::Null);
    let pubkey = content.get("names").and_then(Value::as_object).and_then(|names| names.get(name)).and_then(Value::as_str);
    let Some(pubkey) = pubkey.filter(|p| status == reqwest::StatusCode::OK && is_hex_key(p) && is_public_key(p)) else {
      tracing::warn!("Could not find Nostr address {}.", target);
      return None;
    };

    // Cache the answer for next time
    let pubkey = pubkey.to_lowercase();
    store_set(&cache_key, json!(pubkey));
    Some(pubkey)
  }

  /// Upload attachments; return their URLs and imeta tags.
  async fn upload_links(&self, attachments: &[Attachment]) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    let mut urls = Vec::new();
    let mut tags = Vec::new();
    for (index, attachment) in attachments.iter().enumerate() {
      // A retry reuses files that were already uploaded
      let memo = format!("upload|{}", index);
      let blob = match recall::<Blob>(&memo, true) {
        Some(blob) => blob,
        None => {
          let blob = self.upload(&attachment.data, &attachment.mime_type).await?;
          remember(&memo, &blob, true);
          blob
        }
      };
      // Clients show a preview using the details in the imeta tag
      tags.push(vec![
        "imeta".to_string(),
        format!("url {}", blob.url),
        format!("m {}", blob.mime),
        format!("x {}", blob.sha256),
        format!("size {}", blob.size),
      ]);
      urls.push(blob.url);
    }
    Some((urls, tags))
  }

  /// Encrypt and upload attachments for NIP-17 file messages.
  async fn upload_files(&self, attachments: &[Attachment]) -> Option<Vec<Blob>> {
    let mut files = Vec::new();
    for (index, attachment) in attachments.iter().enumerate() {
      // A retry reuses files that were already uploaded
      let memo = format!("encrypted|{}", index);
      let blob = match recall::<Blob>(&memo, true) {
        Some(blob) => blob,
        None => {
          // Encrypt the file and upload it as anonymous binary data
          let (encrypted, key, nonce) = crypto::encrypt_file(&attachment.data, None);
          let mut blob = self.upload(&encrypted, "application/octet-stream").await?;
          // Keep what the recipient needs to download and decrypt it
          blob.mime = attachment.mime_type.clone();
          blob.key = Some(key);
          blob.nonce = Some(nonce);
          blob.original = Some(hex::encode(Sha256::digest(&attachment.data)));
          remember(&memo, &blob, true);
          blob
        }
      };
      files.push(blob);
    }
    Some(files)
  }

  /// Upload `data` to our Blossom server (BUD-02) and return its details.
  async fn upload(&self, data: &[u8], mime: &str) -> Option<Blob> {
    // Uploads are named by the SHA-256 of their content
    let digest = hex::encode(Sha256::digest(data));
    let server = self.blossom_server().await;

    // Sign a short lived permission to upload this exact file; it travels
    // as unpadded URL-safe base64
    let expiration = (chrono::Utc::now().timestamp() + NOSTR_UPLOAD_AUTH_EXPIRY).to_string();
    let auth =
      self.key.sign(NOSTR_KIND_BLOSSOM_AUTH, "Upload Blob", vec![tag(&["t", "upload"]), tag(&["x", &digest]), tag(&["expiration", &expiration])], None);
    let authorization = format!("Nostr {}", URL_SAFE_NO_PAD.encode(auth.to_py_json()));

    let url = format!("{}/upload", crate::notify::service_base(&server));
    tracing::debug!("Nostr Blossom upload: {} (cert_verify={})", url, self.verify);

    // Always throttle before any remote server i/o is made
    crate::notify::throttle().await;
    let request = async {
      let response = build_client(self.verify)
        .map_err(|_| None)?
        .put(&url)
        .header("User-Agent", APP_ID)
        .header("Content-Type", mime)
        .header("X-SHA-256", &digest)
        .header("Authorization", &authorization)
        .body(data.to_vec())
        .send()
        .await
        .map_err(Some)?;
      let status = response.status();
      let reason = response.headers().get("X-Reason").and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).map(str::to_string);
      // Never read more than a sensible amount
      let reply = crate::utils::http::read_bounded_response(response, NOSTR_MAX_RESPONSE).await.map_err(Some)?;
      Ok::<_, Option<reqwest::Error>>((status, reason, reply))
    };
    let (status, reason, reply) = match request.await {
      Ok(reply) => reply,
      Err(e) => {
        tracing::warn!("A connection error occurred uploading to Blossom server {}.", server);
        if let Some(e) = e {
          tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        }
        return None;
      }
    };

    // The reply describes the stored file; it must be the file we sent, and
    // its link must be as secure as the server
    let content: Value = reply.as_deref().and_then(|r| serde_json::from_slice(r).ok()).unwrap_or(Value::Null);
    let starts = |s: &str, prefix: &str| s.get(..prefix.len()).is_some_and(|p| p.eq_ignore_ascii_case(prefix));
    let link = content
      .get("url")
      .and_then(Value::as_str)
      .filter(|link| link.len() <= NOSTR_MAX_URL_LENGTH && (starts(link, "https://") || (starts(&server, "http://") && starts(link, "http://"))));
    let same_file = content.get("sha256").and_then(Value::as_str).is_some_and(|s| s.to_lowercase() == digest)
      && match content.get("size") {
        None => true,
        Some(Value::Number(n)) => n.as_f64() == Some(data.len() as f64),
        Some(Value::Bool(b)) => u64::from(*b) == data.len() as u64,
        Some(_) => false,
      };

    // 200 means the file was already there; 201 means it is new
    match link {
      Some(link) if same_file && matches!(status.as_u16(), 200 | 201) => {
        Some(Blob { url: link.to_string(), mime: mime.to_string(), sha256: digest, size: data.len() as u64, key: None, nonce: None, original: None })
      }
      _ => {
        let status_str = status.canonical_reason().unwrap_or("");
        tracing::warn!(
          "Failed to upload Nostr attachment to {}: {}{}error={}.",
          server,
          status_str,
          if status_str.is_empty() { "" } else { ", " },
          status.as_u16()
        );
        let details = reason.unwrap_or_else(|| reply.map(|r| String::from_utf8_lossy(&r).into_owned()).unwrap_or_default());
        tracing::debug!("Response Details:\r\n{}", details);
        None
      }
    }
  }
}

#[async_trait]
impl Notify for Nostr {
  fn schemas(&self) -> &[&str] {
    &["nostr"]
  }

  fn service_name(&self) -> &str {
    "Nostr"
  }

  fn details(&self) -> ServiceDetails {
    Self::static_details()
  }

  fn tags(&self) -> Vec<String> {
    self.tags.clone()
  }

  /// Nostr messages are plain text.
  fn notify_format(&self) -> NotifyFormat {
    NotifyFormat::Text
  }

  /// Many relays refuse events larger than 64KB. A direct message grows a
  /// lot once sealed and wrapped, and this keeps even text made only of 4
  /// byte characters (such as emoji) under that size.
  fn body_maxlen(&self) -> usize {
    7000
  }

  /// Nostr has no title field.
  fn title_maxlen(&self) -> usize {
    0
  }

  /// Relays are separate servers, so there is nothing to throttle against.
  fn request_rate_per_sec(&self) -> f64 {
    0.0
  }

  /// Files are uploaded to a Blossom server and linked in the message.
  fn attachment_support(&self) -> bool {
    true
  }

  /// The public key is the identity, whatever form the secret key came in.
  fn url_identifier(&self) -> Option<Vec<String>> {
    Some(vec!["nostr".to_string(), self.key.public_key().to_string()])
  }

  async fn send(&self, ctx: &NotifyContext) -> Result<bool, NotifyError> {
    let body = ctx.body.as_str();
    if self.targets.is_empty() {
      if !self.invalid_targets.is_empty() {
        // Recipients were given but none of them were usable
        tracing::warn!("There are no valid Nostr recipients to notify.");
        return Ok(false);
      }
      // No recipients means a public note
      return Ok(self.send_note(body, &ctx.attachments).await);
    }

    // Upload any attachments once so every recipient shares them
    let (files, links) = match self.mode {
      Mode::Nip17 => match self.upload_files(&ctx.attachments).await {
        Some(files) => (files, Vec::new()),
        None => return Ok(false),
      },
      Mode::Nip04 => match self.upload_links(&ctx.attachments).await {
        Some((urls, _)) => (Vec::new(), urls),
        None => return Ok(false),
      },
    };

    let mut has_error = false;
    for target in &self.targets {
      // Skip a recipient that already has this message
      if crate::delivery::is_delivered(target, false) {
        continue;
      }
      // Turn a Nostr address into a public key if needed
      let sent = match self.resolve(target).await {
        Some(pubkey) => match self.mode {
          Mode::Nip17 => self.send_nip17(&pubkey, body, &files).await,
          Mode::Nip04 => self.send_nip04(&pubkey, body, &links).await,
        },
        None => false,
      };
      if !sent {
        tracing::warn!("Failed to send Nostr direct message to {}.", target);
        has_error = true;
        continue;
      }
      // Delivered; a retry can safely skip this recipient
      tracing::info!("Sent Nostr direct message to {}.", target);
      crate::delivery::mark_delivered(target, false);
    }
    Ok(!has_error)
  }
}
