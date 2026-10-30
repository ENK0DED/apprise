//! Python-Markdown 3.11's `DelimiterProcessor`: `*` and `_` emphasis.
//!
//! It scans ahead from the first delimiter run, pairs runs into regions,
//! and builds the nested `<strong>`/`<em>` elements for the first region.
//! The remaining regions are cached and handed out as the inline processor
//! reaches them, shifted by however much earlier replacements moved the
//! text. Positions are character indices, as in Python: the cache applies
//! a region's lengths at shifted positions, so byte and character lengths
//! would give different results.

use std::collections::VecDeque;

use super::super::pystr;
use super::super::unicode_tables::WORD;
use super::{Id, Text, Tree};

// (start, end, ambiguous, length)
type Entry = (usize, usize, bool, usize);

#[derive(Debug, Clone, Copy)]
struct Region {
  a: usize,
  b: usize,
  c: usize,
  d: usize,
  size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
  Ambiguous,
  End,
  Start,
}

struct Delim {
  token: char,
  smart: bool,
  stack: VecDeque<Entry>,
}

// Both delimiters render "strong,em": two tags, no double/single mode.
const TAG_COUNT: usize = 2;
const MAX_SIZE: usize = 2;

pub(super) struct Emphasis {
  regions: Vec<Region>,
  stack: Vec<Entry>,
  cache_index: usize,
  cache_pos: usize,
  delims: [Delim; 2],
}

fn in_ranges(table: &[(u32, u32)], c: char) -> bool {
  let cp = c as u32;
  table
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

/// Python's `\w`.
fn is_word(c: char) -> bool {
  c.is_ascii_alphanumeric() || c == '_' || (!c.is_ascii() && in_ranges(WORD, c))
}

/// `(?!$)`: not at the end, nor before a final newline.
fn not_end(s: &[char], j: usize) -> bool {
  !(j >= s.len() || (j + 1 == s.len() && s[j] == '\n'))
}

/// Python slice `s[a:b]`.
fn py_slice(s: &[char], a: isize, b: isize) -> String {
  let len = s.len() as isize;
  let fix = |x: isize| -> usize { if x < 0 { (len + x).max(0) as usize } else { x.min(len) as usize } };
  let (a, b) = (fix(a), fix(b));
  if a >= b { String::new() } else { s[a..b].iter().collect() }
}

impl Delim {
  /// `boundary.match(data, i)`: (kind, start, end).
  fn boundary(&self, data: &[char], i: usize) -> Option<(Kind, usize, usize)> {
    if data.get(i) != Some(&self.token) {
      return None;
    }
    let tok = self.token;
    let j = i + data[i..].iter().take_while(|&&c| c == tok).count();
    let prev = if i > 0 { Some(data[i - 1]) } else { None };
    let next = data.get(j).copied();
    let prev_not_ws_tok = prev.is_none_or(|c| !pystr::is_space(c) && c != tok);
    let next_not_ws_tok = next.is_none_or(|c| !pystr::is_space(c) && c != tok);
    let (xstart, xend) = if self.smart { (prev.is_none_or(|c| !is_word(c) && c != tok), next.is_none_or(|c| !is_word(c) && c != tok)) } else { (true, true) };
    if i != 0 && prev_not_ws_tok && xstart && xend && next_not_ws_tok && not_end(data, j) {
      return Some((Kind::Ambiguous, i, j));
    }
    if i != 0 && prev_not_ws_tok && xend {
      return Some((Kind::End, i, j));
    }
    if xstart && next_not_ws_tok && not_end(data, j) {
      return Some((Kind::Start, i, j));
    }
    None
  }
}

impl Emphasis {
  pub fn new() -> Self {
    Emphasis {
      regions: Vec::new(),
      stack: Vec::new(),
      cache_index: 0,
      cache_pos: 0,
      delims: [Delim { token: '*', smart: false, stack: VecDeque::new() }, Delim { token: '_', smart: true, stack: VecDeque::new() }],
    }
  }

  fn reset(&mut self) {
    for d in self.delims.iter_mut() {
      d.stack.clear();
    }
    self.regions.clear();
    self.stack.clear();
    self.cache_index = 0;
    self.cache_pos = 0;
  }

  fn delim_index(token: char) -> usize {
    if token == '*' { 0 } else { 1 }
  }

  fn any_stack(&self) -> bool {
    self.delims.iter().any(|d| !d.stack.is_empty())
  }

  fn other_stack(&self, di: usize) -> bool {
    self.delims.iter().enumerate().any(|(k, d)| k != di && !d.stack.is_empty())
  }

  fn get_match(&self, data: &[char], start: usize) -> Option<(Kind, usize, usize)> {
    self.delims.iter().find_map(|d| d.boundary(data, start))
  }

  fn search(&self, data: &[char], start: usize) -> Option<(Kind, usize, usize)> {
    for i in start..data.len() {
      if data[i] == '*' || data[i] == '_' {
        let d = &self.delims[Self::delim_index(data[i])];
        if let Some(m) = d.boundary(data, i) {
          if d.stack.is_empty() && m.0 == Kind::End {
            continue;
          }
          return Some(m);
        }
      }
    }
    None
  }

  fn add_region(&mut self, a: usize, b: usize, c: usize, d: usize, size: usize) {
    self.regions.push(Region { a, b, c, d, size });
    for delim in self.delims.iter_mut() {
      while let Some(&(p, ..)) = delim.stack.back() {
        if p.max(a) as isize <= (p as isize).min(d as isize - 1) {
          delim.stack.pop_back();
          continue;
        }
        break;
      }
    }
  }

  fn build_element(&self, tree: &mut Tree, data: &[char], start: usize, offset: isize) -> (Option<Id>, usize) {
    let regions = &self.regions;
    let sl = |a: usize, b: usize| py_slice(data, a as isize + offset, b as isize + offset);
    let mut el: Option<Id> = None;
    let mut last: Option<Id> = None;
    let mut previous: Option<Region> = None;
    let mut outer: Vec<Id> = Vec::new();
    let mut outer_r: Vec<Region> = Vec::new();
    let end = regions.len();
    let mut idx = 0;
    for (k, i) in (start..end).enumerate() {
      idx = k + 1;
      let r = regions[i];
      if r.a >= regions[start].d {
        idx -= 1;
        break;
      }
      let el1 = tree.element(if r.size == 2 { "strong" } else { "em" });
      if idx > 1
        && let (Some(lst), Some(prev)) = (last, previous)
      {
        if tree.nodes[lst].text.is_none() {
          let t = if prev.c < r.a { sl(prev.b, prev.c) } else { sl(prev.b, r.a) };
          tree.nodes[lst].text = Some(Text::plain(t));
        }
        if Some(&lst) != outer.last() && tree.nodes[lst].tail.is_none() {
          let o = *outer_r.last().unwrap();
          if r.a < o.d {
            tree.nodes[lst].tail = Some(Text::plain(sl(prev.d, r.a)));
          } else {
            tree.nodes[lst].tail = Some(Text::plain(sl(prev.d, o.c)));
            let oid = *outer.last().unwrap();
            tree.nodes[oid].tail = Some(Text::plain(sl(o.d, r.a)));
          }
        }
      }
      if el.is_none() {
        el = Some(el1);
        last = Some(el1);
        outer.push(el1);
        outer_r.push(r);
      } else {
        while outer_r.len() > 1 && r.d > outer_r.last().unwrap().d {
          outer.pop();
          outer_r.pop();
        }
        let parent = *outer.last().unwrap();
        tree.nodes[parent].children.push(el1);
        if i + 1 < end && r.d > regions[i + 1].d {
          outer.push(el1);
          outer_r.push(r);
        }
        last = Some(el1);
      }
      previous = Some(r);
    }
    while let Some(&o) = outer.last() {
      let (Some(lst), Some(prev)) = (last, previous) else {
        break;
      };
      if tree.nodes[lst].text.is_none() {
        tree.nodes[lst].text = Some(Text::plain(sl(prev.b, prev.c)));
      }
      if tree.nodes[lst].tail.is_none() && lst != o {
        let or = *outer_r.last().unwrap();
        tree.nodes[lst].tail = Some(Text::plain(sl(prev.d, or.c)));
      }
      last = outer.pop();
      previous = outer_r.pop();
    }
    (el, idx)
  }

  fn increment_next_position(&mut self, start: usize, count: usize) {
    self.cache_index += count;
    if self.cache_index < self.regions.len() {
      self.cache_pos = self.regions[self.cache_index].a;
      while !self.stack.is_empty() {
        let entry = self.stack.remove(0);
        if start < entry.0 && entry.0 <= self.cache_pos {
          self.cache_pos = entry.0;
          break;
        }
      }
    } else {
      self.reset();
    }
  }

  fn get_cached_result(&mut self, tree: &mut Tree, pos: usize, data: &[char]) -> (Option<Id>, usize, usize) {
    let region = self.regions[self.cache_index];
    let offset = if pos != self.cache_pos { pos as isize - self.cache_pos as isize } else { pos as isize - region.a as isize };
    let (start, end) = (region.a, region.d);
    let (el, count) = self.build_element(tree, data, self.cache_index, offset);
    self.increment_next_position(start, count);
    let shift = |x: usize| (x as isize + offset).max(0) as usize;
    (el, shift(start), shift(end))
  }

  /// `DelimiterProcessor.handleMatch` for the `*` or `_` at character
  /// `m_start`: the element (if any) replacing characters `start..end`.
  pub fn handle_match(&mut self, tree: &mut Tree, m_start: usize, data: &[char]) -> (Option<Id>, usize, usize) {
    if !self.regions.is_empty() {
      return self.get_cached_result(tree, m_start, data);
    }
    let b = data;
    let m2 = self.get_match(data, m_start);
    let Some((kind, start, mut end)) = m2.filter(|m| m.0 != Kind::End) else {
      if let Some((_, s, e)) = m2 {
        return (None, s, e);
      }
      // delim.bad: token+
      let tok = b[m_start];
      let e = m_start + b[m_start..].iter().take_while(|&&c| c == tok).count();
      return (None, m_start, e);
    };
    let di = Self::delim_index(b[m_start]);
    let length = end - start;
    let is_ambiguous = kind != Kind::Start;
    self.delims[di].stack.push_back((start, start + length, is_ambiguous, length));

    while self.any_stack() {
      let Some((kind, m2_start, m2_end)) = self.search(data, end) else {
        break;
      };
      let di = Self::delim_index(b[m2_start]);
      let mut start = m2_start;
      end = m2_end;
      let mut current = m2_end - m2_start;
      let mut is_start = kind != Kind::End;
      let mut is_end = !is_start || kind != Kind::Start;
      let is_ambiguous = is_start && is_end;
      let mut last = self.delims[di].stack.back().map(|e| e.3).unwrap_or(0);

      if !self.delims[di].stack.is_empty() && is_end && ((!is_ambiguous && current > last) || current == last || current >= 3) {
        is_start = false;
        let original = current;
        while !self.delims[di].stack.is_empty() && current > 0 && last <= current {
          let delimiter = self.delims[di].stack.pop_back().unwrap();
          let size = delimiter.3.min(if TAG_COUNT == 2 && delimiter.3 == 3 { 1 } else { MAX_SIZE });
          self.add_region(delimiter.1 - size, delimiter.1, start, start + size, size);
          start += size;
          current -= size;
          if size < delimiter.3 {
            let new = delimiter.3 - size;
            self.delims[di].stack.push_back((delimiter.0, delimiter.1 - size, delimiter.2, new));
          }
          if self.delims[di].stack.is_empty() {
            if self.other_stack(di) {
              self.delims[di].stack.clear();
              continue;
            }
            break;
          }
          last = self.delims[di].stack.back().unwrap().3;
        }
        if original >= 3 && current > 0 && is_ambiguous {
          self.delims[di].stack.push_back((m2_start + (original - current), end, false, current));
          is_end = false;
        } else {
          is_end = current > 0 && !self.delims[di].stack.is_empty() && last > current;
        }
      }

      if !self.delims[di].stack.is_empty() && is_end && (last >= 3 || !is_ambiguous) && last > current {
        let mut delimiter = self.delims[di].stack.pop_back().unwrap();
        while !self.delims[di].stack.is_empty() && delimiter.3 != 3 && delimiter.2 {
          delimiter = self.delims[di].stack.pop_back().unwrap();
          last = delimiter.3;
        }
        if delimiter.2 && delimiter.3 != 3 {
          if self.other_stack(di) {
            self.delims[di].stack.clear();
            continue;
          }
          break;
        }
        is_start = false;
        let (ds, mut de) = (delimiter.0, delimiter.1);
        while current > 0 {
          let size = current.min(if TAG_COUNT == 2 && current == 3 { 1 } else { MAX_SIZE });
          let new = last.saturating_sub(size);
          self.add_region(ds + new, de, start, start + size, size);
          start += size;
          current -= size;
          last = last.saturating_sub(size);
          de = de.saturating_sub(size);
        }
        if last > 0 {
          self.delims[di].stack.push_back((ds, de, false, last));
        }
      }

      if is_start {
        self.delims[di].stack.push_back((start, end, is_ambiguous, current));
      }
    }

    for d in self.delims.iter() {
      self.stack.extend(d.stack.iter().copied());
    }
    self.stack.sort_by_key(|e| e.0);
    if !self.regions.is_empty() {
      self.regions.sort_by_key(|r| r.a);
      let (start, end) = (self.regions[0].a, self.regions[0].d);
      let (el, count) = self.build_element(tree, data, 0, 0);
      self.increment_next_position(start, count);
      return (el, start, end);
    }
    let start = m_start;
    let end = self.stack.last().map(|e| e.1).unwrap_or(m_start + 1);
    self.reset();
    (None, start, end)
  }
}
