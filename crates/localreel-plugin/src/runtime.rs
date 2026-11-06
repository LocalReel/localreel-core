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

impl PluginRuntime {
    /// Start `program` with the manifest's args and the runtime environment.
    pub fn start(
        program: &Path,
        manifest: &Manifest,
        config: &serde_json::Value,
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

        let mut command = launch_command(program, manifest);
        command
            .args(&manifest.entry.args)
            .env("LOCALREEL_PLUGIN_ID", &manifest.id)
            .env("LOCALREEL_PLUGIN_TOKEN", &token)
            .env("LOCALREEL_PLUGIN_CONFIG", &config)
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
}
