//! Ports of upstream tests that the recorded vectors do not cover: the
//! performance guards (their inputs are too large to record) and named
//! regressions. Expected values come from upstream Apprise.

use std::time::{Duration, Instant};

use super::*;
use crate::types::NotifyFormat;
use crate::utils::format::smart_split;

// Upstream's limits, with room for unoptimized test builds.
fn assert_fast(start: Instant, limit_secs: u64) {
  let elapsed = start.elapsed();
  assert!(elapsed < Duration::from_secs(limit_secs * 3), "took {:?}", elapsed);
}

#[test]
fn test_pre_keeps_line_breaks_and_indentation() {
  // Upstream a76d6778.
  assert_eq!(html_to_text("<pre>  a\n    b\r\nc</pre><p>x</p>"), "a\n    b\nc\nx");
  assert_eq!(html_to_text("<p>before</p><pre>\n  indented\n\n  kept\n</pre>after"), "before\nindented\n\n  kept\nafter");
  assert_eq!(html_to_text("<pre><b>x</b>  y</pre>"), "x  y");
}

#[test]
fn test_split_ignores_boundaries_in_leading_whitespace() {
  // Upstream bf4c7234: a boundary inside the whitespace that opens a
  // window would produce a piece that is empty once trimmed.
  assert_eq!(smart_split("\n\nabc def ghi", 5, &NotifyFormat::Text), vec!["\n\nabc", " def ", "ghi"]);
  assert_eq!(smart_split("   hello world", 6, &NotifyFormat::Text), vec!["   hel", "lo ", "world"]);
}

#[test]
fn test_headings_type7_long_whitespace_is_linear() {
  let time = |spaces: usize| {
    let body = format!("<x {}<\n# real", " ".repeat(spaces));
    let start = Instant::now();
    commonmark_headings_to_bold(&body);
    start.elapsed()
  };
  let (small, large) = (time(1600), time(6400));
  assert!(large < small * 8 + Duration::from_millis(200), "{small:?} vs {large:?}");
}

#[test]
fn test_headings_nested_quotes_and_deep_lines_are_fast() {
  let start = Instant::now();
  commonmark_headings_to_bold(&format!("{}x", "> ".repeat(5000)));
  assert_fast(start, 1);
  let start = Instant::now();
  commonmark_headings_to_bold(&format!("{}x\n", "> ".repeat(8)).repeat(1000));
  assert_fast(start, 1);
}

#[test]
fn test_headings_repeated_fences_are_linear() {
  let small = " ~~~\n text\n ~~~\n".repeat(2000);
  let large = " ~~~\n text\n ~~~\n".repeat(4000);
  let start = Instant::now();
  commonmark_headings_to_bold(&small);
  let small_time = start.elapsed();
  let start = Instant::now();
  commonmark_headings_to_bold(&large);
  let large_time = start.elapsed();
  assert!(large_time < small_time * 5 / 2 + Duration::from_millis(500), "{small_time:?} vs {large_time:?}");
}

#[test]
fn test_headings_long_space_run() {
  let start = Instant::now();
  let body = format!("# a{}b", " ".repeat(100000));
  assert_eq!(commonmark_headings_to_bold(&body), format!("**a{}b**", " ".repeat(100000)));
  assert_fast(start, 5);
  assert_eq!(commonmark_headings_to_bold(&format!("# a{}#", " ".repeat(1000))), "**a**");
}

#[test]
fn test_tag_reducer_long_space_run() {
  let start = Instant::now();
  let run = " \t".repeat(50000);
  assert_eq!(HtmlTagReducer::default().reduce(&format!("<p>a{run}b")), format!("a{run}b"));
  assert_fast(start, 5);
}

#[test]
fn test_split_long_html_is_fast() {
  let expand = |s: &str| s.replace('&', "&amp;");
  let body = "&".repeat(200000);
  let start = Instant::now();
  let pieces = split_dialect_chunk(&body, 100, &expand, Some(&NotifyFormat::Html));
  assert_fast(start, 5);
  assert_eq!(pieces.concat(), "&amp;".repeat(200000));
  assert!(pieces.iter().all(|p| p.chars().count() <= 100));
  assert_eq!(truncate_dialect_chunk(&body, 100, &expand, Some(&NotifyFormat::Html)), "&amp;".repeat(20));
}

#[test]
fn test_split_unbalanced_quotes_are_fast() {
  let identity = |s: &str| s.to_string();
  for body in ["<a title=\"".repeat(20000), "<a title='".repeat(20000)] {
    let start = Instant::now();
    let pieces = split_dialect_chunk(&body, 100, &identity, Some(&NotifyFormat::Html));
    truncate_dialect_chunk(&body, 100, &identity, Some(&NotifyFormat::Html));
    assert_fast(start, 5);
    assert_eq!(pieces.concat(), body);
  }
}

#[test]
fn test_sections_are_fast() {
  let start = Instant::now();
  assert_eq!(commonmark_sections(&"\n".repeat(200000), 10, 10), (String::new(), Vec::new()));
  assert_fast(start, 5);
  let body = format!("x\n{}{}", "\n".repeat(100000), format!("# h\n{}vv\n", "\n".repeat(50)).repeat(2000));
  let start = Instant::now();
  let (desc, sections) = commonmark_sections(&body, 10, 10);
  assert_fast(start, 5);
  assert_eq!(desc, "x");
  assert_eq!(sections, vec![("h".to_string(), "vv".to_string()); 2000]);
}

#[test]
fn test_sentinel_with_every_private_use_character() {
  let every: Vec<char> =
    [(0xE000u32, 0xF8FFu32), (0xF0000, 0xFFFFD), (0x100000, 0x10FFFD)].into_iter().flat_map(|(a, b)| (a..=b).filter_map(char::from_u32)).collect();
  let mut body: String = every.iter().collect();
  for &first in &every[..10] {
    for &c in &every {
      if c != first {
        body.push(first);
        body.push(c);
      }
    }
  }
  let start = Instant::now();
  let picked = commonmark_pick_emphasis_sentinel(&body);
  assert_fast(start, 5);
  assert_eq!(picked, format!("{}{}", '\u{e00a}', '\u{e00c}'));
  assert!(!body.contains(&picked));
}

#[test]
fn test_markdown_long_inputs_are_fast() {
  // Delimiter-heavy and backtick-heavy text must stay well within a send.
  for body in ["a*b ".repeat(4000), "`x` ".repeat(4000), "_a_ **b** ".repeat(2000), "[l](u) <b>x</b> ".repeat(2000)] {
    let start = Instant::now();
    markdown_to_html(&body);
    assert_fast(start, 5);
  }
}
