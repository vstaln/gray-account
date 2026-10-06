//! gray-account's maker verbs: scaffold, check, build, release and publish
//! gray plugins.
//!
//! Every step is a plain function returning a human-readable report, so the
//! shell CLI (`gray account <cmd>`) and the REPL command (`/maker <cmd>`)
//! share one implementation.
//!
//! With no arguments the gray-account binary runs gray's NDJSON sidecar
//! protocol on stdio; the `/maker` command is routed here by the sidecar.
//! `publish` itself lives in [`crate::publish`] — the account plugin's own
//! login-gated pipeline — while [`publish::publish`] is the registry
//! submission step it ends with.

pub mod build;
pub mod check;
pub mod project;
pub mod publish;
pub mod release;
pub mod run;
pub mod template;

use std::path::Path;

/// Usage for the maker verbs. `/maker` help shows the same text with the
/// slash command as the program name.
pub(crate) const USAGE: &str = "make gray plugins

usage: gray account <command> [args]

commands:
  new <name> [--dir D] [--description TEXT] [--no-repo]
                    scaffold ~/grayplugins/gray-<name> and its GitHub repo
  check             build-free sanity checks: one entry point, manifest handshake
  build [--remote HOST]
                    static x86_64 musl release build + reproducible tarball
  release           tag v<version>, upload the tarball, re-download and verify
  publish           release and submit the plugin to the gray registry

check/build/release/publish act on the plugin in the current directory.";

/// The maker verbs shared by the shell CLI and `/maker`: `new`, `check`,
/// `build`, `release`. `publish` is not routed here — the top-level
/// `gray account publish` runs the full pipeline in [`crate::publish`].
pub fn dispatch(args: &[String], cwd: &Path) -> anyhow::Result<String> {
    let flag = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    match args.first().map(String::as_str) {
        Some("new") => template::new_plugin(&template::NewOpts {
            name: args
                .get(1)
                .filter(|a| !a.starts_with('-'))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("usage: new <name> [--dir D] [--no-repo]"))?,
            dir: flag("--dir").map(Into::into),
            description: flag("--description"),
            create_repo: !args.iter().any(|a| a == "--no-repo"),
        }),
        Some("check") => check::check(&project::Project::load(cwd)?),
        Some("build") => build::build(&project::Project::load(cwd)?, flag("--remote").as_deref())
            .map(|b| b.report()),
        Some("release") => release::release(&project::Project::load(cwd)?),
        Some("help") | Some("-h") | Some("--help") => Ok(USAGE.to_string()),
        Some(other) => {
            anyhow::bail!("unknown command '{other}'\n\n{USAGE}");
        }
        None => anyhow::bail!("{USAGE}"),
    }
}

/// A `/maker …` typed in gray's REPL. `new` runs inline (it is seconds);
/// the rest take minutes, so reply with an agent prompt instead of running
/// inside the command timeout.
pub fn slash(params: &serde_json::Value) -> serde_json::Value {
    let argv: Vec<&str> = params["argv"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let cwd = params["session"]["cwd"]
        .as_str()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let owned: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    match argv.first().copied() {
        None | Some("help") => {
            serde_json::json!({ "text": USAGE.replace("gray account ", "/maker ") })
        }
        Some("new") => match dispatch(&owned, &cwd) {
            Ok(report) => serde_json::json!({ "text": report }),
            Err(e) => serde_json::json!({ "text": format!("maker new failed: {e:#}") }),
        },
        Some("check" | "build" | "release" | "publish") => serde_json::json!({
            "prompt": format!(
                "Run `gray account {}` with bash in {} (it can take minutes). If it fails, read the error, fix the plugin, commit, and re-run. Report the result in one short paragraph.",
                argv.join(" "), cwd.display()
            )
        }),
        Some(other) => serde_json::json!({
            "text": format!("unknown /maker command '{other}'\n\n{}", USAGE.replace("gray account ", "/maker "))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slash_help_and_unknown_are_text() {
        let h = slash(&json!({"argv": []}));
        assert!(
            h["text"]
                .as_str()
                .unwrap()
                .contains("usage: /maker <command>")
        );
        let u = slash(&json!({"argv": ["bogus"]}));
        assert!(u["text"].as_str().unwrap().contains("unknown"));
    }

    #[test]
    fn slash_check_is_an_agent_prompt() {
        let r = slash(
            &json!({"argv": ["publish", "--remote", "maid"], "session": {"cwd": "/p/gray-w"}}),
        );
        let p = r["prompt"].as_str().unwrap();
        assert!(p.contains("`gray account publish --remote maid`"));
        assert!(p.contains("/p/gray-w"));
    }
}
