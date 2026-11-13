//! Starting a long-lived background child away from the terminal that launched
//! it.

use std::io;
use std::process::{Child, Command};

/// Start `command` in its own session, away from the launching terminal:
/// closing that terminal or pressing Ctrl-C in it must not stop the child.
/// Set the child's stdio before calling this; the caller's own standard
/// handles never reach it.
pub(crate) fn spawn_detached(command: &mut Command) -> io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no Rust state.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn()
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
        };
        // A hidden console of its own (its children share it, so no window
        // opens), and its own process group, so console events of the
        // launching terminal never reach it.
        let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
        // CreateProcess inherits every inheritable handle, and this process's
        // own standard handles usually are (a shell hands them down that way).
        // The child would then hold the caller's stdout pipe open for its
        // lifetime, so `rayline ... start | ...` never saw EOF. The child's
        // own stdio is set by the caller, so keep ours out of it while it
        // spawns.
        let _quiet = NonInheritableStdHandles::new();
        // Leave the terminal's job too, where the job allows it: a job that
        // kills its processes on close would otherwise take the child with it.
        match command
            .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
            .spawn()
        {
            Ok(child) => Ok(child),
            Err(error)
                if error.raw_os_error()
                    == Some(windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32) =>
            {
                command.creation_flags(flags).spawn()
            }
            Err(error) => Err(error),
        }
    }
}

/// Clears the inherit flag on this process's standard handles while it lives
/// and puts the original flags back when dropped.
#[cfg(windows)]
struct NonInheritableStdHandles {
    saved: Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>,
}

#[cfg(windows)]
impl NonInheritableStdHandles {
    fn new() -> Self {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{
            GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
        };
        let handles = [
            std::io::stdin().as_raw_handle(),
            std::io::stdout().as_raw_handle(),
            std::io::stderr().as_raw_handle(),
        ];
        let mut saved: Vec<(HANDLE, u32)> = Vec::new();
        for handle in handles {
            let handle = handle as HANDLE;
            if handle.is_null()
                || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
                || saved.iter().any(|(seen, _)| *seen == handle)
            {
                continue;
            }
            let mut flags = 0u32;
            // SAFETY: `handle` is one of this process's standard handles and
            // `flags` is a valid out pointer.
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0
                || flags & HANDLE_FLAG_INHERIT == 0
            {
                continue;
            }
            // SAFETY: as above; only the inherit bit changes.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } != 0 {
                saved.push((handle, flags));
            }
        }
        Self { saved }
    }
}

#[cfg(windows)]
impl Drop for NonInheritableStdHandles {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        for (handle, flags) in &self.saved {
            // SAFETY: restores the inherit bit this guard cleared on a handle
            // the process still owns.
            unsafe {
                SetHandleInformation(*handle, HANDLE_FLAG_INHERIT, flags & HANDLE_FLAG_INHERIT)
            };
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::spawn_detached;
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const HELPER_ENV: &str = "RAYLINE_TEST_DETACH_HELPER";

    /// Run by `detached_child_does_not_hold_the_callers_stdout`, in a child
    /// test process whose stdout is a pipe: starts a long-lived detached
    /// child, prints its pid and exits. Does nothing in a normal test run.
    #[test]
    // The child must outlive this process; the calling test kills it.
    #[allow(clippy::zombie_processes)]
    fn detach_helper_starts_a_long_lived_child() {
        if std::env::var_os(HELPER_ENV).is_none() {
            return;
        }
        let mut command = Command::new("ping");
        command
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = spawn_detached(&mut command).expect("spawn the detached child");
        println!("detached-pid={}", child.id());
    }

    #[test]
    fn detached_child_does_not_hold_the_callers_stdout() {
        let exe = std::env::current_exe().expect("test binary path");
        let mut helper = Command::new(exe)
            .args([
                "detach::tests::detach_helper_starts_a_long_lived_child",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the helper test process");
        let mut stdout = helper.stdout.take().expect("helper stdout");
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = Instant::now();
            let mut output = String::new();
            let read = stdout.read_to_string(&mut output);
            let _ = sender.send((read.map(|_| output), started.elapsed()));
        });
        let result = receiver.recv_timeout(Duration::from_secs(15));
        let _ = helper.wait();
        let output = match &result {
            Ok((Ok(output), _)) => output.clone(),
            _ => String::new(),
        };
        if let Some(pid) = output
            .lines()
            .find_map(|line| line.trim().strip_prefix("detached-pid="))
        {
            let _ = Command::new("taskkill")
                .args(["/PID", pid, "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let (read, elapsed) = result.expect(
            "the helper's stdout reached EOF only after the detached child exited: \
             the child inherited the caller's stdout",
        );
        let output = read.expect("read the helper's stdout");
        assert!(
            output.contains("detached-pid="),
            "the helper did not start the detached child: {output}"
        );
        assert!(elapsed < Duration::from_secs(15), "took {elapsed:?}");
    }
}
