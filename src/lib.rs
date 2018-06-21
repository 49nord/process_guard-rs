//! Process guard
//!
//! A process guard takes ownership of a `process::Child` and gently or forcefully kills it upon,
//! prevent the process from running on. Example:
//!
//! ```rust
//! use process_guard::ProcessGuard;
//! use std::process;
//!
//! fn insomnia() {
//!     let pg = ProcessGuard::spawn(process::Command::new("sleep").arg("120"));
//!
//!     // a two-minute sleep process has been started, which will be killed as soon as this
//!     // function returns
//! }
//! ```

#[macro_use]
extern crate log;
extern crate nix;
extern crate ticktock;

use std::{io, process, time};

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
#[derive(Debug)]
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
    pub fn take(&mut self) -> Option<process::Child> {
        self.child.take()
    }

    /// Spawns a command
    ///
    /// Equivalent to calling `cmd.spawn()`, followed by `new`.
    pub fn spawn(cmd: &mut process::Command) -> io::Result<ProcessGuard> {
        let child = cmd.spawn()?;
        Ok(unsafe { Self::new(child, None) })
    }

    /// Spawns a command with a grace timeout
    ///
    /// Equivalent to calling `cmd.spawn()`, followed by `new`.
    pub fn spawn_graceful(
        cmd: &mut process::Command,
        grace_time: time::Duration,
    ) -> io::Result<ProcessGuard> {
        let child = cmd.spawn()?;
        Ok(unsafe { Self::new(child, Some(grace_time)) })
    }

    /// Shut the process down
    ///
    /// Calling `shutdown()` on a guard whose process has already exited is a no-op. Note that
    /// a process whose shutdown failed is also considered shutdown, even though it might still be
    /// running.
    pub fn shutdown(&mut self) -> io::Result<Option<process::ExitStatus>> {
        // remove child from parent struct to satisfy the borrow checker
        let mut child_opt = self.take();

        if let Some(ref mut child) = child_opt {
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
                ).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

                // until we reach `grace_time`, try to reap the child in POLL_INTERVAL intervals
                for _ in ticktock::clock::Clock::new(POLL_INTERVAL)
                    .rel_iter()
                    .take_while(|(_, t)| t <= &grace_time)
                {
                    match io_retry(|| child.try_wait()) {
                        // process did not exit yet, keep polling
                        Ok(None) => continue,
                        // process did exit, we are done
                        Ok(Some(status)) => {
                            return Ok(Some(status));
                        }
                        // error occured - we won't keep trying to wait, but will make another
                        // effort using SIGKILL
                        Err(_) => break,
                    }
                }
            }

            // SIGTERM was either not requested or unsuccessful, proceed with SIGKILL
            // if SIGKILL fails, we keep the child around, but do not wait on it
            io_retry(|| child.kill())?;

            // now wait is bound work. if it fails, we give up
            Ok(Some(io_retry(|| child.wait())?))
        } else {
            Ok(None)
        }
    }
}

impl Drop for ProcessGuard {
    #[inline]
    fn drop(&mut self) {
        let pid = self.child.as_ref().map(|c| c.id()).unwrap_or(0);

        if let Err(e) = self.shutdown() {
            warn!("Could not cleanly kill PID {}: {:?}", pid, e);
        }
    }
}
