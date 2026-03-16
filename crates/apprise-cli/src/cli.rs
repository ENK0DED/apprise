use clap::{ArgAction, Args, Parser, Subcommand};

/// Apprise - Push notifications that work with everything
#[derive(Parser, Debug)]
#[command(
    name = "apprise",
    version = env!("CARGO_PKG_VERSION"),
    about = "Send push notifications to one or more services.",
    long_about = None,
    after_help = "For a list of supported services, use: apprise --details\n\nFor service URL formats, visit: https://github.com/caronc/apprise",
)]
pub struct Cli {
  /// Message body (read from stdin if not provided and no URLs given)
  #[arg(short = 'b', long = "body", env = "APPRISE_BODY")]
  pub body: Option<String>,

  /// Message title
  #[arg(short = 't', long = "title")]
  pub title: Option<String>,

  /// Notification type
  #[arg(
        short = 'n',
        long = "notification-type",
        default_value = "info",
        value_parser = ["info", "success", "warning", "failure"]
    )]
  pub notification_type: String,

  /// Input format of the message body
  #[arg(
        short = 'i',
        long = "input-format",
        default_value = "text",
        value_parser = ["text", "html", "markdown"]
    )]
  pub input_format: String,

  /// Configuration file(s) to load
  #[arg(
        short = 'c',
        long = "config",
        action = ArgAction::Append,
        env = "APPRISE_CONFIG_PATH"
    )]
  pub config: Vec<String>,

  /// Tag filter(s) — multiple uses are OR'd; comma-separated values within one tag are AND'd
  #[arg(short = 'g', long = "tag", action = ArgAction::Append)]
  pub tag: Vec<String>,

  /// Attachment URL(s)
  #[arg(short = 'a', long = "attach", action = ArgAction::Append)]
  pub attach: Vec<String>,

  /// Custom plugin directory path(s)
  #[arg(
        short = 'P',
        long = "plugin-path",
        action = ArgAction::Append,
        env = "APPRISE_PLUGIN_PATH"
    )]
  pub plugin_path: Vec<String>,

  /// Configuration include recursion depth
  #[arg(short = 'R', long = "recursion-depth", default_value = "1")]
  pub recursion_depth: u32,

  /// Theme (currently unused but accepted for compatibility)
  #[arg(short = 'T', long = "theme", default_value = "default")]
  pub theme: String,

  /// Persistent storage path
  #[arg(short = 'S', long = "storage-path", env = "APPRISE_STORAGE_PATH")]
  pub storage_path: Option<String>,

  /// Storage mode
  #[arg(
        long = "storage-mode",
        short_alias = 'M',
        default_value = "auto",
        value_parser = ["auto", "flush", "memory"],
        env = "APPRISE_STORAGE_MODE"
    )]
  pub storage_mode: String,

  /// Number of days before pruning persistent store entries
  #[arg(long = "storage-prune-days", default_value = "30", env = "APPRISE_STORAGE_PRUNE_DAYS")]
  pub storage_prune_days: u32,

  /// UID character length for persistent storage
  #[arg(long = "storage-uid-length", default_value = "8", env = "APPRISE_STORAGE_UID_LENGTH")]
  pub storage_uid_length: u32,

  /// Trial run — show what would be sent without sending
  #[arg(short = 'd', long = "dry-run")]
  pub dry_run: bool,

  /// Send notifications sequentially (disable async)
  #[arg(long = "disable-async")]
  pub disable_async: bool,

  /// Enable backslash escape interpretation in body
  #[arg(short = 'e', long = "interpret-escapes")]
  pub interpret_escapes: bool,

  /// Enable :emoji: interpretation in body
  #[arg(short = 'j', long = "interpret-emojis")]
  pub interpret_emojis: bool,

  /// Increase verbosity (-v, -vv, -vvv, -vvvv)
  #[arg(short = 'v', long = "verbose", action = ArgAction::Count)]
  pub verbose: u8,

  /// Enable debug output (forces verbosity ≥ 3)
  #[arg(short = 'D', long = "debug")]
  pub debug: bool,

  /// Print details of all supported services and exit
  #[arg(short = 'l', long = "details")]
  pub details: bool,

  /// Output JSON schema for all services and exit
  #[arg(long = "schema")]
  pub schema: bool,

  /// Notification service URL(s)
  #[arg(
        name = "URL",
        action = ArgAction::Append,
        env = "APPRISE_URLS"
    )]
  pub urls: Vec<String>,

  #[command(subcommand)]
  pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
  /// Manage persistent storage
  Storage(StorageArgs),
}

#[derive(Args, Debug)]
pub struct StorageArgs {
  /// Storage action to perform
  #[arg(
        value_parser = ["list", "prune", "clean"],
        default_value = "list"
    )]
  pub action: String,

  /// Specific UID(s) to target
  #[arg(name = "UID")]
  pub uids: Vec<String>,

  /// Persistent storage path
  #[arg(short = 'S', long = "storage-path", env = "APPRISE_STORAGE_PATH")]
  pub storage_path: Option<String>,

  /// Days before pruning persistent store entries
  #[arg(long = "storage-prune-days", default_value = "30", env = "APPRISE_STORAGE_PRUNE_DAYS")]
  pub storage_prune_days: u32,

  /// UID character length
  #[arg(long = "storage-uid-length", default_value = "8", env = "APPRISE_STORAGE_UID_LENGTH")]
  pub storage_uid_length: u32,
}
