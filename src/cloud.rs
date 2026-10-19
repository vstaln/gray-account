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

    pub(super) struct RemoteScreen;
    impl RemoteScreen {
        pub(super) fn enter() -> anyhow::Result<Self> {
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

    pub(super) fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .build()
    }

    pub(super) fn post(
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

    pub(super) fn wait_online(http: &ureq::Agent, token: &str) -> anyhow::Result<()> {
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

#[cfg(unix)]
mod chat {
    //! `gray cloud` on a tty: a LOCAL chat loop against `agent/chat`.
    //! Keystrokes echo instantly and the server only sees whole submitted
    //! lines — the old tty byte-stream turned every letter into a network
    //! round trip. `gray cloud tty` keeps the raw remote screen for when
    //! the real remote TUI is wanted.

    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::ChatOut;
    use super::attach::{self, RemoteScreen};
    use crate::ui::{dim, line};

    /// Local escapes: Ctrl-] detaches; Ctrl-D/Ctrl-C do too — muscle
    /// memory should never trap the terminal.
    const DETACH: u8 = 0x1d;
    const EOF_KEY: u8 = 0x04;
    const INT_KEY: u8 = 0x03;
    /// A whole turn can ride a cold VM boot: the same clock the async
    /// chat path uses.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
    /// A lone ESC might be the Escape key, not a sequence start: swallow
    /// CSI/SS3 bytes only while they arrive together.
    const ESC_GRACE: Duration = Duration::from_millis(40);

    /// Input from the stdin pump.
    pub(crate) enum In {
        Bytes(Vec<u8>),
        /// stdin EOF or read error: detach.
        End,
    }

    /// Byte-level input events the line editor reacts to.
    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Ev {
        Char(char),
        Submit,
        Detach,
    }

    /// Raw bytes → edit events: UTF-8 accumulation, CSI/SS3 swallow,
    /// bracketed-paste (pasted newlines never submit).
    #[derive(Default)]
    pub(crate) struct Parser {
        utf8: Vec<u8>,
        esc: Option<(Instant, Vec<u8>)>,
        paste: bool,
    }

    impl Parser {
        pub(crate) fn feed(&mut self, b: u8, ev: &mut Vec<Ev>) {
            if let Some((started, seq)) = self.esc.as_mut() {
                if seq.is_empty() && b != b'[' && b != b'O' {
                    self.esc = None;
                } else {
                    seq.push(b);
                    let done = match seq[0] {
                        b'[' => seq.len() > 1 && (0x40..=0x7e).contains(&b),
                        _ => seq.len() > 1,
                    };
                    if done {
                        match seq.as_slice() {
                            b"[200~" => self.paste = true,
                            b"[201~" => self.paste = false,
                            _ => {}
                        }
                        self.esc = None;
                    } else if started.elapsed() > ESC_GRACE {
                        self.esc = None;
                    }
                    return;
                }
            }
            match b {
                0x1b => self.esc = Some((Instant::now(), Vec::new())),
                DETACH | EOF_KEY | INT_KEY => ev.push(Ev::Detach),
                b'\r' | b'\n' if !self.paste => ev.push(Ev::Submit),
                _ => {
                    self.utf8.push(b);
                    if utf8_complete(&self.utf8) {
                        for c in String::from_utf8_lossy(&self.utf8).chars() {
                            ev.push(Ev::Char(if c == '\r' || c == '\n' { ' ' } else { c }));
                        }
                        self.utf8.clear();
                    }
                }
            }
        }
    }

    fn utf8_complete(buf: &[u8]) -> bool {
        let Some(&b0) = buf.first() else { return false };
        let need = if b0 < 0x80 {
            1
        } else if b0 >= 0xf0 {
            4
        } else if b0 >= 0xe0 {
            3
        } else if b0 >= 0xc0 {
            2
        } else {
            1
        };
        buf.len() >= need
    }

    /// `/new` rotates the conversation; `/exit` detaches; anything else is
    /// the message. Returns `(fresh, message)`.
    fn split_verb(line: &str) -> (bool, &str) {
        match line.strip_prefix("/new") {
            Some(rest) if rest.is_empty() || rest.starts_with(' ') => (true, rest.trim_start()),
            _ => (false, line),
        }
    }

    fn prompt(out: &mut dyn Write, in_flight: Option<Instant>, buf: &str) -> std::io::Result<()> {
        let status = in_flight
            .map(|t| format!("· thinking {}s", t.elapsed().as_secs()))
            .unwrap_or_default();
        write!(out, "\r\x1b[2K")?;
        if !status.is_empty() {
            write!(out, "{}  ", dim(&status))?;
        }
        write!(out, "{} {buf}", dim("›"))?;
        out.flush()
    }

    /// Move the submitted line into the transcript and drop to a new row.
    fn echo_sent(out: &mut dyn Write, msg: &str) -> std::io::Result<()> {
        write!(out, "\r\x1b[2K{} {msg}\r\n", dim("›"))
    }

    /// The whole loop, transport-free for tests: input bytes arrive on
    /// `rx`, replies render on `out`, and `send` does one request per
    /// submitted line. Local echo only — the network sees Enter, never
    /// keystrokes.
    pub(crate) fn loop_io(
        rx: &mpsc::Receiver<In>,
        out: &mut dyn Write,
        send: impl FnMut(&str) -> Result<String, String> + Send + 'static,
    ) -> std::io::Result<()> {
        let (job_tx, job_rx) = mpsc::channel::<String>();
        let (res_tx, res_rx) = mpsc::channel::<Result<String, String>>();
        let mut send = send;
        std::thread::spawn(move || {
            while let Ok(msg) = job_rx.recv() {
                if res_tx.send(send(&msg)).is_err() {
                    break;
                }
            }
        });

        let mut parser = Parser::default();
        let mut line_buf = String::new();
        let mut pending: Option<String> = None;
        let mut in_flight: Option<Instant> = None;
        let mut shown_secs = u64::MAX;

        let drain_replies = |out: &mut dyn Write,
                             in_flight: &mut Option<Instant>,
                             pending: &mut Option<String>,
                             shown_secs: &mut u64,
                             line_buf: &str|
         -> std::io::Result<()> {
            while let Ok(res) = res_rx.try_recv() {
                *in_flight = None;
                write!(out, "\r\x1b[2K")?;
                match res {
                    Ok(text) => {
                        for l in text.trim_end().lines() {
                            write!(out, "{l}\r\n")?;
                        }
                    }
                    Err(e) => write!(out, "{}\r\n", line("◇", "send failed", &format!(" · {e}")))?,
                }
                if let Some(msg) = pending.take() {
                    *in_flight = Some(Instant::now());
                    *shown_secs = u64::MAX;
                    echo_sent(out, &msg)?;
                    let _ = job_tx.send(msg);
                }
                prompt(out, *in_flight, line_buf)?;
            }
            Ok(())
        };

        prompt(out, None, &line_buf)?;

        loop {
            drain_replies(
                out,
                &mut in_flight,
                &mut pending,
                &mut shown_secs,
                &line_buf,
            )?;

            let chunk = match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(In::Bytes(b)) => b,
                Ok(In::End) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => Vec::new(),
            };

            let mut evs = Vec::new();
            for b in chunk {
                parser.feed(b, &mut evs);
            }
            for ev in evs {
                match ev {
                    Ev::Detach => return Ok(()),
                    Ev::Char(c) => {
                        line_buf.push(c);
                        prompt(out, in_flight, &line_buf)?;
                    }
                    Ev::Submit => {
                        let msg = line_buf.trim().to_string();
                        line_buf.clear();
                        if msg.is_empty() {
                            prompt(out, in_flight, &line_buf)?;
                        } else if in_flight.is_some() {
                            echo_sent(out, &msg)?;
                            pending = Some(msg);
                            prompt(out, in_flight, &line_buf)?;
                        } else {
                            in_flight = Some(Instant::now());
                            shown_secs = u64::MAX;
                            echo_sent(out, &msg)?;
                            let _ = job_tx.send(msg);
                            prompt(out, in_flight, &line_buf)?;
                        }
                    }
                }
            }

            if let Some(t) = in_flight {
                let secs = t.elapsed().as_secs();
                if secs != shown_secs {
                    shown_secs = secs;
                    prompt(out, in_flight, &line_buf)?;
                }
            }
        }
        // A reply that landed just before detach still prints — the turn
        // was paid for; give an in-flight one a short grace to arrive.
        let deadline = Instant::now() + Duration::from_secs(2);
        while in_flight.is_some() && Instant::now() < deadline {
            drain_replies(
                out,
                &mut in_flight,
                &mut pending,
                &mut shown_secs,
                &line_buf,
            )?;
            if in_flight.is_some() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        drain_replies(
            out,
            &mut in_flight,
            &mut pending,
            &mut shown_secs,
            &line_buf,
        )?;
        Ok(())
    }

    /// One `agent/chat` POST; a 404 provisions the seat first, the same
    /// retry the async `post_chat` makes.
    fn send(http: &ureq::Agent, token: &str, msg: &str) -> Result<String, String> {
        let chat = crate::account::endpoint("agent/chat").map_err(|e| e.to_string())?;
        let prov = crate::account::endpoint("agent/provision").map_err(|e| e.to_string())?;
        send_to(http, token, &chat, &prov, msg)
    }

    pub(crate) fn send_to(
        http: &ureq::Agent,
        token: &str,
        chat_url: &str,
        prov_url: &str,
        msg: &str,
    ) -> Result<String, String> {
        let (fresh, message) = split_verb(msg);
        let body = serde_json::json!({ "message": message, "fresh": fresh });
        let call = || {
            http.post(chat_url)
                .set("authorization", &format!("Bearer {token}"))
                .timeout(REQUEST_TIMEOUT)
                .send_json(body.clone())
        };
        let resp = match call() {
            Ok(r) => r,
            Err(ureq::Error::Status(404, _)) => {
                http.post(prov_url)
                    .set("authorization", &format!("Bearer {token}"))
                    .send_json(serde_json::json!({}))
                    .map_err(|e| e.to_string())?;
                call().map_err(|e| e.to_string())?
            }
            Err(ureq::Error::Status(code, r)) => {
                let detail = r.into_string().unwrap_or_default();
                return Err(format!("{code} {}", detail.trim()).trim().to_string());
            }
            Err(e) => return Err(e.to_string()),
        };
        resp.into_json::<ChatOut>()
            .map(|c| c.response.trim_end().to_string())
            .map_err(|e| e.to_string())
    }

    /// `gray cloud` / `gray cloud attach`: connect banner immediately,
    /// machine-online wait, then the local chat loop.
    pub fn run(token: &str) -> anyhow::Result<()> {
        let http = attach::agent();
        let screen = RemoteScreen::enter()?;
        let mut out = std::io::stdout();
        write!(out, "{}\r\n", line("◆", "gray cloud", " · connecting…"))?;
        out.flush()?;

        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let stop = stop.clone();
            let start = Instant::now();
            std::thread::spawn(move || {
                let mut out = std::io::stdout();
                while !stop.load(Ordering::Relaxed) {
                    let s = format!(
                        "· {}s — first boot takes a couple of minutes",
                        start.elapsed().as_secs()
                    );
                    let _ = write!(out, "\r\x1b[2K{}", dim(&s));
                    let _ = out.flush();
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
        };
        let res = attach::wait_online(&http, token);
        stop.store(true, Ordering::Relaxed);
        let _ = ticker.join();
        write!(out, "\r\x1b[2K")?;
        res?;
        write!(
            out,
            "{}\r\n\r\n",
            line(
                "◆",
                "gray cloud",
                " · connected · type + enter to send · ctrl-] to detach"
            )
        )?;
        out.flush()?;

        let (tx, rx) = mpsc::channel::<In>();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(In::End);
                        break;
                    }
                    Ok(n) => {
                        if tx.send(In::Bytes(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        {
            let http = http.clone();
            let token = token.to_string();
            let send = move |msg: &str| send(&http, &token, msg);
            loop_io(&rx, &mut out, send)?;
        }

        drop(screen);
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
  (no args)        chat with your machine — type locally, sends on enter
  attach           same as bare `gray cloud`
  tty              raw attach to the live remote session's screen (legacy)
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
        return Ok("new conversation — your next message starts with a clean slate".to_string());
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
        None | Some("attach") | Some("tty")
            if interactive
                && std::io::IsTerminal::is_terminal(&std::io::stdout())
                && std::io::IsTerminal::is_terminal(&std::io::stdin()) =>
        {
            let token = require_token(interactive).await?;
            let raw_tty = matches!(args.first().map(String::as_str), Some("tty"));
            tokio::task::spawn_blocking(move || {
                if raw_tty {
                    attach::run(&token)
                } else {
                    chat::run(&token)
                }
            })
            .await
            .map_err(|e| anyhow::anyhow!("attach worker: {e}"))??;
            Ok(String::new())
        }
        None | Some("status") | Some("attach") | Some("tty") => {
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

#[cfg(all(test, unix))]
mod chat_tests {
    //! End-to-end for the local-echo chat loop: input bytes in, transcript
    //! bytes out, and exactly one request per submitted line.

    use super::chat::{self, In, Parser};
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    fn evs_of(p: &mut Parser, bytes: &[u8]) -> Vec<chat::Ev> {
        let mut evs = Vec::new();
        for &b in bytes {
            p.feed(b, &mut evs);
        }
        evs
    }

    use chat::Ev;

    #[test]
    fn parser_plain_text_submits_on_enter() {
        let mut p = Parser::default();
        assert_eq!(
            evs_of(&mut p, b"hi\r"),
            vec![Ev::Char('h'), Ev::Char('i'), Ev::Submit]
        );
    }

    #[test]
    fn parser_detach_and_eof_keys() {
        let mut p = Parser::default();
        assert_eq!(evs_of(&mut p, b"\x1d"), vec![Ev::Detach]);
        assert_eq!(evs_of(&mut p, b"\x04"), vec![Ev::Detach]);
        assert_eq!(evs_of(&mut p, b"\x03"), vec![Ev::Detach]);
    }

    #[test]
    fn parser_swallows_csi_and_arrow_sequences() {
        let mut p = Parser::default();
        // left arrow ESC[D then 'a'
        assert_eq!(evs_of(&mut p, b"\x1b[Da"), vec![Ev::Char('a')]);
    }

    #[test]
    fn parser_paste_never_submits() {
        let mut p = Parser::default();
        let evs = evs_of(&mut p, b"\x1b[200~line1\nline2\x1b[201~\r");
        assert!(evs.contains(&Ev::Char(' ')), "{evs:?}");
        assert_eq!(
            evs.iter().filter(|e| **e == Ev::Submit).count(),
            1,
            "{evs:?}"
        );
    }

    /// Drive loop_io on a thread; returns its transcript once input ends.
    fn drive(
        chunks: Vec<In>,
        send: impl FnMut(&str) -> Result<String, String> + Send + 'static,
    ) -> String {
        let (tx, rx) = mpsc::channel::<In>();
        let handle = std::thread::spawn(move || {
            let mut out: Vec<u8> = Vec::new();
            chat::loop_io(&rx, &mut out, send).unwrap();
            String::from_utf8_lossy(&out).to_string()
        });
        for c in chunks {
            tx.send(c).unwrap();
        }
        tx.send(In::End).unwrap();
        drop(tx);
        handle.join().unwrap()
    }

    #[test]
    fn keystrokes_echo_locally_without_any_send() {
        let (calls_tx, calls_rx) = mpsc::channel::<String>();
        let send = move |m: &str| {
            calls_tx.send(m.to_string()).unwrap();
            Ok("pong".to_string())
        };
        let out = drive(vec![In::Bytes(b"hel".to_vec())], send);
        assert!(out.contains("hel"), "{out:?}");
        assert!(calls_rx.try_recv().is_err(), "no Enter — nothing sent");
    }

    #[test]
    fn enter_sends_once_and_reply_renders() {
        let (calls_tx, calls_rx) = mpsc::channel::<String>();
        let send = move |m: &str| {
            calls_tx.send(m.to_string()).unwrap();
            Ok("pong".to_string())
        };
        let out = drive(vec![In::Bytes(b"hi there\r".to_vec())], send);
        assert_eq!(calls_rx.recv().unwrap(), "hi there");
        assert!(calls_rx.try_recv().is_err(), "exactly one send per Enter");
        assert!(out.contains("hi there") && out.contains("pong"), "{out:?}");
    }

    #[test]
    fn typing_during_a_reply_stays_local_and_queues_once() {
        let (calls_tx, calls_rx) = mpsc::channel::<String>();
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let send = move |m: &str| {
            calls_tx.send(m.to_string()).unwrap();
            gate_rx.recv().unwrap();
            Ok(format!("re:{m}"))
        };
        let (tx, rx) = mpsc::channel::<In>();
        let handle = std::thread::spawn(move || {
            let mut out: Vec<u8> = Vec::new();
            chat::loop_io(&rx, &mut out, send).unwrap();
            String::from_utf8_lossy(&out).to_string()
        });
        tx.send(In::Bytes(b"first\r".to_vec())).unwrap();
        assert_eq!(calls_rx.recv().unwrap(), "first"); // in flight now
        tx.send(In::Bytes(b"second\r".to_vec())).unwrap(); // queued locally
        std::thread::sleep(Duration::from_millis(150));
        assert!(calls_rx.try_recv().is_err(), "queued, not sent yet");
        gate_tx.send(()).unwrap(); // reply lands
        assert_eq!(calls_rx.recv().unwrap(), "second"); // auto-sent
        gate_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        tx.send(In::End).unwrap();
        let out = handle.join().unwrap();
        assert!(
            out.contains("re:first") && out.contains("re:second"),
            "{out:?}"
        );
    }

    /// A scripted HTTP/1.1 responder: each accepted connection reads one
    /// request, records (path, body), and answers from a response script
    /// that can vary per hit (e.g. first chat 404 then 200).
    struct Stub {
        base: String,
        hits: mpsc::Receiver<(String, String)>,
    }

    fn stub(scripts: Vec<(&'static str, Vec<(u16, &'static str)>)>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (hits_tx, hits_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut used: std::collections::HashMap<String, usize> = Default::default();
            while let Ok((mut s, _)) = listener.accept() {
                let mut req = Vec::new();
                let mut b = [0u8; 8192];
                loop {
                    let n = s.read(&mut b).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&b[..n]);
                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&req[..pos]).to_string();
                        let len: usize = headers
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(|v| v.trim().parse().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if req.len() >= pos + 4 + len {
                            break;
                        }
                    }
                    if req.len() > 65536 {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&req).to_string();
                let path = text
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                let _ = hits_tx.send((path.clone(), body));
                let n = *used.get(&path).unwrap_or(&0);
                used.insert(path.clone(), n + 1);
                let script = scripts.iter().find(|(p, _)| *p == path);
                let (code, resp_body) = script
                    .and_then(|(_, s)| s.get(n).or_else(|| s.last()))
                    .copied()
                    .unwrap_or((404, "{}"));
                let reason = if code == 200 { "OK" } else { "Not Found" };
                let resp = format!(
                    "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    resp_body.len(),
                    resp_body
                );
                use std::io::Write;
                let _ = s.write_all(resp.as_bytes());
            }
        });
        Stub {
            base: format!("http://127.0.0.1:{port}"),
            hits: hits_rx,
        }
    }

    #[test]
    fn chat_post_sends_the_message_and_reads_the_reply() {
        let stub = stub(vec![("/agent/chat", vec![(200, r#"{"response":"pong"}"#)])]);
        let http = ureq::AgentBuilder::new().build();
        let out = chat::send_to(
            &http,
            "tok",
            &format!("{}/agent/chat", stub.base),
            &format!("{}/agent/provision", stub.base),
            "hello",
        )
        .unwrap();
        assert_eq!(out, "pong");
        let (path, body) = stub.hits.recv().unwrap();
        assert_eq!(path, "/agent/chat");
        assert!(body.contains("\"hello\""), "{body}");
        assert!(body.contains("\"fresh\":false"), "{body}");
    }

    #[test]
    fn chat_404_provisions_then_retries_once() {
        let stub = stub(vec![
            (
                "/agent/chat",
                vec![(404, "{}"), (200, r#"{"response":"after-provision"}"#)],
            ),
            ("/agent/provision", vec![(200, "{}")]),
        ]);
        let http = ureq::AgentBuilder::new().build();
        let out = chat::send_to(
            &http,
            "tok",
            &format!("{}/agent/chat", stub.base),
            &format!("{}/agent/provision", stub.base),
            "hi",
        )
        .unwrap();
        assert_eq!(out, "after-provision");
        let paths: Vec<String> = (0..3).map(|_| stub.hits.recv().unwrap().0).collect();
        assert_eq!(
            paths,
            vec!["/agent/chat", "/agent/provision", "/agent/chat"]
        );
    }

    #[test]
    fn chat_new_verb_sets_fresh() {
        let stub = stub(vec![("/agent/chat", vec![(200, r#"{"response":"ok"}"#)])]);
        let http = ureq::AgentBuilder::new().build();
        chat::send_to(
            &http,
            "tok",
            &format!("{}/agent/chat", stub.base),
            &format!("{}/agent/provision", stub.base),
            "/new start over",
        )
        .unwrap();
        let (_, body) = stub.hits.recv().unwrap();
        assert!(body.contains("\"fresh\":true"), "{body}");
        assert!(body.contains("start over"), "{body}");
    }
}
