//! `/cloud` inside gray: this session's messages go to the hosted machine.
//!
//! No screen takeover. `/cloud` flips the session into cloud mode and wakes
//! the machine in the background; the `input/submit` hook then claims each
//! submitted message (gray's own composer and transcript stay in charge),
//! and replies plus progress land in the transcript via `host/say`. Messages
//! typed during a cold wake queue and send once the machine is online.
//! `/cloud off` hands the session back to the local model.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::account;
use crate::cloud::{Phase, phase_of};

/// How long a wake may take before we give up and turn the mode off. A full
/// recreate is 45–95s; Maritime's fc-manager can loop for minutes.
const WAKE_DEADLINE: Duration = Duration::from_secs(240);
const POLL_EVERY: Duration = Duration::from_secs(2);
/// "still waking" notes: first after this, then once per interval.
const NOTE_EVERY: Duration = Duration::from_secs(30);
/// A 503 right after `active` means the machine's gray isn't listening yet.
const RETRY_EVERY: Duration = Duration::from_secs(3);
const RETRY_DEADLINE: Duration = Duration::from_secs(120);
/// A remote turn runs tools; give it room, and say so if it's slow.
const CHAT_TIMEOUT: Duration = Duration::from_secs(900);
const SLOW_NOTE: Duration = Duration::from_secs(20);

/// What a registry call can come back with, as far as the worker cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    /// Not up yet (503/409) or the network blipped — try again shortly.
    Retry(String),
    /// Retrying won't help (401/402/403/…): say it and leave cloud mode.
    Fatal(String),
}

/// The registry surface the worker drives. A trait so tests can script it.
pub trait Machine: Send + Sync + 'static {
    /// Provision if needed; returns Maritime's status string.
    fn provision(&self) -> impl Future<Output = Result<Option<String>, Fail>> + Send;
    fn status(&self) -> impl Future<Output = Result<Option<String>, Fail>> + Send;
    /// Kick a sleeping machine awake (ingress wakes it).
    fn nudge(&self) -> impl Future<Output = ()> + Send;
    fn chat(&self, message: &str, fresh: bool)
    -> impl Future<Output = Result<String, Fail>> + Send;
}

/// What the worker hands to the transcript. Live: `host/say` frames.
pub trait Say: Send + Sync + 'static {
    fn say(&self, text: &str);
}

enum Job {
    Message(String),
    Fresh(Option<String>),
}

struct Mode {
    /// The gray session this mode belongs to; empty until gray has one.
    sid: String,
    generation: u64,
    online: Arc<AtomicBool>,
    tx: mpsc::UnboundedSender<Job>,
}

static MODE: Mutex<Option<Mode>> = Mutex::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn mode() -> std::sync::MutexGuard<'static, Option<Mode>> {
    MODE.lock().unwrap_or_else(|e| e.into_inner())
}

fn current(generation: u64) -> bool {
    mode().as_ref().is_some_and(|m| m.generation == generation)
}

/// Does `sid` belong to the cloud-mode session? An empty pinned sid (mode
/// turned on before gray minted one) adopts the first real one it sees.
fn claims(m: &mut Mode, sid: &str) -> bool {
    if m.sid.is_empty() {
        m.sid = sid.to_string();
        return true;
    }
    sid.is_empty() || m.sid == sid
}

pub fn is_on() -> bool {
    mode().is_some()
}

/// The `input/submit` hook. Fast path when cloud mode is off: `{}` (pass).
pub fn input_submit(params: &Value, say: &dyn Say) -> Value {
    let text = params.get("text").and_then(Value::as_str).unwrap_or_default();
    let sid = params
        .pointer("/session/id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut guard = mode();
    let Some(m) = guard.as_mut() else {
        return json!({});
    };
    if !claims(m, sid) {
        // A different session (gray /new, /resume) is local again.
        *guard = None;
        return json!({});
    }
    let text = text.trim();
    if text.is_empty() {
        return json!({ "handled": true });
    }
    let online = m.online.load(Ordering::Relaxed);
    if m.tx.send(Job::Message(text.to_string())).is_err() {
        *guard = None;
        return json!({});
    }
    drop(guard);
    if !online {
        say.say("· queued — sends once your machine is up");
    }
    json!({ "handled": true })
}

/// Turn cloud mode on for `sid` (idempotent) and optionally queue a job.
/// Returns the line the slash command replies with.
fn turn_on<M: Machine, S: Say>(sid: &str, machine: M, say: S, first: Option<Job>) -> String {
    let mut guard = mode();
    if let Some(m) = guard.as_mut()
        && claims(m, sid)
    {
        let online = m.online.load(Ordering::Relaxed);
        let queued = first.is_some();
        if let Some(job) = first {
            let _ = m.tx.send(job);
        }
        return match (online, queued) {
            (_, true) => String::new(),
            (true, false) => "◆ cloud mode is on — messages go to your machine · /cloud off returns".into(),
            (false, false) => "◆ still waking your machine — type away, messages queue".into(),
        };
    }
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let (tx, rx) = mpsc::unbounded_channel();
    if let Some(job) = first {
        let _ = tx.send(job);
    }
    let online = Arc::new(AtomicBool::new(false));
    *guard = Some(Mode {
        sid: sid.to_string(),
        generation,
        online: online.clone(),
        tx,
    });
    drop(guard);
    tokio::spawn(worker(machine, say, rx, generation, online));
    "◆ cloud mode — your messages go to your machine · /cloud off returns to local".into()
}

fn turn_off() -> String {
    match mode().take() {
        Some(_) => "◆ back to local — /cloud reconnects (your machine keeps its session)".into(),
        None => "cloud mode is already off".into(),
    }
}

async fn worker<M: Machine, S: Say>(
    machine: M,
    say: S,
    mut rx: mpsc::UnboundedReceiver<Job>,
    generation: u64,
    online: Arc<AtomicBool>,
) {
    let speak = |text: &str| {
        if current(generation) {
            say.say(text);
        }
    };
    match wake(&machine, &speak, generation).await {
        Ok(()) => online.store(true, Ordering::Relaxed),
        Err(why) => {
            speak(&format!("◇ {why} — cloud mode off, /cloud to retry"));
            if current(generation) {
                *mode() = None;
            }
            return;
        }
    }
    while let Some(job) = rx.recv().await {
        if !current(generation) {
            return;
        }
        let (message, fresh) = match job {
            Job::Message(m) => (m, false),
            Job::Fresh(None) => match machine.chat("", true).await {
                Ok(_) => {
                    speak("◆ new conversation on your machine");
                    continue;
                }
                Err(Fail::Retry(e) | Fail::Fatal(e)) => {
                    speak(&format!("◇ couldn't start a new conversation · {e}"));
                    continue;
                }
            },
            Job::Fresh(Some(m)) => (m, true),
        };
        match send(&machine, &message, fresh, &speak).await {
            Ok(reply) => speak(&reply_block(&reply)),
            Err(Fail::Retry(e)) => speak(&format!("◇ not sent · {e} — send it again")),
            Err(Fail::Fatal(e)) => {
                speak(&format!("◇ {e} — cloud mode off"));
                if current(generation) {
                    *mode() = None;
                }
                return;
            }
        }
    }
}

/// Provision, then wait for `active`, narrating only real transitions.
async fn wake<M: Machine>(
    machine: &M,
    speak: &impl Fn(&str),
    generation: u64,
) -> Result<(), String> {
    let first = machine.provision().await.map_err(fail_text)?;
    let mut phase = match machine.status().await {
        Ok(s) => phase_of(s.as_deref()),
        Err(_) => phase_of(first.as_deref()),
    };
    if phase == Phase::Online {
        speak("◆ your machine is online");
        return Ok(());
    }
    speak(&format!("◆ your machine · {}", phase.doing()));
    machine.nudge().await;
    let start = Instant::now();
    let mut next_note = NOTE_EVERY;
    loop {
        if !current(generation) {
            return Err("cancelled".into());
        }
        tokio::time::sleep(POLL_EVERY).await;
        let now = match machine.status().await {
            Ok(s) => phase_of(s.as_deref()),
            Err(Fail::Fatal(e)) => return Err(e),
            Err(Fail::Retry(_)) => phase.clone(),
        };
        let secs = start.elapsed().as_secs();
        if now == Phase::Online {
            speak(&format!("◆ your machine is online · woke in {secs}s"));
            return Ok(());
        }
        if now != phase {
            if matches!(now, Phase::Failed(_)) {
                machine.nudge().await;
            }
            speak(&format!("· {} · {secs}s", now.doing()));
            phase = now;
        } else if start.elapsed() >= next_note {
            speak(&format!("· still waking · {secs}s"));
            next_note += NOTE_EVERY;
        }
        if start.elapsed() >= WAKE_DEADLINE {
            return Err(format!("your machine didn't come up in {secs}s"));
        }
    }
}

/// One message, retrying while the machine's gray finishes starting. A
/// slow turn gets one quiet note so silence never reads as hung.
async fn send<M: Machine>(
    machine: &M,
    message: &str,
    fresh: bool,
    speak: &impl Fn(&str),
) -> Result<String, Fail> {
    let start = Instant::now();
    loop {
        let call = machine.chat(message, fresh);
        tokio::pin!(call);
        let res = tokio::select! {
            r = &mut call => r,
            _ = tokio::time::sleep(SLOW_NOTE) => {
                speak("· your machine is working on it…");
                call.await
            }
        };
        match res {
            Err(Fail::Retry(_)) if start.elapsed() < RETRY_DEADLINE => {
                tokio::time::sleep(RETRY_EVERY).await;
            }
            other => return other,
        }
    }
}

fn fail_text(f: Fail) -> String {
    match f {
        Fail::Retry(e) | Fail::Fatal(e) => e,
    }
}

/// A reply as transcript lines: a mark on the first line, the rest aligned
/// under it (gray indents and dims `host/say` text itself).
pub fn reply_block(reply: &str) -> String {
    let reply = reply.trim_end();
    if reply.trim().is_empty() {
        return "◆ (no reply)".into();
    }
    let mut out = String::new();
    for (i, line) in reply.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(if i == 0 { "◆ " } else { "  " });
        out.push_str(line);
    }
    out
}

/// In-REPL `/cloud …`. `None` means "not a mode verb" — the caller falls
/// back to the plain one-shot commands (`status`, `connect`, `logout`, help).
pub fn slash(argv: &[String], sid: &str) -> Option<Value> {
    let verb = argv
        .first()
        .map(|s| s.strip_prefix('/').unwrap_or(s).to_string());
    let rest = || argv.get(1..).unwrap_or_default().join(" ").trim().to_string();
    let live = || {
        let token = account::load_token();
        token.map(|t| (Live::new(t), HostSay))
    };
    const NO_LOGIN: &str = "not logged in — run /login <code> first (codes at https://gray.alignment.id/account)";
    let text = match verb.as_deref() {
        Some("off" | "stop" | "local" | "exit" | "detach") => turn_off(),
        Some("tty") => {
            return Some(json!({ "terminal": ["cloud", "tty"], "text": "/cloud tty needs an interactive terminal" }));
        }
        Some("status" | "connect" | "logout" | "help" | "-h" | "--help") => return None,
        None | Some("on" | "attach") => match live() {
            Some((m, s)) => turn_on(sid, m, s, None),
            None => NO_LOGIN.into(),
        },
        Some("new") => match live() {
            Some((m, s)) => {
                let msg = rest();
                turn_on(sid, m, s, Some(Job::Fresh((!msg.is_empty()).then_some(msg))))
            }
            None => NO_LOGIN.into(),
        },
        Some(_) => match live() {
            Some((m, s)) => turn_on(sid, m, s, Some(Job::Message(argv.join(" ")))),
            None => NO_LOGIN.into(),
        },
    };
    Some(json!({ "text": text }))
}

/// `host/say` through the sidecar's shared stdout.
#[derive(Clone, Copy)]
pub struct HostSay;

impl Say for HostSay {
    fn say(&self, text: &str) {
        crate::host_say(text);
    }
}

/// The real registry, over the stored account token.
pub struct Live {
    token: String,
    client: reqwest::Client,
}

impl Live {
    pub fn new(token: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("gray-account/", env!("CARGO_PKG_VERSION")))
            .timeout(CHAT_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self { token, client }
    }

    async fn call(&self, get: bool, path: &str, body: Value) -> Result<Value, Fail> {
        let url = account::endpoint(path).map_err(|e| Fail::Fatal(e.to_string()))?;
        let req = if get {
            self.client.get(&url).timeout(Duration::from_secs(30))
        } else {
            self.client.post(&url).json(&body)
        };
        let resp = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| Fail::Retry(format!("could not reach the registry: {e}")))?;
        let code = resp.status().as_u16();
        if resp.status().is_success() {
            return resp
                .json()
                .await
                .map_err(|e| Fail::Retry(format!("bad reply: {e}")));
        }
        let msg = account::error_message(resp).await;
        Err(match code {
            401 => Fail::Fatal("not logged in — run /login <code>".into()),
            402 | 403 => Fail::Fatal(msg),
            404 if path == "agent/chat" => Fail::Retry(msg),
            409 | 429 | 500..=599 => Fail::Retry(msg),
            _ => Fail::Fatal(msg),
        })
    }
}

fn status_field(v: &Value) -> Option<String> {
    v.get("status").and_then(Value::as_str).map(str::to_string)
}

impl Machine for Live {
    async fn provision(&self) -> Result<Option<String>, Fail> {
        self.call(false, "agent/provision", json!({}))
            .await
            .map(|v| status_field(&v))
    }

    async fn status(&self) -> Result<Option<String>, Fail> {
        let v = self.call(true, "agent/status", Value::Null).await?;
        if v.get("suspended").and_then(Value::as_bool) == Some(true) {
            return Err(Fail::Fatal("your seat is suspended — see /plans".into()));
        }
        Ok(status_field(&v))
    }

    async fn nudge(&self) {
        // tty/attach wakes a sleeping VM and arms the registry keepalive.
        let _ = self.call(false, "agent/tty/attach", json!({})).await;
    }

    async fn chat(&self, message: &str, fresh: bool) -> Result<String, Fail> {
        let v = self
            .call(false, "agent/chat", json!({ "message": message, "fresh": fresh }))
            .await?;
        Ok(v.get("response")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}

#[cfg(test)]
#[path = "cloud_mode_tests.rs"]
mod tests;
