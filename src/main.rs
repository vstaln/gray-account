//! gray-account CLI entry point.
//!
//! `manifest`, `login`, `whoami`, `logout`, `help` and the maker verbs
//! (`new`, `check`, `build`, `release`, `publish`) run as ordinary shell
//! commands; no arguments starts the NDJSON sidecar protocol over stdio.

use std::io::Write as _;

use gray_account::{Reply, account, maker, manifest, publish};
use serde::Deserialize;
use serde_json::{Value, json};

const USAGE: &str = "gray-account — gray.alignment.id account + plugin maker

usage: gray-account <command> [args]
       (the same commands run as `gray account <command>` in the gray CLI)

commands:
  login [code]      exchange an enrollment code and store the registry token
  whoami            show the stored token's identity
  logout            revoke and forget the stored token
  new <name> [--dir D] [--description TEXT] [--no-repo]
                    scaffold ~/grayplugins/gray-<name> and its GitHub repo
  check             build-free sanity checks: one entry point, manifest handshake
  build [--remote HOST]
                    static x86_64 musl release build + reproducible tarball
  release           tag v<version>, upload the tarball, re-download and verify
  publish [--remote HOST] [--dir PATH]
                    release and publish the plugin in --dir (default: cwd)
  manifest          print the plugin manifest (used by gray at install)
  help              show this text

with no arguments, gray-account runs the NDJSON sidecar protocol on stdio.";

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("manifest") => {
            println!("{}", serde_json::to_string(&manifest::manifest())?);
            Ok(())
        }
        Some("login") => run(account::login(args.get(1).map(String::as_str), true).await),
        Some("whoami") => run(account::whoami().await),
        Some("logout") => run(account::logout().await),
        Some("new" | "check" | "build" | "release") => {
            let cwd = std::env::current_dir()?;
            run(maker::dispatch(&args, &cwd))
        }
        Some("publish") => run(publish::publish_cli(&args[1..]).await),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(_) => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
        None => sidecar().await,
    }
}

fn run(result: anyhow::Result<String>) -> anyhow::Result<()> {
    match result {
        Ok(text) => {
            println!("{text}");
            Ok(())
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

/// NDJSON sidecar: one JSON request per stdin line, one reply per stdout
/// line. Notifications (no `id`) are ignored except `plugin/shutdown`,
/// which exits silently.
async fn sidecar() -> anyhow::Result<()> {
    let mut lines =
        tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(tokio::io::stdin()));
    let mut stdout = std::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        let has_id = request.id.is_some();
        if !has_id && request.method != "plugin/shutdown" {
            continue;
        }
        let reply = gray_account::handle(&request.method, &request.params, has_id).await;
        let frame = match reply {
            Reply::Result(ref result) | Reply::ResultThenExit(ref result) => {
                serde_json::to_string(&json!({
                    "id": request.id.unwrap_or(Value::Null),
                    "result": result,
                }))?
            }
            Reply::MethodNotFound => serde_json::to_string(&json!({
                "id": request.id.unwrap_or(Value::Null),
                "error": { "code": -32601, "message": "method not found" },
            }))?,
            Reply::Exit => return Ok(()),
        };
        writeln!(stdout, "{frame}")?;
        stdout.flush()?;
        if matches!(reply, Reply::ResultThenExit(_)) {
            return Ok(());
        }
    }
    Ok(())
}
