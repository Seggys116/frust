//! Headless virtual terminal; a character in the last column sets a pending wrap rather than wrapping at once, as xterm does.

#[derive(Clone)]
struct Row {
    cells: Vec<char>,
    /// An SGR style was active when something was printed on this row.
    colour: bool,
}

impl Row {
    fn blank(cols: usize) -> Row {
        Row { cells: vec![' '; cols], colour: false }
    }

    fn text(&self) -> String {
        let s: String = self.cells.iter().collect();
        s.trim_end().to_string()
    }
}

enum State {
    Ground,
    Esc,
    Csi(String),
    Osc(String),
    OscEsc(String),
}

pub struct Vt {
    cols: usize,
    rows: usize,
    grid: Vec<Row>,
    scrollback: Vec<Row>,
    row: usize,
    col: usize,
    pending_wrap: bool,
    cursor_visible: bool,
    sgr_active: bool,
    osc_log: Vec<String>,
    state: State,
}

impl Vt {
    pub fn new(cols: u16, rows: u16) -> Vt {
        let cols = usize::from(cols).max(1);
        let rows = usize::from(rows).max(1);
        Vt {
            cols,
            rows,
            grid: vec![Row::blank(cols); rows],
            scrollback: Vec::new(),
            row: 0,
            col: 0,
            pending_wrap: false,
            cursor_visible: true,
            sgr_active: false,
            osc_log: Vec::new(),
            state: State::Ground,
        }
    }

    pub fn feed(&mut self, bytes: &str) {
        for c in bytes.chars() {
            let state = std::mem::replace(&mut self.state, State::Ground);
            self.state = self.step(state, c);
        }
    }

    /// Visible rows with trailing spaces trimmed and trailing empty rows dropped.
    pub fn screen(&self) -> Vec<String> {
        trim_empty_tail(self.grid.iter().map(Row::text).collect())
    }

    pub fn scrollback_and_screen(&self) -> Vec<String> {
        trim_empty_tail(self.scrollback.iter().chain(self.grid.iter()).map(Row::text).collect())
    }

    pub fn osc_log(&self) -> &[String] {
        &self.osc_log
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn plain_text(&self) -> String {
        self.screen().join("\n")
    }

    pub fn line_has_colour(&self, row: usize) -> bool {
        self.grid.get(row).is_some_and(|r| r.colour)
    }

    fn step(&mut self, state: State, c: char) -> State {
        match state {
            State::Ground => self.ground(c),
            State::Esc => match c {
                '[' => State::Csi(String::new()),
                ']' => State::Osc(String::new()),
                _ => State::Ground,
            },
            State::Csi(mut params) => match c {
                '\u{20}'..='\u{3f}' => {
                    params.push(c);
                    State::Csi(params)
                }
                '\u{40}'..='\u{7e}' => {
                    self.csi(&params, c);
                    State::Ground
                }
                _ => State::Ground,
            },
            State::Osc(mut payload) => match c {
                '\x07' => {
                    self.osc_log.push(payload);
                    State::Ground
                }
                '\x1b' => State::OscEsc(payload),
                _ => {
                    payload.push(c);
                    State::Osc(payload)
                }
            },
            State::OscEsc(payload) => {
                if c == '\\' {
                    self.osc_log.push(payload);
                }
                State::Ground
            }
        }
    }

    fn ground(&mut self, c: char) -> State {
        match c {
            '\x1b' => return State::Esc,
            '\n' => {
                self.pending_wrap = false;
                self.line_feed();
                self.col = 0;
            }
            '\r' => {
                self.pending_wrap = false;
                self.col = 0;
            }
            '\x08' => {
                self.pending_wrap = false;
                self.col = self.col.saturating_sub(1);
            }
            '\t' => {
                self.pending_wrap = false;
                self.col = ((self.col / 8 + 1) * 8).min(self.cols - 1);
            }
            c if c.is_control() => {}
            c => self.put(c),
        }
        State::Ground
    }

    fn put(&mut self, c: char) {
        if self.pending_wrap {
            self.col = 0;
            self.line_feed();
            self.pending_wrap = false;
        }
        self.grid[self.row].cells[self.col] = c;
        if self.sgr_active {
            self.grid[self.row].colour = true;
        }
        if self.col + 1 >= self.cols {
            self.pending_wrap = true;
        } else {
            self.col += 1;
        }
    }

    fn line_feed(&mut self) {
        if self.row + 1 >= self.rows {
            let top = self.grid.remove(0);
            self.scrollback.push(top);
            self.grid.push(Row::blank(self.cols));
        } else {
            self.row += 1;
        }
    }

    fn csi(&mut self, params: &str, fin: char) {
        if let Some(modes) = params.strip_prefix('?') {
            if matches!(fin, 'h' | 'l') && modes.split(';').any(|m| m == "25") {
                self.cursor_visible = fin == 'h';
            }
            return;
        }
        let nums: Vec<usize> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
        // Count parameter: a missing or zero value means `default`.
        let arg = |i: usize, default: usize| match nums.get(i) {
            Some(&v) if v > 0 => v,
            _ => default,
        };
        let mode = nums.first().copied().unwrap_or(0);
        match fin {
            'A' => {
                self.pending_wrap = false;
                self.row = self.row.saturating_sub(arg(0, 1));
            }
            'B' => {
                self.pending_wrap = false;
                self.row = (self.row + arg(0, 1)).min(self.rows - 1);
            }
            'C' => {
                self.pending_wrap = false;
                self.col = (self.col + arg(0, 1)).min(self.cols - 1);
            }
            'D' => {
                self.pending_wrap = false;
                self.col = self.col.saturating_sub(arg(0, 1));
            }
            'G' => {
                self.pending_wrap = false;
                self.col = (arg(0, 1) - 1).min(self.cols - 1);
            }
            'H' | 'f' => {
                self.pending_wrap = false;
                self.row = (arg(0, 1) - 1).min(self.rows - 1);
                self.col = (arg(1, 1) - 1).min(self.cols - 1);
            }
            'K' => self.erase_line(mode),
            'J' => self.erase_screen(mode),
            'm' => {
                for n in &nums {
                    self.sgr_active = *n != 0;
                }
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let (from, to) = match mode {
            0 => (self.col, self.cols),
            1 => (0, self.col + 1),
            _ => (0, self.cols),
        };
        for cell in &mut self.grid[self.row].cells[from..to.min(self.cols)] {
            *cell = ' ';
        }
    }

    fn erase_screen(&mut self, mode: usize) {
        match mode {
            0 => {
                self.erase_line(0);
                for row in self.row + 1..self.rows {
                    self.grid[row] = Row::blank(self.cols);
                }
            }
            1 => {
                for row in 0..self.row {
                    self.grid[row] = Row::blank(self.cols);
                }
                self.erase_line(1);
            }
            _ => {
                for row in 0..self.rows {
                    self.grid[row] = Row::blank(self.cols);
                }
                if mode == 3 {
                    self.scrollback.clear();
                }
            }
        }
    }
}

fn trim_empty_tail(mut rows: Vec<String>) -> Vec<String> {
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_and_newlines_like_a_cooked_tty() {
        let mut vt = Vt::new(20, 5);
        vt.feed("one\ntwo\n  three");
        assert_eq!(vt.screen(), vec!["one", "two", "  three"]);
    }

    #[test]
    fn wraps_past_the_last_column() {
        let mut vt = Vt::new(5, 4);
        vt.feed("abcdefgh");
        assert_eq!(vt.screen(), vec!["abcde", "fgh"]);
    }

    #[test]
    fn exact_width_line_followed_by_newline_adds_no_blank_row() {
        let mut vt = Vt::new(5, 4);
        vt.feed("abcde\nxy");
        assert_eq!(vt.screen(), vec!["abcde", "xy"]);
    }

    #[test]
    fn scrolls_and_keeps_scrollback() {
        let mut vt = Vt::new(3, 2);
        vt.feed("a\nb\nc");
        assert_eq!(vt.screen(), vec!["b", "c"]);
        assert_eq!(vt.scrollback_and_screen(), vec!["a", "b", "c"]);
    }

    #[test]
    fn cursor_up_and_erase_redraws_in_place() {
        let mut vt = Vt::new(20, 6);
        vt.feed("keep\nlive 1\nlive 2\n");
        vt.feed("\x1b[2A\r\x1b[Jfinal\n");
        assert_eq!(vt.screen(), vec!["keep", "final"]);
    }

    #[test]
    fn erase_in_line_modes() {
        let mut vt = Vt::new(10, 2);
        vt.feed("abcdef\x1b[3D\x1b[K");
        assert_eq!(vt.screen(), vec!["abc"]);
        let mut vt = Vt::new(10, 2);
        vt.feed("abcdef\x1b[3D\x1b[1K");
        assert_eq!(vt.screen(), vec!["    ef"]);
        let mut vt = Vt::new(10, 2);
        vt.feed("abcdef\x1b[2K");
        assert!(vt.screen().is_empty());
    }

    #[test]
    fn erase_in_display_modes() {
        let mut vt = Vt::new(10, 4);
        vt.feed("aa\nbb\ncc\x1b[2A\x1b[J");
        assert_eq!(vt.screen(), vec!["aa"]);
        vt.feed("\x1b[2J");
        assert!(vt.screen().is_empty());
    }

    #[test]
    fn absolute_positioning_and_relative_moves() {
        let mut vt = Vt::new(6, 3);
        vt.feed("\x1b[2;3Hx\x1b[1Gy\x1b[2Cz\x1b[Bw");
        assert_eq!(vt.screen(), vec!["", "y xz", "    w"]);
    }

    #[test]
    fn osc_sequences_are_logged_and_swallowed() {
        let mut vt = Vt::new(40, 3);
        vt.feed("\x1b]9;4;1;40\x07\x1b]2;frust: building x\x1b\\hello");
        assert_eq!(vt.osc_log(), ["9;4;1;40", "2;frust: building x"]);
        assert_eq!(vt.screen(), vec!["hello"]);
    }

    #[test]
    fn hyperlinks_keep_their_text() {
        let mut vt = Vt::new(40, 3);
        vt.feed("a \x1b]8;;https://docs.rs/x/1\x1b\\crate\x1b]8;;\x1b\\ b");
        assert_eq!(vt.screen(), vec!["a crate b"]);
        assert_eq!(vt.osc_log(), ["8;;https://docs.rs/x/1", "8;;"]);
    }

    #[test]
    fn tracks_cursor_visibility_and_ignores_window_ops() {
        let mut vt = Vt::new(10, 2);
        vt.feed("\x1b[?25l\x1b[22;0tx");
        assert!(!vt.cursor_visible());
        vt.feed("\x1b[23;0t\x1b[?25h");
        assert!(vt.cursor_visible());
        assert_eq!(vt.screen(), vec!["x"]);
    }

    #[test]
    fn records_which_rows_carry_colour() {
        let mut vt = Vt::new(10, 3);
        vt.feed("\x1b[31mred\x1b[0m\nplain");
        assert!(vt.line_has_colour(0));
        assert!(!vt.line_has_colour(1));
        assert_eq!(vt.plain_text(), "red\nplain");
    }
}
