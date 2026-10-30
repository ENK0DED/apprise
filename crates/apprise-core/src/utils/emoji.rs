//! `:emoji:` shortcode interpretation (upstream `apprise/emojis.py`).

use std::collections::HashMap;

#[path = "emoji_table.rs"]
mod table;

// Longest name in the table; longer text between colons cannot match.
const MAX_NAME_LEN: usize = 64;

fn emoji_map() -> &'static HashMap<&'static str, &'static str> {
  static MAP: std::sync::OnceLock<HashMap<&str, &str>> = std::sync::OnceLock::new();
  MAP.get_or_init(|| {
    let mut m = HashMap::with_capacity(table::EMOJIS.len());
    for &(name, emoji) in table::EMOJIS {
      // Upstream tries its keys in order, so the first one wins.
      m.entry(name).or_insert(emoji);
    }
    m
  })
}

/// Replace `:emoji_name:` shortcodes with their Unicode emoji. Names match
/// case-insensitively, and every alias upstream accepts is recognised (for
/// example `:+1:` and `:thumbsup:`). Unknown shortcodes are left as written.
pub fn interpret_emojis(text: &str) -> String {
  if !text.contains(':') {
    return text.to_string();
  }
  let map = emoji_map();
  let mut result = String::with_capacity(text.len());
  let mut rest = text;
  while let Some(open) = rest.find(':') {
    result.push_str(&rest[..open]);
    let after = &rest[open + 1..];
    if let Some(close) = after.find(':') {
      let name = &after[..close];
      if !name.is_empty() && name.len() <= MAX_NAME_LEN {
        let lower = name.to_lowercase();
        if let Some(emoji) = map.get(lower.as_str()) {
          result.push_str(emoji);
          rest = &after[close + 1..];
          continue;
        }
      }
    }
    result.push(':');
    rest = after;
  }
  result.push_str(rest);
  result
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_known_emojis() {
    assert_eq!(interpret_emojis(":smile:"), "😄");
    assert_eq!(interpret_emojis(":heart:"), "❤️");
    assert_eq!(interpret_emojis(":zap:"), "⚡");
    assert_eq!(interpret_emojis(":100:"), "💯");
    assert_eq!(interpret_emojis(":alarm_clock:"), "⏰");
  }

  #[test]
  fn test_regex_keys_are_expanded() {
    // Upstream keys such as "(\+1|thumbsup)" and "(lady_)?beetle" are
    // patterns; every name they match must work (upstream 755f1747).
    assert_eq!(interpret_emojis(":+1: :thumbsup: :-1:"), "👍 👍 👎");
    assert_eq!(interpret_emojis(":beetle: :lady_beetle:"), "🐞 🐞");
    assert_eq!(interpret_emojis(":email: :e-mail: :shirt: :tshirt:"), "📧 📧 👕 👕");
    assert_eq!(interpret_emojis(":fist_raised: :punch: :facepunch:"), "👊 👊 👊");
  }

  #[test]
  fn test_case_insensitive() {
    assert_eq!(interpret_emojis(":SMILE: :Smile:"), "😄 😄");
  }

  #[test]
  fn test_unknown_and_partial() {
    assert_eq!(interpret_emojis(":nonexistent_emoji:"), ":nonexistent_emoji:");
    assert_eq!(interpret_emojis("time is 3:00"), "time is 3:00");
    assert_eq!(interpret_emojis(":foo:smile:"), ":foo😄");
    assert_eq!(interpret_emojis(":smile::zap:"), "😄⚡");
    assert_eq!(interpret_emojis(":smile:smile:"), "😄smile:");
    assert_eq!(interpret_emojis(""), "");
    assert_eq!(interpret_emojis("Hello :smile: world :zap:!"), "Hello 😄 world ⚡!");
  }
}
