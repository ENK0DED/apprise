mod args;
mod help;
mod logging;
mod url_templates;

use std::path::Path;
use std::sync::Arc;

use apprise_core::storage::{PersistentStore, StoreState};
use apprise_core::utils::cwe312::{cwe312_loggable, privacy_url};
use apprise_core::utils::disk::path_decode;
use apprise_core::utils::parse::parse_urls;
use apprise_core::{Apprise, AppriseAsset, Notify, NotifyContext, NotifyFormat, NotifyType, ResultStatus, Selected, StorageMode, TagFilter};

/// Recursion depth for configuration includes.
const DEFAULT_RECURSION_DEPTH: i64 = 1;

/// Storage actions; a prefix selects the first that matches. Upstream's
/// help and man page say `clean` while its code only knows `clear`, so
/// both are accepted.
const STORAGE_ACTIONS: &[&str] = &["list", "prune", "clear", "clean"];

#[tokio::main]
async fn main() {
  let argv: Vec<String> = std::env::args().skip(1).collect();
  std::process::exit(run(&argv).await);
}

/// The value of an integer environment variable, its default when unset or
/// empty, and a note in `invalid` when it is not a number.
fn env_int(name: &str, default: i64, invalid: &mut Vec<(String, String)>) -> i64 {
  let value = std::env::var(name).unwrap_or_default();
  let value = value.trim();
  if value.is_empty() {
    return default;
  }
  match args::parse_int(value) {
    Some(n) => n,
    None => {
      invalid.push((name.to_string(), value.to_string()));
      default
    }
  }
}

fn usage_error(message: &str) -> i32 {
  println!("{message}");
  println!("Try 'apprise --help' for more information.");
  2
}

/// Add every URL in `raw`, logging the ones that cannot be loaded.
fn add_urls(apprise: &mut Apprise, raw: &str) {
  for url in parse_urls(raw) {
    if !apprise.add(&url) {
      if url.contains("://") {
        tracing::error!("Unparseable URL {}", cwe312_loggable(&url, true));
      } else {
        tracing::error!("Unsupported URL: {}", url);
      }
    }
  }
}

/// Upstream `bytes_to_str()`: `0.00B`, `1.50KB`, ...
fn bytes_to_str(value: u64) -> String {
  let mut value = value as f64;
  let mut unit = "B";
  for next in ["KB", "MB", "GB", "TB"] {
    if value < 1024.0 {
      break;
    }
    value /= 1024.0;
    unit = next;
  }
  format!("{value:.2}{unit}")
}

/// The terminal width upstream assumes (`COLUMNS`, else 80).
fn columns() -> usize {
  std::env::var("COLUMNS").ok().and_then(|c| c.trim().parse::<usize>().ok()).filter(|c| *c > 0).unwrap_or(80)
}

fn fit(text: &str, width: usize, keep: usize) -> String {
  if text.chars().count() <= width { text.to_string() } else { format!("{}...", text.chars().take(keep).collect::<String>()) }
}

/// How a service is shown: its URL with secrets masked. Tags are listed
/// on their own line, so a `?tag=` the configuration loader added is left
/// out.
fn shown_url(svc: &dyn Notify) -> String {
  let Some(url) = svc.source_url() else { return format!("{}://", svc.schemas().first().copied().unwrap_or("unknown")) };
  let url = match url.split_once('?') {
    Some((base, query)) => {
      let kept: Vec<&str> =
        query.split('&').filter(|p| !p.is_empty() && !matches!(p.split('=').next().unwrap_or("").to_ascii_lowercase().as_str(), "tag" | "tags")).collect();
      if kept.is_empty() { base.to_string() } else { format!("{base}?{}", kept.join("&")) }
    }
    None => url.to_string(),
  };
  privacy_url(&url)
}

fn sorted_tags(svc: &dyn Notify) -> Vec<String> {
  let mut tags: Vec<String> = apprise_core::tag::parse_service_tags(&svc.tags()).iter().map(|t| t.name().to_string()).collect();
  tags.sort();
  tags
}

fn print_details() {
  for line in details_lines() {
    println!("{line}");
  }
}

/// The `--details` listing, as upstream prints it: each service by name,
/// its schemas when it has more than one, and the URL forms it accepts
/// (with `{schema}` filled in when there is only one).
fn details_lines() -> Vec<String> {
  let mut all = Apprise::all_service_details();
  all.sort_by(|a, b| a.service_name.cmp(b.service_name));
  let mut lines = Vec::new();
  for d in all {
    let protocols = &d.protocols;
    if protocols.len() == 1 {
      lines.push(format!("+ {:<30} ", d.service_name));
    } else {
      lines.push(format!("+ {:<30} | Schema(s): {}", d.service_name, protocols.join(", ")));
    }
    let templates = protocols.iter().map(|p| url_templates::url_templates(&p.to_lowercase())).find(|t| !t.is_empty()).unwrap_or_default();
    for template in templates {
      let shown = match protocols.as_slice() {
        [only] => with_schema(template, only),
        _ => template.to_string(),
      };
      lines.push(format!("   - {shown}"));
    }
    lines.push(String::new());
  }
  lines
}

/// Upstream's `re.sub(r"^[^}]+}://", "<schema>://", template)`.
fn with_schema(template: &str, schema: &str) -> String {
  match template.find('}') {
    Some(end) if end > 0 && template[end + 1..].starts_with("://") => format!("{schema}://{}", &template[end + 4..]),
    _ => template.to_string(),
  }
}

fn version_message() -> String {
  format!(
    "Apprise v{}\nCopyright (c) 2026, Chris Caron <lead2gold@gmail.com>\nThis code is licensed under the BSD 2-Clause License.",
    env!("CARGO_PKG_VERSION")
  )
}

/// Load the configuration files named by `APPRISE_CONFIG_PATH` (or the
/// deprecated `APPRISE_CONFIG`), else the default locations.
async fn load_default_configs(apprise: &mut Apprise, depth: u32) {
  let explicit = match std::env::var("APPRISE_CONFIG_PATH") {
    Ok(v) => {
      tracing::debug!("Loading provided APPRISE_CONFIG_PATH environment variable");
      Some(v)
    }
    Err(_) => {
      std::env::var("APPRISE_CONFIG").ok().inspect(|_| tracing::warn!("The APPRISE_CONFIG environment variable has been renamed to APPRISE_CONFIG_PATH"))
    }
  };
  let Some(paths) = explicit else {
    apprise.load_default_configs(depth).await;
    return;
  };
  for path in paths.trim().split(['\r', '\n', ';']).map(str::trim).filter(|p| !p.is_empty()) {
    let decoded = path_decode(path);
    if decoded.is_file()
      && let Err(e) = apprise.add_config(&decoded.to_string_lossy(), depth).await
    {
      tracing::warn!("Could not load configuration {}: {}", cwe312_loggable(path, true), e);
    }
  }
}

/// Validate `--template-var` entries: `NAME=VALUE`, each name once
/// (ignoring case). Only names are ever echoed; values may be secrets.
fn template_vars(entries: &[String]) -> Result<Vec<(String, String)>, i32> {
  let mut seen = std::collections::HashSet::new();
  let mut out = Vec::new();
  for entry in entries {
    let (name, value) = entry.split_once('=').map(|(n, v)| (n.trim(), Some(v))).unwrap_or((entry.trim(), None));
    let valid_name = !name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let Some(value) = value.filter(|_| valid_name) else {
      return Err(usage_error(&format!("The --template-var (-tv) name '{name}' is not in the expected NAME=VALUE format.")));
    };
    if !seen.insert(name.to_lowercase()) {
      return Err(usage_error(&format!("The --template-var (-tv) name '{name}' was provided more than once.")));
    }
    out.push((name.to_string(), value.to_string()));
  }
  Ok(out)
}

async fn run(argv: &[String]) -> i32 {
  let mut invalid_env = Vec::new();
  let default_prune_days = env_int("APPRISE_STORAGE_PRUNE_DAYS", 30, &mut invalid_env);
  let default_uid_length = env_int("APPRISE_STORAGE_UID_LENGTH", 8, &mut invalid_env);
  let env_storage_path = std::env::var("APPRISE_STORAGE_PATH").ok().map(|p| p.trim().to_string()).filter(|p| !p.is_empty());
  let shown_storage_default =
    env_storage_path.clone().unwrap_or_else(|| if cfg!(windows) { "%APPDATA%/Apprise/cache".into() } else { "~/.local/share/apprise/cache".into() });

  let cli = match args::parse(argv) {
    Ok(cli) => cli,
    Err(e) => {
      eprintln!("{}", e.render());
      return 2;
    }
  };
  if cli.flag("help") {
    print!(
      "{}",
      help::render(&help::HelpDefaults { storage_path: shown_storage_default, storage_prune_days: default_prune_days, storage_uid_length: default_uid_length })
    );
    return 0;
  }

  let debug = cli.flag("debug");
  let verbose = if debug { cli.count("verbose").max(3) } else { cli.count("verbose") };
  logging::init(verbose, true);
  tracing::debug!("Apprise: {}", env!("CARGO_PKG_VERSION"));

  if cli.flag("version") {
    println!("{}", version_message());
    return 0;
  }

  let notification_type = cli.str("notification_type").unwrap_or("info").trim().to_lowercase();
  let Some(notify_type) =
    ["info", "success", "warning", "failure"].contains(&notification_type.as_str()).then(|| notification_type.parse::<NotifyType>().ok()).flatten()
  else {
    return usage_error(&format!("The --notification-type (-n) value of {notification_type} is not supported."));
  };
  let input_format = cli.str("input_format").unwrap_or("text").trim().to_lowercase();
  let Some(body_format) = ["text", "html", "markdown"].contains(&input_format.as_str()).then(|| input_format.parse::<NotifyFormat>().ok()).flatten() else {
    return usage_error(&format!("The --input-format (-i) value of {input_format} is not supported."));
  };
  let storage_mode = cli.str("storage_mode").unwrap_or("auto").trim().to_lowercase();
  let Ok(storage_mode) = storage_mode.parse::<StorageMode>() else {
    return usage_error(&format!("The --storage-mode (-SM) value of {storage_mode} is not supported."));
  };
  if !invalid_env.is_empty() {
    for (name, value) in &invalid_env {
      println!("The {name} environment variable value of '{value}' is not a valid number.");
    }
    println!("Try 'apprise --help' for more information.");
    return 2;
  }
  let storage_uid_length = cli.int("storage_uid_length").unwrap_or(default_uid_length);
  if storage_uid_length < 2 {
    return usage_error("The --storage-uid-length (-SUL) value can not be lower than two (2).");
  }
  if !cli.strs("plugin_path").is_empty() || std::env::var_os("APPRISE_PLUGIN_PATH").is_some() {
    tracing::warn!("Custom plugin paths are not supported by this build; ignoring --plugin-path (-P).");
  }

  // An explicit --storage-path wins over APPRISE_STORAGE_PATH.
  let storage_path = cli.str("storage_path").map(str::to_string).or(env_storage_path).unwrap_or_else(apprise_core::default_storage_path);
  let storage_path = path_decode(&storage_path);
  let mut asset = AppriseAsset {
    body_format: Some(input_format.clone()),
    theme: cli.str("theme").unwrap_or("default").to_string(),
    async_mode: !cli.flag("disable_async"),
    storage_path: Some(storage_path.clone()),
    storage_idlen: storage_uid_length as usize,
    storage_mode: storage_mode.clone(),
    ..Default::default()
  };
  if let Some(limit) = cli.float("service_limit") {
    if !limit.is_finite() || limit < 0.0 {
      tracing::error!("AppriseAsset service_timeout must be >= 0 and finite (0 disables the timeout entirely).");
      return 1;
    }
    asset.service_timeout = limit;
  }
  let mut apprise = Apprise::with_asset(asset);
  if let Err(e) = apprise.set_timeout(cli.float("limit").unwrap_or(0.0)) {
    tracing::error!("{}", e);
    return 1;
  }
  let depth = cli.int("recursion_depth").unwrap_or(DEFAULT_RECURSION_DEPTH).max(0) as u32;
  let dry_run = cli.flag("dry_run");
  let mut tags = cli.strs("tag");
  let config = cli.strs("config");

  // `apprise storage ...` (any prefix of "storage").
  let positional = cli.args.clone();
  let storage_action = positional.first().is_some_and(|first| "storage".starts_with(first.as_str()));
  let mut urls = positional.clone();
  let mut url_filters = false;
  if storage_action {
    let mut rest: Vec<String> = positional[1..].to_vec();
    if rest.first().is_some_and(|a| STORAGE_ACTIONS.iter().any(|m| m.starts_with(a.as_str()))) {
      rest.remove(0);
    }
    let filters: Vec<String> = rest.into_iter().filter(|f| apprise_core::attachment::schema_of(f).is_some()).collect();
    url_filters = !filters.is_empty();
    urls = filters;
  }

  if cli.flag("details") {
    print_details();
    return 0;
  }

  // --template-var values fill ${NAME} entries in the configuration.
  let template_values = match template_vars(&cli.strs("template_var")) {
    Ok(values) => values,
    Err(code) => return code,
  };
  if !template_values.is_empty()
    && let Err(e) = apprise.set_template_values(template_values.into_iter().collect())
  {
    tracing::error!("{}", e);
    return 2;
  }

  // Sources, in upstream's order: URLs, --config, APPRISE_URLS, defaults.
  let mut url_mode = false;
  if !urls.is_empty() && (!storage_action || url_filters) {
    url_mode = true;
    for url in &urls {
      add_urls(&mut apprise, url);
    }
    if !config.is_empty() {
      tracing::warn!("You defined both URLs and a --config (-c) entry; Only the URLs will be referenced.");
    }
  } else if !config.is_empty() {
    for source in &config {
      if let Err(e) = apprise.add_config(source, depth).await {
        tracing::warn!("Could not load configuration {}: {}", cwe312_loggable(source, true), e);
      }
    }
  } else if let Some(env_urls) = std::env::var("APPRISE_URLS").ok().filter(|v| !v.trim().is_empty()) {
    tracing::debug!("Loading provided APPRISE_URLS environment variable");
    url_mode = true;
    add_urls(&mut apprise, env_urls.trim());
  } else {
    load_default_configs(&mut apprise, depth).await;
  }
  if url_mode && !tags.is_empty() {
    tracing::warn!("--tag (-g) entries are ignored when using specified URLs");
    tags.clear();
  }

  if !dry_run && apprise.is_empty() && !storage_action {
    println!("You must specify at least one service URL or populated configuration file.");
    println!("Try 'apprise --help' for more information.");
    return 1;
  }

  // Each --tag is an AND group; the groups are OR'd. Without one, only
  // untagged services are notified (URLs given directly carry no tags).
  let filter = if !tags.is_empty() {
    TagFilter::from_cli(&tags)
  } else if url_mode {
    TagFilter::All
  } else {
    TagFilter::Untagged
  };
  apprise.set_tag_filter(filter);

  if storage_action {
    return storage(&cli, &apprise, &positional, &storage_path, !tags.is_empty(), url_filters, default_prune_days, dry_run);
  }

  let status = if dry_run {
    dry_run_listing(&apprise)
  } else {
    let body = match cli.str("body") {
      Some(b) => b.to_string(),
      None => {
        tracing::trace!("No --body (-b) specified; reading from stdin");
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
        buf
      }
    };
    let ctx = NotifyContext {
      body,
      title: cli.str("title").unwrap_or_default().to_string(),
      notify_type,
      body_format,
      interpret_escapes: cli.flag("interpret_escapes"),
      interpret_emojis: cli.flag("interpret_emojis"),
      ..Default::default()
    };
    let result = apprise.notify_with_attachments(&ctx, &cli.strs("attach")).await;
    tracing::debug!(
      "Finished in {:.2}s. {} service(s) tried ({}): {} sent / {} failed / {} timed out.",
      result.elapsed.as_secs_f64(),
      result.total,
      result.status,
      result.succeeded,
      result.failed,
      result.timed_out()
    );
    result.status
  };
  apprise.flush_stores();
  status.exit_code()
}

/// `--dry-run`: list what would be notified, with its storage UID and tags.
fn dry_run_listing(apprise: &Apprise) -> ResultStatus {
  let (lines, status) = dry_run_lines(apprise, columns());
  for line in lines {
    println!("{line}");
  }
  status
}

/// The `--dry-run` listing: every entry the tag filter selects, in
/// configuration order, as upstream lists them. Entries the template
/// values cannot build are listed where they are and count as failures,
/// as a real run would count them; entries with template variables also
/// show which ones they use.
fn dry_run_lines(apprise: &Apprise, columns: usize) -> (Vec<String>, ResultStatus) {
  let mut lines = Vec::new();
  let mut listed = 0usize;
  let mut unresolved = 0usize;
  for selected in apprise.select(apprise.tag_filter()) {
    listed += 1;
    let (url, uid, tags, template) = match &selected {
      Selected::Service { service, template } => {
        (shown_url(&**service), apprise.url_id(&**service).unwrap_or_else(|| "- n/a -".to_string()), sorted_tags(&**service), *template)
      }
      Selected::Unresolved(entry) => {
        unresolved += 1;
        let mut tags: Vec<String> = apprise_core::tag::parse_service_tags(&entry.tags()).iter().map(|t| t.name().to_string()).collect();
        tags.sort();
        (privacy_url(&entry.display_url()), "- template; unresolved, would not send -".to_string(), tags, Some(*entry))
      }
    };
    lines.push(format!("{:>4}. {}", listed, fit(&url, columns.saturating_sub(8), columns.saturating_sub(9))));
    lines.push(format!("{:>10}: {}", "uid", uid));
    if !tags.is_empty() {
      lines.push(format!("{:>10}: {}", "tags", tags.join(", ")));
    }
    if let Some(entry) = template {
      let required = entry.required();
      let names: Vec<String> = entry.names.iter().map(|n| format!("{} ({})", n, if required.contains(n) { "required" } else { "has default" })).collect();
      lines.push(format!("{:>10}: {}", "template", names.join(", ")));
    }
  }
  if unresolved > 0 {
    lines.push(String::new());
    lines.push(format!("{} of {} matched entries could not be resolved and would not be sent. Check the log for the reason.", unresolved, listed));
  }
  let status = match (listed, unresolved) {
    (0, _) => ResultStatus::NoMatch,
    (_, 0) => ResultStatus::Success,
    (l, u) if l == u => ResultStatus::Failure,
    _ => ResultStatus::Partial,
  };
  (lines, status)
}

/// `apprise storage [list|prune|clear] [UID-prefix|URL]...`.
#[allow(clippy::too_many_arguments)]
fn storage(
  cli: &args::Parsed,
  apprise: &Apprise,
  positional: &[String],
  path: &Path,
  tagged: bool,
  url_filters: bool,
  default_prune_days: i64,
  dry_run: bool,
) -> i32 {
  let mut prune_days = cli.int("storage_prune_days").unwrap_or(default_prune_days);
  if prune_days < 0 {
    return usage_error("The --storage-prune-days (-SPD) value can not be lower than zero (0).");
  }
  let mut rest: Vec<String> = positional[1..].to_vec();
  let mut action = "list";
  if let Some(first) = rest.first()
    && let Some(found) = STORAGE_ACTIONS.iter().find(|a| a.starts_with(first.as_str()))
  {
    action = if *found == "clean" { "clear" } else { found };
    rest.remove(0);
  }
  let uid_filters: Vec<String> = rest.iter().filter(|f| apprise_core::attachment::schema_of(f).is_none()).map(|f| f.to_lowercase()).collect();

  // The loaded services' namespaces, in load order.
  let services: Vec<Arc<dyn Notify>> = if tagged { apprise.find().collect() } else { apprise.services().collect() };
  let mut uids: Vec<(String, Vec<&dyn Notify>, StoreState, u64)> = Vec::new();
  for svc in services.iter().map(|s| s.as_ref()) {
    let Some(id) = apprise.url_id(svc) else { continue };
    if !uid_filters.is_empty() && !uid_filters.iter().any(|u| id.starts_with(u.as_str())) {
      tracing::trace!("Storage: plugin uid '{}' excluded by filter {:?}", id, uid_filters);
      continue;
    }
    match uids.iter_mut().find(|(u, ..)| *u == id) {
      Some(entry) => entry.1.push(svc),
      None => uids.push((id, vec![svc], StoreState::Unused, 0)),
    }
  }
  let scoped =
    |uids: &[(String, Vec<&dyn Notify>, StoreState, u64)]| uids.iter().map(|(u, ..)| u.clone()).chain(uid_filters.iter().cloned()).collect::<Vec<String>>();

  if action == "list" {
    let detected = if url_filters {
      let ns = scoped(&uids);
      if ns.is_empty() { Vec::new() } else { PersistentStore::disk_scan(path, &ns, true) }
    } else {
      PersistentStore::disk_scan(path, &uid_filters, true)
    };
    for id in detected {
      let size = apprise_core::utils::disk::dir_size(&path.join(&id), 3).unwrap_or(0);
      match uids.iter_mut().find(|(u, ..)| *u == id) {
        Some(entry) => {
          entry.2 = StoreState::Active;
          entry.3 = size;
        }
        None if !tagged => uids.push((id, Vec::new(), StoreState::Stale, size)),
        None => {}
      }
    }
    let columns = columns();
    for (idx, (uid, plugins, state, size)) in uids.iter().enumerate() {
      if idx > 0 {
        println!();
      }
      println!("{:>4}. {:<52} {:<8} {}", idx + 1, uid, bytes_to_str(*size), state.as_str());
      for svc in plugins {
        println!("{:>7} {}", "-", fit(&shown_url(*svc), columns.saturating_sub(8), columns.saturating_sub(11)));
        let tags = sorted_tags(*svc);
        if !tags.is_empty() {
          println!("{:>10}: {}", "tags", tags.join(", "));
        }
      }
    }
    return 0;
  }

  if action == "clear" {
    prune_days = 0;
  }
  let namespaces: Option<Vec<String>> = if url_filters || tagged {
    let ns = scoped(&uids);
    if ns.is_empty() {
      return 0;
    }
    Some(ns)
  } else {
    (!uid_filters.is_empty()).then(|| uid_filters.clone())
  };
  // Upstream crashes on a day count this large; prune nothing instead of
  // wrapping around to a recent cutoff.
  let Some(expires) = (prune_days as u64).checked_mul(86_400) else {
    return usage_error("The --storage-prune-days (-SPD) value is too large.");
  };
  PersistentStore::disk_prune(path, namespaces.as_deref(), Some(expires), !dry_run);
  0
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_split_urls_like_upstream() {
    assert_eq!(parse_urls("json://a?to=x,y, json://b json://c;xml://d"), vec!["json://a?to=x,y", "json://b", "json://c", "xml://d"]);
    assert_eq!(parse_urls("foo"), vec!["foo"]);
    assert_eq!(parse_urls("-x"), vec!["-x"]);
    assert_eq!(parse_urls("json://a/b c"), vec!["json://a/b c"]);
  }

  #[test]
  fn test_bytes_to_str_like_upstream() {
    let got: Vec<String> = [0, 1, 1023, 1024, 1536, 1_048_576, 5 * 1024u64.pow(3), 3 * 1024u64.pow(4)].iter().map(|v| bytes_to_str(*v)).collect();
    assert_eq!(got, vec!["0.00B", "1.00B", "1023.00B", "1.00KB", "1.50KB", "1.00MB", "5.00GB", "3.00TB"]);
  }

  #[test]
  fn test_template_vars_are_validated() {
    assert_eq!(template_vars(&["a=1".into(), "B=x=y".into()]).unwrap(), vec![("a".into(), "1".into()), ("B".into(), "x=y".into())]);
    assert_eq!(template_vars(&["bad".into()]), Err(2));
    assert_eq!(template_vars(&["a-b=1".into()]), Err(2));
    assert_eq!(template_vars(&["a=1".into(), "A=2".into()]), Err(2));
  }

  #[tokio::test]
  async fn test_dry_run_lists_entries_in_configuration_order() {
    let yaml = "template:\n  - token\nurls:\n  - json://${TOKEN}@first.example\n  - json://second.example\n  - xml://third.example/${TOKEN}\n";
    let mut apprise = Apprise::new();
    if apprise.add_config_text(yaml, Some(apprise_core::config::ConfigFormat::Yaml), 0).await.is_err() {
      return; // a build without YAML support
    }
    let urls = |lines: &[String]| -> Vec<String> {
      lines.iter().filter_map(|l| l.trim_start().split_once(". ").filter(|(n, _)| n.parse::<usize>().is_ok())).map(|(_, url)| url.to_string()).collect()
    };
    let (lines, status) = dry_run_lines(&apprise, 200);
    assert_eq!(urls(&lines), vec!["json://$...}@first.example", "json://second.example", "xml://third.example/$...}"]);
    assert_eq!(lines[1], "       uid: - template; unresolved, would not send -");
    assert_eq!(lines[2], "  template: token (required)");
    assert!(!lines[4].contains("template"), "{lines:?}");
    assert_eq!(lines.last().unwrap(), "2 of 3 matched entries could not be resolved and would not be sent. Check the log for the reason.");
    assert_eq!(status, ResultStatus::Partial);

    // Once built, a template entry still says which variables it uses
    apprise.set_template_values([("token".to_string(), "abc".to_string())].into()).unwrap();
    let (lines, status) = dry_run_lines(&apprise, 200);
    assert_eq!(urls(&lines), vec!["json://a...c@first.example", "json://second.example", "xml://third.example/a...c"]);
    assert_eq!(lines.iter().filter(|l| *l == "  template: token (required)").count(), 2);
    assert!(!lines[1].contains("unresolved"));
    assert_eq!(status, ResultStatus::Success);
  }

  #[test]
  fn test_details_show_url_templates_like_upstream() {
    let lines = details_lines();
    let block = |name: &str| -> Vec<String> {
      let start = lines.iter().position(|l| l.starts_with(&format!("+ {name} "))).unwrap_or_else(|| panic!("{name} is not listed"));
      lines[start..].iter().take_while(|l| !l.is_empty()).cloned().collect()
    };
    // Several schemas: listed, and templates keep {schema}
    assert_eq!(
      block("46elks"),
      vec![
        "+ 46elks                         | Schema(s): 46elks, elks",
        "   - {schema}://{user}:{password}@{from_phone}",
        "   - {schema}://{user}:{password}@{from_phone}/{targets}",
      ]
    );
    // One schema: no schema list, and {schema} is filled in
    assert_eq!(block("Africas Talking"), vec!["+ Africas Talking                ", "   - atalk://{appuser}@{apikey}/{targets}"]);
    // A template that names its schema is kept as written
    assert_eq!(
      block("AWS Simple Email Service (SES)"),
      vec![
        "+ AWS Simple Email Service (SES) ",
        "   - ses://{from_email}/{access_key_id}/{secret_access_key}/{region}/{targets}",
        "   - ses://{from_email}/{access_key_id}/{secret_access_key}/{region}",
      ]
    );
    // Every service this build supports has upstream's templates
    for d in Apprise::all_service_details() {
      assert!(d.protocols.iter().any(|p| !url_templates::url_templates(p).is_empty()), "{} has no URL templates", d.service_name);
    }
  }

  #[test]
  fn test_with_schema() {
    assert_eq!(with_schema("{schema}://{host}/{token}", "gotify"), "gotify://{host}/{token}");
    assert_eq!(with_schema("ses://{from_email}/{region}", "ses"), "ses://{from_email}/{region}");
    assert_eq!(with_schema("{host}", "x"), "{host}");
  }

  #[test]
  fn test_fit() {
    assert_eq!(fit("abcdef", 6, 3), "abcdef");
    assert_eq!(fit("abcdefg", 6, 3), "abc...");
  }
}
