//! ASCII progress frames. Formatting is separate from compiler workers.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressMode {
    Interactive,
    Static,
    Plain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseMark {
    Done,
    Active,
    Pending,
    Skipped,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phase {
    pub name: String,
    pub mark: PhaseMark,
    pub detail: String,
    pub completed: Option<u64>,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub subtitle: String,
}

pub fn mode_from_env(progress: Option<&str>, term: Option<&str>, stderr_is_tty: bool) -> ProgressMode {
    if term == Some("dumb") {
        return ProgressMode::Plain;
    }
    match progress {
        Some("plain" | "off") => ProgressMode::Plain,
        Some("static") => ProgressMode::Static,
        Some("interactive") => {
            if stderr_is_tty {
                ProgressMode::Interactive
            } else {
                ProgressMode::Static
            }
        }
        Some(_) | None => {
            if stderr_is_tty {
                ProgressMode::Interactive
            } else {
                ProgressMode::Plain
            }
        }
    }
}

pub fn render(header: &Header, phases: &[Phase], mode: ProgressMode, width: usize, tick: u32, color: bool) -> String {
    if mode == ProgressMode::Plain {
        return String::new();
    }
    if width < 48 {
        return narrow(phases, mode, tick, color);
    }
    let mut out = format!("fRust  {}\n", header.subtitle);
    out.push_str(&rail(phases));
    out.push('\n');
    out.push('\n');
    for phase in phases {
        out.push_str(&row(phase, mode, tick, color));
        out.push('\n');
    }
    out
}

fn narrow(phases: &[Phase], mode: ProgressMode, tick: u32, color: bool) -> String {
    let phase = phases
        .iter()
        .find(|phase| phase.mark == PhaseMark::Failed)
        .or_else(|| phases.iter().find(|phase| phase.mark == PhaseMark::Active))
        .or_else(|| phases.iter().rev().find(|phase| phase.mark == PhaseMark::Done));
    let Some(phase) = phase else {
        return String::new();
    };
    format!("fRust {} {}\n", token(phase.mark, color && phase.mark == PhaseMark::Active), detail_text(phase, mode, tick))
}

fn rail(phases: &[Phase]) -> String {
    let mut out = String::from("       ");
    for (index, phase) in phases.iter().enumerate() {
        if index > 0 {
            out.push_str("-----");
        }
        out.push(match phase.mark {
            PhaseMark::Done => 'o',
            PhaseMark::Active => '*',
            PhaseMark::Pending => '.',
            PhaseMark::Skipped => '-',
            PhaseMark::Failed => '!',
        });
    }
    out
}

fn row(phase: &Phase, mode: ProgressMode, tick: u32, color: bool) -> String {
    let active = color && phase.mark == PhaseMark::Active;
    format!(" {} {:<12} {}", token(phase.mark, active), phase.name, detail_text(phase, mode, tick))
}

fn token(mark: PhaseMark, color: bool) -> String {
    let text = match mark {
        PhaseMark::Done => "[ok]",
        PhaseMark::Active => "[>>]",
        PhaseMark::Pending => "[..]",
        PhaseMark::Skipped => "[--]",
        PhaseMark::Failed => "[!!]",
    };
    if color {
        format!("\u{1b}[38;5;172m{text}\u{1b}[0m")
    } else {
        text.to_string()
    }
}

fn detail_text(phase: &Phase, mode: ProgressMode, tick: u32) -> String {
    if phase.mark == PhaseMark::Active {
        if let (Some(done), Some(total)) = (phase.completed, phase.total) {
            if total > 0 {
                let done = done.min(total);
                return format!("{} {done}/{total} units", bar(done, total));
            }
        }
        return pulse(mode, tick);
    }
    if phase.detail.is_empty() { String::new() } else { phase.detail.clone() }
}

fn bar(done: u64, total: u64) -> String {
    const WIDTH: usize = 16;
    let filled = ((done as u128 * WIDTH as u128) / total as u128) as usize;
    let filled = filled.min(WIDTH.saturating_sub(1));
    let mut out = String::from("[");
    for index in 0..WIDTH {
        if index < filled {
            out.push('=');
        } else if index == filled {
            out.push('>');
        } else {
            out.push('.');
        }
    }
    out.push(']');
    out
}

fn pulse(mode: ProgressMode, tick: u32) -> String {
    const WIDTH: usize = 16;
    let pos = if mode == ProgressMode::Static { 0 } else { (tick as usize) % WIDTH };
    let mut out = String::from("[");
    for index in 0..WIDTH {
        out.push(if index == pos { '*' } else { '.' });
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phases() -> Vec<Phase> {
        vec![
            Phase { name: "Resolve".into(), mark: PhaseMark::Done, detail: "Dependencies ready".into(), completed: None, total: None },
            Phase {
                name: "Compile".into(),
                mark: PhaseMark::Active,
                detail: String::new(),
                completed: Some(3),
                total: Some(6),
            },
            Phase { name: "Full LTO".into(), mark: PhaseMark::Pending, detail: String::new(), completed: None, total: None },
            Phase { name: "Native link".into(), mark: PhaseMark::Pending, detail: String::new(), completed: None, total: None },
        ]
    }

    #[test]
    fn modes_follow_progress_and_term() {
        assert_eq!(mode_from_env(None, Some("dumb"), true), ProgressMode::Plain);
        assert_eq!(mode_from_env(Some("plain"), Some("xterm"), true), ProgressMode::Plain);
        assert_eq!(mode_from_env(Some("static"), Some("dumb"), true), ProgressMode::Plain);
        assert_eq!(mode_from_env(Some("static"), Some("xterm"), false), ProgressMode::Static);
        assert_eq!(mode_from_env(None, Some("xterm"), true), ProgressMode::Interactive);
        assert_eq!(mode_from_env(None, Some("xterm"), false), ProgressMode::Plain);
    }

    #[test]
    fn frames_use_marks_and_only_stable_totals() {
        let header = Header { subtitle: "release / full LTO".into() };
        let frame = render(&header, &phases(), ProgressMode::Interactive, 80, 0, false);
        assert!(frame.contains("[ok]"));
        assert!(frame.contains("[>>]"));
        assert!(frame.contains("[..]"));
        assert!(frame.contains("3/6 units"));
        assert!(frame.contains("[=====>..........]") || frame.contains('>'));
        assert!(render(&header, &phases(), ProgressMode::Plain, 80, 1, false).is_empty());

        let mut unknown = phases();
        unknown[1].total = None;
        let first = render(&header, &unknown, ProgressMode::Interactive, 80, 0, false);
        let later = render(&header, &unknown, ProgressMode::Interactive, 80, 3, false);
        assert_ne!(first, later);
        let held = render(&header, &unknown, ProgressMode::Static, 80, 0, false);
        let held_later = render(&header, &unknown, ProgressMode::Static, 80, 3, false);
        assert_eq!(held, held_later);
        assert!(held.contains("[*...............]"));

        let narrow = render(&header, &phases(), ProgressMode::Static, 20, 0, false);
        assert!(narrow.starts_with("fRust [>>]"));
        assert!(!narrow.contains("Full LTO"));
    }

    #[test]
    fn failure_and_skipped_marks_are_text() {
        let header = Header { subtitle: "check".into() };
        let phases = vec![
            Phase { name: "Compile".into(), mark: PhaseMark::Failed, detail: "rustc exited".into(), completed: None, total: None },
            Phase { name: "Native link".into(), mark: PhaseMark::Skipped, detail: "not reached".into(), completed: None, total: None },
        ];
        let frame = render(&header, &phases, ProgressMode::Static, 80, 0, true);
        assert!(frame.contains("[!!]"));
        assert!(frame.contains("[--]"));
        assert!(frame.contains("rustc exited"));
        assert!(frame.contains("\u{1b}[38;5;172m[!!]\u{1b}[0m") || frame.contains("[!!]"));
    }
}
