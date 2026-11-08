//! Root tokens: with `FRUST_CACHE_SHARE_ROOTS=on`, workspace crates in different checkouts of one repository share cache entries.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

pub const ROOT_TOKEN: &str = "{{FRUST_ROOT_DIR}}";

/// What rustc is told to write in place of the checkout root in the paths it embeds.
pub const ROOT_PLACEHOLDER: &str = "/frust-root";

/// A shorter root would match unrelated text, so such a checkout never shares.
const MIN_ROOT_LEN: usize = 4;

const SCAN_CHUNK: usize = 1 << 20;

/// Environment variables whose value is a path of this checkout or its target directory.
const PATH_ENV: &[&str] = &["CARGO_MANIFEST_DIR", "CARGO_MANIFEST_PATH", "CARGO_RUSTC_CURRENT_DIR", "OUT_DIR", "CARGO_TARGET_DIR"];

/// The checkout root and every spelling of it that can appear in compiler input or output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    primary: String,
    /// Longest first, so a verbatim spelling is replaced before the plain one it contains.
    forms: Vec<String>,
}

impl Root {
    pub fn new(root: &Path) -> Option<Root> {
        let primary = root.to_str()?.to_string();
        (primary.len() >= MIN_ROOT_LEN).then(|| Root { forms: spellings(&primary), primary })
    }

    pub fn as_str(&self) -> &str {
        &self.primary
    }

    pub fn forms(&self) -> &[String] {
        &self.forms
    }

    pub fn tokenise(&self, text: &str) -> String {
        self.forms.iter().fold(text.to_string(), |text, form| replace_bounded(&text, form, ROOT_TOKEN))
    }

    pub fn resolve(&self, text: &str) -> String {
        text.replace(ROOT_TOKEN, &self.primary)
    }

    /// Dep-info writes a space in a path as `\ `, but env-dep values are only newline-escaped.
    pub fn tokenise_dep_info(&self, text: &str) -> String {
        per_line(text, |line, env| {
            if env {
                self.tokenise(line)
            } else {
                self.forms
                    .iter()
                    .fold(line.to_string(), |line, form| replace_bounded(&line, &escape_spaces(form), ROOT_TOKEN))
            }
        })
    }

    pub fn resolve_dep_info(&self, text: &str) -> String {
        per_line(text, |line, env| {
            let root = if env { self.primary.clone() } else { escape_spaces(&self.primary) };
            line.replace(ROOT_TOKEN, &root)
        })
    }

    /// The arguments of `--remap-path-prefix` for every spelling, longest first.
    pub fn remap_args(&self) -> Vec<String> {
        self.forms.iter().map(|form| format!("--remap-path-prefix={form}={ROOT_PLACEHOLDER}")).collect()
    }
}

fn per_line(text: &str, rewrite: impl Fn(&str, bool) -> String) -> String {
    text.split_inclusive('\n').map(|line| rewrite(line, line.starts_with("# env-dep:"))).collect()
}

fn escape_spaces(path: &str) -> String {
    path.replace(' ', "\\ ")
}

/// Windows spells one directory with a verbatim prefix or without, and with either separator.
fn spellings(primary: &str) -> Vec<String> {
    const VERBATIM: &str = r"\\?\";
    let mut forms = vec![primary.to_string()];
    let drive = |text: &str| {
        let bytes = text.as_bytes();
        bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/')
    };
    if let Some(plain) = primary.strip_prefix(VERBATIM).filter(|plain| drive(plain)) {
        forms.push(plain.to_string());
    } else if drive(primary) {
        forms.push(format!("{VERBATIM}{primary}"));
    }
    for slashed in forms.iter().map(|form| form.replace('\\', "/")).collect::<Vec<_>>() {
        if !forms.contains(&slashed) {
            forms.push(slashed);
        }
    }
    forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
    forms
}

fn name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// A path ends at a separator, the end of the text, a quote or a list separator; an escaped space (`\ `) continues it.
fn path_ends(rest: &str) -> bool {
    let mut chars = rest.chars();
    match chars.next() {
        None => true,
        Some('\\') => chars.next() != Some(' '),
        Some(c) => matches!(c, '/' | '\n' | '\r' | '"' | '\'' | ':' | ';' | ','),
    }
}

/// Replaces `needle` only where it is a whole leading path, so a sibling directory sharing the prefix is left alone.
fn replace_bounded(text: &str, needle: &str, token: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, _) in text.match_indices(needle) {
        let end = start + needle.len();
        let before = text[..start].chars().next_back().is_none_or(|c| !name_char(c));
        let after = path_ends(&text[end..]);
        if start >= last && before && after {
            out.push_str(&text[last..start]);
            out.push_str(token);
            last = end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// The `--crate-type` values on a command line, comma lists split.
pub fn crate_types(args: &[OsString]) -> Vec<String> {
    let mut kinds = Vec::new();
    let mut args = args.iter().map(|arg| arg.to_string_lossy());
    while let Some(arg) = args.next() {
        let value = if arg == "--crate-type" {
            args.next().map(|value| value.into_owned())
        } else {
            arg.strip_prefix("--crate-type=").map(str::to_string)
        };
        kinds.extend(value.iter().flat_map(|value| value.split(',')).map(str::to_string));
    }
    kinds
}

/// Command-line facts that decide before compiling whether a unit may ever share entries.
pub fn flags_eligible(args: &[OsString], crate_name: &str, test_harness: bool) -> bool {
    let kinds = crate_types(args);
    let plain_kinds = !kinds.is_empty() && kinds.iter().all(|kind| matches!(kind.as_str(), "lib" | "rlib" | "bin"));
    let text = |arg: &OsString| arg.to_string_lossy().into_owned();
    // A user-supplied remap would fight the one added here; rpath bakes absolute directories into binaries.
    let barred = args.iter().map(text).any(|arg| arg.contains("remap-path-prefix") || arg.contains("rpath"));
    plain_kinds && !barred && !test_harness && !crate_name.starts_with("build_script_")
}

/// False when a compile read a variable that holds a checkout path, or any variable whose value mentions one of `paths`.
pub fn env_clean(env: &[(String, Option<String>)], paths: &[&str]) -> bool {
    env.iter().all(|(name, value)| {
        !PATH_ENV.contains(&name.as_str()) && value.as_ref().is_none_or(|value| !paths.iter().any(|path| value.contains(path)))
    })
}

/// Whether any file contains one of `needles`; on Windows and macOS, whose volumes ignore case, the match ignores case and separators too.
pub fn embeds_any(files: &[std::path::PathBuf], needles: &[&str]) -> io::Result<bool> {
    embeds_any_with(files, needles, cfg!(any(windows, target_os = "macos")))
}

pub fn embeds_any_with(files: &[std::path::PathBuf], needles: &[&str], fold: bool) -> io::Result<bool> {
    for file in files {
        if file_contains(file, needles, fold)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// ASCII case folded, with `\` read as `/`, so every spelling of a root compares equal.
fn fold_byte(byte: u8) -> u8 {
    if byte == b'\\' { b'/' } else { byte.to_ascii_lowercase() }
}

fn file_contains(path: &Path, needles: &[&str], fold: bool) -> io::Result<bool> {
    let needles: Vec<Vec<u8>> =
        needles.iter().map(|needle| needle.bytes().map(|byte| if fold { fold_byte(byte) } else { byte }).collect()).collect();
    let longest = needles.iter().map(Vec::len).max().unwrap_or(0);
    if longest == 0 {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; SCAN_CHUNK + longest];
    let mut kept = 0;
    loop {
        let read = file.read(&mut buf[kept..kept + SCAN_CHUNK])?;
        if read == 0 {
            return Ok(false);
        }
        let end = kept + read;
        if fold {
            buf[kept..end].iter_mut().for_each(|byte| *byte = fold_byte(*byte));
        }
        if needles.iter().any(|needle| contains(&buf[..end], needle)) {
            return Ok(true);
        }
        // A needle can straddle two reads, so the tail of this one leads the next.
        kept = (longest - 1).min(end);
        buf.copy_within(end - kept..end, 0);
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some(&first) = needle.first() else { return true };
    let mut from = 0;
    while let Some(offset) = haystack[from..].iter().position(|&byte| byte == first) {
        let at = from + offset;
        if haystack[at..].starts_with(needle) {
            return true;
        }
        from = at + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(text: &str) -> Root {
        Root::new(Path::new(text)).unwrap()
    }

    fn os(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_root_round_trips_through_its_token_in_paths_and_args() {
        let root = root("/work/tree one");
        for text in [
            "/work/tree one/src/lib.rs",
            "--remap=/work/tree one",
            "-Ldependency=/work/tree one/target/debug/deps",
            "/work/tree one",
        ] {
            let tokenised = root.tokenise(text);
            assert!(tokenised.contains(ROOT_TOKEN), "{text}");
            assert_eq!(root.resolve(&tokenised), text);
        }
        assert_eq!(root.tokenise("no paths here"), "no paths here");
    }

    #[test]
    fn only_whole_leading_paths_are_tokenised() {
        let root = root("/tmp/a-root");
        assert_eq!(root.tokenise("/tmp/a-root-2/x"), "/tmp/a-root-2/x");
        assert_eq!(root.tokenise("/other/tmp/a-root/x"), "/other/tmp/a-root/x");
        assert_eq!(root.tokenise("/tmp/a-root.bak"), "/tmp/a-root.bak");
        assert_eq!(root.tokenise("x=/tmp/a-root/y:/tmp/a-root"), format!("x={ROOT_TOKEN}/y:{ROOT_TOKEN}"));
    }

    #[test]
    fn a_sibling_directory_is_not_matched_as_the_root_followed_by_more_text() {
        let root = root("/work/repo");
        for sibling in ["/work/repo 2/x", "/work/repo+x/y", "/work/repo@y", "/work/repo2", "/work/repo~1/z"] {
            assert_eq!(root.tokenise(sibling), sibling);
        }
        let spaced = root.tokenise_dep_info("/t/k.d: /work/repo\\ 2/src/lib.rs\n/work/repo/src/lib.rs:\n");
        assert_eq!(spaced, format!("/t/k.d: /work/repo\\ 2/src/lib.rs\n{ROOT_TOKEN}/src/lib.rs:\n"));
        assert_eq!(root.tokenise("A=\"/work/repo\" B=/work/repo:/usr"), format!("A=\"{ROOT_TOKEN}\" B={ROOT_TOKEN}:/usr"));
    }

    #[test]
    fn two_checkouts_tokenise_equal_texts_to_the_same_form() {
        let (a, b) = (root("/home/u/repo-a"), root("/mnt/ci/repo-b"));
        let text = |root: &str| format!("{root}/crates/x/src/lib.rs: {root}/crates/x/Cargo.toml");
        assert_eq!(a.tokenise(&text("/home/u/repo-a")), b.tokenise(&text("/mnt/ci/repo-b")));
    }

    #[test]
    fn windows_roots_match_with_either_separator_and_with_or_without_the_verbatim_prefix() {
        let root = root(r"C:\work\repo");
        assert_eq!(root.forms().len(), 4);
        for text in [r"C:\work\repo\src\lib.rs", r"\\?\C:\work\repo\src\lib.rs", "C:/work/repo/src/lib.rs"] {
            let tokenised = root.tokenise(text);
            assert!(tokenised.starts_with(ROOT_TOKEN), "{text} -> {tokenised}");
            assert!(!tokenised.contains("work"), "{text} -> {tokenised}");
        }
        assert_eq!(root.resolve(&root.tokenise(r"C:\work\repo\src")), r"C:\work\repo\src");

        let verbatim = self::root(r"\\?\C:\work\repo");
        assert_eq!(verbatim.tokenise(r"C:\work\repo\x"), format!(r"{ROOT_TOKEN}\x"));
        assert_eq!(verbatim.tokenise(r"\\?\C:\work\repo\x"), format!(r"{ROOT_TOKEN}\x"));
    }

    #[test]
    fn short_or_non_utf8_roots_are_refused() {
        assert!(Root::new(Path::new("/")).is_none());
        assert!(Root::new(Path::new("/a")).is_none());
        assert!(Root::new(Path::new("/abc")).is_some());
    }

    #[test]
    fn dep_info_lines_use_their_own_space_spelling() {
        let root = root("/work/my repo");
        let raw = "/t/deps/k.d: /work/my\\ repo/src/lib.rs\n\n/work/my\\ repo/src/lib.rs:\n# env-dep:DIR=/work/my repo/x\n";
        let tokenised = root.tokenise_dep_info(raw);
        assert!(!tokenised.contains("work"), "{tokenised}");
        assert_eq!(tokenised.matches(ROOT_TOKEN).count(), 3);
        assert_eq!(root.resolve_dep_info(&tokenised), raw);
    }

    #[test]
    fn remap_arguments_name_every_spelling() {
        let root = root(r"C:\work\repo");
        let args = root.remap_args();
        assert_eq!(args.len(), root.forms().len());
        assert!(args.iter().all(|arg| arg.ends_with(&format!("={ROOT_PLACEHOLDER}"))));
        assert!(args.contains(&format!(r"--remap-path-prefix=C:\work\repo={ROOT_PLACEHOLDER}")));
    }

    fn with_incremental(base: &[&str]) -> Vec<OsString> {
        os(&[base, &["-C", "incremental=/t/debug/incremental/k-1"]].concat())
    }

    #[test]
    fn only_library_and_binary_units_without_special_flags_are_eligible() {
        let base = ["--crate-name", "k", "--crate-type", "lib", "-C", "debuginfo=2"];
        assert!(flags_eligible(&os(&base), "k", false));
        assert!(flags_eligible(&os(&["--crate-type=bin"]), "k", false));
        assert!(flags_eligible(&os(&["--crate-type", "lib,rlib"]), "k", false));
        assert!(flags_eligible(&with_incremental(&base), "k", false), "incremental units qualify");
        assert!(!flags_eligible(&os(&["--crate-type", "proc-macro"]), "k", false));
        assert!(!flags_eligible(&os(&["--crate-type", "cdylib"]), "k", false));
        assert!(!flags_eligible(&os(&["--crate-type", "lib,dylib"]), "k", false));
        assert!(!flags_eligible(&os(&["--crate-name", "k"]), "k", false));
        assert!(!flags_eligible(&os(&base), "build_script_build", false));
        assert!(!flags_eligible(&os(&base), "k", true));
        let with = |extra: &[&str]| os(&[&base[..], extra].concat());
        assert!(!flags_eligible(&with(&["-C", "rpath"]), "k", false));
        assert!(!flags_eligible(&with(&["-Crpath=yes"]), "k", false));
        assert!(!flags_eligible(&with(&["--remap-path-prefix=/a=/b"]), "k", false));
    }

    #[test]
    fn path_variables_and_values_that_mention_the_root_make_a_unit_ineligible() {
        let var = |name: &str, value: Option<&str>| vec![(name.to_string(), value.map(str::to_string))];
        let paths = ["/work/repo", "/work/target/debug"];
        assert!(env_clean(&var("CARGO_PKG_NAME", Some("k")), &paths));
        assert!(env_clean(&var("SOMETHING", None), &paths));
        for name in ["CARGO_MANIFEST_DIR", "CARGO_RUSTC_CURRENT_DIR", "OUT_DIR", "CARGO_TARGET_DIR"] {
            assert!(!env_clean(&var(name, Some("elsewhere")), &paths), "{name}");
        }
        assert!(!env_clean(&var("GEN", Some("/work/repo/gen")), &paths));
        assert!(!env_clean(&var("GEN", Some("x:/work/target/debug/y")), &paths));
    }

    #[test]
    fn a_folded_scan_finds_other_spellings_of_a_root_and_an_exact_scan_does_not() {
        let dir = std::env::temp_dir().join(format!("frust-share-fold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let spelled = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("xx{text}yy")).unwrap();
            path
        };
        let needles = [r"C:\Work\Repo", "//?/c:/work/repo"];
        for text in [r"c:\work\repo", "C:/Work/Repo", r"C:\WORK/repo", r"\\?\C:\Work\Repo", "c:/WORK/REPO"] {
            let file = spelled("embedded", text);
            assert!(embeds_any_with(std::slice::from_ref(&file), &needles, true).unwrap(), "{text}");
        }
        let exact = spelled("exact", r"c:\work\repo");
        assert!(!embeds_any_with(std::slice::from_ref(&exact), &needles, false).unwrap());
        let other = spelled("other", r"C:\work\repo2-not-it");
        assert!(embeds_any_with(std::slice::from_ref(&other), &needles, true).unwrap(), "a bare prefix still counts: the scan errs towards not sharing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_embedding_the_path_is_found_even_across_read_boundaries() {
        let dir = std::env::temp_dir().join(format!("frust-share-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let needle = "/work/tree-one";
        let clean = dir.join("clean");
        std::fs::write(&clean, vec![b'x'; SCAN_CHUNK * 2 + 17]).unwrap();
        for offset in [0, 5, SCAN_CHUNK - 3, SCAN_CHUNK, SCAN_CHUNK + 40, SCAN_CHUNK * 2 + 3] {
            let mut bytes = vec![b'x'; SCAN_CHUNK * 2 + 17];
            bytes[offset..offset + needle.len()].copy_from_slice(needle.as_bytes());
            let dirty = dir.join("dirty");
            std::fs::write(&dirty, bytes).unwrap();
            assert!(embeds_any(&[clean.clone(), dirty.clone()], &["/other", needle]).unwrap(), "offset {offset}");
            assert!(!embeds_any(std::slice::from_ref(&dirty), &["/other/path"]).unwrap());
        }
        assert!(!embeds_any(&[clean], &[needle]).unwrap());
        assert!(embeds_any(&[dir.join("missing")], &[needle]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
