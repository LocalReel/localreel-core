//! Platform-standard directories for application data, configuration and logs.
//!
//! LocalReel must never resolve persistent state relative to the current
//! working directory: a shortcut, a service manager and a shell all start the
//! same binary from different directories, and an install directory can be
//! moved or removed while the library is expected to survive.
//!
//! The rules implemented here follow each platform's convention:
//!
//! | Platform | Data | Config | Logs |
//! |---|---|---|---|
//! | Windows | `%LOCALAPPDATA%\LocalReel` | `%APPDATA%\LocalReel` | `<data>\logs` |
//! | macOS | `~/Library/Application Support/LocalReel` | same | `~/Library/Logs/LocalReel` |
//! | Linux/BSD | `$XDG_DATA_HOME/localreel` | `$XDG_CONFIG_HOME/localreel` | `$XDG_STATE_HOME/localreel/log` |
//!
//! Everything here is pure: [`Env`] carries the environment and [`Dirs::resolve`]
//! turns it into paths, so the rules are testable without touching the real
//! process environment.

use std::path::{Path, PathBuf};

/// Application directory name used on every platform.
pub const APP_DIR: &str = "LocalReel";

/// Platform family; only the families LocalReel ships for are modelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Unix,
}

impl Os {
    /// The platform this binary was compiled for.
    pub const fn current() -> Self {
        #[cfg(target_os = "windows")]
        {
            Os::Windows
        }
        #[cfg(target_os = "macos")]
        {
            Os::MacOs
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            Os::Unix
        }
    }
}

/// The environment inputs directory resolution depends on.
///
/// Keeping them in a struct keeps [`Dirs::resolve`] pure and testable.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub local_appdata: Option<PathBuf>,
    pub appdata: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_state_home: Option<PathBuf>,
}

impl Env {
    /// Read the real process environment.
    pub fn from_process() -> Self {
        Self {
            home: env_path("HOME").or_else(|| env_path("USERPROFILE")),
            local_appdata: env_path("LOCALAPPDATA"),
            appdata: env_path("APPDATA"),
            xdg_data_home: env_path("XDG_DATA_HOME"),
            xdg_config_home: env_path("XDG_CONFIG_HOME"),
            xdg_state_home: env_path("XDG_STATE_HOME"),
        }
    }
}

/// Resolved application directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub data: PathBuf,
    pub config: PathBuf,
    pub log: PathBuf,
}

impl Dirs {
    /// Resolve the platform directories, or `None` when the environment does
    /// not name a home/anchor directory to start from.
    pub fn resolve(os: Os, env: &Env) -> Option<Self> {
        match os {
            Os::Windows => {
                let local = env.local_appdata.clone().or_else(|| {
                    env.home
                        .clone()
                        .map(|home| home.join("AppData").join("Local"))
                })?;
                let roaming = env.appdata.clone().or_else(|| {
                    env.home
                        .clone()
                        .map(|home| home.join("AppData").join("Roaming"))
                })?;
                let data = local.join(APP_DIR);
                Some(Self {
                    log: data.join("logs"),
                    data,
                    config: roaming.join(APP_DIR),
                })
            }
            Os::MacOs => {
                let home = env.home.clone()?;
                let support = home
                    .join("Library")
                    .join("Application Support")
                    .join(APP_DIR);
                Some(Self {
                    data: support.clone(),
                    config: support,
                    log: home.join("Library").join("Logs").join(APP_DIR),
                })
            }
            Os::Unix => {
                let home = env.home.clone()?;
                let data = env
                    .xdg_data_home
                    .clone()
                    .unwrap_or_else(|| home.join(".local").join("share"))
                    .join("localreel");
                let config = env
                    .xdg_config_home
                    .clone()
                    .unwrap_or_else(|| home.join(".config"))
                    .join("localreel");
                let log = env
                    .xdg_state_home
                    .clone()
                    .unwrap_or_else(|| home.join(".local").join("state"))
                    .join("localreel")
                    .join("log");
                Some(Self { data, config, log })
            }
        }
    }
}

/// Directory of the running executable, used only to adopt legacy layouts.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// Current working directory, used only to adopt legacy layouts.
pub fn current_dir() -> Option<PathBuf> {
    std::env::current_dir().ok()
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|value| !value.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            home: Some(PathBuf::from("C:/Users/Ada")),
            local_appdata: Some(PathBuf::from("C:/Users/Ada/AppData/Local")),
            appdata: Some(PathBuf::from("C:/Users/Ada/AppData/Roaming")),
            ..Env::default()
        }
    }

    #[test]
    fn windows_uses_local_and_roaming_appdata() {
        let dirs = Dirs::resolve(Os::Windows, &env()).unwrap();
        assert_eq!(
            dirs.data,
            PathBuf::from("C:/Users/Ada/AppData/Local/LocalReel")
        );
        assert_eq!(
            dirs.config,
            PathBuf::from("C:/Users/Ada/AppData/Roaming/LocalReel")
        );
        assert_eq!(
            dirs.log,
            PathBuf::from("C:/Users/Ada/AppData/Local/LocalReel/logs")
        );
    }

    #[test]
    fn windows_falls_back_to_home_paths() {
        let bare = Env {
            home: Some(PathBuf::from("C:/Users/Ada")),
            ..Env::default()
        };
        let dirs = Dirs::resolve(Os::Windows, &bare).unwrap();
        assert_eq!(
            dirs.data,
            PathBuf::from("C:/Users/Ada/AppData/Local/LocalReel")
        );
    }

    #[test]
    fn macos_uses_library_directories() {
        let e = Env {
            home: Some(PathBuf::from("/Users/ada")),
            ..Env::default()
        };
        let dirs = Dirs::resolve(Os::MacOs, &e).unwrap();
        assert_eq!(
            dirs.data,
            PathBuf::from("/Users/ada/Library/Application Support/LocalReel")
        );
        assert_eq!(dirs.log, PathBuf::from("/Users/ada/Library/Logs/LocalReel"));
    }

    #[test]
    fn linux_honours_xdg_overrides() {
        let e = Env {
            home: Some(PathBuf::from("/home/ada")),
            xdg_data_home: Some(PathBuf::from("/srv/data")),
            xdg_config_home: Some(PathBuf::from("/srv/config")),
            xdg_state_home: Some(PathBuf::from("/srv/state")),
            ..Env::default()
        };
        let dirs = Dirs::resolve(Os::Unix, &e).unwrap();
        assert_eq!(dirs.data, PathBuf::from("/srv/data/localreel"));
        assert_eq!(dirs.config, PathBuf::from("/srv/config/localreel"));
        assert_eq!(dirs.log, PathBuf::from("/srv/state/localreel/log"));
    }

    #[test]
    fn linux_falls_back_to_xdg_defaults() {
        let e = Env {
            home: Some(PathBuf::from("/home/ada")),
            ..Env::default()
        };
        let dirs = Dirs::resolve(Os::Unix, &e).unwrap();
        assert_eq!(dirs.data, PathBuf::from("/home/ada/.local/share/localreel"));
        assert_eq!(dirs.config, PathBuf::from("/home/ada/.config/localreel"));
        assert_eq!(
            dirs.log,
            PathBuf::from("/home/ada/.local/state/localreel/log")
        );
    }

    #[test]
    fn no_home_means_no_directories() {
        assert!(Dirs::resolve(Os::Unix, &Env::default()).is_none());
        assert!(Dirs::resolve(Os::MacOs, &Env::default()).is_none());
    }
}
