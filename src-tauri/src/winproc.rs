//! Windows process helpers: a kill-on-close job object for every child we spawn (so nothing
//! outlives Relay, even on a crash), hidden-console spawning, orphan detection by PID file,
//! and a best-effort graceful Ctrl+C.

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Console::{
        AttachConsole, CTRL_C_EVENT, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleWindow, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
        JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS, JOB_OBJECT_UILIMIT_READCLIPBOARD,
        JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOBOBJECT_BASIC_UI_RESTRICTIONS,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess,
    };

    pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    struct Job(HANDLE);
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    fn create_job() -> Option<Job> {
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(h);
                return None;
            }
            Some(Job(h))
        }
    }

    /// Process-wide job: closed by the OS when Relay exits, which kills all assigned children.
    fn global_job() -> Option<&'static Job> {
        static JOB: OnceLock<Option<Job>> = OnceLock::new();
        JOB.get_or_init(create_job).as_ref()
    }

    /// A dedicated job for one process tree (e.g. the Claude CLI and its children), so it can be
    /// terminated as a whole on cancel. Dropping it kills the tree.
    pub struct TreeJob(Job);

    impl TreeJob {
        pub fn new() -> Option<Self> {
            create_job().map(TreeJob)
        }
        pub fn assign_pid(&self, pid: u32) -> bool {
            assign(self.0.0, pid)
        }
        /// Assign a process by handle (e.g. one created suspended, before it runs).
        pub fn assign_handle(&self, process: std::os::windows::io::RawHandle) -> bool {
            unsafe { AssignProcessToJobObject(self.0.0, process as HANDLE) != 0 }
        }
        /// Sandbox limits for the job's processes: no clipboard, no logoff/shutdown, no changes
        /// to system parameters or display settings, no switching or creating desktops.
        pub fn restrict_ui(&self) -> bool {
            let info = JOBOBJECT_BASIC_UI_RESTRICTIONS {
                UIRestrictionsClass: JOB_OBJECT_UILIMIT_READCLIPBOARD
                    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                    | JOB_OBJECT_UILIMIT_EXITWINDOWS
                    | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
                    | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                    | JOB_OBJECT_UILIMIT_DESKTOP,
            };
            unsafe {
                SetInformationJobObject(
                    self.0.0,
                    JobObjectBasicUIRestrictions,
                    &info as *const _ as *const _,
                    std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
                ) != 0
            }
        }
        pub fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0.0, 1);
            }
        }
    }

    impl Drop for TreeJob {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0.0);
            }
        }
    }

    fn assign(job: HANDLE, pid: u32) -> bool {
        unsafe {
            let ph = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if ph.is_null() {
                return false;
            }
            let ok = AssignProcessToJobObject(job, ph) != 0;
            CloseHandle(ph);
            ok
        }
    }

    pub fn assign_to_global_job(pid: u32) -> bool {
        match global_job() {
            Some(j) => assign(j.0, pid),
            None => false,
        }
    }

    pub fn image_path(pid: u32) -> Option<String> {
        unsafe {
            let ph = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if ph.is_null() {
                return None;
            }
            let mut buf = vec![0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(ph, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(ph);
            if ok == 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..len as usize]))
        }
    }

    pub fn kill_pid(pid: u32) -> bool {
        unsafe {
            let ph = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if ph.is_null() {
                return false;
            }
            let ok = TerminateProcess(ph, 1) != 0;
            CloseHandle(ph);
            ok
        }
    }

    /// Send Ctrl+C to a console child. Only attempted when Relay itself has no console (release
    /// builds), because detaching from our own console would break dev logging.
    pub fn send_ctrl_c(pid: u32) -> bool {
        unsafe {
            if !GetConsoleWindow().is_null() {
                return false;
            }
            FreeConsole();
            if AttachConsole(pid) == 0 {
                return false;
            }
            // Ignore Ctrl+C in our own process while attached to the child's console.
            SetConsoleCtrlHandler(None, 1);
            let ok = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) != 0;
            FreeConsole();
            ok
        }
    }
}

#[cfg(windows)]
pub use imp::*;

#[cfg(not(windows))]
mod imp {
    pub const CREATE_NO_WINDOW: u32 = 0;
    pub struct TreeJob;
    impl TreeJob {
        pub fn new() -> Option<Self> {
            None
        }
        pub fn assign_pid(&self, _pid: u32) -> bool {
            false
        }
        pub fn terminate(&self) {}
    }
    pub fn assign_to_global_job(_pid: u32) -> bool {
        false
    }
    pub fn image_path(_pid: u32) -> Option<String> {
        None
    }
    pub fn kill_pid(_pid: u32) -> bool {
        false
    }
    pub fn send_ctrl_c(_pid: u32) -> bool {
        false
    }
}
#[cfg(not(windows))]
pub use imp::*;

/// Configure a tokio Command to run without flashing a console window.
pub fn hide_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

pub fn hide_window_std(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}
