// BSD 2-Clause License
//
// Apprise - Push Notification Library.
// Copyright (c) 2026, Chris Caron <lead2gold@gmail.com>
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
// LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
// CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
// SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
// INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
// CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
// ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
// POSSIBILITY OF SUCH DAMAGE.

//! PGP (Pretty Good Privacy) key handling utilities.
//!
//! Mirrors the Python `apprise.utils.pgp` module which provides:
//! - Importing and managing PGP public keys
//! - Encrypting content with PGP for email notifications
//!
//! Full PGP encryption (via the `pgpy` equivalent) requires a dedicated PGP
//! crate such as `sequoia-openpgp` or `rpgp`. This module provides the
//! foundational file I/O, key detection, and gpg binary availability checks
//! that the notification plugins need.

use std::path::Path;

/// Maximum accepted PGP public key file size (matches Python's 8000-byte limit).
pub const MAX_PGP_PUBLIC_KEY_SIZE: usize = 8000;

/// ASCII armor header for PGP public key blocks.
const PGP_PUBLIC_KEY_MARKER: &[u8] = b"-----BEGIN PGP PUBLIC KEY BLOCK-----";

/// Load a PGP public key from a file.
///
/// Returns the raw bytes of the file. The caller is responsible for parsing
/// the OpenPGP structure.
///
/// Returns an error if the file cannot be read or exceeds `MAX_PGP_PUBLIC_KEY_SIZE`.
pub fn load_pgp_key(path: &str) -> Result<Vec<u8>, std::io::Error> {
  let data = std::fs::read(path)?;
  if data.len() > MAX_PGP_PUBLIC_KEY_SIZE {
    return Err(std::io::Error::new(
      std::io::ErrorKind::InvalidData,
      format!("PGP key file exceeds maximum size ({} > {})", data.len(), MAX_PGP_PUBLIC_KEY_SIZE),
    ));
  }
  Ok(data)
}

/// Check if PGP encryption is available by looking for the `gpg` binary.
///
/// This mirrors the Python approach of checking whether the external gpg
/// tool is installed, which is needed as a fallback when no native PGP
/// library is available.
pub fn pgp_available() -> bool {
  std::process::Command::new("gpg")
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

/// Check if data looks like an ASCII-armored PGP public key.
pub fn is_pgp_public_key(data: &[u8]) -> bool {
  let trimmed = data.iter().position(|&b| !b.is_ascii_whitespace()).map(|pos| &data[pos..]).unwrap_or(data);
  trimmed.starts_with(PGP_PUBLIC_KEY_MARKER)
}

/// Check whether a PGP key file exists at the given path.
pub fn pgp_key_exists(path: &str) -> bool {
  Path::new(path).is_file()
}

/// Search for a PGP public key file in the given directory, trying common names.
///
/// Mirrors the Python `ApprisePGPController.public_keyfile()` search order:
/// - `{email_prefix}-pub.asc` (if email is provided, using part before @)
/// - `pgp-public.asc`
/// - `pgp-pub.asc`
/// - `public.asc`
/// - `pub.asc`
///
/// Returns the full path to the first match found, or `None`.
pub fn find_pgp_public_keyfile(dir: &str, email: Option<&str>) -> Option<String> {
  let mut candidates: Vec<String> = Vec::new();

  if let Some(addr) = email {
    // Try full email lowercase first, then just the local part
    let lower = addr.to_lowercase();
    candidates.push(format!("{}-pub.asc", lower));

    if let Some(local) = lower.split('@').next() {
      if local != lower {
        candidates.push(format!("{}-pub.asc", local));
      }
    }
  }

  candidates.extend_from_slice(&["pgp-public.asc".to_string(), "pgp-pub.asc".to_string(), "public.asc".to_string(), "pub.asc".to_string()]);

  let base = Path::new(dir);
  candidates.into_iter().find_map(|fname| {
    let full = base.join(&fname);
    if full.is_file() { full.to_str().map(|s| s.to_string()) } else { None }
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;

  #[test]
  fn test_is_pgp_public_key_valid() {
    let data = b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nVersion: GnuPG\n\nmQENB...";
    assert!(is_pgp_public_key(data));
  }

  #[test]
  fn test_is_pgp_public_key_with_whitespace() {
    let data = b"  \n-----BEGIN PGP PUBLIC KEY BLOCK-----\ndata";
    assert!(is_pgp_public_key(data));
  }

  #[test]
  fn test_is_pgp_public_key_invalid() {
    assert!(!is_pgp_public_key(b"not a pgp key"));
    assert!(!is_pgp_public_key(b""));
    assert!(!is_pgp_public_key(b"-----BEGIN PEM-----"));
  }

  #[test]
  fn test_load_pgp_key_file_not_found() {
    let result = load_pgp_key("/nonexistent/path/key.asc");
    assert!(result.is_err());
  }

  #[test]
  fn test_load_pgp_key_success() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("test.asc");
    let content = b"-----BEGIN PGP PUBLIC KEY BLOCK-----\ntest\n-----END PGP PUBLIC KEY BLOCK-----\n";
    fs::write(&key_path, content).unwrap();

    let result = load_pgp_key(key_path.to_str().unwrap());
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), content.to_vec());
  }

  #[test]
  fn test_load_pgp_key_too_large() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("big.asc");
    let content = vec![b'A'; MAX_PGP_PUBLIC_KEY_SIZE + 1];
    fs::write(&key_path, &content).unwrap();

    let result = load_pgp_key(key_path.to_str().unwrap());
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
  }

  #[test]
  fn test_find_pgp_public_keyfile_generic() {
    let dir = tempfile::tempdir().unwrap();
    let pub_path = dir.path().join("pub.asc");
    fs::write(&pub_path, b"key data").unwrap();

    let result = find_pgp_public_keyfile(dir.path().to_str().unwrap(), None);
    assert!(result.is_some());
    assert!(result.unwrap().ends_with("pub.asc"));
  }

  #[test]
  fn test_find_pgp_public_keyfile_with_email() {
    let dir = tempfile::tempdir().unwrap();
    let named_path = dir.path().join("user-pub.asc");
    fs::write(&named_path, b"key data").unwrap();

    let result = find_pgp_public_keyfile(dir.path().to_str().unwrap(), Some("User@example.com"));
    assert!(result.is_some());
    assert!(result.unwrap().ends_with("user-pub.asc"));
  }

  #[test]
  fn test_find_pgp_public_keyfile_not_found() {
    let dir = tempfile::tempdir().unwrap();
    assert!(find_pgp_public_keyfile(dir.path().to_str().unwrap(), None).is_none());
  }

  #[test]
  fn test_pgp_key_exists() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("key.asc");
    assert!(!pgp_key_exists(key_path.to_str().unwrap()));

    fs::write(&key_path, b"data").unwrap();
    assert!(pgp_key_exists(key_path.to_str().unwrap()));
  }

  #[test]
  fn test_pgp_available() {
    // Just verify it returns a bool without panicking
    let _ = pgp_available();
  }
}
