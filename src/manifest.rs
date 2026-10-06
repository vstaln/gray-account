//! The plugin manifest gray reads at install and sidecar startup.

/// Declares the three account commands: slash commands in the REPL, argv
/// subcommands under `gray account …` in a shell. `publish` is argv-only —
/// a build takes minutes, far past a `command/run` timeout — so it appears
/// in `completion` (the shell completer) but not in `commands`.
pub fn manifest() -> serde_json::Value {
    serde_json::json!({
        "name": "account",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [],
        "commands": ["/login", "/whoami", "/logout"],
        "completion": ["login", "whoami", "logout", "publish"],
    })
}
