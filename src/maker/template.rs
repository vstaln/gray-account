//! `gray account new`: scaffold a sidecar plugin repo from the bundled template.

use std::path::{Path, PathBuf};

use crate::maker::project::valid_key;
use crate::maker::run;

const FILES: [(&str, &str); 5] = [
    (
        "Cargo.toml",
        include_str!("../../templates/sidecar/Cargo.toml"),
    ),
    (
        "src/main.rs",
        include_str!("../../templates/sidecar/src/main.rs"),
    ),
    (
        "README.md",
        include_str!("../../templates/sidecar/README.md"),
    ),
    (
        ".gitignore",
        include_str!("../../templates/sidecar/gitignore"),
    ),
    ("LICENSE", include_str!("../../LICENSE")),
];

pub struct NewOpts {
    /// Plugin key (`hello`) or package (`gray-hello`); both mean the same.
    pub name: String,
    /// Target directory; default `~/grayplugins/gray-<key>`.
    pub dir: Option<PathBuf>,
    pub description: Option<String>,
    /// Create `vstaln/gray-<key>` on GitHub and push.
    pub create_repo: bool,
}

/// Render the template into `dir` without touching git or the network.
pub fn render(key: &str, description: &str, dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !valid_key(key) {
        anyhow::bail!(
            "invalid plugin name '{key}': use lowercase letters, digits and hyphens, starting with a letter or digit"
        );
    }
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        anyhow::bail!("{} already exists and is not empty", dir.display());
    }
    let package = format!("gray-{key}");
    let tool = format!("{}_hello", key.replace('-', "_"));
    let toml_desc = description.replace('\\', "\\\\").replace('"', "\\\"");
    let mut written = Vec::new();
    for (rel, body) in FILES {
        let desc = if rel == "Cargo.toml" {
            &toml_desc
        } else {
            description
        };
        let text = body
            .replace("__PKG__", &package)
            .replace("__KEY__", key)
            .replace("__TOOL__", &tool)
            .replace("__DESC__", desc);
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("template paths have a parent"))?;
        std::fs::write(&path, text)?;
        written.push(path);
    }
    Ok(written)
}

pub fn new_plugin(opts: &NewOpts) -> anyhow::Result<String> {
    let key = crate::maker::project::key_for(opts.name.trim());
    let dir = match &opts.dir {
        Some(d) => d.clone(),
        None => home()?.join("grayplugins").join(format!("gray-{key}")),
    };
    let description = opts
        .description
        .clone()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| format!("{key} plugin for the gray agent harness"));
    render(&key, &description, &dir)?;

    let mut report = vec![format!("scaffolded gray-{key} at {}", dir.display())];
    // Binaries pin their dependency graph: commit the lockfile from day one so
    // `--locked` release builds are reproducible.
    run::capture(&dir, "cargo", &["generate-lockfile", "--quiet"])?;
    run::capture(&dir, "git", &["init", "-q", "-b", "main"])?;
    run::capture(&dir, "git", &["add", "-A"])?;
    run::capture(
        &dir,
        "git",
        &[
            "commit",
            "-q",
            "-m",
            &format!("gray-{key}: scaffolded by `gray account new`"),
        ],
    )?;
    report.push("  git: initial commit on main".into());

    if opts.create_repo {
        let repo = format!("vstaln/gray-{key}");
        run::capture(
            &dir,
            "gh",
            &[
                "repo",
                "create",
                &repo,
                "--public",
                "--source",
                ".",
                "--push",
                "--description",
                &description,
            ],
        )?;
        report.push(format!("  github: https://github.com/{repo}"));
    }
    report.push(format!(
        "next: cd {} && cargo test && gray account check && gray account publish",
        dir.display()
    ));
    Ok(report.join("\n"))
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; pass --dir"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_every_placeholder() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("gray-weather");
        render("weather", "Says \"hi\"", &dir).unwrap();
        for (rel, _) in FILES {
            let text = std::fs::read_to_string(dir.join(rel)).unwrap();
            assert!(!text.contains("__"), "{rel} kept a placeholder");
        }
        let toml = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
        assert!(toml.contains("name = \"gray-weather\""));
        assert!(toml.contains("description = \"Says \\\"hi\\\"\""));
        let main = std::fs::read_to_string(dir.join("src/main.rs")).unwrap();
        assert!(main.contains("\"weather_hello\"") && main.contains("\"/weather\""));
    }

    #[test]
    fn render_refuses_bad_names_and_non_empty_dirs() {
        let t = tempfile::tempdir().unwrap();
        assert!(render("Bad Name", "x", &t.path().join("a")).is_err());
        std::fs::write(t.path().join("keep"), "x").unwrap();
        let err = render("ok", "x", t.path()).unwrap_err();
        assert!(err.to_string().contains("not empty"));
    }
}
