//! Command-line parsing that behaves like upstream's Click interface.
//!
//! Click accepts multi-letter single-dash options (`-SPD`, `-SL`, `-tv`) as
//! whole option names, lets options and URLs mix, splits combined short
//! flags (`-vvvv`, `-bvalue`), and reports problems in its own words. This
//! parser follows its algorithm so scripts written for upstream keep working.

use std::collections::HashMap;

/// What an option holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
  Str,
  Int,
  Float,
  Flag,
  Count,
}

/// One option: its names (declaration order, long first), metavar, help.
#[derive(Debug, Clone)]
pub struct OptSpec {
  pub id: &'static str,
  pub names: &'static [&'static str],
  pub kind: Kind,
  pub metavar: &'static str,
  pub multiple: bool,
}

/// Every option, in the order upstream's `--help` lists them.
pub const OPTIONS: &[OptSpec] = &[
  opt("body", &["--body", "-b"], Kind::Str, "TEXT", false),
  opt("title", &["--title", "-t"], Kind::Str, "TEXT", false),
  opt("plugin_path", &["--plugin-path", "-P"], Kind::Str, "PATH", true),
  opt("storage_path", &["--storage-path", "-S"], Kind::Str, "PATH", false),
  opt("storage_prune_days", &["--storage-prune-days", "-SPD"], Kind::Int, "INTEGER", false),
  opt("storage_uid_length", &["--storage-uid-length", "-SUL"], Kind::Int, "INTEGER", false),
  opt("storage_mode", &["--storage-mode", "-SM"], Kind::Str, "MODE", false),
  opt("config", &["--config", "-c"], Kind::Str, "CONFIG_URL", true),
  opt("attach", &["--attach", "-a"], Kind::Str, "ATTACHMENT_URL", true),
  opt("notification_type", &["--notification-type", "-n"], Kind::Str, "TYPE", false),
  opt("input_format", &["--input-format", "-i"], Kind::Str, "FORMAT", false),
  opt("theme", &["--theme", "-T"], Kind::Str, "THEME", false),
  opt("template_var", &["--template-var", "-tv"], Kind::Str, "NAME=VALUE", true),
  opt("tag", &["--tag", "-g"], Kind::Str, "TAG", true),
  opt("limit", &["--limit", "-L"], Kind::Float, "SECONDS", false),
  opt("service_limit", &["--service-limit", "-SL"], Kind::Float, "SECONDS", false),
  opt("disable_async", &["--disable-async", "-Da"], Kind::Flag, "", false),
  opt("dry_run", &["--dry-run", "-d"], Kind::Flag, "", false),
  opt("details", &["--details", "-l"], Kind::Flag, "", false),
  opt("recursion_depth", &["--recursion-depth", "-R"], Kind::Int, "INTEGER", false),
  opt("verbose", &["--verbose", "-v"], Kind::Count, "", false),
  opt("interpret_escapes", &["--interpret-escapes", "-e"], Kind::Flag, "", false),
  opt("interpret_emojis", &["--interpret-emojis", "-j"], Kind::Flag, "", false),
  opt("debug", &["--debug", "-D"], Kind::Flag, "", false),
  opt("version", &["--version", "-V"], Kind::Flag, "", false),
  opt("help", &["-h", "--help"], Kind::Flag, "", false),
];

const fn opt(id: &'static str, names: &'static [&'static str], kind: Kind, metavar: &'static str, multiple: bool) -> OptSpec {
  OptSpec { id, names, kind, metavar, multiple }
}

/// A typed option value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
  Str(String),
  Int(i64),
  Float(f64),
}

/// The parsed command line.
#[derive(Debug, Default)]
pub struct Parsed {
  /// Values by option id, in the order given.
  pub values: HashMap<&'static str, Vec<Value>>,
  /// Flags and counts by option id.
  pub counts: HashMap<&'static str, u32>,
  /// Positional arguments (URLs, or the storage action and filters).
  pub args: Vec<String>,
}

impl Parsed {
  pub fn flag(&self, id: &str) -> bool {
    self.counts.get(id).is_some_and(|c| *c > 0)
  }

  pub fn count(&self, id: &str) -> u32 {
    self.counts.get(id).copied().unwrap_or(0)
  }

  /// The last string given for `id`.
  pub fn str(&self, id: &str) -> Option<&str> {
    self.values.get(id)?.iter().rev().find_map(|v| if let Value::Str(s) = v { Some(s.as_str()) } else { None })
  }

  /// Every string given for `id`.
  pub fn strs(&self, id: &str) -> Vec<String> {
    self.values.get(id).map(|vs| vs.iter().filter_map(|v| if let Value::Str(s) = v { Some(s.clone()) } else { None }).collect()).unwrap_or_default()
  }

  pub fn int(&self, id: &str) -> Option<i64> {
    self.values.get(id)?.iter().rev().find_map(|v| if let Value::Int(i) = v { Some(*i) } else { None })
  }

  pub fn float(&self, id: &str) -> Option<f64> {
    self.values.get(id)?.iter().rev().find_map(|v| if let Value::Float(f) = v { Some(*f) } else { None })
  }
}

/// A command-line problem, worded as Click words it.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgError {
  /// An unknown option, with close matches among the long names.
  NoSuchOption { name: String, suggestions: Vec<String> },
  /// An option given without its value, or a flag given one.
  BadUsage(String),
  /// A value of the wrong type.
  BadValue(String),
}

impl ArgError {
  /// The full message Click prints (to stderr), exit code 2.
  pub fn render(&self) -> String {
    const USAGE: &str = "Usage: apprise [OPTIONS] SERVICE_URL [SERVICE_URL2 [SERVICE_URL3]]\nTry 'apprise --help' for help.\n\n";
    match self {
      Self::NoSuchOption { name, suggestions } => {
        let hint = match suggestions.as_slice() {
          [] => String::new(),
          [one] => format!(" Did you mean '{one}'?"),
          many => format!(" (Did you mean one of: {}?)", many.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ")),
        };
        format!("{USAGE}Error: No such option '{name}'.{hint}")
      }
      Self::BadUsage(msg) => format!("Error: {msg}"),
      Self::BadValue(msg) => format!("{USAGE}Error: {msg}"),
    }
  }
}

fn find(name: &str) -> Option<&'static OptSpec> {
  OPTIONS.iter().find(|o| o.names.contains(&name))
}

/// Long option names: everything that is not a dash and one character.
fn long_names() -> Vec<&'static str> {
  OPTIONS.iter().flat_map(|o| o.names.iter().copied()).filter(|n| n.len() != 2).collect()
}

/// Parse `argv` (without the program name).
pub fn parse(argv: &[String]) -> Result<Parsed, ArgError> {
  let mut parsed = Parsed::default();
  // (spec, name as typed, raw value) in the order given; converted later.
  let mut raw: Vec<(&'static OptSpec, String, String)> = Vec::new();
  let mut rargs: std::collections::VecDeque<String> = argv.iter().cloned().collect();
  while let Some(arg) = rargs.pop_front() {
    if arg == "--" {
      parsed.args.extend(rargs.drain(..));
      break;
    }
    if !(arg.starts_with('-') && arg.len() > 1) {
      parsed.args.push(arg);
      continue;
    }
    let (name, explicit) = match arg.split_once('=') {
      Some((n, v)) => (n.to_string(), Some(v.to_string())),
      None => (arg.clone(), None),
    };
    if let Some(spec) = find(&name).filter(|_| name.len() != 2) {
      // A long option (including Click's multi-letter single-dash names).
      if spec.kind == Kind::Str || spec.kind == Kind::Int || spec.kind == Kind::Float {
        let value = match explicit {
          Some(v) => v,
          None => rargs.pop_front().ok_or_else(|| ArgError::BadUsage(format!("Option '{name}' requires an argument.")))?,
        };
        raw.push((spec, name, value));
      } else if explicit.is_some() {
        return Err(ArgError::BadUsage(format!("Option '{name}' does not take a value.")));
      } else {
        *parsed.counts.entry(spec.id).or_default() += 1;
        if spec.id == "help" {
          return Ok(help_only());
        }
      }
      continue;
    }
    if arg.starts_with("--") {
      return Err(ArgError::NoSuchOption { suggestions: close_matches(&name, &long_names()), name });
    }
    // Short options, possibly combined (`-vvv`, `-bvalue`).
    let chars: Vec<char> = arg.chars().collect();
    let mut i = 1;
    while i < chars.len() {
      let short = format!("-{}", chars[i]);
      i += 1;
      let Some(spec) = find(&short).filter(|o| o.names.contains(&short.as_str())) else {
        return Err(ArgError::NoSuchOption { name: short, suggestions: Vec::new() });
      };
      match spec.kind {
        Kind::Str | Kind::Int | Kind::Float => {
          let value = if i < chars.len() {
            chars[i..].iter().collect()
          } else {
            rargs.pop_front().ok_or_else(|| ArgError::BadUsage(format!("Option '{short}' requires an argument.")))?
          };
          raw.push((spec, short, value));
          break;
        }
        _ => {
          *parsed.counts.entry(spec.id).or_default() += 1;
          if spec.id == "help" {
            return Ok(help_only());
          }
        }
      }
    }
  }

  for (spec, _, value) in raw {
    let typed = match spec.kind {
      Kind::Int => Value::Int(parse_int(&value).ok_or_else(|| bad_value(spec, &value, "integer"))?),
      Kind::Float => Value::Float(parse_float(&value).ok_or_else(|| bad_value(spec, &value, "float"))?),
      _ => Value::Str(value),
    };
    let entry = parsed.values.entry(spec.id).or_default();
    if !spec.multiple {
      entry.clear();
    }
    entry.push(typed);
  }
  Ok(parsed)
}

fn help_only() -> Parsed {
  let mut p = Parsed::default();
  p.counts.insert("help", 1);
  p
}

fn bad_value(spec: &OptSpec, value: &str, what: &str) -> ArgError {
  let names = spec.names.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(" / ");
  ArgError::BadValue(format!("Invalid value for {names}: '{value}' is not a valid {what}."))
}

/// Python `int()`: surrounding whitespace, a sign, `_` between digits.
pub(crate) fn parse_int(value: &str) -> Option<i64> {
  let v = value.trim();
  let digits = v.strip_prefix(['+', '-']).unwrap_or(v);
  if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
    return None;
  }
  v.replace('_', "").parse().ok()
}

/// Python `float()`, including `inf` and `nan`.
fn parse_float(value: &str) -> Option<f64> {
  let v = value.trim().replace('_', "");
  let lower = v.to_ascii_lowercase();
  let unsigned = lower.trim_start_matches(['+', '-']);
  if matches!(unsigned, "inf" | "infinity" | "nan") {
    return lower.parse().ok().or(Some(if lower.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY }));
  }
  if unsigned.chars().any(|c| c.is_ascii_alphabetic() && c != 'e') {
    return None;
  }
  v.parse().ok()
}

/// Python `difflib.get_close_matches(word, possibilities)`: up to three
/// names with a similarity ratio of at least 0.6, returned sorted.
pub fn close_matches(word: &str, possibilities: &[&str]) -> Vec<String> {
  let mut scored: Vec<(f64, &str)> = possibilities.iter().map(|p| (ratio(p, word), *p)).filter(|(r, _)| *r >= 0.6).collect();
  scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| b.1.cmp(a.1)));
  let mut out: Vec<String> = scored.into_iter().take(3).map(|(_, p)| p.to_string()).collect();
  out.sort();
  out
}

/// `SequenceMatcher(a=a, b=b).ratio()`.
fn ratio(a: &str, b: &str) -> f64 {
  let a: Vec<char> = a.chars().collect();
  let b: Vec<char> = b.chars().collect();
  let total = a.len() + b.len();
  if total == 0 {
    return 1.0;
  }
  2.0 * matched(&a, &b) as f64 / total as f64
}

/// Characters in the matching blocks, found the way difflib does: the
/// longest common run (earliest in `a`, then in `b`), then both sides of it.
fn matched(a: &[char], b: &[char]) -> usize {
  if a.is_empty() || b.is_empty() {
    return 0;
  }
  let (mut best_i, mut best_j, mut best) = (0, 0, 0);
  let mut prev = vec![0usize; b.len() + 1];
  for i in 0..a.len() {
    let mut cur = vec![0usize; b.len() + 1];
    for j in 0..b.len() {
      if a[i] == b[j] {
        cur[j + 1] = prev[j] + 1;
        let k = cur[j + 1];
        let (si, sj) = (i + 1 - k, j + 1 - k);
        if k > best || (k == best && (si < best_i || (si == best_i && sj < best_j))) {
          (best_i, best_j, best) = (si, sj, k);
        }
      }
    }
    prev = cur;
  }
  if best == 0 {
    return 0;
  }
  best + matched(&a[..best_i], &b[..best_j]) + matched(&a[best_i + best..], &b[best_j + best..])
}

#[cfg(test)]
mod tests {
  use super::*;

  fn p(args: &[&str]) -> Result<Parsed, ArgError> {
    parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
  }

  #[test]
  fn test_long_short_and_multi_letter_names() {
    let r = p(&["-b", "body", "--title=T", "-SPD", "10", "-SUL=4", "-tv", "a=b", "-tv", "c=d", "json://x", "-Da", "-vvv", "-L2.5"]).unwrap();
    assert_eq!(r.str("body"), Some("body"));
    assert_eq!(r.str("title"), Some("T"));
    assert_eq!(r.int("storage_prune_days"), Some(10));
    assert_eq!(r.int("storage_uid_length"), Some(4));
    assert_eq!(r.strs("template_var"), vec!["a=b", "c=d"]);
    assert_eq!(r.args, vec!["json://x"]);
    assert!(r.flag("disable_async"));
    assert_eq!(r.count("verbose"), 3);
    assert_eq!(r.float("limit"), Some(2.5));
  }

  #[test]
  fn test_combined_short_options() {
    // Not a long name, so -S takes the rest as its value, as Click does.
    let r = p(&["-SPD10", "-bhello", "-dv"]).unwrap();
    assert_eq!(r.str("storage_path"), Some("PD10"));
    assert_eq!(r.str("body"), Some("hello"));
    assert!(r.flag("dry_run"));
    assert_eq!(r.count("verbose"), 1);
    // A value may look like an option.
    assert_eq!(p(&["-b", "-x"]).unwrap().str("body"), Some("-x"));
    // Everything after -- is positional.
    assert_eq!(p(&["-d", "--", "-x", "--y"]).unwrap().args, vec!["-x", "--y"]);
  }

  // Messages checked against upstream's CLI.
  #[test]
  fn test_errors_match_click() {
    assert_eq!(
      p(&["--foo"]).unwrap_err().render(),
      "Usage: apprise [OPTIONS] SERVICE_URL [SERVICE_URL2 [SERVICE_URL3]]\nTry 'apprise --help' for help.\n\nError: No such option '--foo'."
    );
    assert!(p(&["--bod", "x"]).unwrap_err().render().ends_with("Error: No such option '--bod'. Did you mean '--body'?"));
    assert!(
      p(&["--storage"])
        .unwrap_err()
        .render()
        .ends_with("Error: No such option '--storage'. (Did you mean one of: '--storage-mode', '--storage-path', '--tag'?)")
    );
    assert!(
      p(&["--interpret"])
        .unwrap_err()
        .render()
        .ends_with("Error: No such option '--interpret'. (Did you mean one of: '--interpret-emojis', '--interpret-escapes'?)")
    );
    assert!(p(&["-x"]).unwrap_err().render().ends_with("Error: No such option '-x'."));
    assert!(p(&["-vvvvq"]).unwrap_err().render().ends_with("Error: No such option '-q'."));
    assert_eq!(p(&["-b"]).unwrap_err().render(), "Error: Option '-b' requires an argument.");
    assert_eq!(p(&["--body"]).unwrap_err().render(), "Error: Option '--body' requires an argument.");
    assert_eq!(p(&["--dry-run=yes"]).unwrap_err().render(), "Error: Option '--dry-run' does not take a value.");
    assert!(p(&["-R", "abc"]).unwrap_err().render().ends_with("Error: Invalid value for '--recursion-depth' / '-R': 'abc' is not a valid integer."));
    assert!(p(&["-L", "abc"]).unwrap_err().render().ends_with("Error: Invalid value for '--limit' / '-L': 'abc' is not a valid float."));
    assert!(p(&["-SL", "x"]).unwrap_err().render().ends_with("Error: Invalid value for '--service-limit' / '-SL': 'x' is not a valid float."));
  }

  #[test]
  fn test_help_is_eager() {
    assert!(p(&["-R", "abc", "--help"]).unwrap().flag("help"));
    assert!(p(&["-h"]).unwrap().flag("help"));
  }

  #[test]
  fn test_numbers() {
    assert_eq!(parse_int(" 1_000 "), Some(1000));
    assert_eq!(parse_int("-3"), Some(-3));
    assert_eq!(parse_int("1.5"), None);
    assert_eq!(parse_float("1e3"), Some(1000.0));
    assert_eq!(parse_float("inf"), Some(f64::INFINITY));
    assert_eq!(parse_float("abc"), None);
  }
}
