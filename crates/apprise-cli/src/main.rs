mod cli;

use apprise_core::{Apprise, NotifyContext, NotifyFormat, NotifyType, StorageMode, default_storage_path, storage::PersistentStore, utils::parse::mask_url};
use clap::Parser;
use cli::{Cli, Commands};

#[tokio::main]
async fn main() {
  let cli = Cli::parse();

  // Configure tracing
  let level = if cli.debug || cli.verbose >= 3 {
    tracing::Level::DEBUG
  } else if cli.verbose >= 1 {
    tracing::Level::INFO
  } else {
    tracing::Level::WARN
  };
  tracing_subscriber::fmt().with_max_level(level).init();

  if !cli.plugin_path.is_empty() {
    eprintln!("Warning: --plugin-path is not supported in the Rust port. External plugins are ignored.");
  }

  // Handle storage subcommand
  if let Some(Commands::Storage(storage_args)) = &cli.command {
    let storage_path = storage_args.storage_path.clone().or_else(|| cli.storage_path.clone()).unwrap_or_else(default_storage_path);
    let storage_mode = cli.storage_mode.parse::<StorageMode>().unwrap_or(StorageMode::Auto);
    let store =
      PersistentStore::new(std::path::PathBuf::from(&storage_path), storage_args.storage_uid_length as usize, storage_args.storage_prune_days, storage_mode);
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
      "prune" => println!("Pruned {} entries.", store.prune().await),
      "clean" => println!("Cleaned {} entries.", store.clean().await),
      other => {
        eprintln!("Unknown storage action: {}", other);
        std::process::exit(2);
      }
    }
    return;
  }

  // --details
  if cli.details {
    let all = Apprise::all_service_details();
    println!("{:<30} {:<50} Protocols", "Service", "Description");
    println!("{}", "-".repeat(100));
    for d in all {
      println!("{:<30} {:<50} {}", d.service_name, d.description, d.protocols.join(", "));
    }
    return;
  }

  // --schema
  if cli.schema {
    let all = Apprise::all_service_details();
    let json = serde_json::to_string_pretty(&all.iter().map(|d| d.to_json()).collect::<Vec<_>>()).unwrap();
    println!("{}", json);
    return;
  }

  // Build Apprise instance
  let mut apprise = Apprise::new();

  // Add URLs from CLI
  for raw in &cli.urls {
    for url in raw.split([';', ',', '\n', '\r']) {
      let url = url.trim();
      if url.is_empty() || !url.contains("://") {
        continue;
      }
      if !apprise.add(url) {
        eprintln!("Warning: could not parse URL: {}", mask_url(url));
      }
    }
  }

  // Add from config files
  for raw_path in &cli.config {
    for cfg_path in raw_path.split([';', '\n', '\r']) {
      let cfg_path = cfg_path.trim();
      if cfg_path.is_empty() {
        continue;
      }
      if let Err(e) = apprise.add_config(cfg_path, cli.recursion_depth).await {
        eprintln!("Warning: failed to load config {}: {}", cfg_path, e);
      }
    }
  }

  // Try default configs if nothing loaded
  if apprise.is_empty() {
    apprise.load_default_configs(cli.recursion_depth).await;
  }

  if apprise.is_empty() {
    eprintln!("No notification services specified.");
    std::process::exit(3);
  }

  // Apply tag filters
  if !cli.tag.is_empty() {
    apprise.set_tag_strings(&cli.tag);
  }

  // Dry run
  if cli.dry_run {
    println!("Dry run - would notify {} service(s):", apprise.len());
    for d in apprise.details() {
      println!("  {} ({})", d.service_name, d.protocols.join(", "));
    }
    return;
  }

  // Build body
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

  // Load attachments
  let mut attachments = Vec::new();
  for path in &cli.attach {
    match Apprise::load_attachment(path).await {
      Ok(att) => attachments.push(att),
      Err(e) => eprintln!("Warning: could not load attachment {}: {}", path, e),
    }
  }

  let ctx = NotifyContext {
    body,
    title: cli.title.unwrap_or_default(),
    notify_type: cli.notification_type.parse::<NotifyType>().unwrap_or(NotifyType::Info),
    body_format: cli.input_format.parse::<NotifyFormat>().unwrap_or(NotifyFormat::Text),
    attachments,
    interpret_escapes: cli.interpret_escapes,
    interpret_emojis: cli.interpret_emojis,
    ..Default::default()
  };

  // Send
  let result = if cli.disable_async { apprise.notify_sequential(&ctx).await } else { apprise.notify(&ctx).await };

  if result.failed > 0 {
    std::process::exit(1);
  }
}
