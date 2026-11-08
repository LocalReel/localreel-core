//! Sidecar process runtime.
//!
//! The host starts the plugin's entry program, waits for one `ready` line on
//! stdout, then talks to the plugin over loopback HTTP. Everything is bounded:
//! a handshake timeout, a token that the plugin must echo, and a stderr tail kept
//! for diagnostics. Dropping the runtime kills the child.
//!
//! The runtime is also where "the child must not outlive the host" is enforced:
//! [`crate::platform`] attaches the process tree to the host's lifecycle before
//! the handshake, so a host crash cannot leave an orphan holding a port.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

use crate::http;
use crate::manifest::{Manifest, PluginError};
use crate::platform;
use crate::stdio::{BrokerProxy, StdioTransport};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
/// How much of the plugin's stderr the host keeps. Older bytes are dropped from
/// the front, so the tail an operator needs for a crash is always present.
const STDERR_RING_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
struct Ready {
    ready: bool,
    port: u16,
}

pub struct PluginRuntime {
    manifest: Manifest,
    address: SocketAddr,
    token: String,
    child: Mutex<Child>,
    stderr: Arc<Mutex<StderrRing>>,
    guard: platform::Guard,
    /// Present for `entry.transport = "stdio"`; the control channel is pipes,
    /// not a listening socket.
    stdio: Option<Arc<StdioTransport>>,
}

/// Variables a plugin may inherit.
///
/// Everything else stays behind. `LOCALREEL_DATA_DIR`, `LOCALREEL_CONFIG` and
/// `LOCALREEL_LOG_DIR` point straight at the host's database and configuration:
/// a plugin that can read them does not need a permission to reach the library,
/// which would make the whole permission model decorative.
const INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "LANG",
    "LC_ALL",
    "TEMP",
    "TMP",
    // Windows needs these to load some runtimes at all.
    "SYSTEMROOT",
    "WINDIR",
    "USERNAME",
    "USERDOMAIN",
    "LOCALAPPDATA",
    "APPDATA",
    "ALLUSERSPROFILE",
    "PROGRAMDATA",
    "COMPUTERNAME",
    "PROCESSOR_ARCHITECTURE",
    "NUMBER_OF_PROCESSORS",
    "OS",
];

/// What the host gives a plugin besides its own program.
#[derive(Debug, Clone, Default)]
pub struct PluginSandbox {
    /// Working directory, normally `<data>/plugins/<id>/`.
    pub working_dir: Option<std::path::PathBuf>,
    /// Source roots this plugin is responsible for, as URIs.
    pub roots: Vec<String>,
    /// Base URL of the host, for plugins that need to call the capability
    /// broker. Loopback in phase 2; phase 3 replaces it with a non-TCP channel.
    pub host_url: Option<String>,
    /// Resource ceilings applied to the plugin process tree.
    pub limits: crate::limits::PluginLimits,
    /// Windows-only: run the plugin inside an AppContainer with no network
    /// capabilities. Requires `sandbox_program` (the `plugin-sandbox` wrapper).
    pub app_container: bool,
    /// Path to the `plugin-sandbox` wrapper; defaults to a sibling executable.
    pub sandbox_program: Option<std::path::PathBuf>,
}

/// A bounded tail of the plugin's stderr.
#[derive(Default)]
struct StderrRing {
    bytes: VecDeque<u8>,
}

impl StderrRing {
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data.iter().copied());
        while self.bytes.len() > STDERR_RING_BYTES {
            self.bytes.pop_front();
        }
    }

    fn text(&self, max_bytes: usize) -> String {
        let mut bytes: Vec<u8> = self.bytes.iter().copied().collect();
        if bytes.len() > max_bytes {
            bytes.drain(..bytes.len() - max_bytes);
        }
        String::from_utf8_lossy(&bytes).trim().to_string()
    }
}

/// Build the command that starts a plugin.
///
/// A manifest may name an interpreter (`entry.runtime`); the entry program is
/// then passed to it as the first argument. Without a runtime the program runs
/// directly, which is the precompiled-binary case.
fn launch_command(program: &Path, manifest: &Manifest) -> Command {
    if let Some(runtime) = manifest.entry.runtime.as_deref() {
        let mut command = Command::new(runtime);
        command.arg(program);
        command
    } else {
        Command::new(program)
    }
}

/// The command, with a cleared environment and only the whitelist injected.
///
/// `env_clear` is the whole point: `Command` otherwise inherits the host's
/// environment, and the host's environment names its database.
fn sandboxed_command(
    program: &Path,
    manifest: &Manifest,
    token: &str,
    config: &str,
    sandbox: &PluginSandbox,
) -> Command {
    let mut command = launch_command(program, manifest);
    command.args(&manifest.entry.args);
    if let Some(dir) = &sandbox.working_dir {
        command.current_dir(dir);
    }
    apply_plugin_env(command, manifest, token, config, sandbox)
}

fn apply_plugin_env(
    mut command: Command,
    manifest: &Manifest,
    token: &str,
    config: &str,
    sandbox: &PluginSandbox,
) -> Command {
    command.env_clear();
    for key in INHERITED_ENV {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("LOCALREEL_PLUGIN_ID", &manifest.id)
        .env("LOCALREEL_PLUGIN_TOKEN", token)
        .env("LOCALREEL_PLUGIN_TRANSPORT", &manifest.entry.transport)
        .env("LOCALREEL_PLUGIN_BROKER", &manifest.entry.transport)
        .env("LOCALREEL_PLUGIN_CONFIG", config)
        .env(
            "LOCALREEL_PLUGIN_ROOTS",
            serde_json::to_string(&sandbox.roots).unwrap_or_else(|_| "[]".to_string()),
        );
    if let Some(url) = &sandbox.host_url {
        command.env("LOCALREEL_PLUGIN_HOST_URL", url);
    }
    command
}

fn appcontainer_command(
    program: &Path,
    manifest: &Manifest,
    token: &str,
    config: &str,
    sandbox: &PluginSandbox,
) -> Result<Command, PluginError> {
    if manifest.entry.runtime.is_some() {
        return Err(PluginError::NotReady(
            "AppContainer plugins must be native binaries, not interpreter scripts".into(),
        ));
    }
    let wrapper = sandbox.sandbox_program.clone().or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("plugin-sandbox.exe")))
    });
    let wrapper = wrapper
        .ok_or_else(|| PluginError::NotReady("the plugin-sandbox wrapper is unavailable".into()))?;
    if !wrapper.is_file() {
        return Err(PluginError::NotReady(format!(
            "the plugin-sandbox wrapper {} is missing",
            wrapper.display()
        )));
    }
    let source = sandbox
        .working_dir
        .clone()
        .or_else(|| program.parent().map(Path::to_path_buf))
        .ok_or_else(|| PluginError::NotReady("plugin source directory is unknown".into()))?;
    let profile: String = manifest
        .id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let mut command = Command::new(wrapper);
    command
        .arg("--profile")
        .arg(format!("localreel-{profile}"))
        .arg("--source")
        .arg(&source)
        .arg("--program")
        .arg(program)
        .arg("--")
        .args(&manifest.entry.args);
    Ok(apply_plugin_env(command, manifest, token, config, sandbox))
}

impl PluginRuntime {
    /// Start `program` with the manifest's args and the runtime environment.
    pub fn start(
        program: &Path,
        manifest: &Manifest,
        config: &serde_json::Value,
        sandbox: &PluginSandbox,
    ) -> Result<Self, PluginError> {
        manifest.validate()?;
        if !program.is_file() {
            return Err(PluginError::Missing(format!(
                "entry program {}",
                program.display()
            )));
        }
        let config = serde_json::to_string(config)
            .map_err(|error| PluginError::Manifest(format!("invalid plugin config: {error}")))?;
        if config.len() > 32 * 1024 {
            return Err(PluginError::Manifest("plugin config is too large".into()));
        }
        if sandbox.app_container && manifest.entry.transport != "stdio" {
            return Err(PluginError::NotReady(
                "AppContainer isolation requires the stdio transport".into(),
            ));
        }
        let token = uuid::Uuid::new_v4().to_string();
        if manifest.entry.transport == "stdio" {
            return Self::start_stdio(program, manifest, &config, sandbox, token);
        }

        let mut command = sandboxed_command(program, manifest, &token, &config, sandbox);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        platform::configure(&mut command, sandbox.limits);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|error| PluginError::NotReady(format!("cannot start plugin: {error}")))?;
        // Attach the tree before the plugin can fork helpers of its own.
        let guard = platform::attach(&mut child, sandbox.limits);
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PluginError::NotReady("plugin stdout is unavailable".into()))?;
        let stderr = Arc::new(Mutex::new(StderrRing::default()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut buffer = [0u8; 1024];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let mut sink = sink.lock().unwrap_or_else(|p| p.into_inner());
                            sink.push(&buffer[..read]);
                        }
                    }
                }
            });
        }

        let (sender, receiver) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut first = String::new();
            if reader.read_line(&mut first).is_ok() {
                let _ = sender.send(first);
            }
            // Keep draining so the plugin never blocks on a full stdout pipe.
            let mut rest = String::new();
            loop {
                rest.clear();
                match reader.read_line(&mut rest) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });

        let ready_line = match receiver.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(line) => line,
            Err(_) => {
                platform::terminate(&mut child, &guard);
                return Err(PluginError::NotReady(
                    "plugin did not print a ready line in time".into(),
                ));
            }
        };
        let ready: Ready = serde_json::from_str(ready_line.trim()).map_err(|error| {
            platform::terminate(&mut child, &guard);
            let tail = stderr
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .text(8 * 1024);
            PluginError::NotReady(format!(
                "invalid ready line {ready_line:?}: {error}; stderr: {tail}"
            ))
        })?;
        if !ready.ready || ready.port == 0 {
            platform::terminate(&mut child, &guard);
            return Err(PluginError::NotReady("plugin reported not ready".into()));
        }
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ready.port);
        let runtime = Self {
            manifest: manifest.clone(),
            address,
            token,
            child: Mutex::new(child),
            stderr,
            guard,
            stdio: None,
        };
        runtime.wait_until_healthy()?;
        Ok(runtime)
    }

    fn start_stdio(
        program: &Path,
        manifest: &Manifest,
        config: &str,
        sandbox: &PluginSandbox,
        token: String,
    ) -> Result<Self, PluginError> {
        let mut command = if sandbox.app_container {
            appcontainer_command(program, manifest, &token, config, sandbox)?
        } else {
            sandboxed_command(program, manifest, &token, config, sandbox)
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        platform::configure(&mut command, sandbox.limits);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|error| PluginError::NotReady(format!("cannot start plugin: {error}")))?;
        let guard = platform::attach(&mut child, sandbox.limits);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PluginError::NotReady("plugin stdin is unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PluginError::NotReady("plugin stdout is unavailable".into()))?;
        let stderr = Arc::new(Mutex::new(StderrRing::default()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut buffer = [0u8; 1024];
                while let Ok(read) = pipe.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    sink.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(&buffer[..read]);
                }
            });
        }
        let broker = sandbox
            .host_url
            .as_ref()
            .map(|url| BrokerProxy::new(url.clone(), token.clone()));
        let transport = Arc::new(StdioTransport::new(stdin, stdout, token.clone(), broker));
        let runtime = Self {
            manifest: manifest.clone(),
            address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            token,
            child: Mutex::new(child),
            stderr,
            guard,
            stdio: Some(transport),
        };
        if let Err(error) = runtime.wait_until_healthy() {
            let mut child = runtime
                .child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            platform::terminate(&mut child, &runtime.guard);
            return Err(error);
        }
        Ok(runtime)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The token the plugin must present to the host (proxy, callbacks).
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn pid(&self) -> u32 {
        self.child.lock().unwrap_or_else(|p| p.into_inner()).id()
    }

    pub fn request(&self, path: &str) -> Result<http::Response, PluginError> {
        if self.stdio.is_some() {
            return Err(PluginError::Request(
                "stdio plugins do not expose HTTP paths; use stdio_call".to_string(),
            ));
        }
        http::get(
            self.address,
            &self.token,
            path,
            CONNECT_TIMEOUT,
            READ_TIMEOUT,
        )
    }

    pub fn is_stdio(&self) -> bool {
        self.stdio.is_some()
    }

    /// One JSON-RPC call over the stdio control channel.
    pub fn stdio_call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        match &self.stdio {
            Some(stdio) => stdio.call(method, params),
            None => Err(PluginError::Request(
                "plugin is not using the stdio transport".to_string(),
            )),
        }
    }

    /// One scheduled callback, over HTTP or the stdio pipe.
    pub fn schedule(&self, payload: &str) -> Result<(), PluginError> {
        if self.stdio.is_some() {
            self.stdio_call("schedule", serde_json::json!({ "payload": payload }))?;
            return Ok(());
        }
        let path = format!("/v1/schedule?payload={}", http::encode(payload));
        match self.request(&path) {
            Ok(response) if response.status == 200 => Ok(()),
            Ok(response) => Err(PluginError::Request(format!(
                "plugin answered HTTP {}",
                response.status
            ))),
            Err(error) => Err(error),
        }
    }

    /// One health check with its own short budget.
    pub fn probe(&self, timeout: Duration) -> bool {
        if let Some(stdio) = &self.stdio {
            return stdio.health().is_ok();
        }
        matches!(
            http::get(self.address, &self.token, "/v1/health", CONNECT_TIMEOUT, timeout),
            Ok(response) if response.status == 200
        )
    }

    /// Close the child's stdin. Official plugins treat EOF as "shut down now".
    pub fn close_stdin(&self) {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        drop(child.stdin.take());
    }

    /// Has the process exited? Non-blocking; used by the supervisor.
    pub fn poll_exit(&self) -> Option<ExitStatus> {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        child.try_wait().ok().flatten()
    }

    /// Kill the process tree now, without waiting for a graceful exit.
    pub fn terminate(&self) {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        platform::terminate(&mut child, &self.guard);
        let _ = child.wait();
    }

    /// The tail of the plugin's stderr, at most `max_bytes` bytes.
    pub fn stderr_text(&self, max_bytes: usize) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .text(max_bytes)
    }

    fn wait_until_healthy(&self) -> Result<(), PluginError> {
        let mut last_error = String::new();
        // Bounded in wall-clock time, not in attempts: a plugin that accepts
        // the connection but never answers would otherwise burn one full read
        // timeout per attempt (see ADR-015's follow-up).
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            if self.probe(Duration::from_secs(2)) {
                return Ok(());
            }
            last_error = "health did not return 200 within 2s".to_string();
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(PluginError::NotReady(format!(
            "{last_error}; stderr: {}",
            self.stderr_text(8 * 1024)
        )))
    }
}

impl Drop for PluginRuntime {
    fn drop(&mut self) {
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        platform::terminate(&mut child, &self.guard);
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Entry, Manifest, PluginKind, API_VERSION};

    fn manifest(runtime: Option<&str>) -> Manifest {
        Manifest {
            id: "example".into(),
            name: "Example".into(),
            version: "0.1.0".into(),
            api_version: API_VERSION.into(),
            kind: PluginKind::Storage,
            entry: Entry {
                program: "plugin.js".into(),
                args: vec!["--flag".into()],
                runtime: runtime.map(str::to_string),
                transport: "http".into(),
            },
            permissions: vec![],
            capabilities: vec![],
            description: None,
        }
    }

    #[test]
    fn an_interpreter_receives_the_program() {
        let command = launch_command(Path::new("pkg/plugin.js"), &manifest(Some("node")));
        assert_eq!(command.get_program(), "node");
        let args: Vec<_> = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["pkg/plugin.js"]);
    }

    #[test]
    fn a_binary_runs_directly() {
        let command = launch_command(Path::new("pkg/plugin"), &manifest(None));
        assert_eq!(command.get_program(), "pkg/plugin");
        assert_eq!(command.get_args().count(), 0);
    }

    #[test]
    fn the_plugin_environment_is_only_the_whitelist() {
        let sandbox = PluginSandbox {
            working_dir: Some(std::path::PathBuf::from("data/plugins/example")),
            roots: vec!["https://nas/dav".to_string()],
            host_url: Some("http://127.0.0.1:3001".to_string()),
            limits: Default::default(),
            app_container: false,
            sandbox_program: None,
        };
        let command = sandboxed_command(
            Path::new("pkg/plugin.js"),
            &manifest(Some("node")),
            "token",
            "{}",
            &sandbox,
        );

        let keys: Vec<String> = command
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for key in &keys {
            assert!(
                INHERITED_ENV.contains(&key.as_str()) || key.starts_with("LOCALREEL_PLUGIN_"),
                "{key} must never reach a plugin"
            );
        }
        // The host's own locations are the ones that matter.
        assert!(!keys.contains(&"LOCALREEL_DATA_DIR".to_string()));
        assert!(!keys.contains(&"LOCALREEL_CONFIG".to_string()));
        assert!(!keys.contains(&"LOCALREEL_LOG_DIR".to_string()));

        // The three documented variables are still there, plus the roots.
        assert!(keys.contains(&"LOCALREEL_PLUGIN_ID".to_string()));
        assert!(keys.contains(&"LOCALREEL_PLUGIN_TOKEN".to_string()));
        assert!(keys.contains(&"LOCALREEL_PLUGIN_CONFIG".to_string()));
        assert!(keys.contains(&"LOCALREEL_PLUGIN_ROOTS".to_string()));
        let host_url = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("LOCALREEL_PLUGIN_HOST_URL"))
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(host_url.as_deref(), Some("http://127.0.0.1:3001"));

        assert_eq!(
            command.get_current_dir(),
            Some(std::path::Path::new("data/plugins/example"))
        );
    }

    #[test]
    fn roots_are_passed_as_json() {
        let sandbox = PluginSandbox {
            working_dir: None,
            roots: vec!["/library".to_string(), "https://nas/dav".to_string()],
            host_url: None,
            limits: Default::default(),
            app_container: false,
            sandbox_program: None,
        };
        let command = sandboxed_command(
            Path::new("pkg/plugin"),
            &manifest(None),
            "token",
            "{}",
            &sandbox,
        );
        let roots = command
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("LOCALREEL_PLUGIN_ROOTS"))
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
            .expect("roots are always present");
        let parsed: Vec<String> = serde_json::from_str(&roots).unwrap();
        assert_eq!(parsed, vec!["/library", "https://nas/dav"]);
    }

    #[test]
    fn the_stderr_ring_keeps_the_tail() {
        let mut ring = StderrRing::default();
        ring.push(b"first line\n");
        ring.push(&vec![b'x'; STDERR_RING_BYTES]);
        ring.push(b"\nlast line");
        let text = ring.text(64);
        assert!(text.ends_with("last line"), "{text}");
        assert!(ring.text(usize::MAX).len() <= STDERR_RING_BYTES + 16);
    }
}
