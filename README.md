# process_guard

A process guard takes ownership of a `process::Child` and gently or forcefully kills it upon,
prevent the process from running on. Example:

```rust
use process_guard::ProcessGuard;
use std::process;

fn insomnia() {
   let cmd = process::Command::new("sleep").arg("120");
   let pg = ProcessGuard::spawn(cmd);

   // a two-minute sleep process has been started, which will be killed as soon as this
   // function returns
}
```

## OS support

The crate is currently only developed with Linux in mind. Windows/BSD/Mac OS X ports are appreciated
