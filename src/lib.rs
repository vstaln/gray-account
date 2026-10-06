//! gray-account: a protocol-1.1 sidecar and CLI plugin for gray.
//!
//! Carries the gray.alignment.id account commands — login, whoami, logout —
//! that used to live in gray core, plus the plugin-maker verbs (`new`,
//! `check`, `build`, `release`) merged in from the standalone maker plugin.
//! The token file format and location are unchanged, so a login made by an
//! older gray keeps working.

pub mod account;
pub mod maker;
pub mod manifest;
pub mod publish;

use serde_json::Value;
use serde_json::json;

/// What the NDJSON loop should do after [`handle`] runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// Write this value as the response `result`, keep serving.
    Result(Value),
    /// Write this value as the response `result`, then exit 0.
    ResultThenExit(Value),
    /// Write a JSON-RPC-style error reply, keep serving.
    MethodNotFound,
    /// Write nothing, exit 0.
    Exit,
}

/// Pure dispatch for one sidecar request. `has_id` distinguishes requests
/// (which get a reply) from notifications (which do not); a notification
/// `plugin/shutdown` exits without a reply.
pub async fn handle(method: &str, params: &Value, has_id: bool) -> Reply {
    match method {
        "plugin/manifest" => Reply::Result(manifest::manifest()),
        "command/run" => Reply::Result(command_run(params).await),
        "plugin/shutdown" => {
            if has_id {
                Reply::ResultThenExit(json!({}))
            } else {
                Reply::Exit
            }
        }
        _ => Reply::MethodNotFound,
    }
}

async fn command_run(params: &Value) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // `/maker` replies with text or an agent prompt, not always `{"text"}`.
    if name == "/maker" {
        return maker::slash(params);
    }
    let text = match name {
        "/login" => {
            // The code is the first non-empty argv entry; whitespace-only
            // entries and escape-wrapped pastes are normalized inside.
            let code = params
                .get("argv")
                .and_then(Value::as_array)
                .and_then(|argv| {
                    argv.iter()
                        .filter_map(Value::as_str)
                        .find(|arg| !arg.trim().is_empty())
                });
            render(account::login(code, false).await, "login")
        }
        "/whoami" => render(account::whoami().await, "whoami"),
        "/logout" => render(account::logout().await, "logout"),
        _ => String::new(),
    };
    json!({ "text": text })
}

fn render(result: anyhow::Result<String>, verb: &str) -> String {
    match result {
        Ok(text) => text,
        Err(e) => format!("{verb} failed: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_declares_the_account_and_maker_commands() {
        let m = manifest::manifest();
        assert_eq!(m["name"], "account");
        assert_eq!(m["protocol"], "1.1");
        assert_eq!(m["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            m["commands"],
            json!(["/login", "/whoami", "/logout", "/maker"])
        );
        assert_eq!(
            m["completion"],
            json!([
                "login", "whoami", "logout", "new", "check", "build", "release", "publish"
            ])
        );
        assert_eq!(m["tools"], json!([]));
    }

    #[tokio::test]
    async fn unknown_method_is_a_method_not_found_error() {
        assert_eq!(
            handle("nope", &json!({}), true).await,
            Reply::MethodNotFound
        );
    }

    #[tokio::test]
    async fn login_with_empty_argv_returns_the_walkthrough() {
        let reply = handle(
            "command/run",
            &json!({ "name": "/login", "argv": [] }),
            true,
        )
        .await;
        let Reply::Result(v) = reply else {
            panic!("expected a result");
        };
        let text = v["text"].as_str().unwrap_or_default();
        assert!(text.contains("https://gray.alignment.id/account"), "{text}");
        assert!(text.ends_with("Then run /login <code>."), "{text}");
    }

    #[tokio::test]
    async fn unknown_command_returns_empty_text() {
        let reply = handle("command/run", &json!({ "name": "/nope", "argv": [] }), true).await;
        let Reply::Result(v) = reply else {
            panic!("expected a result");
        };
        assert_eq!(v["text"], "");
    }

    #[tokio::test]
    async fn shutdown_with_an_id_replies_then_exits() {
        assert_eq!(
            handle("plugin/shutdown", &json!({}), true).await,
            Reply::ResultThenExit(json!({}))
        );
        assert_eq!(
            handle("plugin/shutdown", &json!({}), false).await,
            Reply::Exit
        );
    }
}
