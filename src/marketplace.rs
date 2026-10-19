//! `/marketplace`: browse the gray plugin registry and install from it.
//!
//! This plugin owns the browsing screen only. Installing hands the real
//! terminal to `gray plugin install <name>`, so gray core's download, hash
//! check, security scan and capability consent stay the one install path;
//! core reloads the agent afterwards when the installed set changed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// One registry index entry, reduced to what the screen shows. Built by
/// hand from JSON so a null or missing field never hides a plugin.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub version: String,
    pub description: String,
    pub author: String,
    pub official: bool,
    pub verified: bool,
    pub license: String,
    pub kind: String,
    pub homepage: String,
    pub repo: String,
    pub commands: Vec<String>,
    pub requires: Vec<String>,
    pub conflicts: Vec<String>,
}

impl Entry {
    fn from_value(v: &Value) -> Self {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        let list = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            version: s("version"),
            description: s("description"),
            author: s("author"),
            official: b("official"),
            verified: b("verified"),
            license: s("license"),
            kind: s("kind"),
            homepage: s("homepage"),
            repo: s("repo"),
            commands: list("commands"),
            requires: list("requires_plugins"),
            conflicts: list("conflicts"),
        }
    }
}

/// `plugins/index.json` is `{"plugins": {name: entry}}`.
pub fn parse_index(index: &Value) -> BTreeMap<String, Entry> {
    index
        .get("plugins")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), Entry::from_value(v)))
                .collect()
        })
        .unwrap_or_default()
}

/// Installed plugin names from gray's lock file; unknown schema or a
/// missing file reads as nothing installed rather than an error.
pub fn installed_from_lock(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|lock| lock.get("schema").and_then(Value::as_u64) == Some(1))
        .and_then(|lock| {
            lock.get("plugins")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
        })
        .unwrap_or_default()
}

fn installed() -> BTreeSet<String> {
    crate::account::gray_home()
        .map(|home| installed_from_lock(&home.join("plugins/lock.json")))
        .unwrap_or_default()
}

/// Registry strings are untrusted: drop escape sequences (CSI, OSC, DCS…),
/// bidi overrides and control characters, and collapse whitespace, so a
/// description can never repaint or reorder the screen.
pub fn safe_text(raw: &str) -> String {
    let mut plain = String::new();
    let mut chars = raw.chars();
    let skip_csi = |chars: &mut std::str::Chars<'_>| {
        for c in chars.by_ref() {
            if ('\x40'..='\x7e').contains(&c) {
                break;
            }
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => skip_csi(&mut chars),
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || c == '\u{9c}' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\u{9b}' => skip_csi(&mut chars),
            '\n' => plain.push('\n'),
            '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {}
            c if c.is_control() => plain.push(' '),
            c => plain.push(c),
        }
    }
    plain
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
}

fn one_line(raw: &str) -> String {
    safe_text(raw).replace('\n', " ")
}

fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn elide(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Names matching `query` (case-insensitive) in name, description, author
/// or commands. Name matches rank first (exact, then prefix, then anywhere),
/// ties keep index order. An empty query matches everything.
pub fn filter<'a>(entries: &'a BTreeMap<String, Entry>, query: &str) -> Vec<&'a str> {
    let q = query.trim().to_lowercase();
    let rank = |name: &str, e: &Entry| -> Option<u8> {
        let name = name.to_lowercase();
        if q.is_empty() || name == q {
            Some(0)
        } else if name.starts_with(&q) {
            Some(1)
        } else if name.contains(&q) {
            Some(2)
        } else if e.description.to_lowercase().contains(&q)
            || e.author.to_lowercase().contains(&q)
            || e.commands.iter().any(|c| c.to_lowercase().contains(&q))
        {
            Some(3)
        } else {
            None
        }
    };
    let mut ranked: Vec<(u8, &str)> = entries
        .iter()
        .filter_map(|(name, e)| rank(name, e).map(|r| (r, name.as_str())))
        .collect();
    ranked.sort_by_key(|(r, _)| *r);
    ranked.into_iter().map(|(_, name)| name).collect()
}

/// The `/marketplace` reply text where there is no terminal to hand over
/// (gateway, `gray -c`): the matching entries and how to install one.
pub fn summary_text(entries: &BTreeMap<String, Entry>, query: &str) -> String {
    const SHOWN: usize = 20;
    let names = filter(entries, query);
    let mut out = if query.trim().is_empty() {
        format!("{} plugins in the gray marketplace", entries.len())
    } else {
        format!(
            "{} of {} plugins match '{}'",
            names.len(),
            entries.len(),
            one_line(query.trim())
        )
    };
    for name in names.iter().take(SHOWN) {
        let e = &entries[*name];
        out.push_str(&format!("\n  {}  {}", one_line(name), one_line(&e.version)));
        let description = one_line(&e.description);
        if !description.is_empty() {
            out.push_str(&format!("  — {}", elide(&description, 70)));
        }
    }
    if names.len() > SHOWN {
        out.push_str(&format!("\n  … and {} more", names.len() - SHOWN));
    }
    out.push_str("\ninstall: gray plugin install <name>");
    out
}

pub async fn summary(query: &str) -> anyhow::Result<String> {
    let index = crate::publish::fetch_index().await?;
    Ok(summary_text(&parse_index(&index), query))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Loading,
    LoadError(String),
    Browsing,
    /// Asking before installing this plugin.
    Confirm(String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Reload,
    Install(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub ok: bool,
    pub text: String,
}

pub struct Model {
    pub phase: Phase,
    pub entries: BTreeMap<String, Entry>,
    pub installed: BTreeSet<String>,
    pub query: String,
    pub search_focused: bool,
    pub selected: usize,
    pub offset: usize,
    pub scroll: u16,
    pub status: Option<Status>,
}

impl Model {
    pub fn new(query: String) -> Self {
        Self {
            phase: Phase::Loading,
            entries: BTreeMap::new(),
            installed: BTreeSet::new(),
            query,
            search_focused: false,
            selected: 0,
            offset: 0,
            scroll: 0,
            status: None,
        }
    }

    pub fn loaded(&mut self, entries: BTreeMap<String, Entry>, installed: BTreeSet<String>) {
        self.entries = entries;
        self.installed = installed;
        self.phase = Phase::Browsing;
        self.selected = 0;
        self.offset = 0;
        self.scroll = 0;
    }

    pub fn visible(&self) -> Vec<&str> {
        filter(&self.entries, &self.query)
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.visible().get(self.selected).copied()
    }

    /// Keeps the selection inside the list and the list window around it.
    pub fn clamp(&mut self, rows: usize) {
        let len = self.visible().len();
        let rows = rows.max(1);
        self.selected = self.selected.min(len.saturating_sub(1));
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + rows {
            self.offset = self.selected + 1 - rows;
        }
        self.offset = self.offset.min(len.saturating_sub(rows));
    }

    fn select(&mut self, index: usize) {
        self.selected = index.min(self.visible().len().saturating_sub(1));
        self.scroll = 0;
        self.status = None;
    }

    fn move_by(&mut self, delta: isize) {
        self.select(self.selected.saturating_add_signed(delta));
    }

    fn set_query(&mut self, query: String) {
        self.query = query;
        self.selected = 0;
        self.offset = 0;
        self.scroll = 0;
        self.status = None;
    }

    fn request_install(&mut self) -> Action {
        let Some(name) = self.selected_name().map(str::to_string) else {
            return Action::None;
        };
        if self.installed.contains(&name) {
            self.status = Some(Status {
                ok: true,
                text: format!("{} is already installed", one_line(&name)),
            });
        } else {
            self.status = None;
            self.phase = Phase::Confirm(name);
        }
        Action::None
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        match self.phase.clone() {
            Phase::Loading => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => Action::Quit,
                _ => Action::None,
            },
            Phase::LoadError(_) => match key.code {
                KeyCode::Char('r') => Action::Reload,
                KeyCode::Esc | KeyCode::Char('q') => Action::Quit,
                _ => Action::None,
            },
            Phase::Confirm(name) => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.phase = Phase::Browsing;
                    Action::Install(name)
                }
                KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                    self.phase = Phase::Browsing;
                    Action::None
                }
                _ => Action::None,
            },
            Phase::Browsing if self.search_focused => {
                match key.code {
                    KeyCode::Enter => self.search_focused = false,
                    KeyCode::Esc => {
                        self.search_focused = false;
                        self.set_query(String::new());
                    }
                    KeyCode::Backspace => {
                        let mut query = self.query.clone();
                        query.pop();
                        self.set_query(query);
                    }
                    KeyCode::Up => self.move_by(-1),
                    KeyCode::Down => self.move_by(1),
                    KeyCode::Char(c) if !ctrl && !c.is_control() => {
                        let mut query = self.query.clone();
                        query.push(c);
                        self.set_query(query);
                    }
                    _ => {}
                }
                Action::None
            }
            Phase::Browsing => {
                match key.code {
                    KeyCode::Char('/') => self.search_focused = true,
                    KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
                    KeyCode::Home | KeyCode::Char('g') => self.select(0),
                    KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX),
                    KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(5),
                    KeyCode::PageDown => self.scroll = self.scroll.saturating_add(5).min(200),
                    KeyCode::Enter => return self.request_install(),
                    KeyCode::Esc if !self.query.is_empty() => self.set_query(String::new()),
                    KeyCode::Esc | KeyCode::Char('q') => return Action::Quit,
                    KeyCode::Char('r') => return Action::Reload,
                    KeyCode::Char(c) if !ctrl && !c.is_control() && c != ' ' => {
                        self.search_focused = true;
                        let mut query = self.query.clone();
                        query.push(c);
                        self.set_query(query);
                    }
                    _ => {}
                }
                Action::None
            }
        }
    }
}

type Rgb = (u8, u8, u8);
const ACCENT: Rgb = (246, 173, 126);
const ACCENT_SOFT: Rgb = (254, 215, 170);
const SUCCESS: Rgb = (74, 222, 128);
const ERROR: Rgb = (239, 68, 68);
const INFO: Rgb = (59, 130, 246);
const TEXT_BRIGHT: Rgb = (245, 245, 245);
const TEXT_BODY: Rgb = (225, 225, 225);
const TEXT_MUTED: Rgb = (140, 140, 140);
const TEXT_DIM: Rgb = (110, 110, 110);
const TEXT_FAINT: Rgb = (70, 70, 70);
const ON_SELECTION: Rgb = (22, 22, 22);

/// gray's palette, or none at all under `NO_COLOR`.
#[derive(Clone, Copy)]
pub struct Theme {
    pub plain: bool,
}

impl Theme {
    pub fn from_env() -> Self {
        Self {
            plain: crate::ui::no_color(),
        }
    }

    fn fg(self, (r, g, b): Rgb) -> Style {
        if self.plain {
            Style::default()
        } else {
            Style::default().fg(Color::Rgb(r, g, b))
        }
    }

    fn bold(self, rgb: Rgb) -> Style {
        self.fg(rgb).add_modifier(Modifier::BOLD)
    }

    fn selection(self) -> Style {
        if self.plain {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            let ((r, g, b), (br, bg, bb)) = (ON_SELECTION, ACCENT);
            Style::default()
                .fg(Color::Rgb(r, g, b))
                .bg(Color::Rgb(br, bg, bb))
                .add_modifier(Modifier::BOLD)
        }
    }
}

fn trust(e: &Entry) -> (&'static str, &'static str, Rgb) {
    if e.official {
        ("●", "official", ACCENT_SOFT)
    } else if e.verified {
        ("◐", "verified community", INFO)
    } else {
        ("○", "unverified community", TEXT_DIM)
    }
}

const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 14;
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 90;
const KEY_WIDTH: usize = 10;

struct Areas {
    title: Rect,
    search: Rect,
    body: Rect,
    list: Rect,
    details: Rect,
    /// The rule between list and details, and whether it runs vertically.
    divider: (Rect, bool),
    status: Rect,
    footer: Rect,
}

fn areas(area: Rect) -> Option<Areas> {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return None;
    }
    let (x, w) = (area.x + 2, area.width - 4);
    let row = |y: u16| Rect::new(x, y, w, 1);
    let status = row(area.bottom() - 2);
    let top = area.y + 5;
    let body = Rect::new(x, top, w, status.y.saturating_sub(1).saturating_sub(top));
    let (list, divider, details) = if area.width >= SIDE_BY_SIDE_MIN_WIDTH {
        // Wide enough to keep the trust icon, version and installed mark.
        let lw = ((u32::from(body.width) * 42 / 100) as u16)
            .max(42)
            .min(body.width);
        (
            Rect::new(body.x, body.y, lw, body.height),
            (Rect::new(body.x + lw + 1, body.y, 1, body.height), true),
            Rect::new(
                body.x + lw + 3,
                body.y,
                body.width.saturating_sub(lw + 3),
                body.height,
            ),
        )
    } else {
        let lh = (body.height / 2).clamp(1, 12);
        (
            Rect::new(body.x, body.y, body.width, lh),
            (Rect::new(body.x, body.y + lh, body.width, 1), false),
            Rect::new(
                body.x,
                body.y + lh + 1,
                body.width,
                body.height.saturating_sub(lh + 1),
            ),
        )
    };
    Some(Areas {
        title: row(area.y + 1),
        search: row(area.y + 3),
        body,
        list,
        details,
        divider,
        status,
        footer: row(area.bottom() - 1),
    })
}

/// How many list rows the current screen shows, for [`Model::clamp`].
pub fn list_rows(area: Rect) -> usize {
    areas(area).map_or(1, |a| usize::from(a.list.height))
}

pub fn draw(frame: &mut Frame<'_>, model: &Model, theme: Theme, elapsed: Duration) {
    let area = frame.area();
    let Some(a) = areas(area) else {
        let y = area.y + area.height / 2;
        frame.render_widget(
            Paragraph::new("enlarge the terminal to browse the marketplace")
                .alignment(Alignment::Center)
                .style(theme.fg(TEXT_MUTED)),
            Rect::new(area.x, y, area.width, area.height.min(1)),
        );
        return;
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("⬡ ", theme.fg(ACCENT)),
            Span::styled("gray marketplace", theme.bold(TEXT_BRIGHT)),
            Span::styled("  plugins for the gray agent harness", theme.fg(TEXT_DIM)),
        ])),
        a.title,
    );
    frame.render_widget(
        Paragraph::new(Line::from(footer_spans(model, theme))),
        a.footer,
    );
    if let Some(line) = status_line(model, theme) {
        frame.render_widget(Paragraph::new(line), a.status);
    }
    match &model.phase {
        Phase::Loading => {
            let label = "⬡ loading marketplace…";
            let spans: Vec<Span<'static>> = if theme.plain {
                vec![Span::raw(label)]
            } else {
                crate::ui::shimmer_cells(label, elapsed)
                    .into_iter()
                    .map(|(ch, c, bold)| {
                        let mut style = Style::default().fg(Color::Rgb(c[0], c[1], c[2]));
                        if bold {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        Span::styled(ch.to_string(), style)
                    })
                    .collect()
            };
            let lines = vec![
                Line::from(spans),
                Line::from(Span::styled(
                    format!("{}s", elapsed.as_secs()),
                    theme.fg(TEXT_FAINT),
                )),
            ];
            let y = a.body.y + a.body.height.saturating_sub(2) / 2;
            frame.render_widget(
                Paragraph::new(lines).alignment(Alignment::Center),
                Rect::new(a.body.x, y, a.body.width, 2.min(a.body.height)),
            );
        }
        Phase::LoadError(message) => {
            let lines = vec![
                Line::from(vec![
                    Span::styled("✗ ", theme.fg(ERROR)),
                    Span::styled("marketplace unavailable", theme.bold(ERROR)),
                ]),
                Line::default(),
                Line::from(Span::styled(one_line(message), theme.fg(TEXT_BODY))),
            ];
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), a.body);
        }
        Phase::Browsing | Phase::Confirm(_) => draw_browsing(frame, &a, model, theme),
    }
}

fn draw_browsing(frame: &mut Frame<'_>, a: &Areas, model: &Model, theme: Theme) {
    frame.render_widget(
        Paragraph::new(search_line(model, usize::from(a.search.width), theme)),
        a.search,
    );
    let (rule, vertical) = a.divider;
    let rule_text: Vec<Line> = if vertical {
        vec![Line::from("│"); usize::from(rule.height)]
    } else {
        vec![Line::from("─".repeat(usize::from(rule.width)))]
    };
    frame.render_widget(Paragraph::new(rule_text).style(theme.fg(TEXT_FAINT)), rule);
    let names = model.visible();
    if names.is_empty() {
        let text = if model.entries.is_empty() {
            "no plugins available"
        } else {
            "no matching plugins"
        };
        frame.render_widget(Paragraph::new(text).style(theme.fg(TEXT_MUTED)), a.list);
        return;
    }
    let mut state = ListState::default()
        .with_selected(Some(model.selected))
        .with_offset(model.offset);
    frame.render_stateful_widget(
        List::new(list_items(model, &names, a.list.width, theme))
            .highlight_symbol("> ")
            .highlight_style(theme.selection()),
        a.list,
        &mut state,
    );
    let lines = details_lines(model, theme);
    let scroll = model.scroll.min(lines.len().saturating_sub(1) as u16);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        a.details,
    );
}

fn search_line(model: &Model, cols: usize, theme: Theme) -> Line<'static> {
    let query = one_line(&model.query);
    let mut spans = if model.search_focused {
        vec![
            Span::styled("/ ", theme.bold(ACCENT)),
            Span::styled(query, theme.fg(TEXT_BRIGHT)),
            Span::styled("█", theme.fg(ACCENT)),
        ]
    } else if query.is_empty() {
        vec![Span::styled("/ to search", theme.fg(TEXT_DIM))]
    } else {
        vec![
            Span::styled("/ ", theme.fg(TEXT_DIM)),
            Span::styled(query, theme.fg(TEXT_BODY)),
        ]
    };
    let total = model.entries.len();
    let count = if model.query.trim().is_empty() {
        format!("{total} plugin{}", if total == 1 { "" } else { "s" })
    } else {
        format!("{} of {total}", model.visible().len())
    };
    let used: usize = spans.iter().map(|s| width(&s.content)).sum();
    if used + width(&count) + 2 <= cols {
        spans.push(Span::raw(" ".repeat(cols - used - width(&count))));
        spans.push(Span::styled(count, theme.fg(TEXT_DIM)));
    }
    Line::from(spans)
}

fn list_items(
    model: &Model,
    names: &[&str],
    list_width: u16,
    theme: Theme,
) -> Vec<ListItem<'static>> {
    // The list reserves the highlight symbol's width on every row.
    let row_width = usize::from(list_width).saturating_sub(2);
    let show_version = row_width >= 28;
    let version_width = if show_version {
        names
            .iter()
            .map(|name| width(&one_line(&model.entries[*name].version)))
            .max()
            .unwrap_or(0)
            .min(12)
    } else {
        0
    };
    let fixed = 2 + if show_version { version_width + 1 } else { 0 } + 2;
    let name_budget = row_width.saturating_sub(fixed).max(4);
    names
        .iter()
        .map(|name| {
            let entry = &model.entries[*name];
            let (icon, _, color) = trust(entry);
            let label = elide(&one_line(name), name_budget);
            let pad = name_budget.saturating_sub(width(&label));
            let mut spans = vec![
                Span::styled(icon, theme.fg(color)),
                Span::raw(" "),
                Span::styled(label, theme.fg(TEXT_BODY)),
                Span::raw(" ".repeat(pad)),
            ];
            if show_version {
                let version = elide(&one_line(&entry.version), version_width);
                let lead = version_width.saturating_sub(width(&version));
                spans.push(Span::styled(
                    format!(" {}{version}", " ".repeat(lead)),
                    theme.fg(TEXT_DIM),
                ));
            }
            spans.push(Span::raw(" "));
            spans.push(if model.installed.contains(*name) {
                Span::styled("✓", theme.fg(SUCCESS))
            } else {
                Span::raw(" ")
            });
            ListItem::new(Line::from(spans))
        })
        .collect()
}

fn kv(key: &str, value: Vec<Span<'static>>, theme: Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{key:<KEY_WIDTH$}"),
        theme.fg(TEXT_DIM),
    )];
    spans.extend(value);
    Line::from(spans)
}

fn tokens(items: &[String], theme: Theme, rgb: Rgb) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(one_line(item), theme.fg(rgb)));
    }
    spans
}

fn details_lines(model: &Model, theme: Theme) -> Vec<Line<'static>> {
    let Some(name) = model.selected_name() else {
        return Vec::new();
    };
    let entry = &model.entries[name];
    let installed = model.installed.contains(name);
    let value = |raw: &str| Span::styled(one_line(raw), theme.fg(TEXT_BODY));
    let mut header = vec![
        Span::styled(one_line(name), theme.bold(TEXT_BRIGHT)),
        Span::styled(
            format!("  {}", one_line(&entry.version)),
            theme.fg(TEXT_DIM),
        ),
    ];
    if installed {
        header.push(Span::styled("  ✓ installed", theme.fg(SUCCESS)));
    }
    let mut lines = vec![Line::from(header)];
    let description = safe_text(&entry.description);
    if !description.is_empty() {
        lines.extend(
            description
                .lines()
                .map(|l| Line::from(Span::styled(l.to_string(), theme.fg(TEXT_BODY)))),
        );
    }
    lines.push(Line::default());
    if !entry.author.trim().is_empty() {
        lines.push(kv("Author", vec![value(&entry.author)], theme));
    }
    let (icon, label, color) = trust(entry);
    lines.push(kv(
        "Trust",
        vec![
            Span::styled(icon, theme.fg(color)),
            Span::styled(format!(" {label}"), theme.fg(TEXT_BODY)),
        ],
        theme,
    ));
    if !entry.kind.trim().is_empty() {
        lines.push(kv("Kind", vec![value(&entry.kind)], theme));
    }
    if !entry.license.trim().is_empty() {
        lines.push(kv("License", vec![value(&entry.license)], theme));
    }
    let lists = [
        ("Commands", &entry.commands, ACCENT_SOFT),
        ("Requires", &entry.requires, TEXT_BODY),
        ("Conflicts", &entry.conflicts, ERROR),
    ];
    if lists.iter().any(|(_, items, _)| !items.is_empty()) {
        lines.push(Line::default());
        for (key, items, rgb) in lists {
            if !items.is_empty() {
                lines.push(kv(key, tokens(items, theme, rgb), theme));
            }
        }
    }
    let links = [("Homepage", &entry.homepage), ("Repo", &entry.repo)];
    if links.iter().any(|(_, url)| !url.trim().is_empty()) {
        lines.push(Line::default());
        for (key, url) in links {
            if !url.trim().is_empty() {
                lines.push(kv(
                    key,
                    vec![Span::styled(one_line(url), theme.fg(TEXT_DIM))],
                    theme,
                ));
            }
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        if installed {
            format!(
                "installed · update with gray plugin update {}",
                one_line(name)
            )
        } else {
            "Enter to install".to_string()
        },
        theme.fg(TEXT_DIM),
    )));
    lines
}

fn status_line(model: &Model, theme: Theme) -> Option<Line<'static>> {
    if let Phase::Confirm(name) = &model.phase {
        let version = model
            .entries
            .get(name)
            .map(|e| one_line(&e.version))
            .unwrap_or_default();
        return Some(Line::from(vec![
            Span::styled("install ", theme.fg(TEXT_BODY)),
            Span::styled(one_line(name), theme.bold(ACCENT)),
            Span::styled(format!(" {version}"), theme.fg(TEXT_DIM)),
            Span::styled("?  ", theme.fg(TEXT_BODY)),
            Span::styled("Enter ", theme.bold(TEXT_BRIGHT)),
            Span::styled("confirm", theme.fg(TEXT_DIM)),
            Span::styled(" · ", theme.fg(TEXT_FAINT)),
            Span::styled("Esc ", theme.bold(TEXT_BRIGHT)),
            Span::styled("cancel", theme.fg(TEXT_DIM)),
        ]));
    }
    let status = model.status.as_ref()?;
    let (mark, rgb) = if status.ok {
        ("✓ ", SUCCESS)
    } else {
        ("✗ ", ERROR)
    };
    Some(Line::from(vec![
        Span::styled(mark, theme.fg(rgb)),
        Span::styled(one_line(&status.text), theme.fg(TEXT_BODY)),
    ]))
}

fn footer_spans(model: &Model, theme: Theme) -> Vec<Span<'static>> {
    let keys: &[(&str, &str)] = match model.phase {
        Phase::Loading => &[("q", "quit")],
        Phase::LoadError(_) => &[("r", "retry"), ("q", "quit")],
        Phase::Confirm(_) => &[],
        Phase::Browsing if model.search_focused => {
            &[("", "type to search"), ("Enter", "done"), ("Esc", "clear")]
        }
        Phase::Browsing => &[
            ("/", "search"),
            ("↑↓/jk", "select"),
            ("Enter", "install"),
            ("PgUp/PgDn", "details"),
            ("r", "reload"),
            ("q", "quit"),
        ],
    };
    let mut spans = Vec::new();
    for (i, (key, label)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme.fg(TEXT_FAINT)));
        }
        if !key.is_empty() {
            spans.push(Span::styled(format!("{key} "), theme.bold(TEXT_BRIGHT)));
        }
        spans.push(Span::styled(*label, theme.fg(TEXT_DIM)));
    }
    spans
}

/// `gray account marketplace [query]` on a tty: the full-screen browser.
/// Returns what to print after the screen closes.
pub async fn run(args: &[String]) -> anyhow::Result<String> {
    let query = args.join(" ");
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || screen::run(handle, query)).await?
}

mod screen {
    use std::collections::BTreeMap;
    use std::io::{Stdout, Write as _};
    use std::process::ExitStatus;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use crossterm::event::{self, Event, KeyEventKind};
    use crossterm::{cursor, execute, terminal};
    use ratatui::Terminal;
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::Rect;

    use super::{Action, Entry, Model, Phase, Status, Theme};

    type Load = mpsc::Receiver<anyhow::Result<BTreeMap<String, Entry>>>;

    fn enter() -> std::io::Result<()> {
        terminal::enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            terminal::EnterAlternateScreen,
            cursor::Hide
        )
    }

    fn leave() {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
    }

    /// The alternate screen and raw mode, restored on every exit path.
    struct Screen(Terminal<CrosstermBackend<Stdout>>);

    impl Drop for Screen {
        fn drop(&mut self) {
            leave();
        }
    }

    fn spawn_load(handle: &tokio::runtime::Handle) -> Load {
        let (tx, rx) = mpsc::channel();
        let handle = handle.clone();
        std::thread::spawn(move || {
            let result = handle
                .block_on(crate::publish::fetch_index())
                .map(|index| super::parse_index(&index));
            let _ = tx.send(result);
        });
        rx
    }

    fn wait_key() {
        if terminal::enable_raw_mode().is_err() {
            return;
        }
        while let Ok(ev) = event::read() {
            if matches!(ev, Event::Key(k) if k.kind != KeyEventKind::Release) {
                break;
            }
        }
        let _ = terminal::disable_raw_mode();
    }

    /// Runs `gray plugin install <name>` on the normal screen so gray core's
    /// scan and capability consent prompt reach the user unchanged.
    fn install(name: &str, version: &str) -> std::io::Result<ExitStatus> {
        let exe = std::env::var_os("GRAY_EXE")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "gray".into());
        let mut out = std::io::stdout();
        let _ = writeln!(
            out,
            "\n{}\n",
            crate::ui::line("⬡", &format!("installing {name}"), &format!(" {version}"))
        );
        let _ = out.flush();
        let status = std::process::Command::new(&exe)
            .args(["plugin", "install", name])
            .status();
        let _ = writeln!(
            out,
            "\n{}",
            crate::ui::dim("press any key to return to the marketplace")
        );
        let _ = out.flush();
        wait_key();
        status
    }

    pub(super) fn run(handle: tokio::runtime::Handle, query: String) -> anyhow::Result<String> {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            leave();
            previous(info);
        }));
        enter()?;
        let mut screen = Screen(Terminal::new(CrosstermBackend::new(std::io::stdout()))?);
        let theme = Theme::from_env();
        let mut model = Model::new(query);
        let mut load = Some(spawn_load(&handle));
        let mut since = Instant::now();
        let mut added: Vec<String> = Vec::new();
        loop {
            if let Some(result) = load.as_ref().and_then(|rx| rx.try_recv().ok()) {
                load = None;
                match result {
                    Ok(entries) => model.loaded(entries, super::installed()),
                    Err(e) => model.phase = Phase::LoadError(format!("{e:#}")),
                }
            }
            let size = screen.0.size()?;
            model.clamp(super::list_rows(Rect::new(0, 0, size.width, size.height)));
            let elapsed = since.elapsed();
            screen.0.draw(|f| super::draw(f, &model, theme, elapsed))?;
            if !event::poll(Duration::from_millis(60))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            match model.key(key) {
                Action::None => {}
                Action::Quit => break,
                Action::Reload => {
                    model.phase = Phase::Loading;
                    model.status = None;
                    load = Some(spawn_load(&handle));
                    since = Instant::now();
                }
                Action::Install(name) => {
                    let version = model
                        .entries
                        .get(&name)
                        .map(|e| super::one_line(&e.version))
                        .unwrap_or_default();
                    leave();
                    let result = install(&name, &version);
                    enter()?;
                    screen.0.clear()?;
                    model.installed = super::installed();
                    let now_installed = model.installed.contains(&name);
                    let shown = super::one_line(&name);
                    model.status = Some(match result {
                        Ok(status) if status.success() && now_installed => {
                            added.push(shown.clone());
                            Status {
                                ok: true,
                                text: format!("installed {shown} {version}"),
                            }
                        }
                        Ok(status) if status.success() => Status {
                            ok: false,
                            text: format!("{shown} was not installed"),
                        },
                        Ok(status) => Status {
                            ok: false,
                            text: format!("installing {shown} failed ({status})"),
                        },
                        Err(e) => Status {
                            ok: false,
                            text: format!("could not run gray: {e}"),
                        },
                    });
                }
            }
        }
        drop(screen);
        let _ = std::panic::take_hook();
        Ok(if added.is_empty() {
            String::new()
        } else {
            format!("installed from the marketplace: {}", added.join(", "))
        })
    }
}

#[cfg(test)]
mod tests;
