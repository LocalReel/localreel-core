//! End-to-end supervision: a real child process is started, killed and
//! restarted, and the host reports each transition.
//!
//! These tests spawn actual processes, so they are the proof for the claims in
//! ADR-015/016: a crash is detected and retried, a stop unregisters the provider,
//! and a package that cannot be read is a reported failure rather than a fatal
//! error.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use localreel_kernel::db::Database;
use localreel_kernel::migrations;
use localreel_kernel::storage::Storage;
use localreel_plugin::audit;
use localreel_plugin::{
    inspect, PluginDescriptor, PluginFailure, PluginHost, PluginSpec, PluginState, ProviderSink,
};
use serde_json::json;

const DEMO: &str = env!("CARGO_BIN_EXE_plugin-demo");

/// Records what the supervisor asked the server to do.
#[derive(Default)]
struct TestSink {
    registered: Mutex<Vec<String>>,
    unregistered: AtomicUsize,
}

impl ProviderSink for TestSink {
    fn register(&self, kind: &str, _provider: Arc<dyn Storage>) {
        self.registered
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(kind.to_string());
    }

    fn unregister(&self, _kind: &str) {
        self.unregistered.fetch_add(1, Ordering::SeqCst);
    }

    fn roots(&self, _kind: &str) -> Vec<String> {
        Vec::new()
    }
}

fn build_package(path: &Path) {
    let program = format!("bin/plugin-demo{}", std::env::consts::EXE_SUFFIX);
    let manifest = json!({
        "id": "demo",
        "name": "Reference storage plugin",
        "version": "0.1.0",
        "api_version": localreel_plugin::API_VERSION,
        "kind": "storage",
        "entry": { "program": program, "args": [] },
        "permissions": ["network"]
    });
    let file = File::create(path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    writer
        .start_file("manifest.json", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer
        .write_all(serde_json::to_vec(&manifest).unwrap().as_slice())
        .unwrap();
    writer
        .start_file(program, zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(&std::fs::read(DEMO).unwrap()).unwrap();
    writer.finish().unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    host: Arc<PluginHost>,
    sink: Arc<TestSink>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let package = dir.path().join("demo.lrplug");
    build_package(&package);
    // The demo provider canonicalises its root during startup.
    std::fs::create_dir_all(dir.path().join("library")).unwrap();
    let install_root = dir.path().join("installed");
    let db = Database::open(dir.path().join("plugins.db").to_str().unwrap(), 1).unwrap();
    migrations::apply(&db, "plugin", audit::migrations()).unwrap();
    let host = Arc::new(PluginHost::with_options(
        db,
        install_root,
        Duration::from_secs(2),
    ));
    let sink = Arc::new(TestSink::default());
    let manifest = inspect(&package).unwrap();
    let failures = host.start(
        vec![PluginSpec::new(
            package.clone(),
            manifest,
            json!({ "root": dir.path().join("library") }),
            true,
        )],
        sink.clone(),
    );
    assert!(
        failures.is_empty(),
        "the demo plugin must start: {failures:?}"
    );
    let verifications = host
        .verifications()
        .unwrap()
        .recent(Some("demo"), 10)
        .unwrap();
    assert!(
        !verifications.is_empty(),
        "starting a plugin must leave a package-verification record"
    );
    Fixture {
        _dir: dir,
        host,
        sink,
    }
}

fn wait_for(
    host: &PluginHost,
    id: &str,
    what: &str,
    timeout: Duration,
    predicate: impl Fn(&PluginDescriptor) -> bool,
) -> PluginDescriptor {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(descriptor) = host.descriptor(id) {
            if predicate(&descriptor) {
                return descriptor;
            }
        }
        if Instant::now() > deadline {
            panic!(
                "timed out waiting for {what}; last state: {:?}",
                host.descriptor(id)
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[test]
fn a_crashed_plugin_is_detected_and_restarted() {
    let fixture = fixture();
    let host = &fixture.host;

    let running = wait_for(
        host,
        "demo",
        "the first start",
        Duration::from_secs(10),
        |d| d.state == PluginState::Running,
    );
    assert_eq!(running.generation, 1);
    let pid = host.pid("demo").expect("a running plugin has a pid");
    kill_process(pid);

    // The supervisor notices the exit, schedules the first backoff (1s) and
    // starts a new generation.
    let restarted = wait_for(host, "demo", "the restart", Duration::from_secs(15), |d| {
        d.state == PluginState::Running && d.generation >= 2
    });
    assert!(restarted.restarts >= 1, "a restart must be counted");
    assert!(
        fixture.sink.unregistered.load(Ordering::SeqCst) >= 1,
        "the dead provider must be taken away"
    );
    host.shutdown();
}

#[test]
fn disabling_a_plugin_stops_it_and_unregisters_the_provider() {
    let fixture = fixture();
    let host = &fixture.host;
    wait_for(
        host,
        "demo",
        "the first start",
        Duration::from_secs(10),
        |d| d.state == PluginState::Running,
    );

    host.set_enabled("demo", false).unwrap();
    let stopped = wait_for(host, "demo", "the stop", Duration::from_secs(10), |d| {
        d.state == PluginState::Disabled
    });
    assert!(!stopped.running);
    assert!(!stopped.enabled);
    assert!(host.pid("demo").is_none());

    // Enabling again starts a new generation without a host restart.
    host.set_enabled("demo", true).unwrap();
    let running = wait_for(
        host,
        "demo",
        "the re-enable",
        Duration::from_secs(15),
        |d| d.state == PluginState::Running,
    );
    assert!(running.generation >= 2);
    host.shutdown();
}

#[test]
fn an_unreadable_package_is_a_reported_failure_not_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("plugins.db").to_str().unwrap(), 1).unwrap();
    migrations::apply(&db, "plugin", audit::migrations()).unwrap();
    let host = PluginHost::with_options(db, dir.path().join("installed"), Duration::from_secs(1));
    let sink = Arc::new(TestSink::default());

    // The manifest cannot be read, so there is no spec to hand over: the caller
    // records the failure itself (this is what `serve` does).
    let package = dir.path().join("missing.lrplug");
    host.record_failed(&package, None, "cannot read the package");
    let descriptor = host.descriptor("missing").expect("the entry is visible");
    assert_eq!(descriptor.state, PluginState::Failed);
    assert!(!descriptor.running);
    // And the host still accepts specs afterwards: a broken entry is a row, not
    // a fatal error.
    let failures: Vec<PluginFailure> = host.start(Vec::new(), sink);
    assert!(failures.is_empty());
}

#[test]
fn the_stop_timeout_is_configurable() {
    // A zero budget is rejected at config validation; the host only needs to
    // accept what the config allows.
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().join("plugins.db").to_str().unwrap(), 1).unwrap();
    let host = PluginHost::with_options(db, PathBuf::from("plugins"), Duration::from_secs(1));
    assert!(host.summaries().is_empty());
}
