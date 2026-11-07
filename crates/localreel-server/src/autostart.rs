//! Start LocalReel with the user session.
//!
//! Each platform gets its native mechanism, because a "start at login" entry
//! that pops a console window on every login is worse than none:
//!
//! - Windows: a Task Scheduler entry running the desktop persona with the tray
//!   and without opening a browser. A scheduled task gets no console, so the
//!   tray is the only surface.
//! - macOS: a LaunchAgent running the server persona, the same shape as the
//!   plist shipped in the `.dmg`.
//! - Linux: an XDG autostart entry running the server persona without a
//!   terminal. There is no tray, so the app is stopped from the admin page
//!   (Admin -> Server -> Shut down server) instead of a window.
//!
//! The decision (which mechanism, which path, which content) is a pure function
//! so it can be tested for every platform on any platform; only the executor is
//! platform-specific.

use std::path::{Path, PathBuf};

use localreel_kernel::error::{KernelError, Result};

/// The per-user Run key. A scheduled task with a logon trigger needs
/// administrator rights, and the MSI installs per user, so this is the only
/// mechanism that works without elevation.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_VALUE: &str = "LocalReel";

/// Where an entry would be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Windows Run-key value with this name and command line.
    RunKey { name: String, command: String },
    /// A file written verbatim (XDG autostart entry, LaunchAgent plist).
    File { path: PathBuf, content: String },
}

impl Backend {
    /// One line for `doctor` and `autostart status`.
    pub fn describe(&self) -> String {
        match self {
            Backend::RunKey { name, .. } => {
                format!("HKCU\\{RUN_KEY} value '{name}'")
            }
            Backend::File { path, .. } => path.display().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        }
    }
}

pub const LAUNCH_AGENT_NAME: &str = "com.localreel.server.plist";
pub const XDG_ENTRY_NAME: &str = "localreel.desktop";

/// Decide the mechanism and its content. `home` is only used on Unix.
pub fn backend(platform: Platform, exe: &Path, home: &Path) -> Backend {
    let exe = exe.display();
    match platform {
        Platform::Windows => Backend::RunKey {
            name: RUN_VALUE.to_string(),
            // Desktop persona so the tray appears; --no-open because a login
            // should not steal the browser.
            command: format!("\"{exe}\" serve --mode desktop --tray --no-open"),
        },
        Platform::MacOs => Backend::File {
            path: home.join("Library/LaunchAgents").join(LAUNCH_AGENT_NAME),
            content: launch_agent(&exe.to_string()),
        },
        Platform::Linux => Backend::File {
            path: home.join(".config/autostart").join(XDG_ENTRY_NAME),
            content: xdg_entry(&exe.to_string()),
        },
    }
}

fn launch_agent(exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.localreel.server</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>serve</string>
        <string>--mode</string>
        <string>server</string>
        <string>--no-open</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>LOCALREEL_MODE</key>
        <string>server</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
</dict>
</plist>
"#
    )
}

fn xdg_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=LocalReel\n\
         Comment=Local-first media server (starts with the session)\n\
         Exec={exe} serve --mode server --no-open\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         Hidden=false\n"
    )
}

/// Read the current state without changing anything.
pub fn status() -> String {
    let backend = current_backend();
    let installed = match &backend {
        Backend::RunKey { name, .. } => run_key_exists(name),
        Backend::File { path, .. } => path.is_file(),
    };
    let state = if installed { "enabled" } else { "disabled" };
    format!("{state} ({})", backend.describe())
}

pub fn enable() -> Result<()> {
    match current_backend() {
        Backend::RunKey { name, command } => {
            run_reg(&[
                "add",
                &format!("HKCU\\{RUN_KEY}"),
                "/v",
                &name,
                "/t",
                "REG_SZ",
                "/d",
                &command,
                "/f",
            ])?;
            Ok(())
        }
        Backend::File { path, content } => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    KernelError::InvalidInput(format!(
                        "cannot create {}: {error}",
                        parent.display()
                    ))
                })?;
            }
            std::fs::write(&path, content).map_err(|error| {
                KernelError::InvalidInput(format!("cannot write {}: {error}", path.display()))
            })?;
            Ok(())
        }
    }
}

pub fn disable() -> Result<()> {
    match current_backend() {
        Backend::RunKey { name, .. } => {
            if run_key_exists(&name) {
                run_reg(&["delete", &format!("HKCU\\{RUN_KEY}"), "/v", &name, "/f"])?;
            }
            Ok(())
        }
        Backend::File { path, .. } => {
            if path.exists() {
                std::fs::remove_file(&path).map_err(|error| {
                    KernelError::InvalidInput(format!("cannot remove {}: {error}", path.display()))
                })?;
            }
            Ok(())
        }
    }
}

fn current_backend() -> Backend {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("localreel"));
    let home = home_dir();
    backend(Platform::current(), &exe, &home)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn run_key_exists(name: &str) -> bool {
    command_ok("reg", &["query", &format!("HKCU\\{RUN_KEY}"), "/v", name])
}

fn run_reg(args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("reg")
        .args(args)
        .output()
        .map_err(|error| KernelError::InvalidInput(format!("cannot run reg: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(KernelError::InvalidInput(format!(
        "reg {} failed: {}{}",
        args.first().unwrap_or(&""),
        stdout.trim(),
        stderr.trim()
    )))
}

fn command_ok(program: &str, args: &[&str]) -> bool {
    std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from(r"C:\Users\ana\AppData\Local\LocalReel\localreel.exe")
    }

    #[test]
    fn windows_uses_the_per_user_run_key() {
        let backend = backend(Platform::Windows, &exe(), Path::new(r"C:\Users\ana"));
        match backend {
            Backend::RunKey { name, command } => {
                assert_eq!(name, RUN_VALUE);
                // A login must not steal the browser, and the tray needs the
                // desktop persona because there is no console to look at.
                assert!(command.contains("serve --mode desktop --tray --no-open"));
                assert!(command.starts_with('"'));
            }
            other => panic!("unexpected backend: {other:?}"),
        }
    }

    #[test]
    fn linux_writes_an_xdg_entry_that_keeps_the_terminal() {
        let backend = backend(
            Platform::Linux,
            Path::new("/home/ana/.local/share/localreel/localreel"),
            Path::new("/home/ana"),
        );
        let Backend::File { path, content } = backend else {
            panic!("expected a file backend");
        };
        assert_eq!(
            path,
            Path::new("/home/ana/.config/autostart/localreel.desktop")
        );
        assert!(content.contains("Exec=/home/ana/.local/share/localreel/localreel serve"));
        // Server persona + no terminal: nothing pops up at login, and the
        // process is stopped from the admin page.
        assert!(content.contains("--mode server --no-open"));
        assert!(content.contains("Terminal=false"));
        assert!(content.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn macos_writes_a_launch_agent_for_the_server_persona() {
        let backend = backend(
            Platform::MacOs,
            Path::new("/Applications/LocalReel.app/Contents/MacOS/localreel"),
            Path::new("/Users/ana"),
        );
        let Backend::File { path, content } = backend else {
            panic!("expected a file backend");
        };
        assert_eq!(
            path,
            Path::new("/Users/ana/Library/LaunchAgents/com.localreel.server.plist")
        );
        assert!(content.contains("<key>RunAtLoad</key>"));
        assert!(content.contains("<string>--mode</string>"));
        assert!(content.contains("<string>server</string>"));
        assert!(content.contains("<key>KeepAlive</key>"));
    }
}
