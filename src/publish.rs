//! `gray account publish`: release a plugin and submit it to the registry.
//!
//! CLI-only — no `/publish` sidecar command, because the build step takes
//! minutes and a `command/run` call would time out long before it finished.
//! The heavy lifting is gray-maker's check → build → release → publish
//! pipeline; this module adds the login gate, a registry preflight, and
//! adoption of a release the tag already ships instead of rebuilding it.
//!
//! Progress goes to stderr as it happens so a multi-minute run is never
//! silent; the returned string is the one-line result for stdout.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tokio::task::spawn_blocking;

use gray_maker::project::Project;
use gray_maker::{build, check, publish as maker_publish, release};

use crate::account::{self, Account};

/// `gray account publish [--remote HOST] [--dir PATH]`, resolved.
///
/// `dir` defaults to the caller's cwd: `Project::load` walks up from it to
/// the nearest Cargo.toml, so anywhere inside the plugin repo works.
pub struct PublishArgs {
    pub dir: PathBuf,
    pub remote: Option<String>,
}

/// Same position-based flag rules as gray-maker's dispatch: `--flag value`.
pub fn parse_args(args: &[String], cwd: &Path) -> anyhow::Result<PublishArgs> {
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    Ok(PublishArgs {
        dir: flag("--dir")
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.to_path_buf()),
        remote: flag("--remote"),
    })
}

/// What the registry index plus the caller's account say about a publish,
/// decided before any build starts.
#[derive(Debug, PartialEq, Eq)]
pub enum Preflight {
    /// The index already serves exactly this version — nothing to do.
    AlreadyPublished,
    /// New name, or an update to a plugin the caller may publish to.
    Proceed,
    /// The name belongs to another author the caller has no rights over.
    NotYours { author: String },
}

/// Pure preflight over `/plugins/index.json` and `/auth/me`. A plugin that
/// exists in the index but not in the caller's plugin list is off-limits —
/// `me.plugins` already contains the official set when the caller is a
/// maintainer, so the publish permission stays the server's decision.
pub fn preflight(key: &str, version: &str, index: &Value, me: &Account) -> Preflight {
    let entry = &index["plugins"][key];
    if entry.is_null() {
        return Preflight::Proceed;
    }
    if entry["version"].as_str() == Some(version) {
        return Preflight::AlreadyPublished;
    }
    if !me.plugins.iter().any(|p| p.name == key) {
        return Preflight::NotYours {
            author: entry["author"]
                .as_str()
                .unwrap_or("another author")
                .to_string(),
        };
    }
    Preflight::Proceed
}

/// `gray account publish` argv → the publish run.
pub async fn publish_cli(args: &[String]) -> anyhow::Result<String> {
    let cwd = std::env::current_dir()?;
    publish(&parse_args(args, &cwd)?).await
}

pub async fn publish(args: &PublishArgs) -> anyhow::Result<String> {
    eprintln!("publish: reading the project in {}", args.dir.display());
    let dir = args.dir.clone();
    let p = spawn_blocking(move || Project::load(&dir)).await??;
    eprintln!(
        "publish: {} {} (registry key '{}')",
        p.package, p.version, p.key
    );

    if account::load_token().is_none() {
        anyhow::bail!("not logged in — run `gray account login`");
    }

    eprintln!("publish: checking the registry before building anything");
    let me = account::fetch_account().await?;
    let index = fetch_index().await?;
    match preflight(&p.key, &p.version, &index, &me) {
        Preflight::AlreadyPublished => {
            return Ok(format!(
                "{} {} is already published — nothing to do\n  install: gray plugin install {}",
                p.key, p.version, p.key
            ));
        }
        Preflight::NotYours { author } => {
            anyhow::bail!(
                "'{}' belongs to {author}; you can only publish plugins you own",
                p.key
            );
        }
        Preflight::Proceed => {}
    }

    // A release another machine (or an earlier run) already shipped can be
    // adopted as-is: same tag, same asset, bytes verified by sha256. Only
    // when there is nothing to adopt do we spend minutes on a build.
    let adopted = {
        let p = p.clone();
        spawn_blocking(move || release::adopt(&p)).await??
    };
    match adopted {
        Some(report) => eprintln!("{report}"),
        None => {
            eprintln!("publish: no {} asset to adopt — building", p.tag());
            for step in [
                run({
                    let p = p.clone();
                    move || check::check(&p)
                })
                .await?,
                run({
                    let p = p.clone();
                    let remote = args.remote.clone();
                    move || build::build(&p, remote.as_deref()).map(|b| b.report())
                })
                .await?,
                run({
                    let p = p.clone();
                    move || release::release(&p)
                })
                .await?,
            ] {
                eprintln!("{step}");
            }
        }
    }

    eprintln!("publish: submitting to the registry");
    let submitted = {
        let p = p.clone();
        spawn_blocking(move || maker_publish::publish(&p)).await??
    };
    eprintln!("{submitted}");

    // Read the index back rather than trusting the 201: the confirmation
    // line should reflect what `gray plugin install` will actually see.
    let index = fetch_index().await?;
    let served = index["plugins"][&p.key]["version"].as_str();
    if served != Some(p.version.as_str()) {
        anyhow::bail!(
            "submitted, but the index shows {} {}",
            p.key,
            served.unwrap_or("no version")
        );
    }
    Ok(format!(
        "{} {} is published\n  install: gray plugin install {}",
        p.key, p.version, p.key
    ))
}

/// Run one blocking gray-maker step off the async executor.
async fn run<F>(step: F) -> anyhow::Result<String>
where
    F: FnOnce() -> anyhow::Result<String> + Send + 'static,
{
    spawn_blocking(step).await?
}

/// The public catalog index (`plugins.<key>.{version,author,...}`).
async fn fetch_index() -> anyhow::Result<Value> {
    let url = account::endpoint("plugins/index.json")?;
    let resp = account::http_client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!("{url} returned {}", resp.status());
    }
    Ok(resp.json().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::OwnedPlugin;
    use serde_json::json;

    fn index() -> Value {
        json!({"plugins": {
            "claude": {"version": "0.1.0", "author": "gray (github)"},
            "mine": {"version": "0.2.0", "author": "me (github)"},
        }})
    }

    fn me(plugins: &[&str]) -> Account {
        Account {
            plugins: plugins
                .iter()
                .map(|n| OwnedPlugin {
                    name: n.to_string(),
                    version: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn preflight_proceeds_on_a_name_the_index_does_not_have() {
        assert_eq!(
            preflight("new-plugin", "0.1.0", &index(), &me(&[])),
            Preflight::Proceed
        );
    }

    #[test]
    fn preflight_short_circuits_when_the_version_is_already_live() {
        assert_eq!(
            preflight("claude", "0.1.0", &index(), &me(&[])),
            Preflight::AlreadyPublished
        );
        // Even the owner gets AlreadyPublished — the registry would 409 anyway.
        assert_eq!(
            preflight("mine", "0.2.0", &index(), &me(&["mine"])),
            Preflight::AlreadyPublished
        );
    }

    #[test]
    fn preflight_refuses_a_plugin_the_caller_does_not_own() {
        assert_eq!(
            preflight("claude", "0.2.0", &index(), &me(&["mine"])),
            Preflight::NotYours {
                author: "gray (github)".to_string()
            }
        );
    }

    #[test]
    fn preflight_proceeds_on_own_plugin_and_on_the_official_set_for_maintainers() {
        assert_eq!(
            preflight("mine", "0.3.0", &index(), &me(&["mine"])),
            Preflight::Proceed
        );
        // The server adds the official plugins to a maintainer's plugins
        // list, so "in my list" is the whole client-side check.
        let mut maintainer = me(&["mine", "claude"]);
        maintainer.maintainer = true;
        assert_eq!(
            preflight("claude", "0.2.0", &index(), &maintainer),
            Preflight::Proceed
        );
    }

    #[test]
    fn args_default_the_dir_to_cwd() {
        let parsed = parse_args(&[], Path::new("/plugins/gray-x")).unwrap();
        assert_eq!(parsed.dir, PathBuf::from("/plugins/gray-x"));
        assert_eq!(parsed.remote, None);
    }

    #[test]
    fn args_take_remote_and_dir() {
        let parsed = parse_args(
            &[
                "publish".to_string(),
                "--remote".to_string(),
                "maid".to_string(),
                "--dir".to_string(),
                "/elsewhere".to_string(),
            ],
            Path::new("/plugins/gray-x"),
        )
        .unwrap();
        assert_eq!(parsed.dir, PathBuf::from("/elsewhere"));
        assert_eq!(parsed.remote.as_deref(), Some("maid"));
    }
}
