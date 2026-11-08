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

/// Prepare a command for tree-aware lifecycle handling.
pub fn configure(_command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // The child becomes the leader of a new process group; signalling
        // `-pid` then reaches every helper it spawns.
        _command.process_group(0);
    }
}

/// Attach the freshly spawned child to the host's lifecycle guard.
pub fn attach(child: &mut Child) -> Guard {
    Guard::new(child)
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
    fn new(child: &mut Child) -> Self {
        // A job whose handle is closed kills everything inside it. Attaching
        // fails only when the process already has a job that forbids nesting;
        // the entry process is still killed by `terminate`, just not its
        // helpers.
        let job = JobHandle::create().and_then(|job| job.assign(child).then_some(job));
        Self { job }
    }

    #[cfg(not(windows))]
    fn new(_child: &mut Child) -> Self {
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
    fn create() -> Option<Self> {
        use std::sync::atomic::AtomicBool;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return None;
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let assigned = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
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
