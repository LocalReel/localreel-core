//! Windows AppContainer launcher used by `PluginRuntime` for isolated plugins.
//!
//! The host starts this wrapper with ordinary inherited stdio. The wrapper:
//! 1. creates/derives an AppContainer profile;
//! 2. copies the plugin install directory into the profile folder (the one
//!    place the container can execute from without ACL surgery);
//! 3. copies `vcruntime140.dll` beside the entry program;
//! 4. starts the plugin with no capabilities and the inherited std handles.
//!
//! It prints nothing on stdout: stdout belongs to the plugin protocol.

#[cfg(windows)]
fn main() {
    if let Err(error) = run() {
        eprintln!("plugin sandbox failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("plugin sandbox is Windows-only");
    std::process::exit(2);
}

#[cfg(windows)]
struct Args {
    profile: String,
    source: std::path::PathBuf,
    program: std::path::PathBuf,
    child_args: Vec<String>,
}

#[cfg(windows)]
fn parse_args() -> Result<Args, String> {
    let mut profile = None;
    let mut source = None;
    let mut program = None;
    let mut child_args = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => profile = args.next(),
            "--source" => source = args.next().map(std::path::PathBuf::from),
            "--program" => program = args.next().map(std::path::PathBuf::from),
            "--" => {
                child_args.extend(args);
                break;
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Args {
        profile: profile.ok_or("missing --profile")?,
        source: source.ok_or("missing --source")?,
        program: program.ok_or("missing --program")?,
        child_args,
    })
}

#[cfg(windows)]
fn run() -> Result<(), String> {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::Isolation::{
        CreateAppContainerProfile, DeleteAppContainerProfile,
        DeriveAppContainerSidFromAppContainerName, GetAppContainerFolderPath,
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

    let args = parse_args()?;
    let source = std::fs::canonicalize(&args.source)
        .map_err(|error| format!("cannot resolve source: {error}"))?;
    let program = std::fs::canonicalize(&args.program)
        .map_err(|error| format!("cannot resolve program: {error}"))?;
    let relative = program
        .strip_prefix(&source)
        .map_err(|_| "program must live inside --source".to_string())?;

    let profile_wide = wide(&args.profile);
    let display_wide = wide("LocalReel plugin sandbox");
    let description_wide = wide("Isolated LocalReel plugin");
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
    let mut folder: windows_sys::core::PWSTR = null_mut();
    let path_hr = unsafe { GetAppContainerFolderPath(wide(&sid_string).as_ptr(), &mut folder) };
    if path_hr < 0 {
        unsafe { FreeSid(sid) };
        return Err(format!("GetAppContainerFolderPath failed: 0x{path_hr:08x}"));
    }
    let profile_dir = std::path::PathBuf::from(pwstr_to_string(folder));
    unsafe { LocalFree(folder as *mut c_void) };

    let plugin_dir = profile_dir.join("plugin");
    if plugin_dir.exists() {
        std::fs::remove_dir_all(&plugin_dir)
            .map_err(|error| format!("cannot clear old plugin dir: {error}"))?;
    }
    std::fs::create_dir_all(&plugin_dir)
        .map_err(|error| format!("cannot create plugin dir: {error}"))?;
    let mut files = 0usize;
    copy_tree(&source, &plugin_dir, 0, &mut files)?;

    let target = plugin_dir.join(relative);
    let target_dir = target.parent().ok_or("program has no parent")?;
    let runtime = std::path::Path::new(r"C:\Windows\System32\vcruntime140.dll");
    if runtime.is_file() {
        let _ = std::fs::copy(runtime, target_dir.join("vcruntime140.dll"));
    }

    let mut command_line = quote_path(&target.to_string_lossy());
    for arg in &args.child_args {
        command_line.push(' ');
        command_line.push_str(&quote_arg(arg));
    }
    let mut command_line_wide = wide(&command_line);
    let current_dir_wide = wide(&target_dir.to_string_lossy());

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

    let code = unsafe {
        CloseHandle(process.hThread);
        let wait = WaitForSingleObject(process.hProcess, 300_000);
        let mut code = 1u32;
        GetExitCodeProcess(process.hProcess, &mut code);
        CloseHandle(process.hProcess);
        if wait != WAIT_OBJECT_0 {
            return Err(format!("child did not exit (wait={wait})"));
        }
        code
    };
    let _ = std::fs::remove_dir_all(&plugin_dir);
    unsafe { DeleteAppContainerProfile(profile_wide.as_ptr()) };
    let _ = std::fs::remove_dir_all(&profile_dir);
    std::process::exit(code as i32);
}

#[cfg(windows)]
fn copy_tree(
    source: &std::path::Path,
    destination: &std::path::Path,
    depth: usize,
    files: &mut usize,
) -> Result<(), String> {
    if depth > 8 {
        return Err("plugin directory is too deep".to_string());
    }
    for entry in std::fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let target = destination.join(entry.file_name());
        let metadata = entry.metadata().map_err(|error| error.to_string())?;
        if metadata.is_dir() {
            std::fs::create_dir_all(&target).map_err(|error| error.to_string())?;
            copy_tree(&path, &target, depth + 1, files)?;
        } else if metadata.is_file() {
            *files += 1;
            if *files > 512 {
                return Err("plugin install directory has too many files".to_string());
            }
            if metadata.len() > 64 * 1024 * 1024 {
                return Err("plugin file is larger than 64 MiB".to_string());
            }
            std::fs::copy(&path, &target).map_err(|error| error.to_string())?;
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
