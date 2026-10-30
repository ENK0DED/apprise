//! Phone number helpers matching upstream `apprise.utils.parse`.
//!
//! [`is_phone_no`] mirrors upstream `is_phone_no()` and [`parse_phone_no`]
//! mirrors `parse_phone_no()` (without prefixes), so SMS plugins accept and
//! drop exactly the numbers upstream does.

/// Validate a phone number and return its digits (upstream `is_phone_no()`'s
/// `full` value).
///
/// The entry may only hold digits, whitespace, `(`, `)`, `+` and `-`. Once
/// stripped down to its digits it must be between `min_len` and 14 digits
/// long. Upstream SMS plugins use a `min_len` of 10 unless they say otherwise.
pub fn is_phone_no(phone: &str, min_len: usize) -> Option<String> {
  // Upstream IS_PHONE_NO: ^\+?[0-9\s)(+-]+\s*$
  let rest = phone.strip_prefix('+').unwrap_or(phone);
  if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '(' | ')' | '+' | '-')) {
    return None;
  }
  let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
  if digits.len() > 14 || digits.len() < min_len {
    return None;
  }
  Some(digits)
}

/// Split strings holding one or more phone numbers into a list (upstream
/// `parse_phone_no()`).
///
/// Numbers may be separated by commas or whitespace. An entry with nothing
/// resembling a phone number is kept, split on the usual delimiters, so the
/// caller can report it as invalid.
pub fn parse_phone_no<S: AsRef<str>>(entries: &[S]) -> Vec<String> {
  let mut result = Vec::new();
  for entry in entries {
    let entry = entry.as_ref();
    if entry.is_empty() {
      continue;
    }
    let found = find_phone_numbers(entry);
    if found.is_empty() {
      // Upstream STRING_DELIMITERS: [\[\]\;,\s]+
      result.extend(entry.split(|c: char| c.is_whitespace() || matches!(c, '[' | ']' | ';' | ',')).filter(|s| !s.is_empty()).map(str::to_string));
    } else {
      result.extend(found);
    }
  }
  result
}

/// Every match of upstream `PHONE_NO_DETECTION_RE`:
/// `((?:[+(][+(\s]{0,32})?[0-9][0-9()\s-]{1,32}[0-9])(?=$|[\s,+(]+[0-9])`
fn find_phone_numbers(text: &str) -> Vec<String> {
  let chars: Vec<char> = text.chars().collect();
  let mut found = Vec::new();
  let mut start = 0;
  while start < chars.len() {
    match match_phone_at(&chars, start) {
      Some(end) => {
        found.push(chars[start..end].iter().collect());
        start = end;
      }
      None => start += 1,
    }
  }
  found
}

/// Try `PHONE_NO_DETECTION_RE` at `start`; return the end of the match.
fn match_phone_at(chars: &[char], start: usize) -> Option<usize> {
  let mut pos = start;

  // Optional prefix: [+(][+(\s]{0,32}. The prefix characters never overlap
  // with the digit that must follow, so taking as many as allowed is the
  // only way the rest can match.
  if matches!(chars.get(pos), Some('+' | '(')) {
    pos += 1;
    let mut taken = 0;
    while taken < 32 && chars.get(pos).is_some_and(|c| matches!(c, '+' | '(') || c.is_whitespace()) {
      pos += 1;
      taken += 1;
    }
  }

  // The first digit
  if !chars.get(pos).is_some_and(char::is_ascii_digit) {
    return None;
  }
  pos += 1;

  // [0-9()\s-]{1,32}[0-9] followed by the lookahead, longest first as the
  // greedy regex backtracks.
  let middle = |c: &char| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '(' | ')' | '-');
  let run = chars[pos..].iter().take(33).take_while(|c| middle(c)).count();
  for len in (1..run.min(33)).rev() {
    let last = pos + len;
    if chars[last].is_ascii_digit() && lookahead(chars, last + 1) {
      return Some(last + 1);
    }
  }
  None
}

/// `(?=$|[\s,+(]+[0-9])`; Python's `$` also matches before a final newline.
fn lookahead(chars: &[char], pos: usize) -> bool {
  if pos == chars.len() || (pos + 1 == chars.len() && chars[pos] == '\n') {
    return true;
  }
  let sep = chars[pos..].iter().take_while(|c| c.is_whitespace() || matches!(c, ',' | '+' | '(')).count();
  sep > 0 && chars.get(pos + sep).is_some_and(char::is_ascii_digit)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_is_phone_no() {
    assert_eq!(is_phone_no("+1 (555) 123-4567", 10).as_deref(), Some("15551234567"));
    assert_eq!(is_phone_no("15551234567", 10).as_deref(), Some("15551234567"));
    assert_eq!(is_phone_no("15551234567  ", 10).as_deref(), Some("15551234567"));
    // Too short, too long, or not a phone number at all
    assert_eq!(is_phone_no("123", 10), None);
    assert_eq!(is_phone_no("123456789", 10), None);
    assert_eq!(is_phone_no("123456789", 9).as_deref(), Some("123456789"));
    assert_eq!(is_phone_no("123456789012345", 10), None);
    assert_eq!(is_phone_no("abcd", 10), None);
    assert_eq!(is_phone_no("555-abc-1234", 10), None);
    assert_eq!(is_phone_no("+", 1), None);
    assert_eq!(is_phone_no("", 1), None);
  }

  #[test]
  fn test_parse_phone_no() {
    assert_eq!(parse_phone_no(&["15551234567"]), vec!["15551234567"]);
    assert_eq!(parse_phone_no(&["15551234567,15551234568"]), vec!["15551234567", "15551234568"]);
    assert_eq!(parse_phone_no(&["+1 555 123 4567, +1 555 123 4568"]), vec!["+1 555 123 4567", "+1 555 123 4568"]);
    assert_eq!(parse_phone_no(&["+61 400 000 000"]), vec!["+61 400 000 000"]);
    assert_eq!(parse_phone_no(&["(555) 123-4567"]), vec!["(555) 123-4567"]);
    // Unparseable content is kept so it can be reported
    assert_eq!(parse_phone_no(&["abcd"]), vec!["abcd"]);
    assert_eq!(parse_phone_no(&["abc, def"]), vec!["abc", "def"]);
    assert_eq!(parse_phone_no(&["12", "abcd", "123"]), vec!["12", "abcd", "123"]);
    assert!(parse_phone_no(&[""]).is_empty());
  }

  #[test]
  fn test_parse_phone_no_matches_upstream() {
    // Results captured from upstream parse_phone_no()
    assert_eq!(parse_phone_no(&["1"]), vec!["1"]);
    // A number must be followed by the end or a separator and another number
    assert_eq!(parse_phone_no(&["5551234567 abc 5551234568"]), vec!["5551234568"]);
    assert_eq!(parse_phone_no(&["555-123-4567+15551234568"]), vec!["555-123-4567", "+15551234568"]);
    assert_eq!(parse_phone_no(&["1-2"]), vec!["1-2"]);
    assert_eq!(parse_phone_no(&["12a34"]), vec!["12a34"]);
    assert_eq!(parse_phone_no(&["12,abc,34"]), vec!["12", "abc", "34"]);
    assert_eq!(parse_phone_no(&["++ ++1234"]), vec!["++ ++1234"]);
    assert_eq!(parse_phone_no(&["+(1) 555"]), vec!["+(1) 555"]);
    // At most 34 digits in a row; the match slides to the end
    assert_eq!(parse_phone_no(&["5".repeat(40)]), vec!["5".repeat(34)]);
    assert_eq!(parse_phone_no(&["5".repeat(33)]), vec!["5".repeat(33)]);
  }
}
