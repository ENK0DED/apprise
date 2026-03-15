//! Timezone utilities matching Python's apprise time module.
//!
//! Provides a forgiving timezone lookup that accepts common aliases
//! (UTC, GMT, Z) and case-insensitive IANA timezone names.

use chrono_tz::Tz;

/// Parse a timezone name into a `chrono_tz::Tz`, matching Python's forgiving `zoneinfo()`.
///
/// - Accepts lower/upper case
/// - Normalises common UTC variants (utc, z, gmt, etc/utc, etc/gmt, gmt0, utc0)
/// - Falls back to case-insensitive search of IANA timezone database
pub fn zoneinfo(name: &str) -> Option<Tz> {
    let raw = name.trim();
    if raw.is_empty() {
        return None;
    }

    let lower = raw.to_lowercase();

    // Handle common UTC aliases
    match lower.as_str() {
        "utc" | "z" | "gmt" | "etc/utc" | "etc/gmt" | "gmt0" | "utc0" => {
            return Some(chrono_tz::UTC);
        }
        _ => {}
    }

    // Try exact match first
    if let Ok(tz) = raw.parse::<Tz>() {
        return Some(tz);
    }

    // Case-insensitive search
    for tz in chrono_tz::TZ_VARIANTS {
        let tz_name = tz.name().to_lowercase();
        if tz_name == lower {
            return Some(tz);
        }
        // Also try matching just the location part (e.g., "montreal" matches "America/Montreal")
        if let Some(location) = tz_name.split('/').last() {
            if location == lower {
                return Some(tz);
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
        assert_eq!(zoneinfo("UTC").unwrap(), chrono_tz::UTC);
        assert_eq!(zoneinfo("utc").unwrap(), chrono_tz::UTC);
        assert_eq!(zoneinfo("Z").unwrap(), chrono_tz::UTC);
        assert_eq!(zoneinfo("z").unwrap(), chrono_tz::UTC);
        assert_eq!(zoneinfo("GMT").unwrap(), chrono_tz::UTC);
        assert_eq!(zoneinfo("gmt").unwrap(), chrono_tz::UTC);
        assert_eq!(zoneinfo("Etc/UTC").unwrap(), chrono_tz::UTC);
        assert_eq!(zoneinfo("Etc/GMT").unwrap(), chrono_tz::UTC);
        assert_eq!(zoneinfo("GMT0").unwrap(), chrono_tz::UTC);
        assert_eq!(zoneinfo("UTC0").unwrap(), chrono_tz::UTC);
    }

    #[test]
    fn test_exact_match() {
        let tz = zoneinfo("America/New_York").unwrap();
        assert_eq!(tz.name(), "America/New_York");
    }

    #[test]
    fn test_case_insensitive() {
        let tz = zoneinfo("america/new_york").unwrap();
        assert_eq!(tz.name(), "America/New_York");
    }

    #[test]
    fn test_location_only() {
        // "Montreal" should match "America/Montreal"
        let tz = zoneinfo("Montreal");
        assert!(tz.is_some());
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
}
