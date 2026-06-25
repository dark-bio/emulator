//! Tie a spawned child to the current process so it can't be orphaned: when
//! this process exits — including a hard kill (`kill -9`, `TerminateProcess`,
//! force-quit, OOM) where no in-process cleanup runs — the child dies with it.
//!
//! There is no portable primitive, so each OS uses its own mechanism. Only the
//! three below are implemented; any other target fails to compile rather than
//! silently leaving children unprotected.
//!
//!   - **Linux**   — `PR_SET_PDEATHSIG=SIGKILL`, armed pre-spawn via `pre_exec`.
//!   - **Windows** — a Job Object flagged `KILL_ON_JOB_CLOSE`, armed post-spawn.
//!   - **macOS**   — a "death pipe": this process holds the write end open for
//!     its lifetime while a stock `/bin/sh` blocks on the read end, so the EOF on
//!     exit fires the kill. (macOS has no parent-death signal, so it needs a
//!     separate process.)
//!
//! Build a [`std::process::Command`], wrap it with [`guard`], then
//! [`GuardedCommand::spawn`] it in place of [`Command::spawn`].

use std::io;
use std::process::{Child, Command};

/// A `Command` whose spawned child will be tied to the current process. Build
/// the command as usual, hand it to [`guard`], then call [`GuardedCommand::spawn`].
pub(crate) struct GuardedCommand {
    inner: Command,
}

/// Wrap a fully-configured command so its child gets orphan protection on spawn.
pub(crate) fn guard(command: Command) -> GuardedCommand {
    GuardedCommand { inner: command }
}

impl GuardedCommand {
    /// Spawn the command with orphan protection armed around it. Mirrors
    /// [`Command::spawn`], including its error type. If protection can't be armed
    /// the child is killed and reaped before returning the error, so a failed
    /// spawn never leaves an unguarded child running.
    pub(crate) fn spawn(mut self) -> io::Result<Child> {
        arm_pre_spawn(&mut self.inner);
        let mut child = self.inner.spawn()?;
        if let Err(e) = arm_post_spawn(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok(child)
    }
}

/// Linux: attach `PR_SET_PDEATHSIG=SIGKILL` so the kernel kills the child the
/// moment this process dies — for any reason, including SIGKILL or a panic,
/// neither of which run `Drop`. Other platforms have no pre-exec hook and arm
/// their equivalents in [`arm_post_spawn`].
#[cfg(target_os = "linux")]
fn arm_pre_spawn(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec runs in the forked child before exec; only the
    // async-signal-safe prctl is called.
    unsafe {
        cmd.pre_exec(|| {
            let rc = libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
            if rc == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(any(target_os = "macos", windows))]
fn arm_pre_spawn(_cmd: &mut Command) {}

/// Linux armed protection pre-spawn (PDEATHSIG), so there's nothing to do here.
#[cfg(target_os = "linux")]
fn arm_post_spawn(_child: &Child) -> io::Result<()> {
    Ok(())
}

/// Windows: put the child in a Job Object flagged `KILL_ON_JOB_CLOSE` and keep
/// the job handle open for this process's lifetime. When this process ends —
/// cleanly, via `TerminateProcess`, or by crashing — the OS closes its handles,
/// the job's last reference goes with them, and Windows terminates the child.
/// A sub-millisecond window between spawn and assignment can still orphan the
/// child if this process dies inside it.
#[cfg(windows)]
fn arm_post_spawn(child: &Child) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // SAFETY: standard Win32 Job Object setup. The info struct is fully
    // initialized (zeroed + one field) and pointers are valid for each call.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let err = io::Error::last_os_error();
            CloseHandle(job);
            return Err(err);
        }

        if AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) == 0 {
            let err = io::Error::last_os_error();
            CloseHandle(job);
            return Err(err);
        }

        // Intentionally never CloseHandle(job) on success: the open handle is
        // exactly what pins the job's lifetime to this process.
        Ok(())
    }
}

/// macOS: no parent-death signal exists, and `kill -9` runs no in-process
/// cleanup — so the child's fate is tied to a *death pipe* watched by a separate
/// process. This process keeps the write end open for its lifetime while a stock
/// `/bin/sh` blocks on the read end. However this process dies (clean exit,
/// panic, SIGKILL, OOM), the kernel closes its fds, the read hits EOF, and the
/// shell SIGKILLs the child. No polling and no re-exec of this binary.
#[cfg(target_os = "macos")]
fn arm_post_spawn(child: &Child) -> io::Result<()> {
    use std::process::Stdio;

    let (reader, writer) = std::io::pipe()?;

    // `read` returns failure at EOF, so `||` fires the kill. SIGKILL of a
    // since-exited child is a harmless no-op; the only residual risk is the brief
    // window in which the child's PID could be recycled before the kill fires.
    // `$0` is the label, `$1` the child PID.
    Command::new("/bin/sh")
        .args(["-c", r#"read _ || kill -9 "$1""#, "orphan-watchdog"])
        .arg(child.id().to_string())
        .stdin(Stdio::from(reader))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // Std pipe ends are close-on-exec, so the shell never inherited the write
    // end — only this process holds it. Leak it so it stays open until exit; that
    // EOF is the watchdog's signal. (`reader` moved into the child's stdin and
    // our copy is already closed.)
    std::mem::forget(writer);
    Ok(())
}
