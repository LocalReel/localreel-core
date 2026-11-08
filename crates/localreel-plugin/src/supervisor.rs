//! Converging observed plugin state towards the desired one.
//!
//! One thread owns every blocking lifecycle step: extract, spawn, handshake,
//! health probe, drain, kill. Callers never start or stop a process directly;
//! they change an intent (`restart`, `set_enabled`) and this loop does the work.
//! That is what makes restarts race-free: there is exactly one writer, and a
//! start that is superseded by a newer intent is disposed by the next pass.
//!
//! Copied from Cordis: the state machine, the "one transition at a time"
//! discipline, and the rule that a plugin is not torn down while its callers
//! are still inside it. Not copied from Cordis: everything that only makes
//! sense when the plugin cannot die 鈥?backoff, crash detection, and the fact
//! that a failure is a state rather than an exception.

use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use localreel_kernel::storage::Storage;
use tracing::{info, warn};

use crate::gate::Gate;
use crate::host::{
    Desired, Inner, PluginDescriptor, PluginFailure, PluginSpec, PluginState, Shared,
};
use crate::manifest::PluginError;
use crate::package::{digest, extract, verify_signature};
use crate::runtime::{PluginRuntime, PluginSandbox};
use crate::storage::PluginStorage;

/// What the supervisor may do to a plugin's storage registration.
///
/// Implemented by the server, which owns the registry and the source list; the
/// plugin crate deliberately knows nothing about either.
pub trait ProviderSink: Send + Sync {
    fn register(&self, kind: &str, provider: Arc<dyn Storage>);
    fn unregister(&self, kind: &str);
    /// Source URIs this plugin owns (`kind == plugin id`).
    fn roots(&self, kind: &str) -> Vec<String>;
    /// Called whenever a descriptor changed. The server forwards this to the
    /// event bus so the UI can refresh without polling.
    fn state_changed(&self, _descriptor: &PluginDescriptor) {}
}

/// Publish the current descriptor for `id`, if the entry still exists.
fn notify(sink: &dyn ProviderSink, shared: &Shared, id: &str) {
    let descriptor = shared
        .read()
        .entries
        .get(id)
        .map(|entry| entry.descriptor.clone());
    if let Some(descriptor) = descriptor {
        sink.state_changed(&descriptor);
    }
}

/// A request from the API surface. Applied on the next convergence pass.
pub(crate) enum Intent {
    Restart(String),
    SetEnabled(String, bool),
    Shutdown,
}

const TICK: Duration = Duration::from_millis(250);
const HEALTH_INTERVAL: Duration = Duration::from_secs(10);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// Consecutive failed probes before the plugin is reported unhealthy.
const UNHEALTHY_AFTER: u32 = 3;
/// Consecutive failed probes before the process is replaced.
const RESTART_AFTER_UNHEALTHY: u32 = 6;
/// How many health-driven restarts before the supervisor gives up.
const MAX_UNHEALTHY_RESTARTS: u32 = 3;
/// Start attempts (including the first) before a transient failure is final.
const MAX_ATTEMPTS: u32 = 5;
/// How long a stop waits for the child to exit after stdin is closed.
const STOP_GRACE: Duration = Duration::from_secs(1);

enum Action {
    Start(String),
    Stop {
        id: String,
        reason: &'static str,
        then: Desired,
    },
    Probe(String),
    Exited {
        id: String,
        status: String,
    },
}

/// The supervisor loop. Runs until `Shutdown` or the intent channel closes.
pub(crate) fn run(
    shared: Arc<Shared>,
    sink: Arc<dyn ProviderSink>,
    rx: Receiver<Intent>,
    boot: Option<Sender<Vec<PluginFailure>>>,
) {
    converge(&shared, &sink, Instant::now());
    if let Some(boot) = boot {
        // One pass is enough for `strict` mode to decide: everything that is
        // still `failed` after the first attempt was reported.
        let _ = boot.send(boot_failures(&shared));
    }

    loop {
        match rx.recv_timeout(TICK) {
            Ok(Intent::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Intent::Restart(id)) => restart(&shared, &id),
            Ok(Intent::SetEnabled(id, enabled)) => set_enabled(&shared, &id, enabled),
            Err(RecvTimeoutError::Timeout) => {}
        }
        converge(&shared, &sink, Instant::now());
    }

    stop_all(&shared, &sink);
}

fn boot_failures(shared: &Shared) -> Vec<PluginFailure> {
    shared
        .read()
        .entries
        .values()
        .filter(|entry| entry.descriptor.state == PluginState::Failed)
        .map(|entry| PluginFailure {
            id: entry.descriptor.id.clone(),
            error: entry
                .descriptor
                .error
                .clone()
                .unwrap_or_else(|| "plugin failed to start".to_string()),
        })
        .collect()
}

fn converge(shared: &Arc<Shared>, sink: &Arc<dyn ProviderSink>, now: Instant) {
    let actions = {
        let inner = shared.read();
        plan(&inner, now)
    };
    for action in actions {
        match action {
            Action::Start(id) => start_entry(shared, sink, &id),
            Action::Stop { id, reason, then } => stop_entry(shared, sink, &id, reason, then),
            Action::Probe(id) => probe(shared, sink, &id),
            Action::Exited { id, status } => handle_exit(shared, sink, &id, &status),
        }
    }
}

fn plan(inner: &Inner, now: Instant) -> Vec<Action> {
    let mut actions = Vec::new();
    for (id, entry) in &inner.entries {
        // Entries without a spec were never loadable (disabled or unusable);
        // they are reported, not converged.
        let Some(_spec) = &entry.spec else {
            continue;
        };
        match entry.desired {
            Desired::Stopped => {
                if entry.runtime.is_some() {
                    actions.push(Action::Stop {
                        id: id.clone(),
                        reason: "stopped",
                        then: Desired::Stopped,
                    });
                }
            }
            Desired::Running => match &entry.runtime {
                Some(runtime) => {
                    if entry.running_revision != entry.desired_revision {
                        actions.push(Action::Stop {
                            id: id.clone(),
                            reason: "restart",
                            then: Desired::Running,
                        });
                    } else if let Some(status) = runtime.poll_exit() {
                        actions.push(Action::Exited {
                            id: id.clone(),
                            status: status.to_string(),
                        });
                    } else if entry
                        .last_probe
                        .is_none_or(|at| now.saturating_duration_since(at) >= HEALTH_INTERVAL)
                    {
                        actions.push(Action::Probe(id.clone()));
                    }
                }
                None => {
                    if !entry.permanent && entry.next_attempt.is_none_or(|at| at <= now) {
                        actions.push(Action::Start(id.clone()));
                    }
                }
            },
        }
    }
    actions
}

fn start_entry(shared: &Arc<Shared>, sink: &Arc<dyn ProviderSink>, id: &str) {
    // Bump the generation before the blocking work so a late observer cannot
    // mistake the old process for the new one.
    let spec = {
        let mut inner = shared.write();
        let Some(entry) = inner.entries.get_mut(id) else {
            return;
        };
        entry.generation += 1;
        entry.descriptor.state = PluginState::Starting;
        entry.descriptor.running = false;
        entry.spec.clone()
    };
    let Some(spec) = spec else {
        return;
    };

    let install_root = shared.install_root.join(&spec.manifest.id);
    let roots = sink.roots(&spec.manifest.id);
    let host_url = shared
        .host_url
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    match spawn(&spec, &install_root, roots, host_url, shared.limits) {
        Ok(runtime) => {
            let gate = Gate::new();
            sink.register(
                &spec.manifest.id,
                Arc::new(PluginStorage::with_gate(runtime.clone(), gate.clone())),
            );
            {
                let mut inner = shared.write();
                if let Some(entry) = inner.entries.get_mut(id) {
                    entry.runtime = Some(runtime);
                    entry.gate = Some(gate);
                    entry.running_revision = entry.desired_revision;
                    entry.attempts = 0;
                    entry.health_failures = 0;
                    entry.unhealthy_restarts = 0;
                    entry.next_attempt = None;
                    entry.descriptor.state = PluginState::Running;
                    entry.descriptor.running = true;
                    entry.descriptor.enabled = true;
                    entry.descriptor.error = None;
                    entry.descriptor.started_at = Some(localreel_kernel::time::now_secs());
                    entry.descriptor.generation = entry.generation;
                }
            }
            shared.record_audit(id, "start", &format!("version {}", spec.manifest.version));
            info!(plugin = %id, "plugin started");
            notify(sink.as_ref(), shared, id);
        }
        Err((error, permanent)) => {
            {
                let mut inner = shared.write();
                let Some(entry) = inner.entries.get_mut(id) else {
                    return;
                };
                entry.attempts += 1;
                entry.descriptor.state = PluginState::Failed;
                entry.descriptor.running = false;
                entry.descriptor.error = Some(error.clone());
                entry.descriptor.restarts = entry.attempts;
                if permanent || entry.attempts >= MAX_ATTEMPTS {
                    entry.permanent = true;
                    entry.next_attempt = None;
                } else {
                    entry.next_attempt = Some(Instant::now() + backoff(entry.attempts));
                }
            }
            shared.record_audit(id, "start.failed", &error);
            warn!(plugin = %id, "plugin failed to start: {error}");
            notify(sink.as_ref(), shared, id);
        }
    }
}

/// Extract and spawn one generation. The boolean is "permanent failure".
fn spawn(
    spec: &PluginSpec,
    install_root: &Path,
    roots: Vec<String>,
    host_url: Option<String>,
    limits: crate::limits::PluginLimits,
) -> std::result::Result<Arc<PluginRuntime>, (String, bool)> {
    if let Some(expected) = &spec.expected_sha256 {
        let actual = digest(&spec.package).map_err(|error| (error.to_string(), true))?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err((
                format!("package sha256 mismatch: expected {expected}, got {actual}"),
                true,
            ));
        }
    }
    if let Some(signature) = &spec.expected_signature {
        let Some(public_key) = &spec.publisher_public_key else {
            return Err((
                "signed plugin has no trusted publisher key".to_string(),
                true,
            ));
        };
        verify_signature(&spec.package, signature, public_key)
            .map_err(|error| (error.to_string(), true))?;
    }
    let version_dir = install_root.join(&spec.manifest.version);
    let manifest = extract(&spec.package, &version_dir)
        .map_err(|error| (error.to_string(), is_permanent(&error)))?;
    let program = version_dir.join(&manifest.entry.program);
    let runtime = PluginRuntime::start(
        &program,
        &manifest,
        &spec.config,
        &PluginSandbox {
            working_dir: Some(install_root.to_path_buf()),
            roots,
            host_url,
            limits,
        },
    )
    .map_err(|error| (error.to_string(), is_permanent(&error)))?;
    Ok(Arc::new(runtime))
}

/// A package that cannot be read is a configuration problem, not a transient
/// one: retrying it five times only delays the error.
fn is_permanent(error: &PluginError) -> bool {
    matches!(
        error,
        PluginError::Manifest(_)
            | PluginError::ApiVersion(_)
            | PluginError::ManifestTooLarge
            | PluginError::PackageTooLarge
            | PluginError::TooManyEntries
            | PluginError::ExtractedTooLarge
            | PluginError::UnsafeEntry(_)
            | PluginError::Signature(_)
            | PluginError::Missing(_)
            | PluginError::Archive(_)
    )
}

/// 1s, 2s, 4s, 8s, 16s (then the attempt limit stops it).
fn backoff(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(4);
    Duration::from_secs(1 << shift)
}

fn stop_entry(
    shared: &Arc<Shared>,
    sink: &Arc<dyn ProviderSink>,
    id: &str,
    reason: &'static str,
    then: Desired,
) {
    let (runtime, gate) = {
        let mut inner = shared.write();
        match inner.entries.get_mut(id) {
            Some(entry) => (entry.runtime.take(), entry.gate.take()),
            None => (None, None),
        }
    };

    // 1. Refuse new calls.
    if let Some(gate) = &gate {
        gate.close();
    }
    // 2. Wait for the readers already inside.
    if let Some(gate) = &gate {
        let deadline = Instant::now() + shared.stop_timeout;
        while gate.inflight() > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        if gate.inflight() > 0 {
            warn!(
                plugin = %id,
                "stop timed out with {} request(s) still in flight",
                gate.inflight()
            );
        }
    }
    // 3. Take the provider away.
    sink.unregister(id);
    // 4. Ask nicely, then insist.
    if let Some(runtime) = &runtime {
        runtime.close_stdin();
        let deadline = Instant::now() + STOP_GRACE;
        while Instant::now() < deadline {
            if runtime.poll_exit().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        runtime.terminate();
    }

    {
        let mut inner = shared.write();
        if let Some(entry) = inner.entries.get_mut(id) {
            entry.runtime = None;
            entry.gate = None;
            entry.descriptor.running = false;
            entry.descriptor.state = match (then, entry.descriptor.enabled) {
                (Desired::Running, _) => PluginState::Stopped,
                (_, false) => PluginState::Disabled,
                (_, true) if entry.permanent && entry.descriptor.error.is_some() => {
                    PluginState::Failed
                }
                (_, true) => PluginState::Stopped,
            };
        }
    }
    drop(runtime);
    shared.record_audit(id, "stop", reason);
    info!(plugin = %id, reason, "plugin stopped");
    notify(sink.as_ref(), shared, id);
}

fn handle_exit(shared: &Arc<Shared>, sink: &Arc<dyn ProviderSink>, id: &str, status: &str) {
    let (runtime, gate) = {
        let mut inner = shared.write();
        match inner.entries.get_mut(id) {
            Some(entry) => (entry.runtime.take(), entry.gate.take()),
            None => (None, None),
        }
    };
    if let Some(gate) = &gate {
        gate.close();
    }
    sink.unregister(id);
    drop(runtime);

    {
        let mut inner = shared.write();
        let Some(entry) = inner.entries.get_mut(id) else {
            return;
        };
        entry.attempts += 1;
        entry.descriptor.restarts = entry.attempts;
        entry.descriptor.running = false;
        entry.descriptor.state = PluginState::Failed;
        entry.descriptor.error = Some(format!("plugin exited unexpectedly ({status})"));
        if entry.attempts >= MAX_ATTEMPTS {
            entry.permanent = true;
            entry.next_attempt = None;
        } else {
            entry.next_attempt = Some(Instant::now() + backoff(entry.attempts));
        }
    }
    shared.record_audit(id, "crash", status);
    warn!(plugin = %id, "plugin exited unexpectedly: {status}");
    notify(sink.as_ref(), shared, id);
}

fn probe(shared: &Arc<Shared>, sink: &Arc<dyn ProviderSink>, id: &str) {
    let runtime = {
        let inner = shared.read();
        inner
            .entries
            .get(id)
            .and_then(|entry| entry.runtime.as_ref().map(Arc::clone))
    };
    let Some(runtime) = runtime else {
        return;
    };
    let healthy = runtime.probe(HEALTH_TIMEOUT);

    let mut changed = false;
    {
        let mut inner = shared.write();
        let Some(entry) = inner.entries.get_mut(id) else {
            return;
        };
        entry.last_probe = Some(Instant::now());
        if healthy {
            entry.health_failures = 0;
            if entry.descriptor.state == PluginState::Unhealthy {
                entry.descriptor.state = PluginState::Running;
                entry.descriptor.error = None;
                changed = true;
                info!(plugin = %id, "plugin recovered");
            }
        } else {
            entry.health_failures += 1;
            if entry.health_failures >= UNHEALTHY_AFTER
                && entry.descriptor.state == PluginState::Running
            {
                entry.descriptor.state = PluginState::Unhealthy;
                entry.descriptor.error = Some("health check is failing".to_string());
                changed = true;
                warn!(plugin = %id, "plugin is unhealthy");
            }

            if entry.health_failures >= RESTART_AFTER_UNHEALTHY {
                entry.health_failures = 0;
                entry.unhealthy_restarts += 1;
                if entry.unhealthy_restarts >= MAX_UNHEALTHY_RESTARTS {
                    entry.permanent = true;
                    entry.desired = Desired::Stopped;
                    entry.descriptor.state = PluginState::Failed;
                    entry.descriptor.error =
                        Some("health checks kept failing after restarts".to_string());
                } else {
                    // Same intent, new generation: the next pass stops and
                    // restarts it.
                    entry.desired_revision += 1;
                }
                changed = true;
            }
        }
    }
    if changed {
        notify(sink.as_ref(), shared, id);
    }
}

fn restart(shared: &Shared, id: &str) {
    let mut inner = shared.write();
    let Some(entry) = inner.entries.get_mut(id) else {
        return;
    };
    entry.desired = Desired::Running;
    entry.desired_revision += 1;
    entry.permanent = false;
    entry.attempts = 0;
    entry.next_attempt = None;
    entry.health_failures = 0;
    entry.unhealthy_restarts = 0;
    entry.descriptor.enabled = true;
    entry.descriptor.error = None;
    if entry.runtime.is_none() {
        entry.descriptor.state = PluginState::Starting;
    }
}

fn set_enabled(shared: &Shared, id: &str, enabled: bool) {
    let mut inner = shared.write();
    let Some(entry) = inner.entries.get_mut(id) else {
        return;
    };
    entry.descriptor.enabled = enabled;
    if enabled {
        entry.desired = Desired::Running;
        entry.desired_revision += 1;
        entry.permanent = false;
        entry.attempts = 0;
        entry.next_attempt = None;
        entry.health_failures = 0;
        entry.unhealthy_restarts = 0;
        entry.descriptor.error = None;
        if entry.runtime.is_none() {
            entry.descriptor.state = PluginState::Starting;
        }
    } else {
        entry.desired = Desired::Stopped;
        entry.permanent = true;
        entry.next_attempt = None;
        if entry.spec.is_none() {
            entry.descriptor.state = PluginState::Disabled;
            entry.descriptor.running = false;
        }
    }
}

fn stop_all(shared: &Arc<Shared>, sink: &Arc<dyn ProviderSink>) {
    let ids: Vec<String> = shared.read().entries.keys().cloned().collect();
    for id in ids {
        let running = shared
            .read()
            .entries
            .get(&id)
            .is_some_and(|entry| entry.runtime.is_some());
        if running {
            stop_entry(shared, sink, &id, "shutdown", Desired::Stopped);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_then_saturates() {
        let seconds: Vec<u64> = (1..=6).map(|attempt| backoff(attempt).as_secs()).collect();
        assert_eq!(seconds, vec![1, 2, 4, 8, 16, 16]);
    }

    #[test]
    fn packaging_errors_are_permanent_but_handshakes_are_not() {
        assert!(is_permanent(&PluginError::Manifest("bad".into())));
        assert!(is_permanent(&PluginError::ApiVersion("v9".into())));
        assert!(is_permanent(&PluginError::Missing("bin/plugin".into())));
        assert!(!is_permanent(&PluginError::NotReady("timeout".into())));
        assert!(!is_permanent(&PluginError::Request("refused".into())));
    }

    #[test]
    fn a_bad_publisher_signature_is_permanent() {
        use ed25519_dalek::SigningKey;

        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("demo.lrplug");
        std::fs::write(&package, b"not a zip").unwrap();
        let manifest = crate::manifest::Manifest::parse(
            br#"{
                "id": "demo",
                "name": "Demo",
                "version": "0.1.0",
                "api_version": "localreel.plugin/v1",
                "kind": "storage",
                "entry": { "program": "plugin" }
            }"#,
        )
        .unwrap();
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let public_key: String = signing
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let spec = PluginSpec::new(package, manifest, serde_json::json!({}), true).with_signature(
            Some("acme".into()),
            Some("00".repeat(64)),
            Some(public_key),
        );
        let result = spawn(
            &spec,
            &dir.path().join("install"),
            Vec::new(),
            None,
            crate::limits::PluginLimits::default(),
        );
        let error = match result {
            Ok(_) => panic!("a bad signature must not start"),
            Err(error) => error,
        };
        assert!(error.1, "a bad signature is permanent");
        assert!(error.0.contains("signature"), "{}", error.0);
    }

    #[test]
    fn a_sha256_mismatch_is_permanent_and_precedes_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("demo.lrplug");
        std::fs::write(&package, b"not a zip").unwrap();
        let manifest = crate::manifest::Manifest::parse(
            br#"{
                "id": "demo",
                "name": "Demo",
                "version": "0.1.0",
                "api_version": "localreel.plugin/v1",
                "kind": "storage",
                "entry": { "program": "plugin" }
            }"#,
        )
        .unwrap();
        let spec = PluginSpec::new(package, manifest, serde_json::json!({}), true)
            .with_sha256(Some("00".repeat(32)));
        let result = spawn(
            &spec,
            &dir.path().join("install"),
            Vec::new(),
            None,
            crate::limits::PluginLimits::default(),
        );
        let error = match result {
            Ok(_) => panic!("a mismatched package must not start"),
            Err(error) => error,
        };
        assert!(error.1, "a digest mismatch is permanent");
        assert!(error.0.contains("sha256 mismatch"), "{}", error.0);
    }
}
