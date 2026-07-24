//! Thin wrapper around `std::process::Command` for shelling out to external
//! tools (`brew`, `defaults`, `killall`, ...). Centralizes command execution so
//! logging, dry-run, and error handling are consistent everywhere.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Result of running an external command.
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Is `bin` resolvable on PATH?
pub fn has_command(bin: &str) -> bool {
    which(bin).is_some()
}

/// Resolve a binary on PATH, returning its full path.
pub fn which(bin: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return candidate.to_str().map(String::from);
        }
    }
    None
}

/// Run a command and capture its output. Does NOT fail on non-zero exit — the
/// caller inspects `Output::status`. Mirrors the "continue on failure" spirit of
/// the shell `run` helper, but hands control back to the caller.
pub fn capture(bin: &str, args: &[&str]) -> Result<Output> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn `{bin}`"))?;
    Ok(Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Run a command for its side effects, streaming stdio to the terminal.
/// Returns an error if the command exits non-zero.
pub fn run(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `{bin}`"))?;
    if !status.success() {
        bail!("`{bin} {}` exited with {}", args.join(" "), status);
    }
    Ok(())
}

/// Run a command, capturing output, returning stdout on success or an error
/// carrying stderr on failure.
pub fn output(bin: &str, args: &[&str]) -> Result<String> {
    let out = capture(bin, args)?;
    if !out.success() {
        bail!(
            "`{bin} {}` exited with {}: {}",
            args.join(" "),
            out.status,
            out.stderr.trim()
        );
    }
    Ok(out.stdout)
}
