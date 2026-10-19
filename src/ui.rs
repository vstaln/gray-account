//! Terminal styling shared by the `/cloud` attach screen and `/marketplace`.

use std::time::Duration;

/// `NO_COLOR` keeps the words and drops the paint.
pub fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

/// gray's working shimmer, per character: the colour and boldness of a
/// bright band sweeping across the label every 2s — the same curve and
/// colours the composer paints `⬡ Working…` with.
pub fn shimmer_cells(text: &str, elapsed: Duration) -> Vec<(char, [u8; 3], bool)> {
    let chars: Vec<char> = text.chars().collect();
    let padding = 10usize;
    let period = chars.len() + padding * 2;
    let pos = ((elapsed.as_secs_f32() % 2.0) / 2.0 * period as f32) as usize;
    let (base, hilite) = ([150.0f32, 148.0, 144.0], [240.0f32, 240.0, 240.0]);
    chars
        .iter()
        .enumerate()
        .map(|(i, &ch)| {
            let dist = ((i + padding) as f32 - pos as f32).abs();
            let t = if dist <= 5.0 {
                0.5 * (1.0 + (std::f32::consts::PI * (dist / 5.0)).cos())
            } else {
                0.0
            };
            let k = t.clamp(0.0, 1.0) * 0.9;
            let c = [0, 1, 2].map(|j| (base[j] + (hilite[j] - base[j]) * k).round() as u8);
            (ch, c, t > 0.3)
        })
        .collect()
}

/// The shimmer as ANSI text, for screens drawn without ratatui.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn shimmer_ansi(text: &str, elapsed: Duration) -> String {
    if no_color() {
        return text.to_string();
    }
    shimmer_cells(text, elapsed)
        .into_iter()
        .map(|(ch, c, bold)| {
            let bold = if bold { "\x1b[1m" } else { "" };
            format!("\x1b[38;2;{};{};{}m{bold}{ch}\x1b[0m", c[0], c[1], c[2])
        })
        .collect()
}

pub fn dim(text: &str) -> String {
    if no_color() {
        return text.to_string();
    }
    format!("\x1b[2m{text}\x1b[0m")
}

/// One status line in gray's accent: a mark, a bold head, a dim tail.
pub fn line(mark: &str, head: &str, tail: &str) -> String {
    if no_color() {
        return format!("{mark} {head}{tail}");
    }
    format!("\x1b[38;2;246;173;126m{mark}\x1b[0m \x1b[1m{head}\x1b[0m\x1b[2m{tail}\x1b[0m")
}
