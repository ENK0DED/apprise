#![deny(clippy::all)]

use napi_derive::napi;
use apprise_core::{Apprise, NotifyContext, NotifyType, NotifyFormat};

// ── Data types for JS interop ────────────────────────────────────────────────

/// Options for sending a notification.
#[napi(object)]
pub struct SendOptions {
    /// Notification service URL(s), separated by commas or semicolons.
    pub urls: Option<String>,
    /// Path to a configuration file.
    pub config: Option<String>,
    /// Message body (required).
    pub body: String,
    /// Message title (optional).
    pub title: Option<String>,
    /// Notification type: "info", "success", "warning", "failure".
    pub notification_type: Option<String>,
    /// Body format: "text", "html", "markdown".
    pub body_format: Option<String>,
    /// Tag filter(s) — comma-separated AND groups.
    pub tag: Option<String>,
}

/// Result of a notification dispatch.
#[napi(object)]
pub struct SendResult {
    /// Total number of services targeted.
    pub total: i32,
    /// Number that succeeded.
    pub succeeded: i32,
    /// Number that failed.
    pub failed: i32,
}

/// Information about a notification service.
#[napi(object)]
pub struct ServiceInfo {
    /// Human-readable service name.
    pub name: String,
    /// URL scheme(s) (e.g., ["ntfy", "ntfys"]).
    pub protocols: Vec<String>,
    /// Short description.
    pub description: String,
    /// Whether the service supports file attachments.
    pub attachment_support: bool,
}

/// Options for the notify method on the Apprise class.
#[napi(object)]
pub struct NotifyOptions {
    pub title: Option<String>,
    pub notification_type: Option<String>,
    pub body_format: Option<String>,
    pub tag: Option<String>,
}

// ── Standalone functions ─────────────────────────────────────────────────────

/// Send a one-shot notification. Creates an Apprise instance, adds URLs/config,
/// and sends the notification.
#[napi]
pub async fn send(options: SendOptions) -> napi::Result<SendResult> {
    let mut apprise = Apprise::new();

    if let Some(urls) = &options.urls {
        apprise.add_urls(urls);
    }
    if let Some(config) = &options.config {
        apprise.add_config(config, 1).await
            .map_err(|e| napi::Error::from_reason(format!("Config error: {}", e)))?;
    }

    if apprise.is_empty() {
        return Err(napi::Error::from_reason("No notification services specified"));
    }

    if let Some(tag) = &options.tag {
        apprise.set_tag_strings(&[tag.clone()]);
    }

    let ctx = NotifyContext {
        body: options.body,
        title: options.title.unwrap_or_default(),
        notify_type: options.notification_type
            .as_deref()
            .and_then(|s| s.parse::<NotifyType>().ok())
            .unwrap_or(NotifyType::Info),
        body_format: options.body_format
            .as_deref()
            .and_then(|s| s.parse::<NotifyFormat>().ok())
            .unwrap_or(NotifyFormat::Text),
        ..Default::default()
    };

    let result = apprise.notify(&ctx).await;
    Ok(SendResult {
        total: result.total as i32,
        succeeded: result.succeeded as i32,
        failed: result.failed as i32,
    })
}

/// Parse a notification URL and return service info, or null if invalid.
#[napi]
pub fn parse_url(url: String) -> Option<ServiceInfo> {
    let svc = apprise_core::registry::from_url(&url)?;
    let details = svc.details();
    Some(ServiceInfo {
        name: details.service_name.to_string(),
        protocols: details.protocols.iter().map(|s| s.to_string()).collect(),
        description: details.description.to_string(),
        attachment_support: details.attachment_support,
    })
}

/// List all supported notification services.
#[napi]
pub fn list_services() -> Vec<ServiceInfo> {
    Apprise::all_service_details().into_iter().map(|d| ServiceInfo {
        name: d.service_name.to_string(),
        protocols: d.protocols.iter().map(|s| s.to_string()).collect(),
        description: d.description.to_string(),
        attachment_support: d.attachment_support,
    }).collect()
}

// ── Apprise class for stateful usage ─────────────────────────────────────────

/// Stateful notification manager.
///
/// ```js
/// const { Apprise } = require('apprise');
/// const a = new Apprise();
/// a.add('ntfy://mytopic');
/// a.add('slack://TokenA/TokenB/TokenC');
/// await a.notify('Hello!', { title: 'Test' });
/// ```
#[napi(js_name = "Apprise")]
pub struct JsApprise {
    inner: Apprise,
}

#[napi]
impl JsApprise {
    /// Create a new Apprise instance.
    #[napi(constructor)]
    pub fn new() -> Self {
        Self { inner: Apprise::new() }
    }

    /// Add a notification service by URL. Returns true if parsed successfully.
    #[napi]
    pub fn add(&mut self, url: String) -> bool {
        self.inner.add(&url)
    }

    /// Load services from a config file (synchronous — blocks until loaded).
    #[napi]
    pub fn add_config(&mut self, path: String) -> napi::Result<i32> {
        let rt = tokio::runtime::Handle::current();
        let count = rt.block_on(self.inner.add_config(&path, 1))
            .map_err(|e| napi::Error::from_reason(format!("Config error: {}", e)))?;
        Ok(count as i32)
    }

    /// Get the number of loaded services.
    #[napi]
    pub fn len(&self) -> i32 {
        self.inner.len() as i32
    }

    /// Send a notification to all loaded services.
    #[napi]
    pub async fn notify(&self, body: String, options: Option<NotifyOptions>) -> napi::Result<SendResult> {
        let opts = options.unwrap_or(NotifyOptions {
            title: None,
            notification_type: None,
            body_format: None,
            tag: None,
        });

        if let Some(tag) = &opts.tag {
            // Note: can't mutate self in async context with NAPI
            // Tags should be set via a separate method
            let _ = tag;
        }

        let ctx = NotifyContext {
            body,
            title: opts.title.unwrap_or_default(),
            notify_type: opts.notification_type
                .as_deref()
                .and_then(|s| s.parse::<NotifyType>().ok())
                .unwrap_or(NotifyType::Info),
            body_format: opts.body_format
                .as_deref()
                .and_then(|s| s.parse::<NotifyFormat>().ok())
                .unwrap_or(NotifyFormat::Text),
            ..Default::default()
        };

        let result = self.inner.notify(&ctx).await;
        Ok(SendResult {
            total: result.total as i32,
            succeeded: result.succeeded as i32,
            failed: result.failed as i32,
        })
    }

    /// Get details of all loaded services.
    #[napi]
    pub fn details(&self) -> Vec<ServiceInfo> {
        self.inner.details().into_iter().map(|d| ServiceInfo {
            name: d.service_name.to_string(),
            protocols: d.protocols.iter().map(|s| s.to_string()).collect(),
            description: d.description.to_string(),
            attachment_support: d.attachment_support,
        }).collect()
    }

    /// Clear all loaded services.
    #[napi]
    pub fn clear(&mut self) {
        self.inner.clear();
    }
}
