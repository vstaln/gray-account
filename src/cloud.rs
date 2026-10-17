//! `gray cloud`: the hosted machine that belongs to this account.
//!
//! The machine lives on Maritime and is provisioned lazily by the registry
//! the first time it is needed. Auth is the same `~/.gray/registry-token.json`
//! the account commands use — there is no second credential, and
//! `gray cloud logout` is `gray logout`.

use serde::Deserialize;

use crate::account;

const USAGE: &str = "gray cloud — your hosted Gray machine

usage: gray cloud [command] [args]

commands:
  connect [code]   pair this terminal with a one-time code from the account page
  status           provision if needed and print the machine's state
  logout           revoke the stored token and forget the pairing
  <message>        chat with your machine (first call provisions it)";

#[derive(Debug, Default, Deserialize)]
struct StatusOut {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    suspended: bool,
    #[serde(default)]
    agent_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProvisionOut {
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatOut {
    #[serde(default)]
    response: String,
}

/// Cloud calls ride a longer clock than account calls: a first chat includes
/// a cold VM boot plus a full agent turn, so 30s would cut live requests.
fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("gray-account/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()?)
}

/// The stored token, or the enroll flow that mints one. Non-interactive
/// callers (the `/cloud` slash command) cannot prompt, so they get a clear
/// error naming the way in.
async fn require_token(interactive: bool) -> anyhow::Result<String> {
    match account::load_token() {
        Some(token) => Ok(token),
        None if interactive => Ok(account::enroll(None, true).await?.0),
        None => anyhow::bail!("not logged in — run `gray login <code>` first"),
    }
}

async fn post<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<T> {
    let url = account::endpoint(path)?;
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(account::error_message(resp).await);
    }
    Ok(resp.json().await?)
}

async fn provision(client: &reqwest::Client, token: &str) -> anyhow::Result<ProvisionOut> {
    post(client, token, "agent/provision", &serde_json::json!({})).await
}

async fn fetch_status(client: &reqwest::Client, token: &str) -> anyhow::Result<StatusOut> {
    let url = account::endpoint("agent/status")?;
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(account::error_message(resp).await);
    }
    Ok(resp.json().await?)
}

async fn status_text(client: &reqwest::Client, token: &str) -> anyhow::Result<String> {
    let prov = provision(client, token).await?;
    let status = fetch_status(client, token).await.unwrap_or(StatusOut {
        status: prov.status.clone(),
        ..Default::default()
    });
    if status.suspended {
        return Ok("your Gray machine is suspended — check your seat at /plans".to_string());
    }
    Ok(match status.status.as_deref() {
        Some("active") => match status.agent_id.as_deref() {
            Some(id) => {
                format!("your Gray machine {id} is up — chat with it: gray cloud <message>")
            }
            None => "your Gray machine is up — chat with it: gray cloud <message>".to_string(),
        },
        Some(s) => {
            format!("your Gray machine is {s} — first boot takes a couple of minutes")
        }
        None => "your Gray machine is provisioned".to_string(),
    })
}

async fn connect(code: Option<&str>, interactive: bool) -> anyhow::Result<String> {
    let (token, acct) = account::enroll(code, interactive).await?;
    let client = http_client()?;
    let status = status_text(&client, &token).await?;
    Ok(format!("connected as {}\n{status}", acct.label()))
}

async fn chat(words: &[String], interactive: bool) -> anyhow::Result<String> {
    let client = http_client()?;
    let token = require_token(interactive).await?;
    let message = words.join(" ");
    if message.trim().is_empty() {
        return status_text(&client, &token).await;
    }
    provision(&client, &token).await?;
    let out: ChatOut = post(
        &client,
        &token,
        "agent/chat",
        &serde_json::json!({ "message": message }),
    )
    .await?;
    Ok(out.response.trim_end().to_string())
}

/// `gray cloud …` entry point: subcommand dispatch plus the chat fallback,
/// where anything that is not a known verb is the message.
pub async fn run(args: &[String], interactive: bool) -> anyhow::Result<String> {
    match args.first().map(String::as_str) {
        None | Some("status") => {
            let client = http_client()?;
            let token = require_token(interactive).await?;
            status_text(&client, &token).await
        }
        Some("-h" | "--help" | "help") => Ok(USAGE.to_string()),
        Some("connect") => connect(args.get(1).map(String::as_str), interactive).await,
        Some("logout") => account::logout().await,
        Some(_) => chat(args, interactive).await,
    }
}
