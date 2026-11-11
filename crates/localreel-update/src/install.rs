//! Which installation this process is running from, and whether it can be
//! replaced without an administrator.
//!
//! Per-user setups live under `%LOCALAPPDATA%\Programs\LocalReel` and carry
//! `localreel-setup.exe` next to the server, so the updater can hand the new
//! installer to that copy. An MSI installation lives under `Program Files` and
//! needs elevation, which a background service must not ask for; a bare
//! executable (a zip or a build directory) has no installer to hand off to.

use std::path::{Path, PathBuf};

/// What this process is installed as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `localreel-setup.exe` next to the server, outside Program Files.
    PerUserSetup { install_dir: PathBuf },
    /// A machine-wide MSI: replacing it needs an administrator.
    SystemMsi { install_dir: PathBuf },
    /// A bare executable: zip, tarball, or a `target/debug` run.
    Portable { install_dir: PathBuf },
}

impl InstallKind {
    pub fn install_dir(&self) -> &Path {
        match self {
            InstallKind::PerUserSetup { install_dir }
            | InstallKind::SystemMsi { install_dir }
            | InstallKind::Portable { install_dir } => install_dir,
        }
    }

    /// Whether the updater may run the new installer itself.
    pub fn can_apply(&self) -> bool {
        matches!(self, InstallKind::PerUserSetup { .. })
    }

    /// A stable identifier for the API and the UI.
    pub fn kind(&self) -> &'static str {
        match self {
            InstallKind::PerUserSetup { .. } => "per_user_setup",
            InstallKind::SystemMsi { .. } => "system_msi",
            InstallKind::Portable { .. } => "portable",
        }
    }

    /// Why an installation cannot be updated in place, for the UI.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            InstallKind::PerUserSetup { .. } => None,
            InstallKind::SystemMsi { .. } => Some("system_msi"),
            InstallKind::Portable { .. } => Some("portable"),
        }
    }
}

fn setup_name() -> &'static str {
    if cfg!(windows) {
        "localreel-setup.exe"
    } else {
        "localreel-setup"
    }
}

/// Whether a path sits under a Windows `Program Files` directory.
fn under_program_files(path: &Path) -> bool {
    let text = path.to_string_lossy().to_lowercase();
    text.contains("\\program files\\") || text.contains("\\program files (x86)\\")
}

/// Decide the installation kind from the running executable's location.
pub fn detect(exe: &Path) -> InstallKind {
    let install_dir = exe
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let has_setup = install_dir.join(setup_name()).is_file();
    let system = under_program_files(&install_dir);
    if has_setup && !system {
        InstallKind::PerUserSetup { install_dir }
    } else if system {
        InstallKind::SystemMsi { install_dir }
    } else {
        InstallKind::Portable { install_dir }
    }
}

/// The command that replaces this installation, when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyPlan {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// The installer waits for this process to exit before touching the files.
    pub waits_for_exit: bool,
}

/// Build the command for `kind`, or `None` when the installation cannot update
/// itself (MSI, portable, or a platform without a setup executable).
///
/// `payload` is the verified installer downloaded for this platform. `wait`
/// asks the setup to wait until this process is gone: Windows will not replace
/// a running executable, and the server shuts itself down right after handing
/// over.
pub fn plan(kind: &InstallKind, payload: &Path, wait: bool) -> Option<ApplyPlan> {
    match kind {
        InstallKind::PerUserSetup { install_dir } => {
            let mut args = vec![
                "/S".to_string(),
                // Bring the program back after a silent install: a plain `/S`
                // install deliberately starts nothing.
                "/launch".to_string(),
                format!("--dir={}", install_dir.display()),
            ];
            if wait {
                args.push("/wait".to_string());
            }
            Some(ApplyPlan {
                program: payload.to_path_buf(),
                args,
                waits_for_exit: wait,
            })
        }
        InstallKind::SystemMsi { .. } | InstallKind::Portable { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "localreel-update-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_setup_next_to_the_server_is_a_per_user_install() {
        let dir = temp_dir("per-user");
        let exe = dir.join("localreel.exe");
        fs::write(&exe, b"stub").unwrap();
        fs::write(dir.join(setup_name()), b"stub").unwrap();
        let kind = detect(&exe);
        assert_eq!(kind.kind(), "per_user_setup");
        assert!(kind.can_apply());

        let payload = dir.join("new-setup.exe");
        let plan = plan(&kind, &payload, true).unwrap();
        assert_eq!(plan.program, payload);
        assert!(plan.waits_for_exit);
        assert_eq!(plan.args[0], "/S");
        assert!(plan.args.iter().any(|arg| arg == "/wait"));
        assert!(plan.args.iter().any(|arg| arg.starts_with("--dir=")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bare_executable_is_portable_and_cannot_apply() {
        let dir = temp_dir("portable");
        let exe = dir.join("localreel");
        fs::write(&exe, b"stub").unwrap();
        let kind = detect(&exe);
        assert_eq!(kind.kind(), "portable");
        assert!(!kind.can_apply());
        assert_eq!(kind.reason(), Some("portable"));
        assert!(plan(&kind, &dir.join("x"), true).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    // The path is a Windows one, and only Windows has a "Program Files" that
    // marks a machine-wide MSI install; on Unix the same literal is a single
    // file name and detection correctly says "portable".
    #[cfg(windows)]
    #[test]
    fn program_files_is_treated_as_an_msi() {
        let kind = detect(Path::new(r"C:\Program Files\LocalReel\localreel.exe"));
        assert_eq!(kind.kind(), "system_msi");
        assert!(!kind.can_apply());
        assert!(plan(&kind, Path::new("x"), true).is_none());
    }
}
