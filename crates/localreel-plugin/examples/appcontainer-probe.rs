//! Windows AppContainer network-isolation prototype.
//!
//! Run with:
//! `cargo run -p localreel-plugin --example appcontainer-probe -- <profile> <child.exe>`
//!
//! The child is copied into the AppContainer profile folder (the one directory
//! the container can execute from without touching user ACLs) and launched with
//! no capabilities. A successful probe means the child could not reach the
//! network.

#[cfg(windows)]
fn main() {
    if let Err(error) = run() {
        eprintln!("appcontainer probe failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("appcontainer probe is Windows-only");
    std::process::exit(2);
}

#[cfg(windows)]
fn run() -> Result<(), String> {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use windows_sys::core::{HRESULT, PWSTR};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
        GetAppContainerFolderPath,
    };
    use windows_sys::Win32::Security::{FreeSid, SECURITY_CAPABILITIES};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
        EXTENDED_STARTUPINFO_PRESENT, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
        STARTUPINFOW,
    };

    let mut args = std::env::args().skip(1);
    let profile = args.next().ok_or("missing profile name")?;
    let program = args.next().ok_or("missing child executable")?;
    let child_args: Vec<String> = args.collect();

    let profile_wide = wide(&profile);
    let display_wide = wide("LocalReel plugin sandbox");
    let description_wide = wide("LocalReel AppContainer probe");
    let mut sid = null_mut();
    let created = unsafe {
        CreateAppContainerProfile(
            profile_wide.as_ptr(),
            display_wide.as_ptr(),
            description_wide.as_ptr(),
            null(),
            0,
            &mut sid,
        )
    };
    if created < 0 {
        let derived =
            unsafe { DeriveAppContainerSidFromAppContainerName(profile_wide.as_ptr(), &mut sid) };
        if derived < 0 {
            return Err(format!(
                "AppContainer profile unavailable (create=0x{created:08x}, derive=0x{derived:08x})"
            ));
        }
    }

    let sid_string = sid_to_string(sid)?;
    let sid_string_wide = wide(&sid_string);
    let mut folder: PWSTR = null_mut();
    let folder_hr: HRESULT =
        unsafe { GetAppContainerFolderPath(sid_string_wide.as_ptr(), &mut folder) };
    if folder_hr < 0 {
        unsafe { FreeSid(sid) };
        return Err(format!(
            "GetAppContainerFolderPath failed: 0x{folder_hr:08x}"
        ));
    }
    let folder_path = pwstr_to_string(folder);
    unsafe { LocalFree(folder as *mut c_void) };
    let profile_dir = std::path::PathBuf::from(folder_path);
    std::fs::create_dir_all(&profile_dir)
        .map_err(|error| format!("cannot create profile folder: {error}"))?;

    let source = std::fs::canonicalize(&program)
        .map_err(|error| format!("cannot resolve {program}: {error}"))?;
    let file_name = source
        .file_name()
        .ok_or("child executable has no file name")?;
    let local_child = profile_dir.join(file_name);
    std::fs::copy(&source, &local_child)
        .map_err(|error| format!("cannot copy child into AppContainer folder: {error}"))?;
    // Rust's MSVC binaries import the VC runtime; the AppContainer cannot read
    // arbitrary loader search paths, so place it beside the executable.
    let runtime = std::path::Path::new(r"C:\Windows\System32\vcruntime140.dll");
    if runtime.is_file() {
        let target = profile_dir.join("vcruntime140.dll");
        let _ = std::fs::copy(runtime, target);
    }

    let mut command_line = quote_path(&local_child.to_string_lossy());
    for arg in &child_args {
        command_line.push(' ');
        command_line.push_str(&quote_arg(arg));
    }
    let mut command_line_wide = wide(&command_line);
    let current_dir_wide = wide(&profile_dir.to_string_lossy());

    let mut size = 0usize;
    unsafe {
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
    }
    let words = size.div_ceil(std::mem::size_of::<usize>()).max(1);
    let mut attribute_buffer = vec![0usize; words];
    let list = attribute_buffer.as_mut_ptr() as *mut c_void;
    if unsafe { InitializeProcThreadAttributeList(list, 1, 0, &mut size) } == 0 {
        unsafe { FreeSid(sid) };
        return Err(format!(
            "InitializeProcThreadAttributeList failed: {}",
            unsafe { GetLastError() }
        ));
    }
    let mut capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid,
        Capabilities: null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let updated = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES
                as usize,
            &mut capabilities as *mut _ as *mut c_void,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null_mut(),
        )
    };
    if updated == 0 {
        unsafe {
            DeleteProcThreadAttributeList(list);
            FreeSid(sid);
        }
        return Err(format!("UpdateProcThreadAttribute failed: {}", unsafe {
            GetLastError()
        }));
    }

    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: unsafe { GetStdHandle(STD_INPUT_HANDLE) },
        hStdOutput: unsafe { GetStdHandle(STD_OUTPUT_HANDLE) },
        hStdError: unsafe { GetStdHandle(STD_ERROR_HANDLE) },
        ..unsafe { std::mem::zeroed() }
    };
    startup.lpAttributeList = list;

    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let child_created = unsafe {
        CreateProcessW(
            null(),
            command_line_wide.as_mut_ptr(),
            null(),
            null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT,
            null(),
            current_dir_wide.as_ptr(),
            &startup.StartupInfo,
            &mut process,
        )
    };
    unsafe {
        DeleteProcThreadAttributeList(list);
        FreeSid(sid);
    }
    if child_created == 0 {
        return Err(format!("CreateProcessW failed: {}", unsafe {
            GetLastError()
        }));
    }

    unsafe {
        CloseHandle(process.hThread);
        let wait = WaitForSingleObject(process.hProcess, 30_000);
        let mut code = 1u32;
        GetExitCodeProcess(process.hProcess, &mut code);
        CloseHandle(process.hProcess);
        if wait != WAIT_OBJECT_0 {
            return Err(format!("child did not exit (wait={wait})"));
        }
        if code != 0 {
            return Err(format!("child exited with {code}"));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn sid_to_string(sid: windows_sys::Win32::Security::PSID) -> Result<String, String> {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows_sys::core::PWSTR;
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;

    let mut text: PWSTR = null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(format!("ConvertSidToStringSidW failed: {}", unsafe {
            GetLastError()
        }));
    }
    let value = pwstr_to_string(text);
    unsafe { LocalFree(text as *mut c_void) };
    Ok(value)
}

#[cfg(windows)]
fn pwstr_to_string(text: windows_sys::core::PWSTR) -> String {
    let mut length = 0usize;
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, length))
    }
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn quote_path(path: &str) -> String {
    format!("\"{}\"", path.replace('"', "\\\""))
}

#[cfg(windows)]
fn quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    if !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut quoted = String::from("\"");
    for character in arg.chars() {
        if character == '"' {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}
