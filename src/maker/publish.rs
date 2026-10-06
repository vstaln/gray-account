//! `gray account publish`: submit the verified release to the gray registry.
//!
//! Auth reuses the token `gray account login` stores; the registry downloads
//! and hashes the tarball itself and rejects a mismatch with ours.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::maker::project::Project;
use crate::maker::release::{Released, record_path};

pub const REGISTRY_URL_ENV: &str = "GRAY_REGISTRY_URL";
const DEFAULT_REGISTRY: &str = "https://gray.alignment.id/api";

pub fn publish(p: &Project) -> anyhow::Result<String> {
    let rec: Released =
        serde_json::from_str(&std::fs::read_to_string(record_path(p)).map_err(|_| {
            anyhow::anyhow!("no dist/release.json — run `gray account release` first")
        })?)?;
    if rec.version != p.version {
        anyhow::bail!(
            "last release was {} but Cargo.toml says {} — run `gray account release` again",
            rec.version,
            p.version
        );
    }
    let token = load_token()?;
    let endpoint = format!("{}/plugins/submit", registry_base()?);
    let body = submission(p, &rec);
    let resp = ureq::post(&endpoint)
        .timeout(Duration::from_secs(180))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(body);
    match resp {
        Ok(r) => {
            let v: Value = r.into_json().unwrap_or(Value::Null);
            Ok(format!(
                "published {} {} to the gray registry{}\n  install: gray plugin install {}",
                p.key,
                p.version,
                v.get("hash")
                    .and_then(Value::as_str)
                    .map(|h| format!(" ({h})"))
                    .unwrap_or_default(),
                p.key
            ))
        }
        Err(ureq::Error::Status(code, r)) => {
            let msg = r
                .into_json::<Value>()
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_else(|| "no error message".into());
            anyhow::bail!("registry refused ({code}): {msg}")
        }
        Err(e) => anyhow::bail!("cannot reach {endpoint}: {e}"),
    }
}

pub fn submission(p: &Project, rec: &Released) -> Value {
    let repo = format!("https://github.com/{}", rec.repo);
    json!({
        "name": p.key,
        "version": p.version,
        "source_url": rec.url,
        "provided_hash": format!("sha256:{}", rec.sha256),
        "description": p.description.trim(),
        "homepage": repo,
        "repo": repo,
        "scope": "user",
        "commands": commands_of(p),
    })
}

/// Slash commands from the debug build's manifest (`check` builds it);
/// empty when unavailable — the registry treats commands as display-only.
fn commands_of(p: &Project) -> Vec<String> {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| p.root.join("target"));
    let bin = target.join("debug").join(&p.bin);
    std::process::Command::new(bin)
        .arg("manifest")
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|m| m.get("commands").cloned())
        .and_then(|c| serde_json::from_value(c).ok())
        .unwrap_or_default()
}

fn gray_home() -> anyhow::Result<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .ok_or_else(|| anyhow::anyhow!("cannot resolve home: set GRAY_HOME or HOME"))
}

fn load_token() -> anyhow::Result<String> {
    let path = gray_home()?.join("registry-token.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|b| serde_json::from_str::<Value>(&b).ok())
        .and_then(|v| v.get("token").and_then(Value::as_str).map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty())
        .ok_or_else(|| anyhow::anyhow!("not logged in — run `gray account login` (install it with `gray plugin install account`)"))
}

/// Same rules as gray-account: env override, one trailing slash stripped,
/// https required except on loopback (the token rides in the header).
pub fn registry_base() -> anyhow::Result<String> {
    let raw = std::env::var(REGISTRY_URL_ENV).unwrap_or_default();
    base_from(raw.trim())
}

pub fn base_from(raw: &str) -> anyhow::Result<String> {
    let base = if raw.is_empty() {
        DEFAULT_REGISTRY
    } else {
        raw
    };
    let base = base.strip_suffix('/').unwrap_or(base);
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| {
            base.strip_prefix(p)
                .is_some_and(|r| r.is_empty() || r.starts_with([':', '/']))
        });
    if !base.starts_with("https://") && !loopback {
        anyhow::bail!("{REGISTRY_URL_ENV} must be https:// (http is allowed only on loopback)");
    }
    Ok(base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn registry_base_defaults_and_guards_cleartext() {
        assert_eq!(base_from("").unwrap(), DEFAULT_REGISTRY);
        assert_eq!(
            base_from("http://127.0.0.1:4000/api/").unwrap(),
            "http://127.0.0.1:4000/api"
        );
        assert!(base_from("http://example.com/api").is_err());
        assert!(base_from("http://127.0.0.1.evil.com/api").is_err());
    }

    #[test]
    fn submission_carries_hash_and_links() {
        let p = Project::parse(
            Path::new("/nonexistent"),
            "[package]\nname=\"gray-w\"\nversion=\"0.1.0\"\ndescription=\" W \"\n",
        )
        .unwrap();
        let rec = Released {
            version: "0.1.0".into(),
            url: "https://u".into(),
            sha256: "ab".into(),
            repo: "vstaln/gray-w".into(),
        };
        let s = submission(&p, &rec);
        assert_eq!(s["name"], "w");
        assert_eq!(s["provided_hash"], "sha256:ab");
        assert_eq!(s["description"], "W");
        assert_eq!(s["repo"], "https://github.com/vstaln/gray-w");
        assert_eq!(s["commands"], json!([]));
    }
}
