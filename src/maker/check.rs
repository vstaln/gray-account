//! `gray account check`: prove gray can start the plugin before releasing it.
//!
//! Stages the debug binary exactly as the tarball will lay it out, picks the
//! entry point with gray's rule (`plugin.sh`, else the single executable),
//! and performs the `plugin/manifest` handshake gray runs at install.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::maker::project::{Project, valid_key};
use crate::maker::run;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

pub fn check(p: &Project) -> anyhow::Result<String> {
    if !valid_key(&p.key) {
        anyhow::bail!(
            "'{}' (from package {}) is not a valid registry name",
            p.key,
            p.package
        );
    }
    if p.description.trim().is_empty() {
        anyhow::bail!("Cargo.toml needs a description — the registry requires one");
    }
    run::capture(&p.root, "cargo", &["build", "--quiet", "--bin", &p.bin])?;
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(Into::into)
        .unwrap_or_else(|| p.root.join("target"));
    let binary = target_dir.join("debug").join(&p.bin);

    let stage = tempdir()?;
    std::fs::copy(&binary, stage.join(&p.bin))?;
    let launcher = p.root.join("plugin.sh");
    let entry = if launcher.is_file() {
        std::fs::copy(&launcher, stage.join("plugin.sh"))?;
        "plugin.sh"
    } else {
        p.bin.as_str()
    };
    let result = handshake(&stage, entry);
    let _ = std::fs::remove_dir_all(&stage);
    let manifest = result?;
    let note = verify_manifest(p, &manifest)?;
    let tools = manifest["tools"].as_array().map_or(0, Vec::len);
    let commands = manifest["commands"].as_array().map_or(0, Vec::len);
    Ok(format!(
        "check ok: {} {} — entry {entry}, protocol {}, {tools} tool(s), {commands} command(s){}",
        p.key,
        p.version,
        manifest["protocol"].as_str().unwrap_or("?"),
        note.map(|n| format!("\n  note: {n}")).unwrap_or_default(),
    ))
}

/// Returns a note for the report when the manifest name differs from the
/// registry key (fine — e.g. `claude-sub` publishes as `claude`); version and
/// tool-shape mismatches still fail.
pub fn verify_manifest(p: &Project, m: &Value) -> anyhow::Result<Option<String>> {
    let note = (m["name"] != p.key.as_str()).then(|| {
        format!(
            "manifest name {} differs from registry key \"{}\"",
            m["name"], p.key
        )
    });
    if m["version"] != p.version.as_str() {
        anyhow::bail!(
            "manifest version {} != Cargo.toml version {}",
            m["version"],
            p.version
        );
    }
    for t in m["tools"].as_array().into_iter().flatten() {
        if t["name"].as_str().is_none_or(str::is_empty) || t["parameters"].is_null() {
            anyhow::bail!("tool entry needs a name and parameters: {t}");
        }
    }
    Ok(note)
}

fn handshake(dir: &Path, entry: &str) -> anyhow::Result<Value> {
    let mut child = Command::new(dir.join(entry))
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot start {entry}: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    // A CLI-mode binary can exit before we write: a broken pipe here is the
    // same "did not answer" case the EOF below reports — do not lose that
    // message behind an io error.
    let _ = writeln!(stdin, "{}", json!({"id": 1, "method": "plugin/manifest"}));
    let _ = stdin.flush();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let line = BufReader::new(stdout).lines().next();
        let _ = tx.send(line);
    });
    let reply = rx.recv_timeout(HANDSHAKE_TIMEOUT);
    let _ = writeln!(stdin, "{}", json!({"id": 2, "method": "plugin/shutdown"}));
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let line = match reply {
        Ok(Some(Ok(line))) => line,
        Ok(_) => anyhow::bail!(
            "{entry} exited without answering plugin/manifest — with no arguments it must speak the sidecar protocol (add a plugin.sh if it needs flags)"
        ),
        Err(_) => {
            anyhow::bail!("{entry} did not answer plugin/manifest within {HANDSHAKE_TIMEOUT:?}")
        }
    };
    let v: Value = serde_json::from_str(&line)
        .map_err(|e| anyhow::anyhow!("{entry} wrote non-JSON to stdout: {e}: {line}"))?;
    if let Some(err) = v.get("error") {
        anyhow::bail!("plugin/manifest returned an error: {err}");
    }
    v.get("result")
        .filter(|r| r.is_object())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("plugin/manifest reply has no result object: {line}"))
}

fn tempdir() -> anyhow::Result<std::path::PathBuf> {
    let d = std::env::temp_dir().join(format!("gray-account-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        Project::parse(
            Path::new("/x"),
            "[package]\nname=\"gray-w\"\nversion=\"1.2.3\"\ndescription=\"d\"\n",
        )
        .unwrap()
    }

    #[test]
    fn manifest_must_match_version_but_a_different_name_is_only_a_note() {
        let p = project();
        assert_eq!(
            verify_manifest(&p, &json!({"name": "w", "version": "1.2.3", "tools": []})).unwrap(),
            None
        );
        let note = verify_manifest(&p, &json!({"name": "w-sub", "version": "1.2.3"})).unwrap();
        assert_eq!(
            note.as_deref(),
            Some("manifest name \"w-sub\" differs from registry key \"w\"")
        );
        assert!(verify_manifest(&p, &json!({"name": "w", "version": "1.0.0"})).is_err());
        assert!(
            verify_manifest(
                &p,
                &json!({"name": "w", "version": "1.2.3", "tools": [{"name": "t"}]})
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn handshake_reads_the_manifest_and_flags_cli_style_binaries() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempdir().unwrap().join("hs");
        std::fs::create_dir_all(&d).unwrap();
        let write = |name: &str, body: &str| {
            let f = d.join(name);
            std::fs::write(&f, body).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        write(
            "good",
            "#!/bin/sh\nread l; echo '{\"id\":1,\"result\":{\"name\":\"w\"}}'\n",
        );
        write("cli", "#!/bin/sh\necho 'usage: cli <cmd>' >&2; exit 2\n");
        assert_eq!(handshake(&d, "good").unwrap()["name"], "w");
        assert!(
            handshake(&d, "cli")
                .unwrap_err()
                .to_string()
                .contains("plugin.sh")
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
