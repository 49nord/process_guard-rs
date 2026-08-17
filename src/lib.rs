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

use std::{io, process, thread, time};

/// Retry an IO operation if it returns with `EINTR`.
#[inline]
fn io_retry<T, F: FnMut() -> io::Result<T>>(mut f: F) -> io::Result<T> {
    // FIXME: do we really need/want `FnMut` here?
    loop {
        match f() {
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            r => break r,
        }
    }
}

/// Interval used when polling, waiting for a process to exit
const POLL_INTERVAL: time::Duration = time::Duration::from_millis(100);

/// Identifies a signal that can be sent to a guarded process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    /// Requests that a process reload its configuration or terminate.
    Hangup,
    /// Requests an interrupt-driven shutdown.
    Interrupt,
    /// Forces immediate termination.
    Kill,
    /// Requests termination and a core dump.
    Quit,
    /// Requests an orderly shutdown.
    Terminate,
}

impl Signal {
    /// Returns the platform signal represented by this value.
    fn as_nix(self) -> nix::sys::signal::Signal {
        match self {
            Self::Hangup => nix::sys::signal::Signal::SIGHUP,
            Self::Interrupt => nix::sys::signal::Signal::SIGINT,
            Self::Kill => nix::sys::signal::Signal::SIGKILL,
            Self::Quit => nix::sys::signal::Signal::SIGQUIT,
            Self::Terminate => nix::sys::signal::Signal::SIGTERM,
        }
    }
}

/// Configures how a guarded process is shut down.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownPolicy {
    /// Forces immediate termination with [`Signal::Kill`].
    Kill,
    /// Requests graceful shutdown before forcing termination.
    Graceful {
        /// Signal used to request graceful shutdown.
        signal: Signal,
        /// Maximum time allowed for graceful shutdown.
        grace_time: time::Duration,
    },
}

/// Protects a process from becoming an orphan or zombie by killing it when the guard is dropped
#[derive(Debug)]
pub struct ProcessGuard {
    /// Child process. The process might be removed prematurely, in which case we do not kill
    /// anything
    child: Option<process::Child>,

    /// Policy used when shutting down the process.
    policy: ShutdownPolicy,
}

impl ProcessGuard {
    /// Creates a guard for an existing child process.
    pub fn new(child: process::Child, grace_time: Option<time::Duration>) -> ProcessGuard {
        let policy = grace_time.map_or(ShutdownPolicy::Kill, |grace_time| {
            ShutdownPolicy::Graceful {
                signal: Signal::Terminate,
                grace_time,
            }
        });
        Self::with_policy(child, policy)
    }

    /// Creates a guard with an explicit shutdown policy.
    pub fn with_policy(child: process::Child, policy: ShutdownPolicy) -> ProcessGuard {
        ProcessGuard {
            child: Some(child),
            policy,
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
        Self::spawn_with_policy(cmd, ShutdownPolicy::Kill)
    }

    /// Spawns a command with a grace timeout
    ///
    /// Equivalent to calling `cmd.spawn()`, followed by `new`.
    pub fn spawn_graceful(
        cmd: &mut process::Command,
        grace_time: time::Duration,
    ) -> io::Result<ProcessGuard> {
        Self::spawn_with_policy(
            cmd,
            ShutdownPolicy::Graceful {
                signal: Signal::Terminate,
                grace_time,
            },
        )
    }

    /// Spawns a command with an explicit shutdown policy.
    pub fn spawn_with_policy(
        cmd: &mut process::Command,
        policy: ShutdownPolicy,
    ) -> io::Result<ProcessGuard> {
        let child = cmd.spawn()?;
        Ok(Self::with_policy(child, policy))
    }

    /// Shuts the process down and reaps it.
    ///
    /// Returns `None` if the guard no longer owns a process. If shutdown fails, the process remains
    /// guarded so the operation can be retried.
    pub fn shutdown(&mut self) -> io::Result<Option<process::ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };

        let result = shutdown_child(child, self.policy);
        match result {
            Ok(status) => {
                self.child.take();
                Ok(Some(status))
            }
            Err(error) => Err(error),
        }
    }
}

/// Shuts down and reaps an owned child process.
fn shutdown_child(
    child: &mut process::Child,
    policy: ShutdownPolicy,
) -> io::Result<process::ExitStatus> {
    if let Some(status) = io_retry(|| child.try_wait())? {
        return Ok(status);
    }

    // An unreaped child retains its PID, so it cannot be reused between this check and wait.
    if let ShutdownPolicy::Graceful { signal, grace_time } = policy {
        let signal_result = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            signal.as_nix(),
        )
        .map_err(io::Error::from);

        if signal_result.is_ok() {
            let started = time::Instant::now();
            loop {
                match io_retry(|| child.try_wait()) {
                    Ok(None) => {}
                    Ok(Some(status)) => return Ok(status),
                    Err(_) => break,
                }

                let remaining = grace_time.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    break;
                }
                thread::sleep(POLL_INTERVAL.min(remaining));
            }
        } else if let Ok(Some(status)) = io_retry(|| child.try_wait()) {
            return Ok(status);
        }
    }

    match io_retry(|| child.kill()) {
        Ok(()) => io_retry(|| child.wait()),
        Err(kill_error) => match io_retry(|| child.try_wait()) {
            Ok(Some(status)) => Ok(status),
            Ok(None) | Err(_) => Err(kill_error),
        },
    }
}

impl Drop for ProcessGuard {
    #[inline]
    fn drop(&mut self) {
        let pid = self.child.as_ref().map(|c| c.id()).unwrap_or(0);

        if let Err(e) = self.shutdown() {
            log::warn!("Could not cleanly kill PID {}: {:?}", pid, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io, io::Read, process, time};

    use super::{ProcessGuard, ShutdownPolicy, Signal};

    #[test]
    fn shutdown_reaps_a_completed_child() -> io::Result<()> {
        let mut command = process::Command::new("true");
        command.stdout(process::Stdio::piped());
        let mut child = command.spawn()?;

        let mut output = Vec::new();
        child
            .stdout
            .take()
            .expect("stdout was configured to be piped")
            .read_to_end(&mut output)?;

        let mut guard = ProcessGuard::new(child, Some(time::Duration::from_secs(1)));
        let status = guard
            .shutdown()?
            .expect("the guard still owns the completed child");

        assert!(status.success());
        assert!(guard.shutdown()?.is_none());
        Ok(())
    }

    #[test]
    fn graceful_shutdown_accepts_a_custom_signal() -> io::Result<()> {
        let mut command = process::Command::new("sh");
        command
            .arg("-c")
            .arg("trap 'exit 42' INT; printf ready; while :; do :; done")
            .stdout(process::Stdio::piped());
        let mut child = command.spawn()?;

        let mut ready = [0; 5];
        child
            .stdout
            .take()
            .expect("stdout was configured to be piped")
            .read_exact(&mut ready)?;
        assert_eq!(&ready, b"ready");

        let mut guard = ProcessGuard::with_policy(
            child,
            ShutdownPolicy::Graceful {
                signal: Signal::Interrupt,
                grace_time: time::Duration::from_secs(1),
            },
        );
        let status = guard
            .shutdown()?
            .expect("the guard still owns the running child");

        assert_eq!(status.code(), Some(42));
        Ok(())
    }

    #[test]
    fn graceful_shutdown_handles_fast_exit_races() -> io::Result<()> {
        for _ in 0..100 {
            let mut command = process::Command::new("true");
            let mut guard = ProcessGuard::spawn_graceful(&mut command, time::Duration::ZERO)?;

            assert!(guard.shutdown()?.is_some());
        }
        Ok(())
    }

    #[test]
    fn shutdown_kills_and_reaps_a_running_child() -> io::Result<()> {
        let mut command = process::Command::new("sleep");
        command.arg("60");
        let mut guard = ProcessGuard::spawn(&mut command)?;

        let status = guard
            .shutdown()?
            .expect("the guard still owns the running child");

        assert!(!status.success());
        assert!(guard.shutdown()?.is_none());
        Ok(())
    }
}
