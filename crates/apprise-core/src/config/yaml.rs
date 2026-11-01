//! YAML configuration (upstream `ConfigBase.config_parse_yaml()`).
//!
//! ```yaml
//! version: 1
//! asset:
//!   app_id: MyApp
//! tag: global, tags          # added to every URL below
//! groups:
//!   admins: tagA, tagB       # also a list, or a list of mappings
//! template:                  # ${NAME} variables (see config::template)
//!   - token
//!   - host: example.com
//! include:
//!   - /etc/other.yml
//! urls:
//!   - json://localhost
//!   - json://localhost:      # settings under the URL ...
//!       tag: one
//!       retry: 2
//!   - mailtos://user:${token}@${host}:
//!     to: me@example.com     # ... or beside it, at the same level
//!   - json://localhost:      # a list makes one service per entry
//!     - to: a
//!     - to: b
//! ```
//!
//! Settings under a URL win over settings beside it, which win over the
//! URL's own query string. Plugins in this port read their settings from
//! the query string, so each setting is applied as the query argument of
//! the same name; `user`, `password` (or `pass`), `host` and `port` set
//! those parts of the URL. A setting named after one of the service's URL
//! tokens (Discord's `botname`, ntfy's `topic`, a `target_phone`) sets the
//! URL part or argument that carries the same value, as upstream's
//! `_special_token_handler()` maps it (table in `yaml/tokens.rs`). Target
//! settings add to the URL's own targets, where upstream replaces them.
//! A setting for a constructor argument no URL argument carries (Gotify's
//! `token` and `path`, SMPP's `from_phone` and targets) is written at that
//! argument's place in the URL instead (its host, user or password, or
//! path segments), replacing what the URL had there. Where that place
//! depends on the URL's form (Office 365's `source` and `tenant`, a
//! sender's `from_email` spread over the password and host), the setting
//! goes to the plugin beside the URL ([`ParsedUrl::kwargs`]), which reads
//! it after parsing the URL, as upstream's constructor gets it.
//! `tag`/`tags` give the service its tags; a
//! `?tag=`/`?tags=` on the URL itself stays with the plugin (upstream
//! c511e1d4, 9b64369c).

use std::collections::BTreeSet;
use std::sync::Arc;

use indexmap::IndexMap;

mod tokens;
mod value;
use tokens::{Bound, Place, Segments, UrlPosition, UrlToken, url_position, url_token};
pub use value::{Mapping, Value, python_float};

use super::template::{PendingService, PlaceholderMap, TemplateSchema, template_in_schema, template_references};
use super::{
  ConfigItem, GroupTags, LoadOptions, ParseOutput, apply_groups, assign_group, config_bool, is_valid_token, loggable, normalize_tag_groups, tag_has_retry,
};
use crate::asset::AppriseAsset;
use crate::error::ConfigError;
use crate::notify::registry::{self, url_schema};
use crate::utils::parse::{ParsedUrl, parse_list, parse_urls, quote, unquote};

/// Settings written for one service, in YAML order.
type Tokens = IndexMap<String, Value>;

/// Upstream `parse_list(value, cast=str)` for a YAML value: strings are
/// split, sequences flattened, numbers converted; booleans, nulls and
/// mappings give nothing.
fn yaml_list(v: &Value) -> Vec<String> {
  let mut out = Vec::new();
  fn walk(v: &Value, out: &mut Vec<String>) {
    match v {
      Value::String(s) => out.extend(parse_list(s)),
      Value::Int(_) | Value::Float(_) => out.extend(yaml_text(v).as_deref().map(parse_list).unwrap_or_default()),
      Value::Sequence(seq) => seq.iter().for_each(|e| walk(e, out)),
      _ => {}
    }
  }
  walk(v, &mut out);
  out.sort();
  out.dedup();
  out
}

/// A YAML scalar as the text a query argument carries (numbers as
/// Python's `str()` writes them).
fn yaml_text(v: &Value) -> Option<String> {
  match v {
    Value::String(s) => Some(s.clone()),
    Value::Bool(b) => Some(if *b { "yes" } else { "no" }.to_string()),
    Value::Int(n) => Some(n.to_string()),
    Value::Float(f) => Some(value::python_float(*f)),
    Value::Sequence(seq) => {
      let parts: Vec<String> = seq.iter().filter_map(yaml_text).collect();
      (!parts.is_empty()).then(|| parts.join(","))
    }
    Value::Null | Value::Mapping(_) => None,
  }
}

/// Replace declared variables in every string of a YAML value, leaving
/// mapping keys as written (upstream `encode_obj()`).
fn encode_value(v: &Value, map: &PlaceholderMap) -> Value {
  match v {
    Value::String(s) => Value::String(map.encode(s)),
    Value::Sequence(seq) => Value::Sequence(seq.iter().map(|e| encode_value(e, map)).collect()),
    Value::Mapping(m) => Value::Mapping(m.iter().map(|(k, v)| (k.clone(), encode_value(v, map))).collect()),
    other => other.clone(),
  }
}

/// Collect a mapping's string keys as tokens; other keys can never be
/// settings and are reported.
fn tokens_of(m: &Mapping, skip: &dyn Fn(&str) -> bool, entry: usize) -> Tokens {
  let mut out = Tokens::new();
  for (k, v) in m {
    match k.as_str() {
      Some(key) if skip(key) => {}
      Some(key) => {
        out.insert(key.to_string(), v.clone());
      }
      None => tracing::warn!("Ignoring invalid token ({}) found in YAML configuration entry #{}", yaml_scalar(k), entry),
    }
  }
  out
}

fn yaml_scalar(v: &Value) -> String {
  v.python_str()
}

/// Upstream `_special_token_handler()`: `pass` fills `password` unless
/// that is already set, and a setting named after one of the service's URL
/// tokens (Discord's `botname`, ntfy's `topic`) becomes the URL part or
/// argument that carries the same value. Settings that name the same list
/// add up; a token that stands for several URL values is dropped. Siblings
/// and children go through this separately, before they are merged.
///
/// A setting for a constructor argument no URL argument carries (Gotify's
/// `token` and `path`, SMPP's `from_phone`) is kept under [`AT`] and its
/// argument's name, and [`apply_tokens`] writes it at that argument's
/// place in the URL; one without such a place, under [`KWARG`], and
/// [`apply_tokens`] hands it to the plugin beside the URL.
///
/// Settings with one prefix (`+name`, `-name`, `:name`) become one token
/// named after the prefix, holding a mapping of names to Python's `str()`
/// of their values: upstream collects them into the one plugin argument
/// that prefix fills (JSON's `headers`), which replaces the URL's, and a
/// child's replaces a sibling's.
fn special_tokens(schema: &str, mut tokens: Tokens) -> Tokens {
  // Only this function names tokens for a URL position or a plugin
  // argument.
  tokens.retain(|key, _| !key.starts_with(AT) && !key.starts_with(KWARG));
  if let Some(pass) = tokens.shift_remove("pass")
    && tokens.get("password").is_none_or(Value::is_null)
  {
    tokens.insert("password".to_string(), pass);
  }
  for prefix in URL_ARG_PREFIXES {
    let mut group = Mapping::new();
    tokens.retain(|key, value| match key.strip_prefix(prefix) {
      Some(name) => {
        group.insert(Value::String(name.to_string()), Value::String(value.python_str()));
        false
      }
      None => true,
    });
    if !group.is_empty() {
      tokens.insert(prefix.to_string(), Value::Mapping(group));
    }
  }
  let mut out = Tokens::new();
  let mut lists = BTreeSet::new();
  for (key, value) in tokens {
    // `first`: upstream keeps a list written under the argument's own name
    // where it is and adds the other settings for it after it.
    let (name, list, first) = match url_token(schema, &key) {
      None => (key, false, false),
      Some(UrlToken::UrlOnly) => {
        tracing::warn!("The {}:// {} keyword can only be defined from within a URL; ignoring it.", schema, key);
        continue;
      }
      Some(UrlToken::Arg(name) | UrlToken::Part(name)) => (name.to_string(), false, false),
      Some(UrlToken::List(name)) => (name.to_string(), true, false),
      Some(UrlToken::At(name)) => {
        let list = url_position(schema, name).is_some_and(|p| matches!(p.place, Place::Path { kind: Segments::List, .. }));
        (format!("{AT}{name}"), list, key == name)
      }
      Some(UrlToken::Kwarg(name)) => (format!("{KWARG}{name}"), false, false),
    };
    if list || lists.contains(&name) {
      // A list collects every setting that names it, including one
      // written under the argument's own name
      let values = match value {
        Value::Sequence(more) => more,
        other => vec![other],
      };
      let slot = out.entry(name.clone()).or_insert(Value::Sequence(Vec::new()));
      let seq = match std::mem::replace(slot, Value::Null) {
        Value::Sequence(seq) => seq,
        previous => vec![previous],
      };
      *slot = Value::Sequence(if first { [values, seq].concat() } else { [seq, values].concat() });
      lists.insert(name);
    } else {
      out.insert(name, value);
    }
  }
  out
}

/// Names a setting kept for its constructor argument's place in the URL
/// ([`special_tokens`]); no YAML key can start with it.
const AT: &str = "\u{0}at:";

/// Names a setting kept for a constructor argument that goes to the plugin
/// beside the URL ([`special_tokens`]); no YAML key can start with it.
const KWARG: &str = "\u{0}kwarg:";

/// Hand a setting to the plugin as its constructor argument `name`, beside
/// the URL ([`ParsedUrl::kwargs`]): upstream passes it to the constructor,
/// over whatever the URL gave. The value is trimmed, as URL values are, and
/// an empty one is ignored, as an empty setting for a URL position is.
fn set_kwarg(parsed: &mut ParsedUrl, name: &str, value: &Value, entry: usize, item: usize) {
  if value.is_null() {
    return;
  }
  match yaml_text(value).map(|text| text.trim().to_string()) {
    Some(text) if !text.is_empty() => {
      parsed.kwargs.insert(name.to_string(), text);
    }
    Some(_) => tracing::warn!("The {} setting is empty; ignoring it.", name),
    None => {
      tracing::warn!("The {} setting in YAML configuration entry #{}, item #{} can not be expressed as text; ignoring it.", name, entry, item)
    }
  }
}

/// Write a setting at the URL place of constructor argument `name`
/// (upstream passes it to the constructor, over whatever the URL gave): a
/// URL part, or a run of path segments. The URL arguments that would give
/// the argument instead are dropped. `Err` as for [`set_url_part`].
fn apply_position(parsed: &mut ParsedUrl, name: &str, value: &Value) -> Result<(), String> {
  let Some(UrlPosition { place, overrides }) = url_position(&parsed.schema, name) else { return Ok(()) };
  let written = match place {
    Place::Part(part) => set_url_part(parsed, part, value).map(|()| true)?,
    Place::Path { start, end, kind } => write_segments(parsed, name, start, end, kind, value),
  };
  if written {
    let overridden = |k: &str| overrides.iter().any(|o| k.trim().eq_ignore_ascii_case(o));
    parsed.qsd.retain(|k, _| !overridden(k));
    parsed.qsd_order.retain(|k| !overridden(k));
    rewrite_raw(parsed, matches!(place, Place::Part(_)), &overridden);
  }
  Ok(())
}

/// Write `value` over the path segments from `start` to `end`; false when
/// the URL has no room there.
fn write_segments(parsed: &mut ParsedUrl, name: &str, start: Bound, end: Bound, kind: Segments, value: &Value) -> bool {
  let text = || yaml_text(value).unwrap_or_default();
  let (new, ends_with_slash): (Vec<String>, bool) = match kind {
    Segments::One => (Some(text()).filter(|t| !t.is_empty()).into_iter().collect(), false),
    Segments::Path => {
      let path = text();
      (path.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect(), path.ends_with('/'))
    }
    Segments::List => {
      let items: Vec<String> = match value {
        Value::Sequence(seq) => seq.iter().filter_map(yaml_text).collect(),
        other => yaml_text(other).into_iter().collect(),
      };
      (items.into_iter().filter(|t| !t.is_empty()).collect(), false)
    }
  };
  if kind == Segments::One && new.is_empty() {
    // Removing the segment would shift the next one into its place.
    tracing::warn!("The {} setting is empty; ignoring it.", name);
    return false;
  }
  let parts = &mut parsed.path_parts;
  let n = parts.len();
  let at = |bound: Bound| match bound {
    Bound::Start(i) => i as isize,
    Bound::End(i) => n as isize - i as isize,
  };
  let (s, e) = (at(start).max(0) as usize, at(end).clamp(0, n as isize) as usize);
  if s > e {
    tracing::warn!("The URL has no place for the {} setting; ignoring it.", name);
    return false;
  }
  // A path written at the end of the URL decides its trailing slash.
  let slash = if e == n && kind == Segments::Path { ends_with_slash } else { parsed.path.ends_with('/') };
  parts.splice(s..e, new);
  let mut path = parts.iter().map(|p| quote(p, PATH_SEGMENT_SAFE)).collect::<Vec<_>>().join("/");
  if slash && !path.is_empty() {
    path.push('/');
  }
  parsed.path = path;
  parsed.refresh_path_parts();
  true
}

/// Bring `parsed.raw` in line for plugins that read the URL as written
/// (Telegram finds its bot token there): the URL authority (`part`) or
/// path is rebuilt, and the overridden arguments are left out.
fn rewrite_raw(parsed: &mut ParsedUrl, part: bool, overridden: &dyn Fn(&str) -> bool) {
  let raw = parsed.raw.clone();
  let Some((schema, rest)) = raw.split_once("://") else { return };
  let (before, query) = rest.split_once('?').map_or((rest, None), |(b, q)| (b, Some(q)));
  let (authority, written_path) = before.split_once('/').unwrap_or((before, ""));
  let authority = if part {
    let mut out = String::new();
    if parsed.user.is_some() || parsed.password.is_some() {
      out.push_str(&parsed.user.as_deref().map(|u| quote(u, "")).unwrap_or_default());
      if let Some(password) = &parsed.password {
        out.push(':');
        out.push_str(&quote(password, ""));
      }
      out.push('@');
    }
    out.push_str(&parsed.host.as_deref().map(|h| quote(h, ":[]")).unwrap_or_default());
    if let Some(port) = parsed.port {
      out.push_str(&format!(":{port}"));
    }
    out
  } else {
    authority.to_string()
  };
  let path = if part { written_path.to_string() } else { parsed.path.clone() };
  let query: Vec<&str> =
    query.into_iter().flat_map(|q| q.split(['&', ';'])).filter(|p| !p.is_empty() && !overridden(&unquote(p.split('=').next().unwrap_or("")))).collect();
  parsed.raw = format!("{schema}://{authority}/{path}{}", if query.is_empty() { String::new() } else { format!("?{}", query.join("&")) });
}

/// Characters a path segment may hold as they are (RFC 3986 `pchar`).
const PATH_SEGMENT_SAFE: &str = ":@!$&'()*+,;=";

/// One URL entry, with its settings applied and its tags (groups
/// included) assigned.
struct Collected {
  parsed: ParsedUrl,
  entry: usize,
  item: usize,
}

/// Parse a YAML document.
pub fn parse_yaml(content: &str, opts: &LoadOptions) -> Result<ParseOutput, ConfigError> {
  let (mut out, collected, placeholders) = collect(content, opts)?;
  let asset = out.asset.clone().unwrap_or_else(|| opts.asset.clone());

  for Collected { parsed, entry, item } in collected {
    if let Some(map) = &placeholders {
      let names = map.used(&parsed);
      if !names.is_empty() {
        out.items.push(ConfigItem::Pending(Box::new(PendingService {
          parsed,
          placeholders: map.clone(),
          names,
          location: format!("YAML entry #{}, item #{}", entry, item),
        })));
        continue;
      }
    }

    match registry::from_parsed(&parsed) {
      Some(service) => {
        tracing::debug!("Loaded URL: {}", loggable(&parsed.raw, &asset));
        out.items.push(ConfigItem::Service(service));
      }
      None => tracing::error!("Could not load Apprise YAML configuration entry #{}, item #{}", entry, item),
    }
  }

  Ok(out)
}

/// The document's other sections, its URL entries and its placeholders.
type CollectOutput = (ParseOutput, Vec<Collected>, Option<Arc<PlaceholderMap>>);

/// Read the document into resolved URLs; services are not built yet.
fn collect(content: &str, opts: &LoadOptions) -> Result<CollectOutput, ConfigError> {
  let doc = match value::load(content) {
    Ok(v) => v,
    Err(e) => {
      tracing::error!("Invalid Apprise YAML data specified.");
      tracing::debug!("YAML Exception: {}", e);
      return Err(ConfigError::InvalidFormat(format!("invalid YAML: {}", e)));
    }
  };

  let Value::Mapping(root) = &doc else {
    tracing::error!("Invalid Apprise YAML based configuration specified.");
    return Err(ConfigError::InvalidFormat("YAML configuration must be a mapping".into()));
  };
  let get = |k: &str| root.get(k);

  // Python compares with ==, so 1.0 and true also pass.
  let version_ok = match get("version") {
    None => true,
    Some(Value::Int(n)) => *n == 1,
    Some(Value::Float(f)) => *f == 1.0,
    Some(Value::Bool(b)) => *b,
    _ => false,
  };
  if !version_ok {
    let shown = get("version").map(yaml_scalar).unwrap_or_default();
    tracing::error!("Invalid Apprise YAML version specified {}.", shown);
    return Err(ConfigError::InvalidFormat(format!("unsupported YAML version {}", shown)));
  }

  tracing::debug!("Apprise YAML template variables are {}.", if opts.allow_templates { "enabled" } else { "disabled" });

  // template: section
  let mut placeholders: Option<Arc<PlaceholderMap>> = None;
  if opts.allow_templates
    && let Some(entries) = get("template").filter(|v| !v.is_null())
  {
    let schema = TemplateSchema::parse(entries).map_err(|e| {
      tracing::error!("Invalid Apprise YAML template section. {}", e);
      ConfigError::InvalidFormat(format!("invalid template section: {}", e))
    })?;
    if !schema.is_empty() {
      let map = PlaceholderMap::new(schema, content).map_err(|e| {
        tracing::error!("{}", e);
        ConfigError::InvalidFormat(e.to_string())
      })?;
      placeholders = Some(Arc::new(map));
    }
  }

  let mut out = ParseOutput::default();

  // asset: section
  if let Some(Value::Mapping(tokens)) = get("asset") {
    out.asset = Some(parse_asset(tokens, &opts.asset));
  }
  let asset = out.asset.clone().unwrap_or_else(|| opts.asset.clone());

  // tag: / tags: apply to every URL
  let global_tags: BTreeSet<String> = match get("tag").or_else(|| get("tags")) {
    Some(v @ (Value::String(_) | Value::Sequence(_))) => yaml_list(v).into_iter().collect(),
    _ => BTreeSet::new(),
  };

  // groups: a mapping, or a list of mappings
  let mut group_tags = GroupTags::new();
  let mut add_groups = |m: &Mapping| {
    for (names, tags) in m {
      let members: Vec<String> = match tags {
        Value::Sequence(seq) => {
          let mut v = Vec::new();
          for e in seq {
            match e {
              Value::Mapping(inner) => v.extend(inner.keys().flat_map(yaml_list)),
              other => v.extend(yaml_list(other)),
            }
          }
          v
        }
        other => yaml_list(other),
      };
      assign_group(&mut group_tags, &yaml_list(names), &members, false);
    }
  };
  match get("groups") {
    Some(Value::Mapping(m)) => add_groups(m),
    Some(Value::Sequence(seq)) => {
      for (no, entry) in seq.iter().enumerate() {
        match entry {
          Value::Mapping(m) => add_groups(m),
          other => tracing::warn!("No assignment for group {}, entry #{}", yaml_scalar(other), no + 1),
        }
      }
    }
    _ => {}
  }

  // include: a string (split like a URL list), or a list of them
  match get("include") {
    Some(Value::String(s)) => out.includes.extend(parse_urls(s)),
    Some(Value::Sequence(seq)) => {
      for url in seq {
        match url {
          Value::String(s) => out.includes.extend(parse_urls(s)),
          Value::Mapping(m) => out.includes.extend(m.keys().filter_map(|k| k.as_str().map(str::to_string))),
          _ => {}
        }
      }
    }
    _ => {}
  }

  let urls: &[Value] = match get("urls") {
    Some(Value::Sequence(seq)) => seq,
    _ => &[],
  };

  if let Some(map) = &placeholders
    && tracing::enabled!(tracing::Level::DEBUG)
  {
    // Guidance only: undeclared ${NAME} stays literal and unused
    // declarations are valid.
    let mut referenced = BTreeSet::new();
    urls.iter().for_each(|u| template_references(u, &mut referenced));
    let declared: BTreeSet<String> = map.schema.names().into_iter().collect();
    for name in referenced.difference(&declared) {
      tracing::debug!("Template entry '{}' is not defined in the template section; it is kept as written.", name);
    }
    for name in declared.difference(&referenced) {
      tracing::warn!("Template entry '{}' is defined but not referenced by a service entry.", name);
    }
  }

  // Entries waiting for the tag groups to be complete.
  struct Preloaded {
    parsed: ParsedUrl,
    tags: BTreeSet<String>,
    entry: usize,
    item: usize,
  }
  let mut preloaded: Vec<Preloaded> = Vec::new();

  for (index, url) in urls.iter().enumerate() {
    let no = index + 1;
    // (parsed URL, its YAML settings)
    let mut results: Vec<(ParsedUrl, Tokens)> = Vec::new();

    match url {
      Value::String(url) => {
        let logged = loggable(url, &asset);
        if let Some(map) = &placeholders
          && template_in_schema(url, &map.schema).is_some()
        {
          tracing::error!("Template variables are not permitted in URL schemas (YAML entry #{})", no);
          continue;
        }
        let encoded = placeholders.as_ref().map(|m| m.encode_url(url)).unwrap_or_else(|| url.clone());
        let Some(schema) = url_schema(&encoded) else {
          tracing::warn!("Invalid URL {}, entry #{}", logged, no);
          continue;
        };
        let Some(parsed) = registry::parse_service_url(&encoded) else {
          if placeholders.as_ref().is_some_and(|m| uses(m, &encoded)) {
            tracing::error!("A template variable can not be used at this position for {}://, entry #{}", schema, no);
          } else {
            tracing::debug!("Unparseable URL {}, entry #{}", logged, no);
          }
          continue;
        };
        if let Some(name) = dropped_template(&encoded, &parsed, placeholders.as_deref()) {
          tracing::error!("Template variable '{}' can not be used at this position for {}://, entry #{}", name, schema, no);
          continue;
        }
        results.push((parsed, Tokens::new()));
      }

      Value::Mapping(m) => {
        // The first key that looks like a URL is the URL; other keys are
        // settings beside it.
        let mut found: Option<(String, Value, Value, String)> = None;
        let mut invalid_schema = false;
        for (key, value) in m {
          let Some(key_str) = key.as_str() else { continue };
          if let Some(map) = &placeholders
            && key_str.contains("://")
            && template_in_schema(key_str, &map.schema).is_some()
          {
            tracing::error!("Template variables are not permitted in URL schemas (YAML entry #{})", no);
            invalid_schema = true;
            break;
          }
          let encoded = placeholders.as_ref().map(|m| m.encode_url(key_str)).unwrap_or_else(|| key_str.to_string());
          if let Some(schema) = url_schema(&encoded) {
            found = Some((encoded, key.clone(), value.clone(), schema));
            break;
          }
        }
        if invalid_schema {
          continue;
        }
        let Some((url_, url_key, tokens, schema)) = found else {
          tracing::warn!("Unsupported URL, entry #{}", no);
          continue;
        };

        let url_key_str = url_key.as_str().unwrap_or_default().to_string();
        let mut siblings = tokens_of(m, &|k| k == url_key_str || k == "schema", no);
        let mut tokens = tokens;
        if let Some(map) = &placeholders {
          siblings = siblings.into_iter().map(|(k, v)| (k, encode_value(&v, map))).collect();
          tokens = encode_value(&tokens, map);
        }

        let parsed = match registry::parse_service_url(&url_) {
          Some(parsed) => {
            if let Some(name) = dropped_template(&url_, &parsed, placeholders.as_deref()) {
              tracing::error!("Template variable '{}' can not be used at this position for {}://, entry #{}", name, schema, no);
              continue;
            }
            parsed
          }
          None => {
            if placeholders.as_ref().is_some_and(|m| uses(m, &url_)) {
              tracing::error!("A template variable can not be used at this position for {}://, entry #{}", schema, no);
              continue;
            }
            // The settings may still supply everything the plugin needs.
            ParsedUrl { schema: schema.clone(), raw: url_.clone(), ..Default::default() }
          }
        };

        let siblings = special_tokens(&schema, siblings);
        match &tokens {
          Value::Sequence(entries) => {
            // One service per entry; entries that are not mappings are skipped
            for entry in entries {
              if let Value::Mapping(e) = entry {
                let child = special_tokens(&schema, tokens_of(e, &|k| k == "schema", no));
                let mut merged = siblings.clone();
                merged.extend(child);
                results.push((parsed.clone(), merged));
              }
            }
          }
          Value::Mapping(e) => {
            let child = special_tokens(&schema, tokens_of(e, &|k| k == "schema", no));
            let mut merged = siblings.clone();
            merged.extend(child);
            results.push((parsed, merged));
          }
          _ => results.push((parsed, siblings)),
        }
      }

      _ => {
        tracing::warn!("Unsupported Apprise YAML entry #{}", no);
        continue;
      }
    }

    for (item_index, (mut parsed, mut tokens)) in results.into_iter().enumerate() {
      let item = item_index + 1;
      if !registry::is_supported_schema(&parsed.schema) {
        tracing::warn!("An invalid Apprise schema ({}) in YAML configuration entry #{}, item #{}", parsed.schema, no, item);
        continue;
      }

      // Only tag/tags written in the YAML name the service's tags.
      let mut tags = global_tags.clone();
      if let Some(tag) = tokens.shift_remove("tag") {
        tags.extend(yaml_list(&tag));
        if tokens.shift_remove("tags").is_some() {
          tracing::warn!("URL #{}: {} contains both 'tag' and 'tags' keyword", no, loggable(&parsed.raw, &asset));
        }
      } else if let Some(tag) = tokens.shift_remove("tags") {
        tags.extend(yaml_list(&tag));
      }

      if let Some(map) = &placeholders {
        let mut used = BTreeSet::new();
        let mut referenced = BTreeSet::new();
        for t in &tags {
          map.used_in(t, &mut used);
          template_references(&Value::String(t.clone()), &mut referenced);
        }
        let declared: BTreeSet<String> = map.schema.names().into_iter().collect();
        if !used.is_empty() || referenced.intersection(&declared).next().is_some() {
          tracing::error!("Template variables are not permitted in tag/tags (YAML entry #{}, item #{})", no, item);
          continue;
        }
      }

      if tags.iter().any(|t| tag_has_retry(t)) {
        tracing::warn!("You can not specify a retry count as part of a service tag (YAML entry #{}, item #{}); use the 'retry:' key instead.", no, item);
        continue;
      }

      if let Err(e) = apply_tokens(&mut parsed, tokens, no, item) {
        tracing::error!("Could not load Apprise YAML configuration entry #{}, item #{}", no, item);
        tracing::debug!("Loading Exception: {}", e);
        continue;
      }
      preloaded.push(Preloaded { parsed, tags, entry: no, item });
    }
  }

  normalize_tag_groups(&mut group_tags);

  let collected = preloaded
    .into_iter()
    .map(|Preloaded { mut parsed, mut tags, entry, item }| {
      apply_groups(&mut tags, &group_tags);
      parsed.assigned_tags = Some(tags.into_iter().collect());
      Collected { parsed, entry, item }
    })
    .collect();

  Ok((out, collected, placeholders))
}

/// Whether a string holds any placeholder.
fn uses(map: &PlaceholderMap, text: &str) -> bool {
  let mut found = BTreeSet::new();
  map.used_in(text, &mut found);
  !found.is_empty()
}

/// Upstream `dropped_template()`: a declared marker the URL parser lost
/// (a fragment, a dropped `;params` segment, ...).
fn dropped_template(url: &str, parsed: &ParsedUrl, map: Option<&PlaceholderMap>) -> Option<String> {
  let map = map?;
  let mut written = BTreeSet::new();
  map.used_in(url, &mut written);
  let kept = map.used(parsed);
  written.difference(&kept).next().cloned()
}

/// Apply YAML settings to the parsed URL (upstream merges them into the
/// plugin's keyword arguments, over what the URL gave). `Err` where
/// upstream's `URLBase` raises on a value (`user: yes`); the entry does
/// not load then.
fn apply_tokens(parsed: &mut ParsedUrl, tokens: Tokens, entry: usize, item: usize) -> Result<(), String> {
  for (key, value) in tokens {
    if key == "asset" || key == "schema" {
      continue;
    }
    if let Some(name) = key.strip_prefix(AT) {
      apply_position(parsed, name, &value)?;
      continue;
    }
    if let Some(name) = key.strip_prefix(KWARG) {
      set_kwarg(parsed, name, &value, entry, item);
      continue;
    }
    if let (Some(prefix), Value::Mapping(group)) = (URL_ARG_PREFIXES.into_iter().find(|p| key == *p), &value) {
      // The settings replace the URL's own arguments with this prefix
      replace_prefixed_args(parsed, prefix, group);
      continue;
    }
    let prefixed = key.starts_with(['+', '-', ':']);
    if !prefixed && !is_valid_token(&key) {
      tracing::warn!("Ignoring invalid token ({}) found in YAML configuration entry #{}, item #{}", key, entry, item);
      continue;
    }
    if matches!(key.as_str(), "user" | "password" | "host" | "port") {
      set_url_part(parsed, &key, &value)?;
      continue;
    }
    if value.is_null() {
      continue;
    }
    let Some(text) = yaml_text(&value) else {
      tracing::warn!("The {} setting in YAML configuration entry #{}, item #{} can not be expressed as a URL argument; ignoring it.", key, entry, item);
      continue;
    };
    parsed.set_arg(&key, &text);
  }
  Ok(())
}

/// The prefixes of URL arguments that fill a plugin argument together
/// (`?+name=` headers, `?-name=` and `?:name=` for others).
const URL_ARG_PREFIXES: [&str; 3] = ["+", "-", ":"];

/// Drop the URL's arguments with `prefix` (for `+` also those whose `+`
/// was decoded to a space) and set `group`'s names under it instead.
fn replace_prefixed_args(parsed: &mut ParsedUrl, prefix: &str, group: &Mapping) {
  let starts = |k: &str| k.starts_with(prefix) || (prefix == "+" && k.starts_with(' '));
  parsed.qsd.retain(|k, _| !starts(k));
  parsed.qsd_order.retain(|k| !starts(k));
  match prefix {
    "+" => parsed.qsd_add.clear(),
    "-" => parsed.qsd_del.clear(),
    _ => parsed.qsd_colon.clear(),
  }
  for (name, value) in group.iter() {
    if let (Value::String(name), Value::String(value)) = (name, value) {
      parsed.set_arg(&format!("{prefix}{name}"), value);
    }
  }
}

/// A YAML `user`, `password`, `host` or `port`, taken the way upstream's
/// `URLBase` takes it: a false-like value (null, `no`, `0`, an empty
/// list) leaves the part unset, text is percent-decoded, and `port` is
/// `int()` of what it is given (`yes` is port 1, `8080.9` is 8080).
/// Upstream raises on any other value for the other parts, so `user: yes`
/// is an error; a number is kept as its text, where upstream raises too.
fn set_url_part(parsed: &mut ParsedUrl, key: &str, value: &Value) -> Result<(), String> {
  let unset = match value {
    Value::Null => true,
    Value::Bool(b) => !b,
    Value::Int(n) => *n == 0,
    Value::Float(f) => *f == 0.0,
    Value::String(s) => s.is_empty(),
    Value::Sequence(seq) => seq.is_empty(),
    Value::Mapping(m) => m.is_empty(),
  };
  if key == "port" {
    let port = match value {
      _ if unset => None,
      Value::Bool(_) => Some(1),
      Value::Int(n) => u16::try_from(*n).ok(),
      Value::Float(f) if f.is_finite() => u16::try_from(f.trunc() as i128).ok(),
      Value::String(s) => s.trim().parse::<u16>().ok(),
      _ => None,
    };
    if port.is_none() && !unset {
      tracing::warn!("Invalid port number specified {}", yaml_scalar(value));
    }
    parsed.port = port;
    return Ok(());
  }
  let text = match value {
    _ if unset => None,
    // URLBase decodes these parts
    Value::String(s) => Some(unquote(s)).filter(|s| !s.is_empty()),
    Value::Int(_) | Value::Float(_) => yaml_text(value),
    other => return Err(format!("the {} setting must be text, not {}", key, yaml_scalar(other))),
  };
  match key {
    "user" => parsed.user = text,
    "password" => parsed.password = text,
    _ => parsed.host = text,
  }
  Ok(())
}

/// Upstream asset keys the Rust asset does not model; accepted silently.
const UNMODELLED_ASSET_KEYS: &[&str] =
  &["default_ascii_chars", "default_extension", "default_html_color", "default_image_size", "encoding", "image_path_mask", "storage_mode"];

/// Apply an `asset:` section on top of the current asset.
fn parse_asset(tokens: &Mapping, base: &AppriseAsset) -> AppriseAsset {
  let mut asset = base.clone();

  let get = |k: &str| tokens.get(k);
  match get("timezone").or_else(|| get("tz")) {
    Some(Value::String(tz)) => {
      // Upstream keeps only word characters, '/' and '-' (so not '+')
      let cleaned: String = tz.chars().filter(|c| c.is_alphanumeric() || matches!(c, '_' | '/' | '-')).collect();
      match crate::utils::time::zoneinfo(&cleaned) {
        Some(zone) => asset.timezone = Some(zone),
        None => tracing::warn!("Ignored invalid timezone \"{}\"", tz),
      }
    }
    Some(Value::Null) | None => {}
    Some(other) => tracing::warn!("Ignored invalid timezone \"{}\"", yaml_scalar(other)),
  }

  for (k, v) in tokens {
    let Some(key) = k.as_str() else {
      tracing::warn!("Invalid asset key \"{}\".", yaml_scalar(k));
      continue;
    };
    if matches!(key, "lang" | "language" | "timezone" | "tz") {
      continue;
    }
    if key.starts_with('_') || key.ends_with('_') {
      tracing::warn!("Ignored asset key \"{}\".", key);
      continue;
    }
    if UNMODELLED_ASSET_KEYS.contains(&key) {
      tracing::debug!("Asset key \"{}\" is not used by this port.", key);
      continue;
    }

    let null = Value::Null;
    let v = if v.is_null() { &null } else { v };
    let text = match v {
      Value::Null => Some(String::new()),
      Value::String(s) => Some(s.clone()),
      _ => None,
    };

    match key {
      "secure_logging" | "async_mode" | "abort_on_chain_failure" | "http_redirects" | "interpret_escapes" | "pgp_autogen" | "pem_autogen" => {
        let value = match v {
          Value::Bool(b) => *b,
          Value::String(_) | Value::Null => config_bool(text.as_deref().unwrap_or_default()),
          _ => {
            tracing::warn!("Invalid asset value to \"{}\".", key);
            continue;
          }
        };
        match key {
          "secure_logging" => asset.secure_logging = value,
          "async_mode" => asset.async_mode = value,
          "abort_on_chain_failure" => asset.abort_on_chain_failure = value,
          "http_redirects" => asset.http_redirects = value,
          "pgp_autogen" => asset.pgp_autogen = value,
          "pem_autogen" => asset.pem_autogen = value,
          _ => asset.interpret_escapes = value,
        }
      }
      // Upstream only replaces a value the asset already holds, and this
      // one is unset unless the asset was given one (the CLI's -j)
      "interpret_emojis" if asset.interpret_emojis.is_some() => match v {
        Value::Bool(b) => asset.interpret_emojis = Some(*b),
        Value::String(_) | Value::Null => asset.interpret_emojis = Some(config_bool(text.as_deref().unwrap_or_default())),
        _ => tracing::warn!("Invalid asset value to \"{}\".", key),
      },
      "app_id" | "app_desc" | "app_url" | "theme" | "image_url_mask" | "image_url_logo" => {
        let Some(text) = text else {
          tracing::warn!("Invalid asset value to \"{}\".", key);
          continue;
        };
        let text = text.trim().to_string();
        match key {
          "app_id" => asset.app_id = text,
          "app_desc" => asset.app_desc = text,
          "app_url" => asset.app_url = text,
          "theme" => asset.theme = text,
          "image_url_mask" => asset.image_url_mask = Some(text),
          _ => asset.image_url_logo = Some(text),
        }
      }
      _ => tracing::warn!("Invalid asset key \"{}\".", key),
    }
  }

  asset
}

/// Settings as a map of strings, for tests.
#[cfg(test)]
fn args_of(parsed: &ParsedUrl) -> std::collections::HashMap<String, String> {
  parsed.qsd.clone()
}

/// The resolved URLs a document produces, for tests.
#[cfg(test)]
fn resolved(content: &str) -> Vec<ParsedUrl> {
  collect(content, &LoadOptions::default()).unwrap().1.into_iter().map(|c| c.parsed).collect()
}

/// The services of YAML document `doc`, loaded into an `Apprise`, and
/// their storage UIDs in order: for the plugins' tests of the settings
/// upstream passes to their constructors.
#[cfg(test)]
pub(crate) async fn load_for_test(doc: &str) -> (crate::Apprise, Vec<String>) {
  let mut a = crate::Apprise::new();
  a.add_config_text(doc, Some(super::ConfigFormat::Yaml), 0).await.unwrap();
  let uids = a.services().map(|s| a.url_id(s).unwrap_or_default()).collect();
  (a, uids)
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use super::*;
  use crate::notify::Notify;

  fn parse(content: &str) -> ParseOutput {
    parse_yaml(content, &LoadOptions::default()).unwrap()
  }

  fn services(content: &str) -> Vec<Box<dyn Notify>> {
    parse(content)
      .items
      .into_iter()
      .filter_map(|i| match i {
        ConfigItem::Service(s) => Some(s),
        ConfigItem::Pending(_) => None,
      })
      .collect()
  }

  fn tags(content: &str) -> Vec<Vec<String>> {
    services(content).iter().map(|s| s.tags()).collect()
  }

  fn pending(content: &str) -> Vec<PendingService> {
    parse(content)
      .items
      .into_iter()
      .filter_map(|i| match i {
        ConfigItem::Pending(p) => Some(*p),
        ConfigItem::Service(_) => None,
      })
      .collect()
  }

  #[test]
  fn test_urls_basic() {
    assert_eq!(services("urls:\n  - json://localhost\n  - xml://localhost\n").len(), 2);
    assert!(services("urls: []").is_empty());
    assert!(services("version: 1").is_empty());
    assert!(services("urls:\n  - totallyunknownschema://whatever").is_empty());
    assert_eq!(services("urls:\n  - json://localhost\n  - xml://localhost:\n      tag: special\n").len(), 2);
  }

  #[test]
  fn test_yaml_loader_matches_pyyaml() {
    // A repeated key keeps its last value (PyYAML), it is not an error
    assert_eq!(tags("urls:\n  - json://a:\n      tag: x\n      tag: y\n"), vec![vec!["y"]]);
    // Anchors, aliases and merge keys
    let content = "base: &base\n  tag: shared\n  format: html\nurls:\n  - json://a:\n      <<: *base\n      retry: 1\n  - json://b: *base\n";
    let got = resolved(content);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].tags(), vec!["shared"]);
    assert_eq!(got[0].get("format"), Some("html"));
    assert_eq!(got[0].get("retry"), Some("1"));
    assert_eq!(got[1].tags(), vec!["shared"]);
    // An explicit core tag is honoured; PyYAML's SafeLoader refuses the
    // whole document for any other tag
    assert_eq!(tags("urls:\n  - json://a:\n      tag: !!str yes\n"), vec![vec!["yes"]]);
    assert!(parse_yaml("urls:\n  - json://a:\n      tag: !custom mine\n", &LoadOptions::default()).is_err());
  }

  #[test]
  fn test_malformed_yaml_fails_closed() {
    // apprise-go #57: never fall back to TEXT
    assert!(parse_yaml("{{{{invalid yaml", &LoadOptions::default()).is_err());
    assert!(parse_yaml("urls:\n  - json://localhost\n bad: [", &LoadOptions::default()).is_err());
    assert!(parse_yaml("- just\n- a list", &LoadOptions::default()).is_err());
    assert!(parse_yaml("version: 2\nurls: []", &LoadOptions::default()).is_err());
    assert!(parse_yaml("version: '1'\nurls: []", &LoadOptions::default()).is_err());
  }

  #[test]
  fn test_tags() {
    assert_eq!(tags("urls:\n  - json://localhost:\n      tag: my_tag\n"), vec![vec!["my_tag"]]);
    assert_eq!(tags("urls:\n  - json://localhost:\n      tags: tag1,tag2\n"), vec![vec!["tag1", "tag2"]]);
    assert_eq!(tags("urls:\n  - json://localhost:\n      tag:\n        - tag_a\n        - tag_b\n"), vec![vec!["tag_a", "tag_b"]]);
    // tag wins over tags
    assert_eq!(tags("urls:\n  - json://localhost:\n      tag: a\n      tags: b\n"), vec![vec!["a"]]);
    // priority prefix kept raw
    assert_eq!(tags("urls:\n  - json://localhost:\n      tag: 2:endpoint\n"), vec![vec!["2:endpoint"]]);
  }

  #[test]
  fn test_global_tags_merge() {
    let got = tags("tag: admin, devops\nurls:\n  - json://localhost\n  - xml://localhost:\n      tag: local\n");
    assert_eq!(got, vec![vec!["admin", "devops"], vec!["admin", "devops", "local"]]);
    assert_eq!(tags("tags:\n  - a\n  - b\nurls:\n  - json://localhost\n"), vec![vec!["a", "b"]]);
  }

  #[test]
  fn test_url_tags_parameter_stays_with_plugin() {
    // upstream c511e1d4
    let out = parse("urls:\n  - ntfy://localhost/topic?tags=emoji:\n      tag: mine\n");
    let ConfigItem::Service(svc) = &out.items[0] else { panic!() };
    assert_eq!(svc.tags(), vec!["mine"]);
  }

  #[test]
  fn test_retry_in_tag_rejected() {
    assert!(services("urls:\n  - json://localhost:\n      tag: alerts:3\n").is_empty());
  }

  #[test]
  fn test_groups_mapping_and_list() {
    let content = "
groups:
  admins: tagA, tagB
  devops:
    - tagX
    - tagY
  everyone:
    - admins
    - devops
urls:
  - json://localhost:
      tag: tagA
  - json://localhost:
      tag: tagY
";
    let got = tags(content);
    assert_eq!(got[0], vec!["admins", "everyone", "tagA"]);
    assert_eq!(got[1], vec!["devops", "everyone", "tagY"]);

    let got = tags("groups:\n  - g1, g2: t1\n  - g3:\n    - t1: ignored\nurls:\n  - json://localhost:\n      tag: t1\n");
    assert_eq!(got[0], vec!["g1", "g2", "g3", "t1"]);
  }

  #[test]
  fn test_settings_child_and_sibling() {
    // upstream 5682b378: settings beside the URL, in any order
    for content in [
      "urls:\n  - json://localhost/?format=text:\n    format: markdown\n    tag: x\n",
      "urls:\n  - format: markdown\n    json://localhost/?format=text:\n    tag: x\n",
      "urls:\n  - json://localhost/?format=text:\n      format: markdown\n      tag: x\n",
    ] {
      let got = resolved(content);
      assert_eq!(got.len(), 1, "{}", content);
      assert_eq!(got[0].get("format"), Some("markdown"), "{}", content);
      assert_eq!(got[0].tags(), vec!["x"], "{}", content);
    }
    // A child setting beats a sibling, which beats the URL
    let got = resolved("urls:\n  - json://localhost/?retry=1&wait=1:\n      retry: 3\n    retry: 2\n    wait: 4\n");
    assert_eq!(got[0].get("retry"), Some("3"));
    assert_eq!(got[0].get("wait"), Some("4"));
    // Siblings apply to every listed instance
    let got = resolved("urls:\n  - json://localhost:\n    - to: a\n    - to: b\n      tag: two\n    tag: shared\n");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].get("to"), Some("a"));
    assert_eq!(got[0].tags(), vec!["shared"]);
    assert_eq!(got[1].tags(), vec!["two"]);
  }

  #[test]
  fn test_url_token_keys_set_their_url_value() {
    // Upstream test_yaml_url_token_map_to: Discord's botname is the URL user
    let got = resolved(
      "urls:\n  - discord://1234567890/abcdefghijklmnopqrstuvwxyz/:\n      - tag: test1\n        botname: App 1\n      - tag: test2\n        botname: App 2\n",
    );
    assert_eq!(got.iter().map(|p| p.user.as_deref()).collect::<Vec<_>>(), vec![Some("App 1"), Some("App 2")]);
    assert_eq!(resolved("urls:\n  - discord://1234567890/abcdefghijklmnopqrstuvwxyz/:\n    botname: App 3\n")[0].user.as_deref(), Some("App 3"));
    // test_yaml_token_alias_chains: RingCentral's from_phone is its source
    let got = resolved("urls:\n  - ringc://15551234567/abcd/efgh:\n      from_phone: '+15557654321'\n");
    assert_eq!(got[0].get("from"), Some("+15557654321"));
    // A key that is neither an argument nor a URL token is left alone
    assert_eq!(resolved("urls:\n  - discord://1234567890/abcdefghijklmnopqrstuvwxyz/:\n      not-a-token: x\n")[0].get("not-a-token"), Some("x"));

    // Settings that name the same list add up, in YAML order
    let got = resolved("urls:\n  - ntfy://localhost:\n      topic: one\n      to: two\n      targets: [three, four]\n");
    assert_eq!(got[0].get("to"), Some("one,two,three,four"));
    // ... but a child setting replaces a sibling one, as other settings do
    let got = resolved("urls:\n  - ntfy://localhost:\n      topic: child\n    topic: sibling\n");
    assert_eq!(got[0].get("to"), Some("child"));
    // The same names do nothing for a service without those tokens
    assert_eq!(resolved("urls:\n  - json://localhost:\n      topic: x\n")[0].get("topic"), Some("x"));
  }

  #[test]
  fn test_prefixed_settings_replace_the_url_ones() {
    // What upstream's json:// gets as headers (+), payload extras (:) and
    // params (-) from each document: the settings with one prefix replace
    // the URL's, a child's replace a sibling's, and values are str()
    let args = |doc: &str| {
      let p = resolved(doc).remove(0);
      let list = |m: &IndexMap<String, String>| m.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>();
      (list(&p.qsd_add), list(&p.qsd_colon), list(&p.qsd_del))
    };
    let none: Vec<String> = Vec::new();
    let strs = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(args("urls:\n  - json://localhost/?+X-A=1&:keep=u&-drop=u:\n      +X-B: 2\n"), (strs(&["X-B=2"]), strs(&["keep=u"]), strs(&["drop=u"])));
    assert_eq!(args("urls:\n  - json://localhost/?+X-A=1:\n      - +X-C: c\n    +X-S: s\n"), (strs(&["X-C=c"]), none.clone(), none.clone()));
    assert_eq!(args("urls:\n  - json://localhost/?+X-A=1&:k=u:\n      - tag: t\n    +X-S: s\n"), (strs(&["X-S=s"]), strs(&["k=u"]), none.clone()));
    assert_eq!(
      args("urls:\n  - json://localhost/:\n      +X-N: null\n      +X-T: yes\n      +X-F: 1.50\n      +X-L: [a, 1]\n"),
      (strs(&["X-N=None", "X-T=True", "X-F=1.5", "X-L=['a', 1]"]), none.clone(), none.clone())
    );
    assert_eq!(args("urls:\n  - json://localhost/?+X-A=1&:k=u&-p=u:\n      :k2: v\n"), (strs(&["X-A=1"]), strs(&["k2=v"]), strs(&["p=u"])));
    // The raw arguments follow
    let p = resolved("urls:\n  - json://localhost/?+X-A=1&other=1:\n      +X-B: 2\n").remove(0);
    assert_eq!((p.get("+X-A"), p.get("+X-B"), p.get("other")), (None, Some("2"), Some("1")));
  }

  #[test]
  fn test_settings_for_unlisted_arguments_rewrite_the_url() {
    let one = |doc: &str| resolved(doc).remove(0);
    // A URL part: Telegram's bot token is the host, also in the URL as
    // written, which the plugin reads
    let p = one("urls:\n  - tgram://123456789:abcdefghijklmnopqrstuvwxyz0123456789/12345:\n      bot_token: 987654321:zyxwvutsrqponmlkjihgfedcba9876543210\n");
    assert_eq!(p.host.as_deref(), Some("987654321:zyxwvutsrqponmlkjihgfedcba9876543210"));
    assert_eq!(p.raw, "tgram://987654321:zyxwvutsrqponmlkjihgfedcba9876543210/12345");
    // One segment, counted from the end: Gotify's token follows its path
    let p = one("urls:\n  - gotify://localhost/a/b/aaaaaaaaaaaaaaa?priority=high:\n      token: bbbbbbbbbbbbbbb\n");
    assert_eq!((p.path.as_str(), p.raw.as_str()), ("a/b/bbbbbbbbbbbbbbb", "gotify://localhost/a/b/bbbbbbbbbbbbbbb?priority=high"));
    // A path: the segments before the token, whatever their number
    for (url, path) in
      [("gotify://localhost/aaaaaaaaaaaaaaa", "my/sub/aaaaaaaaaaaaaaa"), ("gotify://localhost/x/y/z/aaaaaaaaaaaaaaa/", "my/sub/aaaaaaaaaaaaaaa/")]
    {
      assert_eq!(one(&format!("urls:\n  - {url}:\n      path: /my/sub/\n")).path, path, "{url}");
    }
    // A path at the end of the URL sets its trailing slash
    assert_eq!(one("urls:\n  - flowtriq://key@localhost/old/hook/:\n      path: /new/hook\n").path, "new/hook");
    assert_eq!(one("urls:\n  - flowtriq://key@localhost/old/hook:\n      path: /new/hook/\n").path, "new/hook/");
    // SMPP: the source is the first segment, the targets the rest; the
    // settings replace the URL's targets, its own `?to=` and `?from=`
    // included, and a list under the argument's own name comes first
    let p = one(concat!(
      "urls:\n  - smpp://user:pass@localhost/15551234567/15557654321?to=15550000000&from=15556666666&other=1:\n",
      "      target_phone: '15559998888'\n      targets: ['15553334444']\n      from_phone: '15550001111'\n",
    ));
    assert_eq!(p.path_parts, vec!["15550001111", "15553334444", "15559998888"]);
    assert_eq!((p.get("to"), p.get("from"), p.get("other")), (None, None, Some("1")));
    assert_eq!(p.raw, "smpp://user:pass@localhost/15550001111/15553334444/15559998888?other=1");
    // Nextcloud Talk: the rooms; a child setting replaces a sibling one
    let got = resolved(
      "urls:\n  - nctalk://user:pass@localhost/roomA/roomB:\n      - target_room_id: roomC\n      - target_room_id: [roomD, roomE]\n    target_room_id: roomF\n",
    );
    assert_eq!(got.iter().map(|p| p.path_parts.clone()).collect::<Vec<_>>(), vec![vec!["roomC"], vec!["roomD", "roomE"]]);
    // A value is one segment, encoded in the path
    assert_eq!(one("urls:\n  - nctalk://user:pass@localhost/roomA:\n      targets: ['#a b/c']\n").path, "%23a%20b%2Fc");
    // A URL without room for the setting keeps it as it is, and so does
    // an empty value for one segment
    let p = one("urls:\n  - github://token@owner:\n      number: 5\n");
    assert_eq!((p.path.as_str(), p.raw.as_str()), ("", "github://token@owner"));
    assert_eq!(one("urls:\n  - gotify://localhost/a/aaaaaaaaaaaaaaa:\n      token: ''\n").path, "a/aaaaaaaaaaaaaaa");
    // GoAlert tells its path from its keys by their form: the setting goes
    // to the plugin beside the URL, which stays as written
    let p = one("urls:\n  - goalert://localhost/11111111-2222-3333-4444-555555555555:\n      path: /sub\n");
    assert_eq!((p.kwarg("fullpath"), p.get("path"), p.path.as_str()), (Some("/sub"), None, "11111111-2222-3333-4444-555555555555"));
    // No YAML key can pose as one of these settings
    let p = one("urls:\n  - gotify://localhost/aaaaaaaaaaaaaaa:\n      \"\\0at:token\": bbbbbbbbbbbbbbb\n");
    assert_eq!(p.path, "aaaaaaaaaaaaaaa");
  }

  #[test]
  fn test_settings_for_arguments_without_a_place_go_beside_the_url() {
    let one = |doc: &str| resolved(doc).remove(0);
    // Office 365: the setting goes to the plugin, the URL stays as written
    let p = one("urls:\n  - azure://user@example.edu/tenantA/cid/secret?to=a@b.c:\n      tenant: tenantB\n      source: x@example.edu\n");
    assert_eq!((p.kwarg("tenant"), p.kwarg("source")), (Some("tenantB"), Some("x@example.edu")));
    assert_eq!((p.path.as_str(), p.get("tenant"), p.get("source"), p.get("to")), ("tenantA/cid/secret", None, None, Some("a@b.c")));
    assert_eq!(p.raw, "azure://user@example.edu/tenantA/cid/secret?to=a@b.c");
    // A child setting replaces a sibling one, per entry
    let got = resolved("urls:\n  - notica://aaaaaa:\n      - token: bbbbbb\n      - tag: t\n    token: cccccc\n");
    assert_eq!(got.iter().map(|p| p.kwarg("token")).collect::<Vec<_>>(), [Some("bbbbbb"), Some("cccccc")]);
    // Values are text, trimmed as URL values are; empty and null values are
    // left out, so the URL's own value stays
    assert_eq!(one("urls:\n  - fluxer://1234567890123/abcdefghijklmnopqrst:\n      webhook_id: 9876543210987\n").kwarg("webhook_id"), Some("9876543210987"));
    assert_eq!(one("urls:\n  - notica://aaaaaa:\n      token: ' bbbbbb '\n").kwarg("token"), Some("bbbbbb"));
    for value in ["''", "' '", "null", "~"] {
      assert!(one(&format!("urls:\n  - notica://aaaaaa:\n      token: {value}\n")).kwargs.is_empty(), "{value}");
    }
    // No YAML key can pose as one of these settings
    let p = one("urls:\n  - notica://aaaaaa:\n      \"\\0kwarg:token\": bbbbbb\n");
    assert!(p.kwargs.is_empty() && p.qsd.is_empty());
    // Template variables in such a setting wait for their value like any
    let p = pending("template:\n  - tok\nurls:\n  - notica://aaaaaa:\n      token: ${tok}\n");
    assert_eq!(p[0].names.iter().cloned().collect::<Vec<_>>(), vec!["tok"]);
    let values = p[0].values(&HashMap::from([("TOK".to_string(), "bbbbbb".to_string())]), &|_| None).unwrap();
    assert_eq!(p[0].placeholders.substitute(&p[0].parsed, &values).kwarg("token"), Some("bbbbbb"));
  }

  /// Each setting of the round-3 list (`gotify`, `goalert`, `notica`,
  /// `pinglet` and `flowtriq` `path`, `nctalk` `target_room_id`, with
  /// Gotify's `token`) sends what upstream sends: its requests, captured
  /// with `requests` patched, for the same YAML.
  #[tokio::test]
  async fn test_settings_for_unlisted_arguments_reach_the_plugins() {
    use wiremock::matchers::{any, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_string("{}")).mount(&server).await;
    Mock::given(any()).respond_with(ResponseTemplate::new(200).set_body_string("{}")).mount(&server).await;
    let at = server.address().to_string();
    let doc = format!(
      concat!(
        "urls:\n",
        "  - gotify://{at}/aaaaaaaaaaaaaaa:\n      path: /my/sub/\n",
        "  - gotify://{at}/aaaaaaaaaaaaaaa:\n      token: bbbbbbbbbbbbbbb\n",
        "  - goalert://{at}/sub/11111111-2222-3333-4444-555555555555:\n      path: /other/sub\n",
        "  - notica://{at}/a/rrrrrr:\n      path: /my/sub/\n",
        "  - pinglet://abc123@{at}/prefix/acme/deploys:\n      path: /my/sub/\n",
        "  - flowtriq://apikey@{at}/old/hook/:\n      path: /new/hook/\n",
        "  - nctalk://user:pass@{at}/roomA/roomB:\n      target_room_id: roomC\n",
      ),
      at = at
    );
    let mut a = crate::Apprise::new();
    assert_eq!(a.add_config_text(&doc, Some(crate::config::ConfigFormat::Yaml), 0).await.unwrap(), 7);
    let ctx = crate::NotifyContext { title: "title".into(), body: "body".into(), ..Default::default() };
    a.notify(&ctx).await;
    let mut seen: Vec<String> = server
      .received_requests()
      .await
      .unwrap()
      .iter()
      .filter(|r| r.url.path() != "/")
      .map(|r| {
        let key = ["x-gotify-key", "authorization"].iter().find_map(|h| r.headers.get(*h)).map(|v| v.to_str().unwrap().to_string());
        let query = r.url.query().map(|q| format!("?{q}")).unwrap_or_default();
        format!("{} {}{} {}", r.method, r.url.path(), query, key.unwrap_or_default())
      })
      .collect();
    seen.sort();
    let token = |s: &str| format!("Bearer {s}");
    let mut expected = vec![
      "POST /my/sub/message aaaaaaaaaaaaaaa".to_string(),
      "POST /message bbbbbbbbbbbbbbb".into(),
      format!("POST /other/sub/api/v2/generic/incoming {}", token("11111111-2222-3333-4444-555555555555")),
      "POST /my/sub/?token=rrrrrr ".into(),
      format!("POST /my/sub/acme/deploys {}", token("abc123")),
      "POST /new/hook ".into(),
    ];
    expected.sort();
    // Nextcloud Talk authenticates with the URL's user
    let talk: Vec<String> = seen.iter().filter(|l| l.contains("/spreed/")).cloned().collect();
    seen.retain(|l| !l.contains("/spreed/"));
    assert_eq!(seen, expected);
    assert_eq!(talk.len(), 1, "{talk:?}");
    assert!(talk[0].starts_with("POST /ocs/v2.php/apps/spreed/api/v1/chat/roomC "), "{talk:?}");
  }

  #[test]
  fn test_url_token_for_several_values_is_ignored() {
    // Upstream test_yaml_token_multi_alias: only the URL can set the three
    // Slack tokens `token` stands for
    let content = "urls:\n  - slack://T1JJ3T3L2/A1BRTD4JD/TIiajkdnlazkcOXrIdevi7FQ/#general:\n      token: ignored\n";
    let got = resolved(content);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].get("token"), None);
    assert_eq!(services(content).len(), 1);
  }

  #[test]
  fn test_common_options_from_yaml() {
    let got = resolved(
      "urls:\n  - json://localhost:\n      retry: 2\n      wait: 1.5\n      optional: yes\n      format: html\n      verify: false\n      pass: secret\n      user: me\n",
    );
    let p = &got[0];
    assert_eq!(p.get("retry"), Some("2"));
    assert_eq!(p.get("wait"), Some("1.5"));
    assert_eq!(p.get("optional"), Some("yes"));
    assert_eq!(p.get("format"), Some("html"));
    assert!(!p.verify_certificate());
    assert_eq!(p.password.as_deref(), Some("secret"));
    assert_eq!(p.user.as_deref(), Some("me"));
    let svc = crate::notify::registry::from_parsed(p).unwrap();
    let opts = svc.service_options().unwrap();
    assert_eq!(opts.retry, Some(2));
    assert_eq!(opts.optional, Some(true));
  }

  #[test]
  fn test_native_url_in_yaml() {
    let got = resolved("urls:\n  - https://discord.com/api/webhooks/0123456789/abcdef:\n      tag: d\n");
    assert_eq!(got[0].schema, "discord");
    assert_eq!(got[0].tags(), vec!["d"]);
  }

  #[test]
  fn test_apply_tokens_priority() {
    let mut parsed = ParsedUrl::parse("json://user@localhost/?format=text&to=url").unwrap();
    let mut tokens = Tokens::new();
    tokens.insert("format".into(), Value::String("markdown".into()));
    tokens.insert("retry".into(), Value::Int(3));
    tokens.insert("optional".into(), Value::Bool(true));
    tokens.insert("to".into(), Value::Sequence(vec![Value::String("a".into()), Value::String("b".into())]));
    tokens.insert("password".into(), Value::String("p%40ss".into()));
    tokens.insert("port".into(), Value::Int(8080));
    tokens.insert("x".into(), Value::String("too short".into()));
    tokens.insert("+X-Header".into(), Value::String("h".into()));
    apply_tokens(&mut parsed, tokens, 1, 1).unwrap();
    let args = args_of(&parsed);
    assert_eq!(args.get("format").map(String::as_str), Some("markdown"));
    assert_eq!(args.get("retry").map(String::as_str), Some("3"));
    assert_eq!(args.get("optional").map(String::as_str), Some("yes"));
    assert_eq!(args.get("to").map(String::as_str), Some("a,b"));
    assert!(!args.contains_key("x"));
    assert_eq!(parsed.password.as_deref(), Some("p@ss"));
    assert_eq!(parsed.port, Some(8080));
    assert_eq!(parsed.qsd_add.get("X-Header").map(String::as_str), Some("h"));
  }

  #[test]
  fn test_per_url_instances() {
    let content =
      "urls:\n  - json://localhost:\n    - to: person1@example.com\n      tag: tag1\n    - to: person2@example.com\n      tag: tag2\n    - not-a-mapping\n";
    assert_eq!(tags(content), vec![vec!["tag1"], vec!["tag2"]]);
  }

  #[test]
  fn test_includes() {
    let out = parse("include: /etc/a.yml, http://localhost/b\nurls: []");
    assert_eq!(out.includes, vec!["/etc/a.yml, http://localhost/b"].into_iter().flat_map(parse_urls).collect::<Vec<_>>());
    let out = parse("include:\n  - /etc/a.yml\n  - http://localhost/b:\n      ignored: yes\n");
    assert_eq!(out.includes, vec!["/etc/a.yml", "http://localhost/b"]);
  }

  #[test]
  fn test_asset_section() {
    let out = parse(
      "asset:\n  app_id: MyApp\n  app_desc: '  My Application  '\n  theme: dark\n  secure_logging: no\n  _private: x\n  bogus: y\nurls:\n  - json://localhost\n",
    );
    let asset = out.asset.unwrap();
    assert_eq!(asset.app_id, "MyApp");
    assert_eq!(asset.app_desc, "My Application");
    assert_eq!(asset.theme, "dark");
    assert!(!asset.secure_logging);
    assert!(parse("urls:\n  - json://localhost\n").asset.is_none());

    // Delivery settings the dispatcher reads
    let out = parse(
      "asset:\n  async_mode: no\n  http_redirects: false\n  abort_on_chain_failure: yes\n  interpret_escapes: 'yes'\n  pem_autogen: no\n  pgp_autogen: 'off'\nurls:\n  - json://localhost\n",
    );
    let asset = out.asset.unwrap();
    assert!(!asset.async_mode);
    assert!(!asset.http_redirects);
    assert!(asset.abort_on_chain_failure);
    assert!(asset.interpret_escapes);
    assert!(!asset.pem_autogen);
    assert!(!asset.pgp_autogen);
    let asset = parse("asset:\n  pem_autogen: no\nurls:\n  - json://localhost\n").asset.unwrap();
    assert!(!asset.pem_autogen && asset.pgp_autogen);
  }

  // Each case checked against upstream's ConfigBase.config_parse_yaml().
  #[test]
  fn test_asset_time_zone() {
    use crate::utils::time::Timezone;
    let zone = |content: &str| parse(&format!("asset:\n{}\nurls:\n  - json://localhost\n", content)).asset.unwrap().timezone;
    assert_eq!(zone("  tz: ' Z '"), Some(Timezone::utc()));
    assert_eq!(zone("  timezone: etc/utc"), Some(Timezone::utc()));
    // Invalid values are ignored: unknown names, paths, non-text
    for content in ["  tz: invalid", "  tz: /etc/passwd", "  tz: ../../etc/passwd", "  tz: 5", "  tz: [utc]"] {
      assert_eq!(zone(content), None, "{}", content);
    }
    // `timezone` wins over `tz`, even when it is null
    assert_eq!(zone("  timezone: ~\n  tz: utc"), None);
    // A named zone, where this machine has a database
    if crate::utils::time::zoneinfo("America/Toronto").is_some() {
      assert_eq!(zone("  tz: '  america/toronto  '").map(|z| z.name().to_string()).as_deref(), Some("America/Toronto"));
    }
    // The base asset's zone stays when the section sets none
    let base = AppriseAsset { timezone: Some(Timezone::utc()), ..Default::default() };
    let opts = LoadOptions { asset: base, ..Default::default() };
    let out = parse_yaml("asset:\n  tz: invalid\n  app_id: x\nurls: []\n", &opts).unwrap();
    assert_eq!(out.asset.unwrap().timezone, Some(Timezone::utc()));
  }

  /// Upstream only sets an asset key that already holds a boolean or text:
  /// `interpret_emojis` starts unset, so it is an invalid key unless the
  /// asset was given a value first (the CLI's `--interpret-emojis`).
  #[test]
  fn test_asset_interpret_emojis_needs_a_value_to_replace() {
    let emojis = |base: Option<bool>, value: &str| {
      let opts = LoadOptions { asset: AppriseAsset { interpret_emojis: base, ..Default::default() }, ..Default::default() };
      parse_yaml(&format!("asset:\n  interpret_emojis: {value}\nurls: []\n"), &opts).unwrap().asset.unwrap().interpret_emojis
    };
    assert_eq!(emojis(None, "yes"), None);
    assert_eq!(emojis(None, "no"), None);
    assert_eq!(emojis(Some(true), "no"), Some(false));
    assert_eq!(emojis(Some(true), "'yes'"), Some(true));
    assert_eq!(emojis(Some(false), "on"), Some(true));
    assert_eq!(emojis(Some(true), "[x]"), Some(true));
  }

  // Each case checked against upstream (PyYAML's YAML 1.1 scalars).
  #[test]
  fn test_yaml_1_1_scalars_in_settings() {
    // A bare yes is a boolean and tags nothing; a quoted one is the tag
    let none: Vec<String> = Vec::new();
    assert_eq!(tags("urls:\n  - json://a:\n      tag: yes\n  - json://b:\n      tag: 'yes'\n"), vec![none.clone(), vec!["yes".to_string()]]);
    assert_eq!(tags("tag: [yes, glob]\nurls:\n  - json://a:\n      tag: [on, x, 7, 1.5, 0755]\n"), vec![vec!["1.5", "493", "7", "glob", "x"]]);
    assert_eq!(tags("tag: no\nurls:\n  - json://a\n"), vec![none]);
    // Numbers reach the plugin as Python writes them; digit strings that
    // are not YAML 1.1 numbers (a phone number) stay as written
    let got =
      resolved("urls:\n  - json://localhost:\n      to: 0612345678\n      retry: 0x2\n      wait: 1_0.5\n      cto: 1e5\n      rto: 2.0\n      region: y\n");
    assert_eq!(got[0].get("to"), Some("0612345678"));
    assert_eq!(got[0].get("retry"), Some("2"));
    assert_eq!(got[0].get("wait"), Some("10.5"));
    assert_eq!(got[0].get("cto"), Some("1e5"));
    assert_eq!(got[0].get("rto"), Some("2.0"));
    assert_eq!(got[0].get("region"), Some("y"));
  }

  // Each case checked against upstream's URLBase.__init__().
  #[test]
  fn test_typed_url_parts() {
    // False-like values unset the part, over what the URL gave
    let got = resolved("urls:\n  - json://u:p@example.com:8080/a:\n      user: ~\n      password: no\n      port: ~\n      host: ''\n");
    let p = &got[0];
    assert_eq!((p.user.as_deref(), p.password.as_deref(), p.host.as_deref(), p.port), (None, None, None, None));
    let got = resolved("urls:\n  - json://u:p@localhost/a:\n      pass: off\n      user: []\n");
    assert_eq!((got[0].user.as_deref(), got[0].password.as_deref()), (None, None));
    // Other values that are not text: the entry does not load
    for setting in ["user: yes", "password: on", "pass: true", "host: yes", "user: [a]", "host: {a: 1}"] {
      assert!(resolved(&format!("urls:\n  - json://u@localhost/x:\n      {}\n  - json://localhost/y\n", setting)).len() == 1, "{}", setting);
    }
    // port is int() of the value
    for (setting, port) in
      [("yes", Some(1)), ("no", None), ("0x1F", Some(31)), ("1:30", Some(90)), ("8080.9", Some(8080)), ("'8080'", Some(8080)), ("abc", None)]
    {
      let got = resolved(&format!("urls:\n  - json://localhost:\n      port: {}\n", setting));
      assert_eq!(got[0].port, port, "{}", setting);
    }
    // A number is kept as its text (upstream cannot load the entry)
    let got = resolved("urls:\n  - json://localhost:\n      user: 123\n      password: 4.5\n");
    assert_eq!((got[0].user.as_deref(), got[0].password.as_deref()), (Some("123"), Some("4.5")));
  }

  #[test]
  fn test_template_pending_and_resolve() {
    let content = "
template:
  - token
  - host: localhost
urls:
  - json://${host}/${token}:
      tag: t
";
    let p = pending(content);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].names.iter().cloned().collect::<Vec<_>>(), vec!["host", "token"]);
    assert_eq!(p[0].required(), vec!["token"]);
    assert_eq!(p[0].tags(), vec!["t"]);
    assert_eq!(p[0].display_url(), "json://${HOST}/${TOKEN}");

    let env = |_: &str| None;
    assert!(p[0].resolve(&HashMap::new(), &env).is_none());
    let values = HashMap::from([("TOKEN".to_string(), "abc".to_string())]);
    let svc = p[0].resolve(&values, &env).unwrap();
    assert_eq!(svc.tags(), vec!["t"]);
  }

  #[test]
  fn test_template_env_fallback() {
    let p = pending("template:\n  - target\nurls:\n  - json://localhost/${target}\n");
    let env = |k: &str| (k == "APPRISE_TEMPLATE_TARGET").then(|| "path".to_string());
    assert!(p[0].resolve(&HashMap::new(), &env).is_some());
  }

  #[test]
  fn test_template_rules() {
    // Not in the schema
    assert!(parse("template:\n  - proto\nurls:\n  - ${proto}://localhost\n").items.is_empty());
    // Not in tags
    assert!(parse("template:\n  - t\nurls:\n  - json://localhost:\n      tag: ${t}\n").items.is_empty());
    // A marker the URL parser drops (a fragment) is refused; after '?' a
    // '#' is part of the query and is kept
    assert!(parse("template:\n  - t\nurls:\n  - json://localhost/path#${t}\n").items.is_empty());
    assert_eq!(pending("template:\n  - t\nurls:\n  - json://localhost/path?x=1#${t}\n").len(), 1);
    // Undeclared markers stay literal and the URL loads now
    let out = parse("template:\n  - t\nurls:\n  - json://localhost/${other}\n");
    assert!(matches!(out.items[0], ConfigItem::Service(_)));
    // Templates can be turned off: everything is literal
    let off = LoadOptions { allow_templates: false, ..Default::default() };
    let out = parse_yaml("template:\n  - t\nurls:\n  - json://localhost/${t}\n", &off).unwrap();
    assert!(matches!(out.items[0], ConfigItem::Service(_)));
    // Bad declarations fail the document
    assert!(parse_yaml("template: 5\nurls: []", &LoadOptions::default()).is_err());
    assert!(parse_yaml("template:\n  - bad-name\nurls: []", &LoadOptions::default()).is_err());
    assert!(parse_yaml("template:\n  - a\n  - A\nurls: []", &LoadOptions::default()).is_err());
  }

  #[test]
  fn test_template_settings_are_templated() {
    let p = pending("template:\n  - to\nurls:\n  - json://localhost:\n      to: ${to}\n");
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].names.iter().cloned().collect::<Vec<_>>(), vec!["to"]);
  }
}
