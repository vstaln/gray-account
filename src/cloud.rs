//! `gray cloud`: the hosted machine that belongs to this account.
//!
//! The machine lives on Maritime and is provisioned lazily by the registry
//! the first time it is needed. Auth is the same `~/.gray/registry-token.json`
//! the account commands use — there is no second credential, and
//! `gray cloud logout` is `gray logout`.

use serde::Deserialize;

use crate::account;

#[cfg(unix)]
mod attach {
    //! `gray cloud` on a tty: attach to the gray REPL running under a pty on
    //! this account's Maritime VM. The VM can only speak outbound HTTP, so
    //! both directions ride the registry's relay — keystrokes POST to
    //! /input and get long-polled by the VM; pty output POSTs from the VM
    //! get long-polled here on /output. Bytes are verbatim: the remote pty
    //! owns echo and line editing, so the screen IS gray's own UI.

    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use base64::Engine;
    use serde::Deserialize;

    /// A paste arrives as back-to-back stdin reads; one POST per burst keeps
    /// input latency at one round trip.
    const BATCH_WINDOW: Duration = Duration::from_millis(40);
    /// Server-side long-poll holds ~30s; client timeout stays above it.
    const POLL_TIMEOUT: Duration = Duration::from_secs(45);
    /// Local escape: Ctrl-] detaches; the remote session keeps running.
    const DETACH: u8 = 0x1d;

    const B64: base64::engine::general_purpose::GeneralPurpose =
        base64::engine::general_purpose::STANDARD;

    use crate::ui::{dim, line, shimmer_ansi as shimmer};

    /// Nudge the remote pty one row short and back: two SIGWINCHes make the
    /// remote gray repaint its whole screen, since attach only streams new
    /// output.
    fn repaint(tx: &mpsc::Sender<ToVm>) {
        let (c, r) = crossterm::terminal::size().unwrap_or((0, 0));
        let _ = tx.send(ToVm::Resize(c, r.saturating_sub(1).max(1)));
        let _ = tx.send(ToVm::Resize(c, r));
    }

    fn note(mark: &str, head: &str, tail: &str) {
        let mut out = std::io::stdout();
        let _ = write!(out, "\r\n{}\r\n", line(mark, head, tail));
        let _ = out.flush();
    }

    #[derive(Deserialize)]
    struct AttachOut {
        attach_id: i64,
        online: bool,
        out_seq: i64,
    }

    #[derive(Deserialize)]
    struct OutputOut {
        #[serde(default)]
        seq: i64,
        #[serde(default)]
        data: String,
        #[serde(default)]
        supplanted: bool,
        #[serde(default)]
        reset: bool,
        #[serde(default)]
        online: bool,
    }

    #[derive(Deserialize)]
    struct TtyStatus {
        online: bool,
    }

    enum ToVm {
        Bytes(Vec<u8>),
        Resize(u16, u16),
        Stop,
    }

    struct RemoteScreen;
    impl RemoteScreen {
        fn enter() -> anyhow::Result<Self> {
            crossterm::terminal::enable_raw_mode()?;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x1b[?1049h\x1b[H\x1b[2J");
            let _ = out.flush();
            Ok(Self)
        }
    }
    impl Drop for RemoteScreen {
        fn drop(&mut self) {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x1b[?1049l\x1b[?25h");
            let _ = out.flush();
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }

    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .build()
    }

    fn post(
        agent: &ureq::Agent,
        token: &str,
        path: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<ureq::Response> {
        let url = crate::account::endpoint(path)?;
        agent
            .post(&url)
            .set("authorization", &format!("Bearer {token}"))
            .send_json(body)
            .map_err(|e| anyhow::anyhow!("{path}: {e}"))
    }

    /// Input carries a client seq so a retry after a dropped connection
    /// can't double-type into the remote pty — the registry dedupes on it.
    fn post_input(
        agent: &ureq::Agent,
        token: &str,
        attach_id: i64,
        iseq: i64,
        data: &str,
    ) -> anyhow::Result<()> {
        let url = crate::account::endpoint("agent/tty/input")?;
        let body = serde_json::json!({ "data": data, "iseq": iseq, "attach": attach_id });
        let send = || -> Result<(), Box<ureq::Error>> {
            agent
                .post(&url)
                .set("authorization", &format!("Bearer {token}"))
                .send_json(body.clone())
                .map(drop)
                .map_err(Box::new)
        };
        match send() {
            Err(e) if matches!(*e, ureq::Error::Transport(_)) => {
                send().map_err(|e| anyhow::anyhow!("agent/tty/input: {e}"))?;
            }
            Err(e) => return Err(anyhow::anyhow!("agent/tty/input: {e}")),
            Ok(_) => {}
        }
        Ok(())
    }

    fn wait_online(http: &ureq::Agent, token: &str) -> anyhow::Result<()> {
        let base = crate::account::endpoint("agent/tty/status")?;
        let deadline = Instant::now() + Duration::from_secs(150);
        while Instant::now() < deadline {
            let resp = http
                .get(&format!("{base}?wait=25000"))
                .set("authorization", &format!("Bearer {token}"))
                .timeout(Duration::from_secs(40))
                .call();
            match resp {
                Ok(r) => {
                    if let Ok(st) = r.into_json::<TtyStatus>()
                        && st.online
                    {
                        return Ok(());
                    }
                }
                Err(ureq::Error::Status(404 | 405, _)) => return Ok(()),
                Err(_) => std::thread::sleep(Duration::from_secs(2)),
            }
        }
        anyhow::bail!("your machine never came online — try again in a minute")
    }

    pub fn run(token: &str) -> anyhow::Result<()> {
        let http = agent();
        let att: AttachOut =
            post(&http, token, "agent/tty/attach", serde_json::json!({}))?.into_json()?;

        let mut out = std::io::stdout();
        if !att.online {
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let ticker = {
                let stop = stop.clone();
                let start = Instant::now();
                std::thread::spawn(move || {
                    let mut out = std::io::stdout();
                    let _ = write!(out, "\x1b[?25l");
                    while !stop.load(Ordering::Relaxed) {
                        let elapsed = start.elapsed();
                        let tail = format!(
                            " {}s · first boot takes a couple of minutes",
                            elapsed.as_secs()
                        );
                        let _ = write!(
                            out,
                            "\r\x1b[2K{}{}",
                            shimmer(" ⬡ waking your Gray machine…", elapsed),
                            dim(&tail)
                        );
                        let _ = out.flush();
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    let _ = write!(out, "\x1b[?25h");
                })
            };
            let woke = wait_online(&http, token);
            stop.store(true, Ordering::Relaxed);
            let _ = ticker.join();
            write!(out, "\r\x1b[2K")?;
            woke?;
        }
        writeln!(
            out,
            "{}",
            line("◆", "gray cloud", " · connected · ctrl-] to detach")
        )?;
        out.flush()?;

        let _screen = RemoteScreen::enter()?;
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<ToVm>();

        {
            let tx = tx.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut stdin = std::io::stdin();
                let mut buf = [0u8; 4096];
                while !done.load(Ordering::Relaxed) {
                    match stdin.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(ToVm::Stop);
                            break;
                        }
                        Ok(n) => {
                            let bytes = &buf[..n];
                            if bytes.contains(&DETACH) {
                                let _ = tx.send(ToVm::Stop);
                                break;
                            }
                            if tx.send(ToVm::Bytes(bytes.to_vec())).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }

        {
            let tx = tx.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let mut last = crossterm::terminal::size().unwrap_or((0, 0));
                repaint(&tx);
                while !done.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                    let now = crossterm::terminal::size().unwrap_or((0, 0));
                    if now != last {
                        last = now;
                        let _ = tx.send(ToVm::Resize(now.0, now.1));
                    }
                }
            });
        }

        {
            let done = done.clone();
            let tx = tx.clone();
            let http_out = agent();
            let token = token.to_string();
            let attach_id = att.attach_id;
            std::thread::spawn(move || {
                let mut after = att.out_seq;
                let mut was_online = true;
                let mut repainted = false;
                let url = crate::account::endpoint("agent/tty/output");
                while !done.load(Ordering::Relaxed) {
                    let Ok(base) = &url else { break };
                    let resp = http_out
                        .get(&format!("{base}?after={after}&attach={attach_id}"))
                        .set("authorization", &format!("Bearer {token}"))
                        .timeout(POLL_TIMEOUT)
                        .call();
                    let payload: OutputOut = match resp {
                        Ok(r) => match r.into_json() {
                            Ok(p) => p,
                            Err(_) => continue,
                        },
                        Err(_) => {
                            std::thread::sleep(Duration::from_millis(800));
                            continue;
                        }
                    };
                    if payload.supplanted {
                        note(
                            "◇",
                            "detached",
                            " · another client attached to this machine",
                        );
                        let _ = tx.send(ToVm::Stop);
                        break;
                    }
                    if payload.online != was_online {
                        was_online = payload.online;
                        if was_online {
                            note("◆", "machine back online", "");
                            repaint(&tx);
                        } else {
                            note("◇", "machine unreachable", " · retrying…");
                        }
                    }
                    if payload.reset {
                        let mut out = std::io::stdout();
                        let _ = out.write_all(b"\x1b[2J\x1b[H");
                        let _ = out.flush();
                        repaint(&tx);
                    }
                    if let Ok(bytes) = B64.decode(&payload.data)
                        && !bytes.is_empty()
                    {
                        // The first bytes prove the VM is live even when the
                        // attach-time nudge reached it mid-wake.
                        if !repainted {
                            repainted = true;
                            repaint(&tx);
                        }
                        let mut out = std::io::stdout();
                        let _ = out.write_all(&bytes);
                        let _ = out.flush();
                    }
                    after = payload.seq;
                }
            });
        }

        let mut last_send = Instant::now() - BATCH_WINDOW;
        let mut pending: Vec<u8> = Vec::new();
        let mut iseq: i64 = 0;
        loop {
            match rx.recv_timeout(Duration::from_millis(25)) {
                Ok(ToVm::Bytes(b)) => pending.extend_from_slice(&b),
                Ok(ToVm::Resize(c, r)) => {
                    let _ = post(
                        &http,
                        token,
                        "agent/tty/input",
                        serde_json::json!({ "resize": { "cols": c, "rows": r } }),
                    );
                }
                Ok(ToVm::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if !pending.is_empty() && (pending.len() > 1024 || last_send.elapsed() >= BATCH_WINDOW)
            {
                let body = std::mem::take(&mut pending);
                iseq += 1;
                let _ = post_input(&http, token, att.attach_id, iseq, &B64.encode(&body));
                last_send = Instant::now();
            }
        }

        done.store(true, Ordering::Relaxed);
        let _ = post(&http, token, "agent/tty/detach", serde_json::json!({}));
        drop(_screen);
        writeln!(
            out,
            "\r\n\x1b[J{}",
            line(
                "◆",
                "detached",
                " · your cloud session keeps running · /cloud to reattach"
            )
        )?;
        Ok(())
    }
}

const USAGE: &str = "gray cloud — your hosted Gray machine

usage: gray cloud [command] [args]

commands:
  (no args)        attach to the live remote session — same UI, remote brain
  attach           same as bare `gray cloud`
  connect [code]   pair this terminal with a one-time code from the account page
  status           provision if needed and print the machine's state
  logout           revoke the stored token and forget the pairing
  <message>        one-shot chat with your machine (first call provisions it)\n  new <message>    same, but starts a fresh session — forgets prior context

detaching: Ctrl-] leaves the remote session running";

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

/// Live "thinking" line on stderr while the model round-trips — pure
/// perception work: the request itself doesn't change, but a moving counter
/// makes a 3–10s turn feel attended instead of hung. Clears itself on exit.
fn thinking_guard(interactive: bool) -> Option<tokio::task::JoinHandle<()>> {
    use std::io::Write;
    if !interactive || !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        return None;
    }
    Some(tokio::spawn(async move {
        let t0 = std::time::Instant::now();
        loop {
            let e = t0.elapsed().as_secs();
            let _ = write!(std::io::stderr(), "\r\x1b[2K · thinking {e}s");
            let _ = std::io::stderr().flush();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }))
}

fn drop_guard(g: Option<tokio::task::JoinHandle<()>>) {
    use std::io::Write;
    if let Some(h) = g {
        h.abort();
        let _ = write!(std::io::stderr(), "\r\x1b[2K");
        let _ = std::io::stderr().flush();
    }
}

/// Raw chat POST that keeps the status code, so the caller can provision on
/// first use instead of paying a round trip on every message.
async fn post_chat(
    client: &reqwest::Client,
    token: &str,
    message: &str,
    fresh: bool,
) -> anyhow::Result<ChatOut> {
    let url = account::endpoint("agent/chat")?;
    let send = || {
        client
            .post(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({ "message": message, "fresh": fresh }))
            .send()
    };
    let mut resp = send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        // No seat row yet — provision, then resend once.
        provision(client, token).await?;
        resp = send()
            .await
            .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
    }
    if !resp.status().is_success() {
        anyhow::bail!(account::error_message(resp).await);
    }
    Ok(resp.json().await?)
}

async fn chat(words: &[String], fresh: bool, interactive: bool) -> anyhow::Result<String> {
    let client = http_client()?;
    let token = require_token(interactive).await?;
    let message = words.join(" ");
    if message.trim().is_empty() {
        if !fresh {
            return status_text(&client, &token).await;
        }
        // Bare `new`: rotate the seat's conversation pointer — the next
        // one-shot, web chat, or gateway message lands in a fresh session.
        post_chat(&client, &token, "", true).await?;
        return Ok(
            "new conversation — your next message starts with a clean slate".to_string(),
        );
    }
    let guard = thinking_guard(interactive);
    let out = post_chat(&client, &token, &message, fresh).await;
    drop_guard(guard);
    Ok(out?.response.trim_end().to_string())
}

/// `gray cloud …` entry point: subcommand dispatch plus the chat fallback,
/// where anything that is not a known verb is the message.
pub async fn run(args: &[String], interactive: bool) -> anyhow::Result<String> {
    // `/verb` and `verb` are the same thing — slash style is what users type
    // inside the REPL and it must not fall through to the message path.
    let verb = args.first().map(|s| s.strip_prefix('/').unwrap_or(s));
    match verb {
        #[cfg(unix)]
        None | Some("attach")
            if interactive
                && std::io::IsTerminal::is_terminal(&std::io::stdout())
                && std::io::IsTerminal::is_terminal(&std::io::stdin()) =>
        {
            let token = require_token(interactive).await?;
            tokio::task::spawn_blocking(move || attach::run(&token))
                .await
                .map_err(|e| anyhow::anyhow!("attach worker: {e}"))??;
            Ok(String::new())
        }
        None | Some("status") | Some("attach") => {
            let client = http_client()?;
            let token = require_token(interactive).await?;
            status_text(&client, &token).await
        }
        Some("-h" | "--help" | "help") => Ok(USAGE.to_string()),
        Some("connect") => connect(args.get(1).map(String::as_str), interactive).await,
        Some("logout") => account::logout().await,
        Some("new") => chat(&args[1..], true, interactive).await,
        Some(_) => chat(args, false, interactive).await,
    }
}
