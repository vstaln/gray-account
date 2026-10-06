//! `gray account build`: a static x86_64 musl binary in a reproducible tarball.
//!
//! Builds from the committed tree only (`git archive HEAD` for remote builds,
//! and a clean-tree check for local ones) so a release can never ship edits
//! that are not in the tagged commit.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::maker::project::Project;
use crate::maker::run;

pub const TARGET: &str = "x86_64-unknown-linux-musl";

pub struct Built {
    pub tarball: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub commit: String,
    pub entries: Vec<String>,
}

impl Built {
    pub fn report(&self) -> String {
        format!(
            "built {} ({} bytes) from {}\n  contents: {}\n  sha256:{}",
            self.tarball.display(),
            self.bytes,
            &self.commit[..self.commit.len().min(7)],
            self.entries.join(", "),
            self.sha256
        )
    }
}

pub fn require_clean(root: &Path) -> anyhow::Result<String> {
    let dirty = run::capture(
        root,
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    if !dirty.is_empty() {
        anyhow::bail!(
            "uncommitted changes — commit first so the release matches its tag:\n{dirty}"
        );
    }
    run::capture(root, "git", &["rev-parse", "HEAD"])
}

pub fn build(p: &Project, remote: Option<&str>) -> anyhow::Result<Built> {
    let commit = require_clean(&p.root)?;
    let binary = match remote {
        Some(host) => build_remote(p, host)?,
        None => build_local(p)?,
    };
    if !is_static_elf(&std::fs::read(&binary)?)? {
        anyhow::bail!(
            "{} is dynamically linked; gray plugins ship one static musl binary",
            binary.display()
        );
    }
    let mut entries = vec![(p.bin.clone(), binary)];
    let launcher = p.root.join("plugin.sh");
    if launcher.is_file() {
        entries.push(("plugin.sh".into(), launcher));
    }
    let tarball = package(p, &entries)?;
    let bytes = std::fs::read(&tarball)?;
    Ok(Built {
        sha256: sha256_hex(&bytes),
        bytes: bytes.len() as u64,
        tarball,
        commit,
        entries: entries.into_iter().map(|(n, _)| n).collect(),
    })
}

fn cargo_args(p: &Project, zig: bool) -> Vec<String> {
    let mut a: Vec<String> = vec![
        if zig { "zigbuild" } else { "build" }.into(),
        "--release".into(),
    ];
    if lock_tracked(&p.root) {
        a.push("--locked".into());
    }
    a.extend([
        "--target".into(),
        TARGET.into(),
        "--bin".into(),
        p.bin.clone(),
    ]);
    a
}

/// `--locked` only when Cargo.lock is committed: remote builds see the
/// `git archive` tree, so an untracked local lockfile must not count.
fn lock_tracked(root: &Path) -> bool {
    run::capture(root, "git", &["ls-files", "--error-unmatch", "Cargo.lock"]).is_ok()
}

fn build_local(p: &Project) -> anyhow::Result<PathBuf> {
    let zig = run::available("cargo-zigbuild");
    let args = cargo_args(p, zig);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run::capture(&p.root, "cargo", &args)?;
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| p.root.join("target"));
    Ok(target_dir.join(TARGET).join("release").join(&p.bin))
}

/// `git archive HEAD | ssh host` → `cargo zigbuild` there → `scp` back.
/// Expects the build box to have `~/.cargo/bin/cargo`, cargo-zigbuild, and
/// zig either on PATH or at `~/build/zig/zig`.
fn build_remote(p: &Project, host: &str) -> anyhow::Result<PathBuf> {
    if host.starts_with('-') {
        anyhow::bail!("invalid --remote host '{host}'");
    }
    let src = format!("build/gray-account/{}", p.package);
    let target = "build/gray-account/target";
    let args = cargo_args(p, true).join(" ");
    let script = format!(
        "set -e; rm -rf ~/{src}; mkdir -p ~/{src}; tar -x -C ~/{src}; \
         export PATH=~/build/zig:~/.cargo/bin:$PATH CARGO_TARGET_DIR=~/{target}; \
         cd ~/{src} && cargo {args} >&2"
    );
    let mut archive = Command::new("git")
        .args(["archive", "HEAD"])
        .current_dir(&p.root)
        .stdout(Stdio::piped())
        .spawn()?;
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host, &script])
        .stdin(archive.stdout.take().expect("piped"))
        .output()?;
    archive.wait()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = err
            .lines()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        anyhow::bail!("remote build on {host} failed:\n{}", tail.join("\n"));
    }
    let local = p.root.join("target").join("gray-account");
    std::fs::create_dir_all(&local)?;
    let dest = local.join(&p.bin);
    let remote_bin = format!("{host}:{target}/{TARGET}/release/{}", p.bin);
    run::capture(
        &p.root,
        "scp",
        &[
            "-q",
            "-o",
            "BatchMode=yes",
            &remote_bin,
            &dest.to_string_lossy(),
        ],
    )?;
    Ok(dest)
}

/// Tar the entries flat, mode 0755, root-owned, epoch mtime, gzip without a
/// timestamp: identical inputs give byte-identical tarballs.
fn package(p: &Project, entries: &[(String, PathBuf)]) -> anyhow::Result<PathBuf> {
    let stage = p.dist().join(".stage");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    for (name, src) in entries {
        let dst = stage.join(name);
        std::fs::copy(src, &dst)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    let out = p.tarball_path();
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    let tar = Command::new("tar")
        .args([
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--mtime=@0",
            "--sort=name",
            "-cf",
            "-",
            "-C",
        ])
        .arg(&stage)
        .args(&names)
        .stdout(Stdio::piped())
        .spawn()?;
    let gz = Command::new("gzip")
        .args(["-n", "-9"])
        .stdin(tar.stdout.expect("piped"))
        .output()?;
    if !gz.status.success() || gz.stdout.is_empty() {
        anyhow::bail!("packaging {} failed", out.display());
    }
    std::fs::write(&out, gz.stdout)?;
    std::fs::remove_dir_all(&stage)?;
    Ok(out)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An x86_64 ELF is static when it has no PT_INTERP program header.
pub fn is_static_elf(b: &[u8]) -> anyhow::Result<bool> {
    if b.len() < 64 || &b[..4] != b"\x7fELF" || b[4] != 2 || b[5] != 1 {
        anyhow::bail!("not a little-endian 64-bit ELF binary");
    }
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as usize;
    let phoff = u64::from_le_bytes(b[0x20..0x28].try_into()?) as usize;
    let (size, count) = (u16_at(0x36), u16_at(0x38));
    for i in 0..count {
        let o = phoff + i * size;
        let Some(raw) = b.get(o..o + 4) else {
            anyhow::bail!("truncated ELF program headers")
        };
        if u32::from_le_bytes(raw.try_into()?) == 3 {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elf(phdr_types: &[u32]) -> Vec<u8> {
        let mut b = vec![0u8; 64 + 56 * phdr_types.len()];
        b[..6].copy_from_slice(b"\x7fELF\x02\x01");
        b[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        b[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        b[0x38..0x3a].copy_from_slice(&(phdr_types.len() as u16).to_le_bytes());
        for (i, t) in phdr_types.iter().enumerate() {
            b[64 + 56 * i..68 + 56 * i].copy_from_slice(&t.to_le_bytes());
        }
        b
    }

    #[test]
    fn interp_header_means_dynamic() {
        assert!(is_static_elf(&elf(&[1, 1])).unwrap());
        assert!(!is_static_elf(&elf(&[6, 3, 1])).unwrap());
        assert!(is_static_elf(b"#!/bin/sh\n").is_err());
    }

    #[test]
    fn the_host_shell_is_dynamic() {
        if let Ok(b) = std::fs::read("/bin/sh")
            && b.starts_with(b"\x7fELF\x02")
        {
            assert!(!is_static_elf(&b).unwrap());
        }
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
