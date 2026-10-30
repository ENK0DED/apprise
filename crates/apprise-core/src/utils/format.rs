//! Message splitting helpers (upstream `apprise/utils/format.py`).
//!
//! Positions and limits are counted in characters, as upstream counts
//! Python string characters, so multibyte text is never cut inside a
//! character and limits mean the same thing as upstream's.

use crate::conversion::pystr;
use crate::types::NotifyFormat;

/// The longest an HTML entity can be, `&` and `;` included. The longest
/// named one is `&CounterClockwiseContourIntegral;`.
pub const HTML_ENTITY_MAXLEN: usize = 33;

// Characters we can apply a new line to if found.
const PUNCTUATION_CHARS: &str = ".!?:;";

fn rfind(text: &[char], c: char, start: usize, end: usize) -> Option<usize> {
  let end = end.min(text.len());
  if start >= end {
    return None;
  }
  text[start..end].iter().rposition(|&x| x == c).map(|p| p + start)
}

fn find(text: &[char], c: char, start: usize, end: usize) -> Option<usize> {
  let end = end.min(text.len());
  if start >= end {
    return None;
  }
  text[start..end].iter().position(|&x| x == c).map(|p| p + start)
}

/// Return where the tag opened at `start` ends (the index of its `>`), or
/// `None` if it is unclosed. A `>` inside a quoted attribute value does not
/// end the tag. `end` limits the scan (defaults to the end of the text).
pub fn html_tag_end(text: &[char], start: usize, end: Option<usize>) -> Option<usize> {
  let end = end.unwrap_or(text.len()).min(text.len());
  let mut idx = start + 1;
  loop {
    if idx >= end {
      return None;
    }
    let pos = text[idx..end].iter().position(|&c| c == '>' || c == '"' || c == '\'')? + idx;
    if text[pos] == '>' {
      return Some(pos);
    }
    // Skip past the matching quote so a ">" inside it is ignored.
    idx = find(text, text[pos], pos + 1, end)? + 1;
  }
}

/// Move a split before any HTML entity it would divide. For example, a
/// split inside `&nbsp;` moves back to `&` so the next chunk receives the
/// complete entity.
pub fn html_adjust(text: &[char], window_start: usize, split_at: usize) -> usize {
  if split_at <= window_start || split_at > text.len() {
    return split_at;
  }
  let search_start = window_start.max(split_at.saturating_sub(HTML_ENTITY_MAXLEN));
  let Some(amp_index) = rfind(text, '&', search_start, split_at) else {
    return split_at;
  };
  let forward_end = text.len().min(split_at + HTML_ENTITY_MAXLEN);
  if let Some(semi_index) = find(text, ';', amp_index, forward_end)
    && amp_index > window_start
    && amp_index < split_at
    && split_at <= semi_index
  {
    return amp_index;
  }
  split_at
}

/// Move a split out of an HTML tag: before its `<` so the tag goes to the
/// next chunk, or, when the tag opens the chunk, after its `>` when that
/// still fits. Returns the original split when neither applies.
pub fn html_tag_adjust(text: &[char], window_start: usize, split_at: usize, window_end: Option<usize>) -> usize {
  if split_at <= window_start || split_at > text.len() {
    return split_at;
  }
  let mut lt_index = find(text, '<', window_start, split_at);
  let mut gt_index = None;
  while let Some(lt) = lt_index {
    gt_index = html_tag_end(text, lt, window_end);
    match gt_index {
      Some(gt) if gt < split_at => lt_index = find(text, '<', gt + 1, split_at),
      _ => break,
    }
  }
  let Some(lt) = lt_index else {
    return split_at;
  };
  if lt > window_start {
    return lt;
  }
  gt_index.map(|gt| gt + 1).unwrap_or(split_at)
}

/// Move a split left when it cuts a Markdown or chat link (`[label](url)`,
/// `![alt](url)` or `<url|label>`). Returns the original split or the
/// construct's opening position.
pub fn markdown_adjust(text: &[char], window_start: usize, split_at: usize) -> usize {
  let len = text.len();
  if split_at <= window_start || split_at > len {
    return split_at;
  }
  let search_start = window_start;
  let forward_end = len.min(split_at + (split_at - window_start) + 1);

  let mut link_start = rfind(text, '[', search_start, split_at);
  if link_start.is_none()
    && let Some(bang) = rfind(text, '!', search_start, split_at)
    && bang + 1 < len
    && text[bang + 1] == '['
  {
    link_start = Some(bang);
  }
  if let Some(ls) = link_start
    && let Some(le) = find(text, ')', ls, forward_end)
    && ls < split_at
    && split_at < le
  {
    return ls;
  }

  if let Some(angle_start) = rfind(text, '<', search_start, split_at)
    && let Some(pipe) = find(text, '|', angle_start + 1, forward_end)
    && let Some(angle_end) = find(text, '>', pipe, forward_end)
    && angle_start < split_at
    && split_at <= angle_end
  {
    return angle_start;
  }
  split_at
}

fn is_split_ws(c: char) -> bool {
  matches!(c, ' ' | '\t' | '\r' | '\n' | '\x0b' | '\x0c')
}

/// Split `text` into pieces of at most `limit` characters, preferring
/// natural boundaries: a newline, then a space or tab, then punctuation
/// followed by whitespace, then the hard limit. HTML splits avoid cutting
/// entities and tags; Markdown splits also keep common link constructs and
/// backslash escapes whole.
///
/// A `limit` of zero returns `[""]`, as upstream does for `limit <= 0`.
pub fn smart_split(text: &str, limit: usize, body_format: &NotifyFormat) -> Vec<String> {
  if text.is_empty() || limit == 0 {
    return vec![String::new()];
  }
  let chars: Vec<char> = text.chars().collect();
  smart_split_chars(&chars, limit, body_format).into_iter().map(|(a, b)| chars[a..b].iter().collect()).collect()
}

/// [`smart_split`] over characters, returning `(start, end)` index pairs.
pub fn smart_split_chars(text: &[char], limit: usize, body_format: &NotifyFormat) -> Vec<(usize, usize)> {
  if text.is_empty() || limit == 0 {
    return vec![(0, 0)];
  }
  let mut result = Vec::new();
  let length = text.len();
  let mut start = 0;
  while start < length {
    let remaining = length - start;
    if remaining <= limit {
      result.push((start, length));
      break;
    }
    let window_end = (start + limit).min(length);

    // Each chunk is trimmed before it is sent, so only look for boundaries
    // after the first visible character.
    let mut visible = start;
    while visible < window_end && pystr::is_space(text[visible]) {
      visible += 1;
    }

    let split_nl = [rfind(text, '\n', visible, window_end), rfind(text, '\r', visible, window_end)].into_iter().flatten().max().map(|i| i + 1);
    let split_space_tab = [rfind(text, ' ', visible, window_end), rfind(text, '\t', visible, window_end)].into_iter().flatten().max().map(|i| i + 1);
    let mut split_punct = None;
    let mut i = visible;
    while i < window_end {
      if PUNCTUATION_CHARS.contains(text[i]) && i + 1 < window_end && is_split_ws(text[i + 1]) {
        let mut j = i + 1;
        while j < window_end && is_split_ws(text[j]) {
          j += 1;
        }
        split_punct = Some(j);
        i = j;
      } else {
        i += 1;
      }
    }

    let mut split_at = split_nl.or(split_space_tab).or(split_punct).unwrap_or(window_end);
    let orig_split = split_at;
    match body_format {
      NotifyFormat::Html => {
        split_at = html_adjust(text, start, split_at);
        split_at = html_tag_adjust(text, start, split_at, Some(window_end));
      }
      NotifyFormat::Markdown => {
        split_at = html_adjust(text, start, split_at);
        split_at = markdown_adjust(text, start, split_at);
        // Never separate an escaping backslash from the character it escapes.
        let backslashes = text[start..split_at.max(start)].iter().rev().take_while(|&&c| c == '\\').count();
        if backslashes % 2 == 1 {
          split_at -= 1;
        }
      }
      NotifyFormat::Text => {}
    }
    if split_at <= start {
      split_at = orig_split;
    }
    result.push((start, split_at));
    start = split_at;
  }
  result
}

/// Convert `body` between formats; see [`crate::conversion::convert_between`].
pub fn convert_format(body: &str, from: &NotifyFormat, to: &NotifyFormat) -> String {
  crate::conversion::convert_between(from, to, body)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn split(text: &str, limit: usize, fmt: NotifyFormat) -> Vec<String> {
    smart_split(text, limit, &fmt)
  }

  #[test]
  fn test_smart_split_boundaries() {
    assert_eq!(split("hello", 100, NotifyFormat::Text), vec!["hello"]);
    assert_eq!(split("hello\nworld!", 8, NotifyFormat::Text), vec!["hello\n", "world!"]);
    assert_eq!(split("hello world!", 8, NotifyFormat::Text), vec!["hello ", "world!"]);
    assert_eq!(split("abcdefghij", 5, NotifyFormat::Text), vec!["abcde", "fghij"]);
    assert_eq!(split("", 10, NotifyFormat::Text), vec![""]);
    assert_eq!(split("hello", 0, NotifyFormat::Text), vec![""]);
  }

  #[test]
  fn test_smart_split_counts_characters() {
    // Multibyte characters count once each and are never cut in half.
    assert_eq!(split("ééééé", 2, NotifyFormat::Text), vec!["éé", "éé", "é"]);
    assert_eq!(split("🐧🐧 🐧", 3, NotifyFormat::Text), vec!["🐧🐧 ", "🐧"]);
  }

  #[test]
  fn test_smart_split_keeps_entities_and_tags() {
    assert_eq!(split("aaaa &amp; b", 7, NotifyFormat::Html), vec!["aaaa ", "&amp; b"]);
    // Expected values come from upstream's smart_split(); a tag too long
    // for its window is moved to the next chunk, then cut at spaces.
    assert_eq!(split("x <a href=\"y z\">l</a>", 10, NotifyFormat::Html), vec!["x ", "<a ", "href=\"y ", "z\">l</a>"]);
  }

  #[test]
  fn test_html_tag_end() {
    let t: Vec<char> = "<a b=\">\">x".chars().collect();
    assert_eq!(html_tag_end(&t, 0, None), Some(8));
    let t: Vec<char> = "<a b=\"x".chars().collect();
    assert_eq!(html_tag_end(&t, 0, None), None);
  }
}
