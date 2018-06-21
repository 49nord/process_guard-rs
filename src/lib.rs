extern crate nix;
extern crate ticktock;

use std::{io, mem, process, time};

/// Retry an IO operation if it returns with `EINTR`.
#[inline]
fn io_retry<T, F: FnMut() -> io::Result<T>>(mut f: F) -> io::Result<T> {
    // FIXME: do we really need/want `FnMut` here?
    loop {
        match f() {
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r @ _ => break r,
        }
    }
}

/// Interval used when polling, waiting for a process to exit
const POLL_INTERVAL: time::Duration = time::Duration::from_millis(100);

/// Protects a process from becoming an orphan or zombie by killing it when the guard is dropped
pub struct ProcessGuard {
    /// Child process. The process might be removed prematurely, in which case we do not kill
    /// anything
    child: Option<process::Child>,

    /// An optional grace time. If set, sends a `SIGTERM` and waits up to `grace_time` before
    /// sending `SIGKILL`.
    grace_time: Option<time::Duration>,
}

impl ProcessGuard {
    /// Create a new child process
    ///
    /// # unsafe
    ///
    /// It is unsafe to put any arbitrary child process into a process guard, mainly because the
    /// guard relies on the child not having been waited on beforehand. Otherwise, it cannot be
    /// guaranteed that the child process has not exited and its PID been reused, potentially
    /// killing an innocent bystander process on `Drop`.
    pub unsafe fn new(child: process::Child, grace_time: Option<time::Duration>) -> ProcessGuard {
        ProcessGuard {
            child: Some(child),
            grace_time,
        }
    }

    /// Retrieves the child process from the process guard
    pub fn into_inner(mut self) -> Option<process::Child> {
        let mut child = None;

        mem::swap(&mut self.child, &mut child);
        child
    }

    /// Spawns a command
    ///
    /// Equivalent to calling `cmd.spawn()`, followed by `new`.
    pub fn spawn(mut cmd: process::Command) -> io::Result<ProcessGuard> {
        let child = cmd.spawn()?;
        Ok(unsafe { Self::new(child, None) })
    }

    /// Spawns a command with a grace timeout
    ///
    /// Equivalent to calling `cmd.spawn()`, followed by `new`.
    pub fn spawn_graceful(
        mut cmd: process::Command,
        grace_time: time::Duration,
    ) -> io::Result<ProcessGuard> {
        let child = cmd.spawn()?;
        Ok(unsafe { Self::new(child, Some(grace_time)) })
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            // NOTE: we assume that it is impossible for a child's PID to be reused before it has
            //       been reaped, i.e. since we are the first to call `wait` on it, we should never
            //       kill the wrong process

            // upon drop, we terminate, then kill the child
            if let Some(grace_time) = self.grace_time {
                // send SIGKILL, we ignore the result of the kill call, we cannot do anything about
                // any Err. According to the kill(2) manpage, EINTR is not possible when calling
                // kill().
                nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(child.id() as i32),
                    nix::sys::signal::Signal::SIGTERM,
                ).ok();

                // until we reach `grace_time`, try to reap the child in POLL_INTERVAL intervals
                for _ in ticktock::clock::Clock::new(POLL_INTERVAL)
                    .rel_iter()
                    .take_while(|(_, t)| t <= &grace_time)
                {
                    match io_retry(|| child.try_wait()) {
                        // process did not exit yet, keep polling
                        Ok(None) => continue,
                        // process did exit, we are done
                        Ok(_) => return,
                        // error occured - we won't keep trying to wait, but will make another
                        // effort using SIGKILL
                        Err(_) => break,
                    }
                }
            }

            // SIGTERM was either not requested or unsuccessful, proceed with SIGKILL
            io_retry(|| child.kill()).ok();

            // now wait is bound work. if it fails, we give up
            io_retry(|| child.wait()).ok();
        }
    }
}
