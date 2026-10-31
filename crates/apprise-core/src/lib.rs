//! Apprise — Push notifications that work with everything.
//!
//! This is the core library providing 127 notification service plugins,
//! configuration parsing, attachment handling, and notification dispatch.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use apprise_core::{Apprise, NotifyContext, NotifyType, NotifyFormat};
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut apprise = Apprise::new();
//!     apprise.add("ntfy://mytopic");
//!     apprise.add("slack://TokenA/TokenB/TokenC");
//!
//!     let ctx = NotifyContext {
//!         body: "Hello from Rust!".into(),
//!         title: "Test".into(),
//!         ..Default::default()
//!     };
//!
//!     let result = apprise.notify(&ctx).await;
//!     println!("{}: sent to {}/{} services", result.status, result.succeeded, result.total);
//! }
//! ```

pub mod asset;
pub mod attachment;
pub mod config;
pub mod conversion;
mod dispatch;
pub mod error;
pub mod notify;
mod prepare;
pub mod result;
pub mod storage;
pub mod tag;
pub mod types;
pub mod utils;

// Public re-exports for convenience
pub use asset::{AppriseAsset, NotifyImageSize};
pub use error::{AttachError, ConfigError, NotifyError};
pub use notify::registry;
pub use notify::{Attachment, Notify, NotifyContext, OverflowMode, ServiceDetails};
pub use result::{NotifyAttempt, ResultStatus, SendResult, ServiceResult};
pub use tag::{AppriseTag, TagFilter};
pub use types::{NotifyFormat, NotifyType, StorageMode};

use std::sync::Arc;

use utils::parse::{parse_list, parse_urls};

/// Delivery tracking for plugins that send one notification to several
/// targets: with retries enabled, a retry can skip targets that already
/// accepted the message, and reuse work (uploads, tokens) from an earlier
/// attempt.
pub mod delivery {
  pub use crate::dispatch::{is_delivered, mark_delivered, recall, remember};
}

/// Install the ring crypto provider for rustls. Safe to call multiple times.
pub fn ensure_crypto_provider() {
  let _ = rustls::crypto::ring::default_provider().install_default();
}

/// The main Apprise orchestrator.
///
/// Collects notification services from URLs and config files, then dispatches
/// notifications to all matching services with format conversion, overflow
/// handling, and tag filtering.
///
/// Cloning is cheap: services are shared, as are the persistent stores.
#[derive(Clone)]
pub struct Apprise {
  entries: Vec<Entry>,
  asset: AppriseAsset,
  tag_filter: TagFilter,
  timeout: Option<std::time::Duration>,
  stores: storage::StoreRegistry,
  location: attachment::ContentLocation,
  config: ConfigState,
}

/// One loaded entry, in load order.
#[derive(Clone)]
enum Entry {
  Service(Arc<dyn Notify>),
  /// A configuration entry with `${NAME}` variables, built from the values
  /// current each time it is used (upstream keeps its `NotifyTemplate`).
  Template(Arc<config::TemplateEntry>),
}

impl Entry {
  fn tags(&self) -> Vec<String> {
    match self {
      Self::Service(svc) => svc.tags(),
      Self::Template(t) => t.pending.tags(),
    }
  }
}

/// A loaded entry as the current template values make it (see
/// [`Apprise::select`]).
pub enum Selected<'a> {
  /// A service ready to notify. `template` is the configuration entry it
  /// was built from, if it was.
  Service { service: Arc<dyn Notify>, template: Option<&'a config::PendingService> },
  /// A configuration entry the current values cannot build: one is
  /// missing, or the service refused the URL they made.
  Unresolved(&'a config::PendingService),
}

/// Configuration and template settings of an [`Apprise`] instance.
#[derive(Default, Clone)]
struct ConfigState {
  /// Values for `${NAME}` template variables.
  template_values: std::collections::HashMap<String, String>,
  /// Ignore YAML `template:` sections (upstream `allow_templates=False`).
  templates_disabled: bool,
  /// Let any configuration include a local file.
  insecure_includes: bool,
}

impl Default for Apprise {
  fn default() -> Self {
    Self::new()
  }
}

impl Apprise {
  /// Create a new empty Apprise instance.
  pub fn new() -> Self {
    Self::with_asset(AppriseAsset::default())
  }

  /// Create with a custom asset (branding, delivery and storage settings).
  pub fn with_asset(asset: AppriseAsset) -> Self {
    ensure_crypto_provider();
    Self {
      entries: Vec::new(),
      asset,
      tag_filter: TagFilter::All,
      timeout: None,
      stores: storage::StoreRegistry::default(),
      location: attachment::ContentLocation::Local,
      config: ConfigState::default(),
    }
  }

  /// Set the asset (branding/theming) for this instance.
  pub fn set_asset(&mut self, asset: AppriseAsset) {
    self.asset = asset;
  }

  /// Add the notification services in `urls`: one URL, or several
  /// separated by commas, semicolons and/or whitespace (upstream
  /// `Apprise.add()` with `parse_urls()`). Returns `true` only when every
  /// URL loaded; the ones that did load are kept either way.
  pub fn add(&mut self, urls: &str) -> bool {
    self.add_with_tags(urls, &[])
  }

  /// [`Apprise::add`], giving each new service `tags` (raw strings; a
  /// `priority:tag` form is kept as written). Tags are never read from the
  /// URL itself: `?tag=`/`?tags=` belong to the plugin.
  pub fn add_with_tags(&mut self, urls: &str, tags: &[&str]) -> bool {
    let entries = parse_urls(urls);
    if entries.is_empty() {
      return false;
    }
    let tags: Vec<String> = parse_list(&tags.join(","));
    let mut all = true;
    for url in entries {
      let service = registry::parse_service_url(&url).and_then(|mut parsed| {
        parsed.assigned_tags = Some(tags.clone());
        registry::from_parsed(&parsed)
      });
      match service {
        Some(svc) => self.entries.push(Entry::Service(svc.into())),
        None => all = false,
      }
    }
    all
  }

  /// Add every URL in a list string (see [`Apprise::add`]). Returns the
  /// number of services added.
  pub fn add_urls(&mut self, raw: &str) -> usize {
    let before = self.entries.len();
    self.add(raw);
    self.entries.len() - before
  }

  /// Load services from a configuration source: a local path, `file://`,
  /// `http://` or `https://` URL. `recursion` is how many levels of
  /// `include` to follow (zero ignores them).
  ///
  /// Returns the number of services loaded. A YAML entry with `${NAME}`
  /// variables is kept as written and built from the values current each
  /// time it is used (see [`Apprise::set_template_values`]); it counts
  /// here when the current values build it.
  pub async fn add_config(&mut self, source: &str, recursion: u32) -> Result<usize, ConfigError> {
    let loaded = config::load_config(source, &self.load_options(recursion)).await?;
    Ok(self.take_loaded(loaded))
  }

  /// Load services from configuration text (upstream
  /// `AppriseConfig.add_config()`). The format is detected when `None`.
  pub async fn add_config_text(&mut self, content: &str, format: Option<config::ConfigFormat>, recursion: u32) -> Result<usize, ConfigError> {
    let loaded = config::load_config_text(content, format, &self.load_options(recursion)).await?;
    Ok(self.take_loaded(loaded))
  }

  /// Load every default configuration file that exists (upstream CLI
  /// `DEFAULT_CONFIG_PATHS`). Returns the number of services loaded.
  pub async fn load_default_configs(&mut self, recursion: u32) -> usize {
    let mut count = 0;
    for path in default_config_paths() {
      if path.is_file() {
        match self.add_config(&path.to_string_lossy(), recursion).await {
          Ok(n) => count += n,
          Err(e) => tracing::warn!("Could not load configuration {}: {}", path.display(), e),
        }
      }
    }
    count
  }

  /// Supply values for `${NAME}` template variables (names are
  /// case-insensitive), replacing any given before. They apply to every
  /// YAML entry with variables, loaded before or after; returns how many
  /// of the loaded ones they build. A value not given here falls back to
  /// the configuration default, then to `APPRISE_TEMPLATE_<NAME>`, read
  /// each time an entry is used.
  pub fn set_template_values(&mut self, values: std::collections::HashMap<String, String>) -> Result<usize, ConfigError> {
    config::template::validate_overrides(&values).map_err(|e| ConfigError::Other(e.to_string()))?;
    self.config.template_values = values;
    Ok(self.entries.iter().filter(|e| matches!(e, Entry::Template(t) if self.build(t, false).is_some())).count())
  }

  /// Number of loaded services the current tag filter selects.
  pub fn filtered_len(&self) -> usize {
    self.services_for(&self.tag_filter, false).len()
  }

  /// YAML entries the current template values cannot build.
  pub fn pending_templates(&self) -> Vec<&config::PendingService> {
    self.unresolved(&TagFilter::All).collect()
  }

  /// The entries the current template values cannot build that `filter`
  /// selects. A notification counts each of them as a failed service, as
  /// upstream does for an entry it cannot resolve.
  pub fn pending_matching<'a>(&'a self, filter: &'a TagFilter) -> impl Iterator<Item = &'a config::PendingService> + 'a {
    self.unresolved(filter)
  }

  fn unresolved<'a>(&'a self, filter: &'a TagFilter) -> impl Iterator<Item = &'a config::PendingService> + 'a {
    self.matching(filter).filter_map(|e| match e {
      Entry::Template(t) if self.build(t, false).is_none() => Some(&t.pending),
      _ => None,
    })
  }

  /// The entries `filter` selects, in load order.
  fn matching<'a>(&'a self, filter: &'a TagFilter) -> impl Iterator<Item = &'a Entry> + 'a {
    self.entries.iter().filter(move |e| filter.matches(&tag::parse_service_tags(&e.tags())))
  }

  /// The service a template entry's current values build.
  fn build(&self, template: &config::TemplateEntry, log: bool) -> Option<Arc<dyn Notify>> {
    template.build(&self.config.template_values, &config::template::process_env, log)
  }

  /// Every entry `filter` selects, in load order, with each template entry
  /// built from the current values (upstream `find()` with a report). Why
  /// an entry cannot be built is logged.
  pub fn select<'a>(&'a self, filter: &TagFilter) -> Vec<Selected<'a>> {
    self
      .entries
      .iter()
      .filter(|e| filter.matches(&tag::parse_service_tags(&e.tags())))
      .map(|e| match e {
        Entry::Service(svc) => Selected::Service { service: svc.clone(), template: None },
        Entry::Template(t) => match self.build(t, true) {
          Some(service) => Selected::Service { service, template: Some(&t.pending) },
          None => Selected::Unresolved(&t.pending),
        },
      })
      .collect()
  }

  /// The services `filter` selects, in load order, template entries built
  /// from the current values; entries they cannot build are left out.
  fn services_for(&self, filter: &TagFilter, log: bool) -> Vec<Arc<dyn Notify>> {
    self
      .matching(filter)
      .filter_map(|e| match e {
        Entry::Service(svc) => Some(svc.clone()),
        Entry::Template(t) => self.build(t, log),
      })
      .collect()
  }

  /// Turn YAML `template:` sections on (the default) or off. With them off,
  /// `${NAME}` text is kept literally.
  pub fn set_allow_templates(&mut self, allow: bool) {
    self.config.templates_disabled = !allow;
  }

  /// Let configuration from any source include local files. Only for
  /// trusted configuration.
  pub fn set_insecure_includes(&mut self, insecure: bool) {
    self.config.insecure_includes = insecure;
  }

  fn load_options(&self, recursion: u32) -> config::LoadOptions {
    config::LoadOptions {
      recursion,
      insecure_includes: self.config.insecure_includes,
      allow_templates: !self.config.templates_disabled,
      asset: self.asset.clone(),
    }
  }

  /// Keep what a configuration produced; returns the services added,
  /// counting the template entries the current values build.
  fn take_loaded(&mut self, loaded: config::Loaded) -> usize {
    if let Some(asset) = loaded.asset {
      self.asset = asset;
    }
    let mut count = 0;
    for item in loaded.items {
      let entry = match item {
        config::ConfigItem::Service(svc) => Entry::Service(svc.into()),
        config::ConfigItem::Pending(pending) => Entry::Template(Arc::new(config::TemplateEntry::new(*pending))),
      };
      if match &entry {
        Entry::Service(_) => true,
        Entry::Template(t) => self.build(t, false).is_some(),
      } {
        count += 1;
      }
      self.entries.push(entry);
    }
    count
  }

  /// Set tag filter groups.
  /// Each inner Vec is an AND group (all tags must match); outer Vec is OR'd.
  /// An empty list selects every service.
  pub fn set_tags(&mut self, tag_groups: Vec<Vec<String>>) {
    self.tag_filter = if tag_groups.is_empty() { TagFilter::All } else { TagFilter::Groups(tag_groups) };
  }

  /// Parse tag strings from CLI format: each string is one AND group
  /// (tokens separated by commas or spaces) and the strings are OR'd. Tokens
  /// may carry a priority (`2:tag`) and a retry count (`tag:3`). An empty
  /// list selects every service.
  pub fn set_tag_strings(&mut self, tags: &[String]) {
    self.tag_filter = if tags.is_empty() { TagFilter::All } else { TagFilter::from_cli(tags) };
  }

  /// Set which services notifications go to.
  pub fn set_tag_filter(&mut self, filter: TagFilter) {
    self.tag_filter = filter;
  }

  /// The current tag filter.
  pub fn tag_filter(&self) -> &TagFilter {
    &self.tag_filter
  }

  /// Limit every notification call to `seconds` (upstream
  /// `notify(timeout=)`); unfinished services report a timeout. `0` removes
  /// the limit, leaving only the asset's per-service limit. Negative or
  /// non-finite values are rejected.
  pub fn set_timeout(&mut self, seconds: f64) -> Result<(), String> {
    self.timeout = dispatch::validate_timeout(seconds)?;
    Ok(())
  }

  /// The services the current tag filter selects, in load order, with
  /// template entries built from the current values (entries they cannot
  /// build are left out; see [`select`](Self::select)).
  pub fn find(&self) -> impl Iterator<Item = Arc<dyn Notify>> {
    self.services_for(&self.tag_filter, false).into_iter()
  }

  /// Every loaded service, in load order, with template entries built from
  /// the current values (entries they cannot build are left out).
  pub fn services(&self) -> impl Iterator<Item = Arc<dyn Notify>> {
    self.services_for(&TagFilter::All, false).into_iter()
  }

  /// The number of loaded entries: services, and configuration entries
  /// with template variables whether or not their values are known
  /// (upstream `len()`).
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Whether nothing is loaded.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Remove every loaded service and configuration entry.
  pub fn clear(&mut self) {
    self.entries.clear();
  }

  /// Move the entries `other` loaded into this instance, and take its
  /// asset (which a configuration may have set). For loading configuration
  /// without holding this instance: start `other` from a clone of this
  /// asset, load into it, then absorb it.
  pub fn absorb(&mut self, other: Apprise) {
    self.entries.extend(other.entries);
    self.asset = other.asset;
  }

  /// Get details of all loaded services.
  pub fn details(&self) -> Vec<ServiceDetails> {
    self.services().map(|s| s.details()).collect()
  }

  /// Get details of all supported services (static, doesn't require instances).
  pub fn all_service_details() -> Vec<ServiceDetails> {
    registry::all_service_details()
  }

  /// Every supported service, the asset and the version, as upstream's
  /// `Apprise.details()` reports them.
  pub fn details_json(&self) -> serde_json::Value {
    serde_json::json!({
      "version": env!("CARGO_PKG_VERSION"),
      "schemas": Self::all_service_details().iter().map(ServiceDetails::to_json).collect::<Vec<_>>(),
      "asset": self.asset.details(),
    })
  }

  /// [`details_json`](Self::details_json) as compact JSON (upstream
  /// `Apprise.json()`).
  pub fn json(&self) -> String {
    self.details_json().to_string()
  }

  /// Get the current asset.
  pub fn asset(&self) -> &AppriseAsset {
    &self.asset
  }

  /// Send a notification to the services the tag filter selects.
  ///
  /// Services are notified concurrently (unless the asset disables async
  /// mode), each with its own retries (`?retry=`, `?wait=`) and time limit
  /// (the asset's `service_timeout`, and the call limit from
  /// [`set_timeout`](Self::set_timeout)). Without an explicit priority in
  /// the filter, services are tried by tag priority and lower-priority
  /// groups only run if a more urgent one failed. Plugins see the asset of
  /// this instance in `ctx.asset`.
  ///
  /// `ctx.body_format` says what the title and body are written in. Left
  /// at its default (`Text`), the asset's `body_format` applies when it
  /// sets one, so a call can give Markdown or HTML over an asset default
  /// but not plain text.
  pub async fn notify(&self, ctx: &NotifyContext) -> SendResult {
    self.dispatch(ctx, &self.dispatch_options(), &self.tag_filter).await
  }

  /// [`notify_with_attachments`](Self::notify_with_attachments) for the
  /// services `filter` selects instead of the instance's tag filter
  /// (upstream `notify(tag=...)`). Pass no `sources` to send without
  /// attachments.
  pub async fn notify_tagged(&self, ctx: &NotifyContext, filter: &TagFilter, sources: &[String]) -> SendResult {
    self.notify_attached(ctx, sources, filter).await
  }

  /// [`notify`](Self::notify) with attachments loaded from `sources` (paths
  /// or `file://`/`http(s)://` URLs) added to `ctx.attachments`.
  ///
  /// As upstream: a source that can never load (unknown schema, invalid
  /// `?mime=`, or not allowed from this instance's location) fails the whole
  /// call; one that cannot be read now (missing file, HTTP error, too large)
  /// fails only the services that take attachments, and the others send the
  /// message without it.
  pub async fn notify_with_attachments(&self, ctx: &NotifyContext, sources: &[String]) -> SendResult {
    self.notify_attached(ctx, sources, &self.tag_filter).await
  }

  async fn notify_attached(&self, ctx: &NotifyContext, sources: &[String], filter: &TagFilter) -> SendResult {
    let mut opts = self.dispatch_options();
    let mut ctx = ctx.clone();
    if !self.entries.is_empty() {
      let mut parsed = Vec::with_capacity(sources.len());
      for source in sources {
        match attachment::parse_attachment_from(source, self.location) {
          Ok(p) => parsed.push(p),
          Err(_) => {
            tracing::warn!("Could not prepare the notification: One or more attachments could not be added.");
            return SendResult::new(ResultStatus::Failure, Vec::new(), std::time::Duration::ZERO);
          }
        }
      }
      for source in parsed {
        tracing::debug!("Loading attachment: {}", source.loggable());
        match source.load().await {
          Ok(att) => ctx.attachments.push(att),
          Err(_) => opts.attachment_errors.push(source.loggable().to_string()),
        }
      }
    }
    self.dispatch(&ctx, &opts, filter).await
  }

  /// Where this instance runs, which limits where attachments may come
  /// from (a hosted instance only accepts web content).
  pub fn set_location(&mut self, location: attachment::ContentLocation) {
    self.location = location;
  }

  /// [`notify`](Self::notify), but one service at a time.
  pub async fn notify_sequential(&self, ctx: &NotifyContext) -> SendResult {
    let mut opts = self.dispatch_options();
    opts.sequential = true;
    self.dispatch(ctx, &opts, &self.tag_filter).await
  }

  fn dispatch_options(&self) -> dispatch::DispatchOptions {
    let mut opts = dispatch::DispatchOptions::from_asset(&self.asset);
    opts.call_timeout = self.timeout;
    opts.stores = self.stores.clone();
    opts
  }

  /// Write the services' pending persistent storage to disk. Stores also
  /// flush when the instance is dropped; call this before exiting the
  /// process without unwinding.
  pub fn flush_stores(&self) {
    self.stores.flush();
  }

  /// The persistent storage namespace (URL ID) of a loaded service, or
  /// `None` when it has no URL.
  pub fn url_id<'a>(&self, svc: impl std::ops::Deref<Target = dyn Notify + 'a>) -> Option<String> {
    notify::options::url_id(&*svc, &self.asset.storage_salt, self.asset.storage_idlen)
  }

  async fn dispatch(&self, ctx: &NotifyContext, opts: &dispatch::DispatchOptions, filter: &TagFilter) -> SendResult {
    let mut ctx = ctx.clone();
    ctx.asset = self.asset.clone();
    ctx.interpret_escapes |= self.asset.interpret_escapes;
    // A call that leaves the input format at its default takes the asset's
    // (upstream `notify(body_format=None)`).
    if ctx.body_format == NotifyFormat::Text
      && let Some(format) = self.asset.body_format.as_deref().and_then(|f| f.trim().parse::<NotifyFormat>().ok())
    {
      ctx.body_format = format;
    }
    // Names reach MIME and multipart headers; attachments built by a
    // library caller have not been through the loaders' cleanup.
    for attachment in &mut ctx.attachments {
      attachment.name = attachment::safe_name(&attachment.name);
    }
    let mut services = Vec::new();
    let mut pending = Vec::new();
    for selected in self.select(filter) {
      match selected {
        Selected::Service { service, .. } => services.push(service),
        Selected::Unresolved(entry) => pending.push(entry),
      }
    }
    let result = dispatch::notify(&services, filter, &ctx, opts).await;
    Self::with_unresolved(result, &pending)
  }

  /// Add a failed result for every selected entry the template values
  /// could not build: a run with one is partial at best, as upstream.
  fn with_unresolved(result: SendResult, pending: &[&config::PendingService]) -> SendResult {
    if pending.is_empty() {
      return result;
    }
    let SendResult { mut results, elapsed, .. } = result;
    for entry in pending {
      let optional = notify::ServiceOptions::from_url(&entry.parsed).ok().and_then(|o| o.optional).unwrap_or(false);
      let mut tags: Vec<String> = tag::parse_service_tags(&entry.tags()).iter().map(|t| t.name().to_string()).collect();
      tags.sort();
      results.push(result::ServiceResult::new(
        registry::service_name(entry.schema()),
        utils::cwe312::privacy_url(&entry.display_url()),
        tags,
        optional,
        1,
        vec![result::NotifyAttempt { status: ResultStatus::Failure, elapsed: std::time::Duration::ZERO }],
        std::time::Duration::ZERO,
      ));
    }
    let status = result::aggregate_status(false, &results);
    SendResult::new(status, results, elapsed)
  }

  /// Load an attachment from a file path or URL.
  pub async fn load_attachment(source: &str) -> Result<Attachment, AttachError> {
    attachment::load_attachment(source).await
  }
}

/// Default configuration files, in the order upstream's CLI checks them.
pub fn default_config_paths() -> Vec<std::path::PathBuf> {
  use std::path::PathBuf;
  if cfg!(windows) {
    let mut out = Vec::new();
    for var in ["APPDATA", "LOCALAPPDATA", "ALLUSERSPROFILE", "PROGRAMFILES", "COMMONPROGRAMFILES"] {
      if let Ok(base) = std::env::var(var) {
        for name in ["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
          out.push(PathBuf::from(&base).join("Apprise").join(name));
        }
      }
    }
    return out;
  }

  let home = dirs::home_dir().unwrap_or_default();
  let mut out = Vec::new();
  for dir in [".apprise", ".config/apprise"] {
    for ext in ["", ".conf", ".yml", ".yaml"] {
      out.push(home.join(format!("{}{}", dir, ext)));
    }
  }
  for dir in [".apprise", ".config/apprise"] {
    for name in ["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
      out.push(home.join(dir).join(name));
    }
  }
  for path in [
    "/etc/apprise",
    "/etc/apprise.yml",
    "/etc/apprise.yaml",
    "/etc/apprise/apprise",
    "/etc/apprise/apprise.conf",
    "/etc/apprise/apprise.yml",
    "/etc/apprise/apprise.yaml",
  ] {
    out.push(PathBuf::from(path));
  }
  out
}

/// Default persistent storage path, as upstream: `~/.local/share/apprise/cache`
/// (`%APPDATA%/Apprise/cache` on Windows). `APPRISE_STORAGE_PATH` is not
/// consulted here.
pub fn default_storage_path() -> String {
  if cfg!(windows) {
    let appdata = std::env::var("APPDATA").ok().filter(|v| !v.is_empty());
    return appdata.map(|a| format!("{a}/Apprise/cache")).unwrap_or_else(|| "%APPDATA%/Apprise/cache".to_string());
  }
  dirs::home_dir()
    .map(|h| h.join(".local").join("share").join("apprise").join("cache").to_string_lossy().into_owned())
    .unwrap_or_else(|| "~/.local/share/apprise/cache".to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_details_json() {
    let a = Apprise::new();
    let d: serde_json::Value = serde_json::from_str(&a.json()).unwrap();
    assert_eq!(d["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(d["asset"]["app_id"], "Apprise");
    assert_eq!(d["asset"]["default_extension"], ".png");
    assert!(d["schemas"].as_array().unwrap().iter().any(|s| s["service_name"] == "Slack"));
  }

  #[tokio::test]
  async fn test_with_asset_installs_the_crypto_provider() {
    let a = Apprise::with_asset(AppriseAsset::default());
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    drop(a);
  }

  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_clear_removes_entries_waiting_for_template_values() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/notify")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut a = Apprise::new();
    a.add_config_text("template:\n  host:\nurls:\n  - json://${HOST}/notify\n", None, 1).await.unwrap();
    assert_eq!(a.pending_templates().len(), 1);

    a.clear();
    assert!(a.pending_templates().is_empty());
    assert!(a.add(&format!("json://{}/notify", server.address())));
    let result = a.notify(&NotifyContext { body: "x".into(), ..Default::default() }).await;
    assert_eq!((result.status, result.total, result.succeeded), (ResultStatus::Success, 1, 1));

    // A value supplied later brings nothing back
    a.set_template_values([("host".to_string(), server.address().to_string())].into()).unwrap();
    assert_eq!(a.len(), 1);
  }

  #[cfg(feature = "yaml")]
  fn hosts(a: &Apprise) -> Vec<String> {
    a.services().map(|s| s.url_base().and_then(|u| u.host.clone()).unwrap_or_default()).collect()
  }

  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_template_entries_follow_the_current_values() {
    let mut a = Apprise::new();
    a.add_config_text("version: 1\ntemplate:\n  host: old.example\nurls:\n - json://${HOST}\n", None, 0).await.unwrap();
    assert_eq!(hosts(&a), vec!["old.example"]);
    // A value given later replaces the default, and taking it away brings
    // the default back
    assert_eq!(a.set_template_values([("host".to_string(), "new.example".to_string())].into()).unwrap(), 1);
    assert_eq!(hosts(&a), vec!["new.example"]);
    assert_eq!(a.set_template_values([("HOST".to_string(), "other.example".to_string())].into()).unwrap(), 1);
    assert_eq!(hosts(&a), vec!["other.example"]);
    a.set_template_values(Default::default()).unwrap();
    assert_eq!(hosts(&a), vec!["old.example"]);
    // The same values hand back the same service, so what it remembers
    // between notifications is kept
    assert!(Arc::ptr_eq(&a.services().next().unwrap(), &a.services().next().unwrap()));
    assert_eq!(a.len(), 1);

    // A required value read from the environment is read again each time
    let mut a = Apprise::new();
    a.add_config_text("version: 1\ntemplate:\n - core1_env_host\nurls:\n - json://${CORE1_ENV_HOST}\n", None, 0).await.unwrap();
    assert_eq!(a.len(), 1);
    assert!(hosts(&a).is_empty());
    assert_eq!(a.pending_templates().len(), 1);
    // SAFETY: no other test reads or writes this variable.
    unsafe { std::env::set_var("APPRISE_TEMPLATE_CORE1_ENV_HOST", "env-one.example") };
    assert_eq!(hosts(&a), vec!["env-one.example"]);
    assert!(a.pending_templates().is_empty());
    unsafe { std::env::set_var("APPRISE_TEMPLATE_CORE1_ENV_HOST", "env-two.example") };
    assert_eq!(hosts(&a), vec!["env-two.example"]);
    unsafe { std::env::remove_var("APPRISE_TEMPLATE_CORE1_ENV_HOST") };
    assert!(hosts(&a).is_empty());
  }

  #[cfg(feature = "yaml")]
  #[tokio::test]
  async fn test_select_lists_every_entry_in_load_order() {
    let mut a = Apprise::new();
    let yaml = "template:\n  - token\nurls:\n  - json://${TOKEN}@first.example\n  - json://second.example\n  - xml://third.example/${TOKEN}\n";
    a.add_config_text(yaml, None, 0).await.unwrap();
    let shown = |a: &Apprise| -> Vec<String> {
      a.select(&TagFilter::All)
        .into_iter()
        .map(|s| match s {
          Selected::Service { service, template } => {
            format!("{}{}", service.url_base().unwrap().host.clone().unwrap(), if template.is_some() { " (template)" } else { "" })
          }
          Selected::Unresolved(entry) => format!("unresolved {}", entry.schema()),
        })
        .collect()
    };
    assert_eq!(shown(&a), vec!["unresolved json", "second.example", "unresolved xml"]);
    a.set_template_values([("token".to_string(), "abc".to_string())].into()).unwrap();
    assert_eq!(shown(&a), vec!["first.example (template)", "second.example", "third.example (template)"]);
  }

  /// The `message` of every JSON notification `server` received, in order.
  async fn messages(server: &wiremock::MockServer) -> Vec<String> {
    let posts = server.received_requests().await.unwrap().into_iter().filter(|r| r.method == wiremock::http::Method::POST && r.url.path() == "/notify");
    posts.map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["message"].as_str().unwrap_or_default().to_string()).collect()
  }

  #[tokio::test]
  async fn test_asset_body_format_is_the_default_input_format() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/notify")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let asset = AppriseAsset { body_format: Some("markdown".into()), ..Default::default() };
    let mut a = Apprise::with_asset(asset);
    assert!(a.add(&format!("json://{}/notify?format=html", server.address())));

    // No format given: the body is the asset's Markdown
    assert!(a.notify(&NotifyContext { body: "**Hello**".into(), ..Default::default() }).await.is_success());
    // A format given for the call wins over the asset
    let ctx = NotifyContext { body: "**Hello**".into(), body_format: NotifyFormat::Html, ..Default::default() };
    assert!(a.notify(&ctx).await.is_success());
    assert_eq!(messages(&server).await, vec!["<p><strong>Hello</strong></p>", "**Hello**"]);
  }
}
