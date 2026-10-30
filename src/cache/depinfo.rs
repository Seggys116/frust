//! Parses rustc dep-info: the first rule lists source files, `# env-dep:` lines list env vars read.

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DepInfo {
    pub files: Vec<String>,
    /// `(name, value)`; `None` means the variable was observed as unset.
    pub env: Vec<(String, Option<String>)>,
}

pub fn parse(text: &str) -> DepInfo {
    let mut info = DepInfo::default();
    let mut rule_done = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# env-dep:") {
            match rest.split_once('=') {
                Some((name, value)) => info.env.push((name.to_string(), Some(unescape_env(value)))),
                None => info.env.push((rest.to_string(), None)),
            }
            continue;
        }
        if line.is_empty() || line.starts_with('#') || rule_done {
            continue;
        }
        if let Some(deps) = split_rule(line) {
            info.files = split_escaped(deps);
            rule_done = true;
        }
    }
    info
}

fn split_rule(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b':' if i + 1 == bytes.len() || bytes[i + 1] == b' ' => {
                return Some(line[i + 1..].trim_start());
            }
            _ => i += 1,
        }
    }
    None
}

fn split_escaped(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                current.push(' ');
                chars.next();
            }
            ' ' => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn unescape_env(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_files_and_env() {
        let text = "t/debug/deps/libfoo-1.rmeta: src/lib.rs src/a\\ b.rs\n\nsrc/lib.rs:\nsrc/a b.rs:\n\n# env-dep:CARGO_PKG_NAME=foo\n# env-dep:MISSING\n# env-dep:MULTI=a\\nb\n";
        let info = parse(text);
        assert_eq!(info.files, vec!["src/lib.rs", "src/a b.rs"]);
        assert_eq!(
            info.env,
            vec![
                ("CARGO_PKG_NAME".to_string(), Some("foo".to_string())),
                ("MISSING".to_string(), None),
                ("MULTI".to_string(), Some("a\nb".to_string())),
            ]
        );
    }

    #[test]
    fn empty_rule() {
        assert_eq!(parse("out.d:\n").files, Vec::<String>::new());
    }
}
