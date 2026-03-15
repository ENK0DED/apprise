use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use tokio::fs;
use crate::types::StorageMode;

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
        if matches!(self.mode, StorageMode::Memory) { return Vec::new(); }
        let mut entries = Vec::new();
        let Ok(mut dir) = fs::read_dir(&self.path).await else { return entries; };
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
        if matches!(self.mode, StorageMode::Memory) { return 0; }
        let cutoff = chrono::Utc::now() - chrono::Duration::days(self.prune_days as i64);
        let entries = self.list().await;
        let mut pruned = 0;
        for entry in entries {
            if entry.last_used < cutoff {
                let p = self.path.join(format!("{}.json", entry.uid));
                if fs::remove_file(&p).await.is_ok() { pruned += 1; }
            }
        }
        pruned
    }

    /// Remove all entries
    pub async fn clean(&self) -> usize {
        if matches!(self.mode, StorageMode::Memory) { return 0; }
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
        if matches!(self.mode, StorageMode::Memory) { return Ok(()); }
        fs::create_dir_all(&self.path).await?;
        let entry = StorageEntry {
            uid: uid.to_string(),
            url_hash: url_hash.to_string(),
            last_used: chrono::Utc::now(),
        };
        let content = serde_json::to_string(&entry).unwrap();
        fs::write(self.path.join(format!("{}.json", uid)), content).await
    }

    /// Generate a random UID
    pub fn generate_uid(&self) -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..self.uid_length).map(|_| rng.sample(rand::distributions::Alphanumeric) as char).collect()
    }
}
