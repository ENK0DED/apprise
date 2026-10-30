//! Target list parsing shared by the SMS plugins, mirroring upstream
//! `apprise.utils.parse` (`parse_list`, `parse_phone_no`, `is_phone_no`), so
//! they split and validate targets the same way the Python library does.
//!
//! URL path entries need no extra splitting: upstream quotes the path before
//! `split_path()`, so only `/` separates them, exactly like
//! `ParsedUrl::path_parts`.

/// A phone number broken apart the way upstream `is_phone_no()` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PhoneNo {
  /// Leftover leading digits (empty for numbers of 10 digits or fewer).
  pub(super) country: String,
  /// The 3 digits before the line number (may be empty).
  pub(super) area: String,
  /// The last 7 digits.
  pub(super) line: String,
  /// Every digit of the number, without markup.
  pub(super) full: String,
}

fn is_list_delim(c: char, allow_whitespace: bool) -> bool {
  matches!(c, '[' | ']' | ';' | ',') || (allow_whitespace && c.is_whitespace())
}

/// Upstream `parse_list()`: split every entry on `[\[\];,\s]+` (whitespace
/// only when `allow_whitespace`) and return the unique entries sorted.
pub(super) fn parse_list<I, S>(entries: I, allow_whitespace: bool) -> Vec<String>
where
  I: IntoIterator<Item = S>,
  S: AsRef<str>,
{
  let mut result: Vec<String> = entries
    .into_iter()
    .flat_map(|e| {
      e.as_ref().split(|c| is_list_delim(c, allow_whitespace)).map(|s| if allow_whitespace { s.to_string() } else { s.trim().to_string() }).collect::<Vec<_>>()
    })
    .filter(|s| !s.is_empty())
    .collect();
  result.sort();
  result.dedup();
  result
}

/// Upstream `is_phone_no()`: the entry may only hold digits, whitespace,
/// brackets, `+` and `-`, and must carry between `min_len` and 14 digits.
pub(super) fn is_phone_no(phone: &str, min_len: usize) -> Option<PhoneNo> {
  if phone.is_empty() || !phone.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '(' | ')' | '+' | '-')) {
    return None;
  }
  let full: String = phone.chars().filter(char::is_ascii_digit).collect();
  if full.len() > 14 || full.len() < min_len {
    return None;
  }
  let split = full.len().saturating_sub(7);
  let (rest, line) = full.split_at(split);
  let (country, area) = rest.split_at(rest.len().saturating_sub(3));
  Some(PhoneNo { country: country.to_string(), area: area.to_string(), line: line.to_string(), full: full.clone() })
}

/// Upstream `parse_phone_no()`: pull every phone-number-looking entry out of
/// each string. Content that holds no phone number at all is kept (split on
/// the list delimiters) so the caller can report it. With `prefix`, an
/// optional `label:` may precede each number (Twilio's `w:` WhatsApp prefix).
pub(super) fn parse_phone_no<I, S>(entries: I, prefix: bool) -> Vec<String>
where
  I: IntoIterator<Item = S>,
  S: AsRef<str>,
{
  let mut result = Vec::new();
  for entry in entries {
    let entry = entry.as_ref();
    if entry.is_empty() {
      continue;
    }
    let found = find_phone_numbers(entry, prefix);
    if found.is_empty() {
      result.extend(entry.split(|c| is_list_delim(c, true)).filter(|s| !s.is_empty()).map(str::to_string));
    } else {
      result.extend(found);
    }
  }
  result
}

/// `findall()` of upstream `PHONE_NO_DETECTION_RE` (or
/// `PHONE_NO_WPREFIX_DETECTION_RE` with `prefix`):
/// `((?:[a-z]{1,32}:)?(?:[+(][+(\s]{0,32})?[0-9][0-9()\s-]{1,32}[0-9])`
/// followed by `(?=$|(?:[a-z]{1,32}:)?[\s,+(]+[0-9])`.
fn find_phone_numbers(s: &str, prefix: bool) -> Vec<String> {
  let chars: Vec<char> = s.chars().collect();
  let mut found = Vec::new();
  let mut pos = 0;
  while pos < chars.len() {
    if let Some(end) = match_phone_at(&chars, pos, prefix) {
      found.push(chars[pos..end].iter().collect());
      pos = end;
    } else {
      pos += 1;
    }
  }
  found
}

/// Count how many leading chars of `chars[from..]` satisfy `f`, up to `max`.
fn run_len(chars: &[char], from: usize, max: usize, f: impl Fn(char) -> bool) -> usize {
  chars.get(from..).map(|rest| rest.iter().take(max).take_while(|c| f(**c)).count()).unwrap_or(0)
}

/// Where a `[a-z]{1,32}:` label starting at `at` ends, if there is one.
fn label_end(chars: &[char], at: usize) -> Option<usize> {
  let n = run_len(chars, at, 32, |c| c.is_ascii_alphabetic());
  (n > 0 && chars.get(at + n) == Some(&':')).then_some(at + n + 1)
}

fn is_digit_at(chars: &[char], at: usize) -> bool {
  chars.get(at).is_some_and(char::is_ascii_digit)
}

fn match_phone_at(chars: &[char], start: usize, prefix: bool) -> Option<usize> {
  // Greedy optional groups: try the longer alternative first
  let mut starts = Vec::with_capacity(2);
  if prefix && let Some(end) = label_end(chars, start) {
    starts.push(end);
  }
  starts.push(start);

  for p0 in starts {
    let mut digit_starts = Vec::new();
    if matches!(chars.get(p0), Some('+' | '(')) {
      let run = run_len(chars, p0 + 1, 32, |c| matches!(c, '+' | '(') || c.is_whitespace());
      digit_starts.extend((0..=run).rev().map(|n| p0 + 1 + n));
    }
    digit_starts.push(p0);

    for p in digit_starts {
      if !is_digit_at(chars, p) {
        continue;
      }
      let run = run_len(chars, p + 1, 32, |c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '(' | ')' | '-'));
      for k in (1..=run).rev() {
        let end = p + 1 + k;
        if is_digit_at(chars, end) && phone_lookahead(chars, end + 1, prefix) {
          return Some(end + 1);
        }
      }
    }
  }
  None
}

fn phone_lookahead(chars: &[char], at: usize, prefix: bool) -> bool {
  // Python's `$` also matches just before a final newline
  if at >= chars.len() || (at + 1 == chars.len() && chars[at] == '\n') {
    return true;
  }
  let separated = |from: usize| {
    let n = run_len(chars, from, usize::MAX, |c| matches!(c, ',' | '+' | '(') || c.is_whitespace());
    n > 0 && is_digit_at(chars, from + n)
  };
  (prefix && label_end(chars, at).is_some_and(separated)) || separated(at)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_list() {
    assert_eq!(parse_list(["b, a", "c;a [d]"], true), vec!["a", "b", "c", "d"]);
    assert_eq!(parse_list(["b c, a"], false), vec!["a", "b c"]);
    assert!(parse_list(["", ",;"], true).is_empty());
  }

  #[test]
  fn test_is_phone_no() {
    let p = is_phone_no("+1 (800) 123-4567", 10).unwrap();
    assert_eq!(p.full, "18001234567");
    assert_eq!(p.country, "1");
    assert_eq!(p.area, "800");
    assert_eq!(p.line, "1234567");
    assert!(is_phone_no("123", 10).is_none());
    assert!(is_phone_no("123", 3).is_some());
    assert!(is_phone_no("123456789012345", 10).is_none());
    assert!(is_phone_no("abc1234567890", 10).is_none());
    assert!(is_phone_no("", 0).is_none());
    let short = is_phone_no("4567", 3).unwrap();
    assert_eq!((short.country.as_str(), short.area.as_str(), short.line.as_str()), ("", "", "4567"));
  }

  #[test]
  fn test_parse_phone_no() {
    assert_eq!(parse_phone_no(["15551232000"], false), vec!["15551232000"]);
    assert_eq!(parse_phone_no(["15551232000,15551232001 +1 555 123 2002"], false), vec!["15551232000", "15551232001", "+1 555 123 2002"]);
    assert_eq!(parse_phone_no(["(555) 123-4567"], false), vec!["(555) 123-4567"]);
    // Unparseable content is kept so it can be reported later
    assert_eq!(parse_phone_no(["abcd, efgh"], false), vec!["abcd", "efgh"]);
    // A number glued to text is found inside it
    assert_eq!(parse_phone_no(["id123456"], false), vec!["123456"]);
    // Prefixed entries
    assert_eq!(parse_phone_no(["w:15551232000,15551232001"], true), vec!["w:15551232000", "15551232001"]);
    assert_eq!(parse_phone_no(["w:15551232000"], false), vec!["15551232000"]);
  }

  #[test]
  fn test_parse_phone_no_matches_python() {
    // Results captured from upstream parse_phone_no()
    assert_eq!(parse_phone_no(["+1-555-123-4567 / 12"], false), vec!["+1-555-123-4567", "/", "12"]);
    assert_eq!(parse_phone_no(["1 2 3"], false), vec!["1 2 3"]);
    assert_eq!(parse_phone_no(["((+1) 555 123-4567)"], false), vec!["((+1) 555"]);
    assert_eq!(parse_phone_no(["+15551232000 w:+15551232001, sms:12345678901"], true), vec!["sms:12345678901"]);
    assert_eq!(parse_phone_no(["12"], false), vec!["12"]);
    assert_eq!(parse_phone_no(["1"], false), vec!["1"]);
    assert_eq!(parse_phone_no(["15551232000\n"], false), vec!["15551232000"]);
    assert_eq!(parse_phone_no(["12345678901,+12345678902,#g1"], false), vec!["12345678901"]);
    assert_eq!(parse_phone_no(["12345678901,#g1,#g2"], false), vec!["12345678901", "#g1", "#g2"]);
  }
}
