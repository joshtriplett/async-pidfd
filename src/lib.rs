//! Process file descriptors (`pidfd`) for Linux
//! ============================================
//!
//! Process file descriptors (`pidfd`) provide a race-free way to manage processes
//! on Linux, maintaining a persistent reference to a process using a file
//! descriptor rather than a numeric process ID (PID) that could be reused after
//! the process exits.
//!
//! This crate only works on Linux; if you need support for other platforms, or for
//! older Linux kernels, see
//! [async-process](https://crates.io/crates/async-process).
//!
//! `async-pidfd` provides Rust support for pidfd, and supports managing processes
//! both synchronously (via the `PidFd` type) and asynchronously (via the
//! `AsyncPidFd` type).
//!
//! Sync - `PidFd`
//! --------------
//!
//! The `PidFd` type manages processes synchronously.  Use `PidFd::from_pid` to
//! construct a `PidFd` from a process ID, such as from
//! [`Child::id`](https://doc.rust-lang.org/std/process/struct.Child.html#method.id)
//! in the standard library. (Note that the portable `Child::id` function returns
//! process IDs as `u32`, rather than as a `libc::pid_t`, necessitating a cast.)
//!
//! ```rust
//! use std::os::unix::process::ExitStatusExt;
//! use std::process::{Command, ExitStatus};
//!
//! use async_pidfd::PidFd;
//!
//! fn main() -> std::io::Result<()> {
//!     let child = Command::new("/bin/true").spawn()?;
//!     let pidfd = PidFd::from_pid(child.id() as libc::pid_t)?;
//!     let status = pidfd.wait()?.status();
//!     assert_eq!(status.code(), Some(0));
//!
//!     let child = Command::new("/bin/sh").arg("-c").arg("kill -9 $$").spawn()?;
//!     let pidfd = PidFd::from_pid(child.id() as libc::pid_t)?;
//!     let status = pidfd.wait()?.status();
//!     assert_eq!(status.signal(), Some(9));
//!
//!     Ok(())
//! }
//! ```
//!
//! `PidFd::wait` returns information about an exited process via the `ExitInfo`
//! structure. `ExitInfo` includes a `libc::siginfo_t` indicating how the process
//! exited (including the exit code if it exited normally, or the signal if it was
//! killed by a signal), and a `libc::rusage` describing the resource usage of the
//! process and its children. `libc::siginfo_t` has complex semantics; to get a
//! [`std::process::ExitStatus`](https://doc.rust-lang.org/std/process/struct.ExitStatus.html)
//! instead, you can call `.status()` on an `ExitInfo`.
//!
//! Note that while opening the PID for an arbitrary process can potentially race
//! with the exit of that process, opening the PID for a child process that you
//! have not yet waited on is safe, as the process ID will not get reused until you
//! wait on the process (or block `SIGCHLD`).
//!
//! If you only want to use the synchronous `PidFd` type, you can use `async-pidfd`
//! with `default-features = false` in `Cargo.toml` to remove async-related
//! dependencies.
//!
//! Async - `AsyncPidFd`
//! --------------------
//!
//! The `AsyncPidFd` type manages processes asynchronously, based on the
//! [`async-io`](https://docs.rs/async-io/) crate by Stjepan Glavina. `async-io`
//! provides an `Async` wrapper that makes it easy to turn any synchronous type
//! based on a file descriptor into an asynchronous type; the resulting
//! asynchronous code uses `epoll` to wait for all the file descriptors
//! concurrently.
//!
//! `AsyncPidFd` wraps an `Async<PidFd>` and provides the same API as `PidFd`, but
//! with an `async` version of the `wait` function.
//!
//! ```rust
//! use std::os::unix::process::ExitStatusExt;
//! use std::process::{Command, ExitStatus};
//!
//! use async_pidfd::AsyncPidFd;
//! use futures_lite::future;
//!
//! async fn async_spawn_and_status(cmd: &mut Command) -> std::io::Result<ExitStatus> {
//!     let child = cmd.spawn()?;
//!     let pidfd = AsyncPidFd::from_pid(child.id() as libc::pid_t)?;
//!     Ok(pidfd.wait().await?.status())
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     future::block_on(async {
//!         let (status1, status2) = future::try_join(
//!             async_spawn_and_status(&mut Command::new("/bin/true")),
//!             async_spawn_and_status(&mut Command::new("/bin/false")),
//!         )
//!         .await?;
//!         assert_eq!(status1.code(), Some(0));
//!         assert_eq!(status2.code(), Some(1));
//!         Ok(())
//!     })
//! }
//! ```
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

fn syscall_result(ret: libc::c_long) -> io::Result<libc::c_long> {
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

// pidfd_open sets `O_CLOEXEC` by default, without requiring a flag.
fn pidfd_open(pid: libc::pid_t, flags: libc::c_uint) -> io::Result<RawFd> {
    let ret = syscall_result(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, flags) })?;
    Ok(ret as RawFd)
}

// Use the raw waitid syscall, which additionally provides the rusage argument.
fn waitid_pidfd(pidfd: RawFd) -> io::Result<(libc::siginfo_t, libc::rusage)> {
    let mut siginfo = MaybeUninit::uninit();
    let mut rusage = MaybeUninit::uninit();
    unsafe {
        syscall_result(libc::syscall(
            libc::SYS_waitid,
            libc::P_PIDFD,
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
        let si_status = unsafe { self.siginfo.si_status() };
        let raw_status = if self.siginfo.si_code == libc::CLD_EXITED {
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
