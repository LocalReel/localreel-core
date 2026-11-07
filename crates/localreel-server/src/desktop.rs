//! `localreel-desktop.exe`: the Windows shortcut and login entry point.
//!
//! The server is a console program: started directly it shows a console window
//! for a moment before the tray hides it, and every login does that again. This
//! launcher is a GUI-subsystem program (no console of its own) that starts the
//! server detached with `CREATE_NO_WINDOW`, so nothing flashes.
//!
//! It does not stay resident: the server owns the process lifetime, the tray is
//! its UI, and a second launch simply re-opens the browser (the server handles
//! that case itself).
//!
//! On other platforms the binary exists only so the workspace builds; it is not
//! packaged anywhere.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

    let exe = std::env::current_exe().and_then(|path| {
        path.parent()
            .map(|dir| dir.join("localreel.exe"))
            .ok_or_else(|| std::io::Error::other("no parent directory"))
    });
    let Ok(exe) = exe else {
        message("LocalReel could not find localreel.exe next to the launcher.");
        return;
    };
    if !exe.is_file() {
        message(&format!(
            "LocalReel is incomplete: {} is missing.",
            exe.display()
        ));
        return;
    }

    // Desktop persona, tray, browser. The server hides nothing here because it
    // has no console to hide.
    let mut child = Command::new(&exe);
    child
        .arg("serve")
        .arg("--mode")
        .arg("desktop")
        .arg("--tray")
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Err(error) = child.spawn() {
        message(&format!("LocalReel could not start:\n\n{error}"));
    }
}

#[cfg(windows)]
fn message(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let wide =
        |value: &str| -> Vec<u16> { value.encode_utf16().chain(std::iter::once(0)).collect() };
    let body = wide(text);
    let title = wide("LocalReel");
    // SAFETY: both strings are NUL-terminated and live for the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("localreel-desktop is the Windows launcher; use `localreel serve` on this platform.");
    std::process::exit(1);
}
