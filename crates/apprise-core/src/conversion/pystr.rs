//! Python string semantics the conversions depend on.
//!
//! Upstream counts, strips and classifies characters the way Python's `str`
//! and `html` modules do. Rust's definitions differ in small ways (for
//! example `char::is_whitespace` excludes U+001C..U+001F, which Python's
//! `str.isspace()` includes), and those differences change converted output.

use super::entities::{HTML5, INVALID_CHARREFS, INVALID_CODEPOINTS};
use super::unicode_tables::PUNCT_OR_SYMBOL;

/// Python `str.isspace()` for one character.
pub fn is_space(c: char) -> bool {
  c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str.strip()`.
pub fn strip(s: &str) -> &str {
  s.trim_matches(is_space)
}

/// Python `str.lstrip()`.
pub fn lstrip(s: &str) -> &str {
  s.trim_start_matches(is_space)
}

/// Python `str.rstrip()`.
pub fn rstrip(s: &str) -> &str {
  s.trim_end_matches(is_space)
}

/// Python `str.strip(chars)`.
pub fn strip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
  s.trim_matches(|c| chars.contains(c))
}

/// Python `str.lstrip(chars)`.
pub fn lstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
  s.trim_start_matches(|c| chars.contains(c))
}

/// Python `str.rstrip(chars)`.
pub fn rstrip_chars<'a>(s: &'a str, chars: &str) -> &'a str {
  s.trim_end_matches(|c| chars.contains(c))
}

/// Unicode general category `Zs` (space separator).
pub fn is_space_separator(c: char) -> bool {
  matches!(c, ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// Unicode general category `P*` or `S*` (punctuation or symbol).
pub fn is_punct_or_symbol(c: char) -> bool {
  let cp = c as u32;
  PUNCT_OR_SYMBOL
    .binary_search_by(|&(lo, hi)| {
      if hi < cp {
        std::cmp::Ordering::Less
      } else if lo > cp {
        std::cmp::Ordering::Greater
      } else {
        std::cmp::Ordering::Equal
      }
    })
    .is_ok()
}

/// Look up an HTML5 named character reference (`"amp;"`, `"amp"`, ...).
pub fn html5_entity(name: &str) -> Option<&'static str> {
  HTML5.binary_search_by(|(k, _)| (*k).cmp(name)).ok().map(|i| HTML5[i].1)
}

/// Python `html.unescape()`: decode every character reference in `s`.
pub fn unescape(s: &str) -> String {
  if !s.contains('&') {
    return s.to_string();
  }
  let mut out = String::with_capacity(s.len());
  let mut rest = s;
  while let Some(amp) = rest.find('&') {
    out.push_str(&rest[..amp]);
    let after = &rest[amp + 1..];
    match match_charref(after) {
      Some(len) => {
        decode_charref(&after[..len], &mut out);
        rest = &after[len..];
      }
      None => {
        out.push('&');
        rest = after;
      }
    }
  }
  out.push_str(rest);
  out
}

/// Length (in bytes) of the reference body after `&`, as matched by
/// `&(#[0-9]+;?|#[xX][0-9a-fA-F]+;?|[^\t\n\f <&#;]{1,32};?)`.
fn match_charref(s: &str) -> Option<usize> {
  let b = s.as_bytes();
  if b.first() == Some(&b'#') {
    let digits = b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
      let end = 1 + digits;
      return Some(if b.get(end) == Some(&b';') { end + 1 } else { end });
    }
    if matches!(b.get(1), Some(b'x' | b'X')) {
      let hex = b[2..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
      if hex > 0 {
        let end = 2 + hex;
        return Some(if b.get(end) == Some(&b';') { end + 1 } else { end });
      }
    }
    return None;
  }
  let mut end = 0;
  for (count, (i, c)) in s.char_indices().enumerate() {
    if count == 32 || matches!(c, '\t' | '\n' | '\x0c' | ' ' | '<' | '&' | '#' | ';') {
      break;
    }
    end = i + c.len_utf8();
  }
  if end == 0 {
    return None;
  }
  Some(if b.get(end) == Some(&b';') { end + 1 } else { end })
}

/// Python `html._replace_charref` for one matched reference body.
fn decode_charref(body: &str, out: &mut String) {
  if let Some(num) = body.strip_prefix('#') {
    let num = num.trim_end_matches(';');
    let value = match num.strip_prefix(['x', 'X']) {
      Some(hex) => parse_saturating(hex, 16),
      None => parse_saturating(num, 10),
    };
    if let Some(&(_, c)) = INVALID_CHARREFS.iter().find(|(k, _)| u64::from(*k) == value) {
      out.push(c);
      return;
    }
    if (0xD800..=0xDFFF).contains(&value) || value > 0x10FFFF {
      out.push('\u{fffd}');
      return;
    }
    let cp = value as u32;
    if INVALID_CODEPOINTS.iter().any(|&(lo, hi)| (lo..=hi).contains(&cp)) {
      return;
    }
    if let Some(c) = char::from_u32(cp) {
      out.push(c);
    }
    return;
  }
  if let Some(v) = html5_entity(body) {
    out.push_str(v);
    return;
  }
  // Find the longest matching name, at least two characters long.
  let bounds: Vec<usize> = body.char_indices().map(|(i, _)| i).skip(2).collect();
  for &x in bounds.iter().rev() {
    if let Some(v) = html5_entity(&body[..x]) {
      out.push_str(v);
      out.push_str(&body[x..]);
      return;
    }
  }
  out.push('&');
  out.push_str(body);
}

fn parse_saturating(digits: &str, radix: u32) -> u64 {
  let mut value: u64 = 0;
  for c in digits.chars() {
    let d = c.to_digit(radix).unwrap_or(0) as u64;
    value = value.saturating_mul(radix as u64).saturating_add(d);
  }
  value
}

/// Python `str.expandtabs(tabsize)`.
pub fn expandtabs(s: &str, tabsize: usize) -> String {
  let mut out = String::with_capacity(s.len());
  let mut col = 0usize;
  for c in s.chars() {
    match c {
      '\t' => {
        if tabsize > 0 {
          let pad = tabsize - (col % tabsize);
          out.extend(std::iter::repeat_n(' ', pad));
          col += pad;
        }
      }
      '\n' | '\r' => {
        out.push(c);
        col = 0;
      }
      _ => {
        out.push(c);
        col += 1;
      }
    }
  }
  out
}

/// Python `html.escape(s, quote=False)`.
pub fn escape_html(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for c in s.chars() {
    match c {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      _ => out.push(c),
    }
  }
  out
}

/// Python `html.escape(s, quote=True)`.
pub fn escape_html_quote(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for c in s.chars() {
    match c {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&#x27;"),
      _ => out.push(c),
    }
  }
  out
}

/// Collapse every run of Python whitespace into one space (`re.sub(r"\s+", " ", s)`).
pub fn collapse_whitespace(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  let mut in_ws = false;
  for c in s.chars() {
    if is_space(c) {
      if !in_ws {
        out.push(' ');
        in_ws = true;
      }
    } else {
      out.push(c);
      in_ws = false;
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_is_space_matches_python() {
    for c in ['\t', '\n', '\x0b', '\x0c', '\r', '\x1c', '\x1f', ' ', '\u{85}', '\u{a0}', '\u{2028}', '\u{3000}'] {
      assert!(is_space(c), "{:?}", c);
    }
    for c in ['a', '\u{200b}', '\u{feff}', '\0'] {
      assert!(!is_space(c), "{:?}", c);
    }
  }

  #[test]
  fn test_unescape_matches_python() {
    // Expected values come from Python's html.unescape().
    let cases = [
      ("&amp;", "&"),
      ("&amp", "&"),
      ("&ampx", "&x"),
      ("&notit;", "¬it;"),
      ("&notin;", "∉"),
      ("&#65;", "A"),
      ("&#x41", "A"),
      ("&#0;", "\u{fffd}"),
      ("&#128;", "€"),
      ("&#xD800;", "\u{fffd}"),
      ("&#1;", ""),
      ("&#99999999999999999999;", "\u{fffd}"),
      ("&#;", "&#;"),
      ("&bogus;", "&bogus;"),
      ("a & b", "a & b"),
      ("&CounterClockwiseContourIntegral;", "∳"),
    ];
    for (input, expected) in cases {
      assert_eq!(unescape(input), expected, "{}", input);
    }
  }

  #[test]
  fn test_punct_or_symbol() {
    assert!(is_punct_or_symbol('!'));
    assert!(is_punct_or_symbol('€'));
    assert!(is_punct_or_symbol('“'));
    assert!(!is_punct_or_symbol('a'));
    assert!(!is_punct_or_symbol(' '));
  }

  #[test]
  fn test_expandtabs() {
    assert_eq!(expandtabs("Q:\tQuestion", 4), "Q:  Question");
    assert_eq!(expandtabs("\tx\n\ty", 4), "    x\n    y");
  }
}
