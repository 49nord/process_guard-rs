# process_guard

`process_guard` owns a child process and shuts it down when explicitly requested or when the guard is dropped. By default it sends `SIGTERM`, waits up to 10 seconds, and then sends `SIGKILL`. Shutdown can target either the direct child or a dedicated process group.

```rust,no_run
use process_guard::ProcessGuard;
use std::process::Command;

fn main() -> std::io::Result<()> {
    let mut command = Command::new("sleep");
    command.arg("120");
    let guard = ProcessGuard::spawn(&mut command)?;

    // The child is gracefully shut down and reaped when the guard is dropped.
    drop(guard);
    Ok(())
}
```

Graceful shutdown sends a configurable signal, waits for a bounded period, and then sends `SIGKILL`. For example, PostgreSQL interprets `SIGINT` as a fast-shutdown request:

```rust,no_run
use process_guard::{ProcessGuard, ShutdownPolicy, Signal};
use std::{process::Command, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let policy = ShutdownPolicy::Graceful {
        signal: Signal::SIGINT,
        grace_time: Duration::from_secs(5),
    };
    let mut command = Command::new("postgres");
    command.arg("-D").arg("database");
    let mut guard = ProcessGuard::spawn_process_group(&mut command, policy)?;

    guard.shutdown()?;
    Ok(())
}
```

`ProcessGuard::shutdown` reports cleanup errors and retains ownership when cleanup fails, allowing another attempt. `Drop` uses the same bounded shutdown operation as a best-effort fallback.

## Process groups

`ProcessGuard::spawn_process_group` starts the direct child as the leader of a dedicated process group. `ShutdownPolicy::Graceful` asks the direct child to coordinate shutdown, while `ShutdownPolicy::GracefulProcessGroup` broadcasts the graceful signal. Forceful fallback always signals the complete group. The guard reaps the direct child.

`GracefulProcessGroup` broadcasts only when the guard created and owns a dedicated group. On a direct-child guard it behaves like `Graceful`; the guard never discovers or signals the child's inherited process group, which could also contain the caller or its parent.

`wait`, `try_wait`, and `take` operate on the direct child. Calling one of them can relinquish process-group cleanup after the leader exits, so process-group owners should normally use `shutdown`.

## Lifecycle limits

Cleanup through `Drop` only runs when Rust destructors execute. It does not run when the owning process is terminated by `SIGKILL`, calls `abort`, or exits through another path that bypasses unwinding. Applications should handle ordinary termination signals and return through normal control flow. Surviving abrupt parent death requires an independent watchdog process.

## Platform support

The crate uses Unix signals and process groups. Linux and arm64 macOS are tested in CI. Runtime validation has also been completed on macOS 15.5 on Apple Silicon. Windows and other Unix platforms are not currently supported.
