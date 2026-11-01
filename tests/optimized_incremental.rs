//! Optimized incremental cache hits preserve artifacts while rustc retains exclusive ownership of its native sessions.

#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{Sandbox, assert_success, run_binary, settle};

const MAIN: &str = "fn main() { println!(\"{}\", leaf::value()); }\n";
#[derive(Clone, Debug, PartialEq, Eq)]
enum Contents {
    File(Vec<u8>),
    Link(PathBuf),
}
type Files = BTreeMap<PathBuf, (Contents, u32)>;

fn canonical_object_name(name: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(name).ok()?;
    let parts: Vec<_> = text.split('.').collect();
    if parts.len() != 5 || parts[3..] != ["rcgu", "o"] {
        return None;
    }
    let hash = parts[0].strip_prefix("leaf-")?;
    if hash.len() != 16
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let base36 = |text: &str| {
        text.bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
    };
    if parts[1].is_empty()
        || parts[1].len() > 25
        || !base36(parts[1])
        || parts[2].len() != 7
        || !base36(parts[2])
    {
        return None;
    }
    let mut result = name.to_vec();
    let start = parts[0].len() + 1 + parts[1].len() + 1;
    result[start..start + 7].fill(b'0');
    Some(result)
}

// Native incremental archives encode one random session suffix in the object name and its linker-metadata reference.
fn canonical_archive(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    if !bytes.starts_with(b"!<arch>\n") {
        return Err("unsupported archive magic");
    }
    let mut normalized = bytes.to_vec();
    let mut names = None;
    let mut objects = BTreeMap::new();
    let mut linker_metadata = None;
    let mut at = 8usize;
    while at < bytes.len() {
        let begin = at.checked_add(60).ok_or("header offset overflow")?;
        let header = bytes.get(at..begin).ok_or("truncated archive header")?;
        if &header[58..] != b"`\n" {
            return Err("bad archive header terminator");
        }
        let number = |field: &[u8]| -> Result<usize, &'static str> {
            let text = std::str::from_utf8(field)
                .map_err(|_| "non-ASCII archive size")?
                .trim();
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err("invalid archive size");
            }
            text.parse().map_err(|_| "archive size overflow")
        };
        let size = number(&header[48..58])?;
        let end = begin.checked_add(size).ok_or("member offset overflow")?;
        let data = bytes.get(begin..end).ok_or("truncated archive member")?;
        let field = std::str::from_utf8(&header[..16])
            .map_err(|_| "non-ASCII member header")?
            .trim_end();
        let (name, name_at, payload) = if field == "//" {
            if names.replace((begin, end)).is_some() {
                return Err("duplicate GNU name table");
            }
            (b"//".as_slice(), at, begin..end)
        } else if let Some(length) = field.strip_prefix("#1/") {
            let length = number(length.as_bytes())?;
            let raw_name = data.get(..length).ok_or("truncated BSD name")?;
            let used = raw_name
                .iter()
                .rposition(|byte| *byte != 0)
                .map_or(0, |index| index + 1);
            (&raw_name[..used], begin, begin + length..end)
        } else if let Some(offset) = field
            .strip_prefix('/')
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        {
            let (start, stop) = names.ok_or("missing GNU name table")?;
            let offset = number(offset.as_bytes())?;
            let start = start.checked_add(offset).ok_or("name offset overflow")?;
            let table = bytes.get(start..stop).ok_or("name offset out of bounds")?;
            if offset != 0 && bytes[start - 1] != b'\n' {
                return Err("name offset is not an entry boundary");
            }
            let length = table
                .windows(2)
                .position(|pair| pair == b"/\n")
                .ok_or("unterminated GNU name")?;
            (&table[..length], start, begin..end)
        } else {
            (field.trim_end_matches('/').as_bytes(), at, begin..end)
        };
        if name.ends_with(b".rcgu.o") {
            let canonical = canonical_object_name(name).ok_or("unrecognized rcgu object name")?;
            if objects.values().any(|previous| previous == &canonical) || objects.contains_key(name)
            {
                return Err("ambiguous object names");
            }
            normalized[name_at..name_at + name.len()].copy_from_slice(&canonical);
            objects.insert(name.to_vec(), canonical);
        } else if name == b"lib.rmeta-link" && linker_metadata.replace(payload).is_some() {
            return Err("duplicate linker metadata");
        }
        at = end.checked_add(size % 2).ok_or("padding overflow")?;
        if at > bytes.len() {
            return Err("missing archive padding");
        }
    }
    if objects.is_empty() {
        return Err("no recognized native object members");
    }
    if let Some(range) = linker_metadata {
        let original = &bytes[range.clone()];
        for (name, canonical) in objects {
            let matches: Vec<_> = original
                .windows(name.len())
                .enumerate()
                .filter_map(|(index, window)| (window == name.as_slice()).then_some(index))
                .collect();
            if matches.len() != 1 {
                return Err("object name must have exactly one linker-metadata reference");
            }
            let start = range.start + matches[0];
            normalized[start..start + name.len()].copy_from_slice(&canonical);
        }
    }
    Ok(normalized)
}

fn assert_files(expected: &Files, actual: &Files, native: bool, phase: &str) {
    let expected_paths: Vec<_> = expected.keys().collect();
    let actual_paths: Vec<_> = actual.keys().collect();
    assert_eq!(
        expected_paths, actual_paths,
        "{phase}: artifact paths differ"
    );
    for (path, (contents, mode)) in expected {
        let (other_contents, other_mode) = &actual[path];
        assert_eq!(mode, other_mode, "{phase}: {} mode differs", path.display());
        let (Contents::File(bytes), Contents::File(other)) = (contents, other_contents) else {
            assert_eq!(contents, other_contents, "{phase}: {} alias differs", path.display());
            continue;
        };
        if bytes == other {
            continue;
        }
        if native
            && path
                .extension()
                .is_some_and(|extension| extension == "rlib")
        {
            let left = canonical_archive(bytes)
                .unwrap_or_else(|error| panic!("{phase}: {}: {error}", path.display()));
            let right = canonical_archive(other)
                .unwrap_or_else(|error| panic!("{phase}: {}: {error}", path.display()));
            assert!(
                left == right,
                "{phase}: {} archive differs beyond mapped member-name suffixes",
                path.display()
            );
        } else {
            panic!(
                "{phase}: {} differs: lengths {}/{}, BLAKE3 {}/{}",
                path.display(),
                bytes.len(),
                other.len(),
                blake3::hash(bytes),
                blake3::hash(other)
            );
        }
    }
}

struct Fixture {
    sb: Sandbox,
    project: PathBuf,
    target: PathBuf,
    compiler: PathBuf,
    builds: usize,
}

impl Fixture {
    fn new(tag: &str, lto: &str) -> Self {
        let sb = Sandbox::new(tag);
        let project = sb.project("app", &[], MAIN);
        let leaf = project.join("leaf");
        fs::create_dir_all(leaf.join("src")).unwrap();
        fs::write(
            leaf.join("Cargo.toml"),
            "[package]\nname = \"leaf\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let mut manifest = fs::read_to_string(project.join("Cargo.toml")).unwrap();
        manifest.push_str(&format!("leaf = {{ path = \"leaf\" }}\n\n[profile.release]\nopt-level = 3\nlto = {lto}\ncodegen-units = 2\nincremental = true\ndebug = 0\n"));
        fs::write(project.join("Cargo.toml"), manifest).unwrap();
        let compiler = sb.root.join("audited-rustc");
        // Each actual compile writes one complete argv record; compiler queries remain visible to Cargo but uncounted.
        fs::write(
            &compiler,
            r#"#!/bin/sh
compile=no
query=no
for arg in "$@"; do
    case "$arg" in --emit|--emit=*) compile=yes ;; --print|--print=*) query=yes ;; esac
done
if [ "$compile" = yes ] && [ "$query" = no ]; then
    { pwd; printf '%s\n' "$@"; } > "$FRUST_TEST_AUDIT/$$" || exit 125
fi
exec rustc "$@"
"#,
        )
        .unwrap();
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
        let target = sb.root.join("target");
        Self {
            sb,
            project,
            target,
            compiler,
            builds: 0,
        }
    }

    fn edit(&self, value: u32) {
        let source = self.project.join("leaf/src/lib.rs");
        fs::write(
            &source,
            format!("#[inline(never)]\npub fn value() -> u32 {{ {value} }}\n"),
        )
        .unwrap();
        settle(&source);
    }

    fn build(&mut self, cached: bool, extra: &[(&str, &str)]) -> Vec<String> {
        let audit = self.sb.root.join(format!("audit-{}", self.builds));
        self.builds += 1;
        fs::create_dir(&audit).unwrap();
        let mut command = if cached {
            self.sb.bin("fcargo")
        } else {
            self.sb.cargo()
        };
        command
            .args(["build", "--release", "--offline", "--message-format=json"])
            .current_dir(&self.project)
            .env("CARGO_TARGET_DIR", &self.target)
            .env("RUSTC", &self.compiler)
            .env("FRUST_TEST_AUDIT", &audit)
            .env("FRUST_AUTO_COMPACT", "off")
            .env("FRUST_TUNE", "off")
            .env("FRUST_LINK", "off");
        for &(key, value) in extra {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        assert_success(&output);
        let mut calls: Vec<String> = fs::read_dir(audit)
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        calls.sort();
        calls
    }

    fn clean(&self) {
        assert_success(
            &self
                .sb
                .cargo()
                .arg("clean")
                .current_dir(&self.project)
                .env("CARGO_TARGET_DIR", &self.target)
                .output()
                .unwrap(),
        );
    }

    fn run(&self, value: u32) {
        let output = run_binary(&self.target, "release", "app");
        assert_success(&output);
        assert_eq!(output.stdout, format!("{value}\n").as_bytes());
    }

    fn park(&self, name: &str) {
        frust::sys::rename(&self.target, &self.sb.root.join(name)).unwrap();
    }

    fn resume(&self, name: &str) {
        frust::sys::rename(&self.sb.root.join(name), &self.target).unwrap();
    }
}

// Compare complete artifact bytes and modes, including any recursively emitted debug companions.
fn files(root: &Path) -> Files {
    fn visit(root: &Path, dir: &Path, found: &mut Files) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                "incremental" | ".fingerprint" | "build" | ".cargo-lock"
            ) {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                let resolved = path.parent().unwrap().join(&target).canonicalize().unwrap();
                // Cargo publishes the packed-debug bundle through a top-level alias into deps.
                assert!(
                    path.parent() == Some(root)
                        && path.extension().is_some_and(|ext| ext == "dSYM")
                        && resolved.parent() == Some(root.join("deps").canonicalize().unwrap().as_path())
                        && resolved.extension().is_some_and(|ext| ext == "dSYM")
                        && resolved.is_dir(),
                    "unexpected output symlink: {} -> {}",
                    path.display(), target.display()
                );
                found.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    (Contents::Link(target), meta.permissions().mode() & 0o777),
                );
            } else if meta.is_dir() {
                visit(root, &path, found);
            } else {
                assert!(meta.is_file());
                found.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    (Contents::File(fs::read(&path).unwrap()), meta.permissions().mode() & 0o777),
                );
            }
        }
    }
    let mut found = BTreeMap::new();
    visit(root, root, &mut found);
    found
}

fn write_inventory(path: &Path, files: &Files) {
    let rows: Vec<_> = files.iter().map(|(path, (contents, mode))| {
        let mut row = serde_json::json!({"path": path, "mode": mode});
        match contents {
            Contents::File(bytes) => {
                row["length"] = serde_json::json!(bytes.len());
                row["blake3"] = serde_json::json!(blake3::hash(bytes).to_hex().as_str());
            }
            Contents::Link(target) => row["link"] = serde_json::json!(target),
        }
        row
    }).collect();
    fs::write(path, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
}

#[test]
fn packed_debug_inventory_preserves_alias_targets_and_real_bundle_contents() {
    let sb = Sandbox::new("packed-debug-inventory");
    let root = sb.root.join("release");
    for name in ["app-first.dSYM", "app-second.dSYM"] {
        let bundle = root.join("deps").join(name);
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        fs::write(bundle.join("Contents/debug"), b"complete debug bytes").unwrap();
    }
    let alias = root.join("app.dSYM");
    std::os::unix::fs::symlink("deps/app-first.dSYM", &alias).unwrap();
    let first = files(&root);
    assert_eq!(first.len(), 3);
    assert_eq!(first[Path::new("app.dSYM")].0, Contents::Link("deps/app-first.dSYM".into()));
    assert!(matches!(&first[Path::new("deps/app-first.dSYM/Contents/debug")].0,
        Contents::File(bytes) if bytes == b"complete debug bytes"));
    frust::sys::rename(&alias, &sb.root.join("first-alias")).unwrap();
    std::os::unix::fs::symlink("deps/app-second.dSYM", &alias).unwrap();
    let second = files(&root);
    assert_ne!(first, second, "equal bundle bytes cannot hide a changed Cargo alias");
    fs::write(root.join("deps/app-first.dSYM/Contents/debug"), b"changed debug bytes").unwrap();
    assert_ne!(second, files(&root), "the alias cannot hide changes in either real bundle");
    write_inventory(&sb.root.join("inventory.json"), &first);
    let inventory: serde_json::Value = serde_json::from_slice(&fs::read(sb.root.join("inventory.json")).unwrap()).unwrap();
    assert_eq!(inventory.as_array().unwrap().len(), 3);
}

// The cached A restoration may not rewrite any file in the retained B session.
fn sessions(target: &Path) -> Files {
    let root = target.join("release/incremental");
    let mut found = BTreeMap::new();
    for krate in fs::read_dir(&root).unwrap() {
        let krate = krate.unwrap();
        if !krate.file_name().to_string_lossy().starts_with("leaf-") {
            continue;
        }
        for session in fs::read_dir(krate.path()).unwrap() {
            let session = session.unwrap();
            let name = session.file_name();
            let name = name.to_string_lossy();
            if session.file_type().unwrap().is_dir()
                && name.starts_with("s-")
                && !name.ends_with("-working")
            {
                for (path, value) in files(&session.path()) {
                    found.insert(
                        session.path().strip_prefix(&root).unwrap().join(path),
                        value,
                    );
                }
            }
        }
    }
    assert!(
        !found.is_empty(),
        "the actual leaf compile must produce finalized native incremental state"
    );
    found
}

// Native failures or ignored incremental settings fail the test instead of qualifying as unsupported passes.
fn incremental_calls(calls: &[String]) {
    assert_eq!(
        calls.len(),
        2,
        "expected one native app and one native leaf compile: {calls:#?}"
    );
    for call in calls {
        assert!(
            call.lines().any(|arg| arg.starts_with("incremental=")),
            "{call}"
        );
        assert!(call.lines().any(|arg| arg == "opt-level=3"), "{call}");
    }
}

fn history(lto: &str, tag: &str) {
    let mut fixture = Fixture::new(tag, lto);
    let mut producer_a = None;
    let mut native_a = None;
    let mut native_a_calls = None;
    // Both histories use the same source and target paths; only the parked native target state differs.
    for (step, value) in [1, 1, 2, 1, 3].into_iter().enumerate() {
        fixture.edit(value);
        if step > 0 {
            fixture.resume("cached-target");
        }
        let before_events = fixture.sb.cache_events().len();
        let before_sessions = (step == 3).then(|| sessions(&fixture.target));
        if step == 1 {
            fixture.clean();
        }
        let cached_calls = fixture.build(true, &[]);
        fixture.run(value);
        let cached_outputs = files(&fixture.target.join("release"));
        assert!(
            cached_outputs
                .keys()
                .any(|path| path.extension().is_some_and(|ext| ext == "rlib"))
        );
        assert!(
            cached_outputs
                .keys()
                .any(|path| path.extension().is_some_and(|ext| ext == "rmeta"))
        );
        let events = fixture.sb.cache_events();
        let events = &events[before_events..];
        // The first repeated A follows a clean; the second follows a genuine native B compilation.
        if step == 1 || step == 3 {
            // Restores must reproduce the producer's entire archive bytes, including its original session-name suffixes.
            assert_files(
                producer_a.as_ref().unwrap(),
                &cached_outputs,
                false,
                "raw cached A restoration",
            );
            assert!(
                cached_calls.is_empty(),
                "restored artifacts must not invoke either native compiler: {cached_calls:#?}"
            );
            for krate in ["leaf", "app"] {
                assert!(
                    events.iter().any(|event| event["crate"] == krate
                        && event["result"] == "hit"
                        && event["bytes"].as_u64().is_some_and(|bytes| bytes > 0)),
                    "{events:#?}"
                );
            }
            assert!(!events.iter().any(|event| event["result"] == "miss"));
        } else {
            incremental_calls(&cached_calls);
            assert!(
                events
                    .iter()
                    .any(|event| event["crate"] == "leaf" && event["result"] == "miss")
            );
        }
        if step == 0 {
            producer_a = Some(cached_outputs.clone());
        }
        if let Some(before) = before_sessions {
            assert_files(
                &before,
                &sessions(&fixture.target),
                false,
                "retained B native sessions after restoring A",
            );
        }
        fixture.park("cached-target");
        if step > 0 {
            fixture.resume("native-target");
        }
        if step == 1 {
            fixture.clean();
        }
        let native_calls = fixture.build(false, &[]);
        incremental_calls(&native_calls);
        if step != 1 && step != 3 {
            assert_eq!(
                cached_calls, native_calls,
                "compiler argv and cwd must match the native history"
            );
        }
        fixture.run(value);
        let native_outputs = files(&fixture.target.join("release"));
        assert_files(
            &cached_outputs,
            &native_outputs,
            true,
            &format!("native history step {step}"),
        );
        if step == 0 {
            native_a = Some(native_outputs);
            native_a_calls = Some(native_calls);
        } else if step == 1 {
            // Two fresh native A compilations exercise the same narrow archive comparator without any cache restoration.
            assert_eq!(native_a_calls.as_ref().unwrap(), &native_calls);
            assert_files(
                native_a.as_ref().unwrap(),
                &native_outputs,
                true,
                "native-versus-native clean A control",
            );
            let changed: Vec<_> = native_a
                .as_ref()
                .unwrap()
                .iter()
                .filter_map(|(path, (contents, _))| {
                    let (Contents::File(bytes), Contents::File(other)) = (contents, &native_outputs[path].0) else {
                        return None;
                    };
                    (bytes != other).then(|| (path, blake3::hash(bytes), blake3::hash(other)))
                })
                .collect();
            eprintln!(
                "{tag}: native-versus-native changes confined to mapped archive names: {changed:?}"
            );
        }
        fixture.park("native-target");
    }
    fixture.resume("cached-target");
    assert!(
        fixture.build(true, &[]).is_empty(),
        "settled build must be a real Cargo no-op"
    );
}

#[test]
fn archive_comparison_only_maps_correlated_native_object_name_suffixes() {
    fn archive(nonce: &str, object: &[u8], metadata: &[u8], bsd: bool) -> Vec<u8> {
        fn member(archive: &mut Vec<u8>, name: &str, data: &[u8]) {
            let header = format!(
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                0,
                644,
                data.len()
            );
            assert_eq!(header.len(), 60);
            archive.extend_from_slice(header.as_bytes());
            archive.extend_from_slice(data);
            if !data.len().is_multiple_of(2) {
                archive.push(b'\n');
            }
        }
        let name = format!("leaf-0123456789abcdef.3r1d9dmzikip0xvs8ofscqxpu.{nonce}.rcgu.o");
        let mut result = b"!<arch>\n".to_vec();
        if !bsd {
            member(&mut result, "//", format!("{name}/\n").as_bytes());
        }
        member(&mut result, "lib.rmeta/", metadata);
        member(
            &mut result,
            "lib.rmeta-link/",
            format!("prefix\0{name}\0suffix").as_bytes(),
        );
        if bsd {
            let mut data = name.as_bytes().to_vec();
            data.extend_from_slice(object);
            member(&mut result, &format!("#1/{}", name.len()), &data);
        } else {
            member(&mut result, "/0", object);
        }
        result
    }
    for bsd in [false, true] {
        let first = archive("1234567", b"object", b"metadata", bsd);
        let second = archive("abcdefg", b"object", b"metadata", bsd);
        assert_eq!(
            first.iter().zip(&second).filter(|(a, b)| a != b).count(),
            14
        );
        assert_eq!(
            canonical_archive(&first).unwrap(),
            canonical_archive(&second).unwrap()
        );
        for changed in [
            archive("abcdefg", b"OBJECT", b"metadata", bsd),
            archive("abcdefg", b"object", b"METADATA", bsd),
        ] {
            assert_ne!(
                canonical_archive(&first).unwrap(),
                canonical_archive(&changed).unwrap()
            );
        }
        assert!(canonical_archive(&archive("ABCDEF!", b"object", b"metadata", bsd)).is_err());
        assert!(canonical_archive(&first[..first.len() - 1]).is_err());
        let mut unrelated = second.clone();
        let prefix = unrelated
            .windows(6)
            .position(|window| window == b"prefix")
            .unwrap();
        unrelated[prefix] = b'P';
        assert_ne!(
            canonical_archive(&first).unwrap(),
            canonical_archive(&unrelated).unwrap()
        );
        let mut uncorrelated = second;
        let token = uncorrelated
            .windows(7)
            .position(|window| window == b"abcdefg")
            .unwrap();
        uncorrelated[token] = b'h';
        assert!(canonical_archive(&uncorrelated).is_err());
    }
}

#[test]
fn release_incremental_restore_and_retained_session_edits_match_native() {
    history("false", "optimized-inc-release");
}

#[test]
fn thin_incremental_restore_and_retained_session_edits_match_native() {
    history("\"thin\"", "optimized-inc-thin");
}

#[test]
fn fat_incremental_restore_and_retained_session_edits_match_native() {
    history("\"fat\"", "optimized-inc-fat");
}

#[test]
fn optimized_incremental_optouts_remain_effective() {
    for option in [
        "FRUST_CACHE_INCREMENTAL",
        "FRUST_CACHE_PRIMARY_INCREMENTAL",
        "FRUST_CACHE_PRIMARY",
    ] {
        let mut fixture = Fixture::new(&format!("optimized-inc-{option}"), "false");
        fixture.edit(7);
        incremental_calls(&fixture.build(true, &[(option, "off")]));
        fixture.clean();
        let calls = fixture.build(true, &[(option, "off")]);
        let expected = if option == "FRUST_CACHE_INCREMENTAL" {
            2
        } else {
            1
        };
        assert_eq!(calls.len(), expected, "{option}: {calls:#?}");
        assert!(calls.iter().any(|call| call.contains("\napp\n")));
        assert_eq!(fixture.sb.count_cache("hit", "app"), 0);
        if expected == 2 {
            assert_eq!(fixture.sb.count_cache("hit", "leaf"), 0);
        } else {
            assert_eq!(fixture.sb.count_cache("hit", "leaf"), 1);
        }
        fixture.run(7);
    }
}

#[test]
fn optimized_incremental_flags_and_nonincremental_domain_do_not_cross_hit() {
    // LTO may leave a dependency invocation unchanged, so distinct primary invocations are the negative oracle.
    let mut fixture = Fixture::new("optimized-inc-domains", "false");
    fixture.edit(9);
    let variants = [
        vec![],
        vec![("CARGO_PROFILE_RELEASE_OPT_LEVEL", "2")],
        vec![("CARGO_PROFILE_RELEASE_LTO", "thin")],
        vec![("CARGO_INCREMENTAL", "0")],
    ];
    let mut original_call = None;
    for options in variants {
        fixture.clean();
        let hits = fixture.sb.count_cache("hit", "app");
        let calls = fixture.build(true, &options);
        assert!(
            calls.iter().any(|call| call.contains("\napp\n")),
            "different compiler flags must compile the primary unit: {calls:#?}"
        );
        let app_call = calls.iter().find(|call| call.contains("\napp\n")).unwrap();
        match &original_call {
            None => original_call = Some(app_call.clone()),
            Some(original) => assert_ne!(
                app_call, original,
                "the negative control must change actual rustc arguments"
            ),
        }
        if options.iter().any(|&(key, _)| key == "CARGO_INCREMENTAL") {
            assert!(!app_call.lines().any(|arg| arg.starts_with("incremental=")));
        }
        assert_eq!(fixture.sb.count_cache("hit", "app"), hits);
        fixture.run(9);
        fixture.clean();
        assert!(
            fixture.build(true, &options).is_empty(),
            "the identical option set should restore both units"
        );
        fixture.run(9);
    }
}

#[test]
fn direct_optimized_incremental_metadata_restores_without_a_native_compile() {
    let fixture = Fixture::new("optimized-inc-direct", "false");
    fixture.edit(11);
    let audit = fixture.sb.root.join("direct-audit");
    let out = fixture.sb.root.join("direct-out");
    let session = fixture.sb.root.join("direct-session");
    fs::create_dir(&audit).unwrap();
    fs::create_dir(&out).unwrap();
    let invoke = || {
        fixture
            .sb
            .bin("frustc")
            .args([
                "--edition=2021",
                "--crate-type=lib",
                "--crate-name",
                "direct_leaf",
                "--emit=dep-info,metadata",
                "-Copt-level=3",
                "-C",
            ])
            .arg(format!("incremental={}", session.display()))
            .arg("--out-dir")
            .arg(&out)
            .arg(fixture.project.join("leaf/src/lib.rs"))
            .current_dir(&fixture.project)
            .env("FRUST_RUSTC", &fixture.compiler)
            .env("FRUST_TEST_AUDIT", &audit)
            .output()
            .unwrap()
    };
    assert_success(&invoke());
    assert_eq!(fs::read_dir(&audit).unwrap().count(), 1);
    assert_eq!(fixture.sb.count_cache("miss", "direct_leaf"), 1);
    let expected = files(&out);
    assert!(
        expected
            .keys()
            .any(|path| path.extension().is_some_and(|ext| ext == "rmeta"))
    );
    // Direct invocations must preserve their own native state just as Cargo-owned invocations do.
    let native = files(&session);
    frust::sys::rename(&out, &fixture.sb.root.join("first-direct-out")).unwrap();
    fs::create_dir(&out).unwrap();
    assert_success(&invoke());
    assert_eq!(fs::read_dir(&audit).unwrap().count(), 1);
    assert_eq!(fixture.sb.count_cache("hit", "direct_leaf"), 1);
    assert!(
        files(&out) == expected,
        "direct output bytes and modes must restore"
    );
    assert!(
        files(&session) == native,
        "direct restore must leave native session bytes intact"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn optimized_incremental_packed_debug_restores_the_complete_bundle() {
    let mut fixture = Fixture::new("optimized-inc-packed-debug", "false");
    fixture.edit(13);
    let options = [
        ("CARGO_PROFILE_RELEASE_DEBUG", "2"),
        ("CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO", "packed"),
    ];
    incremental_calls(&fixture.build(true, &options));
    fixture.run(13);
    // Restore the original packed-debug bytes; do not equate independent Mach-O debug compilations.
    let original = files(&fixture.target.join("release"));
    write_inventory(&fixture.sb.root.join("packed-original.json"), &original);
    assert!(
        original.keys().any(|path| path
            .components()
            .any(|part| part.as_os_str().to_string_lossy().ends_with(".dSYM"))),
        "the compiler must actually emit a packed debug bundle"
    );
    fixture.clean();
    assert!(fixture.build(true, &options).is_empty());
    fixture.run(13);
    let restored = files(&fixture.target.join("release"));
    write_inventory(&fixture.sb.root.join("packed-restored.json"), &restored);
    assert_files(&original, &restored, false, "packed executable, bundle and alias restoration");
    assert_eq!(fixture.sb.count_cache("hit", "app"), 1);
}
