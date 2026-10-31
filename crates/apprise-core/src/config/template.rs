//! `${NAME}` template variables in YAML configuration, ported from
//! upstream `apprise/utils/template.py`, `apprise/utils/yaml.py` and
//! `apprise/template.py`.
//!
//! A YAML `template:` section declares variable names, optionally with a
//! default. Declared `${NAME}` markers in URLs and settings are swapped for
//! random placeholders made of URL-safe characters before the URL is
//! parsed, so a value can never change the URL's structure: it is
//! substituted into the parsed fields afterwards. The schema itself may
//! never be templated, and neither may tags. An entry that still needs a
//! value is held back as a [`PendingService`] until one is supplied by the
//! caller, the configuration default or `APPRISE_TEMPLATE_<NAME>`, in that
//! order.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

use indexmap::IndexMap;
use regex_lite::Regex;

use crate::notify::Notify;
use crate::notify::registry;
use crate::utils::parse::{ParsedUrl, unquote};

/// Prefix used to look a variable up in the environment.
pub const TEMPLATE_ENV_PREFIX: &str = "APPRISE_TEMPLATE_";

/// Longest variable name accepted.
pub const TEMPLATE_NAME_MAXLEN: usize = 32;

/// Longest value a variable may resolve to.
pub const MAX_TEMPLATE_VALUE_LEN: usize = 1024;

/// Longest URL built once every value has been filled in.
pub const MAX_RESOLVED_URL_LEN: usize = 8192;

/// How many random prefixes to try before giving up.
const PLACEHOLDER_ATTEMPTS: usize = 8;

/// How many services built from one entry are kept for reuse.
pub const MAX_RESOLVE_CACHE: usize = 32;

/// A template problem; the message never contains a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
  pub message: String,
  /// The variable concerned, when there is one.
  pub variable: Option<String>,
}

impl TemplateError {
  fn new(message: impl Into<String>) -> Self {
    Self { message: message.into(), variable: None }
  }

  fn for_var(message: impl Into<String>, name: &str) -> Self {
    Self { message: message.into(), variable: Some(name.to_string()) }
  }
}

impl std::fmt::Display for TemplateError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.message)
  }
}

/// `${NAME}` with a 1-32 character name.
fn template_var_re() -> &'static Regex {
  static RE: OnceLock<Regex> = OnceLock::new();
  RE.get_or_init(|| Regex::new(r"\$\{(?P<name>[A-Za-z0-9_]{1,32})\}").unwrap())
}

/// Whether `name` is a valid variable name on its own.
pub fn is_template_name(name: &str) -> bool {
  (1..=TEMPLATE_NAME_MAXLEN).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Names are matched without regard to case.
pub fn normalize_name(name: &str) -> String {
  name.to_lowercase()
}

/// Check a value and return it as text (upstream `validate_value()`).
pub fn validate_value(name: &str, value: &str) -> Result<String, TemplateError> {
  if value.chars().count() > MAX_TEMPLATE_VALUE_LEN {
    return Err(TemplateError::for_var(format!("Template variable '{}' exceeds {} characters.", name, MAX_TEMPLATE_VALUE_LEN), name));
  }
  if value.chars().any(|c| ('\x00'..='\x1f').contains(&c) || c == '\x7f' || c == '\u{2028}' || c == '\u{2029}') {
    return Err(TemplateError::for_var(format!("Template variable '{}' contains a control character.", name), name));
  }
  Ok(value.to_string())
}

/// Check caller-supplied values (upstream `validate_overrides()`): names
/// must be valid and may not repeat in another case.
pub fn validate_overrides(values: &HashMap<String, String>) -> Result<(), TemplateError> {
  let mut seen = BTreeSet::new();
  for key in values.keys() {
    if !is_template_name(key) {
      return Err(TemplateError::new(format!("Invalid template variable name {:?}.", key)));
    }
    if !seen.insert(normalize_name(key)) {
      let name = normalize_name(key);
      return Err(TemplateError::for_var(format!("Template variable '{}' was supplied more than once.", name), &name));
    }
  }
  Ok(())
}

/// Every variable a configuration declares, with its default (`None`
/// marks a required value; an empty string is a valid default).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TemplateSchema {
  pub variables: IndexMap<String, Option<String>>,
}

impl TemplateSchema {
  /// Read a `template:` section: a list of `- name` / `- name: default`
  /// entries, or a mapping of `name: default`.
  #[cfg(feature = "yaml")]
  pub fn parse(entries: &serde_yaml::Value) -> Result<Self, TemplateError> {
    use serde_yaml::Value;

    let mut items: Vec<(Value, Value)> = Vec::new();
    match entries {
      Value::Mapping(m) => items.extend(m.iter().map(|(k, v)| (k.clone(), v.clone()))),
      Value::Sequence(seq) => {
        for entry in seq {
          match entry {
            Value::String(_) => items.push((entry.clone(), Value::Null)),
            Value::Mapping(m) => items.extend(m.iter().map(|(k, v)| (k.clone(), v.clone()))),
            _ => return Err(TemplateError::new("Invalid template declaration entry.")),
          }
        }
      }
      _ => return Err(TemplateError::new("The template section must be a list or a mapping.")),
    }

    let mut variables = IndexMap::new();
    for (name, default) in items {
      let name = match &name {
        Value::String(s) if is_template_name(s) => s.clone(),
        other => return Err(TemplateError::new(format!("Invalid template variable name {}.", yaml_repr(other)))),
      };
      let key = normalize_name(&name);
      if variables.contains_key(&key) {
        return Err(TemplateError::for_var(format!("Template variable '{}' is declared more than once.", key), &key));
      }
      let default = match default {
        Value::Null => None,
        Value::String(s) => Some(validate_value(&key, &s)?),
        Value::Bool(b) => Some(if b { "yes" } else { "no" }.to_string()),
        Value::Number(n) => Some(validate_value(&key, &n.to_string())?),
        _ => return Err(TemplateError::for_var(format!("Template variable '{}' must be text.", key), &key)),
      };
      variables.insert(key, default);
    }
    Ok(Self { variables })
  }

  /// Declared names in declaration order.
  pub fn names(&self) -> Vec<String> {
    self.variables.keys().cloned().collect()
  }

  pub fn is_empty(&self) -> bool {
    self.variables.is_empty()
  }

  pub fn contains(&self, name: &str) -> bool {
    self.variables.contains_key(&normalize_name(name))
  }

  /// Whether a declared name has no default.
  pub fn is_required(&self, name: &str) -> bool {
    matches!(self.variables.get(name), Some(None))
  }
}

#[cfg(feature = "yaml")]
fn yaml_repr(v: &serde_yaml::Value) -> String {
  serde_yaml::to_string(v).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Swaps declared variables for parse-safe placeholders and back
/// (upstream `TemplatePlaceholderMap`). Placeholders look like
/// `a<16 hex>t<index>z`, with a prefix that does not already occur in the
/// configuration.
#[derive(Debug)]
pub struct PlaceholderMap {
  pub schema: TemplateSchema,
  nonce: String,
  /// placeholder -> variable name
  placeholders: std::sync::Mutex<PlaceholderTables>,
  pattern: Regex,
}

#[derive(Debug, Default)]
struct PlaceholderTables {
  by_token: HashMap<String, String>,
  by_name: HashMap<String, String>,
}

impl PlaceholderMap {
  pub fn new(schema: TemplateSchema, content: &str) -> Result<Self, TemplateError> {
    for _ in 0..PLACEHOLDER_ATTEMPTS {
      let nonce: String = (0..8).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
      if !content.contains(&format!("a{}", nonce)) {
        let pattern = Regex::new(&format!(r"a{}t[0-9]{{1,10}}z", nonce)).unwrap();
        return Ok(Self { schema, nonce, placeholders: Default::default(), pattern });
      }
    }
    Err(TemplateError::new("Could not prepare template variable placeholders."))
  }

  fn placeholder(&self, name: &str) -> String {
    let mut tables = self.placeholders.lock().unwrap();
    if let Some(token) = tables.by_name.get(name) {
      return token.clone();
    }
    let token = format!("a{}t{}z", self.nonce, tables.by_name.len());
    tables.by_name.insert(name.to_string(), token.clone());
    tables.by_token.insert(token.clone(), name.to_string());
    token
  }

  fn name_of(&self, token: &str) -> Option<String> {
    self.placeholders.lock().unwrap().by_token.get(token).cloned()
  }

  /// Replace declared `${NAME}` markers; undeclared ones stay as written.
  pub fn encode(&self, text: &str) -> String {
    if !text.contains("${") {
      return text.to_string();
    }
    template_var_re()
      .replace_all(text, |c: &regex_lite::Captures<'_>| {
        let name = normalize_name(&c["name"]);
        if self.schema.variables.contains_key(&name) { self.placeholder(&name) } else { c[0].to_string() }
      })
      .into_owned()
  }

  /// Replace declared markers after the URL's `://` only, so the schema
  /// stays literal.
  pub fn encode_url(&self, url: &str) -> String {
    match url.split_once("://") {
      Some((schema, rest)) => format!("{}://{}", schema, self.encode(rest)),
      None => url.to_string(),
    }
  }

  /// The variable names whose placeholders appear in `text`.
  pub fn used_in(&self, text: &str, found: &mut BTreeSet<String>) {
    for m in self.pattern.find_iter(text) {
      if let Some(name) = self.name_of(m.as_str()) {
        found.insert(name);
      }
    }
  }

  /// The variable names a parsed URL still needs.
  pub fn used(&self, parsed: &ParsedUrl) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for field in [&parsed.user, &parsed.password, &parsed.host].into_iter().flatten() {
      self.used_in(field, &mut found);
    }
    self.used_in(&parsed.path, &mut found);
    for (k, v) in parsed.qsd.iter().chain(parsed.qsd_add.iter()).chain(parsed.qsd_del.iter()).chain(parsed.qsd_colon.iter()) {
      self.used_in(k, &mut found);
      self.used_in(v, &mut found);
    }
    found
  }

  /// Put the real values in where the placeholders sit. Only the original
  /// text is scanned, so a value that looks like a placeholder stays as is.
  fn fill(&self, text: &str, values: &HashMap<String, String>, decode: bool) -> String {
    self
      .pattern
      .replace_all(text, |c: &regex_lite::Captures<'_>| {
        let token = &c[0];
        match self.name_of(token).and_then(|n| values.get(&n)) {
          Some(v) if decode => unquote(v),
          Some(v) => v.clone(),
          None => token.to_string(),
        }
      })
      .into_owned()
  }

  /// Restore the `${NAME}` spelling for display.
  pub fn display(&self, text: &str) -> String {
    self
      .pattern
      .replace_all(text, |c: &regex_lite::Captures<'_>| self.name_of(&c[0]).map(|n| format!("${{{}}}", n.to_uppercase())).unwrap_or_else(|| c[0].to_string()))
      .into_owned()
  }

  /// Substitute values into every parsed field (upstream
  /// `TemplatePlaceholderMap.substitute()`).
  ///
  /// User, password and host are decoded after filling, as upstream's
  /// `URLBase` decodes them; the path is re-split into segments; query
  /// names and values are filled as given. A marker that owns the whole
  /// host may carry `user[:password]@host`, and one that owns the whole
  /// user may carry `user:password`.
  pub fn substitute(&self, parsed: &ParsedUrl, values: &HashMap<String, String>) -> ParsedUrl {
    let mut out = parsed.clone();
    let fill = |s: &Option<String>| s.as_ref().map(|v| self.fill(v, values, true));
    out.user = fill(&parsed.user);
    out.password = fill(&parsed.password);
    out.host = fill(&parsed.host);

    let decoded = |s: &str| Some(unquote(s)).filter(|s| !s.is_empty());
    let whole = |field: &Option<String>| -> Option<String> {
      let f = field.as_deref()?;
      let m = self.pattern.find(f)?;
      (m.start() == 0 && m.end() == f.len()).then(|| self.name_of(f)).flatten().and_then(|n| values.get(&n).cloned())
    };

    if parsed.user.is_none()
      && parsed.password.is_none()
      && let Some(authority) = whole(&parsed.host)
      && let Some((credentials, hostname)) = authority.rsplit_once('@')
      && !credentials.is_empty()
      && !hostname.is_empty()
    {
      match credentials.split_once(':') {
        Some((u, p)) => {
          out.user = decoded(u);
          out.password = decoded(p);
        }
        None => out.user = decoded(credentials),
      }
      out.host = decoded(hostname);
    }

    if parsed.password.is_none()
      && let Some(credentials) = whole(&parsed.user)
      && let Some((u, p)) = credentials.split_once(':')
    {
      out.user = decoded(u);
      out.password = decoded(p);
    }

    // Upstream fills the path text as is, but fills the targets it already
    // split, so a value with a `/` stays one target.
    out.path = self.fill(&parsed.path, values, false);
    out.path_parts = parsed.path_parts.iter().map(|part| self.fill(part, values, false)).collect();

    let fill_map = |m: &IndexMap<String, String>| -> IndexMap<String, String> {
      m.iter().map(|(k, v)| (self.fill(k, values, false), self.fill(v, values, false))).collect()
    };
    out.qsd = parsed.qsd.iter().map(|(k, v)| (self.fill(k, values, false), self.fill(v, values, false))).collect();
    out.qsd_order = parsed.qsd_order.iter().map(|k| self.fill(k, values, false)).collect();
    out.qsd_add = fill_map(&parsed.qsd_add);
    out.qsd_del = fill_map(&parsed.qsd_del);
    out.qsd_colon = fill_map(&parsed.qsd_colon);
    out.raw = self.fill(&parsed.raw, values, false);
    out
  }
}

/// Upstream `template_references()`: every `${NAME}` (normalized) in a
/// YAML value, looking into mapping values and into keys that look like
/// URLs.
#[cfg(feature = "yaml")]
pub fn template_references(value: &serde_yaml::Value, out: &mut BTreeSet<String>) {
  use serde_yaml::Value;
  match value {
    Value::String(s) => {
      for c in template_var_re().captures_iter(s) {
        out.insert(normalize_name(&c["name"]));
      }
    }
    Value::Sequence(seq) => seq.iter().for_each(|v| template_references(v, out)),
    Value::Mapping(m) => {
      for (k, v) in m {
        if let Value::String(key) = k
          && key.contains("://")
        {
          template_references(k, out);
        }
        template_references(v, out);
      }
    }
    Value::Tagged(t) => template_references(&t.value, out),
    _ => {}
  }
}

/// Upstream `template_in_schema()`: a declared variable used before the
/// URL's `://` separator.
pub fn template_in_schema(url: &str, schema: &TemplateSchema) -> Option<String> {
  let separator = url.find("://");
  template_var_re().captures_iter(url).find_map(|c| {
    let name = normalize_name(&c["name"]);
    let start = c.get(0).unwrap().start();
    (schema.variables.contains_key(&name) && separator.is_none_or(|s| start < s)).then_some(name)
  })
}

/// Upstream `resolve_values()`: the value for each name, from the caller,
/// then the configuration default, then `APPRISE_TEMPLATE_<NAME>`. Call and
/// environment values are trimmed and a blank one counts as missing.
pub fn resolve_values(
  schema: &TemplateSchema,
  overrides: &HashMap<String, String>,
  names: &BTreeSet<String>,
  environ: &dyn Fn(&str) -> Option<String>,
) -> Result<HashMap<String, String>, TemplateError> {
  validate_overrides(overrides)?;

  let mut supplied = HashMap::new();
  for (key, value) in overrides {
    let name = normalize_name(key);
    if !schema.variables.contains_key(&name) {
      continue;
    }
    let value = value.trim();
    if value.is_empty() {
      continue;
    }
    supplied.insert(name.clone(), validate_value(&name, value)?);
  }

  let mut results = HashMap::new();
  for (name, default) in &schema.variables {
    if !names.contains(name) {
      continue;
    }
    if let Some(v) = supplied.get(name) {
      results.insert(name.clone(), v.clone());
      continue;
    }
    if let Some(d) = default {
      results.insert(name.clone(), d.clone());
      continue;
    }
    let env = environ(&format!("{}{}", TEMPLATE_ENV_PREFIX, name.to_uppercase())).unwrap_or_default();
    let env = env.trim();
    if !env.is_empty() {
      results.insert(name.clone(), validate_value(name, env)?);
      continue;
    }
    return Err(TemplateError::for_var(format!("No value available for template variable '{}'.", name), name));
  }
  Ok(results)
}

/// Read the process environment.
pub fn process_env(key: &str) -> Option<String> {
  std::env::var(key).ok()
}

/// A configuration entry waiting for one or more template values
/// (upstream `NotifyTemplate`).
#[derive(Debug, Clone)]
pub struct PendingService {
  /// The resolved plugin URL with placeholders still in it, YAML settings
  /// and tags already applied.
  pub parsed: ParsedUrl,
  pub placeholders: Arc<PlaceholderMap>,
  /// The variables this entry needs.
  pub names: BTreeSet<String>,
  /// Where the entry came from, for log messages.
  pub location: String,
}

impl PendingService {
  pub fn schema(&self) -> &str {
    &self.parsed.schema
  }

  /// Raw tags, known before any value is.
  pub fn tags(&self) -> Vec<String> {
    self.parsed.tags()
  }

  /// The pending URL with `${NAME}` markers shown.
  pub fn display_url(&self) -> String {
    self.placeholders.display(&self.parsed.raw)
  }

  /// Names without a configuration default.
  pub fn required(&self) -> Vec<String> {
    self.names.iter().filter(|n| self.placeholders.schema.is_required(n)).cloned().collect()
  }

  /// Build the service from caller values (plus defaults and the
  /// environment). Returns `None`, after logging why, when a value is
  /// missing or the service rejects the filled-in URL.
  pub fn resolve(&self, overrides: &HashMap<String, String>, environ: &dyn Fn(&str) -> Option<String>) -> Option<Box<dyn Notify>> {
    match self.values(overrides, environ) {
      Ok(values) => self.build(&values),
      Err(e) => {
        self.log_missing(&e);
        None
      }
    }
  }

  /// The value of every variable this entry uses: from the caller, then
  /// the configuration default, then the environment.
  pub fn values(&self, overrides: &HashMap<String, String>, environ: &dyn Fn(&str) -> Option<String>) -> Result<HashMap<String, String>, TemplateError> {
    resolve_values(&self.placeholders.schema, overrides, &self.names, environ)
  }

  /// Say that the entry is skipped for want of a value (never the value).
  pub fn log_missing(&self, e: &TemplateError) {
    tracing::error!("Template variable '{}' is not available; {}:// was skipped", e.variable.as_deref().unwrap_or("?"), self.schema());
    tracing::warn!("A {}:// entry was skipped; it is waiting on a configuration value", self.schema());
  }

  /// Build the service from [`values`](Self::values). Returns `None`,
  /// after logging why, when the service rejects the filled-in URL.
  pub fn build(&self, values: &HashMap<String, String>) -> Option<Box<dyn Notify>> {
    let parsed = self.placeholders.substitute(&self.parsed, values);
    let length = parsed.schema.len()
      + 3
      + [&parsed.user, &parsed.password, &parsed.host].iter().map(|f| f.as_ref().map_or(0, |s| s.len() + 1)).sum::<usize>()
      + parsed.path.len()
      + parsed.qsd.iter().map(|(k, v)| k.len() + v.len() + 2).sum::<usize>();
    if length > MAX_RESOLVED_URL_LEN {
      tracing::error!("A template built an over-sized {}:// URL ({})", self.schema(), self.location);
      return None;
    }

    let service = registry::from_parsed(&parsed);
    if service.is_none() {
      tracing::error!("Could not load {}:// once template variables were applied ({})", self.schema(), self.location);
    }
    service
  }
}

/// A template entry as an `Apprise` instance keeps it: built again from
/// whatever values are current each time it is used, handing back the same
/// service as last time for the same values so what it remembers between
/// notifications is kept (upstream `NotifyTemplate.resolve()`).
pub struct TemplateEntry {
  pub pending: PendingService,
  built: std::sync::Mutex<Vec<Built>>,
}

/// What one set of values built (`None` when the service refused the
/// URL), by a hash of the values; the most recently used is last.
type Built = (String, Option<Arc<dyn Notify>>);

impl std::fmt::Debug for TemplateEntry {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TemplateEntry").field("pending", &self.pending).finish_non_exhaustive()
  }
}

impl TemplateEntry {
  pub fn new(pending: PendingService) -> Self {
    Self { pending, built: Default::default() }
  }

  /// The service the current values build, or `None` when a value is
  /// missing (logged when `log` is set) or the service refuses the URL.
  pub fn build(&self, overrides: &HashMap<String, String>, environ: &dyn Fn(&str) -> Option<String>, log: bool) -> Option<Arc<dyn Notify>> {
    let values = match self.pending.values(overrides, environ) {
      Ok(values) => values,
      Err(e) => {
        if log {
          self.pending.log_missing(&e);
        }
        return None;
      }
    };
    let key = values_key(&values);
    let mut built = self.built.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(index) = built.iter().position(|(k, _)| *k == key) {
      let entry = built.remove(index);
      let service = entry.1.clone();
      built.push(entry);
      return service;
    }
    let service: Option<Arc<dyn Notify>> = self.pending.build(&values).map(Arc::from);
    built.push((key, service.clone()));
    if built.len() > MAX_RESOLVE_CACHE {
      built.remove(0);
    }
    service
  }
}

/// A hash of names and values, so the cache never holds a secret.
fn values_key(values: &HashMap<String, String>) -> String {
  use sha2::{Digest, Sha256};
  let mut names: Vec<&String> = values.keys().collect();
  names.sort();
  let mut hasher = Sha256::new();
  for name in names {
    hasher.update(name.as_bytes());
    hasher.update([0]);
    hasher.update(values[name].as_bytes());
    hasher.update([0]);
  }
  hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn schema(vars: &[(&str, Option<&str>)]) -> TemplateSchema {
    TemplateSchema { variables: vars.iter().map(|(k, v)| (k.to_string(), v.map(str::to_string))).collect() }
  }

  #[test]
  fn test_names() {
    assert!(is_template_name("TOKEN_1"));
    assert!(is_template_name(&"a".repeat(32)));
    assert!(!is_template_name(&"a".repeat(33)));
    assert!(!is_template_name("bad-name"));
    assert!(!is_template_name(""));
    assert!(!is_template_name("token\n"));
  }

  #[test]
  fn test_validate_value() {
    assert_eq!(validate_value("a", "ok").unwrap(), "ok");
    assert!(validate_value("a", "line\nbreak").is_err());
    assert!(validate_value("a", "sep\u{2028}").is_err());
    assert!(validate_value("a", &"x".repeat(1025)).is_err());
    assert!(validate_value("a", &"x".repeat(1024)).is_ok());
  }

  #[test]
  fn test_validate_overrides() {
    let mut v = HashMap::new();
    v.insert("Token".to_string(), "x".to_string());
    assert!(validate_overrides(&v).is_ok());
    v.insert("TOKEN".to_string(), "y".to_string());
    assert!(validate_overrides(&v).is_err());
    let mut v = HashMap::new();
    v.insert("bad-name".to_string(), "x".to_string());
    assert!(validate_overrides(&v).is_err());
  }

  #[test]
  fn test_encode_only_declared() {
    let map = PlaceholderMap::new(schema(&[("token", None)]), "").unwrap();
    let encoded = map.encode_url("json://${TOKEN}@host/${other}");
    assert!(!encoded.contains("${TOKEN}"));
    assert!(encoded.contains("${other}"));
    // The same name always gets the same placeholder
    assert_eq!(map.encode("${token}"), map.encode("${Token}"));
    assert_eq!(map.display(&map.encode("${token}")), "${TOKEN}");
    // The schema is never encoded
    assert!(map.encode_url("${token}://host").starts_with("${token}://"));
  }

  #[test]
  fn test_template_in_schema() {
    let s = schema(&[("proto", None)]);
    assert_eq!(template_in_schema("${proto}://host", &s).as_deref(), Some("proto"));
    assert_eq!(template_in_schema("json://host/${proto}://x", &s), None);
    assert_eq!(template_in_schema("${undeclared}://host", &s), None);
  }

  #[test]
  fn test_resolve_values_precedence() {
    let s = schema(&[("a", Some("default-a")), ("b", None), ("c", None)]);
    let names: BTreeSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
    let env = |k: &str| (k == "APPRISE_TEMPLATE_B").then(|| " from-env ".to_string());

    // default beats the environment; environment fills a required value
    let got = resolve_values(&s, &HashMap::new(), &names, &env).unwrap();
    assert_eq!(got.get("a").map(String::as_str), Some("default-a"));
    assert_eq!(got.get("b").map(String::as_str), Some("from-env"));
    assert!(!got.contains_key("c"));

    // the call value wins; a blank one falls through
    let mut call = HashMap::new();
    call.insert("A".to_string(), "call-a".to_string());
    call.insert("b".to_string(), "  ".to_string());
    let got = resolve_values(&s, &call, &names, &env).unwrap();
    assert_eq!(got.get("a").map(String::as_str), Some("call-a"));
    assert_eq!(got.get("b").map(String::as_str), Some("from-env"));

    // nothing for a required value is an error naming it
    let no_env = |_: &str| None;
    let err = resolve_values(&s, &HashMap::new(), &names, &no_env).unwrap_err();
    assert_eq!(err.variable.as_deref(), Some("b"));
  }

  #[test]
  fn test_substitute_keeps_structure() {
    let map = PlaceholderMap::new(schema(&[("host", None), ("token", None), ("auth", None)]), "").unwrap();
    let parsed = ParsedUrl::parse(&map.encode_url("json://${host}/path/${token}?to=${token}")).unwrap();
    let mut values = HashMap::new();
    values.insert("host".to_string(), "example.com".to_string());
    values.insert("token".to_string(), "a/b?c=d".to_string());
    let out = map.substitute(&parsed, &values);
    assert_eq!(out.host.as_deref(), Some("example.com"));
    // A value in the query string can not add arguments
    assert_eq!(out.get("to"), Some("a/b?c=d"));
    assert_eq!(out.qsd.len(), 1);
    // The path text takes the value as is, but a value stays one target, as
    // upstream fills the targets it split before substituting
    assert_eq!(out.path, "path/a/b?c=d");
    assert_eq!(out.path_parts, vec!["path", "a/b?c=d"]);

    // A whole-host marker may carry credentials
    let parsed = ParsedUrl::parse(&map.encode_url("json://${auth}/")).unwrap();
    let mut values = HashMap::new();
    values.insert("auth".to_string(), "user:pa%40ss@example.com".to_string());
    let out = map.substitute(&parsed, &values);
    assert_eq!(out.user.as_deref(), Some("user"));
    assert_eq!(out.password.as_deref(), Some("pa@ss"));
    assert_eq!(out.host.as_deref(), Some("example.com"));
  }

  #[test]
  fn test_used() {
    let map = PlaceholderMap::new(schema(&[("a", None), ("b", None)]), "").unwrap();
    let parsed = ParsedUrl::parse(&map.encode_url("json://host/?x=${a}")).unwrap();
    let used = map.used(&parsed);
    assert_eq!(used.into_iter().collect::<Vec<_>>(), vec!["a"]);
  }
}
