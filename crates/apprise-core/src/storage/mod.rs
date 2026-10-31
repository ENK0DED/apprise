//! Persistent storage (upstream `apprise/persistent_store.py`).
//!
//! Every service gets its own namespace below the storage path, named after
//! its URL ID ([`crate::notify::options::url_id`]):
//!
//! ```text
//! <path>/<namespace>/cache.psdata      gzip JSON of cached key/value pairs
//! <path>/<namespace>/var/<key>.psdata  files written with `write()`
//! <path>/<namespace>/tmp/              scratch space for atomic writes
//! ```
//!
//! The layout and the cache format match upstream, so both implementations
//! can share a storage directory.
//!
//! Plugins reach their own store from inside `send()` with
//! [`with_service_store`]:
//!
//! ```ignore
//! let token = crate::storage::with_service_store(|s| s.get("token")).flatten();
//! // ... after fetching a fresh token valid for `ttl` seconds:
//! crate::storage::with_service_store(|s| s.set("token", json!(token), Some(ttl), true));
//! ```
//!
//! It returns `None` when the service is not being sent by `Apprise` (in a
//! plugin's own unit tests, for example), so callers must work without it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::types::StorageMode;

/// Largest total size a namespace may grow to through `write()` (1 MiB).
pub const MAX_FILE_SIZE: u64 = 1_048_576;

/// Largest a namespace's cache may be as JSON, before compression (1 MiB).
/// `set()` refuses a value that would make it larger, so every cache this
/// store writes can be read back. Upstream sets no limit; a larger cache
/// file it wrote is left in place and not used.
pub const MAX_CACHE_SIZE: u64 = 1_048_576;

/// Age after which `disk_prune()` removes files by default, in seconds.
pub const DEFAULT_FILE_EXPIRY: u64 = 2_678_400;

/// Key used by `read()`/`write()` when none is given.
pub const BASE_KEY: &str = "default";

const CACHE_KEY: &str = "cache";
const TEMP_DIR: &str = "tmp";
const DATA_DIR: &str = "var";
const EXTENSION: &str = ".psdata";
const BACKUP_EXTENSION: &str = "._psbak";
const HASH_LENGTH: usize = 6;

/// Whether `key` is usable as a namespace or file key: it starts with a
/// letter or digit and continues with letters, digits, `.`, `_` or `-`.
pub fn is_valid_key(key: &str) -> bool {
  let mut bytes = key.bytes();
  matches!(bytes.next(), Some(b) if b.is_ascii_alphanumeric()) && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// What a store directory holds relative to the loaded services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreState {
  /// A loaded service uses this namespace and it holds data.
  Active,
  /// Data on disk that no loaded service refers to.
  Stale,
  /// A loaded service could use this namespace, but nothing is stored yet.
  Unused,
}

impl StoreState {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Active => "active",
      Self::Stale => "stale",
      Self::Unused => "unused",
    }
  }
}

/// Errors creating a store.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum StorageError {
  #[error("Persistent Storage namespace ({0}) provided is invalid")]
  InvalidNamespace(String),
  #[error("Persistent Storage key ({0}) provided is invalid")]
  InvalidKey(String),
}

/// A cached value as upstream's JSON decoder reads it: object members keep
/// their order, as a Python dict does. Upstream hashes the value's `str()`,
/// so the order is part of the checksum and must survive a rewrite.
#[derive(Debug, Clone, PartialEq)]
enum Stored {
  Null,
  Bool(bool),
  Number(serde_json::Number),
  String(String),
  Array(Vec<Stored>),
  Object(Vec<(String, Stored)>),
}

impl From<&Value> for Stored {
  fn from(value: &Value) -> Self {
    match value {
      Value::Null => Self::Null,
      Value::Bool(b) => Self::Bool(*b),
      Value::Number(n) => Self::Number(n.clone()),
      Value::String(s) => Self::String(s.clone()),
      Value::Array(items) => Self::Array(items.iter().map(Self::from).collect()),
      Value::Object(map) => Self::Object(map.iter().map(|(k, v)| (k.clone(), Self::from(v))).collect()),
    }
  }
}

impl Stored {
  fn to_value(&self) -> Value {
    match self {
      Self::Null => Value::Null,
      Self::Bool(b) => Value::Bool(*b),
      Self::Number(n) => Value::Number(n.clone()),
      Self::String(s) => Value::String(s.clone()),
      Self::Array(items) => Value::Array(items.iter().map(Self::to_value).collect()),
      Self::Object(members) => Value::Object(members.iter().map(|(k, v)| (k.clone(), v.to_value())).collect()),
    }
  }

  fn member(&self, key: &str) -> Option<&Stored> {
    match self {
      Self::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
      _ => None,
    }
  }
}

impl serde::Serialize for Stored {
  fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    match self {
      Self::Null => serializer.serialize_unit(),
      Self::Bool(b) => serializer.serialize_bool(*b),
      Self::Number(n) => n.serialize(serializer),
      Self::String(s) => serializer.serialize_str(s),
      Self::Array(items) => serializer.collect_seq(items),
      Self::Object(members) => {
        let mut map = serializer.serialize_map(Some(members.len()))?;
        for (k, v) in members {
          map.serialize_entry(k, v)?;
        }
        map.end()
      }
    }
  }
}

impl<'de> serde::Deserialize<'de> for Stored {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    struct Visit;
    impl<'de> serde::de::Visitor<'de> for Visit {
      type Value = Stored;

      fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value")
      }
      fn visit_unit<E>(self) -> Result<Stored, E> {
        Ok(Stored::Null)
      }
      fn visit_bool<E>(self, b: bool) -> Result<Stored, E> {
        Ok(Stored::Bool(b))
      }
      fn visit_i64<E>(self, n: i64) -> Result<Stored, E> {
        Ok(Stored::Number(n.into()))
      }
      fn visit_u64<E>(self, n: u64) -> Result<Stored, E> {
        Ok(Stored::Number(n.into()))
      }
      fn visit_f64<E>(self, n: f64) -> Result<Stored, E> {
        Ok(serde_json::Number::from_f64(n).map_or(Stored::Null, Stored::Number))
      }
      fn visit_str<E>(self, s: &str) -> Result<Stored, E> {
        Ok(Stored::String(s.to_string()))
      }
      fn visit_string<E>(self, s: String) -> Result<Stored, E> {
        Ok(Stored::String(s))
      }
      fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Stored, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
          items.push(item);
        }
        Ok(Stored::Array(items))
      }
      fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Stored, A::Error> {
        let mut members: Vec<(String, Stored)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Stored>()? {
          // A repeated name keeps its first position and its last value,
          // as a Python dict does.
          match members.iter_mut().find(|(k, _)| *k == key) {
            Some(member) => member.1 = value,
            None => members.push((key, value)),
          }
        }
        Ok(Stored::Object(members))
      }
    }
    deserializer.deserialize_any(Visit)
  }
}

/// One cached value and when it expires.
#[derive(Debug, Clone, PartialEq)]
struct CacheObject {
  value: Stored,
  /// Upstream's Python class name, kept so values round-trip.
  class_name: String,
  expires: Option<DateTime<Utc>>,
  persistent: bool,
  /// Bytes the entry takes in the cache file.
  size: u64,
}

/// A cache entry as it is written to disk, in upstream's member order.
#[derive(serde::Serialize)]
struct CacheEntry<'a> {
  v: &'a Stored,
  x: Option<f64>,
  c: &'a str,
  #[serde(rename = "!")]
  hash: String,
}

impl CacheObject {
  fn new(value: &Value, expires: Option<Duration>, persistent: bool) -> Self {
    // An expiry too far out for a timestamp (a server's huge expires_in)
    // means never.
    let expires = expires.and_then(|d| chrono::Duration::from_std(d).ok()).and_then(|d| Utc::now().checked_add_signed(d));
    Self::build(Stored::from(value), class_name(value).to_string(), expires, persistent)
  }

  fn build(value: Stored, class_name: String, expires: Option<DateTime<Utc>>, persistent: bool) -> Self {
    let mut co = Self { value, class_name, expires, persistent, size: 0 };
    co.size = serde_json::to_vec(&co.to_entry()).map_or(0, |v| v.len() as u64);
    co
  }

  fn is_live(&self) -> bool {
    self.expires.is_none_or(|e| e > Utc::now())
  }

  /// Upstream `str(CacheObject)`, which the integrity hash covers.
  fn describe(&self) -> String {
    let expires = self.expires.map(|e| e.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()).unwrap_or_else(|| "never".to_string());
    let value = match (&self.value, self.class_name.as_str()) {
      (Stored::String(s), "str" | "bytes" | "aware_datetime" | "naive_datetime" | "datetime") => s.clone(),
      (v, _) => py_str(v),
    };
    format!("{}:{}:{} expires: {}", self.class_name, if self.persistent { "+" } else { "-" }, value, expires)
  }

  fn hash(&self) -> String {
    hex::encode(Sha256::digest(self.describe().as_bytes()))[..HASH_LENGTH].to_string()
  }

  fn to_entry(&self) -> CacheEntry<'_> {
    let x = self.expires.map(|e| e.timestamp() as f64 + f64::from(e.timestamp_subsec_micros()) / 1e6);
    CacheEntry { v: &self.value, x, c: &self.class_name, hash: self.hash() }
  }

  /// Load an entry written by `to_entry()` (or by upstream), or `None` when
  /// it is malformed or fails its integrity check.
  fn from_json(content: &Stored) -> Option<Self> {
    let Stored::Object(_) = content else { return None };
    let value = content.member("v")?.clone();
    let expires = match content.member("x")? {
      Stored::Null => None,
      Stored::Number(x) => {
        let micros = (x.as_f64()? * 1e6).round() as i64;
        Some(Utc.timestamp_micros(micros).single()?)
      }
      _ => return None,
    };
    let class_name = match content.member("c") {
      None => String::new(),
      Some(Stored::String(c)) => c.clone(),
      Some(_) => return None,
    };
    let hashsum = match content.member("!") {
      None => String::new(),
      Some(Stored::String(h)) => h.clone(),
      Some(_) => return None,
    };
    let co = Self::build(value, class_name, expires, true);
    if co.hash() != hashsum {
      tracing::debug!("Tampering detected with a persistent cache entry");
      return None;
    }
    Some(co)
  }
}

/// Upstream's Python class name for a JSON value.
fn class_name(value: &Value) -> &'static str {
  match value {
    Value::Null => "NoneType",
    Value::Bool(_) => "bool",
    Value::Number(n) if n.is_f64() => "float",
    Value::Number(_) => "int",
    Value::String(_) => "str",
    Value::Array(_) => "list",
    Value::Object(_) => "dict",
  }
}

/// Python's `str()` of a JSON-like value, as upstream hashes it.
fn py_str(value: &Stored) -> String {
  match value {
    Stored::String(s) => s.clone(),
    other => py_repr(other),
  }
}

fn py_repr(value: &Stored) -> String {
  match value {
    Stored::Null => "None".to_string(),
    Stored::Bool(true) => "True".to_string(),
    Stored::Bool(false) => "False".to_string(),
    Stored::Number(n) => match n.as_f64() {
      Some(f) if n.is_f64() => py_float(f),
      _ => n.to_string(),
    },
    Stored::String(s) => py_repr_str(s),
    Stored::Array(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
    Stored::Object(members) => format!("{{{}}}", members.iter().map(|(k, v)| format!("{}: {}", py_repr_str(k), py_repr(v))).collect::<Vec<_>>().join(", ")),
  }
}

fn py_float(f: f64) -> String {
  if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
    format!("{:.1}", f)
  } else {
    let s = format!("{:e}", f);
    // Rust writes 1e20 where Python writes 1e+20.
    match s.split_once('e') {
      Some((m, e)) if (f.abs() >= 1e16 || f.abs() < 1e-4) && f != 0.0 => {
        format!("{}e{}{:0>2}", m, if e.starts_with('-') { "-" } else { "+" }, e.trim_start_matches('-'))
      }
      _ => f.to_string(),
    }
  }
}

fn py_repr_str(s: &str) -> String {
  let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
  let mut out = String::with_capacity(s.len() + 2);
  out.push(quote);
  for c in s.chars() {
    match c {
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      c if c == quote => {
        out.push('\\');
        out.push(c);
      }
      c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
      c => out.push(c),
    }
  }
  out.push(quote);
  out
}

/// One file `disk_prune()` matched.
#[derive(Debug, Clone, PartialEq)]
pub struct PruneRecord {
  pub path: PathBuf,
  pub removed: bool,
}

/// A namespaced key/value cache and file store for one service.
#[derive(Debug)]
pub struct PersistentStore {
  mode: StorageMode,
  base_path: Option<PathBuf>,
  cache: Option<HashMap<String, CacheObject>>,
  dirty: bool,
  /// Files whose timestamps are refreshed on the next flush so pruning
  /// keeps them.
  renew: HashSet<PathBuf>,
  cache_size: Option<u64>,
}

impl PersistentStore {
  /// Open `namespace` below `path`. Without a path the store lives in memory.
  /// If the directories cannot be created the store falls back to memory.
  pub fn new(path: Option<&Path>, namespace: &str, mode: StorageMode) -> Result<Self, StorageError> {
    if !is_valid_key(namespace) {
      return Err(StorageError::InvalidNamespace(namespace.to_string()));
    }
    let (base_path, mode) = match path {
      Some(p) => (Some(p.join(namespace)), mode),
      None => (None, StorageMode::Memory),
    };
    let mut store = Self { mode, base_path, cache: None, dirty: false, renew: HashSet::new(), cache_size: None };
    store.prepare(true);
    Ok(store)
  }

  /// The operating mode (it becomes `Memory` when the disk is unusable).
  pub fn mode(&self) -> StorageMode {
    self.mode.clone()
  }

  /// The namespace directory, or `None` in memory mode.
  pub fn path(&self) -> Option<&Path> {
    self.base_path.as_deref().filter(|_| self.mode != StorageMode::Memory)
  }

  /// The cache file of this namespace.
  pub fn cache_file(&self) -> Option<PathBuf> {
    self.base_path.as_ref().map(|b| b.join(format!("{CACHE_KEY}{EXTENSION}")))
  }

  fn temp_path(&self) -> Option<PathBuf> {
    self.base_path.as_ref().map(|b| b.join(TEMP_DIR))
  }

  fn data_path(&self) -> Option<PathBuf> {
    self.base_path.as_ref().map(|b| b.join(DATA_DIR))
  }

  fn data_file(&self, key: &str) -> Option<PathBuf> {
    self.data_path().map(|d| d.join(format!("{key}{EXTENSION}")))
  }

  fn prepare(&mut self, flush: bool) {
    if self.mode == StorageMode::Memory {
      return;
    }
    let dirs = [self.base_path.clone(), self.temp_path(), self.data_path()];
    for dir in dirs.into_iter().flatten() {
      if let Err(e) = create_dir(&dir) {
        tracing::debug!("Could not create persistent store directory {}", dir.display());
        tracing::debug!("Persistent Storage Exception: {}", e);
        self.mode = StorageMode::Memory;
      }
    }
    if self.mode == StorageMode::Memory {
      tracing::warn!("The persistent storage could not be fully initialized; operating in MEMORY mode");
    } else if self.cache.as_ref().is_some_and(|c| !c.is_empty()) {
      self.dirty = true;
      tracing::warn!("The persistent storage environment was disrupted");
      if self.mode == StorageMode::Flush && flush {
        self.flush_inner(true, true);
      }
    }
  }

  // ── Cache ──────────────────────────────────────────────────────────────

  fn load_cache(&mut self) -> bool {
    if self.cache.is_some() {
      return true;
    }
    self.dirty = false;
    if self.mode == StorageMode::Memory {
      self.cache = Some(HashMap::new());
      return true;
    }
    let Some(cache_file) = self.cache_file() else { return false };
    match read_gzip(&cache_file, MAX_CACHE_SIZE) {
      Ok(bytes) if bytes.len() as u64 > MAX_CACHE_SIZE => {
        // Larger than this store ever writes: not corrupt, so not deleted.
        tracing::warn!("Persistent cache content exceeds allowable maximum length ({}KB); leaving it unused: {}", MAX_CACHE_SIZE / 1024, cache_file.display());
        return false;
      }
      Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
        Ok(Stored::Object(entries)) => {
          let mut cache = HashMap::new();
          for (k, v) in entries {
            match CacheObject::from_json(&v) {
              Some(co) => {
                cache.insert(k, co);
              }
              None => self.dirty = true,
            }
          }
          self.cache = Some(cache);
        }
        Ok(_) | Err(_) => return self.recover_cache(&cache_file),
      },
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.cache = Some(HashMap::new()),
      Err(e) if matches!(e.kind(), std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput | std::io::ErrorKind::UnexpectedEof) => {
        return self.recover_cache(&cache_file);
      }
      Err(e) => {
        tracing::warn!("Could not load persistent cache for namespace {}", self.namespace());
        tracing::debug!("Persistent Storage Exception: {}", e);
        return false;
      }
    }
    true
  }

  /// A corrupted cache file is removed and the cache starts empty.
  fn recover_cache(&mut self, cache_file: &Path) -> bool {
    tracing::warn!("Corrupted access persistent cache content: {}", cache_file.display());
    match fs::remove_file(cache_file) {
      Ok(()) => {}
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
      Err(e) => {
        tracing::warn!("Could not remove persistent cache content: {}", cache_file.display());
        tracing::debug!("Persistent Storage Exception: {}", e);
        return false;
      }
    }
    self.cache = Some(HashMap::new());
    true
  }

  fn namespace(&self) -> String {
    self.base_path.as_ref().and_then(|b| b.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
  }

  /// A cached value, if present and not expired. Reading a key keeps the
  /// cache file from being pruned.
  pub fn get(&mut self, key: &str) -> Option<Value> {
    if !self.load_cache() {
      return None;
    }
    let present = self.cache.as_ref().is_some_and(|c| c.contains_key(key));
    if present
      && self.mode != StorageMode::Memory
      && !self.dirty
      && let Some(f) = self.cache_file()
    {
      self.renew.insert(f);
    }
    self.cache.as_ref()?.get(key).filter(|co| co.is_live()).map(|co| co.value.to_value())
  }

  /// Cache `value` under `key`, expiring after `expires` (never when
  /// `None`). A non-persistent value stays in memory only. Returns `false`
  /// when the cache could not be loaded or a flush failed.
  pub fn set(&mut self, key: &str, value: Value, expires: Option<Duration>, persistent: bool) -> bool {
    if !self.load_cache() {
      return false;
    }
    let new = CacheObject::new(&value, expires, persistent);
    let cache = self.cache.as_ref().expect("cache loaded");
    if cache.get(key).is_some_and(|old| old.describe() == new.describe()) {
      return true;
    }
    if self.mode != StorageMode::Memory && new.persistent && new.is_live() {
      let size = cache_size(cache.iter().filter(|(k, _)| k.as_str() != key).chain(std::iter::once((&key.to_string(), &new))));
      if size > MAX_CACHE_SIZE {
        tracing::warn!("Persistent cache content exceeds allowable maximum length ({}KB); key not saved: {}", MAX_CACHE_SIZE / 1024, key);
        return false;
      }
    }
    let cache = self.cache.as_mut().expect("cache loaded");
    // Replacing a saved entry changes what is on disk too.
    let replaced = cache.get(key).is_some_and(|old| old.persistent);
    cache.insert(key.to_string(), new);
    // Never clear the flag here: an earlier change may still be pending.
    self.dirty = self.dirty || persistent || replaced;
    if self.dirty && self.mode == StorageMode::Flush {
      return self.flush();
    }
    true
  }

  /// Whether `key` holds a value that has not expired.
  pub fn contains(&mut self, key: &str) -> bool {
    self.load_cache() && self.cache.as_ref().and_then(|c| c.get(key)).is_some_and(CacheObject::is_live)
  }

  /// Remove `key` from the cache. Returns whether it was there.
  pub fn remove(&mut self, key: &str) -> bool {
    if !self.load_cache() {
      return false;
    }
    let Some(old) = self.cache.as_mut().and_then(|c| c.remove(key)) else { return false };
    if old.persistent {
      self.dirty = true;
    }
    if self.dirty && self.mode == StorageMode::Flush {
      self.flush();
    }
    true
  }

  /// Remove the given cache keys, or every key when `keys` is empty.
  pub fn clear(&mut self, keys: &[&str]) -> bool {
    if !self.load_cache() {
      return false;
    }
    let cache = self.cache.as_mut().expect("cache loaded");
    if keys.is_empty() {
      if !cache.is_empty() {
        cache.clear();
        self.dirty = true;
      }
    } else {
      for key in keys {
        if cache.remove(*key).is_some() {
          self.dirty = true;
        }
      }
    }
    if self.dirty && self.mode == StorageMode::Flush {
      return self.flush();
    }
    true
  }

  /// Drop expired entries. Returns whether anything persistent changed.
  pub fn prune(&mut self) -> bool {
    if !self.load_cache() {
      return false;
    }
    let cache = self.cache.as_mut().expect("cache loaded");
    let expired: Vec<String> = cache.iter().filter(|(_, co)| !co.is_live()).map(|(k, _)| k.clone()).collect();
    let mut change = false;
    for key in expired {
      if cache.remove(&key).is_some_and(|co| co.persistent) {
        change = true;
        self.dirty = true;
      }
    }
    if self.dirty && self.mode == StorageMode::Flush {
      return self.flush();
    }
    change
  }

  /// The cached keys (expired ones included until pruned).
  pub fn keys(&mut self) -> Vec<String> {
    if !self.load_cache() {
      return Vec::new();
    }
    let mut keys: Vec<String> = self.cache.as_ref().map(|c| c.keys().cloned().collect()).unwrap_or_default();
    keys.sort();
    keys
  }

  /// Write pending cache changes to disk and refresh the timestamps of
  /// files read since the last flush.
  pub fn flush(&mut self) -> bool {
    self.flush_inner(false, false)
  }

  fn flush_inner(&mut self, force: bool, recovery: bool) -> bool {
    if self.mode == StorageMode::Memory {
      return true;
    }
    let now = SystemTime::now();
    for path in self.renew.drain() {
      match fs::File::options().append(true).open(&path).and_then(|f| f.set_modified(now)) {
        Ok(()) => tracing::trace!("file timestamp updated: {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
          tracing::debug!("Could not update file timestamp: {}", path.display());
          tracing::debug!("Persistent Storage Exception: {}", e);
        }
      }
    }
    if self.cache.is_none() {
      return true;
    }
    if !force && !self.dirty {
      tracing::trace!("Persistent cache is consistent with memory map");
      return true;
    }
    if recovery {
      self.prepare(false);
    }
    self.cache_size = None;
    let (Some(cache_file), Some(temp)) = (self.cache_file(), self.temp_path()) else { return false };
    let entries: BTreeMap<&String, CacheEntry<'_>> =
      self.cache.iter().flatten().filter(|(_, co)| co.is_live() && co.persistent).map(|(k, co)| (k, co.to_entry())).collect();
    if entries.is_empty() {
      // Nothing to keep: move the old cache file aside.
      let backup = backup_path(&cache_file);
      if let Err(e) = remove_if_exists(&backup) {
        tracing::warn!("Could not remove persistent cache backup: {}", backup.display());
        tracing::debug!("Persistent Storage Exception: {}", e);
        return false;
      }
      match fs::rename(&cache_file, &backup) {
        Ok(()) => tracing::trace!("Persistent cache backup file created: {}", backup.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
          tracing::warn!("Could not remove stale persistent cache file: {}", cache_file.display());
          tracing::debug!("Persistent Storage Exception: {}", e);
          return false;
        }
      }
      self.dirty = false;
      return true;
    }
    let data = serde_json::to_vec(&entries).unwrap_or_default();
    if data.len() as u64 > MAX_CACHE_SIZE {
      // set() keeps the cache below this; never write what cannot be read.
      tracing::error!("Persistent cache content exceeds allowable maximum length ({}KB); not written", MAX_CACHE_SIZE / 1024);
      return false;
    }
    let tmp = match write_temp(&temp, &data, true) {
      Ok(tmp) => tmp,
      Err(e) if e.kind() == std::io::ErrorKind::NotFound && !recovery => return self.flush_inner(true, true),
      Err(e) => {
        tracing::error!("Persistent temporary directory inaccessible: {}", temp.display());
        tracing::debug!("Persistent Storage Exception: {}", e);
        return false;
      }
    };
    if !install(&tmp, &cache_file) {
      let _ = fs::remove_file(&tmp);
      return false;
    }
    self.dirty = false;
    true
  }

  // ── Files ──────────────────────────────────────────────────────────────

  /// Read the file stored under `key` (the default key when `None`),
  /// gunzipping it when `compress` is set. Unless `renew` is false, reading
  /// keeps the file from being pruned.
  pub fn read(&mut self, key: Option<&str>, compress: bool, renew: bool) -> Option<Vec<u8>> {
    let key = key.unwrap_or(BASE_KEY);
    if !is_valid_key(key) || self.mode == StorageMode::Memory {
      return None;
    }
    let path = self.data_file(key)?;
    let result = if compress { read_gzip(&path, MAX_FILE_SIZE) } else { fs::read(&path) };
    match result {
      Ok(mut data) => {
        data.truncate(MAX_FILE_SIZE as usize);
        if renew {
          self.renew.insert(path);
        }
        Some(data)
      }
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
      Err(e) => {
        tracing::warn!("Could not read with persistent key: {}", key);
        tracing::debug!("Persistent Storage Exception: {}", e);
        None
      }
    }
  }

  /// Store `data` under `key` (the default key when `None`), gzipped when
  /// `compress` is set. Fails when the namespace would grow past
  /// [`MAX_FILE_SIZE`], and always in memory mode.
  pub fn write(&mut self, data: &[u8], key: Option<&str>, compress: bool) -> Result<bool, StorageError> {
    let key = key.unwrap_or(BASE_KEY);
    if !is_valid_key(key) {
      return Err(StorageError::InvalidKey(key.to_string()));
    }
    Ok(self.write_inner(data, key, compress, false))
  }

  fn write_inner(&mut self, data: &[u8], key: &str, compress: bool, recovery: bool) -> bool {
    if self.mode == StorageMode::Memory {
      return false;
    }
    if recovery {
      self.prepare(true);
    }
    let (Some(io_file), Some(temp)) = (self.data_file(key), self.temp_path()) else { return false };
    let prev_size = match fs::metadata(&io_file) {
      Ok(m) => m.len(),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
      Err(e) => {
        tracing::warn!("Could not write with persistent key: {}", key);
        tracing::debug!("Persistent Storage Exception: {}", e);
        return false;
      }
    };
    let tmp = match write_temp(&temp, data, compress) {
      Ok(tmp) => tmp,
      Err(e) if e.kind() == std::io::ErrorKind::NotFound && !recovery => return self.write_inner(data, key, compress, true),
      Err(e) => {
        tracing::warn!("Could not write to persistent key: {}", key);
        tracing::debug!("Persistent Storage Exception: {}", e);
        return false;
      }
    };
    let new_size = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    tracing::trace!("Wrote {} bytes of data to persistent key: {}", new_size, key);
    if (new_size + self.size()).saturating_sub(prev_size) > MAX_FILE_SIZE {
      tracing::warn!("Persistent content exceeds allowable maximum file length ({}KB); provide {}KB", MAX_FILE_SIZE / 1024, new_size / 1024);
      let _ = fs::remove_file(&tmp);
      return false;
    }
    if !install(&tmp, &io_file) {
      let _ = fs::remove_file(&tmp);
      return false;
    }
    self.cache_size = None;
    true
  }

  /// Delete stored files: the data files for `keys`, the scratch files when
  /// `temp` is set, and the cache when `cache` is set. With no keys and
  /// neither flag, everything is deleted. Returns `false` if a file could
  /// not be removed.
  pub fn delete(&mut self, keys: &[&str], temp: bool, cache: bool) -> bool {
    let all = keys.is_empty() && !temp && !cache;
    let (temp, cache) = (temp || all, cache || all);
    if cache && let Some(c) = self.cache.as_mut() {
      c.clear();
      self.dirty = false;
    }
    let mut ok = true;
    let (Some(base), Some(data), Some(tmp)) = (self.base_path.clone(), self.data_path(), self.temp_path()) else { return true };
    for path in self.files(false) {
      let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
      let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
      let file_key = name.strip_suffix(EXTENSION).or_else(|| name.strip_suffix(BACKUP_EXTENSION)).filter(|k| is_valid_key(k));
      let wanted = if parent == base {
        cache && file_key == Some(CACHE_KEY)
      } else if parent == data {
        file_key.is_some_and(|k| all || keys.contains(&k))
      } else {
        parent == tmp && temp
      };
      if !wanted {
        tracing::debug!("Persistent File cleanup ignoring file: {}", path.display());
        continue;
      }
      match remove_if_exists(&path) {
        Ok(()) => tracing::info!("Removed persistent file: {}", path.display()),
        Err(e) => {
          ok = false;
          tracing::error!("Failed to remove persistent file: {}", path.display());
          tracing::debug!("Persistent Storage Exception: {}", e);
        }
      }
    }
    self.cache_size = None;
    ok
  }

  /// Every file in the namespace. With `exclude`, backups and scratch files
  /// are left out.
  pub fn files(&self, exclude: bool) -> Vec<PathBuf> {
    let Some(base) = self.path() else { return Vec::new() };
    let mut out = Vec::new();
    walk(base, &mut out);
    if exclude {
      let temp = self.temp_path();
      let cache_backup = backup_path(&self.cache_file().unwrap_or_default());
      let data = self.data_path();
      out.retain(|p| {
        p != &cache_backup
          && temp.as_ref().is_none_or(|t| !p.starts_with(t))
          && !(data.as_ref().is_some_and(|d| p.parent() == Some(d.as_path())) && p.to_string_lossy().ends_with(BACKUP_EXTENSION))
      });
    }
    out.sort();
    out
  }

  /// Bytes used by the namespace (backups and scratch files excluded).
  pub fn size(&mut self) -> u64 {
    if let Some(size) = self.cache_size {
      return size;
    }
    let size = self.files(true).iter().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum();
    self.cache_size = Some(size);
    size
  }

  // ── Whole storage directory ────────────────────────────────────────────

  /// The namespaces found below `path`. With `namespaces`, only those that
  /// start with one of them (`closest`) or equal one of them are returned.
  pub fn disk_scan(path: &Path, namespaces: &[String], closest: bool) -> Vec<String> {
    tracing::trace!("Persistent path scan of: {}", path.display());
    let entries = match fs::read_dir(path) {
      Ok(entries) => entries,
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
        tracing::debug!("Disk Prune path not found; nothing to clean.");
        return Vec::new();
      }
      Err(e) => {
        tracing::error!("Disk Scan detected inaccessible path: {}", path.display());
        tracing::debug!("Persistent Storage Exception: {}", e);
        return Vec::new();
      }
    };
    let mut found: Vec<String> = entries
      .filter_map(Result::ok)
      .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
      .filter_map(|e| e.file_name().into_string().ok())
      .filter(|name| is_valid_key(name))
      .filter(|name| namespaces.is_empty() || namespaces.iter().any(|n| if closest { name.starts_with(n.as_str()) } else { name == n }))
      .collect();
    found.sort();
    found
  }

  /// Find files below `path` not touched for `expires` seconds (31 days
  /// when `None`), limited to `namespaces` (prefixes) when given. With
  /// `action`, they are removed, along with namespace directories left
  /// empty. An `expires` reaching back past the earliest time the system
  /// clock can hold matches no file (upstream raises `OverflowError`).
  pub fn disk_prune(path: &Path, namespaces: Option<&[String]>, expires: Option<u64>, action: bool) -> BTreeMap<String, Vec<PruneRecord>> {
    // `None`: no file can be that old
    let cutoff = SystemTime::now().checked_sub(Duration::from_secs(expires.unwrap_or(DEFAULT_FILE_EXPIRY)));
    let mut map = BTreeMap::new();
    for namespace in Self::disk_scan(path, namespaces.unwrap_or(&[]), true) {
      let base = path.join(&namespace);
      let data = base.join(DATA_DIR);
      let temp = base.join(TEMP_DIR);
      let mut files: Vec<PathBuf> =
        list_files(&data).into_iter().filter(|p| p.to_string_lossy().ends_with(EXTENSION) || p.to_string_lossy().ends_with(BACKUP_EXTENSION)).collect();
      for name in [format!("{CACHE_KEY}{EXTENSION}"), format!("{CACHE_KEY}{BACKUP_EXTENSION}")] {
        let f = base.join(name);
        if f.is_file() {
          files.push(f);
        }
      }
      files.extend(list_files(&temp));
      let mut records = Vec::new();
      let mut sweep = true;
      for file in files {
        let mtime = match fs::metadata(&file).and_then(|m| m.modified()) {
          Ok(t) => t,
          Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
          Err(e) => {
            tracing::error!("Disk Prune (ns={}, clean={}) detected inaccessible file: {}", namespace, if action { "yes" } else { "no" }, file.display());
            tracing::debug!("Persistent Storage Exception: {}", e);
            sweep = false;
            continue;
          }
        };
        if cutoff.is_none_or(|cutoff| mtime > cutoff) {
          continue;
        }
        let mut record = PruneRecord { path: file.clone(), removed: false };
        if action {
          match fs::remove_file(&file) {
            Ok(()) => {
              record.removed = true;
              tracing::info!("Disk Prune (ns={}, clean=yes) removed persistent file: {}", namespace, file.display());
            }
            Err(e) => {
              if e.kind() != std::io::ErrorKind::NotFound {
                tracing::error!("Disk Prune (ns={}, clean=yes) failed to remove persistent file: {}", namespace, file.display());
                tracing::debug!("Persistent Storage Exception: {}", e);
              }
              sweep = false;
            }
          }
        }
        records.push(record);
      }
      if sweep && action {
        for dir in [&temp, &data, &base] {
          if fs::remove_dir(dir).is_ok() {
            tracing::info!("Disk Prune (ns={}, clean=yes) removed persistent dir: {}", namespace, dir.display());
          }
        }
      }
      map.insert(namespace, records);
    }
    map
  }
}

impl Drop for PersistentStore {
  fn drop(&mut self) {
    if self.mode == StorageMode::Auto {
      self.flush();
    }
  }
}

fn create_dir(dir: &Path) -> std::io::Result<()> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().recursive(true).mode(0o770).create(dir)
  }
  #[cfg(not(unix))]
  {
    fs::create_dir_all(dir)
  }
}

fn backup_path(path: &Path) -> PathBuf {
  let s = path.to_string_lossy();
  let stem = s.strip_suffix(EXTENSION).unwrap_or(&s);
  PathBuf::from(format!("{stem}{BACKUP_EXTENSION}"))
}

fn remove_if_exists(path: &Path) -> std::io::Result<()> {
  match fs::remove_file(path) {
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
    other => other,
  }
}

/// Up to `limit` + 1 decompressed bytes of `path`, so a caller can tell
/// when there is more than `limit`.
fn read_gzip(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
  let file = fs::File::open(path)?;
  let mut out = Vec::new();
  flate2::read::GzDecoder::new(file).take(limit + 1).read_to_end(&mut out)?;
  Ok(out)
}

/// Bytes the cache file's JSON takes for these entries; only live,
/// persistent ones are written.
fn cache_size<'a>(entries: impl Iterator<Item = (&'a String, &'a CacheObject)>) -> u64 {
  let mut count = 0u64;
  let mut size = 2u64;
  for (key, co) in entries.filter(|(_, co)| co.persistent && co.is_live()) {
    count += 1;
    size += serde_json::to_vec(key).map_or(0, |k| k.len() as u64) + 1 + co.size;
  }
  size + count.saturating_sub(1)
}

/// Write `data` to a fresh file in `dir` and return its path.
fn write_temp(dir: &Path, data: &[u8], compress: bool) -> std::io::Result<PathBuf> {
  let name = format!("{:016x}{:08x}", rand::random::<u64>(), std::process::id());
  let path = dir.join(name);
  let result = (|| {
    let mut options = fs::File::options();
    options.write(true).create_new(true);
    // Stores hold tokens and keys: owner-only, as upstream's temp files are.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(&path)?;
    if compress {
      let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::best());
      enc.write_all(data)?;
      enc.finish()?.sync_all()
    } else {
      let mut file = file;
      file.write_all(data)?;
      file.sync_all()
    }
  })();
  match result {
    Ok(()) => Ok(path),
    Err(e) => {
      let _ = fs::remove_file(&path);
      Err(e)
    }
  }
}

/// Move `src` over `dst`, keeping the old `dst` as a backup that is put
/// back if the move fails.
fn install(src: &Path, dst: &Path) -> bool {
  let backup = backup_path(dst);
  if let Err(e) = remove_if_exists(&backup) {
    tracing::warn!("Could not remove previous persistent data backup: {}", backup.display());
    tracing::debug!("Persistent Storage Exception: {}", e);
    return false;
  }
  match fs::rename(dst, &backup) {
    Ok(()) => tracing::trace!("Persistent storage backup file created: {}", backup.display()),
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
    Err(e) => {
      tracing::warn!("Could not install persistent content {}", dst.display());
      tracing::debug!("Persistent Storage Exception: {}", e);
      return false;
    }
  }
  if let Err(e) = fs::rename(src, dst) {
    tracing::warn!("Could not install persistent content {}", dst.display());
    tracing::debug!("Persistent Storage Exception: {}", e);
    match fs::rename(&backup, dst) {
      Ok(()) => tracing::trace!("Restoring original persistent content: {}", dst.display()),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
      Err(e) => {
        tracing::warn!("Failed to restore original persistent file: {}", dst.display());
        tracing::debug!("Persistent Storage Exception: {}", e);
      }
    }
    return false;
  }
  tracing::trace!("Persistent file installed: {}", dst.display());
  true
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
  let Ok(entries) = fs::read_dir(dir) else { return };
  for entry in entries.filter_map(Result::ok) {
    match entry.file_type() {
      Ok(t) if t.is_dir() => walk(&entry.path(), out),
      Ok(t) if t.is_file() => out.push(entry.path()),
      _ => {}
    }
  }
}

fn list_files(dir: &Path) -> Vec<PathBuf> {
  fs::read_dir(dir)
    .map(|entries| entries.filter_map(Result::ok).filter(|e| e.file_type().is_ok_and(|t| t.is_file())).map(|e| e.path()).collect())
    .unwrap_or_default()
}

// ── Per-service stores ──────────────────────────────────────────────────────

/// The stores of one `Apprise` instance's services, by namespace, so a
/// service sees the same store on every notification.
#[derive(Clone, Default)]
pub struct StoreRegistry(Arc<Mutex<HashMap<StoreKey, Arc<Mutex<PersistentStore>>>>>);

/// Namespace, storage path and mode of a registered store.
type StoreKey = (String, Option<PathBuf>, String);

impl std::fmt::Debug for StoreRegistry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("StoreRegistry")
  }
}

impl StoreRegistry {
  /// The store for `namespace`, created on first use.
  pub fn store(&self, namespace: &str, path: Option<&Path>, mode: StorageMode) -> Option<Arc<Mutex<PersistentStore>>> {
    let key = (namespace.to_string(), path.map(Path::to_path_buf), mode.to_string());
    let mut stores = self.0.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(store) = stores.get(&key) {
      return Some(store.clone());
    }
    let store = Arc::new(Mutex::new(PersistentStore::new(path, namespace, mode).ok()?));
    stores.insert(key, store.clone());
    Some(store)
  }

  /// Write every store's pending changes to disk.
  pub fn flush(&self) {
    let stores: Vec<_> = self.0.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
    for store in stores {
      store.lock().unwrap_or_else(|e| e.into_inner()).flush();
    }
  }
}

/// Where the service being sent keeps its data.
#[derive(Debug, Clone)]
pub(crate) struct StoreSlot {
  pub registry: StoreRegistry,
  pub namespace: Option<String>,
  pub path: Option<PathBuf>,
  pub mode: StorageMode,
}

tokio::task_local! {
  static SERVICE_STORE: StoreSlot;
}

/// Run `fut` with [`with_service_store`] reaching `slot`.
pub(crate) async fn with_store_slot<F: std::future::Future>(slot: StoreSlot, fut: F) -> F::Output {
  SERVICE_STORE.scope(slot, fut).await
}

/// Run `f` on the persistent store of the service currently being sent.
///
/// The store is created on first use, in the namespace named after the
/// service's URL ID, and is kept for the life of the `Apprise` instance.
/// With `?store=no` (or no storage path) it lives in memory only. Returns
/// `None` outside a dispatch, so plugins must not depend on it.
pub fn with_service_store<R>(f: impl FnOnce(&mut PersistentStore) -> R) -> Option<R> {
  let store =
    SERVICE_STORE.try_with(|slot| slot.namespace.as_deref().and_then(|ns| slot.registry.store(ns, slot.path.as_deref(), slot.mode.clone()))).ok().flatten()?;
  let mut guard = store.lock().unwrap_or_else(|e| e.into_inner());
  Some(f(&mut guard))
}

#[cfg(test)]
mod tests;
