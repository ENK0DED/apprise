//! Configuration sources, ported from upstream `apprise/config/*.py` and
//! `apprise/apprise_config.py`.
//!
//! A source is a local file (a plain path or `file://`), an `http(s)://`
//! URL, or text handed in directly. Its content is TEXT (one URL per line)
//! or YAML. The format comes from `?format=` on a `file://`/`http(s)://`
//! URL, then the file extension (`.yml`/`.yaml`) or the HTTP
//! `Content-Type`, and is TEXT otherwise; text handed in directly is
//! detected from its first meaningful line. Malformed YAML fails closed:
//! it is never re-read as TEXT.
//!
//! `include` entries are followed `recursion` levels deep (zero ignores
//! them). A local file may include local files and web sources; a web
//! source may include web sources only, unless `insecure_includes` is set.

#[cfg(test)]
mod apprise_tests;
pub mod file;
pub mod http;
pub mod template;
pub mod text;
#[cfg(feature = "yaml")]
pub mod yaml;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use indexmap::IndexMap;
use regex_lite::Regex;

use crate::asset::AppriseAsset;
use crate::error::ConfigError;
use crate::notify::Notify;
use crate::notify::registry::url_schema;
use crate::utils::cwe312::cwe312_url;
use crate::utils::parse::{ParsedUrl, parse_bool, parse_list, unquote};

pub use template::PendingService;

/// Largest configuration read, as upstream (128KB).
pub const MAX_BUFFER_SIZE: usize = 131072;

/// Configuration formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
  Text,
  Yaml,
}

impl std::str::FromStr for ConfigFormat {
  type Err = ();

  /// Upstream accepts exactly `text` and `yaml`, in any case.
  fn from_str(s: &str) -> Result<Self, ()> {
    match s.trim().to_lowercase().as_str() {
      "text" => Ok(ConfigFormat::Text),
      "yaml" => Ok(ConfigFormat::Yaml),
      _ => Err(()),
    }
  }
}

/// One entry a configuration produced.
pub enum ConfigItem {
  /// A loaded notification service.
  Service(Box<dyn Notify>),
  /// A YAML entry still waiting for template values.
  Pending(Box<PendingService>),
}

/// Settings that apply while reading one configuration tree.
#[derive(Debug, Clone)]
pub struct LoadOptions {
  /// How many levels of `include` to follow (upstream `recursion`).
  pub recursion: u32,
  /// Let any source include a local file (upstream `insecure_includes`).
  pub insecure_includes: bool,
  /// Honour YAML `template:` sections (upstream `asset.allow_templates`).
  pub allow_templates: bool,
  /// The asset the YAML `asset:` section starts from.
  pub asset: AppriseAsset,
}

impl Default for LoadOptions {
  fn default() -> Self {
    Self { recursion: 1, insecure_includes: false, allow_templates: true, asset: AppriseAsset::default() }
  }
}

/// What one configuration tree produced.
#[derive(Default)]
pub struct Loaded {
  pub items: Vec<ConfigItem>,
  /// The asset after any YAML `asset:` sections, when one was present.
  pub asset: Option<AppriseAsset>,
}

/// What one configuration document produced, before its includes.
#[derive(Default)]
pub struct ParseOutput {
  pub items: Vec<ConfigItem>,
  /// `include` entries, in order.
  pub includes: Vec<String>,
  /// The asset after a YAML `asset:` section, when one was present.
  pub asset: Option<AppriseAsset>,
}

/// Where a configuration came from; decides relative includes and which
/// sources it may include.
#[derive(Debug, Clone)]
enum Origin {
  File(PathBuf),
  Http,
  Memory,
}

impl Origin {
  /// Directory relative includes are resolved against (upstream
  /// `config_path`: the file's directory, else the working directory).
  fn config_dir(&self) -> PathBuf {
    match self {
      Origin::File(path) => path.parent().map(Path::to_path_buf).unwrap_or_default(),
      _ => std::env::current_dir().unwrap_or_default(),
    }
  }
}

/// Mask a URL for logs unless secure logging was turned off.
pub(crate) fn loggable(url: &str, asset: &AppriseAsset) -> String {
  if asset.secure_logging { cwe312_url(url) } else { url.to_string() }
}

/// Upstream `ConfigBase.detect_config_format()`: the format of text handed
/// in directly, from its first line that is not a comment. `None` when a
/// line matches neither format.
pub fn detect_config_format(content: &str) -> Option<ConfigFormat> {
  static RE: OnceLock<Regex> = OnceLock::new();
  let re = RE.get_or_init(|| {
    Regex::new(
      r"(?i)^\s*(?P<line>([;#]+(?P<comment>.*))|(?P<text>((?P<tag>[ \t,a-z0-9_:-]+)=)?[a-z0-9]+://.*)|((?P<yaml>[a-z0-9]+):.*)|(?P<assign>[ \t,a-z0-9_:-]+=[a-z0-9, \t_-]+))?$",
    )
    .unwrap()
  });

  let mut format = None;
  for (no, line) in split_lines(content).enumerate() {
    let Some(c) = re.captures(line) else {
      tracing::error!("Undetectable Apprise configuration found based on line {}.", no + 1);
      return None;
    };
    if c.name("yaml").is_some() {
      tracing::debug!("Detected YAML configuration based on line {}.", no + 1);
      return Some(ConfigFormat::Yaml);
    }
    if c.name("text").is_some() || c.name("assign").is_some() {
      tracing::debug!("Detected TEXT configuration based on line {}.", no + 1);
      return Some(ConfigFormat::Text);
    }
    // A comment or blank line leans towards TEXT
    format = Some(ConfigFormat::Text);
  }
  format
}

/// `re.split(r"\r*\n", content)`.
pub(crate) fn split_lines(content: &str) -> impl Iterator<Item = &str> {
  content.split('\n').map(|l| l.trim_end_matches('\r'))
}

/// Parse a document in the given format.
pub fn config_parse(content: &str, format: ConfigFormat, opts: &LoadOptions) -> Result<ParseOutput, ConfigError> {
  match format {
    ConfigFormat::Text => text::parse_text(content, &opts.asset),
    ConfigFormat::Yaml => {
      #[cfg(feature = "yaml")]
      return yaml::parse_yaml(content, opts);
      #[cfg(not(feature = "yaml"))]
      return Err(ConfigError::InvalidFormat("YAML support not compiled in (rebuild with --features yaml)".into()));
    }
  }
}

/// Load a configuration source (a path, `file://`, `http://` or
/// `https://`) and everything it includes.
pub async fn load_config(source: &str, opts: &LoadOptions) -> Result<Loaded, ConfigError> {
  let mut loaded = Loaded::default();
  let source = resolve_source(source, None, opts)?;
  load_source(source, opts, &mut loaded).await?;
  Ok(loaded)
}

/// Load configuration text handed in directly (upstream
/// `AppriseConfig.add_config()`). The format is detected when not given.
/// Includes follow the same rules; a local file may only be included with
/// `insecure_includes`.
pub async fn load_config_text(content: &str, format: Option<ConfigFormat>, opts: &LoadOptions) -> Result<Loaded, ConfigError> {
  let Some(format) = format.or_else(|| detect_config_format(content)) else {
    return Err(ConfigError::InvalidFormat("The format of the configuration could not be detected.".into()));
  };
  let mut loaded = Loaded::default();
  let parsed = config_parse(content, format, opts)?;
  follow(parsed, Origin::Memory, opts, &mut loaded).await;
  Ok(loaded)
}

/// A source resolved to something we can read.
#[derive(Debug)]
struct Source {
  url: String,
  kind: SourceKind,
  format: Option<ConfigFormat>,
}

#[derive(Debug)]
enum SourceKind {
  File { path: PathBuf, encoding: String },
  Http { parsed: Box<ParsedUrl> },
}

impl Source {
  fn origin(&self) -> Origin {
    match &self.kind {
      SourceKind::File { path, .. } => Origin::File(path.clone()),
      SourceKind::Http { .. } => Origin::Http,
    }
  }
}

/// Turn a source string into a [`Source`] (upstream
/// `AppriseConfig.instantiate()` and each source's `parse_url()`). A plain
/// path is a local file; for an include it is relative to the including
/// file's directory.
fn resolve_source(source: &str, parent: Option<&Origin>, opts: &LoadOptions) -> Result<Source, ConfigError> {
  let source = source.trim();
  let Some(schema) = url_schema(source) else {
    // A plain path; '?' is part of the name here, as upstream quotes it.
    let mut path = PathBuf::from(source);
    if let Some(origin) = parent
      && !path.is_absolute()
      && !source.starts_with('~')
    {
      path = origin.config_dir().join(path);
    }
    let path = absolute(&crate::utils::disk::path_decode(&path.to_string_lossy()));
    return Ok(Source { url: format!("file://{}", path.display()), kind: SourceKind::File { path, encoding: "utf-8".into() }, format: None });
  };

  let parsed = match schema.as_str() {
    "file" => ParsedUrl::parse(source),
    "http" | "https" => ParsedUrl::parse_with(source, crate::utils::parse::ParseOptions { verify_host: true, ..Default::default() }),
    other => return Err(ConfigError::Other(format!("Unsupported schema {}.", other))),
  };
  let Some(parsed) = parsed else {
    return Err(ConfigError::Other(format!("Unparseable URL {}.", loggable(source, &opts.asset))));
  };

  let mut format = None;
  if let Some(f) = parsed.get("format") {
    match f.parse::<ConfigFormat>() {
      Ok(f) => format = Some(f),
      Err(_) => tracing::warn!("Unsupported format specified {}", f),
    }
  }

  // ?cache= only controls in-memory reuse upstream; it is still validated.
  if let Some(cache) = parsed.get("cache")
    && let Ok(n) = cache.trim().parse::<i64>()
    && n < 0
  {
    return Err(ConfigError::Other(format!("A negative cache value ({}) was specified.", cache)));
  }

  let kind = if schema == "file" {
    // file://<path>[?...]: everything up to the '?' is the path.
    let after = &source[source.find("://").unwrap() + 3..];
    let raw_path = after.split('?').next().unwrap_or_default();
    if raw_path.is_empty() {
      return Err(ConfigError::Other(format!("Unparseable URL {}.", loggable(source, &opts.asset))));
    }
    let path = absolute(&crate::utils::disk::path_decode(&unquote(raw_path)));
    let encoding = parsed.get("encoding").map(|e| e.to_string()).unwrap_or_else(|| "utf-8".into());
    SourceKind::File { path, encoding }
  } else {
    SourceKind::Http { parsed: Box::new(parsed) }
  };

  Ok(Source { url: source.to_string(), kind, format })
}

fn absolute(path: &Path) -> PathBuf {
  if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf()) }
}

/// Read, parse and follow one source.
fn load_source<'a>(
  source: Source,
  opts: &'a LoadOptions,
  loaded: &'a mut Loaded,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ConfigError>> + Send + 'a>> {
  Box::pin(async move {
    let (content, detected) = match &source.kind {
      SourceKind::File { path, encoding } => file::read_file(path, encoding).await?,
      SourceKind::Http { parsed } => http::read_http(parsed, &opts.asset).await?,
    };
    let format = source.format.or(detected).unwrap_or(ConfigFormat::Text);

    let mut opts = opts.clone();
    if let Some(asset) = &loaded.asset {
      opts.asset = asset.clone();
    }
    let parsed = config_parse(&content, format, &opts)?;
    let origin = source.origin();
    let before = loaded.items.len();
    follow(parsed, origin, &opts, loaded).await;

    if loaded.items.len() > before {
      tracing::debug!("Loaded {} entries from {}", loaded.items.len() - before, loggable(&source.url, &opts.asset));
    } else {
      tracing::warn!("Failed to load Apprise configuration from {}", loggable(&source.url, &opts.asset));
    }
    Ok(())
  })
}

/// Keep a document's entries and load its includes (upstream
/// `ConfigBase.services()`).
async fn follow(parsed: ParseOutput, origin: Origin, opts: &LoadOptions, loaded: &mut Loaded) {
  loaded.items.extend(parsed.items);
  if parsed.asset.is_some() {
    loaded.asset = parsed.asset;
  }

  for include in parsed.includes {
    let asset = loaded.asset.clone().unwrap_or_else(|| opts.asset.clone());
    if opts.recursion == 0 {
      tracing::debug!("Recursion limit reached; ignoring Include URL: {}", loggable(&include, &asset));
      continue;
    }

    let source = match resolve_source(&include, Some(&origin), opts) {
      Ok(s) => s,
      Err(ConfigError::Other(msg)) if msg.starts_with("Unsupported schema") => {
        tracing::error!("Unsupported include schema {}.", url_schema(&include).unwrap_or_default());
        continue;
      }
      Err(e) => {
        tracing::error!("{}", e);
        continue;
      }
    };

    // file:// may only be included by a file (STRICT); web sources may be
    // included by anything (ALWAYS).
    let strict_violation = matches!(source.kind, SourceKind::File { .. }) && !matches!(origin, Origin::File(_)) && !opts.insecure_includes;
    if strict_violation {
      tracing::warn!("Including file:// based configuration is prohibited. Ignoring URL {}", loggable(&include, &asset));
      continue;
    }

    let child = LoadOptions { recursion: opts.recursion - 1, asset, ..opts.clone() };
    if let Err(e) = load_source(source, &child, loaded).await {
      tracing::error!("Could not load include URL: {}", loggable(&include, &child.asset));
      tracing::debug!("Loading Exception: {}", e);
    }
  }
}

/// Upstream `VALID_TOKEN.match()`: a YAML setting name must start with two
/// characters out of `[a-z0-9_]` (the first not `_`).
pub(crate) fn is_valid_token(key: &str) -> bool {
  let b = key.as_bytes();
  b.len() >= 2 && b[0].is_ascii_alphanumeric() && (b[1].is_ascii_alphanumeric() || b[1] == b'_')
}

/// Whether a raw tag carries a retry count (`tag:3`), which a service tag
/// may not (upstream `AppriseTag.parse(t).retry is not None`).
pub(crate) fn tag_has_retry(tag: &str) -> bool {
  static RE: OnceLock<Regex> = OnceLock::new();
  let re = RE.get_or_init(|| Regex::new(r"(?i)^(?:(?P<priority>[0-9]+):)?(?P<tag>[a-z0-9][a-z0-9_-]*)(?::(?P<retry>[0-9]+))?$").unwrap());
  re.captures(tag.trim()).is_some_and(|c| c.name("retry").is_some())
}

/// Group name -> member tags, in the order groups were first defined.
pub(crate) type GroupTags = IndexMap<String, BTreeSet<String>>;

/// Add `tags` to each group in `groups` (never a group to itself, as the
/// TEXT parser does).
pub(crate) fn assign_group(group_tags: &mut GroupTags, groups: &[String], tags: &[String], exclude_self: bool) {
  for group in groups {
    let entry = group_tags.entry(group.clone()).or_default();
    entry.extend(tags.iter().filter(|t| !exclude_self || *t != group).cloned());
  }
}

/// Upstream `ConfigBase.__normalize_tag_groups()`: expand groups that
/// contain other groups so each group lists the plain tags it reaches.
/// Cycles are cut by an ignore set; a group left with nothing is dropped
/// with a warning.
pub(crate) fn normalize_tag_groups(group_tags: &mut GroupTags) {
  let names: BTreeSet<String> = group_tags.keys().cloned().collect();
  let mut work: BTreeMap<String, BTreeSet<String>> = group_tags.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

  fn expand(tags: &[String], ignore: &mut BTreeSet<String>, work: &mut BTreeMap<String, BTreeSet<String>>, names: &BTreeSet<String>) -> BTreeSet<String> {
    let mut results = BTreeSet::new();
    for tag in tags {
      if ignore.contains(tag) {
        continue;
      }
      let members = work.entry(tag.clone()).or_default().clone();
      results.extend(members.difference(names).cloned());
      for gtag in members.intersection(names) {
        if ignore.contains(gtag) {
          continue;
        }
        ignore.insert(tag.clone());
        let expanded = expand(std::slice::from_ref(gtag), ignore, work, names);
        work.insert(gtag.clone(), expanded.clone());
        results.extend(expanded);
        ignore.remove(tag);
      }
    }
    results
  }

  for name in &names {
    let mut ignore = BTreeSet::new();
    let expanded = expand(std::slice::from_ref(name), &mut ignore, &mut work, &names);
    let entry = work.entry(name.clone()).or_default();
    entry.extend(expanded);
    if entry.is_empty() {
      tracing::warn!("The group {} has no tags assigned to it", name);
      work.remove(name);
    }
  }

  // Keep the original definition order for applying the groups.
  let order: Vec<String> = group_tags.keys().cloned().collect();
  group_tags.clear();
  for name in order {
    if let Some(members) = work.remove(&name) {
      group_tags.insert(name, members);
    }
  }
}

/// Add every group that shares a tag with `tags` (exact string match, as
/// upstream compares the raw strings).
pub(crate) fn apply_groups(tags: &mut BTreeSet<String>, group_tags: &GroupTags) {
  for (group, members) in group_tags {
    if tags.iter().any(|t| members.contains(t)) {
      tags.insert(group.clone());
    }
  }
}

/// Upstream `parse_list()` over a string, returning a set.
pub(crate) fn tag_set(text: &str) -> BTreeSet<String> {
  parse_list(text).into_iter().collect()
}

/// Read a boolean config value the upstream way.
pub(crate) fn config_bool(v: &str) -> bool {
  parse_bool(v)
}

/// Lowercased `HashMap` view used by tests.
#[cfg(test)]
pub(crate) fn groups_of(g: &GroupTags) -> std::collections::HashMap<String, Vec<String>> {
  g.iter().map(|(k, v)| (k.clone(), v.iter().cloned().collect())).collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_detect_config_format() {
    assert_eq!(detect_config_format("json://localhost"), Some(ConfigFormat::Text));
    assert_eq!(detect_config_format("# comment\n\ntag=json://localhost"), Some(ConfigFormat::Text));
    assert_eq!(detect_config_format("2:endpoint=ntfy://topic"), Some(ConfigFormat::Text));
    assert_eq!(detect_config_format("family=me,wife"), Some(ConfigFormat::Text));
    assert_eq!(detect_config_format("# comment\nurls:\n  - json://localhost"), Some(ConfigFormat::Yaml));
    assert_eq!(detect_config_format("version: 1"), Some(ConfigFormat::Yaml));
    assert_eq!(detect_config_format("# only a comment"), Some(ConfigFormat::Text));
    assert_eq!(detect_config_format(""), Some(ConfigFormat::Text));
    assert_eq!(detect_config_format("!! not valid"), None);
  }

  #[test]
  fn test_config_format_from_str() {
    assert_eq!("YAML".parse::<ConfigFormat>(), Ok(ConfigFormat::Yaml));
    assert_eq!("text".parse::<ConfigFormat>(), Ok(ConfigFormat::Text));
    assert!("yml".parse::<ConfigFormat>().is_err());
  }

  #[test]
  fn test_valid_token() {
    assert!(is_valid_token("to"));
    assert!(is_valid_token("smtp_host"));
    assert!(is_valid_token("ab-c"));
    assert!(!is_valid_token("x"));
    assert!(!is_valid_token("_x"));
    assert!(!is_valid_token("+header"));
    assert!(!is_valid_token("a-b"));
  }

  #[test]
  fn test_tag_has_retry() {
    assert!(tag_has_retry("alerts:3"));
    assert!(tag_has_retry("2:alerts:3"));
    assert!(!tag_has_retry("2:alerts"));
    assert!(!tag_has_retry("alerts"));
    assert!(!tag_has_retry("not a tag:3"));
  }

  #[test]
  fn test_normalize_groups_nested() {
    let mut g = GroupTags::new();
    assign_group(&mut g, &["groupA".into(), "groupB".into()], &["tagB".into(), "tagC".into()], true);
    assign_group(&mut g, &["groupC".into()], &["groupA".into()], true);
    assign_group(&mut g, &["groupD".into()], &["groupE".into()], true);
    assign_group(&mut g, &["groupE".into()], &["tagD".into()], true);
    assign_group(&mut g, &["loner".into()], &["loner".into()], true);
    normalize_tag_groups(&mut g);
    let g = groups_of(&g);
    assert_eq!(g["groupC"], vec!["groupA", "tagB", "tagC"]);
    assert_eq!(g["groupD"], vec!["groupE", "tagD"]);
    // A group that only names itself has nothing and is dropped
    assert!(!g.contains_key("loner"));
  }

  #[test]
  fn test_normalize_groups_cycle_terminates() {
    // Upstream's result for cycles depends on Python's set order; it must
    // terminate and stay deterministic here.
    let build = || {
      let mut g = GroupTags::new();
      assign_group(&mut g, &["admins".into()], &["tagA".into(), "devops".into()], true);
      assign_group(&mut g, &["devops".into()], &["tagB".into(), "admins".into()], true);
      assign_group(&mut g, &["a".into()], &["b".into()], true);
      assign_group(&mut g, &["b".into()], &["a".into()], true);
      normalize_tag_groups(&mut g);
      groups_of(&g)
    };
    let first = build();
    assert_eq!(first, build());
    assert!(first["admins"].contains(&"tagA".to_string()));
  }

  fn opts(recursion: u32) -> LoadOptions {
    LoadOptions { recursion, ..Default::default() }
  }

  fn tags_of(loaded: &Loaded) -> Vec<Vec<String>> {
    loaded
      .items
      .iter()
      .map(|i| match i {
        ConfigItem::Service(s) => s.tags(),
        ConfigItem::Pending(p) => p.tags(),
      })
      .collect()
  }

  #[tokio::test]
  async fn test_includes_follow_recursion() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("top.conf"), "include sub.conf\ninclude ./sub.conf\njson://top").unwrap();
    std::fs::write(dir.path().join("sub.conf"), "include deeper.conf\ntagS=json://sub").unwrap();
    std::fs::write(dir.path().join("deeper.conf"), "tagD=json://deeper").unwrap();
    let top = dir.path().join("top.conf").to_string_lossy().to_string();

    // recursion 0 ignores includes; 1 reads them (twice here, as upstream)
    assert_eq!(load_config(&top, &opts(0)).await.unwrap().items.len(), 1);
    let loaded = load_config(&top, &opts(1)).await.unwrap();
    assert_eq!(tags_of(&loaded), vec![vec![], vec!["tagS".to_string()], vec!["tagS".to_string()]]);
    // 2 follows the include inside the include
    assert_eq!(load_config(&top, &opts(2)).await.unwrap().items.len(), 5);
  }

  #[tokio::test]
  async fn test_format_selection() {
    let dir = tempfile::tempdir().unwrap();
    let yaml = "urls:\n  - json://localhost\n";
    std::fs::write(dir.path().join("a.yml"), yaml).unwrap();
    std::fs::write(dir.path().join("a.conf"), yaml).unwrap();
    std::fs::write(dir.path().join("bad.yml"), "urls: [json://localhost\n").unwrap();
    let path = |n: &str| dir.path().join(n).to_string_lossy().to_string();

    // By extension
    assert_eq!(load_config(&path("a.yml"), &opts(1)).await.unwrap().items.len(), 1);
    // YAML in a TEXT file is not detected, so the TEXT parse fails
    assert!(load_config(&path("a.conf"), &opts(1)).await.is_err());
    // ?format= works on a file:// URL
    let url = format!("file://{}?format=yaml", path("a.conf"));
    assert_eq!(load_config(&url, &opts(1)).await.unwrap().items.len(), 1);
    // On a plain path the '?' is part of the file name, as upstream
    assert!(load_config(&format!("{}?format=yaml", path("a.conf")), &opts(1)).await.is_err());
    // Malformed YAML fails closed
    assert!(load_config(&path("bad.yml"), &opts(1)).await.is_err());
    // Unsupported schemas
    assert!(load_config("ftp://example.com/cfg", &opts(1)).await.is_err());
  }

  #[tokio::test]
  async fn test_http_source_may_not_include_files() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("local.conf");
    std::fs::write(&local, "json://local").unwrap();

    let server = MockServer::start().await;
    let body = format!("include {}\ninclude {}/other\njson://remote", local.display(), server.uri());
    Mock::given(method("POST")).and(path("/cfg")).respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/plain")).mount(&server).await;
    Mock::given(method("POST")).and(path("/other")).respond_with(ResponseTemplate::new(200).set_body_raw("x=json://other", "text/plain")).mount(&server).await;

    let url = format!("{}/cfg", server.uri());
    let loaded = load_config(&url, &opts(1)).await.unwrap();
    // The local file is refused; the web include is allowed
    assert_eq!(tags_of(&loaded), vec![vec![], vec!["x".to_string()]]);

    let insecure = LoadOptions { insecure_includes: true, ..opts(1) };
    assert_eq!(load_config(&url, &insecure).await.unwrap().items.len(), 3);
  }

  #[tokio::test]
  async fn test_text_handed_in_directly() {
    let loaded = load_config_text("urls:\n  - json://localhost\n", None, &opts(1)).await.unwrap();
    assert_eq!(loaded.items.len(), 1);
    let loaded = load_config_text("t=json://localhost", None, &opts(1)).await.unwrap();
    assert_eq!(tags_of(&loaded), vec![vec!["t".to_string()]]);
    assert!(load_config_text("!!!", None, &opts(1)).await.is_err());
    // A file can not be included from memory without insecure_includes
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("local.conf");
    std::fs::write(&local, "json://local").unwrap();
    let content = format!("include {}\njson://mem", local.display());
    // An include line alone does not reveal the format (upstream too)
    assert!(load_config_text(&content, None, &opts(1)).await.is_err());
    let text = Some(ConfigFormat::Text);
    assert_eq!(load_config_text(&content, text, &opts(1)).await.unwrap().items.len(), 1);
    let insecure = LoadOptions { insecure_includes: true, ..opts(1) };
    assert_eq!(load_config_text(&content, text, &insecure).await.unwrap().items.len(), 2);
  }

  #[test]
  fn test_apply_groups_exact_match() {
    let mut g = GroupTags::new();
    assign_group(&mut g, &["grp".into()], &["tagA".into()], true);
    normalize_tag_groups(&mut g);
    let mut tags: BTreeSet<String> = ["tagA".to_string()].into();
    apply_groups(&mut tags, &g);
    assert!(tags.contains("grp"));
    // Raw strings are compared, so a priority prefix or another case differs
    let mut tags: BTreeSet<String> = ["2:tagA".to_string(), "TAGA".to_string()].into();
    apply_groups(&mut tags, &g);
    assert!(!tags.contains("grp"));
  }
}
