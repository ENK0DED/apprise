mod asset;
mod cli;
mod config;
mod error;
mod types;
mod notify;
mod attachment;
mod storage;
mod utils;

use clap::Parser;
use cli::{Cli, Commands};
use notify::{NotifyContext, OverflowMode, registry};
use types::{NotifyFormat, NotifyType};
use utils::{emoji::interpret_emojis, escape::interpret_escapes, format::smart_split, parse::mask_url};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Configure tracing
    let verbose = cli.verbose;
    let level = if cli.debug || verbose >= 3 {
        tracing::Level::DEBUG
    } else if verbose == 2 {
        tracing::Level::INFO
    } else if verbose == 1 {
        tracing::Level::INFO
    } else {
        tracing::Level::WARN
    };
    tracing_subscriber::fmt().with_max_level(level).init();

    // Warn about unsupported --plugin-path
    if !cli.plugin_path.is_empty() {
        eprintln!("Warning: --plugin-path is not supported in the Rust port. External plugins are ignored.");
    }

    // Handle storage subcommand
    if let Some(Commands::Storage(storage_args)) = &cli.command {
        let storage_path = storage_args
            .storage_path
            .clone()
            .or_else(|| cli.storage_path.clone())
            .unwrap_or_else(default_storage_path);
        let prune_days = storage_args.storage_prune_days;
        let uid_length = storage_args.storage_uid_length;
        let storage_mode = cli.storage_mode.parse::<types::StorageMode>().unwrap_or(types::StorageMode::Auto);
        let store = storage::PersistentStore::new(
            std::path::PathBuf::from(&storage_path),
            uid_length as usize,
            prune_days,
            storage_mode,
        );

        match storage_args.action.as_str() {
            "list" => {
                let entries = store.list().await;
                if entries.is_empty() {
                    println!("No entries.");
                } else {
                    for e in entries {
                        println!("{}\t{}\t{}", e.uid, e.url_hash, e.last_used.format("%Y-%m-%d %H:%M:%S"));
                    }
                }
            }
            "prune" => {
                let n = store.prune().await;
                println!("Pruned {} entries.", n);
            }
            "clean" => {
                let n = store.clean().await;
                println!("Cleaned {} entries.", n);
            }
            other => {
                eprintln!("Unknown storage action: {}", other);
                std::process::exit(2);
            }
        }
        return;
    }

    // --details: list all supported services
    if cli.details {
        let all = registry::all_service_details();
        println!("{:<30} {:<50} {}", "Service", "Description", "Protocols");
        println!("{}", "-".repeat(100));
        for d in all {
            println!("{:<30} {:<50} {}", d.service_name, d.description, d.protocols.join(", "));
        }
        return;
    }

    // --schema: emit JSON schema
    if cli.schema {
        let all = registry::all_service_details();
        let json = serde_json::to_string_pretty(&all.iter().map(|d| d.to_json()).collect::<Vec<_>>()).unwrap();
        println!("{}", json);
        return;
    }

    // Collect notification services
    let mut services: Vec<Box<dyn notify::Notify>> = Vec::new();
    let mut config_asset: Option<asset::AppriseAsset> = None;

    // From direct URLs on command line (split on ;,\n\s for APPRISE_URLS compat)
    for raw in &cli.urls {
        for url in raw.split(|c: char| c == ';' || c == ',' || c == '\n' || c == '\r') {
            let url = url.trim();
            if url.is_empty() || !url.contains("://") { continue; }
            if let Some(svc) = registry::from_url(url) {
                services.push(svc);
            } else {
                eprintln!("Warning: could not parse URL: {}", mask_url(url));
            }
        }
    }

    // From config files (split on ; and newlines for APPRISE_CONFIG_PATH compat)
    let recursion_depth = cli.recursion_depth;
    for raw_path in &cli.config {
        for cfg_path in raw_path.split(|c: char| c == ';' || c == '\n' || c == '\r') {
            let cfg_path = cfg_path.trim();
            if cfg_path.is_empty() { continue; }
            match config::load_config(cfg_path, recursion_depth).await {
                Ok((mut svcs, parsed_asset)) => {
                    services.append(&mut svcs);
                    if config_asset.is_none() {
                        config_asset = parsed_asset;
                    }
                }
                Err(e) => eprintln!("Warning: failed to load config {}: {}", cfg_path, e),
            }
        }
    }

    // If no services found yet, try default config file paths (matching Python)
    if services.is_empty() {
        let mut default_paths: Vec<std::path::PathBuf> = Vec::new();
        if let Some(home) = dirs::home_dir() {
            for name in &["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
                default_paths.push(home.join(format!(".{}", name)));
            }
            for name in &["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
                default_paths.push(home.join(".apprise").join(name));
            }
        }
        if let Some(cfg) = dirs::config_dir() {
            for name in &["apprise", "apprise.conf", "apprise.yml", "apprise.yaml"] {
                default_paths.push(cfg.join("apprise").join(name));
            }
        }
        for name in &[
            "/etc/apprise", "/etc/apprise.conf", "/etc/apprise.yml", "/etc/apprise.yaml",
            "/etc/apprise/apprise", "/etc/apprise/apprise.conf", "/etc/apprise/apprise.yml", "/etc/apprise/apprise.yaml",
        ] {
            default_paths.push(std::path::PathBuf::from(name));
        }
        for path in &default_paths {
            if path.exists() {
                let path_str = path.to_string_lossy().to_string();
                tracing::debug!("Loading default config: {}", path_str);
                match config::load_config(&path_str, recursion_depth).await {
                    Ok((mut svcs, parsed_asset)) if !svcs.is_empty() => {
                        services.append(&mut svcs);
                        if config_asset.is_none() {
                            config_asset = parsed_asset;
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    if services.is_empty() {
        eprintln!("No notification services specified.");
        std::process::exit(3);
    }

    // Apply tag filter: each -g arg can contain comma-separated AND tags;
    // multiple -g args are OR'd together.
    let tag_groups: Vec<Vec<String>> = cli.tag.iter()
        .map(|g| g.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect::<Vec<_>>())
        .filter(|g| !g.is_empty())
        .collect();
    if !tag_groups.is_empty() {
        services.retain(|svc| {
            let svc_tags: Vec<String> = svc.tags().iter().map(|t| t.to_lowercase()).collect();
            // Services with the "always" tag are never filtered out
            if svc_tags.iter().any(|t| t == "always") { return true; }
            // A service matches if ANY AND-group is fully satisfied
            tag_groups.iter().any(|and_group| {
                // "all" in a group matches everything
                if and_group.iter().any(|t| t == "all") { return true; }
                and_group.iter().all(|t| svc_tags.contains(t))
            })
        });
        if services.is_empty() {
            eprintln!("No services matched the specified tags.");
            std::process::exit(3);
        }
    }
    let tag_filters: Vec<String> = tag_groups.into_iter().flatten().collect();

    // Dry run: just print what would be notified
    if cli.dry_run {
        println!("Dry run - would notify {} service(s):", services.len());
        for svc in &services {
            println!("  {} ({})", svc.service_name(), svc.schemas().join(", "));
        }
        return;
    }

    // Build message body
    let body = match cli.body {
        Some(b) => b,
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).unwrap_or(0);
            buf
        }
    };

    if body.is_empty() {
        eprintln!("No message body provided.");
        std::process::exit(2);
    }

    let body = body;
    let title = cli.title.unwrap_or_default();

    // Note: emoji/escape interpretation is deferred to per-service send,
    // applied AFTER format conversion (matching Python's order).

    let notify_type = cli.notification_type
        .parse::<NotifyType>()
        .unwrap_or(NotifyType::Info);

    let body_format = cli.input_format
        .parse::<NotifyFormat>()
        .unwrap_or(NotifyFormat::Text);

    // Load attachments
    let mut attachments = Vec::new();
    for attach_path in &cli.attach {
        match attachment::load_attachment(attach_path).await {
            Ok(att) => attachments.push(att),
            Err(e) => eprintln!("Warning: could not load attachment {}: {}", attach_path, e),
        }
    }

    let ctx = NotifyContext {
        body,
        title,
        notify_type,
        body_format,
        attachments,
        interpret_escapes: cli.interpret_escapes,
        interpret_emojis: cli.interpret_emojis,
        tags: tag_filters,
        asset: config_asset.unwrap_or_default(),
    };

    // Send notifications (with per-plugin format conversion)
    let mut all_ok = true;
    if cli.disable_async {
        // Sequential sending with optional throttling
        let mut last_send = std::time::Instant::now();
        for svc in &services {
            // Throttle if plugin specifies a rate limit
            let rate = svc.request_rate_per_sec();
            if rate > 0.0 {
                let min_interval = std::time::Duration::from_secs_f64(1.0 / rate);
                let elapsed = last_send.elapsed();
                if elapsed < min_interval {
                    tokio::time::sleep(min_interval - elapsed).await;
                }
            }
            let name = svc.service_name().to_string();
            let contexts = prepare_contexts(svc.as_ref(), &ctx);
            for svc_ctx in &contexts {
                last_send = std::time::Instant::now();
                match svc.send(svc_ctx).await {
                    Ok(true) => tracing::info!("Sent via {}", name),
                    Ok(false) => {
                        eprintln!("Partial failure sending via {}", name);
                        all_ok = false;
                    }
                    Err(e) => {
                        eprintln!("Error sending via {}: {}", name, e);
                        all_ok = false;
                    }
                }
            }
        }
    } else {
        // Parallel sending
        let mut set = tokio::task::JoinSet::new();
        for svc in services {
            let name = svc.service_name().to_string();
            let contexts = prepare_contexts(svc.as_ref(), &ctx);
            set.spawn(async move {
                let mut ok = true;
                for svc_ctx in &contexts {
                    match svc.send(svc_ctx).await {
                        Ok(true) => {}
                        Ok(false) => ok = false,
                        Err(e) => return (name, Err::<bool, _>(e)),
                    }
                }
                (name, Ok(ok))
            });
        }
        while let Some(result) = set.join_next().await {
            match result {
                Ok((name, Ok(true))) => tracing::info!("Sent via {}", name),
                Ok((name, Ok(false))) => {
                    eprintln!("Partial failure sending via {}", name);
                    all_ok = false;
                }
                Ok((name, Err(e))) => {
                    eprintln!("Error sending via {}: {}", name, e);
                    all_ok = false;
                }
                Err(e) => {
                    eprintln!("Task join error: {}", e);
                    all_ok = false;
                }
            }
        }
    }

    if !all_ok {
        std::process::exit(1);
    }
}

/// Prepare one or more NotifyContext(s) for a service, handling line-count
/// truncation, format conversion, emoji/escape, title truncation, and overflow.
fn prepare_contexts(svc: &dyn notify::Notify, ctx: &NotifyContext) -> Vec<NotifyContext> {
    let mut svc_ctx = ctx.clone();

    // Per-service format conversion
    let target_format = svc.notify_format();
    if target_format != svc_ctx.body_format {
        svc_ctx.body = utils::format::convert_format(&svc_ctx.body, &svc_ctx.body_format, &target_format);
        svc_ctx.body_format = target_format;
    }

    // Escape / emoji interpretation
    if svc_ctx.interpret_escapes {
        svc_ctx.body = interpret_escapes(&svc_ctx.body);
    }
    if svc_ctx.interpret_emojis {
        svc_ctx.body = interpret_emojis(&svc_ctx.body);
    }

    // Line-count truncation (before overflow handling)
    let max_lines = svc.body_max_line_count();
    if max_lines > 0 {
        let lines: Vec<&str> = svc_ctx.body.lines().collect();
        if lines.len() > max_lines {
            svc_ctx.body = lines[..max_lines].join("\n");
        }
    }

    // Title truncation
    let title_max = svc.title_maxlen();
    if title_max > 0 && svc_ctx.title.len() > title_max {
        svc_ctx.title.truncate(title_max);
    } else if title_max == 0 {
        svc_ctx.title.clear();
    }

    // Overflow handling
    let body_max = svc.body_maxlen();
    match svc.overflow_mode() {
        OverflowMode::Upstream => {
            // Send as-is
            vec![svc_ctx]
        }
        OverflowMode::Truncate => {
            if body_max > 0 && svc_ctx.body.len() > body_max {
                svc_ctx.body.truncate(body_max);
            }
            vec![svc_ctx]
        }
        OverflowMode::Split => {
            if body_max == 0 || svc_ctx.body.len() <= body_max {
                return vec![svc_ctx];
            }
            let chunks = smart_split(&svc_ctx.body, body_max);
            let original_title = svc_ctx.title.clone();
            chunks.into_iter().enumerate().map(|(i, chunk)| {
                let mut chunk_ctx = svc_ctx.clone();
                chunk_ctx.body = chunk;
                if i > 0 {
                    // Subsequent chunks get empty title (overflow_display_title_once)
                    chunk_ctx.title = String::new();
                } else {
                    chunk_ctx.title = original_title.clone();
                }
                chunk_ctx
            }).collect()
        }
    }
}

fn default_storage_path() -> String {
    dirs::data_local_dir()
        .map(|p| p.join("apprise").join("cache").to_string_lossy().to_string())
        .unwrap_or_else(|| "/tmp/apprise/cache".to_string())
}
