//! Fit plugin dialect conversions to service limits after CommonMark repair
//! (upstream `apprise/conversion/dialect.py`).
//!
//! A plugin's dialect conversion can grow or shrink the text (Slack writes
//! `&` as `&amp;`, Telegram escapes punctuation, ...), so a chunk that fits
//! before conversion may not fit after it. These helpers search for the
//! longest source prefix whose *converted* form fits, repairing CommonMark
//! constructs cut at the edge.

use super::commonmark::{RepairState, repair_chars};
use super::pystr;
use crate::types::NotifyFormat;
use crate::utils::format::{HTML_ENTITY_MAXLEN, html_tag_end};

// Only CommonMark markup can make a longer converted prefix fit again.
fn has_commonmark_markup(text: &[char]) -> bool {
  text.iter().any(|c| matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '<' | '>'))
}

// Bound the repeat checks so results do not depend on CPU speed.
const VERIFY_SCAN_UPWARD_PROBES: usize = 24;
const VERIFY_SCAN_DOWNWARD_PROBES: usize = 8;

fn char_len(s: &str) -> usize {
  s.chars().count()
}

/// Longest fitting repaired conversion from `offset`: `(converted, consumed, state)`.
fn longest_fitting_prefix(body: &[char], offset: usize, limit: usize, convert: &dyn Fn(&str) -> String, pending: &RepairState) -> (String, usize, RepairState) {
  let n = body.len() - offset;
  let lookahead_span = (limit * 8).clamp(1, 2048);

  let repair = |mid: usize| -> (String, RepairState) {
    let abs_mid = offset + mid;
    let lookahead_end = (abs_mid + lookahead_span).min(body.len());
    let lookahead = &body[abs_mid.min(body.len())..lookahead_end];
    let boundary = body.get(abs_mid + lookahead_span).copied();
    let next = if lookahead.is_empty() { None } else { Some(lookahead) };
    repair_chars(&body[offset..abs_mid], pending, next, boundary)
  };
  let fits = |mid: usize| -> (bool, String, RepairState) {
    let (candidate, state) = repair(mid);
    let converted = convert(&candidate);
    (char_len(&converted) <= limit, converted, state)
  };

  let mut best: (String, usize, RepairState) = (String::new(), 0, pending.clone());

  // Grow geometrically to find a useful search boundary.
  let mut probe = n.min(limit.max(1));
  let hi;
  loop {
    let (ok, candidate, state) = fits(probe);
    if !ok {
      hi = probe;
      break;
    }
    best = (candidate, probe, state);
    if probe >= n {
      return best;
    }
    probe = n.min(probe * 2);
  }

  let first_fail = hi;
  let verify_end = n.min(first_fail + lookahead_span.min(24));

  // Bisect between the largest fit and the first overflow.
  let (mut lo, mut hi) = (best.1 as isize + 1, hi as isize - 1);
  while lo <= hi {
    let mid = ((lo + hi) / 2) as usize;
    let (ok, candidate, state) = fits(mid);
    if ok {
      best = (candidate, mid, state);
      lo = mid as isize + 1;
    } else {
      hi = mid as isize - 1;
    }
  }

  // Repair can make a longer prefix shorter; verify near the boundary.
  let best_len = best.1;
  let verify_region = &body[offset + best_len.min(verify_end)..offset + verify_end];
  if has_commonmark_markup(verify_region) {
    let upward_end = verify_end.min(best_len + VERIFY_SCAN_UPWARD_PROBES);
    let mut best_upward = None;
    for mid in best_len + 1..=upward_end {
      let (ok, candidate, state) = fits(mid);
      if ok {
        best_upward = Some((candidate, mid, state));
      }
    }
    let mut probes = 0;
    let mut mid = verify_end;
    while mid > upward_end {
      probes += 1;
      let (ok, candidate, state) = fits(mid);
      if ok {
        return (candidate, mid, state);
      }
      if probes >= VERIFY_SCAN_DOWNWARD_PROBES {
        break;
      }
      mid -= 1;
    }
    if let Some(b) = best_upward {
      return b;
    }
  }

  if best.1 == 0 {
    // Not even a single character fits. Force progress anyway.
    let (candidate, state) = repair(1);
    best = (convert(&candidate), 1, state);
  }
  best
}

/// Cut positions outside HTML tags and entities; the end is always included.
fn html_safe_cuts(body: &[char]) -> Vec<usize> {
  let mut cuts = Vec::new();
  let mut entity_end: isize = -1;
  let n = body.len();
  let mut idx = 0;
  while idx < n {
    if idx as isize > entity_end {
      cuts.push(idx);
    }
    let ch = body[idx];
    if ch == '<' {
      match html_tag_end(body, idx, None) {
        Some(end) => idx = end,
        None => break,
      }
    } else if ch == '&' {
      let end = (idx + HTML_ENTITY_MAXLEN).min(n);
      if let Some(semi) = body[idx..end].iter().position(|&c| c == ';').map(|p| p + idx)
        && !body[idx..semi].iter().any(|&c| pystr::is_space(c))
      {
        entity_end = semi as isize;
      }
    }
    idx += 1;
  }
  cuts.push(n);
  cuts
}

fn bisect_right(cuts: &[usize], x: usize, lo: usize) -> usize {
  lo + cuts[lo..].partition_point(|&c| c <= x)
}

/// Longest converted prefix ending at one of `cuts`, without repair.
fn longest_fitting_cut(body: &[char], offset: usize, limit: usize, convert: &dyn Fn(&str) -> String, cuts: &[usize]) -> (String, usize) {
  let slice = |end: usize| -> String { body[offset..end].iter().collect() };
  let first = bisect_right(cuts, offset, 0);
  let (mut lo, mut hi) = (first as isize, cuts.len() as isize - 1);
  let mut best: Option<(String, usize)> = None;

  let mut span = limit.max(1);
  while lo <= hi {
    let probe = (lo as usize).max(bisect_right(cuts, offset + span, lo as usize).saturating_sub(1));
    let candidate = convert(&slice(cuts[probe]));
    if char_len(&candidate) > limit {
      hi = probe as isize - 1;
      break;
    }
    best = Some((candidate, cuts[probe] - offset));
    lo = probe as isize + 1;
    span *= 2;
  }
  while lo <= hi {
    let mid = ((lo + hi) / 2) as usize;
    let candidate = convert(&slice(cuts[mid]));
    if char_len(&candidate) <= limit {
      best = Some((candidate, cuts[mid] - offset));
      lo = mid as isize + 1;
    } else {
      hi = mid as isize - 1;
    }
  }
  best.unwrap_or_else(|| (convert(&slice(cuts[first])), cuts[first] - offset))
}

/// Split `body` into converted pieces of at most `limit` characters.
///
/// `dialect_convert` must be deterministic and generally preserve input
/// order. CommonMark (`Markdown`, or `None`) is repaired around each cut so
/// emphasis and links stay balanced; HTML is only cut outside tags and
/// entities; plain text is cut as it is.
pub fn split_dialect_chunk(body: &str, limit: usize, dialect_convert: &dyn Fn(&str) -> String, body_format: Option<&NotifyFormat>) -> Vec<String> {
  if body.is_empty() {
    return vec![dialect_convert(body)];
  }
  let converted = dialect_convert(body);
  if char_len(&converted) <= limit {
    return vec![converted];
  }
  let chars: Vec<char> = body.chars().collect();
  let n = chars.len();
  let mut pieces: Vec<String> = Vec::new();
  let mut offset = 0;
  let push = |piece: String, pieces: &mut Vec<String>| {
    // Skip redundant empty closers, but always return at least one piece.
    if !piece.is_empty() || pieces.is_empty() {
      pieces.push(piece);
    }
  };
  match body_format {
    Some(NotifyFormat::Html) | Some(NotifyFormat::Text) => {
      let cuts: Vec<usize> = if body_format == Some(&NotifyFormat::Html) { html_safe_cuts(&chars) } else { (0..=n).collect() };
      while offset < n {
        let (piece, consumed) = longest_fitting_cut(&chars, offset, limit, dialect_convert, &cuts);
        push(piece, &mut pieces);
        offset += consumed;
      }
    }
    _ => {
      let mut state = RepairState::default();
      while offset < n {
        let (piece, consumed, next_state) = longest_fitting_prefix(&chars, offset, limit, dialect_convert, &state);
        push(piece, &mut pieces);
        offset += consumed;
        state = next_state;
      }
    }
  }
  pieces
}

/// Return the longest converted prefix of `body` within `limit` characters.
/// HTML cuts keep tags and entities whole; plain text is not repaired as
/// markup; CommonMark is repaired at the cut. Everything after it is
/// discarded.
pub fn truncate_dialect_chunk(body: &str, limit: usize, dialect_convert: &dyn Fn(&str) -> String, body_format: Option<&NotifyFormat>) -> String {
  if body.is_empty() {
    return dialect_convert(body);
  }
  let converted = dialect_convert(body);
  if char_len(&converted) <= limit {
    return converted;
  }
  let chars: Vec<char> = body.chars().collect();
  match body_format {
    Some(NotifyFormat::Html) => longest_fitting_cut(&chars, 0, limit, dialect_convert, &html_safe_cuts(&chars)).0,
    Some(NotifyFormat::Text) => {
      let cuts: Vec<usize> = (0..=chars.len()).collect();
      longest_fitting_cut(&chars, 0, limit, dialect_convert, &cuts).0
    }
    _ => longest_fitting_prefix(&chars, 0, limit, dialect_convert, &RepairState::default()).0,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_identity_split_fits() {
    let id = |s: &str| s.to_string();
    assert_eq!(split_dialect_chunk("abc", 10, &id, Some(&NotifyFormat::Markdown)), vec!["abc"]);
    assert_eq!(split_dialect_chunk("", 10, &id, None), vec![""]);
    assert_eq!(truncate_dialect_chunk("abcdef", 3, &id, Some(&NotifyFormat::Text)), "abc");
  }
}
