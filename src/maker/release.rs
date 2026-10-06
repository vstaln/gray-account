//! `gray account release`: publish the built tarball as a GitHub release and
//! prove the public URL serves exactly those bytes.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::maker::build::{require_clean, sha256_hex};
use crate::maker::project::Project;
use crate::maker::run;

/// What `release` hands to `publish` (written to `dist/release.json`).
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Released {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub repo: String,
}

pub fn record_path(p: &Project) -> std::path::PathBuf {
    p.dist().join("release.json")
}

pub fn release(p: &Project) -> anyhow::Result<String> {
    let commit = require_clean(&p.root)?;
    let tarball = p.tarball_path();
    let bytes = std::fs::read(&tarball).map_err(|_| {
        anyhow::anyhow!(
            "{} not found — run `gray account build` first",
            tarball.display()
        )
    })?;
    let sha = sha256_hex(&bytes);
    let repo = github_repo(&run::capture(
        &p.root,
        "git",
        &["remote", "get-url", "origin"],
    )?)?;
    run::capture(&p.root, "git", &["fetch", "-q", "origin"])?;
    if run::capture(&p.root, "git", &["branch", "-r", "--contains", &commit])?.is_empty() {
        anyhow::bail!(
            "HEAD {} is not pushed to origin — push it so the tag points at public code",
            &commit[..7]
        );
    }
    let tag = p.tag();
    let name = p.tarball_name();
    let url = format!("https://github.com/{repo}/releases/download/{tag}/{name}");

    let existing = run::capture(
        &p.root,
        "gh",
        &[
            "release",
            "view",
            &tag,
            "-R",
            &repo,
            "--json",
            "assets",
            "-q",
            ".assets[].name",
        ],
    );
    let mut lines = vec![];
    match existing {
        Ok(assets) if assets.lines().any(|a| a == name) => {
            let served = fetch_sha(&url)?;
            if served != sha {
                anyhow::bail!(
                    "{tag} already ships {name} with different bytes (sha256:{served}) — published versions are immutable; bump the version in Cargo.toml"
                );
            }
            lines.push(format!(
                "release {tag} already has this exact tarball — nothing to upload"
            ));
        }
        Ok(_) => {
            run::capture(
                &p.root,
                "gh",
                &[
                    "release",
                    "upload",
                    &tag,
                    &tarball.to_string_lossy(),
                    "-R",
                    &repo,
                ],
            )?;
            lines.push(format!("uploaded {name} to existing release {tag}"));
        }
        Err(_) => {
            let notes = format!(
                "Static x86_64 Linux (musl) build.\n\nInstall: `gray plugin install {}`",
                p.key
            );
            run::capture(
                &p.root,
                "gh",
                &[
                    "release",
                    "create",
                    &tag,
                    &tarball.to_string_lossy(),
                    "-R",
                    &repo,
                    "--target",
                    &commit,
                    "--title",
                    &tag,
                    "--notes",
                    &notes,
                ],
            )?;
            lines.push(format!("created release {tag} at {}", &commit[..7]));
        }
    }
    verify_served(&url, &sha)?;
    lines.push(format!("  verified {url}\n  sha256:{sha}"));
    let rec = Released {
        version: p.version.clone(),
        url,
        sha256: sha,
        repo,
    };
    std::fs::write(record_path(p), serde_json::to_string_pretty(&rec)? + "\n")?;
    Ok(lines.join("\n"))
}

/// Reuse the tarball release `p.tag()` already ships, if it does: download it
/// from the public URL, hash it, and write the same `dist/release.json`
/// `release` writes. Needs neither a clean tree nor a local tarball — for a
/// publisher who only wants to push the already-released build to the
/// registry. Returns `None` when the release or the asset does not exist.
pub fn adopt(p: &Project) -> anyhow::Result<Option<String>> {
    let repo = github_repo(&run::capture(
        &p.root,
        "git",
        &["remote", "get-url", "origin"],
    )?)?;
    let tag = p.tag();
    let name = p.tarball_name();
    let existing = run::capture(
        &p.root,
        "gh",
        &[
            "release",
            "view",
            &tag,
            "-R",
            &repo,
            "--json",
            "assets",
            "-q",
            ".assets[].name",
        ],
    );
    match existing {
        Ok(assets) if assets.lines().any(|a| a == name) => {}
        // No release, or a release without our tarball.
        _ => return Ok(None),
    }

    let url = format!("https://github.com/{repo}/releases/download/{tag}/{name}");
    let sha = fetch_sha(&url)?;
    let rec = Released {
        version: p.version.clone(),
        url: url.clone(),
        sha256: sha.clone(),
        repo,
    };
    std::fs::create_dir_all(p.dist())?;
    std::fs::write(record_path(p), serde_json::to_string_pretty(&rec)? + "\n")?;
    Ok(Some(format!(
        "adopted {name} from existing release {tag}\n  verified {url}\n  sha256:{sha}"
    )))
}

/// GitHub's CDN can serve a replaced asset's old bytes for a minute or two,
/// so retry before calling it a mismatch.
fn verify_served(url: &str, want: &str) -> anyhow::Result<()> {
    let mut last = String::new();
    for attempt in 0..8 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(15));
        }
        match fetch_sha(url) {
            Ok(got) if got == want => return Ok(()),
            Ok(got) => last = format!("served sha256:{got}"),
            Err(e) => last = e.to_string(),
        }
    }
    anyhow::bail!("{url} never served the uploaded bytes (want sha256:{want}; {last})")
}

pub fn fetch_sha(url: &str) -> anyhow::Result<String> {
    let resp = ureq::get(url).timeout(Duration::from_secs(120)).call()?;
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader().take(256 << 20), &mut buf)?;
    Ok(sha256_hex(&buf))
}

use std::io::Read as _;

/// `git@github.com:o/r.git` / `https://github.com/o/r(.git)` → `o/r`.
pub fn github_repo(remote: &str) -> anyhow::Result<String> {
    let r = remote.trim();
    let path = r
        .strip_prefix("git@github.com:")
        .or_else(|| r.strip_prefix("ssh://git@github.com/"))
        .or_else(|| r.strip_prefix("https://github.com/"))
        .ok_or_else(|| anyhow::anyhow!("origin {r} is not a GitHub remote"))?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    match path.split('/').collect::<Vec<_>>().as_slice() {
        [o, n] if !o.is_empty() && !n.is_empty() => Ok(path.to_string()),
        _ => anyhow::bail!("cannot read owner/repo from origin {r}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes_parse_in_every_spelling() {
        for r in [
            "git@github.com:vstaln/gray-x.git",
            "https://github.com/vstaln/gray-x",
            "https://github.com/vstaln/gray-x.git\n",
            "ssh://git@github.com/vstaln/gray-x.git",
        ] {
            assert_eq!(github_repo(r).unwrap(), "vstaln/gray-x", "{r}");
        }
        assert!(github_repo("https://gitlab.com/a/b").is_err());
        assert!(github_repo("https://github.com/only").is_err());
    }
}
