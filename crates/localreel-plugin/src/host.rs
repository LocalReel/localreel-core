//! The configured plugin set: states, tokens, permissions, and the network
//! proxy.
//!
//! The host is the single source of truth for "what happened to this plugin".
//! A configuration entry becomes a [`PluginDescriptor`] whether it started or
//! not, so a failure is a reported state instead of a missing row: the admin
//! page can explain a plugin that is not running, and a source that depends on
//! it can answer "unavailable" instead of "unknown".
//!
//! Lifecycle is declarative. Callers change *desired* state (`restart`,
//! `set_enabled`); the supervisor thread converges *actual* state towards it and
//! owns every blocking step (start, drain, kill). Reads never wait for a start.
//!
//! A sidecar can always open its own sockets — the host cannot stop that without
//! an OS sandbox. What it *can* do is stop being the accomplice: the host proxy
//! is the only sanctioned way out, it requires the `network` permission, and it
//! records every request.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use serde::Serialize;

use crate::audit::{AuditEvent, AuditLog};
use crate::broker::{self, BrokerError, BrokerRequest, BrokerResponse};
use crate::capability::{
    self, CapabilityDescriptor, NetFetchScope, RequestedCapability, CAPABILITIES,
};
use crate::gate::Gate;
use crate::grants::{CapabilityGrant, GrantStore};
use crate::manifest::{Manifest, PERMISSION_NETWORK};
use crate::package::inspect;
use crate::runtime::PluginRuntime;
use crate::supervisor::{self, Intent, ProviderSink};

/// Upper bound on a proxied response, so one plugin cannot fill the disk.
pub const MAX_PROXY_BYTES: u64 = 32 * 1024 * 1024;
const PROXY_TIMEOUT: Duration = Duration::from_secs(30);

/// What the admin page needs to answer "what does this plugin ask for, what did
/// we grant, and what is the host's own risk description".
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityReport {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub requested: Vec<RequestedCapability>,
    pub granted: Vec<CapabilityGrant>,
    pub catalog: Vec<CapabilityDescriptor>,
    pub forbidden: Vec<String>,
}

fn requested_capabilities(manifest: &Manifest) -> Vec<RequestedCapability> {
    let mut requested = Vec::new();
    for request in manifest.declared_capabilities() {
        let Some(descriptor) = capability::descriptor(&request.name) else {
            continue;
        };
        let tier = capability::validate_request(request)
            .map(|validated| validated.tier)
            .unwrap_or(descriptor.tier);
        requested.push(RequestedCapability {
            name: request.name.clone(),
            scope: request.scope.clone(),
            tier,
            source: "manifest",
            title: descriptor.title,
            description: descriptor.description,
            audit: descriptor.audit,
        });
    }
    for request in manifest.legacy_capability_requests() {
        let Some(descriptor) = capability::descriptor(&request.name) else {
            continue;
        };
        requested.push(RequestedCapability {
            name: request.name,
            scope: request.scope,
            tier: descriptor.tier,
            source: "legacy",
            title: descriptor.title,
            description: descriptor.description,
            audit: descriptor.audit,
        });
    }
    requested
}

/// Lifecycle state of one configured plugin.
///
/// Every value is derived from facts the host observed (a process it started, an
/// error it caught, a switch in the configuration), never asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginState {
    /// Present in the configuration but switched off (`enabled = false`).
    Disabled,
    /// The supervisor is starting the sidecar.
    Starting,
    /// The sidecar process is running and its provider is registered.
    Running,
    /// The process is up but health checks keep failing.
    Unhealthy,
    /// Loading failed. The supervisor may retry; the admin page shows why.
    Failed,
    /// Intentionally stopped (configuration switch or admin action).
    Stopped,
}

impl PluginState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Unhealthy => "unhealthy",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Unhealthy)
    }
}

/// What the admin API lists for one configured plugin.
///
/// `running` is kept as a convenience for existing clients, but it is derived
/// from `state` rather than asserted, so it cannot drift.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginDescriptor {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Effective permissions, with the storage baseline included.
    pub permissions: Vec<String>,
    /// Whether the host will proxy network requests for it.
    pub network: bool,
    pub running: bool,
    pub state: PluginState,
    /// Whether the configuration entry is switched on.
    pub enabled: bool,
    /// Why the plugin is not running, when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How many times the supervisor restarted it.
    pub restarts: u32,
    /// Increments once per successful start; guards against stale handles.
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// The package this entry came from, for diagnostics.
    pub package: String,
}

/// A response fetched on a plugin's behalf.
#[derive(Debug, Clone)]
pub struct ProxyResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// A plugin the configuration asked for, already inspected.
#[derive(Debug, Clone)]
pub struct PluginSpec {
    pub id: String,
    pub package: PathBuf,
    pub config: serde_json::Value,
    pub manifest: Manifest,
    /// `false` means "present in the configuration, switched off". The spec is
    /// kept so an admin can enable it later without a restart.
    pub enabled: bool,
}

impl PluginSpec {
    pub fn new(
        package: PathBuf,
        manifest: Manifest,
        config: serde_json::Value,
        enabled: bool,
    ) -> Self {
        Self {
            id: manifest.id.clone(),
            package,
            config,
            manifest,
            enabled,
        }
    }
}

/// One plugin that could not be started, for `strict` mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFailure {
    pub id: String,
    pub error: String,
}

/// Desired state. `revision` lets `restart` say "same intent, new generation".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Desired {
    Running,
    Stopped,
}

pub(crate) struct Entry {
    pub(crate) descriptor: PluginDescriptor,
    /// `None` for entries that never had a usable package (disabled or
    /// inspected-failed): they are reported, never started.
    pub(crate) spec: Option<PluginSpec>,
    pub(crate) desired: Desired,
    pub(crate) desired_revision: u64,
    pub(crate) running_revision: u64,
    pub(crate) generation: u64,
    pub(crate) attempts: u32,
    pub(crate) permanent: bool,
    pub(crate) next_attempt: Option<Instant>,
    pub(crate) health_failures: u32,
    pub(crate) unhealthy_restarts: u32,
    pub(crate) last_probe: Option<Instant>,
    pub(crate) runtime: Option<Arc<PluginRuntime>>,
    pub(crate) gate: Option<Arc<Gate>>,
}

#[derive(Default)]
pub(crate) struct Inner {
    pub(crate) entries: BTreeMap<String, Entry>,
}

/// State shared by the read API and the supervisor thread.
pub(crate) struct Shared {
    pub(crate) inner: RwLock<Inner>,
    pub(crate) audit: Option<AuditLog>,
    pub(crate) grants: Option<GrantStore>,
    /// Base URL the plugin uses to call the host broker. `None` keeps the
    /// phase-1 behaviour (no broker calls).
    pub(crate) host_url: RwLock<Option<String>>,
    /// Fixed-window call counts for capabilities that declare a rate.
    pub(crate) rates: Mutex<HashMap<String, RateWindow>>,
    /// `<data>/plugins`; each generation installs under `<id>/<version>`.
    pub(crate) install_root: PathBuf,
    pub(crate) stop_timeout: Duration,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RateWindow {
    minute: i64,
    count: u32,
}

impl Shared {
    pub(crate) fn allow_rate(&self, key: &str, limit: u32) -> bool {
        let minute = now_secs() / 60;
        let mut rates = self
            .rates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let window = rates.entry(key.to_string()).or_default();
        if window.minute != minute {
            window.minute = minute;
            window.count = 0;
        }
        if window.count >= limit {
            return false;
        }
        window.count += 1;
        true
    }
}

impl Shared {
    pub(crate) fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn record_audit(&self, id: &str, action: &str, detail: &str) {
        if let Some(audit) = &self.audit {
            if let Err(error) = audit.record(id, None, action, detail) {
                tracing::warn!(plugin = %id, "failed to record plugin {action}: {error}");
            }
        }
    }
}

struct Supervisor {
    tx: Sender<Intent>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Owns the configured plugins for the lifetime of the process.
pub struct PluginHost {
    shared: Arc<Shared>,
    supervisor: Mutex<Option<Supervisor>>,
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::build(None, None, PathBuf::from("plugins"), Duration::from_secs(5))
    }
}

impl PluginHost {
    pub fn new(db: Database) -> Self {
        Self::build(
            Some(AuditLog::new(db.clone())),
            Some(GrantStore::new(db)),
            PathBuf::from("plugins"),
            Duration::from_secs(5),
        )
    }

    /// The server's constructor: where generations install, and how long a stop
    /// waits for in-flight requests.
    pub fn with_options(db: Database, install_root: PathBuf, stop_timeout: Duration) -> Self {
        Self::build(
            Some(AuditLog::new(db.clone())),
            Some(GrantStore::new(db)),
            install_root,
            stop_timeout,
        )
    }

    fn build(
        audit: Option<AuditLog>,
        grants: Option<GrantStore>,
        install_root: PathBuf,
        stop_timeout: Duration,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                inner: RwLock::new(Inner::default()),
                audit,
                grants,
                host_url: RwLock::new(None),
                rates: Mutex::new(HashMap::new()),
                install_root,
                stop_timeout,
            }),
            supervisor: Mutex::new(None),
        }
    }

    pub fn audit(&self) -> Option<&AuditLog> {
        self.shared.audit.as_ref()
    }

    pub fn grants(&self) -> Option<&GrantStore> {
        self.shared.grants.as_ref()
    }

    /// Tell newly started plugins where the host's capability broker lives.
    ///
    /// Call this before `start` so the first plugin generation receives the
    /// variable. It can be changed later; the next generation picks it up.
    pub fn with_host_url(&self, url: impl Into<String>) {
        *self
            .shared
            .host_url
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(url.into());
    }

    /// Install the specs and start supervising them.
    ///
    /// Returns the plugins that could not be started; with `strict` mode the
    /// caller decides whether that is fatal. Every entry — started, failed or
    /// disabled — stays visible through [`Self::summaries`].
    pub fn start(&self, specs: Vec<PluginSpec>, sink: Arc<dyn ProviderSink>) -> Vec<PluginFailure> {
        if specs.is_empty() {
            return Vec::new();
        }
        // SAFETY of the never-set case: `start` runs once, before the host is
        // shared with the router.
        let shared = Arc::clone(&self.shared);
        {
            let mut inner = shared.write();
            for spec in specs {
                let enabled = spec.enabled;
                inner.entries.insert(
                    spec.id.clone(),
                    Entry {
                        descriptor: descriptor_for_spec(
                            &spec,
                            if enabled {
                                PluginState::Starting
                            } else {
                                PluginState::Disabled
                            },
                            None,
                        ),
                        desired: if enabled {
                            Desired::Running
                        } else {
                            Desired::Stopped
                        },
                        desired_revision: u64::from(enabled),
                        running_revision: 0,
                        generation: 0,
                        attempts: 0,
                        permanent: !enabled,
                        next_attempt: None,
                        health_failures: 0,
                        unhealthy_restarts: 0,
                        last_probe: None,
                        runtime: None,
                        gate: None,
                        spec: Some(spec),
                    },
                );
            }
        }
        let (tx, rx) = mpsc::channel();
        let (boot_tx, boot_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("plugin-supervisor".into())
            .spawn(move || supervisor::run(shared, sink, rx, Some(boot_tx)))
            .ok();
        *self.supervisor.lock().unwrap_or_else(|p| p.into_inner()) = Some(Supervisor {
            tx,
            thread: Mutex::new(thread),
        });
        // The supervisor reports once the first convergence pass is over, so
        // `strict` mode still fails before the server starts serving.
        boot_rx.recv().unwrap_or_default()
    }

    fn send(&self, intent: Intent) -> Result<()> {
        let guard = self.supervisor.lock().unwrap_or_else(|p| p.into_inner());
        let Some(supervisor) = guard.as_ref() else {
            return Err(KernelError::Conflict(
                "plugin supervision is not running".into(),
            ));
        };
        supervisor
            .tx
            .send(intent)
            .map_err(|_| KernelError::Conflict("the plugin supervisor stopped".into()))
    }

    /// Restart one plugin (stop, then start a new generation).
    pub fn restart(&self, id: &str) -> Result<()> {
        self.require_entry(id)?;
        self.send(Intent::Restart(id.to_string()))
    }

    /// Turn a plugin on or off at runtime.
    ///
    /// This only changes the in-memory intent; the caller decides whether to
    /// persist the switch to the configuration file.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        self.require_entry(id)?;
        self.send(Intent::SetEnabled(id.to_string(), enabled))
    }

    fn require_entry(&self, id: &str) -> Result<()> {
        if self.shared.read().entries.contains_key(id) {
            Ok(())
        } else {
            Err(KernelError::NotFound(format!("plugin '{id}'")))
        }
    }

    /// Stop every plugin and join the supervisor. Idempotent.
    pub fn shutdown(&self) {
        let supervisor = self
            .supervisor
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let Some(supervisor) = supervisor else {
            return;
        };
        let _ = supervisor.tx.send(Intent::Shutdown);
        let thread = supervisor
            .thread
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }

    /// Remember an entry that the configuration switched off.
    pub fn record_disabled(&self, package: &Path) {
        let manifest = inspect(package).ok();
        let descriptor = descriptor_for(package, manifest.as_ref(), PluginState::Disabled, None);
        self.insert(descriptor, None, None, None);
    }

    /// Remember an entry that failed to load before it reached the supervisor
    /// (an unusable package, discovered while building the spec list).
    pub fn record_failed(&self, package: &Path, manifest: Option<&Manifest>, error: &str) {
        let descriptor = descriptor_for(package, manifest, PluginState::Failed, Some(error));
        self.shared
            .record_audit(&descriptor.id, "start.failed", error);
        // Keep the inspected manifest so the admin page can still show what the
        // package asked for even though it never started. The spec is disabled:
        // it exists for disclosure, not for a retry.
        let spec = manifest.map(|manifest| {
            PluginSpec::new(
                package.to_path_buf(),
                manifest.clone(),
                serde_json::json!({}),
                false,
            )
        });
        self.insert(descriptor, spec, None, None);
    }

    fn insert(
        &self,
        descriptor: PluginDescriptor,
        spec: Option<PluginSpec>,
        runtime: Option<Arc<PluginRuntime>>,
        gate: Option<Arc<Gate>>,
    ) {
        let id = descriptor.id.clone();
        self.shared.write().entries.insert(
            id,
            Entry {
                descriptor,
                spec,
                desired: Desired::Stopped,
                desired_revision: 0,
                running_revision: 0,
                generation: 0,
                attempts: 0,
                permanent: true,
                next_attempt: None,
                health_failures: 0,
                unhealthy_restarts: 0,
                last_probe: None,
                runtime,
                gate,
            },
        );
    }

    pub fn summaries(&self) -> Vec<PluginDescriptor> {
        self.shared
            .read()
            .entries
            .values()
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    /// One descriptor, for control endpoints that report the new state.
    pub fn descriptor(&self, id: &str) -> Option<PluginDescriptor> {
        self.shared
            .read()
            .entries
            .get(id)
            .map(|entry| entry.descriptor.clone())
    }

    /// The manifest this plugin was inspected from, if the package was usable.
    pub fn manifest(&self, id: &str) -> Option<Manifest> {
        self.shared
            .read()
            .entries
            .get(id)
            .and_then(|entry| entry.spec.as_ref().map(|spec| spec.manifest.clone()))
    }

    /// Disclosed request, granted scope and the host's risk descriptions.
    pub fn capability_report(&self, id: &str) -> Option<CapabilityReport> {
        let (name, version, manifest) = {
            let inner = self.shared.read();
            let entry = inner.entries.get(id)?;
            (
                entry.descriptor.name.clone(),
                entry.descriptor.version.clone(),
                entry.spec.as_ref().map(|spec| spec.manifest.clone()),
            )
        };
        let requested = manifest
            .as_ref()
            .map(requested_capabilities)
            .unwrap_or_default();
        let granted = self
            .shared
            .grants
            .as_ref()
            .and_then(|grants| grants.list(Some(id)).ok())
            .unwrap_or_default();
        Some(CapabilityReport {
            plugin_id: id.to_string(),
            name,
            version,
            requested,
            granted,
            catalog: CAPABILITIES.to_vec(),
            forbidden: capability::forbidden(),
        })
    }

    /// The package path as written in the configuration, used to persist a
    /// `disable`/`enable` switch back into that file.
    pub fn package_of(&self, id: &str) -> Option<String> {
        self.shared
            .read()
            .entries
            .get(id)
            .map(|entry| entry.descriptor.package.clone())
    }

    /// The runtime that presents `token`, if any.
    pub fn authenticate(&self, token: &str) -> Option<Arc<PluginRuntime>> {
        self.shared
            .read()
            .entries
            .values()
            .find_map(|entry| match &entry.runtime {
                Some(runtime) if runtime.token() == token => Some(Arc::clone(runtime)),
                _ => None,
            })
    }

    /// The tail of a plugin's stderr, for the admin log view.
    pub fn logs(&self, id: &str, max_bytes: usize) -> Option<String> {
        self.shared.read().entries.get(id).and_then(|entry| {
            entry
                .runtime
                .as_ref()
                .map(|runtime| runtime.stderr_text(max_bytes))
        })
    }

    /// The pid of the current generation, when one is running.
    pub fn pid(&self, id: &str) -> Option<u32> {
        self.shared
            .read()
            .entries
            .get(id)
            .and_then(|entry| entry.runtime.as_ref().map(|runtime| runtime.pid()))
    }

    /// Fetch `url` on behalf of the plugin that owns `token`.
    ///
    /// Refused — and recorded — when the plugin never declared `network`.
    pub fn proxy(&self, token: &str, url: &str) -> Result<ProxyResponse> {
        // Copy what the fetch needs and drop the lock: the request can take up
        // to 30 seconds and a restart must not wait for it.
        let (id, allowed) = {
            let inner = self.shared.read();
            let entry = inner
                .entries
                .values()
                .find(|entry| {
                    entry
                        .runtime
                        .as_ref()
                        .is_some_and(|runtime| runtime.token() == token)
                })
                .ok_or_else(|| KernelError::PermissionDenied("unknown plugin token".to_string()))?;
            (entry.descriptor.id.clone(), entry.descriptor.network)
        };

        if !allowed {
            if let Some(audit) = &self.shared.audit {
                let _ = audit.record(&id, Some(PERMISSION_NETWORK), "network.denied", url);
            }
            return Err(KernelError::PermissionDenied(format!(
                "plugin '{id}' did not declare the network permission"
            )));
        }

        if let Some(audit) = &self.shared.audit {
            let _ = audit.record(&id, Some(PERMISSION_NETWORK), "network.allowed", url);
        }
        fetch(url)
    }

    /// Host-mediated `net.fetch`: grant, scope, rate limit, SSRF, audit.
    ///
    /// The audit sink is a precondition, not a side effect: without it the
    /// call is refused before any socket is opened.
    pub fn broker_fetch(&self, token: &str, request: BrokerRequest) -> Result<BrokerResponse> {
        let runtime = self
            .authenticate(token)
            .ok_or_else(|| KernelError::PermissionDenied("unknown plugin token".to_string()))?;
        let (plugin_id, version) = {
            let manifest = runtime.manifest();
            (manifest.id.clone(), manifest.version.clone())
        };
        self.broker_fetch_for(&plugin_id, &version, request)
    }

    fn broker_fetch_for(
        &self,
        plugin_id: &str,
        version: &str,
        request: BrokerRequest,
    ) -> Result<BrokerResponse> {
        let audit = self.shared.audit.as_ref().ok_or_else(|| {
            KernelError::PermissionDenied("capability audit is unavailable".to_string())
        })?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let detail = format!(
            "{} {}",
            request.method.to_ascii_uppercase(),
            broker::redact_url(&request.url)
        );
        let started = Instant::now();

        let grant = self
            .grants()
            .map(|grants| grants.active(plugin_id, version))
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .find(|grant| grant.capability == "net.fetch");
        let Some(grant) = grant else {
            let _ = audit.record_event(&AuditEvent {
                plugin_id,
                capability: Some("net.fetch"),
                action: "capability.fetch",
                result: "denied",
                detail: Some("net.fetch is not granted for this plugin version"),
                scope_digest: None,
                bytes_in: None,
                bytes_out: None,
                duration_ms: None,
                request_id: Some(&request_id),
                job_id: None,
                user_id: None,
            });
            return Err(KernelError::PermissionDenied(
                "net.fetch is not granted for this plugin version".to_string(),
            ));
        };

        let scope: NetFetchScope =
            serde_json::from_value(grant.scope.clone()).map_err(|error| {
                KernelError::InvalidInput(format!("stored net.fetch scope is invalid: {error}"))
            })?;
        if !self
            .shared
            .allow_rate(&format!("{plugin_id}:net.fetch"), scope.rate_per_min)
        {
            let _ = audit.record_event(&AuditEvent {
                plugin_id,
                capability: Some("net.fetch"),
                action: "capability.fetch",
                result: "denied",
                detail: Some("rate limit reached"),
                scope_digest: Some(&grant.scope_digest),
                bytes_in: None,
                bytes_out: None,
                duration_ms: None,
                request_id: Some(&request_id),
                job_id: None,
                user_id: None,
            });
            return Err(KernelError::PermissionDenied(
                "net.fetch rate limit reached".to_string(),
            ));
        }

        match broker::fetch(&scope, &request) {
            Ok(response) => {
                let bytes = i64::try_from(response.body.len()).unwrap_or(i64::MAX);
                audit
                    .record_event(&AuditEvent {
                        plugin_id,
                        capability: Some("net.fetch"),
                        action: "capability.fetch",
                        result: "allowed",
                        detail: Some(&detail),
                        scope_digest: Some(&grant.scope_digest),
                        bytes_in: None,
                        bytes_out: Some(bytes),
                        duration_ms: Some(started.elapsed().as_millis() as i64),
                        request_id: Some(&request_id),
                        job_id: None,
                        user_id: None,
                    })
                    .map_err(|error| {
                        KernelError::Storage(format!(
                            "the capability audit could not be written: {error}"
                        ))
                    })?;
                Ok(response)
            }
            Err(error) => {
                let result = match error {
                    BrokerError::Denied(_) | BrokerError::Invalid(_) | BrokerError::Redirect(_) => {
                        "denied"
                    }
                    BrokerError::TooLarge(_) | BrokerError::Timeout | BrokerError::Upstream(_) => {
                        "error"
                    }
                };
                let _ = audit.record_event(&AuditEvent {
                    plugin_id,
                    capability: Some("net.fetch"),
                    action: "capability.fetch",
                    result,
                    detail: Some(&detail),
                    scope_digest: Some(&grant.scope_digest),
                    bytes_in: None,
                    bytes_out: None,
                    duration_ms: Some(started.elapsed().as_millis() as i64),
                    request_id: Some(&request_id),
                    job_id: None,
                    user_id: None,
                });
                Err(map_broker_error(error))
            }
        }
    }
}

fn map_broker_error(error: BrokerError) -> KernelError {
    match error {
        BrokerError::Invalid(message) => KernelError::InvalidInput(message),
        BrokerError::Denied(message) | BrokerError::Redirect(message) => {
            KernelError::PermissionDenied(message)
        }
        BrokerError::TooLarge(limit) => KernelError::InvalidInput(format!(
            "upstream response exceeds the {limit} byte scope limit"
        )),
        BrokerError::Timeout => {
            KernelError::StorageUnavailable("upstream request timed out".to_string())
        }
        BrokerError::Upstream(message) => KernelError::StorageUnavailable(message),
    }
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn descriptor_for_spec(
    spec: &PluginSpec,
    state: PluginState,
    error: Option<&str>,
) -> PluginDescriptor {
    PluginDescriptor {
        id: spec.manifest.id.clone(),
        name: spec.manifest.name.clone(),
        version: spec.manifest.version.clone(),
        permissions: spec.manifest.effective_permissions(),
        network: spec.manifest.has_permission(PERMISSION_NETWORK),
        running: state.is_running(),
        state,
        enabled: spec.enabled,
        error: error.map(str::to_string),
        restarts: 0,
        generation: 0,
        started_at: None,
        package: spec.package.display().to_string(),
    }
}

/// Build a descriptor for an entry that never reached a spec.
fn descriptor_for(
    package: &Path,
    manifest: Option<&Manifest>,
    state: PluginState,
    error: Option<&str>,
) -> PluginDescriptor {
    match manifest {
        Some(manifest) => PluginDescriptor {
            id: manifest.id.clone(),
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            permissions: manifest.effective_permissions(),
            network: manifest.has_permission(PERMISSION_NETWORK),
            running: state.is_running(),
            state,
            enabled: !matches!(state, PluginState::Disabled),
            error: error.map(str::to_string),
            restarts: 0,
            generation: 0,
            started_at: None,
            package: package.display().to_string(),
        },
        None => PluginDescriptor {
            // Inspection failed: the manifest id is unknown, so use the file
            // name. It is only a label; nothing routes on it.
            id: fallback_id(package),
            name: fallback_id(package),
            version: String::new(),
            permissions: Vec::new(),
            network: false,
            running: false,
            state,
            enabled: !matches!(state, PluginState::Disabled),
            error: error.map(str::to_string),
            restarts: 0,
            generation: 0,
            started_at: None,
            package: package.display().to_string(),
        },
    }
}

/// A stable, URL-safe label for a package whose manifest could not be read.
fn fallback_id(package: &Path) -> String {
    let stem = package
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "plugin".to_string());
    let cleaned: String = stem
        .chars()
        .map(|ch| {
            let ch = ch.to_ascii_lowercase();
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "plugin".to_string()
    } else {
        trimmed.chars().take(32).collect()
    }
}

/// Fetch one URL with a timeout and a size cap.
fn fetch(url: &str) -> Result<ProxyResponse> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(KernelError::InvalidInput(
            "only http(s) URLs can be proxied".to_string(),
        ));
    }
    let agent = ureq::AgentBuilder::new().timeout(PROXY_TIMEOUT).build();
    let response = match agent.get(url).call() {
        Ok(response) => response,
        // A 4xx/5xx is still a response: the plugin may want to see it.
        Err(ureq::Error::Status(_, response)) => response,
        Err(error) => {
            return Err(KernelError::Storage(format!(
                "the proxy could not reach {url}: {error}"
            )))
        }
    };
    let status = response.status();
    let content_type = response.header("content-type").map(str::to_string);
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_PROXY_BYTES)
        .read_to_end(&mut body)?;
    Ok(ProxyResponse {
        status,
        content_type,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::manifest::{Entry as ManifestEntry, PluginKind, API_VERSION};
    use localreel_kernel::db::Database;

    fn manifest(permissions: Vec<&str>) -> Manifest {
        Manifest {
            id: "demo".into(),
            name: "Demo".into(),
            version: "0.1.0".into(),
            api_version: API_VERSION.into(),
            kind: PluginKind::Storage,
            entry: ManifestEntry {
                program: "plugin".into(),
                args: Vec::new(),
                runtime: None,
            },
            permissions: permissions.into_iter().map(str::to_string).collect(),
            capabilities: Vec::new(),
            description: None,
        }
    }

    fn package() -> PathBuf {
        PathBuf::from("plugins/demo.lrplug")
    }

    #[test]
    fn a_failed_entry_is_reported_with_its_reason() {
        let host = PluginHost::default();
        let manifest = manifest(vec!["network"]);
        host.record_failed(&package(), Some(&manifest), "handshake timed out");
        let summaries = host.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "demo");
        assert_eq!(summaries[0].state, PluginState::Failed);
        assert!(!summaries[0].running);
        assert_eq!(summaries[0].error.as_deref(), Some("handshake timed out"));
        // The effective permissions are still reported: the entry exists.
        assert_eq!(
            summaries[0].permissions,
            vec!["storage:read".to_string(), "network".to_string()]
        );
    }

    #[test]
    fn a_failed_entry_without_a_manifest_gets_a_stable_id() {
        let host = PluginHost::default();
        host.record_failed(
            &PathBuf::from("plugins/WebDAV Storage.lrplug"),
            None,
            "missing manifest.json",
        );
        let summaries = host.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "webdav-storage");
        assert_eq!(summaries[0].state, PluginState::Failed);
    }

    #[test]
    fn a_disabled_entry_is_reported_as_disabled_not_missing() {
        let host = PluginHost::default();
        host.record_disabled(&package());
        let summaries = host.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].state, PluginState::Disabled);
        assert!(!summaries[0].enabled);
        assert!(!summaries[0].running);
    }

    #[test]
    fn an_unknown_token_is_refused() {
        let host = PluginHost::default();
        let error = host.proxy("nope", "https://example.com").unwrap_err();
        assert!(matches!(error, KernelError::PermissionDenied(_)));
        assert!(host.authenticate("nope").is_none());
    }

    #[test]
    fn controlling_an_unknown_plugin_is_not_found() {
        let host = PluginHost::default();
        assert!(matches!(
            host.restart("nope").unwrap_err(),
            KernelError::NotFound(_)
        ));
        assert!(matches!(
            host.set_enabled("nope", false).unwrap_err(),
            KernelError::NotFound(_)
        ));
    }

    #[test]
    fn broker_fetch_refuses_without_an_active_grant_and_audits_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("broker.db").to_str().unwrap(), 1).unwrap();
        localreel_kernel::migrations::apply(&db, "plugin", crate::audit::migrations()).unwrap();
        let host = PluginHost::new(db);

        // No grant yet: the request is refused before DNS or a socket.
        let error = host
            .broker_fetch_for(
                "demo",
                "0.1.0",
                BrokerRequest::get("https://evil.example/x"),
            )
            .unwrap_err();
        assert!(matches!(error, KernelError::PermissionDenied(_)));

        // A grant for another host still refuses the out-of-scope request.
        host.grants()
            .unwrap()
            .grant(
                "demo",
                "0.1.0",
                "net.fetch",
                serde_json::json!({"hosts": ["api.example.com"]}),
                None,
                None,
            )
            .unwrap();
        let error = host
            .broker_fetch_for(
                "demo",
                "0.1.0",
                BrokerRequest::get("https://evil.example/x"),
            )
            .unwrap_err();
        assert!(matches!(error, KernelError::PermissionDenied(_)));

        // Revocation is effective on the next call; the allowed host is not
        // contacted because the grant is gone.
        host.grants()
            .unwrap()
            .revoke("demo", "0.1.0", "net.fetch")
            .unwrap();
        let error = host
            .broker_fetch_for(
                "demo",
                "0.1.0",
                BrokerRequest::get("https://api.example.com/x"),
            )
            .unwrap_err();
        assert!(matches!(error, KernelError::PermissionDenied(_)));

        let entries = host.audit().unwrap().recent(Some("demo"), 10).unwrap();
        assert!(entries.iter().any(|entry| {
            entry.capability.as_deref() == Some("net.fetch")
                && entry.result.as_deref() == Some("denied")
        }));
        assert!(entries.len() >= 3);
    }

    #[test]
    fn states_serialize_as_snake_case() {
        let json = serde_json::to_string(&PluginState::Failed).unwrap();
        assert_eq!(json, "\"failed\"");
        let json = serde_json::to_string(&PluginState::Unhealthy).unwrap();
        assert_eq!(json, "\"unhealthy\"");
    }

    #[test]
    fn capability_report_discloses_requests_and_grants() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("host.db").to_str().unwrap(), 1).unwrap();
        localreel_kernel::migrations::apply(&db, "plugin", crate::audit::migrations()).unwrap();
        let host = PluginHost::new(db);

        let mut manifest = manifest(vec!["network"]);
        manifest.capabilities = vec![crate::capability::CapabilityRequest {
            name: "net.fetch".into(),
            scope: serde_json::json!({"hosts": ["api.example.com"]}),
        }];
        host.record_failed(&package(), Some(&manifest), "handshake timed out");

        let report = host.capability_report("demo").unwrap();
        assert!(report
            .requested
            .iter()
            .any(|request| request.name == "net.fetch" && request.source == "manifest"));
        assert!(report
            .requested
            .iter()
            .any(|request| request.name == "fs.read" && request.source == "legacy"));
        assert!(report.granted.is_empty());
        assert_eq!(report.catalog.len(), crate::capability::CAPABILITIES.len());
        assert!(!report.forbidden.is_empty());

        host.grants()
            .unwrap()
            .grant(
                "demo",
                "0.1.0",
                "net.fetch",
                serde_json::json!({"hosts": ["api.example.com"]}),
                Some(1),
                None,
            )
            .unwrap();
        let report = host.capability_report("demo").unwrap();
        assert_eq!(report.granted.len(), 1);
        assert_eq!(report.granted[0].capability, "net.fetch");
    }
}
