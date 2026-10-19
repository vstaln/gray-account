use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;

/// MODE is process-global; tests that touch it run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

type Queue<T> = Arc<Mutex<VecDeque<T>>>;

#[derive(Clone, Default)]
struct Script {
    statuses: Queue<Result<Option<String>, Fail>>,
    chats: Queue<Result<String, Fail>>,
    sent: Arc<Mutex<Vec<(String, bool)>>>,
    nudges: Arc<Mutex<u32>>,
}

impl Script {
    fn statuses(self, s: &[&str]) -> Self {
        *self.statuses.lock().unwrap() = s.iter().map(|s| Ok(Some(s.to_string()))).collect();
        self
    }
    fn chats(self, c: Vec<Result<String, Fail>>) -> Self {
        *self.chats.lock().unwrap() = c.into();
        self
    }
    fn sent(&self) -> Vec<(String, bool)> {
        self.sent.lock().unwrap().clone()
    }
}

impl Machine for Script {
    async fn provision(&self) -> Result<Option<String>, Fail> {
        Ok(Some("sleeping".into()))
    }
    async fn status(&self) -> Result<Option<String>, Fail> {
        let mut q = self.statuses.lock().unwrap();
        // The last scripted status sticks.
        if q.len() > 1 { q.pop_front().unwrap() } else { q.front().cloned().unwrap_or(Ok(None)) }
    }
    async fn nudge(&self) {
        *self.nudges.lock().unwrap() += 1;
    }
    async fn chat(&self, message: &str, fresh: bool) -> Result<String, Fail> {
        self.sent.lock().unwrap().push((message.to_string(), fresh));
        self.chats
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(format!("echo: {message}")))
    }
}

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<String>>>);

impl Say for Lines {
    fn say(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_string());
    }
}

impl Lines {
    fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn joined(&self) -> String {
        self.all().join("\n")
    }
}

fn submit(sid: &str, text: &str, say: &Lines) -> Value {
    input_submit(&json!({ "text": text, "session": { "id": sid, "cwd": "/" } }), say)
}

/// Let the worker run until it is idle (paused clock auto-advances).
async fn settle() {
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn off_passes_every_message_through() {
    let _g = SERIAL.lock().await;
    *mode() = None;
    let say = Lines::default();
    assert_eq!(submit("s1", "hello", &say), json!({}));
    assert!(say.all().is_empty());
}

#[tokio::test(start_paused = true)]
async fn online_machine_takes_messages_and_replies_land_in_the_transcript() {
    let _g = SERIAL.lock().await;
    *mode() = None;
    let m = Script::default().statuses(&["active"]);
    let say = Lines::default();
    let reply = turn_on("s1", m.clone(), say.clone(), None);
    assert!(reply.contains("cloud mode"), "{reply}");
    settle().await;
    assert_eq!(say.all(), vec!["◆ your machine is online"]);

    assert_eq!(submit("s1", "  what's up  ", &say), json!({ "handled": true }));
    settle().await;
    assert_eq!(m.sent(), vec![("what's up".to_string(), false)]);
    assert!(say.joined().ends_with("◆ echo: what's up"), "{}", say.joined());
    assert_eq!(*m.nudges.lock().unwrap(), 0, "no wake needed when online");

    assert!(turn_off().contains("back to local"));
    assert_eq!(submit("s1", "local again", &say), json!({}));
}

#[tokio::test(start_paused = true)]
async fn asleep_machine_narrates_the_wake_and_flushes_the_queue() {
    let _g = SERIAL.lock().await;
    *mode() = None;
    let m = Script::default().statuses(&["sleeping", "sleeping", "sleeping", "active"]);
    let say = Lines::default();
    turn_on("s1", m.clone(), say.clone(), None);
    // Typed before the machine is up: claimed, queued, acknowledged.
    tokio::task::yield_now().await;
    assert_eq!(submit("s1", "first", &say), json!({ "handled": true }));
    settle().await;
    let all = say.all();
    assert_eq!(all[0], "◆ your machine · waking from sleep — usually ~25s");
    assert!(all.contains(&"· queued — sends once your machine is up".to_string()), "{all:?}");
    assert!(all.iter().any(|l| l.starts_with("◆ your machine is online · woke in ")), "{all:?}");
    assert!(!all.iter().any(|l| l.contains("first boot")), "{all:?}");
    assert_eq!(*m.nudges.lock().unwrap(), 1);
    assert_eq!(m.sent(), vec![("first".to_string(), false)]);
    assert_eq!(all.last().unwrap(), "◆ echo: first");
    turn_off();
}

#[tokio::test(start_paused = true)]
async fn still_starting_503s_retry_then_deliver() {
    let _g = SERIAL.lock().await;
    *mode() = None;
    let m = Script::default().statuses(&["active"]).chats(vec![
        Err(Fail::Retry("Machine is still starting".into())),
        Err(Fail::Retry("Machine is still starting".into())),
        Ok("finally".into()),
    ]);
    let say = Lines::default();
    turn_on("s1", m.clone(), say.clone(), Some(Job::Message("hi".into())));
    settle().await;
    assert_eq!(m.sent().len(), 3);
    assert_eq!(say.all().last().unwrap(), "◆ finally");
    turn_off();
}

#[tokio::test(start_paused = true)]
async fn fatal_chat_error_leaves_cloud_mode() {
    let _g = SERIAL.lock().await;
    *mode() = None;
    let m = Script::default()
        .statuses(&["active"])
        .chats(vec![Err(Fail::Fatal("Seat suspended.".into()))]);
    let say = Lines::default();
    turn_on("s1", m, say.clone(), Some(Job::Message("hi".into())));
    settle().await;
    assert_eq!(say.all().last().unwrap(), "◇ Seat suspended. — cloud mode off");
    assert!(!is_on());
}

#[tokio::test(start_paused = true)]
async fn wake_that_never_finishes_gives_up_and_turns_off() {
    let _g = SERIAL.lock().await;
    *mode() = None;
    let m = Script::default().statuses(&["deploying"]);
    let say = Lines::default();
    turn_on("s1", m, say.clone(), None);
    for _ in 0..10 {
        settle().await;
    }
    let all = say.all();
    assert_eq!(all[0], "◆ your machine · first boot — setting up, a minute or two");
    assert!(all.iter().any(|l| l.starts_with("· still waking · 3")), "{all:?}");
    assert!(all.last().unwrap().contains("didn't come up"), "{all:?}");
    assert!(all.last().unwrap().ends_with("cloud mode off, /cloud to retry"), "{all:?}");
    assert!(!is_on());
}

#[tokio::test(start_paused = true)]
async fn another_session_is_local_and_drops_the_mode() {
    let _g = SERIAL.lock().await;
    *mode() = None;
    let say = Lines::default();
    turn_on("s1", Script::default().statuses(&["active"]), say.clone(), None);
    assert_eq!(submit("s2", "elsewhere", &say), json!({}));
    assert!(!is_on());
}

#[tokio::test(start_paused = true)]
async fn mode_turned_on_before_gray_has_a_session_adopts_the_first_one() {
    let _g = SERIAL.lock().await;
    *mode() = None;
    let m = Script::default().statuses(&["active"]);
    let say = Lines::default();
    turn_on("", m.clone(), say.clone(), None);
    settle().await;
    assert_eq!(submit("s9", "hey", &say), json!({ "handled": true }));
    assert_eq!(submit("s9", "again", &say), json!({ "handled": true }));
    settle().await;
    assert_eq!(m.sent().len(), 2);
    turn_off();
}

#[tokio::test(start_paused = true)]
async fn new_rotates_the_conversation() {
    let _g = SERIAL.lock().await;
    *mode() = None;
    let m = Script::default().statuses(&["active"]);
    let say = Lines::default();
    turn_on("s1", m.clone(), say.clone(), Some(Job::Fresh(None)));
    settle().await;
    assert_eq!(m.sent(), vec![(String::new(), true)]);
    assert_eq!(say.all().last().unwrap(), "◆ new conversation on your machine");
    turn_on("s1", m.clone(), say.clone(), Some(Job::Fresh(Some("start over".into()))));
    settle().await;
    assert_eq!(m.sent()[1], ("start over".to_string(), true));
    turn_off();
}

#[test]
fn replies_get_a_mark_and_aligned_continuation_lines() {
    assert_eq!(reply_block("one\ntwo\n\n"), "◆ one\n  two");
    assert_eq!(reply_block("  \n"), "◆ (no reply)");
}

#[test]
fn slash_routes_one_shot_verbs_back_to_the_cli_path() {
    for v in ["status", "connect", "logout", "help", "/status"] {
        assert!(slash(&[v.to_string()], "s").is_none(), "{v}");
    }
    let tty = slash(&["tty".to_string()], "s").unwrap();
    assert_eq!(tty["terminal"], json!(["cloud", "tty"]));
}
