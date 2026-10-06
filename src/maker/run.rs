//! Thin wrappers over child processes with errors that name the command.

use std::path::Path;
use std::process::Command;

/// Run `prog args` in `dir`, returning trimmed stdout. A non-zero exit is an
/// error carrying the tail of stderr, so failures read as the tool's own words.
pub fn capture(dir: &Path, prog: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run {prog}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err.lines().rev().take(12).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        anyhow::bail!(
            "{prog} {} failed ({}):\n{}",
            args.join(" "),
            out.status,
            tail.join("\n")
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// True when `prog` resolves and runs `--version` successfully.
pub fn available(prog: &str) -> bool {
    Command::new(prog)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}
