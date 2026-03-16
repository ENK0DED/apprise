use std::env;

/// Detect the user's language from environment variables.
///
/// Checks LC_ALL, LC_CTYPE, LANG, LANGUAGE in order (matching Python's
/// `AppriseLocale.detect_language()`). Returns a 2-character language code
/// (e.g., "en", "fr", "de") or falls back to "en".
pub fn detect_language() -> String {
  for var in &["LC_ALL", "LC_CTYPE", "LANG", "LANGUAGE"] {
    if let Ok(val) = env::var(var) {
      if let Some(lang) = parse_locale_lang(&val) {
        return lang;
      }
    }
  }
  "en".to_string()
}

/// Parse a locale string like "en_US.UTF-8" or "fr_FR" into a 2-char lang code.
fn parse_locale_lang(locale: &str) -> Option<String> {
  let s = locale.trim();
  if s.is_empty() || s == "C" || s == "POSIX" {
    return None;
  }
  // Extract first 2 alphabetic characters
  let chars: Vec<char> = s.chars().collect();
  if chars.len() >= 2 && chars[0].is_ascii_alphabetic() && chars[1].is_ascii_alphabetic() {
    Some(format!("{}{}", chars[0].to_ascii_lowercase(), chars[1].to_ascii_lowercase()))
  } else {
    None
  }
}

/// Format a notification title/body with locale-aware string substitution.
///
/// This is a no-op identity function matching Python's `_()` / `gettext()`
/// when no translation files are available (which is the default for apprise).
/// If gettext `.mo` files were to be added, this would perform the lookup.
pub fn gettext(s: &str) -> String {
  s.to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_locale_lang_full() {
    assert_eq!(parse_locale_lang("en_US.UTF-8"), Some("en".to_string()));
    assert_eq!(parse_locale_lang("fr_FR"), Some("fr".to_string()));
    assert_eq!(parse_locale_lang("de"), Some("de".to_string()));
  }

  #[test]
  fn test_parse_locale_lang_c() {
    assert_eq!(parse_locale_lang("C"), None);
    assert_eq!(parse_locale_lang("POSIX"), None);
  }

  #[test]
  fn test_parse_locale_lang_empty() {
    assert_eq!(parse_locale_lang(""), None);
  }

  #[test]
  fn test_detect_language_fallback() {
    // Should return "en" when no locale vars are set (or whatever the system has)
    let lang = detect_language();
    assert_eq!(lang.len(), 2);
    assert!(lang.chars().all(|c| c.is_ascii_lowercase()));
  }
}
