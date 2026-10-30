//! Sending to encrypted rooms (the E2EE helpers of upstream `base.py`):
//! device key upload, Megolm sessions, room-key sharing over Olm and
//! encrypted messages and attachments.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::e2ee::{self, Megolm, OlmAccount};
use super::sizing::{MATRIX_EVENT_BYTE_LIMIT, MATRIX_EVENT_SAFETY_MARGIN};
use super::{CACHE_EXPIRY, MEGOLM_ALGORITHM, Req, Run};
use crate::notify::{APP_ID, Attachment, ClientDefaults, client_builder, throttle};
use crate::utils::parse::quote;

/// The keys of one recipient device.
struct DeviceKeys {
  curve25519: String,
  ed25519: String,
}

impl Run<'_> {
  /// The identity cached trust and the key-upload flag belong to: user,
  /// device and both account keys (upstream `_e2ee_binding_key()`).
  pub(super) fn e2ee_binding_key(&self) -> String {
    match &self.s.account {
      None => String::new(),
      Some(account) => format!(
        "{}|{}|{}|{}",
        self.s.user_id.as_deref().unwrap_or_default(),
        self.s.device_id.as_deref().unwrap_or_default(),
        account.identity_key(),
        account.signing_key()
      ),
    }
  }

  pub(super) fn store_account(&mut self) -> bool {
    let Some(value) = self.s.account.as_ref().map(OlmAccount::to_value) else { return false };
    self.s.set("e2ee_account", value, Some(CACHE_EXPIRY))
  }

  /// Restore or create this device's account and upload its keys when the
  /// current identity has not done so (upstream `_e2ee_setup()`).
  pub(super) async fn e2ee_setup(&mut self) -> bool {
    if self.s.account.is_none() {
      // A saved identity keeps Matrix seeing the same device.
      self.s.account = self.s.get("e2ee_account").and_then(|v| OlmAccount::from_value(&v));
      if self.s.account.is_none() {
        self.s.account = Some(OlmAccount::new());
        self.store_account();
      }
    }
    // A changed device or account must upload its keys again.
    if self.s.get_str("e2ee_device_binding") != Some(self.e2ee_binding_key()) {
      self.s.clear(&["e2ee_keys_uploaded"]);
    }
    if !matches!(self.s.get("e2ee_keys_uploaded"), Some(Value::Bool(true))) {
      return self.e2ee_upload_keys().await;
    }
    true
  }

  /// Publish the device keys and a fallback key (upstream
  /// `_e2ee_upload_keys()`). No one-time keys: Apprise only sends, and a key
  /// share consumes the recipient's one-time keys, never ours.
  async fn e2ee_upload_keys(&mut self) -> bool {
    let (Some(user_id), Some(device_id)) = (self.s.user_id.clone(), self.s.device_id.clone()) else {
      tracing::warn!("Matrix E2EE: cannot upload keys without user_id and device_id; ensure login completes first.");
      return false;
    };
    let Some(account) = self.s.account.as_mut() else { return false };
    let payload = json!({
      "device_keys": account.device_keys_payload(&user_id, &device_id),
      "fallback_keys": account.fallback_keys_payload(&user_id, &device_id),
    });
    if !self.fetch(Req::post("/keys/upload", payload)).await.ok {
      tracing::warn!("Matrix E2EE: device key upload failed.");
      return false;
    }
    // Keep the fallback key generated for this upload.
    self.store_account();
    self.s.set("e2ee_keys_uploaded", json!(true), Some(CACHE_EXPIRY));
    let binding = self.e2ee_binding_key();
    self.s.set("e2ee_device_binding", json!(binding), Some(CACHE_EXPIRY));
    tracing::debug!("Matrix E2EE: device keys uploaded for {} / {}.", user_id, device_id);
    true
  }

  /// The outbound Megolm session of a room, replaced by a new one when it
  /// reached its rotation threshold (upstream `_e2ee_get_megolm()`).
  fn e2ee_get_megolm(&mut self, room_id: &str) -> Megolm {
    let key = format!("e2ee_megolm_{room_id}");
    if let Some(session) = self.s.get(&key).and_then(|v| Megolm::from_value(&v))
      && !session.should_rotate()
    {
      return session;
    }
    let session = Megolm::new();
    self.s.set(&key, session.to_value(), Some(CACHE_EXPIRY));
    // The new session key must be shared before use.
    self.s.clear(&[&format!("e2ee_key_shared_{room_id}")]);
    session
  }

  fn e2ee_save_megolm(&mut self, room_id: &str, session: &Megolm) {
    self.s.set(&format!("e2ee_megolm_{room_id}"), session.to_value(), Some(CACHE_EXPIRY));
  }

  /// The room's Megolm session, its key shared with the room's devices
  /// first unless this exact session was already announced. A legacy
  /// boolean flag counts as stale, so recipients that missed the key get it.
  async fn e2ee_shared_megolm(&mut self, room_id: &str) -> Option<Megolm> {
    let session = self.e2ee_get_megolm(room_id);
    let flag = format!("e2ee_key_shared_{room_id}");
    if self.s.get_str(&flag) != Some(session.session_id()) {
      if !self.e2ee_share_room_key(room_id, &session).await {
        return None;
      }
      self.s.set(&flag, json!(session.session_id()), Some(CACHE_EXPIRY));
    }
    Some(session)
  }

  /// The verified device keys of every joined member (upstream
  /// `_e2ee_room_members()`); `None` on an HTTP failure.
  async fn e2ee_room_members(&mut self, room_id: &str) -> Option<BTreeMap<String, BTreeMap<String, DeviceKeys>>> {
    let reply = self.fetch(Req::get(format!("/rooms/{}/joined_members", quote(room_id, "/")))).await;
    if !(reply.ok && reply.body.is_object()) {
      return None;
    }
    let members: Vec<String> = reply.body.get("joined").and_then(Value::as_object).map(|j| j.keys().cloned().collect()).unwrap_or_default();
    if members.is_empty() {
      return Some(BTreeMap::new());
    }
    let query: serde_json::Map<String, Value> = members.iter().map(|m| (m.clone(), json!([]))).collect();
    let reply = self.fetch(Req::post("/keys/query", json!({"device_keys": query}))).await;
    if !(reply.ok && reply.body.is_object()) {
      return None;
    }
    let mut result = BTreeMap::new();
    for (user_id, devices) in reply.body.get("device_keys").and_then(Value::as_object).into_iter().flatten() {
      let entry: &mut BTreeMap<String, DeviceKeys> = result.entry(user_id.clone()).or_default();
      for (device_id, info) in devices.as_object().into_iter().flatten() {
        if !e2ee::verify_device_keys(info, user_id, device_id) {
          tracing::debug!("Matrix E2EE: device key signature invalid for {} / {}; device skipped.", user_id, device_id);
          continue;
        }
        let key = |name: &str| info.get("keys").and_then(|k| k.get(format!("{name}:{device_id}"))).and_then(Value::as_str).unwrap_or_default().to_string();
        entry.insert(device_id.clone(), DeviceKeys { curve25519: key("curve25519"), ed25519: key("ed25519") });
      }
    }
    Some(result)
  }

  /// Send the Megolm session key to every device in the room over Olm
  /// (upstream `_e2ee_share_room_key()`): query member devices, claim a
  /// one-time key for each, encrypt an `m.room_key` per device and deliver
  /// them with `/sendToDevice`. Devices that cannot be reached are skipped.
  async fn e2ee_share_room_key(&mut self, room_id: &str, session: &Megolm) -> bool {
    let Some(members) = self.e2ee_room_members(room_id).await else {
      tracing::warn!("Matrix E2EE: failed to query room members for {}.", room_id);
      return false;
    };
    if members.is_empty() {
      tracing::trace!("Matrix E2EE: no room members found for {}; skipping key share.", room_id);
      return true;
    }

    // signed_curve25519 is what clients publish; plain curve25519 is gone.
    let claim: serde_json::Map<String, Value> =
      members.iter().map(|(user, devices)| (user.clone(), Value::Object(devices.keys().map(|d| (d.clone(), json!("signed_curve25519"))).collect()))).collect();
    let reply = self.fetch(Req::post("/keys/claim", json!({"one_time_keys": claim}))).await;
    if !reply.ok {
      tracing::warn!("Matrix E2EE: failed to claim one-time keys.");
      return false;
    }
    if let Some(failures) = reply.body.get("failures").and_then(Value::as_object).filter(|f| !f.is_empty()) {
      tracing::debug!("Matrix E2EE: /keys/claim reported failures for server(s): {:?}", failures.keys().collect::<Vec<_>>());
    }
    let otks = reply.body.get("one_time_keys").cloned().unwrap_or_else(|| json!({}));

    let (own_user, own_device) = (self.s.user_id.clone().unwrap_or_default(), self.s.device_id.clone().unwrap_or_default());
    let Some(account) = self.s.account.as_ref() else { return false };
    let room_key = json!({
      "algorithm": MEGOLM_ALGORITHM,
      "room_id": room_id,
      "session_id": session.session_id(),
      "session_key": session.session_key(),
    });

    let mut messages = serde_json::Map::new();
    let (mut built, mut skipped_no_otk) = (0usize, 0usize);
    for (user_id, devices) in &members {
      let mut per_user = serde_json::Map::new();
      for (device_id, keys) in devices {
        // No Olm session with ourselves.
        if *user_id == own_user && *device_id == own_device {
          continue;
        }
        if keys.curve25519.is_empty() {
          continue;
        }
        // The first signed_curve25519 key object, with a valid signature.
        let mut otk = None;
        if let Some(entries) = otks.get(user_id).and_then(|u| u.get(device_id)).and_then(Value::as_object)
          && let Some((key_id, object)) = entries.iter().find(|(k, _)| k.starts_with("signed_curve25519:"))
        {
          if object.is_object() && !keys.ed25519.is_empty() && e2ee::verify_signed_otk(object, user_id, device_id, &keys.ed25519) {
            otk = object.get("key").and_then(Value::as_str).map(str::to_string);
            tracing::trace!("Matrix E2EE: OTK accepted for {} / {} (key_id={}).", user_id, device_id, key_id);
          } else {
            tracing::debug!("Matrix E2EE: OTK signature invalid for {} / {}; skipped.", user_id, device_id);
          }
        }
        let Some(otk) = otk else {
          skipped_no_otk += 1;
          continue;
        };
        // Only the fields the spec requires; recipients fetch device keys
        // themselves.
        let inner = json!({
          "type": "m.room_key",
          "content": room_key,
          "sender": own_user,
          "recipient": user_id,
          "recipient_keys": {"ed25519": keys.ed25519},
          "keys": {"ed25519": account.signing_key()},
        });
        let ciphertext = match account.encrypt_for_device(&keys.curve25519, &otk, &inner.to_string()) {
          Ok(c) => c,
          Err(e) => {
            tracing::debug!("Matrix E2EE: failed to build Olm session for {} / {}: {}", user_id, device_id, e);
            continue;
          }
        };
        built += 1;
        per_user.insert(
          device_id.clone(),
          json!({
            "algorithm": e2ee::OLM_ALGORITHM,
            "ciphertext": {keys.curve25519.clone(): ciphertext},
            "sender_key": account.identity_key(),
          }),
        );
      }
      messages.insert(user_id.clone(), Value::Object(per_user));
    }
    if skipped_no_otk > 0 {
      // The recipient owns its key pool; only its own clients refill it.
      tracing::warn!(
        "Matrix E2EE: {} device(s) in {} had no one-time key available; those device(s) will not be able to decrypt this message.",
        skipped_no_otk,
        room_id
      );
    }
    if built == 0 {
      return true;
    }

    // Upstream advances before this event, so the next room event reuses
    // its ID; every event gets its own here.
    let path = format!("/sendToDevice/m.room.encrypted/{}", self.txn());
    let ok = self.fetch(Req::put(path, json!({"messages": messages}))).await.ok;
    self.advance_txn();
    if !ok {
      tracing::warn!("Matrix E2EE: failed to deliver room key to devices in {}.", room_id);
      return false;
    }
    tracing::debug!("Matrix E2EE: room key delivered to {} device(s) in {}.", built, room_id);
    true
  }

  /// The `m.room.encrypted` content for an inner event.
  fn e2ee_envelope(&mut self, room_id: &str, session: &mut Megolm, content: Value) -> Value {
    let inner = json!({"type": "m.room.message", "content": content, "room_id": room_id});
    let ciphertext = session.encrypt(&inner);
    self.e2ee_save_megolm(room_id, session);
    json!({
      "algorithm": MEGOLM_ALGORITHM,
      "ciphertext": ciphertext,
      "sender_key": self.s.account.as_ref().map(OlmAccount::identity_key).unwrap_or_default(),
      "session_id": session.session_id(),
      "device_id": self.s.device_id.clone().unwrap_or_default(),
    })
  }

  /// Encrypt and send the notification to one room (upstream
  /// `_e2ee_send_to_room()`).
  pub(super) async fn e2ee_send_to_room(&mut self, room_id: &str) -> bool {
    let Some(mut session) = self.e2ee_shared_megolm(room_id).await else { return false };
    let envelope = self.e2ee_envelope(room_id, &mut session, self.message_content());
    // A device ID learned after sizing may push the event past the limit.
    let size = envelope.to_string().len();
    let budget = MATRIX_EVENT_BYTE_LIMIT - MATRIX_EVENT_SAFETY_MARGIN;
    if size > budget {
      tracing::warn!(
        "Matrix E2EE: encrypted event for room {} is {} bytes, over the safe {}-byte budget (device_id is {} characters); message not sent.",
        room_id,
        size,
        budget,
        self.s.device_id.as_deref().unwrap_or_default().chars().count()
      );
      return false;
    }
    let path = self.message_path(room_id, "m.room.encrypted");
    let ok = self.fetch(Req::put(path, envelope)).await.ok;
    self.advance_txn();
    ok
  }

  /// Encrypt an attachment, upload the ciphertext and send the encrypted
  /// event that references it (upstream `_e2ee_send_attachment()`).
  pub(super) async fn e2ee_send_attachment(&mut self, attachment: &Attachment, room_id: &str) -> bool {
    let (ciphertext, mut file_info) = e2ee::encrypt_attachment(&attachment.data);
    let name = if attachment.name.is_empty() { "file" } else { attachment.name.as_str() };

    let Ok(base) = self.base_url().await else { return false };
    let upload_url = format!("{base}{}/upload", self.m.media_path());
    tracing::debug!("Matrix E2EE: uploading encrypted attachment (name={} size={}).", name, ciphertext.len());
    let client = match client_builder(self.m.verify_certificate, ClientDefaults::default()).build() {
      Ok(c) => c,
      Err(_) => return false,
    };
    let mut request = client
      .post(&upload_url)
      .query(&[("filename", name)])
      .header("User-Agent", APP_ID)
      .header("Content-Type", "application/octet-stream")
      .header("Accept", "application/json")
      .body(ciphertext);
    if let Some(token) = &self.s.access_token {
      request = request.header("Authorization", format!("Bearer {token}"));
    }
    throttle().await;
    let response = match request.send().await {
      Ok(r) => r,
      Err(e) => {
        tracing::warn!("Matrix E2EE: connection error uploading encrypted attachment.");
        tracing::debug!("Socket Exception: {}", crate::error::loggable_http_error(&e));
        return false;
      }
    };
    let status = response.status().as_u16();
    let body: Value = response.bytes().await.ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({}));
    let Some(content_uri) = body.get("content_uri").and_then(Value::as_str).filter(|_| status == 200) else {
      tracing::warn!("Matrix E2EE: media upload failed (HTTP {}).", status);
      return false;
    };
    file_info["url"] = json!(content_uri);

    let is_image = attachment.mime_type.to_lowercase().starts_with("image/");
    let mut content = json!({
      "msgtype": if is_image { "m.image" } else { "m.file" },
      "body": name,
      "file": file_info,
      "info": {"mimetype": attachment.mime_type, "size": attachment.data.len()},
    });
    if !is_image {
      content["filename"] = json!(name);
    }
    // A session rotated since the message was sent is shared first.
    let Some(mut session) = self.e2ee_shared_megolm(room_id).await else { return false };
    let envelope = self.e2ee_envelope(room_id, &mut session, content);
    let path = self.message_path(room_id, "m.room.encrypted");
    let ok = self.fetch(Req::put(path, envelope)).await.ok;
    self.advance_txn();
    ok
  }
}
