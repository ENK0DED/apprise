//! Backslash escape interpretation (upstream `decode_escapes` in
//! `apprise/apprise.py`).
//!
//! Only well-formed escapes change: `\\`, `\'`, `\"`, `\a`, `\b`, `\f`,
//! `\n`, `\r`, `\t`, `\v`, a backslash before a line break, octal `\ooo`,
//! `\xhh`, `\uhhhh` and `\Uhhhhhhhh`. Anything else after a backslash is
//! kept as written, so a Windows path such as `C:\Users` does not change.
//!
//! Upstream also decodes `\N{NAME}` from Unicode's character names; the
//! port keeps those as written rather than carrying the name database.

fn hex_value(s: &[char]) -> Option<u32> {
  s.iter().try_fold(0u32, |acc, c| c.to_digit(16).map(|d| acc * 16 + d))
}

/// Interpret backslash escape sequences in `text`.
pub fn interpret_escapes(text: &str) -> String {
  let chars: Vec<char> = text.chars().collect();
  let n = chars.len();
  let mut out = String::with_capacity(text.len());
  let mut i = 0;
  while i < n {
    if chars[i] != '\\' || i + 1 >= n {
      out.push(chars[i]);
      i += 1;
      continue;
    }
    let next = chars[i + 1];
    let simple = match next {
      '\\' => Some(Some('\\')),
      '\'' => Some(Some('\'')),
      '"' => Some(Some('"')),
      'a' => Some(Some('\x07')),
      'b' => Some(Some('\x08')),
      'f' => Some(Some('\x0c')),
      'n' => Some(Some('\n')),
      'r' => Some(Some('\r')),
      't' => Some(Some('\t')),
      'v' => Some(Some('\x0b')),
      // A backslash before a line break joins the lines.
      '\n' => Some(None),
      _ => None,
    };
    if let Some(c) = simple {
      if let Some(c) = c {
        out.push(c);
      }
      i += 2;
      continue;
    }
    if ('0'..='7').contains(&next) {
      let digits = chars[i + 1..].iter().take(3).take_while(|c| ('0'..='7').contains(*c)).count();
      let value = chars[i + 1..i + 1 + digits].iter().fold(0u32, |acc, c| acc * 8 + c.to_digit(8).unwrap_or(0));
      if let Some(c) = char::from_u32(value) {
        out.push(c);
      }
      i += 1 + digits;
      continue;
    }
    let width = match next {
      'x' => 2,
      'u' => 4,
      'U' => 8,
      _ => 0,
    };
    if width > 0
      && i + 2 + width <= n
      && let Some(value) = hex_value(&chars[i + 2..i + 2 + width])
      && let Some(c) = char::from_u32(value)
    {
      out.push(c);
      i += 2 + width;
      continue;
    }
    // Not a valid escape: keep the backslash and move on.
    out.push('\\');
    i += 1;
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_escapes_match_upstream() {
    // Expected values come from upstream's decode_escapes().
    let cases = [
      (r"a\nb", "a\nb"),
      (r"hello\tworld", "hello\tworld"),
      (r"C:\Users", r"C:\Users"),
      (r"\x41\u00e9\U0001F427", "Aé🐧"),
      (r"\N{NOPE}", r"\N{NOPE}"),
      (r"\101\7\0", "A\x07\0"),
      (r#"\'\\\""#, "'\\\""),
      (r"\q", r"\q"),
      ("a\\\nb", "ab"),
      (r"\U00110000", r"\U00110000"),
      (r"\x4", r"\x4"),
      (r"\8", r"\8"),
      (r"\a\b\f\v\r", "\x07\x08\x0c\x0b\r"),
      ("trailing\\", "trailing\\"),
      (r"\\n", "\\n"),
      ("", ""),
    ];
    for (input, expected) in cases {
      assert_eq!(interpret_escapes(input), expected, "{}", input);
    }
  }

  #[test]
  fn test_invalid_sequence_kept_as_written() {
    // Upstream ae118a4c: an invalid escape no longer fails the message.
    assert_eq!(interpret_escapes(r"path C:\temp\new\xyz"), "path C:\temp\new\\xyz");
  }
}
