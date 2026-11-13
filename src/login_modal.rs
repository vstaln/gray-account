use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, poll, read,
};
use crossterm::{cursor, execute, terminal};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::account::{
    Account, SITE_ACCOUNT_URL, exchange_and_store, normalize_code, open_in_browser,
};

const WIDTH: u16 = 64;
const HEIGHT: u16 = 13;

struct Palette {
    raised: Color,
    faint: Color,
    dim: Color,
    muted: Color,
    body: Color,
    bright: Color,
    accent: Color,
}

fn palette() -> Palette {
    use Color::Rgb;
    let mut t = Palette {
        raised: Rgb(28, 28, 28),
        faint: Rgb(70, 70, 70),
        dim: Rgb(110, 110, 110),
        muted: Rgb(140, 140, 140),
        body: Rgb(225, 225, 225),
        bright: Rgb(240, 240, 240),
        accent: Rgb(246, 173, 126),
    };
    for pair in std::env::var("GRAY_MODAL_THEME")
        .unwrap_or_default()
        .split(',')
    {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = v.trim_start_matches('#');
        let p = |i: usize| v.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
        let (Some(r), Some(g), Some(b)) = (p(0), p(2), p(4)) else {
            continue;
        };
        let c = Rgb(r, g, b);
        match k {
            "raised_bg" => t.raised = c,
            "text_faint" => t.faint = c,
            "text_dim" => t.dim = c,
            "text_muted" => t.muted = c,
            "text_body" => t.body = c,
            "text_bright" => t.bright = c,
            "accent" => t.accent = c,
            _ => {}
        }
    }
    t
}

enum Status {
    Busy,
    Error(String),
}

fn draw(frame: &mut Frame, buf: &str, status: Option<&Status>, th: &Palette) {
    let fg = |c: Color| Style::default().fg(c);
    let area = frame.area();
    let w = WIDTH.min(area.width);
    let h = HEIGHT.min(area.height);
    let rect = Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 3,
        w,
        h,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::default().style(Style::default().bg(th.raised)), rect);
    let row = |i: u16, rows: u16| {
        Rect::new(rect.x + 2, rect.y + i, rect.width.saturating_sub(4), rows).intersection(rect)
    };
    let text = |s: &str, c: Color| Paragraph::new(Span::styled(s.to_string(), fg(c)));

    frame.render_widget(
        Paragraph::new(Span::styled(
            "Log in to Gray",
            fg(th.bright).add_modifier(Modifier::BOLD),
        )),
        row(1, 1),
    );
    frame.render_widget(
        text("Opened gray.alignment.id/account in your browser.", th.body),
        row(3, 1),
    );
    frame.render_widget(
        text(
            "Sign in, choose Generate CLI login code, paste it below.",
            th.dim,
        ),
        row(4, 1),
    );
    frame.render_widget(text(SITE_ACCOUNT_URL, th.muted), row(5, 1));

    let room = (row(7, 1).width as usize).saturating_sub(7);
    let count = buf.chars().count();
    let shown: String = buf.chars().skip(count.saturating_sub(room)).collect();
    let mut input = vec![Span::styled("code  ", fg(th.dim))];
    if buf.is_empty() {
        input.push(Span::styled("▌", fg(th.accent)));
        input.push(Span::styled("paste your login code", fg(th.faint)));
    } else {
        input.push(Span::styled(shown, fg(th.bright)));
        input.push(Span::styled("▌", fg(th.accent)));
    }
    frame.render_widget(Paragraph::new(Line::from(input)), row(7, 1));

    let message = match status {
        Some(Status::Busy) => Some(("logging in…".to_string(), th.muted)),
        Some(Status::Error(e)) => Some((e.clone(), th.accent)),
        None => None,
    };
    if let Some((msg, color)) = message {
        frame.render_widget(
            Paragraph::new(Span::styled(msg, fg(color))).wrap(Wrap { trim: true }),
            row(8, 2),
        );
    }

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("enter", fg(th.muted)),
            Span::styled("  log in    ", fg(th.dim)),
            Span::styled("esc", fg(th.muted)),
            Span::styled("  cancel", fg(th.dim)),
        ])),
        row(11, 1),
    );
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = execute!(
            std::io::stdout(),
            cursor::Show,
            DisableBracketedPaste,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

pub async fn run() -> anyhow::Result<Option<Account>> {
    let th = palette();
    terminal::enable_raw_mode()?;
    let _restore = Restore;
    execute!(
        std::io::stdout(),
        terminal::EnterAlternateScreen,
        cursor::Hide,
        EnableBracketedPaste
    )?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    open_in_browser(SITE_ACCOUNT_URL);

    let mut buf = String::new();
    let mut status: Option<Status> = None;
    loop {
        term.draw(|f| draw(f, &buf, status.as_ref(), &th))?;
        if !poll(Duration::from_millis(100))? {
            continue;
        }
        match read()? {
            Event::Paste(text) => {
                buf.extend(text.chars().filter(|c| !c.is_control()));
                status = None;
            }
            Event::Key(KeyEvent {
                code,
                kind,
                modifiers,
                ..
            }) if kind != KeyEventKind::Release => {
                let ctrl = modifiers.contains(KeyModifiers::CONTROL);
                match code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c') if ctrl => return Ok(None),
                    KeyCode::Char('u') if ctrl => {
                        buf.clear();
                        status = None;
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                        status = None;
                    }
                    KeyCode::Char(c) if !ctrl && !c.is_control() => {
                        buf.push(c);
                        status = None;
                    }
                    KeyCode::Enter => match normalize_code(&buf) {
                        None => {
                            status =
                                Some(Status::Error("that doesn't look like a login code".into()));
                        }
                        Some(code) => {
                            status = Some(Status::Busy);
                            term.draw(|f| draw(f, &buf, status.as_ref(), &th))?;
                            match exchange_and_store(&code).await {
                                Ok((_, account)) => return Ok(Some(account)),
                                Err(e) => status = Some(Status::Error(format!("{e:#}"))),
                            }
                        }
                    },
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
