//! Startup-scoped Job Object (design R1, REQ-DAEMON-003 GIVEN-3).
//!
//! The spawner must guarantee that a daemon it started does not become an
//! orphan when the spawner dies *before* the daemon reaches readiness
//! (cookie written + pipe bound). The Windows mechanism is a Job Object
//! created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`:
//!
//! 1. [`StartupJob::create_kill_on_close`] creates the job (armed).
//! 2. [`StartupJob::assign_process`] binds the freshly spawned daemon to it.
//! 3. The spawner waits for readiness while holding the job handle.
//!    Spawner death pre-readiness closes the last handle → the armed job
//!    terminates the daemon → no orphan (spec GIVEN-3).
//! 4. Post-readiness the spawner calls [`StartupJob::disarm_kill_on_close`]
//!    (clears `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), then drops the handle.
//!    Closing the handle no longer cascades → the daemon outlives any
//!    client (design R1: "post-readiness daemon survives any client").
//!
//! This is the startup-scoped scheme design R1 chose over a daemon-retained
//! handle: no cross-process handle sharing, no starter-held handle after
//! readiness. Windows has no documented API to remove a single process from
//! a job (the design's `AssignProcessToJobObject(NULL)` self-detach does
//! not exist), so the disarm step clears the kill-on-close flag instead —
//! same observable behavior, all-documented APIs.
//!
//! The daemon is spawned with `CREATE_BREAKAWAY_FROM_JOB` (see
//! `memento-cli::spawn`), so it starts outside any job and
//! `AssignProcessToJobObject` can bind it. Non-Windows targets get a
//! no-op stub (pipe transport is Windows-first by design).

use std::io;
use std::process::Child;

#[cfg(windows)]
mod imp {
    use super::*;
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows::core::PCWSTR;

    /// A startup-scoped Job Object handle. Dropping it closes the last
    /// handle; while armed (`KILL_ON_JOB_CLOSE` set) that terminates every
    /// assigned process — which is exactly the orphan guard the spawner
    /// wants *until* readiness, and exactly what [`StartupJob::disarm_kill_on_close`]
    /// disables *after* readiness.
    pub struct StartupJob {
        handle: HANDLE,
    }

    impl StartupJob {
        /// Create an anonymous job armed with `KILL_ON_JOB_CLOSE |
        /// BREAKAWAY_OK` (design R1). Anonymous: the spawner does both the
        /// assign and the disarm, so no name is needed.
        pub fn create_kill_on_close() -> io::Result<Self> {
            // SAFETY: NULL security attributes + NULL name → default DACL,
            // anonymous job. The returned handle is owned by us.
            let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                .map_err(|err| io::Error::from_raw_os_error(err.code().0 & 0xFFFF))?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            // SAFETY: `info` is a valid JOBOBJECT_EXTENDED_LIMIT_INFORMATION
            // of the exact size we pass; `handle` is an open job handle.
            let result = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            match result {
                Ok(()) => Ok(Self { handle }),
                Err(err) => {
                    // SAFETY: the handle was created successfully above and
                    // is not used afterwards.
                    unsafe {
                        let _ = CloseHandle(handle);
                    };
                    Err(io::Error::from_raw_os_error(err.code().0 & 0xFFFF))
                }
            }
        }

        /// Bind a freshly spawned child process to this job. Fails if the
        /// child is already in another job (the spawner must use
        /// `CREATE_BREAKAWAY_FROM_JOB` so the child starts job-free).
        pub fn assign_process(&self, child: &Child) -> io::Result<()> {
            let process = HANDLE(child.as_raw_handle());
            // SAFETY: `self.handle` is an open job handle and `process` is
            // the live process handle of the spawned child.
            match unsafe { AssignProcessToJobObject(self.handle, process) } {
                Ok(()) => Ok(()),
                Err(err) => Err(io::Error::from_raw_os_error(err.code().0 & 0xFFFF)),
            }
        }

        /// Clear `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (readiness passed).
        /// After this, closing the last job handle does NOT terminate the
        /// assigned processes (R1 post-readiness release).
        pub fn disarm_kill_on_close(&self) -> io::Result<()> {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            // Read-modify-write is unnecessary: LimitFlags is a full bit
            // field and KILL_ON_JOB_CLOSE is the only limit we ever set.
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            // SAFETY: same shape as `create_kill_on_close`.
            unsafe {
                SetInformationJobObject(
                    self.handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            }
            .map_err(|err| io::Error::from_raw_os_error(err.code().0 & 0xFFFF))
        }
    }

    impl Drop for StartupJob {
        fn drop(&mut self) {
            // SAFETY: `handle` is the last owned handle to this job; after
            // this call the job object is destroyed (processes stay alive
            // unless KILL_ON_JOB_CLOSE was still armed).
            unsafe {
                let _ = CloseHandle(self.handle);
            };
        }
    }

    /// Whether the process `pid` is still alive (used by the orphan-guard
    /// tests to prove KILL_ON_JOB_CLOSE semantics without polling the
    /// process table).
    pub fn is_process_alive(pid: u32) -> bool {
        use windows::Win32::Foundation::STILL_ACTIVE;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: OpenProcess returns a handle we own and close in every
        // path; the query asks only for limited info (no privilege needs).
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(h) => h,
            Err(_) => return false,
        };
        let mut code = 0u32;
        // SAFETY: `handle` is a valid process handle; `code` is a valid out
        // buffer.
        let queried = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
        // SAFETY: `handle` is owned by this function and not used after.
        unsafe {
            let _ = CloseHandle(handle);
        };
        queried && code == STILL_ACTIVE.0 as u32
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    /// Non-Windows stub: the pipe transport is Windows-first (design D5),
    /// so no job object exists elsewhere. Creation fails with `Unsupported`
    /// and the spawner surfaces it as a spawn error.
    pub struct StartupJob;

    impl StartupJob {
        pub fn create_kill_on_close() -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Job Objects are Windows-only; daemon mode is unsupported here",
            ))
        }
        pub fn assign_process(&self, _child: &Child) -> io::Result<()> {
            Ok(())
        }
        pub fn disarm_kill_on_close(&self) -> io::Result<()> {
            Ok(())
        }
    }

    pub fn is_process_alive(_pid: u32) -> bool {
        false
    }
}

pub use imp::{StartupJob, is_process_alive};

#[cfg(test)]
mod tests {
    //! RED-first tests for REQ-DAEMON-003 GIVEN-3 (orphan guard):
    //! `KILL_ON_JOB_CLOSE` must terminate the assigned child when the last
    //! handle closes, and the disarm step must make the child survive the
    //! handle close (post-readiness release).

    use super::*;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// A cheap long-lived child that needs no admin rights: `ping` against
    /// the loopback with a large count runs for minutes.
    /// `Child` is deliberately never waited on: the tests prove the job
    /// object (or taskkill) terminates it, and dropping the handle is the
    /// point of the orphan-guard assertion.
    fn spawn_long_lived() -> Child {
        Command::new("ping")
            .args(["127.0.0.1", "-n", "100", "-w", "1000"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("ping spawns")
    }

    fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if !is_process_alive(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    // The spawned children are the subject of the assertion: the armed
    // job is what terminates them (never `.wait()`ed — that is the point).
    #[allow(clippy::zombie_processes)]
    #[test]
    fn kill_on_close_terminates_assigned_child_when_last_handle_closes() {
        // Spec GIVEN-3 at the mechanism level: spawner death pre-readiness
        // == last job handle closes while still armed → the child must not
        // survive as an orphan.
        let job = StartupJob::create_kill_on_close().expect("job");
        let child = spawn_long_lived();
        let pid = child.id();
        job.assign_process(&child).expect("assign");
        drop(job);
        assert!(
            wait_until_dead(pid, Duration::from_secs(10)),
            "orphan guard: child pid {pid} survived the armed job close"
        );
    }

    #[allow(clippy::zombie_processes)]
    #[test]
    fn disarmed_job_lets_child_survive_handle_close() {
        // R1 post-readiness release: after `disarm_kill_on_close`, closing
        // the last handle must NOT terminate the child — the daemon
        // outlives the spawning client.
        let job = StartupJob::create_kill_on_close().expect("job");
        let child = spawn_long_lived();
        let pid = child.id();
        job.assign_process(&child).expect("assign");
        job.disarm_kill_on_close().expect("disarm");
        drop(job);
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            is_process_alive(pid),
            "post-readiness release: child pid {pid} must survive the disarmed job close"
        );
        // Cleanup: best-effort force-kill so the test leaves no zombies.
        let _ = Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
    }

    #[allow(clippy::zombie_processes)]
    #[test]
    fn kill_on_close_terminates_every_assigned_child() {
        // Triangulation: the guard applies to every process bound to the
        // job (multiple assigns on the same handle).
        let job = StartupJob::create_kill_on_close().expect("job");
        let c1 = spawn_long_lived();
        let p1 = c1.id();
        let c2 = spawn_long_lived();
        let p2 = c2.id();
        job.assign_process(&c1).expect("assign 1");
        job.assign_process(&c2).expect("assign 2");
        drop(job);
        assert!(
            wait_until_dead(p1, Duration::from_secs(10)),
            "first assigned child {p1} died with the job"
        );
        assert!(
            wait_until_dead(p2, Duration::from_secs(10)),
            "second assigned child {p2} died with the job"
        );
    }
}
