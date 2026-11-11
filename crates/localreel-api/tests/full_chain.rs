//! Full chain: a real `PluginHost`, real API router, a real `fs.read` grant and
//! a stdio storage plugin whose stat/list/read go through the broker.

#[cfg(any(windows, target_os = "linux"))]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use localreel_api::state::AppStateParts;
use localreel_api::{router, AppState};
use localreel_kernel::config::Config;
use localreel_kernel::db::Database;
use localreel_kernel::events::EventBus;
use localreel_kernel::jobs::JobQueue;
use localreel_kernel::migrations;
use localreel_kernel::storage::{MediaUri, Storage};
use localreel_media::MediaService;
use localreel_plugin::{PluginHost, PluginSpec, ProviderSink};
use serde_json::json;

struct Sink {
    provider: Mutex<Option<Arc<dyn Storage>>>,
}

impl Default for Sink {
    fn default() -> Self {
        Self {
            provider: Mutex::new(None),
        }
    }
}

impl ProviderSink for Sink {
    fn register(&self, _kind: &str, provider: Arc<dyn Storage>) {
        *self.provider.lock().unwrap_or_else(|p| p.into_inner()) = Some(provider);
    }

    fn unregister(&self, _kind: &str) {
        *self.provider.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    fn roots(&self, _kind: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Build (once) and locate the isolation wrapper.
///
/// Defined on every platform: `full_chain` references it in shared code and only
/// passes it when the caller asks for OS isolation, so gating the helper to the
/// platforms that use it would not compile on macOS.
fn sandbox_program() -> PathBuf {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("debug")
        .join(format!("plugin-sandbox{}", std::env::consts::EXE_SUFFIX));
    if !target.is_file() {
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "localreel-plugin", "--bin", "plugin-sandbox"])
            .status()
            .expect("cargo build plugin-sandbox");
        assert!(status.success());
    }
    assert!(target.is_file(), "{} is missing", target.display());
    target
}

#[cfg(target_os = "linux")]
fn sandbox_is_available() -> bool {
    localreel_plugin::runtime::sandbox_support().unshare_present
}

#[cfg(not(target_os = "linux"))]
fn sandbox_is_available() -> bool {
    true
}

async fn full_chain(app_container: bool) {
    if app_container && !sandbox_is_available() {
        eprintln!("skipping: OS isolation is unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("library");
    std::fs::create_dir_all(&library).unwrap();
    std::fs::write(library.join("a.txt"), b"abcdef").unwrap();

    // Build the fixture package.
    let source = dir.path().join("plugin-src");
    std::fs::create_dir_all(source.join("bin")).unwrap();
    let program = format!("bin/plugin-fs-fixture{}", std::env::consts::EXE_SUFFIX);
    std::fs::copy(
        env!("CARGO_BIN_EXE_plugin-fs-fixture"),
        source.join(&program),
    )
    .unwrap();
    std::fs::write(
        source.join("manifest.json"),
        serde_json::to_vec(&json!({
            "id": "fs-fixture",
            "name": "FS fixture",
            "version": "0.1.0",
            "api_version": localreel_plugin::API_VERSION,
            "kind": "storage",
            "entry": { "program": program, "args": [], "transport": "stdio" },
            "permissions": []
        }))
        .unwrap(),
    )
    .unwrap();
    let package = dir.path().join("fs-fixture.lrplug");
    localreel_plugin::package::pack(&source, &package).unwrap();
    let manifest = localreel_plugin::inspect(&package).unwrap();

    let db = Database::open(dir.path().join("api.db").to_str().unwrap(), 1).unwrap();
    migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
    migrations::apply(&db, "media", localreel_media::migrations::list()).unwrap();
    migrations::apply(&db, "plugin", localreel_plugin::audit::migrations()).unwrap();

    let host = Arc::new(PluginHost::with_options(
        db.clone(),
        dir.path().join("installed"),
        Duration::from_secs(5),
    ));
    host.grants()
        .unwrap()
        .grant(
            "fs-fixture",
            "0.1.0",
            "fs.read",
            json!({ "roots": [library.to_string_lossy()], "read_only": true }),
            None,
            None,
        )
        .unwrap();

    let mut config = Config::default();
    config.set_data_dir(dir.path().to_path_buf());
    let events = EventBus::new(16);
    let jobs = JobQueue::new(db.clone(), events.clone());
    let state = AppState::new(AppStateParts {
        config,
        db: db.clone(),
        events,
        jobs,
        media: MediaService::new(db.clone()),
        accounts: localreel_media::AccountService::new(db.clone()),
        transcodes: Arc::new(localreel_media::TranscodeGate::new(
            2,
            Duration::from_secs(900),
        )),
        plugins: host.clone(),
        config_source: None,
        metadata_provider: None,
        ffmpeg_capability: localreel_kernel::media_ops::FfmpegCapability::Missing,
        hwaccel: localreel_kernel::media_ops::HwAccel::None,
    });

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    host.with_host_url(format!("http://{address}"));

    let sink = Arc::new(Sink::default());
    let mut spec = PluginSpec::new(package, manifest, json!({}), true);
    if app_container {
        spec = spec.with_app_container(true, Some(sandbox_program()));
    }
    let failures = host.start(vec![spec], sink.clone());
    assert!(failures.is_empty(), "{failures:?}");

    let deadline = Instant::now() + Duration::from_secs(10);
    let provider = loop {
        if let Some(provider) = sink
            .provider
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            break provider;
        }
        assert!(Instant::now() < deadline, "plugin did not register");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let chain = host.verifications().unwrap().verify_chain().unwrap();
    assert!(chain.valid, "{chain:?}");
    assert!(chain.anchored, "{chain:?}");
    assert_eq!(chain.anchor_valid, Some(true), "{chain:?}");

    let file = MediaUri::Local(library.join("a.txt"));
    assert_eq!(provider.stat(&file).unwrap().size, 6);
    let listed = provider.list(&MediaUri::Local(library.clone())).unwrap();
    assert_eq!(listed[0].name, "a.txt");

    use std::io::Read;
    let mut reader = provider.open_range(&file, 1, 3).unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"bcd");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_plugin_uses_real_grant_and_api() {
    full_chain(false).await;
}

#[cfg(any(windows, target_os = "linux"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn appcontainer_storage_uses_real_grant_and_api() {
    full_chain(true).await;
}
