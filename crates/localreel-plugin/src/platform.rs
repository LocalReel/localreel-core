//! Keeping a plugin's *process tree* attached to the host.
//!
//! The protocol kills the entry process, but "the entry process" is not the
//! whole story: a script plugin runs under an interpreter, and a native plugin
//! may spawn helpers. If the host dies abruptly — a crash, `taskkill /F`, a
//! container runtime sending SIGKILL — the child must not survive as an orphan
//! holding a loopback port.
//!
//! - Windows: a Job Object with `KILL_ON_JOB_CLOSE`, assigned before the plugin
//!   does any real work. Closing the last handle (including on host death)
//!   terminates every process in the job.
//! - Unix: the child gets its own process group, so the host can signal the
//!   whole group instead of one pid.

use std::process::{Child, Command};

use crate::limits::PluginLimits;

/// Remove an AppContainer profile after the host has killed its wrapper.
///
/// The wrapper cleans up on a normal child exit; this is the safety net for
/// `Drop`/timeout paths where the job is terminated before the wrapper runs its
/// own cleanup.
#[cfg(windows)]
pub fn cleanup_appcontainer(profile: &str) {
    use windows_sys::Win32::Security::Isolation::DeleteAppContainerProfile;

    let name = wide(profile);
    unsafe {
        DeleteAppContainerProfile(name.as_ptr());
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let path = std::path::PathBuf::from(local)
            .join("Packages")
            .join(profile);
        let _ = std::fs::remove_dir_all(path);
    }
}

#[cfg(not(windows))]
pub fn cleanup_appcontainer(_profile: &str) {}

/// Remove AppContainer profiles whose wrapper is no longer running.
///
/// `root` is normally `%LOCALAPPDATA%\Packages`. A profile is only touched when
/// it has a `localreel.pid` marker and that PID is gone; a live wrapper (another
/// server, or this one) is left alone. The marker is written by `plugin-sandbox`
/// before the child starts.
#[cfg(windows)]
pub fn sweep_stale_appcontainers(root: &std::path::Path) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::Isolation::DeleteAppContainerProfile;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("localreel-") {
            continue;
        }
        let path = entry.path();
        let marker = path.join("localreel.pid");
        let Ok(pid) = std::fs::read_to_string(&marker) else {
            continue;
        };
        let Ok(pid) = pid.trim().parse::<u32>() else {
            continue;
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if !handle.is_null() {
            let mut code = 0u32;
            let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
            unsafe { CloseHandle(handle) };
            if ok != 0 && code == 259 {
                continue;
            }
        }
        let _ = std::fs::remove_dir_all(&path);
        let profile = wide(name);
        unsafe {
            DeleteAppContainerProfile(profile.as_ptr());
        }
    }
}

#[cfg(not(windows))]
pub fn sweep_stale_appcontainers(_root: &std::path::Path) {}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn sweep_removes_dead_profiles_and_keeps_live_ones() {
        let root = tempfile::tempdir().unwrap();
        let dead = root.path().join("localreel-dead-test");
        let alive = root.path().join("localreel-alive-test");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::create_dir_all(&alive).unwrap();
        std::fs::write(dead.join("localreel.pid"), u32::MAX.to_string()).unwrap();
        std::fs::write(alive.join("localreel.pid"), std::process::id().to_string()).unwrap();

        sweep_stale_appcontainers(root.path());

        assert!(!dead.exists());
        assert!(alive.exists());
    }
}

/// Prepare a command for tree-aware lifecycle handling and resource ceilings.
pub fn configure(command: &mut Command, limits: PluginLimits) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // The child becomes the leader of a new process group; signalling
        // `-pid` then reaches every helper it spawns.
        command.process_group(0);
        // Address-space ceiling before exec. `RLIMIT_NPROC` is per-user, not
        // per-tree, so process-count enforcement stays a Windows Job Object
        // property for now; the Unix side at least stops memory blow-ups.
        //
        // macOS does not implement `RLIMIT_AS` and rejects it with EINVAL, and a
        // failing `pre_exec` aborts the spawn itself — so the ceiling is applied
        // only where the kernel honours it.
        #[cfg(not(target_os = "macos"))]
        {
            let max = limits.max_memory_bytes as libc::rlim_t;
            unsafe {
                command.pre_exec(move || {
                    let limit = libc::rlimit {
                        rlim_cur: max,
                        rlim_max: max,
                    };
                    if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(target_os = "macos")]
        let _ = limits;
    }
    #[cfg(not(unix))]
    {
        let _ = (command, limits);
    }
}

/// Attach the freshly spawned child to the host's lifecycle guard.
pub fn attach(child: &mut Child, limits: PluginLimits) -> Guard {
    Guard::new(child, limits)
}

/// Terminate the child and everything it spawned.
pub fn terminate(child: &mut Child, guard: &Guard) {
    #[cfg(unix)]
    {
        // Signal the group first: the leader may already be gone while a
        // helper keeps the port.
        if let Ok(pid) = i32::try_from(child.id()) {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
    guard.close();
}

/// Platform-specific handle that dies with the host.
pub struct Guard {
    #[cfg(windows)]
    job: Option<JobHandle>,
}

impl Guard {
    #[cfg(windows)]
    fn new(child: &mut Child, limits: PluginLimits) -> Self {
        // A job whose handle is closed kills everything inside it. Attaching
        // fails only when the process already has a job that forbids nesting;
        // the entry process is still killed by `terminate`, just not its
        // helpers.
        let job = JobHandle::create(limits).and_then(|job| job.assign(child).then_some(job));
        Self { job }
    }

    #[cfg(not(windows))]
    fn new(_child: &mut Child, _limits: PluginLimits) -> Self {
        Self {}
    }

    /// Close the platform handle. Killing on close is the safety net.
    pub fn close(&self) {
        #[cfg(windows)]
        {
            // Dropping the handle is what triggers KILL_ON_JOB_CLOSE; do it now
            // rather than waiting for the runtime struct to drop.
            if let Some(job) = &self.job {
                job.close();
            }
        }
    }
}

#[cfg(windows)]
struct JobHandle {
    handle: windows_sys::Win32::Foundation::HANDLE,
    open: std::sync::atomic::AtomicBool,
}

#[cfg(windows)]
impl JobHandle {
    fn create(limits: PluginLimits) -> Option<Self> {
        use std::sync::atomic::AtomicBool;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        };

        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return None;
            }
            let mut limits_struct: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits_struct.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits_struct.BasicLimitInformation.ActiveProcessLimit = limits.max_processes;
            limits_struct.ProcessMemoryLimit =
                usize::try_from(limits.max_memory_bytes).unwrap_or(usize::MAX);
            let assigned = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits_struct as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if assigned == 0 {
                windows_sys::Win32::Foundation::CloseHandle(handle);
                return None;
            }
            Some(Self {
                handle,
                open: AtomicBool::new(true),
            })
        }
    }

    fn assign(&self, child: &mut Child) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle() as _) != 0 }
    }

    fn close(&self) {
        use std::sync::atomic::Ordering;
        use windows_sys::Win32::Foundation::CloseHandle;

        if self.open.swap(false, Ordering::SeqCst) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        self.close();
    }
}

// The job handle is only used through Win32 calls, which are thread-safe.
#[cfg(windows)]
unsafe impl Send for JobHandle {}
#[cfg(windows)]
unsafe impl Sync for JobHandle {}
