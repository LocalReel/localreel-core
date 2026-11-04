//! WebDAV sidecar plugin entry point.

use localreel_plugin_webdav::handler_from_config;
use serde_json::Value;

fn main() {
    let config: Value = std::env::var("LOCALREEL_PLUGIN_CONFIG")
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let handler = handler_from_config(&config).unwrap_or_else(|error| {
        eprintln!("webdav plugin configuration error: {error}");
        std::process::exit(1);
    });
    if let Err(error) = localreel_plugin::server::run(Box::new(handler)) {
        eprintln!("webdav plugin server failed: {error}");
        std::process::exit(1);
    }
}
