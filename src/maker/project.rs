//! The plugin project in a directory: Cargo metadata plus its GitHub repo.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub root: PathBuf,
    /// Cargo package name, e.g. `gray-hello`.
    pub package: String,
    /// Index key users install by, e.g. `hello`.
    pub key: String,
    pub version: String,
    pub description: String,
    /// The single `[[bin]]` that becomes the plugin executable.
    pub bin: String,
}

impl Project {
    /// Walk up from `start` to the nearest Cargo.toml and read it.
    pub fn load(start: &Path) -> anyhow::Result<Self> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            let manifest = d.join("Cargo.toml");
            if manifest.is_file() {
                let text = std::fs::read_to_string(&manifest)?;
                return Self::parse(d, &text);
            }
            dir = d.parent();
        }
        anyhow::bail!(
            "no Cargo.toml in {} or any parent — run inside a plugin repo",
            start.display()
        )
    }

    /// Minimal Cargo.toml reader: `[package]` name/version/description and
    /// `[[bin]]` names. Plugin manifests are simple; a TOML crate is overkill.
    pub fn parse(root: &Path, text: &str) -> anyhow::Result<Self> {
        let mut section = String::new();
        let (mut name, mut version, mut description) = (None, None, String::new());
        let mut bins = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                section = line.to_string();
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim().trim_matches('"').to_string());
            match (section.as_str(), k) {
                ("[package]", "name") => name = Some(v),
                ("[package]", "version") => version = Some(v),
                ("[package]", "description") => description = v,
                ("[[bin]]", "name") => bins.push(v),
                _ => {}
            }
        }
        let package = name.ok_or_else(|| anyhow::anyhow!("Cargo.toml has no [package] name"))?;
        let version = version.ok_or_else(|| {
            anyhow::anyhow!(
                "Cargo.toml has no [package] version (workspace versions are not supported)"
            )
        })?;
        let bin = match bins.len() {
            0 => package.clone(),
            1 => bins.remove(0),
            _ => anyhow::bail!(
                "{} declares {} [[bin]] targets ({}); a plugin tarball holds exactly one executable",
                package,
                bins.len(),
                bins.join(", ")
            ),
        };
        let key = key_for(&package);
        Ok(Self {
            root: root.to_path_buf(),
            package,
            key,
            version,
            description,
            bin,
        })
    }

    pub fn tag(&self) -> String {
        format!("v{}", self.version)
    }

    pub fn tarball_name(&self) -> String {
        format!("gray-{}-{}.tar.gz", self.key, self.version)
    }

    pub fn dist(&self) -> PathBuf {
        self.root.join("dist")
    }

    pub fn tarball_path(&self) -> PathBuf {
        self.dist().join(self.tarball_name())
    }
}

/// `gray-foo` / `gray-foo-plugin` / `gray-foo-sub` / `grayfoo` → `foo`.
pub fn key_for(package: &str) -> String {
    let k = package
        .strip_prefix("gray-")
        .or_else(|| package.strip_prefix("gray"))
        .filter(|k| !k.is_empty())
        .unwrap_or(package);
    let k = k
        .strip_suffix("-plugin")
        .or_else(|| k.strip_suffix("-sub"))
        .unwrap_or(k);
    k.trim_start_matches('-').to_string()
}

/// Same rule as the registry: lowercase alphanumerics and hyphens, starting
/// alphanumeric, not reserved.
pub fn valid_key(key: &str) -> bool {
    const RESERVED: [&str; 4] = ["tmp", "pi", "lock", "index-cache"];
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !RESERVED.contains(&key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_strip_the_conventional_affixes() {
        assert_eq!(key_for("gray-account"), "account");
        assert_eq!(key_for("gray-claude-sub"), "claude");
        assert_eq!(key_for("gray-discord-plugin"), "discord");
        assert_eq!(key_for("graysearch"), "search");
        assert_eq!(key_for("systemone"), "systemone");
    }

    #[test]
    fn parse_reads_package_and_single_bin() {
        let p = Project::parse(
            Path::new("/x"),
            "[package]\nname = \"gray-hello\"\nversion = \"0.2.0\"\ndescription = \"hi\"\n\n[[bin]]\nname = \"hello\"\npath = \"src/main.rs\"\n",
        )
        .unwrap();
        assert_eq!(
            (p.key.as_str(), p.version.as_str(), p.bin.as_str()),
            ("hello", "0.2.0", "hello")
        );
        assert_eq!(p.tarball_name(), "gray-hello-0.2.0.tar.gz");
    }

    #[test]
    fn parse_rejects_two_binaries() {
        let err = Project::parse(
            Path::new("/x"),
            "[package]\nname=\"a\"\nversion=\"1.0.0\"\n[[bin]]\nname=\"x\"\n[[bin]]\nname=\"y\"\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("exactly one executable"));
    }

    #[test]
    fn bin_defaults_to_package_name() {
        let p = Project::parse(
            Path::new("/x"),
            "[package]\nname=\"gray-z\"\nversion=\"1.0.0\"\n",
        )
        .unwrap();
        assert_eq!(p.bin, "gray-z");
    }

    #[test]
    fn key_validation_matches_registry_rules() {
        assert!(valid_key("hello-2"));
        assert!(!valid_key("Hello"));
        assert!(!valid_key("-x"));
        assert!(!valid_key("pi"));
        assert!(!valid_key(""));
    }
}
