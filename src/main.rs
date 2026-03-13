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
use notify::{NotifyContext, registry};
use types::{NotifyFormat, NotifyType};
use utils::{emoji::interpret_emojis, escape::interpret_escapes};

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

    // Handle storage subcommand
    if let Some(Commands::Storage(storage_args)) = &cli.command {
        let storage_path = storage_args
            .storage_path
            .clone()
            .or_else(|| cli.storage_path.clone())
            .unwrap_or_else(default_storage_path);
        let prune_days = storage_args.storage_prune_days;
        let uid_length = storage_args.storage_uid_length;
        let store = storage::PersistentStore::new(
            std::path::PathBuf::from(&storage_path),
            uid_length as usize,
            prune_days,
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

    // From direct URLs on command line
    for url in &cli.urls {
        if let Some(svc) = registry::from_url(url) {
            services.push(svc);
        } else {
            eprintln!("Warning: could not parse URL: {}", url);
        }
    }

    // From config files
    let recursion_depth = cli.recursion_depth;
    for cfg_path in &cli.config {
        match config::load_config(cfg_path, recursion_depth).await {
            Ok(mut svcs) => services.append(&mut svcs),
            Err(e) => eprintln!("Warning: failed to load config {}: {}", cfg_path, e),
        }
    }

    if services.is_empty() {
        eprintln!("No notification services specified.");
        std::process::exit(2);
    }

    // Apply tag filter
    let tag_filters: Vec<String> = cli.tag.iter()
        .flat_map(|t| t.split(',').map(|s| s.trim().to_string()))
        .collect();
    if !tag_filters.is_empty() {
        services.retain(|svc| {
            let svc_tags = svc.tags();
            if svc_tags.is_empty() { return true; }
            tag_filters.iter().any(|f| svc_tags.iter().any(|t| t == f))
        });
        if services.is_empty() {
            eprintln!("No services matched the specified tags.");
            std::process::exit(3);
        }
    }

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
            buf.trim().to_string()
        }
    };

    if body.is_empty() {
        eprintln!("No message body provided.");
        std::process::exit(2);
    }

    let mut body = body;
    let title = cli.title.unwrap_or_default();

    if cli.interpret_escapes {
        body = interpret_escapes(&body);
    }
    if cli.interpret_emojis {
        body = interpret_emojis(&body);
    }

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
    };

    // Send notifications
    let mut all_ok = true;
    for svc in &services {
        let name = svc.service_name().to_string();
        match svc.send(&ctx).await {
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

    if !all_ok {
        std::process::exit(1);
    }
}

fn default_storage_path() -> String {
    dirs::data_local_dir()
        .map(|p| p.join("apprise").join("cache").to_string_lossy().to_string())
        .unwrap_or_else(|| "/tmp/apprise/cache".to_string())
}
