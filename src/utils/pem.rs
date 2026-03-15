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

//! PEM certificate and key handling utilities.
//!
//! Mirrors the Python `apprise.utils.pem` module which provides:
//! - Loading PEM-encoded private/public keys from files
//! - Checking whether data looks like PEM-encoded content
//! - Key pair management for VAPID/WebPush
//!
//! The full `ApprisePEMController` (key generation, ECIES encrypt/decrypt,
//! WebPush encryption, ECDSA signing) requires the `ring` or `p256` crates
//! and will be implemented when VAPID plugin support is finalized. This module
//! provides the foundational file I/O and detection helpers.

use std::path::Path;

/// Maximum accepted PEM key file size (matches Python's 8000-byte limit).
pub const MAX_PEM_KEY_SIZE: usize = 8000;

/// Maximum WebPush record size (matches Python's 4096-byte limit).
pub const MAX_WEBPUSH_RECORD_SIZE: usize = 4096;

/// PEM header prefix used to identify PEM-encoded content.
const PEM_BEGIN_MARKER: &[u8] = b"-----BEGIN";

/// Load a PEM-encoded key (private or public) from a file path.
///
/// Returns the raw bytes of the file. The caller is responsible for parsing
/// the PEM structure (e.g., via `rustls-pemfile` or `p256`).
///
/// Returns an error if the file cannot be read or exceeds `MAX_PEM_KEY_SIZE`.
pub fn load_pem_key(path: &str) -> Result<Vec<u8>, std::io::Error> {
    let data = std::fs::read(path)?;
    if data.len() > MAX_PEM_KEY_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "PEM key file exceeds maximum size ({} > {})",
                data.len(),
                MAX_PEM_KEY_SIZE
            ),
        ));
    }
    Ok(data)
}

/// Check if data looks like PEM-encoded content.
///
/// Returns `true` if the data starts with the standard `-----BEGIN` marker.
pub fn is_pem(data: &[u8]) -> bool {
    // Strip leading whitespace before checking
    let trimmed = data
        .iter()
        .position(|&b| !b.is_ascii_whitespace())
        .map(|pos| &data[pos..])
        .unwrap_or(data);
    trimmed.starts_with(PEM_BEGIN_MARKER)
}

/// Check whether a PEM key file exists at the given path.
pub fn pem_key_exists(path: &str) -> bool {
    Path::new(path).is_file()
}

/// Search for a public key file in the given directory, trying common names.
///
/// Mirrors the Python `ApprisePEMController.public_keyfile()` search order:
/// - `{name}-public_key.pem` (if name is provided)
/// - `public_key.pem`
/// - `public.pem`
/// - `pub.pem`
///
/// Returns the full path to the first match found, or `None`.
pub fn find_public_keyfile(dir: &str, name: Option<&str>) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();

    if let Some(n) = name {
        let clean = n.trim_matches(|c: char| " \t/-+!$@#*".contains(c)).to_lowercase();
        if !clean.is_empty() {
            candidates.push(format!("{}-public_key.pem", clean));
        }
    }

    candidates.extend_from_slice(&[
        "public_key.pem".to_string(),
        "public.pem".to_string(),
        "pub.pem".to_string(),
    ]);

    let base = Path::new(dir);
    candidates.into_iter().find_map(|fname| {
        let full = base.join(&fname);
        if full.is_file() {
            full.to_str().map(|s| s.to_string())
        } else {
            None
        }
    })
}

/// Search for a private key file in the given directory, trying common names.
///
/// Mirrors the Python `ApprisePEMController.private_keyfile()` search order:
/// - `{name}-private_key.pem` (if name is provided)
/// - `private_key.pem`
/// - `private.pem`
/// - `prv.pem`
///
/// Returns the full path to the first match found, or `None`.
pub fn find_private_keyfile(dir: &str, name: Option<&str>) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();

    if let Some(n) = name {
        let clean = n.trim_matches(|c: char| " \t/-+!$@#*".contains(c)).to_lowercase();
        if !clean.is_empty() {
            candidates.push(format!("{}-private_key.pem", clean));
        }
    }

    candidates.extend_from_slice(&[
        "private_key.pem".to_string(),
        "private.pem".to_string(),
        "prv.pem".to_string(),
    ]);

    let base = Path::new(dir);
    candidates.into_iter().find_map(|fname| {
        let full = base.join(&fname);
        if full.is_file() {
            full.to_str().map(|s| s.to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_is_pem_valid() {
        assert!(is_pem(b"-----BEGIN PRIVATE KEY-----\ndata\n-----END PRIVATE KEY-----"));
        assert!(is_pem(b"-----BEGIN EC PRIVATE KEY-----\ndata"));
        assert!(is_pem(b"-----BEGIN PUBLIC KEY-----\ndata"));
        assert!(is_pem(b"-----BEGIN CERTIFICATE-----\ndata"));
    }

    #[test]
    fn test_is_pem_with_leading_whitespace() {
        assert!(is_pem(b"  \n\t-----BEGIN PRIVATE KEY-----\ndata"));
    }

    #[test]
    fn test_is_pem_invalid() {
        assert!(!is_pem(b"not a pem file"));
        assert!(!is_pem(b""));
        assert!(!is_pem(b"-----NOTBEGIN"));
        assert!(!is_pem(b"random binary data\x00\x01\x02"));
    }

    #[test]
    fn test_load_pem_key_file_not_found() {
        let result = load_pem_key("/nonexistent/path/key.pem");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_pem_key_success() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("test.pem");
        let content = b"-----BEGIN PRIVATE KEY-----\ntest\n-----END PRIVATE KEY-----\n";
        fs::write(&key_path, content).unwrap();

        let result = load_pem_key(key_path.to_str().unwrap());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), content.to_vec());
    }

    #[test]
    fn test_load_pem_key_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("big.pem");
        let content = vec![b'A'; MAX_PEM_KEY_SIZE + 1];
        fs::write(&key_path, &content).unwrap();

        let result = load_pem_key(key_path.to_str().unwrap());
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_find_public_keyfile() {
        let dir = tempfile::tempdir().unwrap();
        let pub_path = dir.path().join("public_key.pem");
        fs::write(&pub_path, b"key data").unwrap();

        let result = find_public_keyfile(dir.path().to_str().unwrap(), None);
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("public_key.pem"));
    }

    #[test]
    fn test_find_public_keyfile_with_name() {
        let dir = tempfile::tempdir().unwrap();
        let named_path = dir.path().join("myapp-public_key.pem");
        fs::write(&named_path, b"key data").unwrap();

        let result = find_public_keyfile(dir.path().to_str().unwrap(), Some("myapp"));
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("myapp-public_key.pem"));
    }

    #[test]
    fn test_find_private_keyfile() {
        let dir = tempfile::tempdir().unwrap();
        let prv_path = dir.path().join("private_key.pem");
        fs::write(&prv_path, b"key data").unwrap();

        let result = find_private_keyfile(dir.path().to_str().unwrap(), None);
        assert!(result.is_some());
        assert!(result.unwrap().ends_with("private_key.pem"));
    }

    #[test]
    fn test_find_keyfile_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_public_keyfile(dir.path().to_str().unwrap(), None).is_none());
        assert!(find_private_keyfile(dir.path().to_str().unwrap(), None).is_none());
    }

    #[test]
    fn test_pem_key_exists() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("key.pem");
        assert!(!pem_key_exists(key_path.to_str().unwrap()));

        fs::write(&key_path, b"data").unwrap();
        assert!(pem_key_exists(key_path.to_str().unwrap()));
    }
}
