//! `/feedback <what happened>` — moved out of gray core. Builds a
//! prefilled GitHub issue link, saves the full body under
//! `$GRAY_HOME/feedback/`, and opens the browser when it can.

use std::path::{Path, PathBuf};

use serde_json::Value;

pub const ISSUES_NEW_URL: &str = "https://github.com/vstaln/gray/issues/new";
const MAX_URL_BODY_CHARS: usize = 3000;
const REDACTED: &str = "[REDACTED]";

pub fn build_title(text: &str) -> String {
    let trimmed = text.trim();
    let mut title: String = trimmed.chars().take(60).collect();
    if trimmed.chars().count() > 60 {
        title.push('…');
    }
    if title.is_empty() {
        "Feedback".to_string()
    } else {
        title
    }
}

pub fn terminal_label(term_program: Option<&str>, term: Option<&str>) -> String {
    match (term_program, term) {
        (Some(p), Some(t)) => format!("{p} ({t})"),
        (Some(p), None) => p.to_string(),
        (None, Some(t)) => t.to_string(),
        (None, None) => "unknown".to_string(),
    }
}

pub fn shell_label(shell: Option<&str>) -> String {
    shell
        .and_then(|s| s.rsplit('/').next().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn build_body(
    text: &str,
    version: &str,
    os: &str,
    model: &str,
    session: &str,
    terminal: &str,
    shell: &str,
) -> String {
    let title = build_title(text);
    format!(
        "## Summary\n{title}\n\n## Expected Behavior\n—\n\n## Actual Behavior\n{}\n\n## Steps to Reproduce\n—\n\nGray Version: {version}\nOperating System: {os}\nTerminal: {terminal}\nShell: {shell}\nModel: {model}\nSession: {session}\n\n## Fix prompt\n—\n\n## Additional context\n—\n",
        text.trim()
    )
}

/// Credentials stored in `auth.json` get scrubbed verbatim; the
/// `sk-…`/`Bearer …`/`…_KEY=…` shapes get pattern-scrubbed — same policy
/// core's `redact_secrets` applied before this moved.
fn known_secrets(home: &Path) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(home.join("auth.json")) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_strings(&v, &mut found);
    found.retain(|s| s.chars().count() >= 16 && !s.chars().any(char::is_whitespace));
    found.sort_by_key(|s| std::cmp::Reverse(s.len()));
    found.dedup();
    found
}

fn collect_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|e| collect_strings(e, out)),
        Value::Object(o) => o.values().for_each(|e| collect_strings(e, out)),
        _ => {}
    }
}

/// `token sk-…`, `Bearer …`, `key=…` — the shapes `gray_core::redaction`
/// scrubs, re-expressed without the core dep.
fn redact_patterns(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        let w = word.trim_end_matches(|c: char| c.is_whitespace() || c == ',' || c == ';');
        let tail = &word[w.len()..];
        let scrub = (w.len() > 24 && (w.starts_with("sk-") || w.starts_with("sk_")))
            || (w.len() > 40 && w.starts_with("Bearer "))
            || (w.len() > 40 && (w.starts_with("eyJ") || w.starts_with("AIza")))
            || w.split_once('=').is_some_and(|(k, v)| {
                (k.to_ascii_uppercase().contains("KEY")
                    || k.to_ascii_uppercase().contains("TOKEN")
                    || k.to_ascii_uppercase().contains("SECRET"))
                    && v.len() >= 8
            });
        if scrub {
            out.push_str(REDACTED);
        } else {
            out.push_str(w);
        }
        out.push_str(tail);
    }
    out
}

pub fn redact_secrets(text: &str, home: &Path) -> String {
    let mut out = text.to_string();
    for secret in known_secrets(home) {
        out = out.replace(&secret, REDACTED);
    }
    redact_patterns(&out)
}

pub fn issue_url(title: &str, body: &str, home: &Path) -> String {
    let body = redact_secrets(body, home);
    let short = if body.chars().count() > MAX_URL_BODY_CHARS {
        let cut: String = body.chars().take(MAX_URL_BODY_CHARS).collect();
        format!("{cut}\n… [truncated for URL — full text in local file]")
    } else {
        body
    };
    let enc = |s: &str| url_encode(s);
    format!(
        "{ISSUES_NEW_URL}?template=bug_report.yml&title={}&body={}",
        enc(title),
        enc(&short)
    )
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn timestamp() -> String {
    // YYYYMMDDTHHMMSSZ — close enough to the old chrono stamp without the dep.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = epoch_to_utc(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

fn epoch_to_utc(t: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = t / 86400;
    let rem = t % 86400;
    let (h, mi, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y } as u64;
    (y, m, d, h, mi, s)
}

pub fn save_feedback(dir: &Path, title: &str, body: &str, stamp: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let slug: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    let slug = slug.trim_matches('-');
    let name = if slug.is_empty() {
        format!("feedback-{stamp}.md")
    } else {
        format!("{stamp}-{slug}.md")
    };
    let path = dir.join(name);
    std::fs::write(&path, format!("# {title}\n\n{body}"))?;
    Ok(path)
}

pub fn open_in_browser(url: &str) {
    let (cmd, arg) = if cfg!(target_os = "macos") {
        ("open", url)
    } else if cfg!(target_os = "windows") {
        ("cmd", "/c start ")
    } else {
        ("xdg-open", url)
    };
    if cfg!(target_os = "windows") {
        let _ = std::process::Command::new(cmd)
            .arg("/c")
            .arg("start")
            .arg(url)
            .spawn();
    } else {
        let _ = std::process::Command::new(cmd).arg(arg).spawn();
    }
}

/// `gray --version` → `gray X.Y.Z` — the host's version, not ours.
fn gray_version() -> String {
    std::process::Command::new("gray")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .nth(1)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn gray_home() -> PathBuf {
    crate::account::gray_home().unwrap_or_else(|_| PathBuf::from(".gray"))
}

/// The command/run entry: argv text is the report body, `session` carries
/// id/cwd/model from the host.
pub fn run(argv: &[String], session: &Value) -> Value {
    let raw = argv.join(" ").trim().to_string();
    if raw.is_empty() {
        return serde_json::json!({
            "text": "usage: /feedback <what happened> — saves locally and opens a prefilled issue"
        });
    }
    let title = build_title(&raw);
    let version = gray_version();
    let os = format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH);
    let model = session["model"]
        .as_str()
        .or_else(|| session["model"]["model"].as_str())
        .unwrap_or("no model")
        .to_string();
    let sid = session["id"].as_str().unwrap_or("none");
    let terminal = terminal_label(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
    );
    let shell = shell_label(std::env::var("SHELL").ok().as_deref());
    let body = build_body(&raw, &version, &os, &model, sid, &terminal, &shell);
    let home = gray_home();
    let url = issue_url(&title, &body, &home);
    let dir = home.join("feedback");
    match save_feedback(&dir, &title, &body, &timestamp()) {
        Ok(path) => {
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                open_in_browser(&url);
            }
            serde_json::json!({
                "text": format!("saved {}\nfile an issue: {url}", path.display())
            })
        }
        Err(e) => serde_json::json!({
            "text": format!("could not save feedback ({e}) — file manually: {url}")
        }),
    }
}
