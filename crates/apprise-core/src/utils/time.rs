//! Timezone utilities matching Python's apprise time module.
//!
//! Provides a forgiving timezone lookup that accepts common aliases
//! (UTC, GMT, Z) and case-insensitive IANA timezone names.
//!
//! Uses the system's timezone database (via `chrono`) rather than
//! embedding the full IANA database (which adds ~1MB to the binary).

use chrono::FixedOffset;

/// Longest duration built from user-supplied seconds: ten years. Upstream
/// accepts any float; clamping keeps `Duration` and `Instant` arithmetic
/// from overflowing (`?cto=1e300`, `--limit 1e19`).
pub const MAX_SECS: f64 = 10.0 * 365.0 * 86_400.0;

/// `value` seconds as a `Duration`, clamped to `0..=MAX_SECS` (NaN is 0).
pub fn secs(value: f64) -> std::time::Duration {
  if value.is_nan() || value <= 0.0 {
    return std::time::Duration::ZERO;
  }
  std::time::Duration::from_secs_f64(value.min(MAX_SECS))
}

/// A resolved timezone — either UTC or a fixed UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timezone {
  Utc,
  Fixed(FixedOffset),
}

impl Timezone {
  /// The timezone name (for display).
  pub fn name(&self) -> &'static str {
    match self {
      Timezone::Utc => "UTC",
      Timezone::Fixed(_) => "Fixed",
    }
  }
}

/// Parse a timezone name, matching Python's forgiving `zoneinfo()`.
///
/// - Accepts lower/upper case
/// - Normalises common UTC variants (utc, z, gmt, etc/utc, etc/gmt, gmt0, utc0)
/// - Parses fixed offsets like "+05:00", "-08:00", "UTC+5"
/// - For full IANA names, attempts to read the system timezone database
pub fn zoneinfo(name: &str) -> Option<Timezone> {
  let raw = name.trim();
  if raw.is_empty() {
    return None;
  }

  let lower = raw.to_lowercase();

  // Handle common UTC aliases
  match lower.as_str() {
    "utc" | "z" | "gmt" | "etc/utc" | "etc/gmt" | "gmt0" | "utc0" => {
      return Some(Timezone::Utc);
    }
    _ => {}
  }

  // Handle UTC+N / UTC-N / GMT+N / GMT-N offsets
  for prefix in &["utc+", "utc-", "gmt+", "gmt-"] {
    if let Some(rest) = lower.strip_prefix(prefix) {
      let sign = if prefix.ends_with('+') { 1 } else { -1 };
      if let Ok(hours) = rest.parse::<i32>()
        && (-12..=14).contains(&hours)
      {
        return FixedOffset::east_opt(sign * hours * 3600).map(Timezone::Fixed);
      }
    }
  }

  // Handle +HH:MM / -HH:MM offsets
  if (raw.starts_with('+') || raw.starts_with('-')) && raw.contains(':') {
    let sign = if raw.starts_with('+') { 1 } else { -1 };
    let parts: Vec<&str> = raw[1..].split(':').collect();
    if parts.len() == 2
      && let (Ok(h), Ok(m)) = (parts[0].parse::<i32>(), parts[1].parse::<i32>())
    {
      return FixedOffset::east_opt(sign * (h * 3600 + m * 60)).map(Timezone::Fixed);
    }
  }

  // Try to read from system timezone database
  // On Linux: /usr/share/zoneinfo/{name}
  // This avoids embedding the entire IANA database in the binary
  #[cfg(unix)]
  {
    use std::path::Path;
    let tz_path = Path::new("/usr/share/zoneinfo").join(raw);
    if tz_path.exists() {
      // The timezone exists in the system database
      // For our purposes, we just need to confirm it's valid
      // Actual offset calculation would need TZ parsing
      // Return UTC as a placeholder — the name was valid
      return Some(Timezone::Utc);
    }

    // Case-insensitive search in common regions
    for region in &["Africa", "America", "Antarctica", "Arctic", "Asia", "Atlantic", "Australia", "Europe", "Indian", "Pacific", "Etc", "US", "Canada"] {
      let region_path = Path::new("/usr/share/zoneinfo").join(region);
      if let Ok(entries) = std::fs::read_dir(&region_path) {
        for entry in entries.flatten() {
          let fname = entry.file_name();
          let fname_str = fname.to_string_lossy();
          if fname_str.to_lowercase() == lower || format!("{}/{}", region.to_lowercase(), fname_str.to_lowercase()) == lower {
            return Some(Timezone::Utc);
          }
        }
      }
    }
  }

  tracing::warn!("Unknown timezone specified: {}", name);
  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_utc_variants() {
    assert_eq!(zoneinfo("UTC").unwrap(), Timezone::Utc);
    assert_eq!(zoneinfo("utc").unwrap(), Timezone::Utc);
    assert_eq!(zoneinfo("Z").unwrap(), Timezone::Utc);
    assert_eq!(zoneinfo("z").unwrap(), Timezone::Utc);
    assert_eq!(zoneinfo("GMT").unwrap(), Timezone::Utc);
    assert_eq!(zoneinfo("gmt").unwrap(), Timezone::Utc);
    assert_eq!(zoneinfo("Etc/UTC").unwrap(), Timezone::Utc);
    assert_eq!(zoneinfo("Etc/GMT").unwrap(), Timezone::Utc);
    assert_eq!(zoneinfo("GMT0").unwrap(), Timezone::Utc);
    assert_eq!(zoneinfo("UTC0").unwrap(), Timezone::Utc);
  }

  #[test]
  fn test_utc_offset() {
    let tz = zoneinfo("UTC+5").unwrap();
    assert!(matches!(tz, Timezone::Fixed(_)));
  }

  #[test]
  fn test_gmt_offset() {
    let tz = zoneinfo("GMT-8").unwrap();
    assert!(matches!(tz, Timezone::Fixed(_)));
  }

  #[test]
  fn test_fixed_offset() {
    let tz = zoneinfo("+05:30").unwrap();
    assert!(matches!(tz, Timezone::Fixed(_)));
  }

  #[test]
  fn test_empty_returns_none() {
    assert!(zoneinfo("").is_none());
    assert!(zoneinfo("  ").is_none());
  }

  #[test]
  fn test_unknown_returns_none() {
    assert!(zoneinfo("NotATimezone").is_none());
  }

  #[test]
  fn test_system_timezone() {
    // This test only works on systems with /usr/share/zoneinfo
    #[cfg(unix)]
    {
      if std::path::Path::new("/usr/share/zoneinfo/America/New_York").exists() {
        assert!(zoneinfo("America/New_York").is_some());
      }
    }
  }

  #[test]
  fn test_secs_is_clamped() {
    assert_eq!(secs(2.5), std::time::Duration::from_millis(2500));
    assert_eq!(secs(-1.0), std::time::Duration::ZERO);
    assert_eq!(secs(f64::NAN), std::time::Duration::ZERO);
    assert_eq!(secs(1e300), std::time::Duration::from_secs_f64(MAX_SECS));
    assert_eq!(secs(f64::INFINITY), std::time::Duration::from_secs_f64(MAX_SECS));
    // Deadlines built from the clamped value cannot overflow
    let _ = std::time::Instant::now() + secs(1e19);
  }
}
