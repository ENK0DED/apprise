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
//! those parts of the URL. `tag`/`tags` give the service its tags; a
//! `?tag=`/`?tags=` on the URL itself stays with the plugin (upstream
//! c511e1d4, 9b64369c).

use std::collections::BTreeSet;
use std::sync::Arc;

use indexmap::IndexMap;
use serde_yaml::{Mapping, Value};

use super::template::{PendingService, PlaceholderMap, TemplateSchema, template_in_schema, template_references};
use super::{
  ConfigItem, GroupTags, LoadOptions, ParseOutput, apply_groups, assign_group, config_bool, is_valid_token, loggable, normalize_tag_groups, tag_has_retry,
};
use crate::asset::AppriseAsset;
use crate::error::ConfigError;
use crate::notify::registry::{self, url_schema};
use crate::utils::parse::{ParsedUrl, parse_list, parse_urls, unquote};

/// Settings written for one service, in YAML order.
type Tokens = IndexMap<String, Value>;

/// Load YAML the way PyYAML's SafeLoader does where it matters here: a
/// key repeated in one mapping keeps its last value (serde_yaml would
/// reject the document), and `<<` merge keys are applied.
fn load_yaml(content: &str) -> Result<Value, serde_yaml::Error> {
  use serde::de::{self, Deserialize, Deserializer, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};

  struct Lenient(Value);

  impl<'de> Deserialize<'de> for Lenient {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
      d.deserialize_any(LenientVisitor).map(Lenient)
    }
  }

  struct LenientVisitor;

  impl<'de> Visitor<'de> for LenientVisitor {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.write_str("any YAML value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
      Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
      Ok(Value::Number(v.into()))
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
      Ok(Value::Number(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
      Ok(Value::Number(v.into()))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
      Ok(Value::String(v.to_string()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
      Ok(Value::String(v))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
      Ok(Value::Null)
    }
    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
      Ok(Value::Null)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
      Lenient::deserialize(d).map(|l| l.0)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
      let mut out = Vec::new();
      while let Some(Lenient(v)) = seq.next_element()? {
        out.push(v);
      }
      Ok(Value::Sequence(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
      let mut out = Mapping::new();
      while let Some((Lenient(k), Lenient(v))) = map.next_entry()? {
        // Like a Python dict: the first position, the last value
        out.insert(k, v);
      }
      Ok(Value::Mapping(out))
    }
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Value, A::Error> {
      // A !tagged value: keep the value, drop the tag
      let (_tag, variant): (Lenient, _) = data.variant()?;
      variant.newtype_variant::<Lenient>().map(|l| l.0)
    }
  }

  let mut value = Lenient::deserialize(serde_yaml::Deserializer::from_str(content))?.0;
  value.apply_merge()?;
  Ok(value)
}

/// Upstream `parse_list(value, cast=str)` for a YAML value: strings are
/// split, sequences flattened, numbers converted; booleans, nulls and
/// mappings give nothing.
fn yaml_list(v: &Value) -> Vec<String> {
  let mut out = Vec::new();
  fn walk(v: &Value, out: &mut Vec<String>) {
    match v {
      Value::String(s) => out.extend(parse_list(s)),
      Value::Number(n) => out.extend(parse_list(&n.to_string())),
      Value::Sequence(seq) => seq.iter().for_each(|e| walk(e, out)),
      Value::Tagged(t) => walk(&t.value, out),
      _ => {}
    }
  }
  walk(v, &mut out);
  out.sort();
  out.dedup();
  out
}

/// A YAML scalar as the text a query argument carries.
fn yaml_text(v: &Value) -> Option<String> {
  match v {
    Value::String(s) => Some(s.clone()),
    Value::Bool(b) => Some(if *b { "yes" } else { "no" }.to_string()),
    Value::Number(n) => Some(n.to_string()),
    Value::Sequence(seq) => {
      let parts: Vec<String> = seq.iter().filter_map(yaml_text).collect();
      (!parts.is_empty()).then(|| parts.join(","))
    }
    Value::Tagged(t) => yaml_text(&t.value),
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
    Value::Tagged(t) => encode_value(&t.value, map),
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
  serde_yaml::to_string(v).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Upstream `_special_token_handler()` aliases: `pass` fills `password`
/// unless that is already set.
fn normalize_aliases(tokens: &mut Tokens) {
  if let Some(pass) = tokens.shift_remove("pass")
    && tokens.get("password").is_none_or(Value::is_null)
  {
    tokens.insert("password".to_string(), pass);
  }
}

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
  let doc = match load_yaml(content) {
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
  let get = |k: &str| root.get(Value::String(k.into()));

  // Python compares with ==, so 1.0 and true also pass.
  let version_ok = match get("version") {
    None => true,
    Some(Value::Number(n)) => n.as_f64() == Some(1.0),
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

        normalize_aliases(&mut siblings);
        match &tokens {
          Value::Sequence(entries) => {
            // One service per entry; entries that are not mappings are skipped
            for entry in entries {
              if let Value::Mapping(e) = entry {
                let mut child = tokens_of(e, &|k| k == "schema", no);
                normalize_aliases(&mut child);
                let mut merged = siblings.clone();
                merged.extend(child);
                results.push((parsed.clone(), merged));
              }
            }
          }
          Value::Mapping(e) => {
            let mut child = tokens_of(e, &|k| k == "schema", no);
            normalize_aliases(&mut child);
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

      apply_tokens(&mut parsed, tokens, no, item);
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
/// plugin's keyword arguments, over what the URL gave).
fn apply_tokens(parsed: &mut ParsedUrl, tokens: Tokens, entry: usize, item: usize) {
  for (key, value) in tokens {
    if key == "asset" || key == "schema" {
      continue;
    }
    let prefixed = key.starts_with(['+', '-', ':']);
    if !prefixed && !is_valid_token(&key) {
      tracing::warn!("Ignoring invalid token ({}) found in YAML configuration entry #{}, item #{}", key, entry, item);
      continue;
    }
    if value.is_null() {
      continue;
    }
    let Some(text) = yaml_text(&value) else {
      tracing::warn!("The {} setting in YAML configuration entry #{}, item #{} can not be expressed as a URL argument; ignoring it.", key, entry, item);
      continue;
    };
    match key.as_str() {
      // URLBase decodes these parts.
      "user" => parsed.user = Some(unquote(&text)).filter(|s| !s.is_empty()),
      "password" => parsed.password = Some(unquote(&text)).filter(|s| !s.is_empty()),
      "host" => parsed.host = Some(unquote(&text)).filter(|s| !s.is_empty()),
      "port" => {
        parsed.port = match text.trim().parse::<u16>() {
          Ok(p) => Some(p),
          Err(_) => {
            tracing::warn!("Invalid port number specified {}", text);
            None
          }
        }
      }
      _ => parsed.set_arg(&key, &text),
    }
  }
}

/// Upstream asset keys the Rust asset does not model; accepted silently.
const UNMODELLED_ASSET_KEYS: &[&str] = &[
  "default_ascii_chars",
  "default_extension",
  "default_html_color",
  "default_image_size",
  "encoding",
  "image_path_mask",
  "pem_autogen",
  "pgp_autogen",
  "storage_mode",
];

/// Apply an `asset:` section on top of the current asset.
fn parse_asset(tokens: &Mapping, base: &AppriseAsset) -> AppriseAsset {
  let mut asset = base.clone();

  let get = |k: &str| tokens.get(Value::String(k.into()));
  match get("timezone").or_else(|| get("tz")) {
    Some(Value::String(tz)) => {
      let cleaned: String = tz.chars().filter(|c| c.is_alphanumeric() || matches!(c, '_' | '/' | '-')).collect();
      if crate::utils::time::zoneinfo(&cleaned).is_none() {
        tracing::warn!("Ignored invalid timezone \"{}\"", tz);
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
      "secure_logging" | "async_mode" | "abort_on_chain_failure" | "http_redirects" | "interpret_escapes" => {
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
          _ => asset.interpret_escapes = value,
        }
      }
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
    // A tagged scalar keeps its value
    assert_eq!(tags("urls:\n  - json://a:\n      tag: !custom mine\n"), vec![vec!["mine"]]);
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
    tokens.insert("retry".into(), Value::Number(3.into()));
    tokens.insert("optional".into(), Value::Bool(true));
    tokens.insert("to".into(), Value::Sequence(vec![Value::String("a".into()), Value::String("b".into())]));
    tokens.insert("password".into(), Value::String("p%40ss".into()));
    tokens.insert("port".into(), Value::Number(8080.into()));
    tokens.insert("x".into(), Value::String("too short".into()));
    tokens.insert("+X-Header".into(), Value::String("h".into()));
    apply_tokens(&mut parsed, tokens, 1, 1);
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
    let out =
      parse("asset:\n  async_mode: no\n  http_redirects: false\n  abort_on_chain_failure: yes\n  interpret_escapes: 'yes'\nurls:\n  - json://localhost\n");
    let asset = out.asset.unwrap();
    assert!(!asset.async_mode);
    assert!(!asset.http_redirects);
    assert!(asset.abort_on_chain_failure);
    assert!(asset.interpret_escapes);
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
