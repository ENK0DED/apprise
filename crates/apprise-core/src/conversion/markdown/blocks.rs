//! Python-Markdown's `BlockParser` and block processors, plus the `tables`
//! extension's `TableProcessor`.

use std::collections::VecDeque;

use super::super::pystr;
use super::super::unicode_tables::DECIMAL;
use super::{Id, Markdown, Text};

const TAB: usize = 4;

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

/// Python's `\d`.
pub(super) fn is_decimal(c: char) -> bool {
  c.is_ascii_digit() || (!c.is_ascii() && in_ranges(DECIMAL, c))
}

fn strip_is_empty(s: &str) -> bool {
  pystr::strip(s).is_empty()
}

/// Parse the preprocessed document into `root`.
pub(super) fn parse_document(md: &mut Markdown, root: Id, source: &str, tables: bool) {
  let mut p = BlockParser { md, state: Vec::new(), tables, depth: 0 };
  p.parse_chunk(root, source);
}

struct BlockParser<'a> {
  md: &'a mut Markdown,
  state: Vec<&'static str>,
  tables: bool,
  /// How many `parse_blocks` calls are active: the nesting depth of
  /// blockquotes, lists and indented blocks.
  depth: usize,
}

/// Deepest container nesting that is parsed as such; deeper markup stays
/// paragraph text. Python-Markdown stops nesting blockquotes as it nears the
/// interpreter's recursion limit, which upstream reaches at 298 levels (its
/// lists raise `RecursionError` a little deeper). The limit also bounds the
/// recursion here, so hostile input cannot overflow the stack.
const MAX_NESTING: usize = 298;

#[derive(Clone, Copy, PartialEq)]
enum Proc {
  Empty,
  Indent,
  Code,
  Table,
  HashHeader,
  Setext,
  Hr,
  OList,
  UList,
  Quote,
  Reference,
  Paragraph,
}

const PROCS: &[Proc] = &[
  Proc::Empty,
  Proc::Indent,
  Proc::Code,
  Proc::Table,
  Proc::HashHeader,
  Proc::Setext,
  Proc::Hr,
  Proc::OList,
  Proc::UList,
  Proc::Quote,
  Proc::Reference,
  Proc::Paragraph,
];

fn spaces(s: &str) -> usize {
  s.bytes().take_while(|&b| b == b' ').count()
}

// ^[ ]{0,3}\d+\.[ ]+(.*)  /  ^[ ]{0,3}[*+-][ ]+(.*) ; returns group(1) start.
fn list_item(line: &str, ordered: bool) -> Option<usize> {
  let lead = spaces(line);
  if lead > TAB - 1 {
    return None;
  }
  let rest = &line[lead..];
  let marker = if ordered {
    let digits: usize = rest.chars().take_while(|&c| is_decimal(c)).map(char::len_utf8).sum();
    if digits == 0 || !rest[digits..].starts_with('.') {
      return None;
    }
    digits + 1
  } else {
    if !matches!(rest.as_bytes().first(), Some(b'*' | b'+' | b'-')) {
      return None;
    }
    1
  };
  let gap = spaces(&rest[marker..]);
  if gap == 0 {
    return None;
  }
  Some(lead + marker + gap)
}

// CHILD_RE: ^[ ]{0,3}((\d+\.)|[*+-])[ ]+(.*) ; returns group(3) start.
fn child_item(line: &str) -> Option<usize> {
  list_item(line, true).or_else(|| list_item(line, false))
}

// INDENT_RE: ^[ ]{4,7}((\d+\.)|[*+-])[ ]+.*
fn indent_item(line: &str) -> bool {
  let lead = spaces(line);
  if !(TAB..=TAB * 2 - 1).contains(&lead) {
    return false;
  }
  let rest = &line[lead..];
  let digits: usize = rest.chars().take_while(|&c| is_decimal(c)).map(char::len_utf8).sum();
  let marker = if digits > 0 && rest[digits..].starts_with('.') {
    digits + 1
  } else if matches!(rest.as_bytes().first(), Some(b'*' | b'+' | b'-')) {
    1
  } else {
    return false;
  };
  spaces(&rest[marker..]) > 0
}

// (^|\n)[ ]{0,3}>[ ]?(.*) ; returns the match start.
fn quote_search(block: &str) -> Option<usize> {
  let mut line_start = 0;
  loop {
    let rest = &block[line_start..];
    let lead = spaces(rest);
    if lead <= 3 && rest[lead..].starts_with('>') {
      return Some(if line_start == 0 { 0 } else { line_start - 1 });
    }
    line_start += rest.find('\n')? + 1;
  }
}

fn quote_clean(line: &str) -> String {
  if pystr::strip(line) == ">" {
    return String::new();
  }
  let lead = spaces(line);
  if lead <= 3 && line[lead..].starts_with('>') {
    let after = &line[lead + 1..];
    return after.strip_prefix(' ').unwrap_or(after).to_string();
  }
  line.to_string()
}

// HashHeaderProcessor.RE: (?:^|\n)(?P<level>#{1,6})(?P<header>(?:\\.|[^\\])*?)#*(?:\n|$)
// Returns (match start, match end, level, header).
fn hash_header(block: &str) -> Option<(usize, usize, usize, String)> {
  let b = block.as_bytes();
  let n = b.len();
  let mut candidates = vec![0usize];
  candidates.extend(block.match_indices('\n').map(|(i, _)| i + 1));
  for &s in &candidates {
    let hashes = b[s..].iter().take_while(|&&c| c == b'#').count();
    if hashes == 0 {
      continue;
    }
    for level in (1..=hashes.min(6)).rev() {
      let header_start = s + level;
      let mut p = header_start;
      loop {
        // Try `#*(\n|$)` here.
        let q = p + b[p..].iter().take_while(|&&c| c == b'#').count();
        if q == n || (q == n - 1 && b[q] == b'\n') || b[q] == b'\n' {
          let end = if q < n && b[q] == b'\n' { q + 1 } else { q };
          let start = if s == 0 { 0 } else { s - 1 };
          return Some((start, end, level, block[header_start..p].to_string()));
        }
        // Consume one `\\.` or `[^\\]`.
        if b[p] == b'\\' {
          let Some(next) = block[p + 1..].chars().next() else {
            break;
          };
          if next == '\n' {
            break;
          }
          p += 1 + next.len_utf8();
        } else {
          p += block[p..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
        if p > n {
          break;
        }
      }
    }
  }
  None
}

// SetextHeaderProcessor.RE: ^.*?\n(?:=+|-+)[ ]*(\n|$)
fn setext(block: &str) -> bool {
  let Some(nl) = block.find('\n') else {
    return false;
  };
  let line = &block[nl + 1..];
  let Some(&first) = line.as_bytes().first() else {
    return false;
  };
  if first != b'=' && first != b'-' {
    return false;
  }
  let run = line.bytes().take_while(|&c| c == first).count();
  let rest = &line[run..];
  let sp = spaces(rest);
  let rest = &rest[sp..];
  rest.is_empty() || rest.starts_with('\n')
}

// HRProcessor: the first line that is a thematic break; (start, end).
fn hr_search(block: &str) -> Option<(usize, usize)> {
  let mut line_start = 0;
  loop {
    let line_end = block[line_start..].find('\n').map(|p| line_start + p).unwrap_or(block.len());
    let line = &block[line_start..line_end];
    let lead = spaces(line);
    if lead <= 3
      && let Some(&c) = line.as_bytes().get(lead)
      && matches!(c, b'-' | b'_' | b'*')
    {
      let b = line.as_bytes();
      let mut pos = lead;
      let mut count = 0;
      while pos < b.len() && b[pos] == c {
        while pos < b.len() && b[pos] == c {
          pos += 1;
          count += 1;
        }
        let mut sp = 0;
        while pos < b.len() && b[pos] == b' ' && sp < 2 {
          pos += 1;
          sp += 1;
        }
      }
      if count >= 3 && line[pos..].bytes().all(|x| x == b' ') {
        return Some((line_start, line_end));
      }
    }
    if line_end >= block.len() {
      return None;
    }
    line_start = line_end + 1;
  }
}

// ReferenceProcessor.RE (MULTILINE):
// ^[ ]{0,3}\[([^\[\]]*)\]:[ ]*(?:\n[ ]*)?([^\s]+)[ ]*(?:\n[ ]*)?((["\'])(.*)\4[ ]*|\((.*)\)[ ]*)?$
struct RefMatch {
  start: usize,
  end: usize,
  id: String,
  link: String,
  title: Option<String>,
}

fn is_eol(s: &str, p: usize) -> bool {
  // `$` with MULTILINE: end of text or before a newline.
  p == s.len() || s.as_bytes()[p] == b'\n'
}

fn reference_search(block: &str) -> Option<RefMatch> {
  let b = block.as_bytes();
  let mut line_start = 0;
  loop {
    if let Some(m) = reference_at(block, line_start) {
      return Some(m);
    }
    line_start += block[line_start..].find('\n')? + 1;
    if line_start > b.len() {
      return None;
    }
  }
}

fn skip_spaces(s: &str, p: usize) -> usize {
  p + spaces(&s[p..])
}

fn optional_newline(s: &str, p: usize) -> Vec<usize> {
  // (?:\n[ ]*)? : greedy, so with-newline first.
  let mut out = Vec::new();
  if s.as_bytes().get(p) == Some(&b'\n') {
    out.push(skip_spaces(s, p + 1));
  }
  out.push(p);
  out
}

fn reference_at(s: &str, start: usize) -> Option<RefMatch> {
  let b = s.as_bytes();
  let lead = spaces(&s[start..]);
  if lead > 3 {
    return None;
  }
  let mut p = start + lead;
  if b.get(p) != Some(&b'[') {
    return None;
  }
  p += 1;
  let id_start = p;
  while p < b.len() && b[p] != b'[' && b[p] != b']' {
    p += 1;
  }
  if b.get(p) != Some(&b']') || b.get(p + 1) != Some(&b':') {
    return None;
  }
  let id = s[id_start..p].to_string();
  p = skip_spaces(s, p + 2);
  for link_start in optional_newline(s, p) {
    // ([^\s]+) greedy, backtracking from the longest.
    let max_end = link_start + s[link_start..].char_indices().find(|(_, c)| pystr::is_space(*c)).map(|(i, _)| i).unwrap_or(s.len() - link_start);
    let mut ends: Vec<usize> = s[link_start..max_end].char_indices().map(|(i, c)| link_start + i + c.len_utf8()).collect();
    ends.reverse();
    for link_end in ends {
      let after_link = skip_spaces(s, link_end);
      // [ ]* is greedy too; fewer spaces never help since the next part
      // starts with a newline, quote, paren or the end of the line.
      for title_start in optional_newline(s, after_link) {
        if let Some((end, title)) = reference_title(s, title_start) {
          return Some(RefMatch { start, end, id, link: s[link_start..link_end].to_string(), title });
        }
      }
    }
  }
  None
}

// ((["'])(.*)\4[ ]*|\((.*)\)[ ]*)?$  at `p`; returns (end, title).
fn reference_title(s: &str, p: usize) -> Option<(usize, Option<String>)> {
  let b = s.as_bytes();
  let line_end = s[p..].find('\n').map(|x| p + x).unwrap_or(s.len());
  match b.get(p) {
    Some(&q @ (b'"' | b'\'')) => {
      // (.*) greedy then the same quote, then spaces, then end of line.
      let mut k = line_end;
      while k > p + 1 {
        k -= 1;
        if b[k] == q && s[k + 1..line_end].bytes().all(|c| c == b' ') {
          return Some((line_end, Some(s[p + 1..k].to_string())));
        }
      }
    }
    Some(b'(') => {
      let mut k = line_end;
      while k > p + 1 {
        k -= 1;
        if b[k] == b')' && s[k + 1..line_end].bytes().all(|c| c == b' ') {
          return Some((line_end, Some(s[p + 1..k].to_string())));
        }
      }
    }
    _ => {}
  }
  if is_eol(s, p) { Some((p, None)) } else { None }
}

impl BlockParser<'_> {
  fn isstate(&self, s: &str) -> bool {
    self.state.last() == Some(&s)
  }

  fn tag(&self, id: Id) -> &str {
    self.md.tree.tag(id)
  }

  fn last_child(&self, id: Id) -> Option<Id> {
    self.md.tree.last_child(id)
  }

  fn parse_chunk(&mut self, parent: Id, text: &str) {
    let blocks: VecDeque<String> = text.split("\n\n").map(String::from).collect();
    self.parse_blocks(parent, blocks);
  }

  fn parse_blocks(&mut self, parent: Id, mut blocks: VecDeque<String>) {
    self.depth += 1;
    while let Some(first) = blocks.front().cloned() {
      for &proc in PROCS {
        if proc == Proc::Table && !self.tables {
          continue;
        }
        if let Some(true) = self.try_proc(proc, parent, &first, &mut blocks) {
          break;
        }
      }
    }
    self.depth -= 1;
  }

  /// Test and run one processor: `None` when its test fails, else whether
  /// it handled the block.
  fn try_proc(&mut self, proc: Proc, parent: Id, block: &str, blocks: &mut VecDeque<String>) -> Option<bool> {
    match proc {
      Proc::Empty => {
        if !(block.is_empty() || block.starts_with('\n')) {
          return None;
        }
        let block = blocks.pop_front().unwrap();
        let mut filler = "\n\n";
        if !block.is_empty() {
          filler = "\n";
          let rest = &block[1..];
          if !rest.is_empty() {
            blocks.push_front(rest.to_string());
          }
        }
        if let Some(code) = self.code_sibling(parent) {
          let text = format!("{}{}", self.md.tree.text_str(code), filler);
          self.md.tree.nodes[code].text = Some(Text::atomic(text));
        }
        Some(true)
      }
      Proc::Indent => {
        if self.depth > MAX_NESTING {
          return None;
        }
        let parent_is_item = self.tag(parent) == "li";
        let last_is_list = self.last_child(parent).is_some_and(|c| matches!(self.tag(c), "ul" | "ol"));
        if !(block.starts_with("    ") && !self.isstate("detabbed") && (parent_is_item || last_is_list)) {
          return None;
        }
        self.run_indent(parent, blocks);
        Some(true)
      }
      Proc::Code => {
        if !block.starts_with("    ") {
          return None;
        }
        let block = blocks.pop_front().unwrap();
        let (detabbed, rest) = detab(&block, TAB);
        let escaped = code_escape(pystr::rstrip(&detabbed));
        if let Some(code) = self.code_sibling(parent) {
          let text = format!("{}\n{}\n", self.md.tree.text_str(code), escaped);
          self.md.tree.nodes[code].text = Some(Text::atomic(text));
        } else {
          let pre = self.md.tree.sub(parent, "pre");
          let code = self.md.tree.sub(pre, "code");
          self.md.tree.nodes[code].text = Some(Text::atomic(format!("{}\n", escaped)));
        }
        if !rest.is_empty() {
          blocks.push_front(rest);
        }
        Some(true)
      }
      Proc::Table => {
        let (ok, border, separator) = table_test(block);
        if !ok {
          return None;
        }
        let block = blocks.pop_front().unwrap();
        self.run_table(parent, &block, border, &separator);
        Some(true)
      }
      Proc::HashHeader => {
        let (start, end, level, header) = hash_header(block)?;
        let block = blocks.pop_front().unwrap();
        let before = &block[..start];
        let after = &block[end..];
        if !before.is_empty() {
          self.parse_blocks(parent, VecDeque::from([before.to_string()]));
        }
        let h = self.md.tree.sub(parent, &format!("h{}", level));
        self.md.tree.nodes[h].text = Some(Text::plain(pystr::strip(&header)));
        if !after.is_empty() {
          let after = if self.isstate("looselist") { loose_detab(after, 1) } else { after.to_string() };
          blocks.push_front(after);
        }
        Some(true)
      }
      Proc::Setext => {
        if !setext(block) {
          return None;
        }
        let block = blocks.pop_front().unwrap();
        let lines: Vec<&str> = block.split('\n').collect();
        let level = if lines[1].starts_with('=') { 1 } else { 2 };
        let h = self.md.tree.sub(parent, &format!("h{}", level));
        self.md.tree.nodes[h].text = Some(Text::plain(pystr::strip(lines[0])));
        if lines.len() > 2 {
          blocks.push_front(lines[2..].join("\n"));
        }
        Some(true)
      }
      Proc::Hr => {
        let (start, end) = hr_search(block)?;
        let block = blocks.pop_front().unwrap();
        let prelines = block[..start].trim_end_matches('\n');
        if !prelines.is_empty() {
          self.parse_blocks(parent, VecDeque::from([prelines.to_string()]));
        }
        self.md.tree.sub(parent, "hr");
        let postlines = block[end..].trim_start_matches('\n');
        if !postlines.is_empty() {
          blocks.push_front(postlines.to_string());
        }
        Some(true)
      }
      Proc::OList | Proc::UList => {
        if self.depth > MAX_NESTING {
          return None;
        }
        let ordered = proc == Proc::OList;
        list_item(block, ordered)?;
        let block = blocks.pop_front().unwrap();
        self.run_list(parent, &block, if ordered { "ol" } else { "ul" });
        Some(true)
      }
      Proc::Quote => {
        if self.depth > MAX_NESTING {
          return None;
        }
        let start = quote_search(block)?;
        let block = blocks.pop_front().unwrap();
        let before = &block[..start];
        self.parse_blocks(parent, VecDeque::from([before.to_string()]));
        let cleaned: Vec<String> = block[start..].split('\n').map(quote_clean).collect();
        let quote = match self.last_child(parent) {
          Some(s) if self.tag(s) == "blockquote" => s,
          _ => self.md.tree.sub(parent, "blockquote"),
        };
        self.state.push("blockquote");
        self.parse_chunk(quote, &cleaned.join("\n"));
        self.state.pop();
        Some(true)
      }
      Proc::Reference => {
        let block = blocks.pop_front().unwrap();
        let Some(m) = reference_search(&block) else {
          blocks.push_front(block);
          return Some(false);
        };
        let id = pystr::strip(&m.id).to_lowercase();
        let link = m.link.trim_start_matches('<').trim_end_matches('>').to_string();
        self.md.references.insert(id, (link, m.title.filter(|t| !t.is_empty())));
        let after = &block[m.end..];
        if !strip_is_empty(after) {
          blocks.push_front(after.trim_start_matches('\n').to_string());
        }
        let before = &block[..m.start];
        if !strip_is_empty(before) {
          blocks.push_front(before.trim_end_matches('\n').to_string());
        }
        Some(true)
      }
      Proc::Paragraph => {
        let block = blocks.pop_front().unwrap();
        if !strip_is_empty(&block) {
          if self.isstate("list") {
            let tree = &mut self.md.tree;
            if let Some(sibling) = tree.last_child(parent) {
              let tail = match &tree.nodes[sibling].tail {
                Some(t) if !t.s.is_empty() => format!("{}\n{}", t.s, block),
                _ => format!("\n{}", block),
              };
              tree.nodes[sibling].tail = Some(Text::plain(tail));
            } else if tree.has_text(parent) {
              let text = format!("{}\n{}", tree.text_str(parent), block);
              tree.nodes[parent].text = Some(Text::plain(text));
            } else {
              tree.nodes[parent].text = Some(Text::plain(pystr::lstrip(&block)));
            }
          } else {
            let p = self.md.tree.sub(parent, "p");
            self.md.tree.nodes[p].text = Some(Text::plain(pystr::lstrip(&block)));
          }
        }
        Some(true)
      }
    }
  }

  fn code_sibling(&self, parent: Id) -> Option<Id> {
    let sibling = self.last_child(parent)?;
    if self.tag(sibling) == "pre" && self.md.tree.len(sibling) > 0 {
      let code = self.md.tree.nodes[sibling].children[0];
      if self.tag(code) == "code" {
        return Some(code);
      }
    }
    None
  }

  fn run_indent(&mut self, parent: Id, blocks: &mut VecDeque<String>) {
    let block = blocks.pop_front().unwrap();
    // get_level
    let indent_level = spaces(&block) / TAB;
    let mut level = if self.isstate("list") { 1 } else { 0 };
    let mut sibling = parent;
    while indent_level > level {
      match self.last_child(sibling) {
        Some(child) if matches!(self.tag(child), "ul" | "ol" | "li") => {
          if matches!(self.tag(child), "ul" | "ol") {
            level += 1;
          }
          sibling = child;
        }
        _ => break,
      }
    }
    let block = loose_detab(&block, level);
    self.state.push("detabbed");
    if self.tag(parent) == "li" {
      match self.last_child(parent) {
        Some(c) if matches!(self.tag(c), "ul" | "ol") => self.parse_blocks(c, VecDeque::from([block])),
        _ => self.parse_blocks(parent, VecDeque::from([block])),
      }
    } else if self.tag(sibling) == "li" {
      self.parse_blocks(sibling, VecDeque::from([block]));
    } else if let Some(last) = self.last_child(sibling).filter(|&c| self.tag(c) == "li") {
      if self.md.tree.has_text(last) {
        let p = self.md.tree.element("p");
        self.md.tree.nodes[p].text = self.md.tree.nodes[last].text.clone();
        self.md.tree.nodes[last].text = Some(Text::plain(""));
        self.md.tree.insert(last, 0, p);
      }
      self.parse_chunk(last, &block);
    } else {
      let li = self.md.tree.sub(sibling, "li");
      self.parse_blocks(li, VecDeque::from([block]));
    }
    self.state.pop();
  }

  fn run_list(&mut self, parent: Id, block: &str, tag: &str) {
    let mut items = get_items(block);
    let lst;
    match self.last_child(parent) {
      Some(sibling) if matches!(self.tag(sibling), "ol" | "ul") => {
        lst = sibling;
        if let Some(last) = self.last_child(lst) {
          if self.md.tree.has_text(last) {
            let p = self.md.tree.element("p");
            self.md.tree.nodes[p].text = self.md.tree.nodes[last].text.clone();
            self.md.tree.nodes[last].text = Some(Text::plain(""));
            self.md.tree.insert(last, 0, p);
          }
          if let Some(lch) = self.last_child(last)
            && !self.md.tree.tail_str(lch).is_empty()
          {
            let p = self.md.tree.sub(last, "p");
            let tail = pystr::lstrip(self.md.tree.tail_str(lch)).to_string();
            self.md.tree.nodes[p].text = Some(Text::plain(tail));
            self.md.tree.nodes[lch].tail = Some(Text::plain(""));
          }
        }
        let li = self.md.tree.sub(lst, "li");
        self.state.push("looselist");
        let first = items.remove(0);
        self.parse_blocks(li, VecDeque::from([first]));
        self.state.pop();
      }
      _ if matches!(self.tag(parent), "ol" | "ul") => lst = parent,
      _ => lst = self.md.tree.sub(parent, tag),
    }
    self.state.push("list");
    for item in items {
      if item.starts_with("    ") {
        if let Some(last) = self.last_child(lst) {
          self.parse_blocks(last, VecDeque::from([item]));
        }
      } else {
        let li = self.md.tree.sub(lst, "li");
        self.parse_blocks(li, VecDeque::from([item]));
      }
    }
    self.state.pop();
  }

  fn run_table(&mut self, parent: Id, block: &str, border: bool, separator: &[String]) {
    let lines: Vec<&str> = block.split('\n').collect();
    let header = lines[0].trim_matches(' ');
    let rows: Vec<&str> = if lines.len() < 3 { Vec::new() } else { lines[2..].to_vec() };
    let align: Vec<Option<&str>> = separator
      .iter()
      .map(|c| {
        let c = c.trim_matches(' ');
        match (c.starts_with(':'), c.ends_with(':')) {
          (true, true) => Some("center"),
          (true, false) => Some("left"),
          (false, true) => Some("right"),
          _ => None,
        }
      })
      .collect();
    let table = self.md.tree.sub(parent, "table");
    let thead = self.md.tree.sub(table, "thead");
    self.build_row(header, thead, &align, border);
    let tbody = self.md.tree.sub(table, "tbody");
    if rows.is_empty() {
      let tr = self.md.tree.sub(tbody, "tr");
      for _ in &align {
        self.md.tree.sub(tr, "td");
      }
    } else {
      for row in rows {
        self.build_row(row.trim_matches(' '), tbody, &align, border);
      }
    }
  }

  fn build_row(&mut self, row: &str, parent: Id, align: &[Option<&str>], border: bool) {
    let tr = self.md.tree.sub(parent, "tr");
    let tag = if self.tag(parent) == "thead" { "th" } else { "td" };
    let cells = split_row(row, border);
    for (i, a) in align.iter().enumerate() {
      let c = self.md.tree.sub(tr, tag);
      let text = cells.get(i).map(|s| s.trim_matches(' ').to_string()).unwrap_or_default();
      self.md.tree.nodes[c].text = Some(Text::plain(text));
      if let Some(a) = a {
        self.md.tree.set_attr(c, "style", format!("text-align: {};", a));
      }
    }
  }
}

fn detab(text: &str, length: usize) -> (String, String) {
  let lines: Vec<&str> = text.split('\n').collect();
  let prefix = " ".repeat(length);
  let mut newtext: Vec<&str> = Vec::new();
  for line in &lines {
    if let Some(rest) = line.strip_prefix(prefix.as_str()) {
      newtext.push(rest);
    } else if strip_is_empty(line) {
      newtext.push("");
    } else {
      break;
    }
  }
  let rest = lines[newtext.len()..].join("\n");
  (newtext.join("\n"), rest)
}

fn loose_detab(text: &str, level: usize) -> String {
  let prefix = " ".repeat(TAB * level);
  text.split('\n').map(|line| line.strip_prefix(prefix.as_str()).unwrap_or(line)).collect::<Vec<_>>().join("\n")
}

pub(super) fn code_escape(text: &str) -> String {
  text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn get_items(block: &str) -> Vec<String> {
  let mut items: Vec<String> = Vec::new();
  for line in block.split('\n') {
    if let Some(start) = child_item(line) {
      items.push(line[start..].to_string());
    } else if indent_item(line) {
      match items.last_mut() {
        Some(last) if last.starts_with("    ") => {
          last.push('\n');
          last.push_str(line);
        }
        _ => items.push(line.to_string()),
      }
    } else if let Some(last) = items.last_mut() {
      last.push('\n');
      last.push_str(line);
    }
  }
  items
}

// ─── Tables ─────────────────────────────────────────────────────────────────

// RE_END_BORDER: (?<!\\)(?:\\\\)*\|$
fn ends_with_border(row: &str) -> bool {
  let Some(body) = row.strip_suffix('|') else {
    return false;
  };
  let backslashes = body.bytes().rev().take_while(|&b| b == b'\\').count();
  backslashes % 2 == 0
}

fn strip_end_border(row: &str) -> &str {
  if ends_with_border(row) {
    let body = &row[..row.len() - 1];
    // The (?:\\\\)* pairs are part of the match and removed with the pipe.
    let backslashes = body.bytes().rev().take_while(|&b| b == b'\\').count();
    &body[..body.len() - backslashes]
  } else {
    row
  }
}

fn split_row(row: &str, border: bool) -> Vec<String> {
  let mut row = row;
  if border {
    if let Some(r) = row.strip_prefix('|') {
      row = r;
    }
    row = strip_end_border(row);
  }
  split_cells(row)
}

// TableProcessor._split: pipes outside code spans separate cells.
fn split_cells(row: &str) -> Vec<String> {
  let b = row.as_bytes();
  let n = b.len();
  let mut pipes = Vec::new();
  let mut tics: Vec<usize> = Vec::new();
  let mut tic_points: Vec<(usize, usize, usize)> = Vec::new();
  // (?:(\\\\)|(\\`+)|(`+)|(\\\|)|(\|))
  let mut i = 0;
  while i < n {
    if b[i] == b'\\' && b.get(i + 1) == Some(&b'\\') {
      i += 2;
    } else if b[i] == b'\\' && b.get(i + 1) == Some(&b'`') {
      let run = b[i + 1..].iter().take_while(|&&c| c == b'`').count();
      tics.push(run);
      tic_points.push((i, i + run, 1));
      i += 1 + run;
    } else if b[i] == b'`' {
      let run = b[i..].iter().take_while(|&&c| c == b'`').count();
      tics.push(run);
      tic_points.push((i, i + run - 1, 0));
      i += run;
    } else if b[i] == b'\\' && b.get(i + 1) == Some(&b'|') {
      i += 2;
    } else if b[i] == b'|' {
      pipes.push(i);
      i += 1;
    } else {
      i += 1;
    }
  }
  let mut tic_region: Vec<(usize, usize)> = Vec::new();
  let mut pos = 0;
  while pos < tics.len() {
    let tic_size = tics[pos] as isize - tic_points[pos].2 as isize;
    if tic_size == 0 {
      pos += 1;
      continue;
    }
    match tics[pos + 1..].iter().position(|&t| t as isize == tic_size) {
      Some(off) => {
        let index = off + 1;
        tic_region.push((tic_points[pos].0, tic_points[pos + index].1));
        pos += index + 1;
      }
      None => pos += 1,
    }
  }
  let mut elements = Vec::new();
  let mut start = 0;
  for pipe in pipes {
    let mut throw_out = false;
    for region in &tic_region {
      if pipe < region.0 {
        break;
      } else if region.0 <= pipe && pipe <= region.1 {
        throw_out = true;
        break;
      }
    }
    if !throw_out {
      elements.push(row[start..pipe].to_string());
      start = pipe + 1;
    }
  }
  elements.push(row[start..].to_string());
  elements
}

/// `TableProcessor.test`: (is a table, border, separator cells).
fn table_test(block: &str) -> (bool, bool, Vec<String>) {
  let rows: Vec<&str> = block.split('\n').map(|r| r.trim_matches(' ')).collect();
  if rows.len() <= 1 {
    return (false, false, Vec::new());
  }
  let header0 = rows[0];
  let border = header0.starts_with('|') || ends_with_border(header0);
  let row = split_row(header0, border);
  let row0_len = row.len();
  let mut is_table = row0_len > 1;
  if !is_table && row0_len == 1 && border {
    for r in &rows[1..] {
      is_table = r.starts_with('|') || ends_with_border(r);
      if !is_table {
        break;
      }
    }
  }
  if is_table {
    let sep = split_row(rows[1], border);
    is_table = sep.len() == row0_len && sep.concat().chars().all(|c| matches!(c, '|' | ':' | '-' | ' '));
    if is_table {
      return (true, border, sep);
    }
  }
  (false, border, Vec::new())
}
