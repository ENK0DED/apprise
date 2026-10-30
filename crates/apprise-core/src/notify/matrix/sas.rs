//! Opt-in SAS verification (`?autoverify=yes`) from another device of the
//! same Matrix user (upstream `apprise/plugins/matrix/sas.py`). A completed
//! exchange is remembered for the current device identity.
//!
//! Protocol: <https://spec.matrix.org/v1.19/client-server-api/#key-verification-framework>

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::e2ee::{self, SAS_METHOD, SasResponder, SasState};
use super::{CACHE_EXPIRY, Req, Run, uuid4};
use crate::notify::{DEFAULT_READ_TIMEOUT, current_http_options};

const EVENT_REQUEST: &str = "m.key.verification.request";
const EVENT_READY: &str = "m.key.verification.ready";
const EVENT_START: &str = "m.key.verification.start";
const EVENT_ACCEPT: &str = "m.key.verification.accept";
const EVENT_CANCEL: &str = "m.key.verification.cancel";
const EVENT_KEY: &str = "m.key.verification.key";
const EVENT_MAC: &str = "m.key.verification.mac";
const EVENT_DONE: &str = "m.key.verification.done";

/// Requests outside these age limits are stale or replayed.
const REQUEST_MAX_PAST_MS: i64 = 10 * 60 * 1000;
const REQUEST_MAX_FUTURE_MS: i64 = 5 * 60 * 1000;

/// Bound on identifiers before echoing, comparing or hashing them.
const MAX_ID_LEN: usize = 512;

/// Bound on the small key maps devices advertise.
const MAX_DEVICE_KEYS: usize = 8;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Delay between failed attempts, so each send does not wait again.
const RETRY_COOLDOWN: Duration = Duration::from_secs(60 * 15);

/// Refresh trust daily rather than rewriting identity data on every send.
const REFRESH_INTERVAL: f64 = 60.0 * 60.0 * 24.0;

/// Only to-device events are needed; an empty room list avoids nested
/// filters.
const SYNC_FILTER: &str = r#"{"presence": {"types": []}, "account_data": {"types": []}, "room": {"rooms": []}}"#;

const FALLBACK_SYNC_POLL_MS: u64 = 2000;
const MIN_SYNC_POLL_MS: u64 = 250;

fn valid_id(value: Option<&Value>) -> Option<&str> {
  value.and_then(Value::as_str).filter(|s| !s.is_empty() && s.chars().count() <= MAX_ID_LEN)
}

fn now_secs() -> f64 {
  std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Half the configured read timeout, so a long poll ends before it.
fn sync_poll_ms() -> u64 {
  let read = current_http_options().read_timeout.unwrap_or(DEFAULT_READ_TIMEOUT);
  let poll = read * 1000.0 / 2.0;
  if !poll.is_finite() || poll <= 0.0 {
    return FALLBACK_SYNC_POLL_MS;
  }
  (poll as u64).max(MIN_SYNC_POLL_MS)
}

/// The transaction being verified.
struct Active {
  transaction_id: String,
  user_id: String,
  device_id: String,
  peer_keys: Map<String, Value>,
  sas: Option<SasResponder>,
  peer_done: bool,
}

impl Run<'_> {
  /// Remember that the current identity completed SAS.
  fn store_verified_binding(&mut self) -> bool {
    let Some(binding) = self.s.get_str("e2ee_device_binding") else { return false };
    self.s.set("e2ee_verified_binding", json!(binding), Some(CACHE_EXPIRY))
  }

  /// Extend SAS trust after a successful send, at most daily (upstream
  /// `refresh_verified_state()`). A changed device or account must verify
  /// again.
  pub(super) fn refresh_verified_state(&mut self) -> bool {
    let Some(binding) = self.s.get_str("e2ee_device_binding") else { return false };
    if self.s.get_str("e2ee_verified_binding").as_deref() != Some(binding.as_str())
      || self.s.user_id.is_none()
      || self.s.device_id.is_none()
      || self.s.account.is_none()
      || binding != self.e2ee_binding_key()
    {
      return false;
    }
    let last = self.s.get("e2ee_autoverify_refreshed_at").and_then(|v| v.as_f64()).filter(|v| v.is_finite()).unwrap_or(0.0);
    if now_secs() - last < REFRESH_INTERVAL {
      return true;
    }
    // Refresh in order and stop at the first failure; the trust marker goes
    // last so a partial refresh fails closed.
    let device_id = self.s.device_id.clone();
    if !self.s.set("device_id", json!(device_id), Some(CACHE_EXPIRY)) || !self.store_account() {
      return false;
    }
    if !self.s.set("e2ee_device_binding", json!(binding), Some(CACHE_EXPIRY)) || !self.s.set("e2ee_verified_binding", json!(binding), Some(CACHE_EXPIRY)) {
      return false;
    }
    self.s.set("e2ee_autoverify_refreshed_at", json!(now_secs()), Some(CACHE_EXPIRY));
    true
  }

  /// Complete one same-user SAS verification (upstream `auto_verify()`).
  pub(super) async fn auto_verify(&mut self) -> bool {
    let binding = self.s.get_str("e2ee_device_binding");
    if binding.is_some() && self.s.get_str("e2ee_verified_binding") == binding {
      // Already verified for this identity.
      return true;
    }
    if self.s.user_id.is_none() || self.s.device_id.is_none() || self.s.account.is_none() {
      return false;
    }
    // Wait out the cooldown only for the identity that just failed.
    if self.s.get_str("e2ee_autoverify_cooldown") == binding {
      return false;
    }
    let result = Verifier { deadline: Instant::now() + self.m.autoverify_timeout, active: None }.run(self).await;
    if !result {
      self.s.set("e2ee_autoverify_cooldown", json!(binding), Some(RETRY_COOLDOWN));
    }
    result
  }
}

/// One verification attempt (upstream `MatrixSASAutoVerifier`).
struct Verifier {
  deadline: Instant,
  active: Option<Active>,
}

impl Verifier {
  /// A request bounded by this attempt's remaining time. `best_effort` lets
  /// the final cancellation go out after the deadline.
  async fn fetch(&self, run: &mut Run<'_>, mut req: Req<'_>, bound: Option<Duration>, best_effort: bool) -> super::Reply {
    let remaining = run.m.autoverify_timeout.min(self.deadline.saturating_duration_since(Instant::now()));
    if remaining.is_zero() && !best_effort {
      return super::Reply::failed(0);
    }
    let bound = bound.unwrap_or_else(|| Duration::from_millis(sync_poll_ms()));
    // Two seconds for the network around the server's own wait.
    req.timeout = Some(bound.min(remaining) + Duration::from_secs(2));
    req.max_retry_wait = Some(remaining);
    req.max_response_bytes = Some(MAX_RESPONSE_BYTES);
    run.fetch(req).await
  }

  /// Poll `/sync` until a verification completes or the time runs out.
  async fn run(mut self, run: &mut Run<'_>) -> bool {
    tracing::info!("Matrix E2EE: waiting up to {} seconds for a same-user SAS verification request.", run.m.autoverify_timeout.as_secs());
    let max_poll_ms = sync_poll_ms();
    let mut since: Option<String> = None;
    while Instant::now() < self.deadline {
      let remaining_ms = self.deadline.saturating_duration_since(Instant::now()).as_millis() as u64;
      let poll_ms = max_poll_ms.min(remaining_ms);
      let mut req = Req::get("/sync");
      req.params = vec![("timeout".into(), poll_ms.to_string()), ("filter".into(), SYNC_FILTER.into()), ("set_presence".into(), "offline".into())];
      if let Some(since) = &since {
        req.params.push(("since".into(), since.clone()));
      }
      let reply = self.fetch(run, req, Some(Duration::from_millis(poll_ms)), false).await;
      if !(reply.ok && reply.body.is_object()) {
        return false;
      }
      // Without a sync token the poll cannot resume safely.
      let Some(next_batch) = reply.str("next_batch") else { return false };
      since = Some(next_batch);
      let events = reply.body.get("to_device").and_then(|t| t.get("events")).and_then(Value::as_array).cloned().unwrap_or_default();
      for event in events {
        if let Some(result) = self.handle_event(run, &event).await {
          return result;
        }
        if Instant::now() >= self.deadline {
          break;
        }
      }
    }
    self.fail(run, "m.timeout", "SAS verification timed out").await
  }

  /// Route one to-device event. `Some` ends the verification.
  async fn handle_event(&mut self, run: &mut Run<'_>, event: &Value) -> Option<bool> {
    let event_type = event.get("type").and_then(Value::as_str)?;
    let sender = event.get("sender").and_then(Value::as_str)?;
    let content = event.get("content").unwrap_or(&Value::Null);
    // Same-user only, by design.
    if Some(sender) != run.s.user_id.as_deref() || !content.is_object() {
      return None;
    }
    if event_type == EVENT_REQUEST {
      return self.handle_request(run, sender, content).await;
    }
    let active = self.active.as_ref()?;
    if content.get("transaction_id").and_then(Value::as_str) != Some(active.transaction_id.as_str()) || sender != active.user_id {
      // Late events or another attempt.
      return None;
    }
    match event_type {
      EVENT_CANCEL => {
        let reason = content.get("reason").and_then(|r| r.as_str()).filter(|r| !r.is_empty()).unwrap_or("unspecified reason");
        tracing::warn!("Matrix E2EE SAS verification was cancelled: {}", reason);
        Some(false)
      }
      EVENT_START => self.handle_start(run, sender, content).await,
      _ if active.sas.is_none() => Some(self.fail(run, "m.unexpected_message", "SAS verification has not started").await),
      EVENT_KEY => self.handle_key(run, sender, content).await,
      EVENT_MAC => self.handle_mac(run, content).await,
      EVENT_DONE => {
        let active = self.active.as_mut()?;
        active.peer_done = true;
        // Trust is saved only after the peer's MAC checked out.
        if active.sas.as_ref().is_some_and(|s| s.state == SasState::Verified) { Some(run.store_verified_binding()) } else { None }
      }
      _ => None,
    }
  }

  async fn handle_request(&mut self, run: &mut Run<'_>, sender: &str, content: &Value) -> Option<bool> {
    let now_ms = (now_secs() * 1000.0) as i64;
    // Integer timestamps only (apprise-go fcd329c): a float is malformed.
    let timestamp = content.get("timestamp").filter(|t| t.is_i64() || t.is_u64()).and_then(Value::as_i64)?;
    if timestamp < now_ms - REQUEST_MAX_PAST_MS || timestamp > now_ms + REQUEST_MAX_FUTURE_MS {
      return None;
    }
    let transaction_id = valid_id(content.get("transaction_id"))?.to_string();
    let from_device = valid_id(content.get("from_device"))?.to_string();
    if !content.get("methods").and_then(Value::as_array).is_some_and(|m| m.iter().any(|v| v.as_str() == Some(SAS_METHOD))) {
      return None;
    }

    if let Some(active) = &self.active {
      let same_device = sender == active.user_id && from_device == active.device_id;
      if same_device && transaction_id == active.transaction_id {
        // An exact replay.
        return None;
      }
      if same_device {
        // Both attempts from that device must be ended.
        self.cancel(run, "m.unexpected_message", "A newer verification request superseded this one").await;
        self.active = None;
      }
      let cancel = json!({"transaction_id": transaction_id, "code": "m.unexpected_message", "reason": "Another verification is already active"});
      self.send_event(run, EVENT_CANCEL, sender, &from_device, cancel, false).await;
      return None;
    }

    let peer_keys = self.peer_keys(run, sender, &from_device).await.filter(|k| !k.is_empty());
    let Some(peer_keys) = peer_keys else { return Some(false) };
    self.active = Some(Active {
      transaction_id: transaction_id.clone(),
      user_id: sender.to_string(),
      device_id: from_device.clone(),
      peer_keys,
      sas: None,
      peer_done: false,
    });
    let ready = json!({"transaction_id": transaction_id, "from_device": run.s.device_id, "methods": [SAS_METHOD]});
    if !self.send_event(run, EVENT_READY, sender, &from_device, ready, false).await {
      return Some(false);
    }
    None
  }

  async fn handle_start(&mut self, run: &mut Run<'_>, sender: &str, content: &Value) -> Option<bool> {
    let active = self.active.as_ref()?;
    if active.sas.is_some() {
      return Some(self.fail(run, "m.unexpected_message", "SAS verification was already started").await);
    }
    let own_user = run.s.user_id.clone().unwrap_or_default();
    let own_device = run.s.device_id.clone().unwrap_or_default();
    let prepared = SasResponder::new(&own_user, &own_device, sender, &active.device_id, content).and_then(|mut sas| sas.accept_content().map(|a| (sas, a)));
    let Ok((sas, accept)) = prepared else {
      return Some(self.fail(run, "m.unknown_method", "Unsupported SAS verification parameters").await);
    };
    let device_id = active.device_id.clone();
    self.active.as_mut()?.sas = Some(sas);
    if !self.send_event(run, EVENT_ACCEPT, sender, &device_id, accept, false).await {
      return Some(false);
    }
    None
  }

  async fn handle_key(&mut self, run: &mut Run<'_>, sender: &str, content: &Value) -> Option<bool> {
    let signing_key = run.s.account.as_ref().map(|a| a.signing_key()).unwrap_or_default();
    let active = self.active.as_mut()?;
    let sas = active.sas.as_mut()?;
    let exchanged = (|| {
      sas.receive_key(content.get("key").unwrap_or(&Value::Null))?;
      let code = sas.decimal_sas()?;
      let key = sas.key_content()?;
      let mac = sas.mac_content(&signing_key)?;
      Ok::<_, &'static str>((code, key, mac))
    })();
    let Ok((code, key_content, mac_content)) = exchanged else {
      return Some(self.fail(run, "m.invalid_message", "Invalid SAS public key").await);
    };
    // Apprise answers automatically: compare this code on the other device
    // before approving there.
    tracing::info!("Matrix E2EE SAS code (compare against your other device): {}-{}-{}", code.0, code.1, code.2);
    let device_id = active.device_id.clone();
    if !self.send_event(run, EVENT_KEY, sender, &device_id, key_content, false).await
      || !self.send_event(run, EVENT_MAC, sender, &device_id, mac_content, false).await
    {
      return Some(false);
    }
    None
  }

  async fn handle_mac(&mut self, run: &mut Run<'_>, content: &Value) -> Option<bool> {
    let active = self.active.as_mut()?;
    let peer_keys = active.peer_keys.clone();
    if active.sas.as_mut()?.verify_peer_mac(content, &peer_keys).is_err() {
      return Some(self.fail(run, "m.key_mismatch", "SAS device key MAC did not match").await);
    }
    let (user_id, device_id, transaction_id, peer_done) = (active.user_id.clone(), active.device_id.clone(), active.transaction_id.clone(), active.peer_done);
    if !self.send_event(run, EVENT_DONE, &user_id, &device_id, json!({"transaction_id": transaction_id}), false).await {
      return Some(false);
    }
    // Both sides are done: this identity may keep its trust.
    if peer_done { Some(run.store_verified_binding()) } else { None }
  }

  /// Send one unencrypted SAS event to a device.
  async fn send_event(&self, run: &mut Run<'_>, event_type: &str, user_id: &str, device_id: &str, content: Value, best_effort: bool) -> bool {
    // A unique transaction keeps repeated PUTs apart.
    let path = format!("/sendToDevice/{event_type}/{}", uuid4().replace('-', ""));
    let req = Req::put(path, json!({"messages": {user_id: {device_id: content}}}));
    self.fetch(run, req, None, best_effort).await.ok
  }

  /// Cancel the active transaction on the peer device.
  async fn cancel(&self, run: &mut Run<'_>, code: &str, reason: &str) -> bool {
    let Some(active) = &self.active else { return false };
    let content = json!({"transaction_id": active.transaction_id, "code": code, "reason": reason});
    // Sent even when the deadline just passed.
    self.send_event(run, EVENT_CANCEL, &active.user_id, &active.device_id, content, true).await
  }

  async fn fail(&self, run: &mut Run<'_>, code: &str, reason: &str) -> bool {
    self.cancel(run, code, reason).await;
    false
  }

  /// The peer's validated device keys plus its advertised master keys.
  async fn peer_keys(&self, run: &mut Run<'_>, user_id: &str, device_id: &str) -> Option<Map<String, Value>> {
    let reply = self.fetch(run, Req::post("/keys/query", json!({"device_keys": {user_id: [device_id]}})), None, false).await;
    if !(reply.ok && reply.body.is_object()) {
      return None;
    }
    let device = reply.body.get("device_keys")?.get(user_id)?.get(device_id)?;
    if !e2ee::verify_device_keys(device, user_id, device_id) {
      return None;
    }
    let bounded = |v: Option<&Value>| v.and_then(Value::as_object).filter(|m| m.len() <= MAX_DEVICE_KEYS).cloned();
    let mut keys = bounded(device.get("keys")).unwrap_or_default();
    if let Some(master) = bounded(reply.body.get("master_keys").and_then(|m| m.get(user_id)).and_then(|m| m.get("keys"))) {
      keys.extend(master);
    }
    Some(keys)
  }
}
