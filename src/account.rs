//! gray.alignment.id account: login, whoami, logout.
//!
//! The site mints a one-time enrollment code from a Supabase session
//! (GitHub / Google / Discord). `gray account login <code>` exchanges that
//! code here for a long-lived `gray_…` registry token, stored at
//! `~/.gray/registry-token.json` (mode 0600, atomic write).
//!
//! Nothing in gray needs an account: a login only exists so an authenticated
//! registry call can name a caller. Every command here works with no provider
//! configured, so a fresh machine can enroll before it can run a turn.

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Production registry API. Override with [`REGISTRY_URL_ENV`] to point at a
/// local `pnpm backend:dev` instance.
pub const DEFAULT_REGISTRY_URL: &str = "https://gray.alignment.id/api";
/// Environment override for the registry base URL.
pub const REGISTRY_URL_ENV: &str = "GRAY_REGISTRY_URL";
/// Where a user signs in and mints an enrollment code.
pub const SITE_ACCOUNT_URL: &str = "https://gray.alignment.id/account";
/// How long the site says a code lives, for the prompt copy.
pub const CODE_TTL_MINUTES: u32 = 5;

/// Registry base URL: the env override when set, else production. Any trailing
/// slash is stripped so endpoint paths never double up.
pub fn base_url() -> String {
    normalize_base_url(&std::env::var(REGISTRY_URL_ENV).unwrap_or_default())
}

/// Absolute endpoint URL built from [`base_url`]. Fails with a message that
/// names the env var, so a typo in `GRAY_REGISTRY_URL` reads as a config
/// problem instead of "relative URL without a base".
pub fn endpoint(path: &str) -> anyhow::Result<String> {
    endpoint_with(&base_url(), path)
}

/// Pure seam for [`endpoint`]: joins `path` onto an already-normalized base.
pub fn endpoint_with(base: &str, path: &str) -> anyhow::Result<String> {
    // `Url::join` replaces the last segment unless the base ends in '/', and
    // `normalize_base_url` strips that slash — put it back for the join only.
    let base = reqwest::Url::parse(&format!("{base}/"))
        .map_err(|e| anyhow::anyhow!("{REGISTRY_URL_ENV} is not a valid absolute URL: {e}"))?;
    // Every call here carries a bearer token, so cleartext would put the
    // credential on the wire. Loopback is the documented local-dev exception
    // (`GRAY_REGISTRY_URL=http://127.0.0.1:4000/api`, `pnpm backend:dev`).
    if base.scheme() != "https" && !is_loopback_host(base.host_str().unwrap_or_default()) {
        anyhow::bail!(
            "{REGISTRY_URL_ENV} must be https (http is allowed only for loopback): {base}"
        );
    }
    Ok(base.join(path.trim_start_matches('/'))?.to_string())
}

/// Loopback by name or by address, IPv6 brackets included.
fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Pure seam for [`base_url`]: empty/blank input falls back to production.
pub fn normalize_base_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        DEFAULT_REGISTRY_URL.to_string()
    } else {
        raw.trim_end_matches('/').to_string()
    }
}

/// The stored credential. Debug is hand-redacted: it carries a bearer token,
/// so the derived impl would leak it into any log that formats this.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToken {
    pub token: String,
}

impl std::fmt::Debug for StoredToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredToken").finish_non_exhaustive()
    }
}

/// The gray state directory: `GRAY_HOME` when set and non-blank, else the
/// user profile plus `.gray`.
fn gray_home() -> anyhow::Result<PathBuf> {
    gray_home_opt().ok_or_else(|| {
        anyhow::anyhow!("cannot resolve home: set GRAY_HOME or the platform user profile")
    })
}

fn gray_home_opt() -> Option<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| user_home().map(|p| p.join(".gray")))
}

fn user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // std uses USERPROFILE, then the Windows profile API. In particular it
        // does not confuse Git Bash's HOME with the native Windows profile.
        std::env::home_dir()
    }
    #[cfg(not(windows))]
    {
        // Preserve the Unix contract: no implicit passwd fallback when a host
        // deliberately removed HOME.
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Atomic write of a private JSON file (mode 0600): a sibling temp file that
/// is synced and renamed over the target so the file can never be observed
/// half-written or world-readable.
fn save_private_json(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    use std::io::Write as _;
    let body = serde_json::to_vec_pretty(value)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let tmp = parent.join(format!(".gray-{unique}.tmp"));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Path of the registry token file inside `$GRAY_HOME`.
pub fn token_path() -> anyhow::Result<PathBuf> {
    Ok(gray_home()?.join("registry-token.json"))
}

pub fn load_token() -> Option<String> {
    token_path().ok().as_deref().and_then(load_token_at)
}

/// Explicit-path seam for [`load_token`] (tests). A missing, unreadable, or
/// shape-wrong file reads as "not logged in" — never an error.
pub fn load_token_at(path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<StoredToken>(&body)
        .ok()
        .map(|t| t.token)
        .filter(|t| !t.trim().is_empty())
}

pub fn save_token(token: &str) -> anyhow::Result<()> {
    save_token_at(&token_path()?, token)
}

/// Writes the token with the shared 0600 atomic writer so the file can never
/// be observed half-written or world-readable.
pub fn save_token_at(path: &Path, token: &str) -> anyhow::Result<()> {
    let stored = StoredToken {
        token: token.trim().to_string(),
    };
    save_private_json(
        path,
        &serde_json::to_value(&stored).map_err(|e| anyhow::anyhow!("{e}"))?,
    )
}

/// Drops the local token. Returns true when a file was actually removed.
pub fn clear_token_at(path: &Path) -> anyhow::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// An enrollment code as pasted by the user. `None` when there is nothing to
/// exchange, so callers can re-prompt instead of firing an empty request.
///
/// The paste is sanitized first: see [`strip_terminal_escapes`] for why the
/// raw line can carry escape bytes that no hand-typed code ever contains.
pub fn normalize_code(raw: &str) -> Option<String> {
    let code = strip_terminal_escapes(raw);
    let code = code.trim();
    (!code.is_empty()).then(|| code.to_string())
}

/// Strips what only a terminal can produce from a pasted code.
///
/// Bracketed paste (mode 2004) is terminal-global, so it outlives the raw
/// mode a REPL drops for a cooked stdin prompt: the wrapper the terminal
/// adds to every paste — `ESC [ 200~` ... `ESC [ 201~` — then arrives as
/// ordinary text. A shell that leaves the mode on for its children (bash
/// does) hands the same wrapper to `gray account login <pasted code>`.
///
/// Nothing an escape sequence or control byte can be is part of a code, so
/// remove them rather than forward bytes the registry will only reject.
fn strip_terminal_escapes(raw: &str) -> String {
    strip_escape_sequences(raw)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// Drops escape sequences from a pasted string: CSI runs (`ESC [ <params>
/// <final>`, which includes both bracketed-paste markers `ESC [ 200~` and
/// `ESC [ 201~`), OSC runs, and a lone `ESC`.
///
/// A paste can reach us still wrapped in the bracketed-paste markers when a
/// terminal wraps it before we see it, and it can drag along CSI/OSC debris
/// copied out of a rendered page. Neither can be meant as text, so delete
/// them — only escape *sequences* go: newlines and tabs, which a pasted code
/// block legitimately carries, stay.
fn strip_escape_sequences(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // A lone ESC and the byte after it (ESC =): drop the introducer,
            // keep the byte - deleting a character the user pasted would lose
            // text the escape never explained.
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

// ---- Wire types (mirror services/registry/routes/auth.mjs) ----------------

/// One plugin the caller owns, newest version resolved server-side.
#[derive(Debug, Clone, Deserialize)]
pub struct OwnedPlugin {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// The identity behind a token. Every field is optional because the two
/// endpoints that produce it disagree: `/auth/token` nests a `publicUser`
/// subset, `/auth/me` adds `id`, `created_at`, and owned plugins.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub plugins: Vec<OwnedPlugin>,
}

impl Account {
    /// Best human label: display name, then @handle, then provider, then
    /// "your account". Never invents a name the registry did not send.
    pub fn label(&self) -> String {
        self.display_name
            .clone()
            .or_else(|| self.username.clone().map(|u| format!("@{u}")))
            .or_else(|| self.provider.clone())
            .unwrap_or_else(|| "your account".to_string())
    }
}

#[derive(Deserialize)]
struct TokenExchangeResponse {
    token: String,
    #[serde(default)]
    user: Option<Account>,
}

// ---- HTTP -----------------------------------------------------------------

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}

/// The registry reports failures as `{"error": "..."}`; surface that message
/// verbatim instead of a bare status code.
async fn error_message(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&body)
        && let Some(msg) = parsed.get("error").and_then(|v| v.as_str())
        && !msg.trim().is_empty()
    {
        return msg.to_string();
    }
    format!("registry returned {status}")
}

/// Exchanges a one-time enrollment code for a long-lived `gray_…` token.
pub async fn login_with_code(code: &str) -> anyhow::Result<(String, Account)> {
    let url = endpoint("auth/token")?;
    let resp = http_client()?
        .post(&url)
        .json(&serde_json::json!({ "code": code }))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(error_message(resp).await);
    }
    let parsed: TokenExchangeResponse = resp.json().await?;
    Ok((parsed.token, parsed.user.unwrap_or_default()))
}

/// Who the stored token belongs to, plus the plugins it owns.
pub async fn fetch_account() -> anyhow::Result<Account> {
    let token = load_token().ok_or_else(|| {
        anyhow::anyhow!("not logged in — run `gray account login` (or /login in the REPL)")
    })?;
    let url = endpoint("auth/me")?;
    let resp = http_client()?
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(error_message(resp).await);
    }
    Ok(resp.json().await?)
}

/// Best-effort server-side revoke of one token. Failures are swallowed: the
/// caller's next step (saving a new token, clearing the local file) must not
/// depend on the registry being reachable.
async fn revoke_token(token: &str) -> bool {
    let client = match http_client() {
        Ok(c) => c,
        Err(_) => return false,
    };
    let url = match endpoint("auth/tokens") {
        Ok(u) => u,
        Err(_) => return false,
    };
    let sent = client
        .delete(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "token": token }))
        .send()
        .await;
    matches!(sent, Ok(r) if r.status().is_success())
}

/// What a logout accomplished. "No token" and "revocation failed" are
/// different facts: the second leaves a credential the registry still honors,
/// so the caller must be able to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoutOutcome {
    Revoked,
    NoToken,
    RevocationFailed,
}

/// Revokes the stored token server-side, then drops it locally. A token the
/// registry already forgot is still cleared locally: the goal is "this machine
/// holds no credential", not "the server agreed".
pub async fn logout_session() -> anyhow::Result<LogoutOutcome> {
    let Some(token) = load_token() else {
        return Ok(LogoutOutcome::NoToken);
    };
    let outcome = match revoke_token(&token).await {
        true => LogoutOutcome::Revoked,
        false => LogoutOutcome::RevocationFailed,
    };
    clear_token_at(&token_path()?)?;
    Ok(outcome)
}

/// The line `gray account logout` prints for each outcome.
pub fn logout_message(outcome: LogoutOutcome) -> &'static str {
    match outcome {
        LogoutOutcome::Revoked => "logged out — registry token revoked",
        LogoutOutcome::NoToken => "logged out — no registry token was stored",
        LogoutOutcome::RevocationFailed => {
            "logged out locally — the registry could not revoke the token, so it may still be valid"
        }
    }
}

// ---- Prompts --------------------------------------------------------------

/// Reads one line from stdin, trimmed. `None` on EOF (piped empty stdin) so a
/// scripted `gray account login` fails loudly instead of hanging.
pub fn prompt_for_code() -> Option<String> {
    use std::io::Write as _;
    print!("code: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => normalize_code(&line),
    }
}

/// The instructions printed before the prompt. Kept as data so the CLI and
/// the sidecar return the same words.
pub fn login_instructions() -> String {
    format!(
        "1. Open {SITE_ACCOUNT_URL} and sign in (GitHub, Google, or Discord).\n\
         2. Choose \"Generate CLI login code\" — it expires in {CODE_TTL_MINUTES} minutes.\n\
         3. Paste it here."
    )
}

// ---- Command entry points -------------------------------------------------

/// `gray account login [CODE]` / `/login [CODE]`.
///
/// Interactive (shell): prints the walkthrough when no code is given and
/// prompts on stdin. Non-interactive (sidecar): returns the walkthrough
/// ending with "Then run /login <code>." without reading stdin. Either way
/// the returned string is everything the caller should print.
pub async fn login(code: Option<&str>, interactive: bool) -> anyhow::Result<String> {
    // The arg goes through the same sanitizer as the prompt: a code pasted
    // into a shell that leaves bracketed paste on for its children (bash
    // does) arrives still wrapped in `ESC[200~ ... ESC[201~`.
    let mut out = String::new();
    let code = match code.and_then(normalize_code) {
        Some(c) => c,
        None => {
            let mut prefix = format!(
                "Log in to gray.alignment.id from this machine.\n\n{}\n",
                login_instructions()
            );
            if !interactive {
                prefix.push_str("\nThen run /login <code>.");
                return Ok(prefix);
            }
            out.push_str(&prefix);
            match prompt_for_code() {
                Some(c) => c,
                None => anyhow::bail!("no enrollment code given — run `gray account login <code>`"),
            }
        }
    };
    let (token, account) = login_with_code(&code).await?;
    // Persist before revoking. The exchanged token is live server-side the
    // moment it is minted, so the old order (revoke, then save) turned a failed
    // local write into a logged-out machine holding nothing. Now a failed save
    // revokes the new token and keeps the previous session working.
    // Capture the previous token before the save: afterwards load_token()
    // answers with the new one and the old token would never be revoked.
    let previous = load_token().filter(|p| *p != token);
    if let Err(e) = save_token(&token) {
        let _ = revoke_token(&token).await;
        return Err(e);
    }
    // A second login would otherwise orphan the previous token: it stays valid
    // server-side while this machine forgets it ever existed.
    if let Some(previous) = previous
        && !revoke_token(&previous).await
    {
        eprintln!(
            "warning: the previous registry token could not be revoked and may still be valid"
        );
    }
    out.push_str(&format!("logged in as {}\n", account.label()));
    if account.plugins.is_empty() {
        out.push_str("no plugins published yet\n");
    } else {
        out.push_str("plugins:\n");
        for p in &account.plugins {
            match &p.version {
                Some(v) => out.push_str(&format!("  {} {v}\n", p.name)),
                None => out.push_str(&format!("  {}\n", p.name)),
            }
        }
    }
    out.push_str(
        "\nWhat this unlocks today: nothing you didn't already have. The token only names you on registry calls.",
    );
    Ok(out.trim_end().to_string())
}

/// `gray account whoami` / `/whoami`: reports the stored token's identity.
pub async fn whoami() -> anyhow::Result<String> {
    let account = fetch_account().await?;
    let mut out = format!("{}\n", account.label());
    if let Some(provider) = &account.provider {
        out.push_str(&format!("signed in with {provider}\n"));
    }
    if !account.plugins.is_empty() {
        out.push_str("plugins:\n");
        for p in &account.plugins {
            match &p.version {
                Some(v) => out.push_str(&format!("  {} {v}\n", p.name)),
                None => out.push_str(&format!("  {}\n", p.name)),
            }
        }
    }
    Ok(out.trim_end().to_string())
}

/// `gray account logout` / `/logout`: revokes and forgets the token.
pub async fn logout() -> anyhow::Result<String> {
    Ok(logout_message(logout_session().await?).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gray-account-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("tmp");
        dir
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, body).expect("write fixture");
    }

    #[test]
    fn loopback_hosts_are_recognized_with_and_without_brackets() {
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.0.0.2"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("example.com"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host(""));
    }

    #[test]
    fn endpoint_with_refuses_cleartext_except_on_loopback() {
        // The token rides on every call, so a cleartext remote is refused.
        assert!(endpoint_with("http://example.test/api", "auth/token").is_err());
        assert!(endpoint_with("http://169.254.1.1/api", "auth/token").is_err());
        // Loopback stays allowed: that is the documented local registry.
        assert_eq!(
            endpoint_with("http://127.0.0.1:4000/api", "auth/token").unwrap(),
            "http://127.0.0.1:4000/api/auth/token"
        );
        assert_eq!(
            endpoint_with("http://localhost:4000/api", "auth/token").unwrap(),
            "http://localhost:4000/api/auth/token"
        );
        // Production and any https host.
        assert!(endpoint_with(DEFAULT_REGISTRY_URL, "auth/token").is_ok());
        assert!(endpoint_with("https://example.test/api", "auth/token").is_ok());
    }

    #[test]
    fn logout_message_names_each_outcome_distinctly() {
        assert!(logout_message(LogoutOutcome::Revoked).contains("revoked"));
        assert!(
            logout_message(LogoutOutcome::NoToken).contains("no registry token"),
            "no token must not read as a revocation"
        );
        let failed = logout_message(LogoutOutcome::RevocationFailed);
        assert!(failed.contains("could not revoke"), "{failed}");
        assert!(failed.contains("may still be valid"), "{failed}");
        assert_ne!(
            logout_message(LogoutOutcome::NoToken),
            logout_message(LogoutOutcome::RevocationFailed),
            "a failed revocation must never print as 'nothing was stored'"
        );
    }

    #[test]
    fn base_url_falls_back_to_production_when_env_is_blank() {
        assert_eq!(normalize_base_url(""), DEFAULT_REGISTRY_URL);
        assert_eq!(normalize_base_url("   "), DEFAULT_REGISTRY_URL);
    }

    #[test]
    fn base_url_strips_one_trailing_slash_only() {
        assert_eq!(
            normalize_base_url("http://127.0.0.1:4000/api/"),
            "http://127.0.0.1:4000/api"
        );
        // Every trailing slash goes: an endpoint join must never see "//".
        assert_eq!(
            normalize_base_url("https://example.test/api//"),
            "https://example.test/api"
        );
    }

    /// Mutates process env: restore on drop.
    struct EnvGuard {
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(value: Option<&str>) -> Self {
            let prev = std::env::var(REGISTRY_URL_ENV).ok();
            match value {
                Some(v) => unsafe { std::env::set_var(REGISTRY_URL_ENV, v) },
                None => unsafe { std::env::remove_var(REGISTRY_URL_ENV) },
            }
            Self { prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe { std::env::set_var(REGISTRY_URL_ENV, v) },
                None => unsafe { std::env::remove_var(REGISTRY_URL_ENV) },
            }
        }
    }

    #[test]
    fn endpoint_joins_onto_the_api_prefix() {
        assert_eq!(
            endpoint_with("http://127.0.0.1:4000/api", "auth/token").expect("endpoint"),
            "http://127.0.0.1:4000/api/auth/token"
        );
        // A leading slash must not collapse the /api prefix.
        assert_eq!(
            endpoint_with("http://127.0.0.1:4000/api", "/auth/token").expect("endpoint"),
            "http://127.0.0.1:4000/api/auth/token"
        );
    }

    #[test]
    fn endpoint_names_the_env_var_when_the_url_is_junk() {
        let err = endpoint_with("not a url", "auth/token").expect_err("junk url must fail");
        assert!(err.to_string().contains(REGISTRY_URL_ENV), "{err}");
    }

    /// Process env is global: these two readers must not interleave.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn base_url_reads_the_env_override() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let _guard = EnvGuard::set(Some("http://127.0.0.1:4000/api/"));
        assert_eq!(base_url(), "http://127.0.0.1:4000/api");
    }

    #[test]
    fn base_url_ignores_an_empty_env_override() {
        let _lock = ENV_LOCK.lock().expect("env lock");
        let _guard = EnvGuard::set(Some("  "));
        assert_eq!(base_url(), DEFAULT_REGISTRY_URL);
    }

    #[test]
    fn empty_code_normalizes_to_none() {
        assert_eq!(normalize_code(""), None);
        assert_eq!(normalize_code("   \n"), None);
        assert_eq!(normalize_code("  abc123 \n"), Some("abc123".to_string()));
    }

    #[test]
    fn a_pasted_code_sheds_the_bracketed_paste_wrapper() {
        // Mode 2004 is terminal-global: a paste can arrive still wrapped in
        // the ESC[200~ ... ESC[201~ markers the terminal adds, and the
        // registry then rejected a code it had never issued.
        assert_eq!(
            normalize_code("\u{1b}[200~dv3Uq5VPTFRYqH0G2qx_jzlAjTlnztfX\u{1b}[201~\n"),
            Some("dv3Uq5VPTFRYqH0G2qx_jzlAjTlnztfX".to_string())
        );
        // A shell that leaves bracketed paste on for its children (bash does)
        // hands the same wrapper to `gray account login <pasted code>`.
        assert_eq!(
            normalize_code("\u{1b}[200~  abc123  \u{1b}[201~"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn a_pasted_code_sheds_any_other_terminal_escape_or_control() {
        // OSC title-set and CSI cursor sequences ride along with a copy from
        // a rendered page; none of them can be part of a code, so drop them
        // instead of forwarding bytes the registry will only reject.
        assert_eq!(
            normalize_code("\u{1b}]0;gray\u{7}abc\u{1b}[2K123\u{1b}[0m\r\n"),
            Some("abc123".to_string())
        );
        // A truncated paste ends on a bare ESC: the rest of the line survives.
        assert_eq!(
            normalize_code("abc\u{1b}[201"),
            Some("abc".to_string()),
            "an unterminated CSI run is dropped, not kept"
        );
        assert_eq!(normalize_code("\u{1b}[200~\u{1b}[201~"), None);
    }

    #[test]
    fn token_round_trips_through_the_private_writer() {
        let dir = temp_dir();
        let path = dir.join("registry-token.json");
        save_token_at(&path, "gray_secret").expect("save");
        assert_eq!(load_token_at(&path), Some("gray_secret".to_string()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "token file must be 0600");
        }
    }

    #[test]
    fn missing_or_junk_token_file_reads_as_logged_out() {
        let dir = temp_dir();
        let missing = dir.join("nope.json");
        assert_eq!(load_token_at(&missing), None);
        let junk = dir.join("junk.json");
        write(&junk, "not json at all");
        assert_eq!(load_token_at(&junk), None);
        let empty = dir.join("empty.json");
        write(&empty, r#"{"token": "  "}"#);
        assert_eq!(load_token_at(&empty), None);
    }

    #[test]
    fn clear_token_reports_whether_a_file_was_there() {
        let dir = temp_dir();
        let path = dir.join("registry-token.json");
        assert!(!clear_token_at(&path).expect("clear missing"));
        save_token_at(&path, "gray_secret").expect("save");
        assert!(clear_token_at(&path).expect("clear present"));
        assert!(!clear_token_at(&path).expect("clear again"));
    }

    #[test]
    fn account_label_prefers_the_display_name() {
        let account = Account {
            display_name: Some("Vstalin Grady".to_string()),
            username: Some("vstaln".to_string()),
            ..Default::default()
        };
        assert_eq!(account.label(), "Vstalin Grady");
        let handle_only = Account {
            username: Some("vstaln".to_string()),
            ..Default::default()
        };
        assert_eq!(handle_only.label(), "@vstaln");
        assert_eq!(Account::default().label(), "your account");
    }

    #[test]
    fn non_interactive_login_without_a_code_returns_the_walkthrough() {
        let text = futures_login_no_code();
        assert!(text.contains(SITE_ACCOUNT_URL), "{text}");
        assert!(text.ends_with("Then run /login <code>."), "{text}");
    }

    fn futures_login_no_code() -> String {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt")
            .block_on(login(None, false))
            .expect("walkthrough")
    }
}
