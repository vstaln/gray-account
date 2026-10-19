//! The plugin manifest gray reads at install and sidecar startup.

/// Declares the account commands plus `/maker` (the plugin-making verbs,
/// merged in from the standalone maker plugin): slash commands in the REPL,
/// argv subcommands under `gray account …` in a shell. `new`, `check`,
/// `build`, `release` and `publish` are argv-only — a build takes minutes,
/// far past a `command/run` timeout — so they appear in `completion` (the
/// shell completer) but not in `commands`.
pub fn manifest() -> serde_json::Value {
    serde_json::json!({
        "name": "account",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        // `/cloud` mode: the hook claims submitted messages while it's on
        // (a no-op `{}` otherwise); replies print via `host/say`.
        "hooks": ["input/submit"],
        "capabilities": ["host.say"],
        "commands": ["/login", "/whoami", "/logout", "/maker", "/cloud", "/marketplace"],
        "completion": [
            "login", "whoami", "logout", "cloud", "marketplace", "new", "check", "build", "release", "publish",
        ],
    })
}
