use super::*;
use serde_json::json;

fn age(path: &Path, secs: u64) {
  let when = SystemTime::now() - Duration::from_secs(secs);
  fs::File::options().append(true).open(path).unwrap().set_modified(when).unwrap();
}

fn age_tree(dir: &Path, secs: u64) {
  let mut files = Vec::new();
  walk(dir, &mut files);
  for f in files {
    age(&f, secs);
  }
}

#[test]
fn test_invalid_namespaces_and_keys_are_rejected() {
  let tmp = tempfile::tempdir().unwrap();
  for bad in ["default/../../escaped_ns", "pwned/../../../outside", "abc/def", "abc\\def", "abc%", "", "-abc", ".abc"] {
    assert!(PersistentStore::new(Some(tmp.path()), bad, StorageMode::Auto).is_err(), "{bad}");
  }
  let mut store = PersistentStore::new(Some(tmp.path()), "default", StorageMode::Flush).unwrap();
  for bad in ["default/../../escaped_ns", "abc/def", "abc%"] {
    assert_eq!(store.write(b"data", Some(bad), true), Err(StorageError::InvalidKey(bad.to_string())));
    assert_eq!(store.read(Some(bad), true, true), None);
  }
  assert!(!tmp.path().join("escaped_ns").exists());
  assert!(is_valid_key("a.b_c-D9"));
}

#[test]
fn test_memory_mode() {
  let mut store = PersistentStore::new(None, "abc", StorageMode::Auto).unwrap();
  assert_eq!(store.mode(), StorageMode::Memory);
  assert!(store.path().is_none());
  assert!(store.set("key", json!("value"), None, true));
  assert_eq!(store.get("key"), Some(json!("value")));
  assert_eq!(store.write(b"data", None, true), Ok(false));
  assert_eq!(store.read(None, true, true), None);
  assert_eq!(store.size(), 0);
  assert!(store.flush());
}

#[test]
fn test_cache_round_trip_and_expiry() {
  let tmp = tempfile::tempdir().unwrap();
  {
    let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
    assert!(store.path().unwrap().join("var").is_dir());
    assert!(store.path().unwrap().join("tmp").is_dir());
    assert!(store.set("str", json!("value"), None, true));
    assert!(store.set("int", json!(42), None, true));
    assert!(store.set("dict", json!({"a": [1, 2.5, null, true]}), None, true));
    assert!(store.set("gone", json!("x"), Some(Duration::ZERO), true));
    assert!(!store.contains("gone"));
    assert_eq!(store.get("gone"), None);
    // Dropping an AUTO store flushes it.
  }
  assert!(tmp.path().join("abc").join("cache.psdata").is_file());
  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
  assert_eq!(store.get("str"), Some(json!("value")));
  assert_eq!(store.get("int"), Some(json!(42)));
  assert_eq!(store.get("dict"), Some(json!({"a": [1, 2.5, null, true]})));
  assert_eq!(store.keys(), vec!["dict", "int", "str"]);
  assert!(store.remove("int"));
  assert!(!store.remove("int"));
  assert!(store.clear(&["str"]));
  assert_eq!(store.keys(), vec!["dict"]);
  assert!(store.clear(&[]));
  assert!(store.keys().is_empty());
  assert!(store.flush());
  // An empty cache moves the cache file aside.
  assert!(!tmp.path().join("abc").join("cache.psdata").exists());
  assert!(tmp.path().join("abc").join("cache._psbak").exists());
}

#[test]
fn test_mixed_persistent_set() {
  let tmp = tempfile::tempdir().unwrap();
  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
  assert!(store.set("keep", json!("persistent value"), None, true));
  assert!(store.set("scratch", json!("temp value"), None, false));
  assert!(store.flush());
  drop(store);

  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
  assert_eq!(store.get("keep"), Some(json!("persistent value")));
  assert_eq!(store.get("scratch"), None);
  // Turning a saved entry into a non-persistent one removes it from disk.
  assert!(store.set("keep", json!("temp value"), None, false));
  assert!(store.flush());
  drop(store);

  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
  assert_eq!(store.get("keep"), None);
}

#[test]
fn test_flush_mode_writes_immediately() {
  let tmp = tempfile::tempdir().unwrap();
  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Flush).unwrap();
  assert!(store.set("k", json!("v"), None, true));
  // Already on disk while the first store is still open.
  let mut fresh = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Flush).unwrap();
  assert_eq!(fresh.get("k"), Some(json!("v")));
  assert_eq!(store.get("k"), Some(json!("v")));
}

#[test]
fn test_tampered_entries_are_dropped() {
  let tmp = tempfile::tempdir().unwrap();
  {
    let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Flush).unwrap();
    store.set("a", json!("one"), None, true);
    store.set("b", json!("two"), None, true);
  }
  let cache_file = tmp.path().join("abc").join("cache.psdata");
  let mut content: Value = serde_json::from_slice(&read_gzip(&cache_file, MAX_CACHE_SIZE).unwrap()).unwrap();
  content["a"]["v"] = json!("changed");
  let tmpfile = write_temp(&tmp.path().join("abc").join("tmp"), &serde_json::to_vec(&content).unwrap(), true).unwrap();
  fs::rename(tmpfile, &cache_file).unwrap();
  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
  assert_eq!(store.get("a"), None);
  assert_eq!(store.get("b"), Some(json!("two")));

  // A corrupted file is discarded.
  fs::write(&cache_file, b"not gzip").unwrap();
  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
  assert!(store.keys().is_empty());
  assert!(store.set("c", json!(1), None, true));
}

// Values and hashes produced by upstream for the same entries.
#[test]
fn test_cache_format_matches_upstream() {
  let stored = |v: Value| Stored::from(&v);
  let co = CacheObject::build(stored(json!("value")), "str".into(), None, true);
  assert_eq!(co.describe(), "str:+:value expires: never");
  let co = CacheObject::build(stored(json!({"a": [1, 2.5, null, true], "b": "it's"})), "dict".into(), None, true);
  assert_eq!(co.describe(), "dict:+:{'a': [1, 2.5, None, True], 'b': \"it's\"} expires: never");
  let co = CacheObject::build(stored(json!(3.0)), "float".into(), Some(Utc.timestamp_micros(1_800_000_000_123_456).unwrap()), true);
  assert_eq!(co.describe(), "float:+:3.0 expires: 2027-01-15T08:00:00.123456");
  assert_eq!(serde_json::to_value(co.to_entry()).unwrap()["x"], json!(1_800_000_000.123456));
  assert_eq!(co.hash(), "2a487e");
  let co = CacheObject::build(stored(json!("value")), "str".into(), None, true);
  assert_eq!(co.hash(), "79fe7c");
  assert_eq!(serde_json::to_string(&co.to_entry()).unwrap(), r#"{"v":"value","x":null,"c":"str","!":"79fe7c"}"#);
  let back = CacheObject::from_json(&serde_json::from_str(&serde_json::to_string(&co.to_entry()).unwrap()).unwrap()).unwrap();
  assert_eq!(back, co);
  // A repeated member keeps its first place and its last value, as a
  // Python dict does
  let parsed: Stored = serde_json::from_str(r#"{"b":1,"a":2,"b":3}"#).unwrap();
  assert_eq!(py_repr(&parsed), "{'b': 3, 'a': 2}");
}

// Upstream hashes a dictionary in insertion order. This is the cache file
// upstream wrote for `set('obj', {'z': 1, 'a': 2})`, a nested dictionary
// and a string.
const UPSTREAM_DICT_CACHE: &str = concat!(
  r#"{"obj":{"v":{"z":1,"a":2},"x":null,"c":"dict","!":"ba263c"},"#,
  r#""nested":{"v":{"outer":{"y":[1,{"b":true,"a":null}],"x":1.5},"k":"v"},"x":null,"c":"dict","!":"ee1674"},"#,
  r#""token":{"v":"secret","x":null,"c":"str","!":"8c9b0b"}}"#
);

#[test]
fn test_upstream_dictionaries_keep_their_key_order() {
  let tmp = tempfile::tempdir().unwrap();
  let ns = tmp.path().join("test");
  for dir in [ns.clone(), ns.join("tmp"), ns.join("var")] {
    fs::create_dir_all(dir).unwrap();
  }
  let staged = write_temp(&ns.join("tmp"), UPSTREAM_DICT_CACHE.as_bytes(), true).unwrap();
  fs::rename(staged, ns.join("cache.psdata")).unwrap();

  let mut store = PersistentStore::new(Some(tmp.path()), "test", StorageMode::Auto).unwrap();
  assert_eq!(store.get("obj"), Some(json!({"z": 1, "a": 2})));
  assert_eq!(store.get("nested"), Some(json!({"outer": {"y": [1, {"b": true, "a": null}], "x": 1.5}, "k": "v"})));
  assert_eq!(store.get("token"), Some(json!("secret")));

  // Rewriting the cache keeps the order, so the hash still verifies (here
  // and for upstream).
  assert!(store.set("more", json!({"b": 1, "a": 2}), None, true));
  assert!(store.flush());
  let written = String::from_utf8(read_gzip(&ns.join("cache.psdata"), MAX_CACHE_SIZE).unwrap()).unwrap();
  assert!(written.contains(r#""v":{"z":1,"a":2}"#), "{written}");
  assert!(written.contains(r#""v":{"outer":{"y":[1,{"b":true,"a":null}],"x":1.5},"k":"v"}"#), "{written}");
  let mut store = PersistentStore::new(Some(tmp.path()), "test", StorageMode::Auto).unwrap();
  assert_eq!(store.keys(), vec!["more", "nested", "obj", "token"]);
  assert_eq!(store.get("obj"), Some(json!({"z": 1, "a": 2})));
}

#[test]
fn test_cache_never_grows_past_what_the_reader_takes() {
  let tmp = tempfile::tempdir().unwrap();
  // One value too large for the cache: refused, and the token survives
  {
    let mut store = PersistentStore::new(Some(tmp.path()), "large", StorageMode::Auto).unwrap();
    assert!(store.set("token", json!("secret"), None, true));
    assert!(!store.set("large", json!("x".repeat(MAX_CACHE_SIZE as usize + 10)), None, true));
    assert_eq!(store.get("large"), None);
    assert!(store.flush());
  }
  let mut store = PersistentStore::new(Some(tmp.path()), "large", StorageMode::Auto).unwrap();
  assert_eq!(store.get("token"), Some(json!("secret")));
  assert_eq!(store.keys(), vec!["token"]);

  // Many small values: the one that would cross the limit is refused, and
  // an earlier value it would have replaced is kept
  {
    let mut store = PersistentStore::new(Some(tmp.path()), "many", StorageMode::Auto).unwrap();
    assert!(store.set("token", json!("secret"), None, true));
    let mut accepted = 0;
    while store.set(&format!("k{accepted:05}"), json!("v".repeat(1000)), None, true) {
      accepted += 1;
      assert!(accepted < 2000, "the limit was never reached");
    }
    assert!(accepted > 900, "{accepted}");
    assert!(!store.set("token", json!("t".repeat(2000)), None, true));
    assert_eq!(store.get("token"), Some(json!("secret")));
    // A value that is not saved does not count
    assert!(store.set("scratch", json!("s".repeat(4000)), None, false));
    assert!(store.flush());
    let written = read_gzip(&store.cache_file().unwrap(), MAX_CACHE_SIZE).unwrap();
    assert!(written.len() as u64 <= MAX_CACHE_SIZE);
  }
  let mut store = PersistentStore::new(Some(tmp.path()), "many", StorageMode::Auto).unwrap();
  assert_eq!(store.get("token"), Some(json!("secret")));
  assert_eq!(store.get("k00000"), Some(json!("v".repeat(1000))));
}

#[test]
fn test_oversized_cache_file_is_left_alone() {
  // Upstream sets no limit, so its cache can be larger than this reader
  // takes; that file is not ours to delete
  let tmp = tempfile::tempdir().unwrap();
  let ns = tmp.path().join("big");
  for dir in [ns.clone(), ns.join("tmp"), ns.join("var")] {
    fs::create_dir_all(dir).unwrap();
  }
  let big = Stored::String("x".repeat(MAX_CACHE_SIZE as usize));
  let co = CacheObject::build(big, "str".into(), None, true);
  let content = serde_json::to_vec(&BTreeMap::from([("large", co.to_entry())])).unwrap();
  let staged = write_temp(&ns.join("tmp"), &content, true).unwrap();
  fs::rename(staged, ns.join("cache.psdata")).unwrap();
  let before = fs::read(ns.join("cache.psdata")).unwrap();

  let mut store = PersistentStore::new(Some(tmp.path()), "big", StorageMode::Auto).unwrap();
  assert_eq!(store.get("large"), None);
  assert!(!store.set("token", json!("secret"), None, true));
  assert!(store.flush());
  drop(store);
  assert_eq!(fs::read(ns.join("cache.psdata")).unwrap(), before);
}

#[test]
fn test_write_read_and_size_limit() {
  let tmp = tempfile::tempdir().unwrap();
  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
  assert_eq!(store.write(b"hello", None, true), Ok(true));
  assert_eq!(store.read(None, true, true), Some(b"hello".to_vec()));
  assert_eq!(store.write(b"raw", Some("named"), false), Ok(true));
  assert_eq!(fs::read(store.path().unwrap().join("var").join("named.psdata")).unwrap(), b"raw");
  assert_eq!(store.read(Some("named"), false, true), Some(b"raw".to_vec()));
  assert_eq!(store.read(Some("missing"), true, true), None);
  // Overwriting keeps a backup that size() ignores.
  assert_eq!(store.write(b"raw2", Some("named"), false), Ok(true));
  assert!(store.path().unwrap().join("var").join("named._psbak").exists());
  let size = store.size();
  assert!(size > 0);
  assert!(store.files(false).len() > store.files(true).len());
  // Incompressible data larger than the limit is refused.
  let big: Vec<u8> = (0..MAX_FILE_SIZE + 10).map(|_| rand::random::<u8>()).collect();
  assert_eq!(store.write(&big, Some("big"), false), Ok(false));
  assert!(!store.path().unwrap().join("var").join("big.psdata").exists());
}

#[test]
fn test_delete() {
  let tmp = tempfile::tempdir().unwrap();
  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Flush).unwrap();
  store.write(b"a", Some("a"), true).unwrap();
  store.write(b"b", Some("b"), true).unwrap();
  store.set("k", json!("v"), None, true);
  let var = store.path().unwrap().join("var");
  assert!(store.delete(&["a"], false, false));
  assert!(!var.join("a.psdata").exists());
  assert!(var.join("b.psdata").exists());
  assert!(store.delete(&[], false, true));
  assert!(!tmp.path().join("abc").join("cache.psdata").exists());
  assert!(var.join("b.psdata").exists());
  assert!(store.delete(&[], false, false));
  assert!(!var.join("b.psdata").exists());
}

#[test]
fn test_read_renews_served_file() {
  for key in [None, Some("default"), Some("named")] {
    for renew in [true, false] {
      let tmp = tempfile::tempdir().unwrap();
      let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
      store.write(b"stored data", key, true).unwrap();
      let data_file = store.path().unwrap().join("var").join(format!("{}.psdata", key.unwrap_or(BASE_KEY)));
      age(&data_file, DEFAULT_FILE_EXPIRY + 3600);
      assert_eq!(store.read(key, true, renew), Some(b"stored data".to_vec()));
      assert!(store.flush());
      let results = PersistentStore::disk_prune(tmp.path(), None, None, true);
      assert_eq!(data_file.exists(), renew, "{key:?} {renew}");
      assert_eq!(results["abc"].iter().any(|r| r.path == data_file && r.removed), !renew);
    }
  }
}

#[test]
fn test_get_renews_cache_file() {
  let tmp = tempfile::tempdir().unwrap();
  {
    let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Flush).unwrap();
    store.set("k", json!("v"), None, true);
  }
  let cache_file = tmp.path().join("abc").join("cache.psdata");
  age(&cache_file, DEFAULT_FILE_EXPIRY + 3600);
  let mut store = PersistentStore::new(Some(tmp.path()), "abc", StorageMode::Auto).unwrap();
  assert_eq!(store.get("k"), Some(json!("v")));
  store.flush();
  PersistentStore::disk_prune(tmp.path(), None, None, true);
  assert!(cache_file.exists());
}

#[test]
fn test_disk_prune_uses_default_expiry() {
  let tmp = tempfile::tempdir().unwrap();
  for ns in ["old", "new"] {
    let mut store = PersistentStore::new(Some(tmp.path()), ns, StorageMode::Flush).unwrap();
    assert_eq!(store.write(b"data", None, true), Ok(true));
  }
  age_tree(&tmp.path().join("old"), DEFAULT_FILE_EXPIRY + 3600);
  let results = PersistentStore::disk_prune(tmp.path(), None, None, false);
  assert!(!results["old"].is_empty());
  assert!(results["new"].is_empty());
  assert!(results["old"].iter().all(|r| !r.removed));

  // Zero days removes everything, and empty namespace directories go too.
  let results = PersistentStore::disk_prune(tmp.path(), Some(&["ol".to_string()]), Some(0), true);
  assert_eq!(results.keys().collect::<Vec<_>>(), vec!["old"]);
  assert!(results["old"].iter().all(|r| r.removed));
  assert!(!tmp.path().join("old").exists());
  assert!(tmp.path().join("new").exists());
}

/// An expiry reaching back past the earliest time the clock can hold
/// matches no file. Upstream raises `OverflowError` from about 739,000
/// days; the port panicked.
#[test]
fn test_disk_prune_past_the_clock_range_matches_nothing() {
  let tmp = tempfile::tempdir().unwrap();
  let mut store = PersistentStore::new(Some(tmp.path()), "old", StorageMode::Flush).unwrap();
  assert_eq!(store.write(b"data", None, true), Ok(true));
  let data_file = store.path().unwrap().join("var").join("default.psdata");
  fs::File::options().append(true).open(&data_file).unwrap().set_modified(std::time::UNIX_EPOCH).unwrap();
  // 150,000,000,000,000 days, then the largest expiry there is
  for expires in [150_000_000_000_000 * 86_400, u64::MAX] {
    for action in [false, true] {
      let results = PersistentStore::disk_prune(tmp.path(), None, Some(expires), action);
      assert_eq!(results.keys().collect::<Vec<_>>(), vec!["old"]);
      assert!(results["old"].is_empty(), "{expires} {action}");
      assert!(data_file.exists());
    }
  }
  // A long but representable expiry still finds it
  let results = PersistentStore::disk_prune(tmp.path(), None, Some(50 * 365 * 86_400), true);
  assert!(results["old"].iter().any(|r| r.removed && r.path == data_file));
  assert!(!data_file.exists());
}

#[test]
fn test_disk_scan() {
  let tmp = tempfile::tempdir().unwrap();
  for ns in ["abc123", "abd456", "xyz"] {
    PersistentStore::new(Some(tmp.path()), ns, StorageMode::Auto).unwrap();
  }
  fs::create_dir(tmp.path().join("not valid")).unwrap();
  fs::write(tmp.path().join("afile"), b"").unwrap();
  assert_eq!(PersistentStore::disk_scan(tmp.path(), &[], true), vec!["abc123", "abd456", "xyz"]);
  assert_eq!(PersistentStore::disk_scan(tmp.path(), &["ab".into()], true), vec!["abc123", "abd456"]);
  assert_eq!(PersistentStore::disk_scan(tmp.path(), &["ab".into()], false), Vec::<String>::new());
  assert_eq!(PersistentStore::disk_scan(tmp.path(), &["xyz".into()], false), vec!["xyz"]);
  assert!(PersistentStore::disk_scan(&tmp.path().join("missing"), &[], true).is_empty());
}

#[test]
fn test_unwritable_path_falls_back_to_memory() {
  let tmp = tempfile::tempdir().unwrap();
  let file = tmp.path().join("file");
  fs::write(&file, b"").unwrap();
  let mut store = PersistentStore::new(Some(&file), "abc", StorageMode::Auto).unwrap();
  assert_eq!(store.mode(), StorageMode::Memory);
  assert!(store.set("k", json!(1), None, true));
  assert_eq!(store.get("k"), Some(json!(1)));
}

#[tokio::test]
async fn test_service_store_slot() {
  assert_eq!(with_service_store(|_| ()), None);
  let tmp = tempfile::tempdir().unwrap();
  let registry = StoreRegistry::default();
  let slot = StoreSlot { registry: registry.clone(), namespace: Some("abcd1234".into()), path: Some(tmp.path().to_path_buf()), mode: StorageMode::Auto };
  with_store_slot(slot.clone(), async {
    assert_eq!(with_service_store(|s| s.set("token", json!("t"), None, true)), Some(true));
  })
  .await;
  // The same store is handed out again.
  with_store_slot(slot, async {
    assert_eq!(with_service_store(|s| s.get("token")), Some(Some(json!("t"))));
  })
  .await;
  registry.flush();
  assert!(tmp.path().join("abcd1234").join("cache.psdata").is_file());
}

#[cfg(unix)]
#[test]
fn test_store_files_are_owner_only() {
  use std::os::unix::fs::PermissionsExt;
  let tmp = tempfile::tempdir().unwrap();
  let mut store = PersistentStore::new(Some(tmp.path()), "secrets", StorageMode::Flush).unwrap();
  assert!(store.set("token", json!("s3cret"), None, true));
  assert!(store.write(b"key material", Some("key1"), false).unwrap());
  let mut files = Vec::new();
  let mut dirs = vec![tmp.path().to_path_buf()];
  while let Some(dir) = dirs.pop() {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
      if entry.file_type().unwrap().is_dir() { dirs.push(entry.path()) } else { files.push(entry.path()) }
    }
  }
  assert!(files.len() >= 2, "{:?}", files);
  for file in files {
    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{}", file.display());
  }
}

#[test]
fn test_huge_expiry_never_expires() {
  // e.g. a token response with "expires_in": 99999999999999
  let tmp = tempfile::tempdir().unwrap();
  let mut store = PersistentStore::new(Some(tmp.path()), "huge", StorageMode::Memory).unwrap();
  assert!(store.set("token", json!("t"), Some(Duration::from_secs(99_999_999_999_999)), false));
  assert_eq!(store.get("token"), Some(json!("t")));
  assert!(store.set("forever", json!("f"), Some(Duration::MAX), false));
  assert_eq!(store.get("forever"), Some(json!("f")));
}

#[test]
fn test_float_values_survive_a_reopen() {
  // Each entry's integrity hash covers Python's repr of its value, so a
  // float must read back bit for bit. serde_json's default parser can be
  // one unit in the last place off, which dropped about 1 entry in 8 as
  // tampered (a Matrix Megolm record's created_at, for one).
  let tmp = tempfile::tempdir().unwrap();
  let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
  let mut floats = Vec::new();
  for _ in 0..300 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    floats.push(1_793_420_000.0 + (x % 100_000_000) as f64 / 1e7 + (x % 977) as f64 * 1e-9);
  }
  {
    let mut store = PersistentStore::new(Some(tmp.path()), "floats", StorageMode::Auto).unwrap();
    for (i, f) in floats.iter().enumerate() {
      assert!(store.set(&format!("k{i}"), json!({"created_at": f}), Some(Duration::from_secs(3600)), true));
    }
  }
  let mut store = PersistentStore::new(Some(tmp.path()), "floats", StorageMode::Auto).unwrap();
  let lost: Vec<usize> = (0..floats.len()).filter(|i| store.get(&format!("k{i}")) != Some(json!({"created_at": floats[*i]}))).collect();
  assert!(lost.is_empty(), "{} of {} entries did not survive: {:?}", lost.len(), floats.len(), &lost[..lost.len().min(10)]);
}
