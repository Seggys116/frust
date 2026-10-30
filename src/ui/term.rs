//! Terminal plumbing shared by the renderer, the virtual terminal and the front end.

use std::env;
use std::time::Duration;

pub const RESET: &str = "\x1b[0m";
pub const LINK_CLOSE: &str = "\x1b]8;;\x1b\\";

/// Applies SGR styling only when colour is on, so call sites never branch on it.
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    colour: bool,
}

impl Paint {
    pub fn new(colour: bool) -> Paint {
        Paint { colour }
    }

    pub fn enabled(self) -> bool {
        self.colour
    }

    fn sgr(self, code: &str, text: &str) -> String {
        if self.colour && !text.is_empty() {
            format!("\x1b[{code}m{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    pub fn bold(self, text: &str) -> String {
        self.sgr("1", text)
    }

    pub fn dim(self, text: &str) -> String {
        self.sgr("2", text)
    }

    pub fn red(self, text: &str) -> String {
        self.sgr("31", text)
    }

    pub fn green(self, text: &str) -> String {
        self.sgr("32", text)
    }

    pub fn yellow(self, text: &str) -> String {
        self.sgr("33", text)
    }

    pub fn cyan(self, text: &str) -> String {
        self.sgr("36", text)
    }

    pub fn blue(self, text: &str) -> String {
        self.sgr("34", text)
    }

    pub fn bold_yellow(self, text: &str) -> String {
        self.sgr("1;33", text)
    }

    pub fn bold_cyan(self, text: &str) -> String {
        self.sgr("1;36", text)
    }

    pub fn bold_blue(self, text: &str) -> String {
        self.sgr("1;34", text)
    }

    pub fn bold_green(self, text: &str) -> String {
        self.sgr("1;32", text)
    }

    pub fn bold_red(self, text: &str) -> String {
        self.sgr("1;31", text)
    }
}

pub fn hyperlink(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}{LINK_CLOSE}")
}

/// An unterminated sequence runs to the end of the input.
fn escape_len(s: &str) -> usize {
    let b = s.as_bytes();
    match b.get(1) {
        None => 1,
        Some(b'[') => {
            let mut i = 2;
            while i < b.len() {
                match b[i] {
                    0x20..=0x3f => i += 1,
                    0x40..=0x7e => return i + 1,
                    _ => return i,
                }
            }
            b.len()
        }
        Some(b']') => {
            let mut i = 2;
            while i < b.len() {
                if b[i] == 0x07 {
                    return i + 1;
                }
                if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
            b.len()
        }
        Some(_) => 1 + s[1..].chars().next().map_or(0, char::len_utf8),
    }
}

enum Piece<'a> {
    Esc(&'a str),
    Char(char),
}

fn pieces(text: &str) -> impl Iterator<Item = Piece<'_>> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let c = rest.chars().next()?;
        if c == '\x1b' {
            let (esc, tail) = rest.split_at(escape_len(rest));
            rest = tail;
            Some(Piece::Esc(esc))
        } else {
            rest = &rest[c.len_utf8()..];
            Some(Piece::Char(c))
        }
    })
}

/// Terminal columns a character occupies. Covers the common wide and zero-width ranges only.
fn char_width(c: char) -> usize {
    let cp = c as u32;
    if c.is_control() || matches!(cp, 0x0300..=0x036F | 0x200B..=0x200F | 0xFE00..=0xFE0F) {
        return 0;
    }
    let wide = matches!(
        cp,
        0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD
    );
    if wide { 2 } else { 1 }
}

pub fn strip_ansi(text: &str) -> String {
    pieces(text)
        .filter_map(|p| match p {
            Piece::Char(c) => Some(c),
            Piece::Esc(_) => None,
        })
        .collect()
}

pub fn visible_width(text: &str) -> usize {
    pieces(text)
        .map(|p| match p {
            Piece::Char(c) => char_width(c),
            Piece::Esc(_) => 0,
        })
        .sum()
}

fn osc8_open(esc: &str) -> Option<bool> {
    let body = esc.strip_prefix("\x1b]8;")?;
    let body = body.strip_suffix('\x07').or_else(|| body.strip_suffix("\x1b\\")).unwrap_or(body);
    let uri = body.split_once(';')?.1;
    Some(!uri.is_empty())
}

/// On a cut, an open hyperlink is closed and styling reset so the line cannot bleed into the next.
pub fn truncate_visible(text: &str, cols: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut width = 0;
    let mut styled = false;
    let mut link_open = false;
    let mut truncated = false;
    for piece in pieces(text) {
        match piece {
            Piece::Esc(esc) => {
                out.push_str(esc);
                styled = true;
                if let Some(open) = osc8_open(esc) {
                    link_open = open;
                }
            }
            Piece::Char(c) => {
                let w = char_width(c);
                if width + w > cols {
                    truncated = true;
                    break;
                }
                width += w;
                out.push(c);
            }
        }
    }
    if truncated {
        if link_open {
            out.push_str(LINK_CLOSE);
        }
        if styled {
            out.push_str(RESET);
        }
    }
    out
}

pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    let hundredths = (secs * 100.0).round() as u64;
    if hundredths < 1000 {
        return format!("{}.{:02}s", hundredths / 100, hundredths % 100);
    }
    let tenths = (secs * 10.0).round() as u64;
    if tenths < 600 {
        return format!("{}.{}s", tenths / 10, tenths % 10);
    }
    let total = secs.round() as u64;
    format!("{}m{:02}s", total / 60, total % 60)
}

pub fn fmt_ms(ms: u64) -> String {
    fmt_duration(Duration::from_millis(ms))
}

fn ioctl_width(fd: i32) -> Option<u16> {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ only writes a `winsize` through the pointer we pass.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws as *mut libc::winsize) };
    (rc == 0 && ws.ws_col > 0).then_some(ws.ws_col)
}

pub fn terminal_width() -> u16 {
    ioctl_width(libc::STDERR_FILENO)
        .or_else(|| env::var("COLUMNS").ok().and_then(|v| v.trim().parse::<u16>().ok()).filter(|&w| w > 0))
        .unwrap_or(100)
}

pub fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty only inspects the descriptor.
    unsafe { libc::isatty(fd) == 1 }
}

/// Precedence: non-empty `NO_COLOR`, then `CLICOLOR_FORCE` (not empty or `0`), then `TERM=dumb`, then tty.
pub fn colour_decision(no_color: Option<&str>, force: Option<&str>, term: Option<&str>, tty: bool) -> bool {
    if no_color.is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if force.is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    if term == Some("dumb") {
        return false;
    }
    tty
}

pub fn colour_enabled() -> bool {
    colour_decision(
        env::var("NO_COLOR").ok().as_deref(),
        env::var("CLICOLOR_FORCE").ok().as_deref(),
        env::var("TERM").ok().as_deref(),
        is_tty(libc::STDERR_FILENO),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_csi_and_osc() {
        assert_eq!(strip_ansi("\x1b[1m\x1b[32mwarning\x1b[0m: x"), "warning: x");
        assert_eq!(strip_ansi("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ end"), "link end");
        assert_eq!(strip_ansi("\x1b]2;title\x07text"), "text");
        assert_eq!(strip_ansi("plain \u{2800} text"), "plain \u{2800} text");
    }

    #[test]
    fn strip_survives_unterminated_sequences() {
        assert_eq!(strip_ansi("ab\x1b[31"), "ab");
        assert_eq!(strip_ansi("ab\x1b"), "ab");
        assert_eq!(strip_ansi("ab\x1b]8;;http://x"), "ab");
    }

    #[test]
    fn width_ignores_escapes() {
        assert_eq!(visible_width("\x1b[31mabc\x1b[0m"), 3);
        assert_eq!(visible_width(&hyperlink("https://docs.rs/x/1", "name")), 4);
        assert_eq!(visible_width("\u{2501}\u{2500}"), 2);
        assert_eq!(visible_width("\u{4e2d}a"), 3);
    }

    #[test]
    fn truncate_plain_text_adds_no_escapes() {
        assert_eq!(truncate_visible("hello world", 5), "hello");
        assert_eq!(truncate_visible("ab", 5), "ab");
        assert_eq!(truncate_visible("hello", 5), "hello");
    }

    #[test]
    fn truncate_keeps_escapes_whole_and_resets() {
        assert_eq!(truncate_visible("\x1b[31mhello world\x1b[0m", 5), "\x1b[31mhello\x1b[0m");
        assert_eq!(truncate_visible("\x1b[31mabc", 0), "\x1b[31m\x1b[0m");
        let untouched = "\x1b[31mhi\x1b[0m";
        assert_eq!(truncate_visible(untouched, 2), untouched);
    }

    #[test]
    fn truncate_closes_open_hyperlinks() {
        let text = format!("\x1b]8;;http://x\x07abcdef{LINK_CLOSE}");
        let cut = truncate_visible(&text, 3);
        assert_eq!(cut, format!("\x1b]8;;http://x\x07abc{LINK_CLOSE}{RESET}"));
        assert_eq!(visible_width(&cut), 3);
    }

    #[test]
    fn truncate_never_exceeds_width() {
        let text = format!("  {} {} {}", "\x1b[32mCompiled\x1b[0m", hyperlink("https://docs.rs/a/1", "a-long-name"), "\x1b[2mv1.2.3\x1b[0m");
        for cols in 0..40 {
            assert!(visible_width(&truncate_visible(&text, cols)) <= cols);
        }
    }

    #[test]
    fn duration_formats() {
        let ms = Duration::from_millis;
        assert_eq!(fmt_duration(ms(0)), "0.00s");
        assert_eq!(fmt_duration(ms(1400)), "1.40s");
        assert_eq!(fmt_duration(ms(9994)), "9.99s");
        assert_eq!(fmt_duration(ms(9996)), "10.0s");
        assert_eq!(fmt_duration(ms(12_340)), "12.3s");
        assert_eq!(fmt_duration(ms(59_960)), "1m00s");
        assert_eq!(fmt_duration(ms(65_000)), "1m05s");
        assert_eq!(fmt_duration(ms(125_400)), "2m05s");
        assert_eq!(fmt_ms(38_400), "38.4s");
    }

    #[test]
    fn colour_policy() {
        assert!(!colour_decision(Some("1"), Some("1"), None, true));
        assert!(colour_decision(Some(""), None, None, true));
        assert!(colour_decision(None, Some("1"), Some("dumb"), false));
        assert!(!colour_decision(None, Some("0"), None, false));
        assert!(!colour_decision(None, None, Some("dumb"), true));
        assert!(colour_decision(None, None, Some("xterm"), true));
        assert!(!colour_decision(None, None, Some("xterm"), false));
    }

    #[test]
    fn paint_respects_colour_flag() {
        assert_eq!(Paint::new(false).green("x"), "x");
        assert_eq!(Paint::new(true).green("x"), "\x1b[32mx\x1b[0m");
        assert_eq!(Paint::new(true).green(""), "");
    }
}
