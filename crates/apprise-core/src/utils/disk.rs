//! Disk/filesystem utilities matching Python's apprise disk module.
//!
//! - `path_decode()` — normalize paths with home directory expansion
//! - `tidy_path()` — clean up path formatting (remove duplicate slashes)
//! - `bytes_to_str()` — convert byte counts to human-readable strings
//! - `dir_size()` — calculate directory size recursively

use std::path::{Path, PathBuf};

/// Decode and normalize a filesystem path.
///
/// Expands `~` to home directory and resolves to absolute path,
/// matching Python's `os.path.abspath(expanduser(path))`.
pub fn path_decode(path: &str) -> PathBuf {
  let expanded = if path.starts_with('~') {
    if let Some(home) = dirs::home_dir() {
      let rest = path.strip_prefix("~/").or_else(|| path.strip_prefix("~")).unwrap_or("");
      home.join(rest)
    } else {
      PathBuf::from(path)
    }
  } else {
    PathBuf::from(path)
  };

  // Canonicalize if possible, otherwise just use the expanded path
  std::fs::canonicalize(&expanded).unwrap_or(expanded)
}

/// Clean up a path by removing duplicate slashes and trailing slashes.
///
/// Example: `////absolute//path//` becomes `/absolute/path`
pub fn tidy_path(path: &str) -> String {
  let trimmed = path.trim();
  if trimmed.is_empty() {
    return String::new();
  }

  // Normalize separators
  let normalized = trimmed.replace('\\', "/");

  // Remove duplicate slashes while preserving leading slash
  let mut result = String::with_capacity(normalized.len());
  let mut last_was_slash = false;

  for ch in normalized.chars() {
    if ch == '/' {
      if !last_was_slash || result.is_empty() {
        result.push(ch);
      }
      last_was_slash = true;
    } else {
      result.push(ch);
      last_was_slash = false;
    }
  }

  // Remove trailing slash (unless it's the root "/")
  if result.len() > 1 && result.ends_with('/') {
    result.pop();
  }

  // Expand home directory
  if result.starts_with('~') {
    if let Some(home) = dirs::home_dir() {
      let rest = result.strip_prefix("~/").or_else(|| result.strip_prefix("~")).unwrap_or("");
      return home.join(rest).to_string_lossy().to_string();
    }
  }

  result
}

/// Convert a byte count to a human-readable string.
///
/// Examples: `1024` → `"1.00 KB"`, `1048576` → `"1.00 MB"`
pub fn bytes_to_str(value: u64) -> String {
  let mut val = value as f64;
  let mut unit = "B";

  if val >= 1024.0 {
    val /= 1024.0;
    unit = "KB";
    if val >= 1024.0 {
      val /= 1024.0;
      unit = "MB";
      if val >= 1024.0 {
        val /= 1024.0;
        unit = "GB";
        if val >= 1024.0 {
          val /= 1024.0;
          unit = "TB";
        }
      }
    }
  }

  if unit == "B" { format!("{:.0} {}", val, unit) } else { format!("{:.2} {}", val, unit) }
}

/// Calculate the total size of a directory recursively.
///
/// Returns the total size in bytes. Stops at `max_depth` levels.
pub fn dir_size(path: &Path, max_depth: u32) -> std::io::Result<u64> {
  dir_size_inner(path, max_depth, 0)
}

fn dir_size_inner(path: &Path, max_depth: u32, depth: u32) -> std::io::Result<u64> {
  if depth > max_depth {
    return Ok(0);
  }

  let mut total: u64 = 0;

  if path.is_file() {
    return Ok(path.metadata()?.len());
  }

  if path.is_dir() {
    for entry in std::fs::read_dir(path)? {
      let entry = entry?;
      let ft = entry.file_type()?;
      if ft.is_file() {
        total += entry.metadata()?.len();
      } else if ft.is_dir() {
        total += dir_size_inner(&entry.path(), max_depth, depth + 1)?;
      }
    }
  }

  Ok(total)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_bytes_to_str_bytes() {
    assert_eq!(bytes_to_str(0), "0 B");
    assert_eq!(bytes_to_str(512), "512 B");
    assert_eq!(bytes_to_str(1023), "1023 B");
  }

  #[test]
  fn test_bytes_to_str_kb() {
    assert_eq!(bytes_to_str(1024), "1.00 KB");
    assert_eq!(bytes_to_str(1536), "1.50 KB");
  }

  #[test]
  fn test_bytes_to_str_mb() {
    assert_eq!(bytes_to_str(1048576), "1.00 MB");
  }

  #[test]
  fn test_bytes_to_str_gb() {
    assert_eq!(bytes_to_str(1073741824), "1.00 GB");
  }

  #[test]
  fn test_bytes_to_str_tb() {
    assert_eq!(bytes_to_str(1099511627776), "1.00 TB");
  }

  #[test]
  fn test_tidy_path_duplicates() {
    assert_eq!(tidy_path("////absolute//path//"), "/absolute/path");
  }

  #[test]
  fn test_tidy_path_root() {
    assert_eq!(tidy_path("/"), "/");
  }

  #[test]
  fn test_tidy_path_empty() {
    assert_eq!(tidy_path(""), "");
  }

  #[test]
  fn test_tidy_path_backslash() {
    assert_eq!(tidy_path("C:\\Users\\test\\path"), "C:/Users/test/path");
  }

  #[test]
  fn test_path_decode_absolute() {
    let result = path_decode("/tmp");
    assert!(result.is_absolute());
  }

  #[test]
  fn test_dir_size_nonexistent() {
    let result = dir_size(Path::new("/nonexistent/path/xyz"), 3);
    // Non-existent paths return 0 (not an error) since is_dir/is_file are false
    assert_eq!(result.unwrap(), 0);
  }
}
