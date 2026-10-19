use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;

fn entries() -> BTreeMap<String, Entry> {
    parse_index(&json!({"plugins": {
        "account": {
            "version": "0.5.1",
            "description": "gray.alignment.id account",
            "author": "official",
            "official": true,
            "license": "MIT",
            "commands": ["/login", "/cloud"],
            "homepage": "https://github.com/vstaln/gray-account"
        },
        "fsearch": {
            "version": "1.2.0",
            "description": "fast file search",
            "author": "dan",
            "verified": true,
            "commands": ["/find"],
            "requires_plugins": ["account"]
        },
        "weather": {"version": "0.1.0", "description": null, "author": "someone", "license": null}
    }}))
}

fn model() -> Model {
    let mut m = Model::new(String::new());
    m.loaded(entries(), BTreeSet::from(["account".to_string()]));
    m
}

fn press(m: &mut Model, code: KeyCode) -> Action {
    m.key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn typed(m: &mut Model, text: &str) {
    for c in text.chars() {
        press(m, KeyCode::Char(c));
    }
}

fn screen(m: &mut Model, w: u16, h: u16) -> String {
    m.clamp(list_rows(Rect::new(0, 0, w, h)));
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| draw(f, m, Theme { plain: true }, Duration::from_secs(3)))
        .unwrap();
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn index_entries_tolerate_null_and_missing_fields() {
    let e = entries();
    assert_eq!(e.len(), 3);
    assert!(e["account"].official);
    assert_eq!(e["account"].commands, ["/login", "/cloud"]);
    assert_eq!(e["fsearch"].requires, ["account"]);
    assert_eq!(e["weather"].description, "");
    assert_eq!(e["weather"].license, "");
    assert!(parse_index(&json!({"nope": 1})).is_empty());
}

#[test]
fn installed_set_reads_schema_one_lock_files_only() {
    let dir = std::env::temp_dir().join(format!("ga-mkt-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("lock.json");
    std::fs::write(&path, r#"{"schema":1,"plugins":{"account":{},"cloud":{}}}"#).unwrap();
    assert_eq!(
        installed_from_lock(&path),
        BTreeSet::from(["account".to_string(), "cloud".to_string()])
    );
    std::fs::write(&path, r#"{"schema":2,"plugins":{"account":{}}}"#).unwrap();
    assert!(installed_from_lock(&path).is_empty());
    assert!(installed_from_lock(&dir.join("missing.json")).is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn safe_text_strips_escapes_bidi_and_controls() {
    let raw = "\x1b[31mred\x1b[0m \x1b]8;;http://x\x07link\x1b]8;;\x07 \u{202e}evil\tz\r";
    assert_eq!(safe_text(raw), "red link evil z");
    assert_eq!(safe_text("a\x1b]0;title\x1b\\b\u{9b}2Jc"), "abc");
    assert_eq!(safe_text("one\n  two  three "), "one\ntwo three");
}

#[test]
fn search_matches_name_description_author_and_commands() {
    let e = entries();
    assert_eq!(filter(&e, ""), ["account", "fsearch", "weather"]);
    assert_eq!(filter(&e, "FIND"), ["fsearch"]);
    assert_eq!(filter(&e, "alignment"), ["account"]);
    assert_eq!(filter(&e, "someone"), ["weather"]);
    assert!(filter(&e, "zzz").is_empty());
}

#[test]
fn name_matches_rank_above_description_and_command_matches() {
    let e = parse_index(&json!({"plugins": {
        "account": {"commands": ["/cloud"]},
        "cloud": {},
        "cloudsync": {},
        "mycloud": {},
        "voice": {"description": "talk to your cloud"}
    }}));
    assert_eq!(
        filter(&e, "Cloud"),
        ["cloud", "cloudsync", "mycloud", "account", "voice"]
    );
}

#[test]
fn typing_starts_search_and_escape_clears_then_quits() {
    let mut m = model();
    press(&mut m, KeyCode::Down);
    assert_eq!(m.selected, 1);
    typed(&mut m, "we");
    assert!(m.search_focused);
    assert_eq!(m.query, "we");
    assert_eq!(m.selected, 0);
    assert_eq!(m.visible(), ["weather"]);
    press(&mut m, KeyCode::Backspace);
    assert_eq!(m.query, "w");
    assert_eq!(press(&mut m, KeyCode::Esc), Action::None);
    assert!(!m.search_focused);
    assert_eq!(m.query, "");
    assert_eq!(press(&mut m, KeyCode::Esc), Action::Quit);
}

#[test]
fn enter_reports_installed_or_confirms_before_installing() {
    let mut m = model();
    assert_eq!(press(&mut m, KeyCode::Enter), Action::None);
    assert!(
        m.status
            .as_ref()
            .unwrap()
            .text
            .contains("already installed")
    );
    press(&mut m, KeyCode::Char('j'));
    assert_eq!(press(&mut m, KeyCode::Enter), Action::None);
    assert_eq!(m.phase, Phase::Confirm("fsearch".into()));
    assert_eq!(press(&mut m, KeyCode::Esc), Action::None);
    assert_eq!(m.phase, Phase::Browsing);
    press(&mut m, KeyCode::Enter);
    assert_eq!(
        press(&mut m, KeyCode::Enter),
        Action::Install("fsearch".into())
    );
    assert_eq!(m.phase, Phase::Browsing);
}

#[test]
fn reload_retry_and_ctrl_c() {
    let mut m = model();
    assert_eq!(press(&mut m, KeyCode::Char('r')), Action::Reload);
    m.phase = Phase::LoadError("down".into());
    assert_eq!(press(&mut m, KeyCode::Char('r')), Action::Reload);
    assert_eq!(
        m.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Action::Quit
    );
}

#[test]
fn clamp_keeps_the_selection_in_view() {
    let mut m = model();
    press(&mut m, KeyCode::End);
    assert_eq!(m.selected, 2);
    m.clamp(2);
    assert_eq!(m.offset, 1);
    press(&mut m, KeyCode::Home);
    m.clamp(2);
    assert_eq!(m.offset, 0);
    m.selected = 99;
    m.clamp(10);
    assert_eq!((m.selected, m.offset), (2, 0));
}

#[test]
fn wide_screen_shows_search_list_and_details_side_by_side() {
    let mut m = model();
    let s = screen(&mut m, 120, 36);
    assert!(s.contains("⬡ gray marketplace"), "{s}");
    assert!(s.contains("/ to search"));
    assert!(s.contains("3 plugins"));
    assert!(s.contains("> ● account"));
    assert!(s.contains("◐ fsearch"));
    assert!(s.contains("○ weather"));
    assert!(s.contains("│"));
    assert!(s.contains("✓ installed"));
    assert!(s.contains("Commands  /login  /cloud"));
    assert!(s.contains("Enter install"));
    let row = s.lines().find(|l| l.contains("● account")).unwrap();
    assert!(row.contains("account  0.5.1"), "{row}");
}

#[test]
fn search_count_and_empty_results() {
    let mut m = model();
    typed(&mut m, "zzz");
    let s = screen(&mut m, 120, 36);
    assert!(s.contains("/ zzz█"));
    assert!(s.contains("0 of 3"));
    assert!(s.contains("no matching plugins"));
}

#[test]
fn narrow_screen_stacks_and_tiny_screen_asks_to_enlarge() {
    let mut m = model();
    let s = screen(&mut m, 60, 24);
    assert!(s.contains("● account"));
    assert!(s.contains("─────"));
    assert!(s.contains("Author"));
    let s = screen(&mut m, 30, 10);
    assert!(s.contains("enlarge the terminal"));
}

#[test]
fn loading_error_and_confirm_screens() {
    let mut m = Model::new(String::new());
    assert!(screen(&mut m, 100, 30).contains("⬡ loading marketplace…"));
    m.phase = Phase::LoadError("could not reach \x1b[2Jregistry".into());
    let s = screen(&mut m, 100, 30);
    assert!(s.contains("marketplace unavailable"));
    assert!(s.contains("could not reach registry"));
    assert!(s.contains("r retry"));
    let mut m = model();
    m.phase = Phase::Confirm("fsearch".into());
    assert!(screen(&mut m, 100, 30).contains("install fsearch 1.2.0?  Enter confirm · Esc cancel"));
}

#[test]
fn untrusted_names_and_descriptions_never_reach_the_screen_raw() {
    let mut m = Model::new(String::new());
    m.loaded(
        parse_index(&json!({"plugins": {"evil\x1b[2J": {"version": "1", "description": "x\x1b]0;pwn\x07y"}}})),
        BTreeSet::new(),
    );
    let s = screen(&mut m, 100, 30);
    assert!(!s.contains('\x1b'));
    assert!(s.contains("evil"));
    assert!(s.contains("xy"));
}

#[test]
fn summary_lists_matches_and_the_install_hint() {
    let e = entries();
    let s = summary_text(&e, "");
    assert!(s.starts_with("3 plugins in the gray marketplace"));
    assert!(s.contains("\n  fsearch  1.2.0  — fast file search"));
    assert!(s.contains("\n  weather  0.1.0\n"));
    assert!(s.ends_with("install: gray plugin install <name>"));
    assert!(summary_text(&e, "zzz").starts_with("0 of 3 plugins match 'zzz'"));
    let many: BTreeMap<String, Entry> = (0..25)
        .map(|i| (format!("p{i:02}"), Entry::default()))
        .collect();
    assert!(summary_text(&many, "").contains("… and 5 more"));
}
