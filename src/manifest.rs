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
        "commands": ["/login", "/whoami", "/logout", "/maker"],
        "completion": [
            "login", "whoami", "logout", "new", "check", "build", "release", "publish",
        ],
    })
}
