//! Process file descriptors (`pidfd`) for Linux.
#![forbid(missing_docs)]

#[cfg(not(target_os = "linux"))]
compile_error!("pidfd only works on Linux");

use std::io;
use std::mem::MaybeUninit;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

#[cfg(feature = "async")]
use async_io::Async;

const SYS_PIDFD_OPEN: libc::c_long = 434;
const P_PIDFD: libc::idtype_t = 3;
const CLD_EXITED: libc::c_int = 1;

fn syscall_result(ret: libc::c_long) -> io::Result<libc::c_long> {
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

// pidfd_open sets `O_CLOEXEC` by default, without requiring a flag.
fn pidfd_open(pid: libc::pid_t, flags: libc::c_uint) -> io::Result<RawFd> {
    let ret = syscall_result(unsafe { libc::syscall(SYS_PIDFD_OPEN, pid, flags) })?;
    Ok(ret as RawFd)
}

// Use the raw waitid syscall, which additionally provides the rusage argument.
fn waitid_pidfd(pidfd: RawFd) -> io::Result<(libc::siginfo_t, libc::rusage)> {
    let mut siginfo = MaybeUninit::uninit();
    let mut rusage = MaybeUninit::uninit();
    unsafe {
        syscall_result(libc::syscall(
            libc::SYS_waitid,
            P_PIDFD,
            pidfd,
            siginfo.as_mut_ptr(),
            libc::WEXITED,
            rusage.as_mut_ptr(),
        ))?;
        Ok((siginfo.assume_init(), rusage.assume_init()))
    }
}

/// A process file descriptor.
pub struct PidFd(RawFd);

impl PidFd {
    /// Create a process file descriptor from a PID.
    ///
    /// You can get a process ID from `std::process::Child` by calling `Child::id`.
    ///
    /// As long as this process has not yet waited on the child process, and has not blocked
    /// `SIGCHLD`, the process ID will not get reused, so it does not matter if the child process
    /// has already exited.
    pub fn from_pid(pid: libc::pid_t) -> io::Result<Self> {
        Ok(Self(pidfd_open(pid, 0)?))
    }

    /// Wait for the process to complete.
    pub fn wait(&self) -> io::Result<ExitInfo> {
        let (siginfo, rusage) = waitid_pidfd(self.0)?;
        Ok(ExitInfo { siginfo, rusage })
    }
}

impl Drop for PidFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

impl AsRawFd for PidFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

// Accessor function for siginfo.si_status, until a released version of the libc crate provides it.
mod siginfo_accessor {
    #[repr(C)]
    #[derive(Copy, Clone)]
    struct sifields_sigchld {
        si_pid: libc::pid_t,
        si_uid: libc::uid_t,
        si_status: libc::c_int,
    }

    #[repr(C)]
    union sifields {
        _align_pointer: *mut libc::c_void,
        sigchld: sifields_sigchld,
    }

    #[repr(C)]
    struct siginfo_sifields {
        _siginfo_base: [libc::c_int; 3],
        sifields: sifields,
    }

    pub(crate) unsafe fn siginfo_si_status(siginfo: &libc::siginfo_t) -> libc::c_int {
        (*(siginfo as *const libc::siginfo_t as *const siginfo_sifields))
            .sifields
            .sigchld
            .si_status
    }
}

/// Information about an exited process.
pub struct ExitInfo {
    /// Information about how the process exited.
    pub siginfo: libc::siginfo_t,
    /// Resource usage for the process and its children.
    pub rusage: libc::rusage,
}

impl ExitInfo {
    /// Returns the exit status of the process.
    pub fn status(&self) -> ExitStatus {
        let si_status = unsafe { siginfo_accessor::siginfo_si_status(&self.siginfo) };
        let raw_status = if self.siginfo.si_code == CLD_EXITED {
            si_status << 8
        } else {
            si_status
        };
        ExitStatus::from_raw(raw_status)
    }
}

/// Asynchronous version of `PidFd`.
#[cfg(feature = "async")]
pub struct AsyncPidFd(Async<PidFd>);

#[cfg(feature = "async")]
impl AsyncPidFd {
    /// Create a process file descriptor from a PID.
    ///
    /// You can get a process ID from `std::process::Child` by calling `Child::id`.
    ///
    /// As long as this process has not yet waited on the child process, and has not blocked
    /// `SIGCHLD`, the process ID will not get reused, so it does not matter if the child process
    /// has already exited.
    pub fn from_pid(pid: libc::pid_t) -> io::Result<Self> {
        Ok(Self(Async::new(PidFd::from_pid(pid)?)?))
    }

    /// Wait for the process to complete.
    pub async fn wait(&self) -> io::Result<ExitInfo> {
        self.0.readable().await?;
        self.0.get_ref().wait()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::process::Command;

    fn spawn_and_status(cmd: &mut Command) -> io::Result<ExitStatus> {
        let child = cmd.spawn()?;
        let pidfd = PidFd::from_pid(child.id() as libc::pid_t)?;
        Ok(pidfd.wait()?.status())
    }

    #[test]
    fn test() -> std::io::Result<()> {
        let status = spawn_and_status(&mut Command::new("/bin/true"))?;
        assert_eq!(status.code(), Some(0));
        assert_eq!(status.signal(), None);
        let status = spawn_and_status(&mut Command::new("/bin/false"))?;
        assert_eq!(status.code(), Some(1));
        assert_eq!(status.signal(), None);
        let status = spawn_and_status(Command::new("/bin/sh").arg("-c").arg("kill -9 $$"))?;
        assert_eq!(status.code(), None);
        assert_eq!(status.signal(), Some(9));
        Ok(())
    }

    fn assert_echild(ret: io::Result<ExitInfo>) {
        if let Err(e) = ret {
            assert_eq!(e.raw_os_error(), Some(libc::ECHILD));
        } else {
            panic!("Expected an error!");
        }
    }

    #[test]
    fn test_wait_twice() -> std::io::Result<()> {
        let child = Command::new("/bin/true").spawn()?;
        let pidfd = PidFd::from_pid(child.id() as libc::pid_t)?;
        let status = pidfd.wait()?.status();
        assert!(status.success());
        let ret = pidfd.wait();
        assert_echild(ret);
        Ok(())
    }

    #[cfg(feature = "async")]
    async fn async_spawn_and_status(cmd: &mut Command) -> io::Result<ExitStatus> {
        let child = cmd.spawn()?;
        let pidfd = AsyncPidFd::from_pid(child.id() as libc::pid_t)?;
        Ok(pidfd.wait().await?.status())
    }

    #[cfg(feature = "async")]
    #[test]
    fn test_async() -> std::io::Result<()> {
        use futures_lite::future;
        future::block_on(async {
            let (status1, status2) = future::try_join(
                async_spawn_and_status(&mut Command::new("/bin/true")),
                async_spawn_and_status(&mut Command::new("/bin/false")),
            )
            .await?;
            assert_eq!(status1.code(), Some(0));
            assert_eq!(status2.code(), Some(1));
            Ok(())
        })
    }

    #[cfg(feature = "async")]
    #[test]
    fn test_async_concurrent() -> std::io::Result<()> {
        use futures_lite::future::{self, FutureExt};
        future::block_on(async {
            let status = async_spawn_and_status(
                Command::new("/bin/sh")
                    .arg("-c")
                    .arg("read line")
                    .stdin(std::process::Stdio::piped()),
            )
            .or(async_spawn_and_status(&mut Command::new("/bin/false")))
            .await?;
            assert_eq!(status.code(), Some(1));
            Ok(())
        })
    }

    #[cfg(feature = "async")]
    #[test]
    fn test_async_wait_twice() -> std::io::Result<()> {
        futures_lite::future::block_on(async {
            let child = Command::new("/bin/true").spawn()?;
            let pidfd = AsyncPidFd::from_pid(child.id() as libc::pid_t)?;
            let status = pidfd.wait().await?.status();
            assert!(status.success());
            let ret = pidfd.wait().await;
            assert_echild(ret);
            Ok(())
        })
    }
}
