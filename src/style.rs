//! ANSI styling for frust's own reports; the live build display uses [`crate::ui`] instead.

#[derive(Debug, Clone, Copy)]
pub struct Styler {
    pub on: bool,
}

impl Styler {
    pub fn detect() -> Styler {
        let disabled = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
            || std::env::var("TERM").is_ok_and(|t| t == "dumb");
        let forced = std::env::var("CLICOLOR_FORCE").is_ok_and(|v| !v.is_empty() && v != "0");
        // SAFETY: isatty only inspects the descriptor.
        let tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
        Styler { on: !disabled && (forced || tty) }
    }

    pub fn plain() -> Styler {
        Styler { on: false }
    }

    fn wrap(&self, code: &str, text: &str) -> String {
        if self.on { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() }
    }

    pub fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }
    pub fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }
    pub fn green(&self, text: &str) -> String {
        self.wrap("32", text)
    }
    pub fn red(&self, text: &str) -> String {
        self.wrap("31", text)
    }
    pub fn yellow(&self, text: &str) -> String {
        self.wrap("33", text)
    }
    pub fn cyan(&self, text: &str) -> String {
        self.wrap("36", text)
    }
}

pub fn duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

pub fn row(style: &Styler, label: &str, value: &str) -> String {
    format!("  {}  {}", style.dim(&format!("{label:<18}")), value)
}
