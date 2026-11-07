//! Desktop launch behaviour: persona detection, browser opening, finding an
//! already running instance, and picking a port.
//!
//! One binary serves both personas. This module encodes *how* they differ, so
//! the rest of the server does not need to care:
//!
//! * **desktop** — a person launched it (shortcut, double click, terminal):
//!   open the UI, re-use a running instance, heal a busy port, stay visible.
//! * **server** — a service manager or container launched it: never open a
//!   window, keep the configured port strictly, log to a file/journal.

use std::io::{IsTerminal, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, Context};
use localreel_kernel::config::{Config, RunMode};

/// How many ports to try after the configured one in desktop mode.
pub const PORT_SCAN_ATTEMPTS: u16 = 10;

/// Environment facts used to choose a persona.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signals {
    pub stdin_is_terminal: bool,
    pub has_display: bool,
    pub in_container: bool,
    pub ssh: bool,
}

impl Signals {
    pub fn detect() -> Self {
        Self {
            stdin_is_terminal: std::io::stdin().is_terminal(),
            has_display: has_display(),
            in_container: in_container(),
            ssh: std::env::var_os("SSH_CONNECTION").is_some()
                || std::env::var_os("SSH_TTY").is_some(),
        }
    }
}

/// Decide the persona. `explicit` comes from `--mode`; `LOCALREEL_MODE` is
/// applied by the configuration layer before this runs, so callers pass the
/// resolved value when they have one.
pub fn decide(explicit: Option<RunMode>, signals: &Signals) -> RunMode {
    if let Some(mode) = explicit {
        return mode;
    }
    // A container or an SSH session is never somebody's desktop.
    if signals.in_container || signals.ssh {
        return RunMode::Server;
    }
    if cfg!(target_os = "windows") {
        // A console was allocated: a person launched it from Explorer or a
        // shortcut. A service manager starts it without a console.
        return if signals.stdin_is_terminal {
            RunMode::Desktop
        } else {
            RunMode::Server
        };
    }
    if cfg!(target_os = "macos") {
        // Finder launches have no terminal; the LaunchAgent we ship sets
        // LOCALREEL_MODE=server explicitly.
        return RunMode::Desktop;
    }
    if signals.has_display {
        RunMode::Desktop
    } else {
        RunMode::Server
    }
}

/// The UI address a person should open for this configuration.
pub fn web_url(host: &str, port: u16) -> String {
    let shown = match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
        other => other.to_string(),
    };
    if shown.contains(':') && !shown.starts_with('[') {
        format!("http://[{shown}]:{port}")
    } else {
        format!("http://{shown}:{port}")
    }
}

/// Result of asking a port whether LocalReel is already listening on it.
#[derive(Debug, Clone)]
pub struct RunningInstance {
    pub version: String,
}

/// Ask `host:port` whether it is a LocalReel server, without pulling in an HTTP
/// client: the health payload has a stable `"name":"LocalReel"` marker.
pub fn probe(host: &str, port: u16) -> Option<RunningInstance> {
    let target_host = match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1",
        other => other,
    };
    let address = (target_host, port).to_socket_addrs().ok()?.next()?;

    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(400)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(800)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(400)))
        .ok()?;

    let request =
        format!("GET /api/v1/health HTTP/1.0\r\nHost: {target_host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;

    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    if !response.contains("\"name\":\"LocalReel\"") {
        return None;
    }
    let version = response
        .split("\"version\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or("unknown")
        .to_string();
    Some(RunningInstance { version })
}

/// Bind the configured port.
///
/// Desktop heals a busy port by trying the next few; server stays strict so a
/// reverse proxy, bookmark or container port mapping never silently points at
/// nothing.
pub async fn bind_listener(
    host: &str,
    port: u16,
    mode: RunMode,
) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    match tokio::net::TcpListener::bind((host, port)).await {
        Ok(listener) => Ok((listener, port)),
        Err(error) if mode.is_desktop() && is_addr_in_use(&error) => {
            for offset in 1..=PORT_SCAN_ATTEMPTS {
                let Some(candidate) = port.checked_add(offset) else {
                    break;
                };
                if let Ok(listener) = tokio::net::TcpListener::bind((host, candidate)).await {
                    return Ok((listener, candidate));
                }
            }
            Err(anyhow!(
                "port {port} is in use and ports {}-{} are busy too; \
                 stop the other program or run `localreel serve --port <port>`",
                port + 1,
                port.saturating_add(PORT_SCAN_ATTEMPTS)
            ))
        }
        Err(error) => Err(anyhow!(error)).with_context(|| {
            format!(
                "cannot bind {host}:{port}; choose another port with `--port` or \
                 `[server] port` in localreel.toml"
            )
        }),
    }
}

/// Open the UI in the user's default browser. Failures are returned, never
/// panicked on: a missing `xdg-open` must not stop the server.
pub fn open_browser(url: &str) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "windows") {
        let mut command = Command::new("cmd");
        // `start` treats the first quoted argument as a window title.
        command.args(["/C", "start", "", url]);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(url);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    };

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

/// Print the banner a human can act on.
pub fn print_banner(config: &Config, url: &str, version: &str) {
    println!();
    println!("LocalReel {version}  ({} mode)", config.mode);
    println!("  Open:    {url}");
    println!("  Data:    {}", config.data_dir.display());
    println!("  Logs:    {}", config.log_dir.display());
    println!("  ffmpeg:  {}", ffmpeg_line(config));
    if config.mode.is_desktop() {
        println!("  Stop:    close this window, or press Ctrl+C");
    } else {
        println!("  Stop:    stop the service (Ctrl+C here, or `systemctl stop`)");
    }
    println!();
}

/// Whether the UI should be opened at all for this launch.
pub fn should_open_ui(config: &Config, no_open_flag: bool) -> bool {
    if no_open_flag || !config.mode.is_desktop() {
        return false;
    }
    !env_truthy("LOCALREEL_NO_OPEN")
}

/// Give a double-clicked window a chance to show its error before it closes.
pub fn pause_before_exit() {
    if !std::io::stdin().is_terminal() {
        return;
    }
    eprintln!();
    eprintln!("Press Enter to close this window.");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// Long-running commands should let the console window stay open on failure.
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal()
}

fn ffmpeg_line(config: &Config) -> String {
    format!(
        "{} / {}",
        config.media.ffmpeg.display(),
        config.media.ffprobe.display()
    )
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn has_display() -> bool {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        return true;
    }
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
}

fn in_container() -> bool {
    if std::env::var_os("KUBERNETES_SERVICE_HOST").is_some() {
        return true;
    }
    if std::env::var("container").is_ok_and(|value| !value.trim().is_empty()) {
        return true;
    }
    std::path::Path::new("/.dockerenv").exists()
}

fn is_addr_in_use(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::AddrInUse {
        return true;
    }
    let text = error.to_string();
    text.contains("10048") || text.contains("in use") || text.contains("只允许使用一次")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> Signals {
        Signals {
            stdin_is_terminal: false,
            has_display: false,
            in_container: false,
            ssh: false,
        }
    }

    #[test]
    fn explicit_mode_always_wins() {
        let mut s = signals();
        s.in_container = true;
        assert_eq!(decide(Some(RunMode::Desktop), &s), RunMode::Desktop);
        s.in_container = false;
        s.stdin_is_terminal = true;
        s.has_display = true;
        assert_eq!(decide(Some(RunMode::Server), &s), RunMode::Server);
    }

    #[test]
    fn containers_and_ssh_never_get_a_desktop() {
        let mut s = signals();
        s.stdin_is_terminal = true;
        s.has_display = true;
        s.in_container = true;
        assert_eq!(decide(None, &s), RunMode::Server);
        s.in_container = false;
        s.ssh = true;
        assert_eq!(decide(None, &s), RunMode::Server);
    }

    #[test]
    fn headless_session_is_a_server() {
        let s = signals();
        if !cfg!(target_os = "macos") {
            assert_eq!(decide(None, &s), RunMode::Server);
        }
    }

    #[test]
    fn ui_url_hides_unspecified_binds() {
        assert_eq!(web_url("127.0.0.1", 3001), "http://127.0.0.1:3001");
        assert_eq!(web_url("0.0.0.0", 3001), "http://127.0.0.1:3001");
        assert_eq!(web_url("::", 3001), "http://127.0.0.1:3001");
        assert_eq!(web_url("::1", 3001), "http://[::1]:3001");
        assert_eq!(web_url("nas.local", 8080), "http://nas.local:8080");
    }

    #[test]
    fn addr_in_use_detection_covers_windows_message() {
        let error = std::io::Error::from_raw_os_error(10048);
        // On non-Windows this is just an unknown OS error, so only assert the
        // generic path.
        if cfg!(target_os = "windows") {
            assert!(is_addr_in_use(&error));
        }
        let generic = std::io::Error::new(std::io::ErrorKind::AddrInUse, "in use");
        assert!(is_addr_in_use(&generic));
    }
}
