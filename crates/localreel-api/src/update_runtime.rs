//! The stateful half of the update path.
//!
//! `localreel-update` decides *what* is safe to install; this module owns the
//! one-at-a-time download, the state the console polls, and the hand-off to the
//! per-user setup. Nothing here runs unless an administrator asks for it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use localreel_update::{install, Manifest, Platform};

/// Where the release workflow publishes the signed manifest.
pub const DEFAULT_MANIFEST: &str =
    "https://github.com/LocalReel/LocalReel/releases/latest/download/update.json";

/// Both manifest files live next to the release assets, so the signature is one
/// more file whose name only the version changes.
const SIGNATURE_SUFFIX: &str = ".sig";

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Downloading,
    Ready,
    Installing,
    Failed,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Downloading => "downloading",
            Phase::Ready => "ready",
            Phase::Installing => "installing",
            Phase::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Download {
    pub phase: Phase,
    pub version: Option<String>,
    pub bytes: u64,
    pub total: u64,
    pub payload: Option<PathBuf>,
    pub error: Option<String>,
}

impl Download {
    fn idle() -> Self {
        Self {
            phase: Phase::Idle,
            version: None,
            bytes: 0,
            total: 0,
            payload: None,
            error: None,
        }
    }
}

struct Inner {
    install: install::InstallKind,
    state: Mutex<Download>,
}

/// Cheap to clone: one process-wide instance lives in `AppState` and the
/// download worker gets its own handle.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    pub fn new() -> Self {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("localreel"));
        Self {
            inner: Arc::new(Inner {
                install: install::detect(&exe),
                state: Mutex::new(Download::idle()),
            }),
        }
    }

    /// For tests and for a server that knows better than `current_exe`.
    pub fn with_install(kind: install::InstallKind) -> Self {
        Self {
            inner: Arc::new(Inner {
                install: kind,
                state: Mutex::new(Download::idle()),
            }),
        }
    }

    pub fn install(&self) -> &install::InstallKind {
        &self.inner.install
    }

    fn state(&self) -> Download {
        self.inner
            .state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_else(|_| Download::idle())
    }

    pub fn install_json(&self) -> serde_json::Value {
        let install = &self.inner.install;
        serde_json::json!({
            "kind": install.kind(),
            "can_apply": install.can_apply(),
            "reason": install.reason(),
            "install_dir": install.install_dir().display().to_string(),
        })
    }

    pub fn download_json(&self) -> serde_json::Value {
        let state = self.state();
        serde_json::json!({
            "phase": state.phase.as_str(),
            "version": state.version,
            "bytes": state.bytes,
            "total": state.total,
            "error": state.error,
            "ready": state.phase == Phase::Ready,
        })
    }

    /// Fetch, verify and download the release in the background.
    ///
    /// Returns immediately; the console polls [`Runtime::download_json`].
    pub fn start(&self, manifest_url: &str, data_dir: &Path) -> Result<(), String> {
        if !self.inner.install.can_apply() {
            return Err(match self.inner.install.reason() {
                Some(reason) => format!("this installation cannot update itself ({reason})"),
                None => "this installation cannot update itself".to_string(),
            });
        }
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| "the update state is poisoned".to_string())?;
            if state.phase == Phase::Downloading || state.phase == Phase::Installing {
                return Err("an update is already in progress".to_string());
            }
            state.phase = Phase::Downloading;
            state.bytes = 0;
            state.total = 0;
            state.payload = None;
            state.error = None;
            state.version = None;
        }

        let runtime = self.clone();
        let manifest_url = manifest_url.to_string();
        let directory = data_dir.join("updates");
        std::thread::Builder::new()
            .name("localreel-update".to_string())
            .spawn(move || {
                let result = runtime.fetch_and_download(&manifest_url, &directory);
                if let Ok(mut state) = runtime.inner.state.lock() {
                    match result {
                        Ok((version, payload)) => {
                            state.phase = Phase::Ready;
                            state.version = Some(version);
                            state.payload = Some(payload);
                            state.bytes = state.total;
                        }
                        Err(message) => {
                            state.phase = Phase::Failed;
                            state.error = Some(message);
                        }
                    }
                }
            })
            .map_err(|error| format!("cannot start the download worker: {error}"))?;
        Ok(())
    }

    fn fetch_and_download(
        &self,
        manifest_url: &str,
        directory: &Path,
    ) -> Result<(String, PathBuf), String> {
        if !manifest_url.starts_with("https://") {
            return Err(format!("the manifest url {manifest_url:?} is not https"));
        }
        let body = fetch(manifest_url)?;
        let signature_url = format!("{manifest_url}{SIGNATURE_SUFFIX}");
        let signature = fetch(&signature_url)?;
        let manifest = Manifest::load(
            body.as_bytes(),
            signature.trim(),
            localreel_update::PUBLIC_KEY_HEX,
        )
        .map_err(|error| format!("the update manifest is not trusted: {error}"))?;
        manifest
            .check_against(localreel_kernel::VERSION)
            .map_err(|error| error.to_string())?;

        let edition = localreel_update::Manifest::edition_of(self.inner.install.install_dir());
        let asset = manifest
            .asset_for_edition(Platform::current(), edition)
            .ok_or_else(|| format!("this release has no build for {}", Platform::current()))?;
        if !matches!(
            asset.kind,
            localreel_update::AssetKind::Setup | localreel_update::AssetKind::SetupDesktop
        ) {
            return Err(format!(
                "this release carries a {} for {}, not an installer; update it by hand",
                asset.kind.slug(),
                Platform::current()
            ));
        }
        if let Ok(mut state) = self.inner.state.lock() {
            state.total = asset.size;
            state.version = Some(manifest.version.clone());
        }

        let runtime = self.clone();
        let payload = localreel_update::download_with_progress(asset, directory, move |bytes| {
            if let Ok(mut state) = runtime.inner.state.lock() {
                state.bytes = bytes;
            }
        })
        .map_err(|error| error.to_string())?;
        Ok((manifest.version, payload))
    }

    /// Run the verified installer, then let the caller stop the server.
    ///
    /// The setup is spawned with `/wait`: it blocks until this process is gone
    /// before it touches the install directory, which is the only way Windows
    /// lets the running executable be replaced.
    pub fn apply(&self) -> Result<(), String> {
        self.apply_with(|plan| {
            std::process::Command::new(&plan.program)
                .args(&plan.args)
                .spawn()
                .map(|_| ())
        })
    }

    /// Same sequence, with the process launch injected.
    ///
    /// The interesting parts — refusing to run when nothing is verified, running
    /// the installer the plan describes, and moving to `Installing` so the caller
    /// knows to stop the server — are testable without actually starting an
    /// installer, which on a real release replaces the running files.
    pub fn apply_with(
        &self,
        spawn: impl FnOnce(&localreel_update::ApplyPlan) -> std::io::Result<()>,
    ) -> Result<(), String> {
        if !self.inner.install.can_apply() {
            return Err("this installation cannot update itself".to_string());
        }
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| "the update state is poisoned".to_string())?;
        if state.phase != Phase::Ready {
            return Err("no verified installer is ready".to_string());
        }
        let payload = state
            .payload
            .clone()
            .ok_or_else(|| "the installer file is missing".to_string())?;
        let plan = install::plan(&self.inner.install, &payload, true)
            .ok_or_else(|| "this installation has no installer to run".to_string())?;
        spawn(&plan)
            .map_err(|error| format!("cannot start {}: {error}", plan.program.display()))?;
        state.phase = Phase::Installing;
        Ok(())
    }
}

fn fetch(url: &str) -> Result<String, String> {
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let response = agent
        .get(url)
        .set("Accept", "text/plain, application/json")
        .set(
            "User-Agent",
            concat!("LocalReel/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|error| format!("{url}: {error}"))?;
    response
        .into_string()
        .map_err(|error| format!("{url}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_start_without_an_installer() {
        let runtime = Runtime::with_install(install::InstallKind::Portable {
            install_dir: PathBuf::from("/tmp"),
        });
        let error = runtime
            .start("https://example.invalid/update.json", Path::new("/tmp"))
            .unwrap_err();
        assert!(error.contains("cannot update itself"));
        assert_eq!(runtime.download_json()["phase"], "idle");
    }

    #[test]
    fn a_verified_installer_is_the_only_thing_that_can_be_applied() {
        let runtime = Runtime::with_install(install::InstallKind::Portable {
            install_dir: PathBuf::from("/tmp"),
        });
        assert!(runtime.apply().is_err());
    }

    #[test]
    fn reports_the_install_kind() {
        let runtime = Runtime::with_install(install::InstallKind::SystemMsi {
            install_dir: PathBuf::from(r"C:\Program Files\LocalReel"),
        });
        let json = runtime.install_json();
        assert_eq!(json["kind"], "system_msi");
        assert_eq!(json["can_apply"], false);
        assert_eq!(json["reason"], "system_msi");
    }

    /// End to end for the hand-off: a verified payload, then exactly one launch
    /// of the installer the plan describes, then `Installing` so the caller
    /// knows to stop the server.
    #[test]
    fn applying_runs_the_installer_and_moves_to_installing() {
        let dir = tempfile::tempdir().unwrap();
        let install_dir = dir.path().join("LocalReel");
        std::fs::create_dir_all(&install_dir).unwrap();
        let setup = install_dir.join("localreel-setup.exe");
        std::fs::write(&setup, b"stub").unwrap();
        let payload = dir.path().join("downloaded-setup.exe");
        std::fs::write(&payload, b"stub payload").unwrap();

        let runtime = Runtime::with_install(install::InstallKind::PerUserSetup {
            install_dir: install_dir.clone(),
        });
        {
            // Reach the state `start` would have left behind after a verified
            // download, without making a network request in a unit test.
            let mut state = runtime.inner.state.lock().unwrap();
            state.phase = Phase::Ready;
            state.payload = Some(payload.clone());
        }

        let mut launched: Option<(std::path::PathBuf, Vec<String>)> = None;
        runtime
            .apply_with(|plan| {
                launched = Some((plan.program.clone(), plan.args.clone()));
                Ok(())
            })
            .unwrap();

        let (program, args) = launched.expect("the installer must be launched once");
        // The plan runs the downloaded installer, not the one already on disk:
        // that is the whole point of downloading it first.
        assert_eq!(program, payload, "the launched program must be the payload");
        assert!(
            args.iter().any(|arg| arg == "/S"),
            "silent install: {args:?}"
        );
        assert!(
            args.iter().any(|arg| arg == "/wait"),
            "it waits for us: {args:?}"
        );
        assert!(
            args.iter().any(|arg| arg.starts_with("--dir=")),
            "the installer is told where: {args:?}"
        );
        assert_eq!(runtime.download_json()["phase"], "installing");
    }

    #[test]
    fn a_failed_launch_does_not_claim_the_update_started() {
        let dir = tempfile::tempdir().unwrap();
        let install_dir = dir.path().join("LocalReel");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(install_dir.join("localreel-setup.exe"), b"stub").unwrap();
        let payload = dir.path().join("downloaded-setup.exe");
        std::fs::write(&payload, b"stub payload").unwrap();

        let runtime = Runtime::with_install(install::InstallKind::PerUserSetup { install_dir });
        {
            let mut state = runtime.inner.state.lock().unwrap();
            state.phase = Phase::Ready;
            state.payload = Some(payload);
        }
        let error = runtime
            .apply_with(|_| Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no")))
            .unwrap_err();
        assert!(error.contains("cannot start"), "{error}");
        assert_eq!(runtime.download_json()["phase"], "ready");
    }
}
