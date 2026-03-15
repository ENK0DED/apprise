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

//! Socket and TLS utilities for TCP transports.
//!
//! Mirrors the Python `apprise.utils.socket` module which provides:
//! - TCP keepalive configuration
//! - Timeout coercion (requests-compatible float or (connect, read) tuple)
//! - TLS context building helpers
//!
//! The full `SocketTransport` (connect, read, write, start_tls, reconnect)
//! is handled by `tokio::net::TcpStream` and `tokio-rustls` in the Rust port.
//! This module provides shared configuration helpers used across plugins that
//! need raw TCP connections (SMTP, IRC, XMPP, MQTT, etc.).

use std::time::Duration;

/// Default connection/read timeout in seconds (matches Python's default of 10.0).
pub const DEFAULT_TIMEOUT_SECS: f64 = 10.0;

/// Timeout specification matching Python's requests-compatible format.
///
/// - `Single(f64)` - both connect and read use the same timeout
/// - `Split { connect, read }` - separate connect and read timeouts
/// - `None` via the `Default` impl - uses `DEFAULT_TIMEOUT_SECS` for both
#[derive(Debug, Clone, PartialEq)]
pub enum TimeoutConfig {
    /// Both connect and read timeouts set to the same value.
    Single(f64),
    /// Separate connect and read timeouts (either may be None for no limit).
    Split {
        connect: Option<f64>,
        read: Option<f64>,
    },
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        TimeoutConfig::Single(DEFAULT_TIMEOUT_SECS)
    }
}

impl TimeoutConfig {
    /// Create a timeout config from a single value in seconds.
    pub fn from_secs(secs: f64) -> Result<Self, String> {
        if secs < 0.0 {
            return Err("timeout must be >= 0".to_string());
        }
        Ok(TimeoutConfig::Single(secs))
    }

    /// Create a split timeout config with separate connect and read values.
    pub fn split(connect: Option<f64>, read: Option<f64>) -> Result<Self, String> {
        if let Some(c) = connect {
            if c < 0.0 {
                return Err("connect timeout must be >= 0".to_string());
            }
        }
        if let Some(r) = read {
            if r < 0.0 {
                return Err("read timeout must be >= 0".to_string());
            }
        }
        Ok(TimeoutConfig::Split { connect, read })
    }

    /// Get the connect timeout as a `Duration`, or `None` for no limit.
    pub fn connect_timeout(&self) -> Option<Duration> {
        match self {
            TimeoutConfig::Single(secs) => Some(Duration::from_secs_f64(*secs)),
            TimeoutConfig::Split { connect, .. } => connect.map(Duration::from_secs_f64),
        }
    }

    /// Get the read timeout as a `Duration`, or `None` for no limit.
    pub fn read_timeout(&self) -> Option<Duration> {
        match self {
            TimeoutConfig::Single(secs) => Some(Duration::from_secs_f64(*secs)),
            TimeoutConfig::Split { read, .. } => read.map(Duration::from_secs_f64),
        }
    }
}

/// Configure TCP keepalive on a `tokio::net::TcpStream`.
///
/// Uses the platform's native keepalive support via `std::net::TcpStream`
/// (accessed through `tokio::net::TcpStream::as_ref()`). This avoids
/// needing the `socket2` crate.
///
/// Mirrors Python's `socket.setsockopt(SOL_SOCKET, SO_KEEPALIVE, 1)` call
/// in `SocketTransport.connect()`.
pub fn set_keepalive(stream: &tokio::net::TcpStream, interval_secs: u64) -> std::io::Result<()> {
    // Enable keepalive using libc-level setsockopt.
    // We access the raw fd directly from the tokio TcpStream on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = stream.as_raw_fd();
        let optval: libc::c_int = 1;

        // Enable SO_KEEPALIVE
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_KEEPALIVE,
                &optval as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Set TCP_KEEPIDLE (time before first keepalive probe)
        let keepidle = interval_secs as libc::c_int;
        let ret = unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_KEEPIDLE,
                &keepidle as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(())
    }

    #[cfg(not(unix))]
    {
        // On non-Unix platforms, just suppress the unused variable warnings
        let _ = stream;
        let _ = interval_secs;
        // Keepalive is best-effort; not critical if unsupported
        Ok(())
    }
}

/// Coerce a timeout value into a `TimeoutConfig`.
///
/// Accepts:
/// - A positive float (used for both connect and read)
/// - Zero (no timeout, immediate)
///
/// Mirrors Python's `SocketTransport._coerce_timeout()`.
pub fn coerce_timeout(secs: f64) -> Result<TimeoutConfig, String> {
    TimeoutConfig::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeout_config_default() {
        let cfg = TimeoutConfig::default();
        assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs_f64(10.0)));
        assert_eq!(cfg.read_timeout(), Some(Duration::from_secs_f64(10.0)));
    }

    #[test]
    fn test_timeout_config_single() {
        let cfg = TimeoutConfig::from_secs(5.0).unwrap();
        assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs(5)));
        assert_eq!(cfg.read_timeout(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn test_timeout_config_split() {
        let cfg = TimeoutConfig::split(Some(3.0), Some(10.0)).unwrap();
        assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs(3)));
        assert_eq!(cfg.read_timeout(), Some(Duration::from_secs(10)));
    }

    #[test]
    fn test_timeout_config_split_none() {
        let cfg = TimeoutConfig::split(None, None).unwrap();
        assert_eq!(cfg.connect_timeout(), None);
        assert_eq!(cfg.read_timeout(), None);
    }

    #[test]
    fn test_timeout_config_negative_rejected() {
        assert!(TimeoutConfig::from_secs(-1.0).is_err());
        assert!(TimeoutConfig::split(Some(-1.0), None).is_err());
        assert!(TimeoutConfig::split(None, Some(-1.0)).is_err());
    }

    #[test]
    fn test_coerce_timeout() {
        let cfg = coerce_timeout(7.5).unwrap();
        assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs_f64(7.5)));
        assert!(coerce_timeout(-1.0).is_err());
    }

    #[test]
    fn test_timeout_config_zero() {
        let cfg = TimeoutConfig::from_secs(0.0).unwrap();
        assert_eq!(cfg.connect_timeout(), Some(Duration::from_secs(0)));
        assert_eq!(cfg.read_timeout(), Some(Duration::from_secs(0)));
    }
}
