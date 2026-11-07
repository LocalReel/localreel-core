//! Sidecar process runtime.
//!
//! The host starts the plugin's entry program, waits for one `ready` line on
//! stdout, then talks to the plugin over loopback HTTP. Everything is bounded:
//! a handshake timeout, a token that the plugin must echo, and a stderr tail kept
//! for diagnostics. Dropping the runtime kills the child.

use std::io::{BufRead, BufReader, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;

use crate::http;
use crate::manifest::{Manifest, PluginError};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const STDERR_TAIL_BYTES: usize = 8 * 1024;

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
    stderr: Arc<Mutex<Vec<u8>>>,
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
];

/// What the host gives a plugin besides its own program.
#[derive(Debug, Clone, Default)]
pub struct PluginSandbox {
    /// Working directory, normally `<data>/plugins/<id>/`.
    pub working_dir: Option<std::path::PathBuf>,
    /// Source roots this plugin is responsible for, as URIs.
    pub roots: Vec<String>,
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
    command.args(&manifest.entry.args).env_clear();
    for key in INHERITED_ENV {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("LOCALREEL_PLUGIN_ID", &manifest.id)
        .env("LOCALREEL_PLUGIN_TOKEN", token)
        .env("LOCALREEL_PLUGIN_CONFIG", config)
        .env(
            "LOCALREEL_PLUGIN_ROOTS",
            serde_json::to_string(&sandbox.roots).unwrap_or_else(|_| "[]".to_string()),
        );
    if let Some(dir) = &sandbox.working_dir {
        command.current_dir(dir);
    }
    command
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
        let token = uuid::Uuid::new_v4().to_string();

        let mut command = sandboxed_command(program, manifest, &token, &config, sandbox);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|error| PluginError::NotReady(format!("cannot start plugin: {error}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PluginError::NotReady("plugin stdout is unavailable".into()))?;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = Arc::clone(&stderr);
            std::thread::spawn(move || {
                let mut buffer = [0u8; 1024];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let mut sink = sink.lock().unwrap_or_else(|p| p.into_inner());
                            sink.extend_from_slice(&buffer[..read]);
                            if sink.len() > STDERR_TAIL_BYTES {
                                let excess = sink.len() - STDERR_TAIL_BYTES;
                                sink.drain(..excess);
                            }
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
                kill(&mut child);
                return Err(PluginError::NotReady(
                    "plugin did not print a ready line in time".into(),
                ));
            }
        };
        let ready: Ready = serde_json::from_str(ready_line.trim()).map_err(|error| {
            kill(&mut child);
            let tail = stderr
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            PluginError::NotReady(format!(
                "invalid ready line {ready_line:?}: {error}; stderr: {}",
                String::from_utf8_lossy(&tail).trim()
            ))
        })?;
        if !ready.ready || ready.port == 0 {
            kill(&mut child);
            return Err(PluginError::NotReady("plugin reported not ready".into()));
        }
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), ready.port);
        let runtime = Self {
            manifest: manifest.clone(),
            address,
            token,
            child: Mutex::new(child),
            stderr,
        };
        runtime.wait_until_healthy()?;
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

    pub fn request(&self, path: &str) -> Result<http::Response, PluginError> {
        http::get(
            self.address,
            &self.token,
            path,
            CONNECT_TIMEOUT,
            READ_TIMEOUT,
        )
    }

    fn wait_until_healthy(&self) -> Result<(), PluginError> {
        let mut last_error = String::new();
        for _ in 0..20 {
            match self.request("/v1/health") {
                Ok(response) if response.status == 200 => return Ok(()),
                Ok(response) => last_error = format!("health returned {}", response.status),
                Err(error) => last_error = error.to_string(),
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(PluginError::NotReady(format!(
            "{last_error}; stderr: {}",
            self.stderr_tail()
        )))
    }

    fn stderr_tail(&self) -> String {
        let tail = self.stderr.lock().unwrap_or_else(|p| p.into_inner());
        String::from_utf8_lossy(&tail).trim().to_string()
    }
}

impl Drop for PluginRuntime {
    fn drop(&mut self) {
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        kill(&mut child);
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
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
            },
            permissions: vec![],
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
}
