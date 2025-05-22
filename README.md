Process file descriptors (`pidfd`) for Linux
============================================

Process file descriptors (`pidfd`) provide a race-free way to manage processes
on Linux, maintaining a persistent reference to a process using a file
descriptor rather than a numeric process ID (PID) that could be reused after
the process exits.

This crate only works on Linux; if you need support for other platforms, or for
older Linux kernels, see
[async-process](https://crates.io/crates/async-process).

`async-pidfd` provides Rust support for pidfd, and supports managing processes
both synchronously (via the `PidFd` type) and asynchronously (via the
`AsyncPidFd` type).

Sync - `PidFd`
--------------

The `PidFd` type manages processes synchronously.  Use `PidFd::from_pid` to
construct a `PidFd` from a process ID, such as from
[`Child::id`](https://doc.rust-lang.org/std/process/struct.Child.html#method.id)
in the standard library. (Note that the portable `Child::id` function returns
process IDs as `u32`, rather than as a `libc::pid_t`, necessitating a cast.)

```rust
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};

use async_pidfd::PidFd;

fn main() -> std::io::Result<()> {
    let child = Command::new("/bin/true").spawn()?;
    let pidfd = PidFd::from_pid(child.id() as libc::pid_t)?;
    let status = pidfd.wait()?.status();
    assert_eq!(status.code(), Some(0));

    let child = Command::new("/bin/sh").arg("-c").arg("kill -9 $$").spawn()?;
    let pidfd = PidFd::from_pid(child.id() as libc::pid_t)?;
    let status = pidfd.wait()?.status();
    assert_eq!(status.signal(), Some(9));

    Ok(())
}
```

`PidFd::wait` returns information about an exited process via the `ExitInfo`
structure. `ExitInfo` includes a `libc::siginfo_t` indicating how the process
exited (including the exit code if it exited normally, or the signal if it was
killed by a signal), and a `libc::rusage` describing the resource usage of the
process and its children. `libc::siginfo_t` has complex semantics; to get a
[`std::process::ExitStatus`](https://doc.rust-lang.org/std/process/struct.ExitStatus.html)
instead, you can call `.status()` on an `ExitInfo`.

Note that while opening the PID for an arbitrary process can potentially race
with the exit of that process, opening the PID for a child process that you
have not yet waited on is safe, as the process ID will not get reused until you
wait on the process (or block `SIGCHLD`).

If you only want to use the synchronous `PidFd` type, you can use `async-pidfd`
with `default-features = false` in `Cargo.toml` to remove async-related
dependencies.

Async - `AsyncPidFd`
--------------------

The `AsyncPidFd` type manages processes asynchronously, based on the
[`async-io`](https://docs.rs/async-io/) crate. `async-io` provides an `Async`
wrapper that makes it easy to turn any synchronous type based on a file
descriptor into an asynchronous type; the resulting asynchronous code uses
`epoll` to wait for all the file descriptors concurrently.

`AsyncPidFd` wraps an `Async<PidFd>` and provides the same API as `PidFd`, but
with an `async` version of the `wait` function.

```rust
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};

use async_pidfd::AsyncPidFd;
use futures_lite::future;

async fn async_spawn_and_status(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    let child = cmd.spawn()?;
    let pidfd = AsyncPidFd::from_pid(child.id() as libc::pid_t)?;
    Ok(pidfd.wait().await?.status())
}

fn main() -> std::io::Result<()> {
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
```
