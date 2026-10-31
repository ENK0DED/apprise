//! YAML documents as upstream sees them through PyYAML's `SafeLoader`.
//!
//! serde-saphyr parses the document: it expands aliases and `<<` merge
//! keys and enforces the size and alias limits. It types scalars by
//! YAML 1.2 rules, which differ from PyYAML's YAML 1.1 (it reads `y` as
//! true and `0612345678` as a float), so every untagged plain scalar is
//! typed again here from its source text, by PyYAML's resolvers. Quoted
//! and block scalars stay strings; explicitly tagged ones keep the type
//! their tag gives.

use std::cell::RefCell;
use std::collections::HashMap;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_saphyr::{Spanned, Tagged};

/// Scalar text a document may produce, aliases replayed included. The
/// input itself is at most a few hundred KB (`MAX_BUFFER_SIZE`), so only
/// alias amplification gets near this.
const MAX_LOADED_BYTES: usize = 16 * 1024 * 1024;

/// What one node costs on top of its text, so node counts are bounded too.
const NODE_COST: usize = 16;

/// A YAML value as PyYAML builds it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
  Null,
  Bool(bool),
  /// Python integers are unbounded; larger ones are kept as their text.
  Int(i128),
  Float(f64),
  String(String),
  Sequence(Vec<Value>),
  Mapping(Mapping),
}

impl Value {
  pub fn as_str(&self) -> Option<&str> {
    match self {
      Value::String(s) => Some(s),
      _ => None,
    }
  }

  pub fn is_null(&self) -> bool {
    matches!(self, Value::Null)
  }

  /// Python's `str()` of the value, for messages.
  pub fn python_str(&self) -> String {
    match self {
      Value::String(s) => s.clone(),
      other => other.python_repr(),
    }
  }

  /// Python's `repr()` of the value.
  fn python_repr(&self) -> String {
    match self {
      Value::Null => "None".to_string(),
      Value::Bool(true) => "True".to_string(),
      Value::Bool(false) => "False".to_string(),
      Value::Int(n) => n.to_string(),
      Value::Float(f) => python_float(*f),
      Value::String(s) if s.contains('\'') && !s.contains('"') => format!("\"{}\"", s.replace('\\', "\\\\")),
      Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
      Value::Sequence(seq) => format!("[{}]", seq.iter().map(Value::python_repr).collect::<Vec<_>>().join(", ")),
      Value::Mapping(m) => format!("{{{}}}", m.iter().map(|(k, v)| format!("{}: {}", k.python_repr(), v.python_repr())).collect::<Vec<_>>().join(", ")),
    }
  }
}

/// Python's `str()` of a float: `1.5`, `2.0`, `1e+16`, `inf`.
pub fn python_float(f: f64) -> String {
  if f.is_nan() {
    return "nan".to_string();
  }
  if f.is_infinite() {
    return if f > 0.0 { "inf" } else { "-inf" }.to_string();
  }
  // Rust's Debug output is the shortest form that reads back, switching
  // to an exponent where Python does; Python writes the exponent with a
  // sign and at least two digits.
  let s = format!("{:?}", f);
  match s.split_once('e') {
    Some((mantissa, exp)) => {
      let (sign, digits) = exp.strip_prefix('-').map_or(("+", exp), |d| ("-", d));
      format!("{}e{}{:0>2}", mantissa, sign, digits)
    }
    None => s,
  }
}

/// A mapping in document order. Like a Python dict, a repeated key keeps
/// its first position and its last value, and `1`, `1.0` and `true` are
/// the same key.
#[derive(Debug, Clone, Default)]
pub struct Mapping {
  entries: Vec<(Value, Value)>,
  index: HashMap<Key, usize>,
}

/// A scalar key as Python compares it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
  Null,
  Int(i128),
  Float(u64),
  Str(String),
}

impl Key {
  /// `None` for sequences and mappings, which Python cannot hash.
  fn of(v: &Value) -> Option<Key> {
    Some(match v {
      Value::Null => Key::Null,
      Value::Bool(b) => Key::Int(i128::from(*b)),
      Value::Int(n) => Key::Int(*n),
      Value::Float(f) if f.fract() == 0.0 && f.abs() < 1e30 => Key::Int(*f as i128),
      Value::Float(f) => Key::Float(f.to_bits()),
      Value::String(s) => Key::Str(s.clone()),
      Value::Sequence(_) | Value::Mapping(_) => return None,
    })
  }
}

impl Mapping {
  pub fn new() -> Self {
    Self::default()
  }

  /// Python's `mapping[key] = value`.
  pub fn insert(&mut self, key: Value, value: Value) {
    match Key::of(&key) {
      Some(k) => match self.index.get(&k) {
        Some(&at) => self.entries[at].1 = value,
        None => {
          self.index.insert(k, self.entries.len());
          self.entries.push((key, value));
        }
      },
      None => self.entries.push((key, value)),
    }
  }

  /// The value of the string key `key`.
  pub fn get(&self, key: &str) -> Option<&Value> {
    self.index.get(&Key::Str(key.to_string())).map(|&at| &self.entries[at].1)
  }

  pub fn iter(&self) -> impl Iterator<Item = (&Value, &Value)> {
    self.entries.iter().map(|(k, v)| (k, v))
  }

  pub fn keys(&self) -> impl Iterator<Item = &Value> {
    self.entries.iter().map(|(k, _)| k)
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }
}

impl PartialEq for Mapping {
  fn eq(&self, other: &Self) -> bool {
    self.entries == other.entries
  }
}

impl FromIterator<(Value, Value)> for Mapping {
  fn from_iter<I: IntoIterator<Item = (Value, Value)>>(iter: I) -> Self {
    let mut m = Mapping::new();
    for (k, v) in iter {
      m.insert(k, v);
    }
    m
  }
}

impl<'a> IntoIterator for &'a Mapping {
  type Item = (&'a Value, &'a Value);
  type IntoIter = std::iter::Map<std::slice::Iter<'a, (Value, Value)>, fn(&'a (Value, Value)) -> (&'a Value, &'a Value)>;

  fn into_iter(self) -> Self::IntoIter {
    self.entries.iter().map(|(k, v)| (k, v))
  }
}

// ── Loading ──────────────────────────────────────────────────────────────

/// The document being loaded: its text, for the scalars' source, and what
/// has been built from it so far.
struct Loading {
  source: String,
  loaded: usize,
}

thread_local! {
  static LOADING: RefCell<Option<Loading>> = const { RefCell::new(None) };
}

/// Restores the outer document (if any) when a load ends, even by panic.
struct LoadingGuard(Option<Loading>);

impl Drop for LoadingGuard {
  fn drop(&mut self) {
    LOADING.with(|l| *l.borrow_mut() = self.0.take());
  }
}

/// Load a single YAML document the way upstream's
/// `yaml.load(content, Loader=yaml.SafeLoader)` does. The error is a
/// one-line message with its position; it quotes no document text.
pub fn load(content: &str) -> Result<Value, String> {
  let previous = LOADING.with(|l| l.borrow_mut().replace(Loading { source: content.to_string(), loaded: 0 }));
  let _guard = LoadingGuard(previous);

  let mut options = serde_saphyr::Options::default();
  // PyYAML keeps the last value of a repeated key
  options.duplicate_keys = serde_saphyr::DuplicateKeyPolicy::LastWins;
  // PyYAML's SafeLoader refuses tags it has no constructor for
  options.reject_unsupported_tags = true;
  // `.inf` and `.nan` are floats (typed again below)
  options.non_finite_float_policy = serde_saphyr::NonFiniteFloatPolicy::PassThrough;
  // Errors must not echo the document, which can hold credentials
  options.with_snippet = false;
  options.alias_limits.max_total_replayed_events = 100_000;
  options.alias_limits.max_alias_expansions_per_anchor = 10_000;

  serde_saphyr::from_str_with_options::<Node>(content, options).map(|n| n.0).map_err(|e| e.to_string())
}

/// One node, typed like PyYAML.
struct Node(Value);

impl<'de> Deserialize<'de> for Node {
  fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    let spanned: Spanned<Tagged<Parsed>> = Spanned::deserialize(d)?;
    let Tagged(parsed, tag) = spanned.value;
    let value = match parsed {
      Parsed::Node(v) => v,
      scalar if tag.is_some() => scalar.into_value(),
      scalar => {
        let span = spanned.defined.span();
        let range = span.byte_offset().zip(span.byte_len()).map(|(at, len)| at as usize..(at + len) as usize);
        let typed = LOADING.with(|l| l.borrow().as_ref().and_then(|l| range.and_then(|r| l.source.get(r))).and_then(retype));
        typed.unwrap_or_else(|| scalar.into_value())
      }
    };
    let cost = NODE_COST + if let Value::String(s) = &value { s.len() } else { 0 };
    let over = LOADING.with(|l| {
      l.borrow_mut().as_mut().is_some_and(|l| {
        l.loaded += cost;
        l.loaded > MAX_LOADED_BYTES
      })
    });
    if over {
      return Err(de::Error::custom("the document expands beyond the size limit"));
    }
    Ok(Node(value))
  }
}

/// A plain scalar's type from its source text, by PyYAML's rules; `None`
/// for quoted, block and multi-line scalars, which are strings already.
fn retype(raw: &str) -> Option<Value> {
  if raw.starts_with(['\'', '"', '|', '>']) || raw.contains(['\n', '\r']) {
    return None;
  }
  Some(resolve_plain(raw))
}

/// A node as serde-saphyr typed it.
enum Parsed {
  Null,
  Bool(bool),
  Int(i128),
  Float(f64),
  Str(String),
  Node(Value),
}

impl Parsed {
  fn into_value(self) -> Value {
    match self {
      Parsed::Null => Value::Null,
      Parsed::Bool(b) => Value::Bool(b),
      Parsed::Int(n) => Value::Int(n),
      Parsed::Float(f) => Value::Float(f),
      Parsed::Str(s) => Value::String(s),
      Parsed::Node(v) => v,
    }
  }
}

impl<'de> Deserialize<'de> for Parsed {
  fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    d.deserialize_any(ParsedVisitor)
  }
}

struct ParsedVisitor;

impl<'de> Visitor<'de> for ParsedVisitor {
  type Value = Parsed;

  fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("any YAML value")
  }
  fn visit_bool<E: de::Error>(self, v: bool) -> Result<Parsed, E> {
    Ok(Parsed::Bool(v))
  }
  fn visit_i64<E: de::Error>(self, v: i64) -> Result<Parsed, E> {
    Ok(Parsed::Int(v.into()))
  }
  fn visit_u64<E: de::Error>(self, v: u64) -> Result<Parsed, E> {
    Ok(Parsed::Int(v.into()))
  }
  fn visit_i128<E: de::Error>(self, v: i128) -> Result<Parsed, E> {
    Ok(Parsed::Int(v))
  }
  fn visit_f64<E: de::Error>(self, v: f64) -> Result<Parsed, E> {
    Ok(Parsed::Float(v))
  }
  fn visit_str<E: de::Error>(self, v: &str) -> Result<Parsed, E> {
    Ok(Parsed::Str(v.to_string()))
  }
  fn visit_string<E: de::Error>(self, v: String) -> Result<Parsed, E> {
    Ok(Parsed::Str(v))
  }
  fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Parsed, E> {
    // `!!binary`
    Ok(Parsed::Str(String::from_utf8_lossy(v).into_owned()))
  }
  fn visit_unit<E: de::Error>(self) -> Result<Parsed, E> {
    Ok(Parsed::Null)
  }
  fn visit_none<E: de::Error>(self) -> Result<Parsed, E> {
    Ok(Parsed::Null)
  }
  fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Parsed, D::Error> {
    Node::deserialize(d).map(|n| Parsed::Node(n.0))
  }
  fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Parsed, A::Error> {
    let mut out = Vec::new();
    while let Some(Node(v)) = seq.next_element()? {
      out.push(v);
    }
    Ok(Parsed::Node(Value::Sequence(out)))
  }
  fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Parsed, A::Error> {
    let mut out = Mapping::new();
    while let Some((Node(k), Node(v))) = map.next_entry()? {
      if Key::of(&k).is_none() {
        return Err(de::Error::custom("found unhashable key"));
      }
      out.insert(k, v);
    }
    Ok(Parsed::Node(Value::Mapping(out)))
  }
}

// ── PyYAML's YAML 1.1 implicit resolvers (yaml/resolver.py) ─────────────────

/// Type a plain scalar: null, bool, int, float, else a string
/// (timestamps stay text).
fn resolve_plain(text: &str) -> Value {
  match text {
    "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
    "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => return Value::Bool(true),
    "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => return Value::Bool(false),
    _ => {}
  }
  if is_yaml11_int(text) {
    // Larger than Python needs to be unusual; keep those as text
    return construct_int(text).map_or_else(|| Value::String(text.to_string()), Value::Int);
  }
  if is_yaml11_float(text) {
    return construct_float(text).map_or_else(|| Value::String(text.to_string()), Value::Float);
  }
  Value::String(text.to_string())
}

fn split_sign(text: &str) -> (bool, &str) {
  match text.as_bytes().first() {
    Some(b'-') => (true, &text[1..]),
    Some(b'+') => (false, &text[1..]),
    _ => (false, text),
  }
}

fn all(s: &str, f: impl Fn(u8) -> bool) -> bool {
  s.bytes().all(f)
}

/// `[0-5]?[0-9]` after each `:` of a sexagesimal number.
fn is_base60_tail(parts: &[&str]) -> bool {
  !parts.is_empty()
    && parts.iter().all(|p| match p.as_bytes() {
      [d] => d.is_ascii_digit(),
      [a, b] => (b'0'..=b'5').contains(a) && b.is_ascii_digit(),
      _ => false,
    })
}

/// `[-+]?0b[0-1_]+ | [-+]?0[0-7_]+ | [-+]?(?:0|[1-9][0-9_]*) |
/// [-+]?0x[0-9a-fA-F_]+ | [-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+`
fn is_yaml11_int(text: &str) -> bool {
  let (_, s) = split_sign(text);
  let digits_ = |s: &str| all(s, |b| b.is_ascii_digit() || b == b'_');
  if let Some(rest) = s.strip_prefix("0b") {
    return !rest.is_empty() && all(rest, |b| matches!(b, b'0' | b'1' | b'_'));
  }
  if let Some(rest) = s.strip_prefix("0x") {
    return !rest.is_empty() && all(rest, |b| b.is_ascii_hexdigit() || b == b'_');
  }
  if s == "0" {
    return true;
  }
  if let Some(rest) = s.strip_prefix('0') {
    return !rest.is_empty() && all(rest, |b| (b'0'..=b'7').contains(&b) || b == b'_');
  }
  let mut parts = s.split(':');
  let head = parts.next().unwrap_or_default();
  let tail: Vec<&str> = parts.collect();
  let head_ok = head.as_bytes().first().is_some_and(|b| (b'1'..=b'9').contains(b)) && digits_(head);
  head_ok && (tail.is_empty() || is_base60_tail(&tail))
}

/// `[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)? |
/// \.[0-9][0-9_]*(?:[eE][-+][0-9]+)? |
/// [-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]* |
/// [-+]?\.(?:inf|Inf|INF) | \.(?:nan|NaN|NAN)`
fn is_yaml11_float(text: &str) -> bool {
  if matches!(text, ".nan" | ".NaN" | ".NAN") {
    return true;
  }
  let (_, s) = split_sign(text);
  if matches!(s, ".inf" | ".Inf" | ".INF") {
    return true;
  }
  let digits_ = |s: &str| all(s, |b| b.is_ascii_digit() || b == b'_');
  let exponent_ok = |e: &str| {
    let bytes = e.as_bytes();
    matches!(bytes.first(), Some(b'e' | b'E')) && matches!(bytes.get(1), Some(b'+' | b'-')) && bytes.len() > 2 && all(&e[2..], |b| b.is_ascii_digit())
  };
  let Some((int_part, frac)) = s.split_once('.') else { return false };
  // The fraction and the optional exponent
  let (frac, exp) = match frac.find(['e', 'E']) {
    Some(at) => (&frac[..at], Some(&frac[at..])),
    None => (frac, None),
  };
  if !digits_(frac) || exp.is_some_and(|e| !exponent_ok(e)) {
    return false;
  }
  if int_part.is_empty() {
    // `.5`: no sign allowed, a digit first
    return text == s && frac.as_bytes().first().is_some_and(u8::is_ascii_digit);
  }
  let mut parts = int_part.split(':');
  let head = parts.next().unwrap_or_default();
  let tail: Vec<&str> = parts.collect();
  let head_ok = head.as_bytes().first().is_some_and(u8::is_ascii_digit) && digits_(head);
  // Sexagesimal floats take no exponent
  head_ok && (tail.is_empty() || (exp.is_none() && is_base60_tail(&tail)))
}

/// PyYAML `construct_yaml_int`.
fn construct_int(text: &str) -> Option<i128> {
  let cleaned = text.replace('_', "");
  let (negative, s) = split_sign(&cleaned);
  let magnitude = if s == "0" {
    0
  } else if let Some(bin) = s.strip_prefix("0b") {
    i128::from_str_radix(bin, 2).ok()?
  } else if let Some(hex) = s.strip_prefix("0x") {
    i128::from_str_radix(hex, 16).ok()?
  } else if s.starts_with('0') {
    i128::from_str_radix(s, 8).ok()?
  } else if s.contains(':') {
    s.split(':').try_fold(0i128, |acc, part| acc.checked_mul(60)?.checked_add(part.parse::<i128>().ok()?))?
  } else {
    s.parse::<i128>().ok()?
  };
  Some(if negative { -magnitude } else { magnitude })
}

/// PyYAML `construct_yaml_float`.
fn construct_float(text: &str) -> Option<f64> {
  let cleaned = text.replace('_', "").to_ascii_lowercase();
  let (negative, s) = split_sign(&cleaned);
  let magnitude = match s {
    ".inf" => f64::INFINITY,
    ".nan" => f64::NAN,
    _ if s.contains(':') => s.split(':').try_fold(0f64, |acc, part| Some(acc * 60.0 + part.parse::<f64>().ok()?))?,
    // Python's float() takes `1.` and `1.e+5`; Rust's parser does not
    _ => s.replace(".e", ".0e").trim_end_matches('.').parse::<f64>().ok()?,
  };
  Some(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
mod tests {
  use super::*;

  fn map(content: &str) -> Mapping {
    match load(content).unwrap() {
      Value::Mapping(m) => m,
      other => panic!("not a mapping: {:?}", other),
    }
  }

  fn scalar(text: &str) -> Value {
    map(&format!("v: {}\n", text)).get("v").unwrap().clone()
  }

  fn s(v: &str) -> Value {
    Value::String(v.to_string())
  }

  // Each expectation checked against PyYAML 6 (yaml.safe_load).
  #[test]
  fn test_plain_scalars_follow_yaml_1_1() {
    let cases: &[(&str, Value)] = &[
      ("yes", Value::Bool(true)),
      ("Yes", Value::Bool(true)),
      ("ON", Value::Bool(true)),
      ("True", Value::Bool(true)),
      ("off", Value::Bool(false)),
      ("No", Value::Bool(false)),
      ("FALSE", Value::Bool(false)),
      // Not booleans in PyYAML
      ("y", s("y")),
      ("n", s("n")),
      ("Y", s("Y")),
      ("yEs", s("yEs")),
      ("tRUE", s("tRUE")),
      ("~", Value::Null),
      ("null", Value::Null),
      ("NULL", Value::Null),
      ("nUll", s("nUll")),
      ("0", Value::Int(0)),
      ("42", Value::Int(42)),
      ("-17", Value::Int(-17)),
      ("+5", Value::Int(5)),
      ("1_000", Value::Int(1000)),
      ("0755", Value::Int(493)),
      ("-0755", Value::Int(-493)),
      ("00", Value::Int(0)),
      ("0x1F", Value::Int(31)),
      ("0x_1f", Value::Int(31)),
      ("0b1010", Value::Int(10)),
      ("1:30", Value::Int(90)),
      ("190:20:30", Value::Int(685_230)),
      ("123456789012345678901234567890", Value::Int(123_456_789_012_345_678_901_234_567_890)),
      // Phone numbers and other digit strings that are not YAML 1.1 numbers
      ("0612345678", s("0612345678")),
      ("0o17", s("0o17")),
      ("0X1F", s("0X1F")),
      ("1:60", s("1:60")),
      ("1e5", s("1e5")),
      ("1.0e5", s("1.0e5")),
      ("1e999", s("1e999")),
      ("+.5", s("+.5")),
      (".", s(".")),
      ("2001-12-14", s("2001-12-14")),
      ("12.50", Value::Float(12.5)),
      ("1.", Value::Float(1.0)),
      (".5", Value::Float(0.5)),
      ("-1_0.5", Value::Float(-10.5)),
      ("1.0e+5", Value::Float(100_000.0)),
      ("1.e-2", Value::Float(0.01)),
      ("1:30.5", Value::Float(90.5)),
      (".inf", Value::Float(f64::INFINITY)),
      ("-.Inf", Value::Float(f64::NEG_INFINITY)),
      ("abc", s("abc")),
    ];
    for (text, want) in cases {
      assert_eq!(&scalar(text), want, "{}", text);
    }
    assert!(matches!(scalar(".NaN"), Value::Float(f) if f.is_nan()));
    assert_eq!(scalar("-.nan"), s("-.nan"));
  }

  #[test]
  fn test_quoted_block_and_tagged_scalars() {
    assert_eq!(scalar("'yes'"), s("yes"));
    assert_eq!(scalar("\"0755\""), s("0755"));
    assert_eq!(scalar("|\n  yes\n"), s("yes\n"));
    assert_eq!(scalar("plain\n  yes"), s("plain yes"));
    assert_eq!(scalar("!!str 123"), s("123"));
    assert_eq!(scalar("!!str yes"), s("yes"));
    assert_eq!(scalar("! 123"), s("123"));
    assert_eq!(scalar("!!int \"12\""), Value::Int(12));
    assert_eq!(scalar("!!bool yes"), Value::Bool(true));
    assert_eq!(scalar(""), Value::Null);
    // Flow collections, aliases and keys are typed the same way
    assert_eq!(scalar("[yes, y, 0755, 'on']"), Value::Sequence(vec![Value::Bool(true), s("y"), Value::Int(493), s("on")]));
    let m = map("a: &x yes\nb: *x\ny: 1\n");
    assert_eq!(m.get("b"), Some(&Value::Bool(true)));
    assert_eq!(m.get("y"), Some(&Value::Int(1)));
  }

  #[test]
  fn test_mappings_behave_like_python_dicts() {
    let m = map("a: 1\nb: 2\na: 3\n");
    assert_eq!(m.keys().cloned().collect::<Vec<_>>(), vec![s("a"), s("b")]);
    assert_eq!(m.get("a"), Some(&Value::Int(3)));
    // yes, true and 1 are one key; 1.0 too
    let m = map("yes: a\ntrue: b\n1: c\n1.0: d\nno: e\n");
    assert_eq!(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<Vec<_>>(), vec![(Value::Bool(true), s("d")), (Value::Bool(false), s("e"))]);
    // Merge keys: explicit keys win wherever they are; earlier merges win
    let m = map("base: &b {tag: shared, format: html}\nx:\n  format: text\n  <<: *b\n  retry: 1\ny:\n  <<: [*b, {tag: second, retry: 4}]\n");
    let Some(Value::Mapping(x)) = m.get("x") else { panic!() };
    assert_eq!(x.get("format"), Some(&s("text")));
    assert_eq!(x.get("tag"), Some(&s("shared")));
    let Some(Value::Mapping(y)) = m.get("y") else { panic!() };
    assert_eq!(y.get("tag"), Some(&s("shared")));
    assert_eq!(y.get("retry"), Some(&Value::Int(4)));
  }

  #[test]
  fn test_documents_upstream_refuses() {
    for content in [
      // Unknown tags, unhashable keys
      "a: !custom mine\n",
      "? [x]\n: y\n",
      "? {a: 1}\n: y\n",
      // Syntax errors
      "{{{{invalid yaml",
      "a: [1, 2\n",
      "a: 'unterminated\n",
      "a: b: c\n",
      // More than one document
      "a: 1\n---\nb: 2\n",
      // An alias to nothing
      "a: *missing\n",
    ] {
      assert!(load(content).is_err(), "{}", content);
    }
    assert_eq!(load("").unwrap(), Value::Null);
    assert_eq!(load("# only a comment\n").unwrap(), Value::Null);
  }

  #[test]
  fn test_errors_do_not_quote_the_document() {
    let err = load("urls:\n  - json://secret-token@localhost: !custom x\n").unwrap_err();
    assert!(!err.contains("secret-token"), "{}", err);
    let err = load("urls:\n  - json://secret-token@localhost\n bad: [").unwrap_err();
    assert!(!err.contains("secret-token"), "{}", err);
  }

  #[test]
  fn test_alias_bombs_are_refused() {
    // The classic exponential expansion
    let mut doc = String::from("a: &a [x, x, x, x, x, x, x, x, x, x]\n");
    for (prev, name) in ["a", "b", "c", "d", "e", "f", "g", "h", "i"].iter().zip(["b", "c", "d", "e", "f", "g", "h", "i", "j"]) {
      doc.push_str(&format!("{name}: &{name} [*{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}, *{prev}]\n"));
    }
    assert!(load(&doc).is_err());
    // Few events, but long text repeated
    let big = "x".repeat(100_000);
    let mut doc = format!("a: &a [\"{big}\", \"{big}\", \"{big}\", \"{big}\", \"{big}\"]\nb: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\n");
    doc.push_str("c: [*b, *b, *b, *b, *b, *b, *b, *b, *b, *b]\n");
    assert!(load(&doc).is_err());
    // Ordinary reuse is fine
    assert!(load("base: &b {tag: x}\nurls:\n  - json://a: *b\n  - json://b: *b\n").is_ok());
  }

  #[test]
  fn test_python_formatting() {
    assert_eq!(python_float(1.5), "1.5");
    assert_eq!(python_float(2.0), "2.0");
    assert_eq!(python_float(100_000.0), "100000.0");
    assert_eq!(python_float(1e16), "1e+16");
    assert_eq!(python_float(1.5e-7), "1.5e-07");
    assert_eq!(python_float(0.0001), "0.0001");
    assert_eq!(python_float(f64::INFINITY), "inf");
    assert_eq!(Value::Sequence(vec![Value::Null, Value::Bool(true), s("it's"), s("a'\"b")]).python_str(), "[None, True, \"it's\", 'a\\'\"b']");
  }
}
