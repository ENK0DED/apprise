//! Phone number helpers matching upstream `apprise.utils.parse`
//! (`is_phone_no`, `parse_phone_no`).

/// A phone number broken apart as upstream `is_phone_no()` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneNo {
  /// The country code (whatever is left before the area code).
  pub country: String,
  /// The area code (the 3 digits before the line).
  pub area: String,
  /// The line (last 7 digits).
  pub line: String,
  /// Every digit, in order.
  pub full: String,
  /// A consistently formatted number, e.g. `+1 800-123-4567`.
  pub pretty: String,
}

/// Upstream `is_phone_no(phone, min_len)`: `None` unless `phone` only holds
/// digits, whitespace, `+`, `-`, `(` and `)` and has `min_len..=14` digits.
pub fn is_phone_no(phone: &str, min_len: usize) -> Option<PhoneNo> {
  // ^\+?[0-9\s)(+-]+\s*$
  if phone.is_empty() || !phone.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '+' | '-' | '(' | ')')) {
    return None;
  }

  let full: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
  if full.len() > 14 || full.len() < min_len {
    return None;
  }

  let split = full.len().saturating_sub(7);
  let line = full[split..].to_string();
  let rest = &full[..split];
  let area_split = rest.len().saturating_sub(3);
  let area = rest[area_split..].to_string();
  let country = rest[..area_split].to_string();

  let mut pretty = String::new();
  if !country.is_empty() {
    pretty.push_str(&format!("+{} ", country));
  }
  if !area.is_empty() {
    pretty.push_str(&format!("{}-", area));
  }
  if line.len() >= 7 {
    pretty.push_str(&format!("{}-{}", &line[..3], &line[3..]));
  } else {
    pretty.push_str(&line);
  }

  Some(PhoneNo { country, area, line, full, pretty })
}

/// Upstream `parse_phone_no(*args)`: pull every phone-number-looking entry
/// out of each string. A string with none is split on `[];,` and whitespace
/// instead, so the caller can still report the bad entries.
pub fn parse_phone_no<S: AsRef<str>>(args: &[S]) -> Vec<String> {
  parse_phone_no_impl(args, false)
}

/// Upstream `parse_phone_no(*args, prefix=True)`: like [`parse_phone_no`] but
/// each number may carry a `name:` prefix (e.g. `wa:15551234567`).
pub fn parse_phone_no_with_prefix<S: AsRef<str>>(args: &[S]) -> Vec<String> {
  parse_phone_no_impl(args, true)
}

fn parse_phone_no_impl<S: AsRef<str>>(args: &[S], prefix: bool) -> Vec<String> {
  let mut result = Vec::new();
  for arg in args {
    let arg = arg.as_ref();
    if arg.is_empty() {
      continue;
    }
    let found = find_phone_numbers(arg, prefix);
    if found.is_empty() {
      result.extend(arg.split(|c: char| matches!(c, '[' | ']' | ';' | ',') || c.is_whitespace()).filter(|s| !s.is_empty()).map(str::to_string));
    } else {
      result.extend(found);
    }
  }
  result
}

/// `findall` over upstream's `PHONE_NO_DETECTION_RE`:
///   `((?:[+(][+(\s]{0,32})?[0-9][0-9()\s-]{1,32}[0-9])(?=$|[\s,+(]+[0-9])`
/// or, with `prefix`, `PHONE_NO_WPREFIX_DETECTION_RE`, which also allows an
/// optional `[a-z]{1,32}:` before the number and before the next one.
fn find_phone_numbers(s: &str, prefix: bool) -> Vec<String> {
  let chars: Vec<char> = s.chars().collect();
  let mut out = Vec::new();
  let mut i = 0;
  while i < chars.len() {
    match match_number_at(&chars, i, prefix) {
      Some(end) => {
        out.push(chars[i..end].iter().collect());
        i = if end > i { end } else { i + 1 };
      }
      None => i += 1,
    }
  }
  out
}

fn is_lead(c: char) -> bool {
  matches!(c, '+' | '(')
}

fn is_lead_tail(c: char) -> bool {
  matches!(c, '+' | '(') || c.is_whitespace()
}

fn is_middle(c: char) -> bool {
  c.is_ascii_digit() || matches!(c, '(' | ')' | '-') || c.is_whitespace()
}

fn is_separator(c: char) -> bool {
  matches!(c, ',' | '+' | '(') || c.is_whitespace()
}

/// Every way `[a-z]{1,32}:` can match at `at`, as end positions, in the
/// order a backtracking engine would try them (there is at most one).
fn label_end(chars: &[char], at: usize) -> Option<usize> {
  let run = chars[at..].iter().take(33).take_while(|c| c.is_ascii_alphabetic()).count();
  if (1..=32).contains(&run) && chars.get(at + run) == Some(&':') { Some(at + run + 1) } else { None }
}

/// Try to match one number starting exactly at `start`; returns its end.
fn match_number_at(chars: &[char], start: usize, prefix: bool) -> Option<usize> {
  // Optional `name:` label (prefix mode only), tried before going without.
  let mut label_starts = Vec::new();
  if prefix && let Some(end) = label_end(chars, start) {
    label_starts.push(end);
  }
  label_starts.push(start);

  for &pos in &label_starts {
    // Optional `[+(][+(\s]{0,32}` lead; greedy, so longest first, then none.
    let mut leads = Vec::new();
    if chars.get(pos).copied().is_some_and(is_lead) {
      let tail = chars[pos + 1..].iter().take(32).take_while(|c| is_lead_tail(**c)).count();
      leads.extend((0..=tail).rev().map(|t| pos + 1 + t));
    }
    leads.push(pos);

    for &digit_at in &leads {
      if !chars.get(digit_at).is_some_and(|c| c.is_ascii_digit()) {
        continue;
      }
      let run = chars[digit_at + 1..].iter().take(32).take_while(|c| is_middle(**c)).count();
      for k in (1..=run).rev() {
        let last = digit_at + 1 + k;
        if !chars.get(last).is_some_and(|c| c.is_ascii_digit()) {
          continue;
        }
        let end = last + 1;
        if lookahead_ok(chars, end, prefix) {
          return Some(end);
        }
      }
    }
  }
  None
}

/// `(?=$|[\s,+(]+[0-9])`, or with `prefix` `(?=$|(?:[a-z]{1,32}:)?[\s,+(]+[0-9])`.
fn lookahead_ok(chars: &[char], at: usize, prefix: bool) -> bool {
  if at == chars.len() {
    return true;
  }
  let separated = |from: usize| {
    let run = chars[from..].iter().take_while(|c| is_separator(**c)).count();
    run > 0 && chars.get(from + run).is_some_and(|c| c.is_ascii_digit())
  };
  if prefix
    && let Some(end) = label_end(chars, at)
    && separated(end)
  {
    return true;
  }
  separated(at)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_is_phone_no() {
    let p = is_phone_no("+1 (800) 123-4567", 10).unwrap();
    assert_eq!(p.full, "18001234567");
    assert_eq!(p.country, "1");
    assert_eq!(p.area, "800");
    assert_eq!(p.line, "1234567");
    assert_eq!(p.pretty, "+1 800-123-4567");

    let p = is_phone_no("411", 3).unwrap();
    assert_eq!(p.full, "411");
    assert_eq!(p.pretty, "411");
    assert_eq!(p.country, "");
    assert_eq!(p.area, "");

    assert!(is_phone_no("411", 10).is_none());
    assert!(is_phone_no("abcd", 3).is_none());
    assert!(is_phone_no("+1234567890abc", 10).is_none());
    assert!(is_phone_no("123456789012345", 10).is_none());
    assert!(is_phone_no("", 0).is_none());
    assert!(is_phone_no("12345678901234", 10).is_some());
  }

  #[test]
  fn test_parse_phone_no() {
    assert_eq!(parse_phone_no(&["+1(555) 123-1234"]), vec!["+1(555) 123-1234"]);
    assert_eq!(parse_phone_no(&["1555 5555555"]), vec!["1555 5555555"]);
    assert_eq!(parse_phone_no(&["12"]), vec!["12"]);
    assert_eq!(parse_phone_no(&["abcd"]), vec!["abcd"]);
    assert_eq!(parse_phone_no(&["11111111111,22222222222"]), vec!["11111111111", "22222222222"]);
    assert_eq!(parse_phone_no(&["+15551234567, +15557654321"]), vec!["+15551234567", "+15557654321"]);
    assert_eq!(parse_phone_no(&["a, b;c"]), vec!["a", "b", "c"]);
    // Numbers embedded in other text are still picked out
    assert_eq!(parse_phone_no(&["abc 12345678901"]), vec!["12345678901"]);
    assert!(parse_phone_no::<&str>(&[]).is_empty());
    assert!(parse_phone_no(&[""]).is_empty());
  }

  #[test]
  fn test_parse_phone_no_with_prefix() {
    assert_eq!(parse_phone_no_with_prefix(&["wa:15551234567"]), vec!["wa:15551234567"]);
    // Upstream's look-ahead does not allow a prefixed number after a plain one
    assert_eq!(parse_phone_no_with_prefix(&["15551234567,wa:15557654321"]), vec!["wa:15557654321"]);
    assert_eq!(parse_phone_no_with_prefix(&["wa:15551234567,15557654321"]), vec!["wa:15551234567", "15557654321"]);
    // Without prefix support the label is not part of the number
    assert_eq!(parse_phone_no(&["wa:15551234567"]), vec!["15551234567"]);
  }
}
