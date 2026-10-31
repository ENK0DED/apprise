//! Time utilities: clamped durations and upstream's forgiving time zone
//! lookup (`apprise/utils/time.py`).
//!
//! Zones come from the tz database: `$TZDIR` when it is set, else on Unix
//! the system's copy (the directories Python's `zoneinfo` searches,
//! starting with `/usr/share/zoneinfo`). Windows has none, so Windows
//! builds bundle one (`jiff-tzdb`) and only they carry the data.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{FixedOffset, Offset};

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

/// UTC spellings upstream accepts without consulting the database.
const UTC_ALIASES: &[&str] = &["utc", "z", "gmt", "etc/utc", "etc/gmt", "gmt0", "utc0"];

/// Where Python's `zoneinfo` looks for the database (its `TZPATH`).
#[cfg(not(windows))]
const TZPATH: &[&str] = &["/usr/share/zoneinfo", "/usr/lib/zoneinfo", "/usr/share/lib/zoneinfo", "/etc/zoneinfo"];

/// Zone files are a few KB; anything far larger is not one.
const MAX_TZIF_BYTES: u64 = 512 * 1024;

/// A time zone: UTC, or a zone from the tz database.
#[derive(Clone)]
pub struct Timezone {
  name: String,
  /// The zone's rules; `None` is UTC.
  rules: Option<Arc<tz::TimeZone>>,
}

impl Timezone {
  /// UTC (what upstream returns for `utc`, `z`, `gmt` and the like).
  pub fn utc() -> Self {
    Self { name: "UTC".to_string(), rules: None }
  }

  /// `UTC`, or the zone's name as the database spells it
  /// (`America/Toronto`).
  pub fn name(&self) -> &str {
    &self.name
  }

  /// The zone's offset from UTC at `unix_secs`.
  pub fn offset_at(&self, unix_secs: i64) -> FixedOffset {
    let secs = self.rules.as_ref().and_then(|tz| tz.find_local_time_type(unix_secs).ok()).map_or(0, |t| t.ut_offset());
    FixedOffset::east_opt(secs).unwrap_or_else(|| chrono::Utc.fix())
  }

  /// `at` in this zone.
  pub fn at(&self, at: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<FixedOffset> {
    at.with_timezone(&self.offset_at(at.timestamp()))
  }
}

impl PartialEq for Timezone {
  fn eq(&self, other: &Self) -> bool {
    self.name == other.name
  }
}

impl Eq for Timezone {}

impl std::fmt::Debug for Timezone {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_tuple("Timezone").field(&self.name).finish()
  }
}

/// Look up a time zone the way upstream's forgiving `zoneinfo()` does.
///
/// - Surrounding spaces are ignored and `UTC`, `Z`, `GMT`, `Etc/UTC`,
///   `Etc/GMT`, `GMT0` and `UTC0` (any case) are UTC.
/// - A database name matches in any case (`america/toronto`).
/// - So does a zone's location, the one or two parts after its region:
///   `Toronto` is `America/Toronto`, `Argentina/Cordoba` is
///   `America/Argentina/Cordoba`. Where several zones share a location,
///   the first in alphabetical order wins (upstream's pick is arbitrary).
/// - Anything else is `None`, with a warning. That includes offsets such
///   as `+05:00` and `UTC+5`. Note that `GMT+5` is the location of
///   `Etc/GMT+5`, which is five hours *behind* UTC (POSIX signs).
///
/// Only zone identifiers are looked up: a name with characters other than
/// ASCII letters, digits, `_`, `-`, `+` and `/`, or with an empty part
/// (`/etc/x`, `a//b`), is refused before the database is touched.
pub fn zoneinfo(name: &str) -> Option<Timezone> {
  let found = lookup(&Source::system(), name);
  if found.is_none() && !name.trim().is_empty() {
    tracing::warn!("Unknown timezone specified: {}", name);
  }
  found
}

fn lookup(source: &Source, name: &str) -> Option<Timezone> {
  let raw = name.trim();
  if raw.is_empty() {
    return None;
  }
  if UTC_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(raw)) {
    return Some(Timezone::utc());
  }
  if !is_zone_identifier(raw) {
    return None;
  }
  if let Some(tz) = source.load(raw) {
    return Some(tz);
  }
  source.names().into_iter().filter(|zone| location(zone).is_some_and(|l| l.eq_ignore_ascii_case(raw))).find_map(|zone| source.load(&zone))
}

/// Whether `name` can only name a zone file inside the database.
fn is_zone_identifier(name: &str) -> bool {
  name.split('/').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'+')))
}

/// Upstream's location of a zone: its second and third parts
/// (`America/Argentina/Cordoba` gives `Argentina/Cordoba`).
fn location(zone: &str) -> Option<String> {
  let parts: Vec<&str> = zone.split('/').skip(1).take(2).collect();
  (!parts.is_empty()).then(|| parts.join("/"))
}

/// A copy of the tz database.
enum Source {
  /// A zoneinfo directory.
  Dir(PathBuf),
  /// The copy built into Windows binaries.
  #[cfg(windows)]
  Bundled,
  /// No database: only the UTC aliases work.
  #[cfg_attr(windows, allow(dead_code))]
  None,
}

impl Source {
  /// The database this system provides.
  fn system() -> Source {
    let tzdir = std::env::var_os("TZDIR").filter(|d| !d.is_empty()).map(PathBuf::from);
    #[cfg(windows)]
    {
      tzdir.filter(|d| d.is_dir()).map_or(Source::Bundled, Source::Dir)
    }
    #[cfg(not(windows))]
    {
      tzdir.into_iter().chain(TZPATH.iter().map(PathBuf::from)).find(|d| d.is_dir()).map_or(Source::None, Source::Dir)
    }
  }

  /// Every file name in the database, sorted ignoring case.
  fn names(&self) -> Vec<String> {
    let mut names = match self {
      Source::Dir(root) => {
        let mut names = Vec::new();
        walk(root, "", 0, &mut names);
        names
      }
      #[cfg(windows)]
      Source::Bundled => jiff_tzdb::available().map(str::to_string).collect(),
      Source::None => Vec::new(),
    };
    names.sort_by_key(|n| n.to_ascii_lowercase());
    names
  }

  /// The zone called `name` (any case), if it is a valid zone file.
  fn load(&self, name: &str) -> Option<Timezone> {
    match self {
      Source::Dir(root) => {
        let canonical = if root.join(name).is_file() { name.to_string() } else { self.names().into_iter().find(|n| n.eq_ignore_ascii_case(name))? };
        let rules = read_tzif(&root.join(&canonical))?;
        Some(Timezone { name: canonical, rules: Some(Arc::new(rules)) })
      }
      #[cfg(windows)]
      Source::Bundled => {
        let (canonical, data) = jiff_tzdb::get(name)?;
        let rules = tz::TimeZone::from_tz_data(data).ok()?;
        Some(Timezone { name: canonical.to_string(), rules: Some(Arc::new(rules)) })
      }
      Source::None => None,
    }
  }
}

/// Collect the files under `dir` as `/`-separated names. Like Python's
/// `available_timezones()`, the `posix` and `right` copies are skipped.
/// Symbolic links count as files, so links to directories are not
/// followed.
fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<String>) {
  let Ok(entries) = std::fs::read_dir(dir) else { return };
  for entry in entries.flatten() {
    let Some(file_name) = entry.file_name().to_str().map(str::to_string) else { continue };
    let name = if prefix.is_empty() { file_name.clone() } else { format!("{}/{}", prefix, file_name) };
    match entry.file_type() {
      Ok(t) if t.is_dir() => {
        if depth < 3 && !(depth == 0 && matches!(file_name.as_str(), "posix" | "right")) {
          walk(&entry.path(), &name, depth + 1, out);
        }
      }
      Ok(_) => out.push(name),
      Err(_) => {}
    }
  }
}

/// Read and parse a TZif file.
fn read_tzif(path: &Path) -> Option<tz::TimeZone> {
  use std::io::Read;
  let file = std::fs::File::open(path).ok()?;
  if !file.metadata().ok()?.is_file() {
    return None;
  }
  let mut data = Vec::new();
  file.take(MAX_TZIF_BYTES).read_to_end(&mut data).ok()?;
  if !data.starts_with(b"TZif") {
    return None;
  }
  tz::TimeZone::from_tz_data(&data).ok()
}

/// Zones built from generated TZif data, so tests do not depend on the
/// machine's database or its own zone.
#[cfg(test)]
pub(crate) mod testing {
  use super::*;

  /// A version 2 TZif file without transitions: its footer TZ string
  /// (`EST5EDT,M3.2.0,M11.1.0`) gives the rules for all time, and
  /// `std_offset`/`abbr` describe its one local time type.
  pub(crate) fn tzif(tz_string: &str, std_offset: i32, abbr: &str) -> Vec<u8> {
    let block = |out: &mut Vec<u8>| {
      out.extend_from_slice(&std_offset.to_be_bytes());
      out.extend_from_slice(&[0, 0]);
      out.extend_from_slice(abbr.as_bytes());
      out.push(0);
    };
    let header = |out: &mut Vec<u8>| {
      out.extend_from_slice(b"TZif2");
      out.extend_from_slice(&[0; 15]);
      // isutcnt, isstdcnt, leapcnt, timecnt, typecnt, charcnt
      for n in [0u32, 0, 0, 0, 1, abbr.len() as u32 + 1] {
        out.extend_from_slice(&n.to_be_bytes());
      }
    };
    let mut out = Vec::new();
    header(&mut out);
    block(&mut out);
    header(&mut out);
    block(&mut out);
    out.extend_from_slice(format!("\n{}\n", tz_string).as_bytes());
    out
  }

  /// The zone `name` following `tz_string` for all time.
  pub(crate) fn zone(name: &str, tz_string: &str, std_offset: i32, abbr: &str) -> Timezone {
    let rules = tz::TimeZone::from_tz_data(&tzif(tz_string, std_offset, abbr)).expect("valid TZif");
    Timezone { name: name.to_string(), rules: Some(Arc::new(rules)) }
  }
}

#[cfg(test)]
mod tests {
  use super::testing::tzif;
  use super::*;

  /// A small zoneinfo directory, so the tests do not depend on the
  /// machine's database or its own zone.
  fn database() -> (tempfile::TempDir, Source) {
    let dir = tempfile::tempdir().unwrap();
    let zones: &[(&str, Vec<u8>)] = &[
      ("America/Toronto", tzif("EST5EDT,M3.2.0,M11.1.0", -5 * 3600, "EST")),
      ("America/Argentina/Cordoba", tzif("<-03>3", -3 * 3600, "-03")),
      ("America/Kentucky/Louisville", tzif("EST5EDT,M3.2.0,M11.1.0", -5 * 3600, "EST")),
      ("Asia/Kolkata", tzif("IST-5:30", 19800, "IST")),
      ("Etc/GMT+5", tzif("<-05>5", -5 * 3600, "-05")),
      ("Europe/London", tzif("GMT0BST,M3.5.0/1,M10.5.0", 0, "GMT")),
      ("Canada/Pacific", tzif("PST8PDT,M3.2.0,M11.1.0", -8 * 3600, "PST")),
      ("US/Pacific", tzif("PST8PDT,M3.2.0,M11.1.0", -8 * 3600, "PST")),
      ("posix/Europe/Paris", tzif("CET-1CEST,M3.5.0,M10.5.0/3", 3600, "CET")),
      ("Factory", tzif("<-00>0", 0, "-00")),
    ];
    for (name, data) in zones {
      let path = dir.path().join(name);
      std::fs::create_dir_all(path.parent().unwrap()).unwrap();
      std::fs::write(path, data).unwrap();
    }
    std::fs::write(dir.path().join("zone.tab"), "# not a zone file\n").unwrap();
    std::fs::create_dir_all(dir.path().join("Antarctica")).unwrap();
    std::fs::write(dir.path().join("Antarctica/Broken"), b"TZif2 truncated").unwrap();
    let source = Source::Dir(dir.path().to_path_buf());
    (dir, source)
  }

  fn name_of(source: &Source, query: &str) -> Option<String> {
    lookup(source, query).map(|tz| tz.name().to_string())
  }

  /// 2026-01-15 12:00 UTC and 2026-07-15 12:00 UTC.
  const WINTER: i64 = 1_768_478_400;
  const SUMMER: i64 = 1_784_116_800;

  // Each case checked against upstream's zoneinfo().
  #[test]
  fn test_utc_variants() {
    let (_dir, db) = database();
    for name in ["UTC", "utc", " utc ", "Z", "z", "GMT", "gmt", "Etc/UTC", "etc/utc", "Etc/GMT", "GMT0", "UTC0"] {
      assert_eq!(lookup(&db, name), Some(Timezone::utc()), "{}", name);
    }
    // No database needed
    assert_eq!(lookup(&Source::None, "UTC"), Some(Timezone::utc()));
    assert_eq!(Timezone::utc().offset_at(SUMMER), chrono::Utc.fix());
  }

  #[test]
  fn test_names_ignore_case_and_spaces() {
    let (_dir, db) = database();
    assert_eq!(name_of(&db, "America/Toronto").as_deref(), Some("America/Toronto"));
    assert_eq!(name_of(&db, "america/toronto").as_deref(), Some("America/Toronto"));
    assert_eq!(name_of(&db, "  AMERICA/TORONTO ").as_deref(), Some("America/Toronto"));
    assert_eq!(name_of(&db, "Factory").as_deref(), Some("Factory"));
  }

  #[test]
  fn test_location_aliases() {
    let (_dir, db) = database();
    assert_eq!(name_of(&db, "Toronto").as_deref(), Some("America/Toronto"));
    assert_eq!(name_of(&db, "toronto").as_deref(), Some("America/Toronto"));
    assert_eq!(name_of(&db, "Argentina/Cordoba").as_deref(), Some("America/Argentina/Cordoba"));
    assert_eq!(name_of(&db, "argentina/cordoba").as_deref(), Some("America/Argentina/Cordoba"));
    assert_eq!(name_of(&db, "Kentucky/Louisville").as_deref(), Some("America/Kentucky/Louisville"));
    // Only the region is dropped, and only whole parts match
    assert_eq!(name_of(&db, "Argentina"), None);
    assert_eq!(name_of(&db, "Cordoba"), None);
    assert_eq!(name_of(&db, "Louisville"), None);
    // POSIX signs: Etc/GMT+5 is five hours behind UTC
    assert_eq!(name_of(&db, "GMT+5").as_deref(), Some("Etc/GMT+5"));
    assert_eq!(lookup(&db, "gmt+5").unwrap().offset_at(SUMMER).local_minus_utc(), -5 * 3600);
    // A shared location picks the first zone alphabetically
    assert_eq!(name_of(&db, "pacific").as_deref(), Some("Canada/Pacific"));
    // The posix/ copy is not searched
    assert_eq!(name_of(&db, "Paris"), None);
  }

  #[test]
  fn test_unknown_names() {
    let (_dir, db) = database();
    for name in ["", "  ", "invalid", "+05:00", "-08:00", "UTC+5", "EST5EDT", "zone.tab", "Antarctica/Broken", "Broken"] {
      assert_eq!(name_of(&db, name), None, "{}", name);
    }
    assert_eq!(name_of(&Source::None, "America/Toronto"), None);
  }

  #[test]
  fn test_only_zone_identifiers_are_looked_up() {
    let (dir, db) = database();
    // A real zone file reached through a path is still refused
    let absolute = dir.path().join("America/Toronto");
    for name in [
      absolute.to_str().unwrap(),
      "/America/Toronto",
      "America/Toronto/",
      "America//Toronto",
      "../America/Toronto",
      "America/../America/Toronto",
      "./America/Toronto",
      "America\\Toronto",
      "/etc/shadow",
      "../../etc/passwd",
    ] {
      assert_eq!(name_of(&db, name), None, "{}", name);
    }
    assert!(is_zone_identifier("Etc/GMT+5"));
    assert!(is_zone_identifier("America/Port-au-Prince"));
    assert!(!is_zone_identifier("Europe/../UTC"));
  }

  #[test]
  fn test_offsets_follow_the_rules() {
    let (_dir, db) = database();
    let toronto = lookup(&db, "America/Toronto").unwrap();
    assert_eq!(toronto.offset_at(WINTER).local_minus_utc(), -5 * 3600);
    assert_eq!(toronto.offset_at(SUMMER).local_minus_utc(), -4 * 3600);
    let london = lookup(&db, "Europe/London").unwrap();
    assert_eq!(london.offset_at(WINTER).local_minus_utc(), 0);
    assert_eq!(london.offset_at(SUMMER).local_minus_utc(), 3600);
    let kolkata = lookup(&db, "asia/kolkata").unwrap();
    assert_eq!(kolkata.offset_at(SUMMER).local_minus_utc(), 19800);
    let at = chrono::DateTime::from_timestamp(SUMMER, 0).unwrap();
    assert_eq!(toronto.at(at).to_rfc2822(), "Wed, 15 Jul 2026 08:00:00 -0400");
    // The zones other tests build without a database
    let built = super::testing::zone("Etc/GMT+5", "<-05>5", -5 * 3600, "-05");
    assert_eq!(built.name(), "Etc/GMT+5");
    assert_eq!(built.offset_at(WINTER).local_minus_utc(), -5 * 3600);
  }

  #[test]
  fn test_system_database() {
    // Whatever this machine's own zone is, a named zone has its own rules
    let Some(tz) = zoneinfo("America/Toronto") else { return };
    assert_eq!(tz.name(), "America/Toronto");
    assert_eq!(tz.offset_at(WINTER).local_minus_utc(), -5 * 3600);
    assert_eq!(zoneinfo("toronto"), Some(tz));
    assert_eq!(zoneinfo("/etc/passwd"), None);
    assert_eq!(zoneinfo("NotATimezone"), None);
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
