#![doc = include_str!("../README.md")]

use std::{io, os::unix::process::CommandExt, process, thread, time};

/// Retries an I/O operation if it returns with `EINTR`.
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

/// Defines the interval used while polling for process exit.
const POLL_INTERVAL: time::Duration = time::Duration::from_millis(100);

/// Signal that can be sent to a guarded process.
pub type Signal = nix::sys::signal::Signal;

/// Configures how a guarded process is shut down.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownPolicy {
    /// Forces immediate termination with [`Signal::SIGKILL`].
    Kill,
    /// Requests graceful shutdown before forcing termination.
    Graceful {
        /// Signal sent to the direct child to request graceful shutdown.
        signal: Signal,
        /// Maximum time allowed for graceful shutdown.
        grace_time: time::Duration,
    },
    /// Requests graceful shutdown from the complete process group before forcing termination.
    GracefulProcessGroup {
        /// Signal sent to the process group to request graceful shutdown.
        signal: Signal,
        /// Maximum time allowed for graceful shutdown.
        grace_time: time::Duration,
    },
}

/// Describes a failure that prevented a guarded child from being shut down.
#[derive(Debug, thiserror::Error)]
pub enum ShutdownError {
    /// Inspecting the child's initial status failed.
    #[error("failed to inspect the child before shutdown")]
    Inspect {
        /// Underlying process error.
        #[source]
        source: io::Error,
    },
    /// Sending the forceful termination signal failed.
    #[error("failed to forcefully terminate the child")]
    Kill {
        /// Underlying signal error.
        #[source]
        source: io::Error,
    },
    /// Reaping the terminated child failed.
    #[error("failed to reap the terminated child")]
    Wait {
        /// Underlying process error.
        #[source]
        source: io::Error,
    },
}

impl From<ShutdownError> for io::Error {
    fn from(error: ShutdownError) -> Self {
        Self::other(error)
    }
}

/// Identifies the operating-system object that receives shutdown signals.
#[derive(Clone, Copy, Debug)]
enum Target {
    /// Sends signals only to the direct child.
    Child,
    /// Sends signals to a dedicated process group.
    ProcessGroup(nix::unistd::Pid),
}

/// Owns a child process and shuts it down when dropped.
#[derive(Debug)]
pub struct ProcessGuard {
    /// Direct child owned by the guard.
    child: Option<process::Child>,

    /// Policy used when shutting down the process.
    policy: ShutdownPolicy,

    /// Object that receives shutdown signals.
    target: Target,
}

impl ProcessGuard {
    /// Creates a guard for an existing child process.
    pub fn new(child: process::Child, grace_time: Option<time::Duration>) -> ProcessGuard {
        let policy = grace_time.map_or(ShutdownPolicy::Kill, |grace_time| {
            ShutdownPolicy::Graceful {
                signal: Signal::SIGTERM,
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
            target: Target::Child,
        }
    }

    /// Returns the direct child's process identifier.
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().map(process::Child::id)
    }

    /// Removes and returns the direct child without shutting it down.
    pub fn take(&mut self) -> Option<process::Child> {
        self.child.take()
    }

    /// Spawns a command with forceful shutdown.
    ///
    /// Equivalent to spawning `cmd` and calling [`ProcessGuard::new`] without a grace period.
    pub fn spawn(cmd: &mut process::Command) -> io::Result<ProcessGuard> {
        Self::spawn_with_policy(cmd, ShutdownPolicy::Kill)
    }

    /// Spawns a command with graceful `SIGTERM` shutdown.
    ///
    /// Equivalent to spawning `cmd` and calling [`ProcessGuard::new`] with `grace_time`.
    pub fn spawn_graceful(
        cmd: &mut process::Command,
        grace_time: time::Duration,
    ) -> io::Result<ProcessGuard> {
        Self::spawn_with_policy(
            cmd,
            ShutdownPolicy::Graceful {
                signal: Signal::SIGTERM,
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

    /// Spawns a command as the leader of a dedicated process group.
    ///
    /// Shutdown signals are sent to the complete process group. The returned guard owns and reaps
    /// only the direct child.
    pub fn spawn_process_group(
        cmd: &mut process::Command,
        policy: ShutdownPolicy,
    ) -> io::Result<ProcessGuard> {
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let process_group = nix::unistd::Pid::from_raw(child.id() as i32);
        Ok(ProcessGuard {
            child: Some(child),
            policy,
            target: Target::ProcessGroup(process_group),
        })
    }

    /// Sends a signal to the guarded child or process group.
    ///
    /// Returns the direct child's exit status if it had already exited. `None` means either that
    /// the signal was delivered or that the guard no longer owns a child.
    pub fn signal(&mut self, signal: Signal) -> io::Result<Option<process::ExitStatus>> {
        if matches!(self.target, Target::Child) {
            if let Some(status) = self.try_wait()? {
                return Ok(Some(status));
            }
        }

        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let result = send_signal(child, self.target, signal);
        if let Err(signal_error) = result {
            return match self.try_wait() {
                Ok(Some(status)) => Ok(Some(status)),
                Ok(None) | Err(_) => Err(signal_error),
            };
        }
        Ok(None)
    }

    /// Checks whether the direct child has exited without blocking.
    ///
    /// A completed child is reaped and removed from the guard. `None` means either that the child
    /// remains running or that the guard no longer owns one.
    pub fn try_wait(&mut self) -> io::Result<Option<process::ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = io_retry(|| child.try_wait())?;
        if status.is_some() {
            self.child.take();
        }
        Ok(status)
    }

    /// Waits for and reaps the direct child without requesting shutdown.
    ///
    /// Returns `None` if the guard no longer owns a child. Waiting for a process-group leader does
    /// not wait for other members of that group.
    pub fn wait(&mut self) -> io::Result<Option<process::ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = io_retry(|| child.wait())?;
        self.child.take();
        Ok(Some(status))
    }

    /// Shuts the process down and reaps it.
    ///
    /// Returns `None` if the guard no longer owns a process. If shutdown fails, the process remains
    /// guarded so the operation can be retried.
    pub fn shutdown(&mut self) -> Result<Option<process::ExitStatus>, ShutdownError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };

        let result = shutdown_child(child, self.policy, self.target);
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
    target: Target,
) -> Result<process::ExitStatus, ShutdownError> {
    match target {
        Target::Child => shutdown_direct_child(child, policy),
        Target::ProcessGroup(process_group) => shutdown_process_group(child, process_group, policy),
    }
}

/// Shuts down and reaps a direct child process.
fn shutdown_direct_child(
    child: &mut process::Child,
    policy: ShutdownPolicy,
) -> Result<process::ExitStatus, ShutdownError> {
    if let Some(status) =
        io_retry(|| child.try_wait()).map_err(|source| ShutdownError::Inspect { source })?
    {
        return Ok(status);
    }

    // An unreaped child retains its PID, so it cannot be reused between this check and wait.
    if let Some((signal, grace_time)) = graceful_shutdown(policy) {
        let signal_result = send_signal(child, Target::Child, signal);

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

    force_shutdown(child, Target::Child)
}

/// Shuts down a process group and reaps its direct child.
fn shutdown_process_group(
    child: &mut process::Child,
    process_group: nix::unistd::Pid,
    policy: ShutdownPolicy,
) -> Result<process::ExitStatus, ShutdownError> {
    let target = Target::ProcessGroup(process_group);
    let mut child_status = None;

    if let Some((signal, grace_time)) = graceful_shutdown(policy) {
        let graceful_target = match policy {
            ShutdownPolicy::Graceful { .. } => Target::Child,
            ShutdownPolicy::GracefulProcessGroup { .. } => target,
            ShutdownPolicy::Kill => unreachable!("graceful policy was checked above"),
        };
        let _ = send_signal(child, graceful_target, signal);
        let started = time::Instant::now();

        loop {
            if child_status.is_none() {
                match io_retry(|| child.try_wait()) {
                    Ok(status) => child_status = status,
                    Err(_) => break,
                }
            }

            let group_exists = process_group_exists(process_group)
                .map_err(|source| ShutdownError::Inspect { source })?;
            if !group_exists && let Some(status) = child_status {
                return Ok(status);
            }

            let remaining = grace_time.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(POLL_INTERVAL.min(remaining));
        }
    }

    let kill_result = send_signal(child, target, Signal::SIGKILL);
    if let Err(kill_error) = kill_result {
        let group_exists = process_group_exists(process_group).unwrap_or(true);
        if group_exists {
            return Err(ShutdownError::Kill { source: kill_error });
        }
    }

    match child_status {
        Some(status) => Ok(status),
        None => io_retry(|| child.wait()).map_err(|source| ShutdownError::Wait { source }),
    }
}

/// Returns the graceful signal and grace period configured by a policy.
fn graceful_shutdown(policy: ShutdownPolicy) -> Option<(Signal, time::Duration)> {
    match policy {
        ShutdownPolicy::Kill => None,
        ShutdownPolicy::Graceful { signal, grace_time }
        | ShutdownPolicy::GracefulProcessGroup { signal, grace_time } => Some((signal, grace_time)),
    }
}

/// Forces a direct child or process group to stop and reaps the direct child.
fn force_shutdown(
    child: &mut process::Child,
    target: Target,
) -> Result<process::ExitStatus, ShutdownError> {
    match send_signal(child, target, Signal::SIGKILL) {
        Ok(()) => io_retry(|| child.wait()).map_err(|source| ShutdownError::Wait { source }),
        Err(kill_error) => match io_retry(|| child.try_wait()) {
            Ok(Some(status)) => Ok(status),
            Ok(None) | Err(_) => Err(ShutdownError::Kill { source: kill_error }),
        },
    }
}

/// Checks whether a process group still has members.
fn process_group_exists(process_group: nix::unistd::Pid) -> io::Result<bool> {
    match nix::sys::signal::killpg(process_group, None) {
        Ok(()) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(io::Error::from(error)),
    }
}

/// Sends a signal to a child or its dedicated process group.
fn send_signal(child: &process::Child, target: Target, signal: Signal) -> io::Result<()> {
    let result = match target {
        Target::Child => {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(child.id() as i32), signal)
        }
        Target::ProcessGroup(process_group) => nix::sys::signal::killpg(process_group, signal),
    };
    result.map_err(io::Error::from)
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
    use std::{fs, io, io::Read, process, thread, time};

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
                signal: Signal::SIGINT,
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

    /// Verifies that graceful shutdown observes its deadline without a full polling overshoot.
    #[test]
    fn graceful_shutdown_honors_its_deadline() -> io::Result<()> {
        let grace_time = time::Duration::from_millis(250);
        let scheduler_tolerance = time::Duration::from_millis(90);
        let mut command = process::Command::new("sh");
        command
            .arg("-c")
            .arg("trap '' TERM; printf ready; exec sleep 60")
            .stdout(process::Stdio::piped());
        let mut child = command.spawn()?;

        let mut ready = [0; 5];
        child
            .stdout
            .take()
            .expect("stdout was configured to be piped")
            .read_exact(&mut ready)?;
        assert_eq!(&ready, b"ready");

        let mut guard = ProcessGuard::new(child, Some(grace_time));
        let started = time::Instant::now();
        let status = guard
            .shutdown()?
            .expect("the guard still owns the running child");
        let elapsed = started.elapsed();

        assert!(!status.success());
        assert!(
            elapsed >= grace_time,
            "shutdown completed before the grace period: {elapsed:?}"
        );
        assert!(
            elapsed < grace_time + scheduler_tolerance,
            "shutdown exceeded the grace period and scheduler tolerance: {elapsed:?}"
        );
        Ok(())
    }

    /// Verifies that a process-group child becomes its dedicated group leader.
    #[test]
    fn process_group_uses_child_pid_as_group_id() -> io::Result<()> {
        let mut command = process::Command::new("sleep");
        command.arg("60");
        let mut guard = ProcessGuard::spawn_process_group(&mut command, ShutdownPolicy::Kill)?;
        let child =
            nix::unistd::Pid::from_raw(guard.id().expect("the guard owns the child") as i32);

        let process_group = nix::unistd::getpgid(Some(child)).map_err(io::Error::from)?;
        let status = guard
            .shutdown()?
            .expect("the guard still owns the running child");

        assert_eq!(process_group, child);
        assert!(!status.success());
        Ok(())
    }

    #[test]
    fn process_group_shutdown_reaches_descendants() -> io::Result<()> {
        let pid_file =
            std::env::temp_dir().join(format!("process-guard-{}-group-child.pid", process::id()));
        let _ = fs::remove_file(&pid_file);

        let mut command = process::Command::new("sh");
        command
            .arg("-c")
            .arg(
                "sleep 60 & child=$!; trap 'wait \"$child\"; exit 0' TERM; \
                 printf '%s' \"$child\" > \"$1\"; wait \"$child\"",
            )
            .arg("sh")
            .arg(&pid_file);
        let mut guard = ProcessGuard::spawn_process_group(
            &mut command,
            ShutdownPolicy::GracefulProcessGroup {
                signal: Signal::SIGTERM,
                grace_time: time::Duration::from_secs(1),
            },
        )?;

        let started = time::Instant::now();
        let descendant = loop {
            if let Ok(pid) = fs::read_to_string(&pid_file) {
                break nix::unistd::Pid::from_raw(pid.parse().map_err(io::Error::other)?);
            }
            if started.elapsed() >= time::Duration::from_secs(1) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "child did not report its PID",
                ));
            }
            thread::sleep(time::Duration::from_millis(10));
        };

        let status = guard
            .shutdown()?
            .expect("the guard still owns the running child");
        let descendant_alive = nix::sys::signal::kill(descendant, None).is_ok();
        if descendant_alive {
            let _ = nix::sys::signal::kill(descendant, nix::sys::signal::Signal::SIGKILL);
        }
        fs::remove_file(pid_file)?;

        assert!(status.success());
        assert!(!descendant_alive);
        Ok(())
    }

    #[test]
    fn process_group_shutdown_kills_descendants_after_leader_exit() -> io::Result<()> {
        let pid_file =
            std::env::temp_dir().join(format!("process-guard-{}-orphan-child.pid", process::id()));
        let _ = fs::remove_file(&pid_file);

        let mut command = process::Command::new("sh");
        command
            .arg("-c")
            .arg(
                "sh -c 'trap \"\" TERM; printf \"%s\" \"$$\" > \"$1\"; \
                 while :; do :; done' sh \"$1\" & \
                 while [ ! -s \"$1\" ]; do :; done",
            )
            .arg("sh")
            .arg(&pid_file);
        let mut guard = ProcessGuard::spawn_process_group(
            &mut command,
            ShutdownPolicy::Graceful {
                signal: Signal::SIGTERM,
                grace_time: time::Duration::from_millis(50),
            },
        )?;

        let started = time::Instant::now();
        let descendant = loop {
            if let Ok(pid) = fs::read_to_string(&pid_file) {
                break nix::unistd::Pid::from_raw(pid.parse().map_err(io::Error::other)?);
            }
            if started.elapsed() >= time::Duration::from_secs(1) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "descendant did not report its PID",
                ));
            }
            thread::sleep(time::Duration::from_millis(10));
        };

        let status = guard
            .shutdown()?
            .expect("the guard still owns the completed group leader");
        let started = time::Instant::now();
        let descendant_alive = loop {
            if nix::sys::signal::kill(descendant, None).is_err() {
                break false;
            }
            if started.elapsed() >= time::Duration::from_secs(1) {
                break true;
            }
            thread::sleep(time::Duration::from_millis(10));
        };
        if descendant_alive {
            let _ = nix::sys::signal::kill(descendant, nix::sys::signal::Signal::SIGKILL);
        }
        fs::remove_file(pid_file)?;

        assert!(status.success());
        assert!(!descendant_alive);
        Ok(())
    }

    #[test]
    fn signal_and_wait_reap_a_running_child() -> io::Result<()> {
        let mut command = process::Command::new("sleep");
        command.arg("60");
        let mut guard = ProcessGuard::spawn(&mut command)?;

        assert!(guard.id().is_some());
        assert!(guard.try_wait()?.is_none());
        assert!(guard.signal(Signal::SIGKILL)?.is_none());

        let status = guard
            .wait()?
            .expect("the guard still owns the signaled child");
        assert!(!status.success());
        assert!(guard.id().is_none());
        assert!(guard.wait()?.is_none());
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
