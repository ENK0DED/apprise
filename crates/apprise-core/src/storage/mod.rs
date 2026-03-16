use crate::types::StorageMode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::fs;

#[derive(Debug, Serialize, Deserialize)]
pub struct StorageEntry {
  pub uid: String,
  pub url_hash: String,
  pub last_used: chrono::DateTime<chrono::Utc>,
}

pub struct PersistentStore {
  pub path: PathBuf,
  pub uid_length: usize,
  pub prune_days: u32,
  pub mode: StorageMode,
}

impl PersistentStore {
  pub fn new(path: PathBuf, uid_length: usize, prune_days: u32, mode: StorageMode) -> Self {
    Self { path, uid_length, prune_days, mode }
  }

  /// List all stored entries
  pub async fn list(&self) -> Vec<StorageEntry> {
    if matches!(self.mode, StorageMode::Memory) {
      return Vec::new();
    }
    let mut entries = Vec::new();
    let Ok(mut dir) = fs::read_dir(&self.path).await else {
      return entries;
    };
    while let Ok(Some(entry)) = dir.next_entry().await {
      let path = entry.path();
      if path.extension().and_then(|e| e.to_str()) == Some("json") {
        if let Ok(content) = fs::read_to_string(&path).await {
          if let Ok(e) = serde_json::from_str::<StorageEntry>(&content) {
            entries.push(e);
          }
        }
      }
    }
    entries
  }

  /// Prune entries older than prune_days
  pub async fn prune(&self) -> usize {
    if matches!(self.mode, StorageMode::Memory) {
      return 0;
    }
    let cutoff = chrono::Utc::now() - chrono::Duration::days(self.prune_days as i64);
    let entries = self.list().await;
    let mut pruned = 0;
    for entry in entries {
      if entry.last_used < cutoff {
        let p = self.path.join(format!("{}.json", entry.uid));
        if fs::remove_file(&p).await.is_ok() {
          pruned += 1;
        }
      }
    }
    pruned
  }

  /// Remove all entries
  pub async fn clean(&self) -> usize {
    if matches!(self.mode, StorageMode::Memory) {
      return 0;
    }
    let entries = self.list().await;
    let count = entries.len();
    for entry in entries {
      let p = self.path.join(format!("{}.json", entry.uid));
      let _ = fs::remove_file(&p).await;
    }
    count
  }

  /// Store a new UID entry
  pub async fn store(&self, uid: &str, url_hash: &str) -> std::io::Result<()> {
    if matches!(self.mode, StorageMode::Memory) {
      return Ok(());
    }
    fs::create_dir_all(&self.path).await?;
    let entry = StorageEntry { uid: uid.to_string(), url_hash: url_hash.to_string(), last_used: chrono::Utc::now() };
    let content = serde_json::to_string(&entry).unwrap();
    fs::write(self.path.join(format!("{}.json", uid)), content).await
  }

  /// Generate a random UID
  pub fn generate_uid(&self) -> String {
    (0..self.uid_length)
      .map(|_| {
        let idx = rand::random::<u8>() % 62;
        let c = match idx {
          0..=9 => b'0' + idx,
          10..=35 => b'a' + idx - 10,
          _ => b'A' + idx - 36,
        };
        c as char
      })
      .collect()
  }

  /// Get a per-plugin cache value by plugin UID and key
  pub async fn get(&self, plugin_uid: &str, key: &str) -> Option<serde_json::Value> {
    if matches!(self.mode, StorageMode::Memory) {
      return None;
    }
    let cache_path = self.path.join(format!("{}.cache.json", plugin_uid));
    let content = fs::read_to_string(&cache_path).await.ok()?;
    let map: std::collections::HashMap<String, CacheEntry> = serde_json::from_str(&content).ok()?;
    let entry = map.get(key)?;
    // Check expiry
    if let Some(expires_epoch) = entry.expires {
      let now = chrono::Utc::now().timestamp() as f64;
      if now > expires_epoch {
        return None; // expired
      }
    }
    Some(entry.value.clone())
  }

  /// Set a per-plugin cache value with optional TTL in seconds
  pub async fn set(&self, plugin_uid: &str, key: &str, value: serde_json::Value, ttl_secs: Option<f64>) -> std::io::Result<()> {
    if matches!(self.mode, StorageMode::Memory) {
      return Ok(());
    }
    fs::create_dir_all(&self.path).await?;
    let cache_path = self.path.join(format!("{}.cache.json", plugin_uid));

    // Load existing cache
    let mut map: std::collections::HashMap<String, CacheEntry> = if let Ok(content) = fs::read_to_string(&cache_path).await {
      serde_json::from_str(&content).unwrap_or_default()
    } else {
      std::collections::HashMap::new()
    };

    let expires = ttl_secs.map(|ttl| chrono::Utc::now().timestamp() as f64 + ttl);

    map.insert(key.to_string(), CacheEntry { value, expires });

    let content = serde_json::to_string(&map).map_err(std::io::Error::other)?;
    fs::write(&cache_path, content).await
  }

  /// Delete a per-plugin cache key
  pub async fn delete(&self, plugin_uid: &str, key: &str) -> std::io::Result<()> {
    if matches!(self.mode, StorageMode::Memory) {
      return Ok(());
    }
    let cache_path = self.path.join(format!("{}.cache.json", plugin_uid));
    if let Ok(content) = fs::read_to_string(&cache_path).await {
      if let Ok(mut map) = serde_json::from_str::<std::collections::HashMap<String, CacheEntry>>(&content) {
        map.remove(key);
        let content = serde_json::to_string(&map).map_err(std::io::Error::other)?;
        fs::write(&cache_path, content).await?;
      }
    }
    Ok(())
  }

  /// Prune expired cache entries for a plugin
  pub async fn prune_cache(&self, plugin_uid: &str) -> usize {
    if matches!(self.mode, StorageMode::Memory) {
      return 0;
    }
    let cache_path = self.path.join(format!("{}.cache.json", plugin_uid));
    let Ok(content) = fs::read_to_string(&cache_path).await else { return 0 };
    let Ok(mut map) = serde_json::from_str::<std::collections::HashMap<String, CacheEntry>>(&content) else { return 0 };
    let now = chrono::Utc::now().timestamp() as f64;
    let before = map.len();
    map.retain(|_, entry| entry.expires.is_none_or(|exp| now <= exp));
    let pruned = before - map.len();
    if pruned > 0 {
      if let Ok(content) = serde_json::to_string(&map) {
        let _ = fs::write(&cache_path, content).await;
      }
    }
    pruned
  }
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
  value: serde_json::Value,
  #[serde(skip_serializing_if = "Option::is_none")]
  expires: Option<f64>,
}
