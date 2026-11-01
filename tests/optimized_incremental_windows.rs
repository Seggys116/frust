//! Windows optimized incremental restores preserve complete artifacts and leave later edits to the real compiler.

#![cfg(windows)]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Sandbox, assert_success, run_binary, settle};

const AUDITOR: &str = r#"
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let real = env::var_os("FRUST_TEST_REAL_RUSTC").expect("real compiler");
    let root = PathBuf::from(env::var_os("FRUST_TEST_AUDIT").expect("audit directory"));
    let mut record = OpenOptions::new().write(true).create_new(true)
        .open(root.join(format!("{}.args", std::process::id()))).expect("exclusive audit record");
    for arg in &args {
        let units: Vec<_> = arg.encode_wide().collect();
        record.write_all(&u32::try_from(units.len()).unwrap().to_le_bytes()).unwrap();
        for unit in units { record.write_all(&unit.to_le_bytes()).unwrap(); }
    }
    record.flush().unwrap();
    drop(record);
    // Named Windows jobserver authentication and every compiler argument remain inherited unchanged.
    let status = Command::new(real).args(&args).stdin(Stdio::inherit())
        .stdout(Stdio::inherit()).stderr(Stdio::inherit()).status().expect("real compiler spawn");
    std::process::exit(status.code().expect("Windows compiler exit code"));
}
"#;

struct Auditor {
    path: PathBuf,
    real: PathBuf,
}

impl Auditor {
    fn build(sb: &Sandbox) -> Self {
        let selected = std::env::var_os("RUSTC")
            .map(PathBuf::from)
            .or_else(|| frust::toolchain::resolve_tool("rustc"))
            .expect("an installed Rust compiler is required");
        let real = if selected.is_absolute() {
            selected
        } else if selected.components().count() == 1 {
            frust::toolchain::find_on_path(selected.to_str().expect("compiler name"))
                .expect("selected compiler on PATH")
        } else {
            std::env::current_dir().unwrap().join(selected)
        };
        let real = std::env::current_dir().unwrap().join(real);
        let source = sb.root.join("auditor.rs");
        let path = sb.root.join("audited-rustc.exe");
        fs::write(&source, AUDITOR).unwrap();
        // Compile this forwarding executable once in the owned sandbox, before any counted Cargo build.
        assert_success(
            &sb.bin_path(real.to_str().expect("compiler path"))
                .args([
                    "--edition=2021",
                    "--crate-name",
                    "incremental_auditor",
                    "-Copt-level=0",
                ])
                .arg(&source)
                .arg("-o")
                .arg(&path)
                .current_dir(&sb.root)
                .output()
                .unwrap(),
        );
        Self { path, real }
    }

    fn configure(&self, command: &mut Command, audit: &Path) {
        command
            .env("RUSTC", &self.path)
            .env("FRUST_RUSTC", &self.path)
            .env("FRUST_TEST_REAL_RUSTC", &self.real)
            .env("FRUST_TEST_AUDIT", audit);
    }
}

fn records(root: &Path) -> Vec<Vec<OsString>> {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let bytes = fs::read(entry.unwrap().path()).unwrap();
            let mut bytes = bytes.as_slice();
            let mut args = Vec::new();
            while !bytes.is_empty() {
                assert!(bytes.len() >= 4, "incomplete argument length");
                let length = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
                bytes = &bytes[4..];
                let length = length.checked_mul(2).unwrap();
                assert!(bytes.len() >= length, "incomplete UTF-16 argument");
                let units: Vec<_> = bytes[..length]
                    .chunks_exact(2)
                    .map(|unit| u16::from_le_bytes(unit.try_into().unwrap()))
                    .collect();
                args.push(OsString::from_wide(&units));
                bytes = &bytes[length..];
            }
            args
        })
        .collect()
}

fn compilation(args: &[OsString]) -> bool {
    let mut emit = false;
    let mut prints = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        let Some(arg) = arg.to_str() else { continue };
        emit |= arg == "--emit" || arg.starts_with("--emit=");
        if arg == "--print" {
            prints.push(
                args.get(index + 1)
                    .and_then(|value| value.to_str())
                    .unwrap_or(""),
            );
        } else if let Some(value) = arg.strip_prefix("--print=") {
            prints.push(value);
        }
    }
    // Only known inspection requests can exempt an emit-bearing call from the native compilation count.
    let inspection = !prints.is_empty()
        && prints.iter().all(|value| {
            matches!(
                *value,
                "file-names"
                    | "sysroot"
                    | "cfg"
                    | "split-debuginfo"
                    | "crate-name"
                    | "target-libdir"
            )
        });
    emit && !inspection
}

fn check_compilations(calls: &[Vec<OsString>]) {
    assert_eq!(
        calls.len(),
        2,
        "expected exactly two actual Rust units: {calls:#?}"
    );
    let mut names = Vec::new();
    for call in calls {
        let name = call
            .windows(2)
            .find(|pair| pair[0] == "--crate-name")
            .expect("crate name")[1]
            .clone();
        names.push(name);
        assert!(call.iter().any(|arg| arg == "opt-level=3"), "{call:#?}");
        assert!(
            call.iter().any(|arg| arg
                .to_str()
                .is_some_and(|arg| arg.starts_with("incremental="))),
            "{call:#?}"
        );
    }
    names.sort();
    assert_eq!(names, [OsString::from("app"), OsString::from("leaf")]);
}

type Files = BTreeMap<PathBuf, (Vec<u8>, bool)>;

fn artifacts(root: &Path) -> Files {
    fn visit(root: &Path, dir: &Path, files: &mut Files) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy();
            if matches!(
                name.as_ref(),
                "incremental" | ".fingerprint" | "build" | ".cargo-lock"
            ) {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            assert!(
                !meta.file_type().is_symlink(),
                "unexpected artifact symlink: {}",
                path.display()
            );
            if meta.is_dir() {
                visit(root, &path, files);
            } else {
                assert!(meta.is_file());
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    (fs::read(&path).unwrap(), meta.permissions().readonly()),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn inventory(project: &Path, phase: &str, files: &Files) {
    assert!(
        files.len() <= 256,
        "fixture artifact inventory exceeded its bound"
    );
    let rows: Vec<_> = files.iter().map(|(path, (bytes, readonly))| {
        let path = path.to_str().expect("fixture output name");
        assert!(path.len() <= 1024);
        serde_json::json!({"path": path, "bytes": bytes.len(), "blake3": blake3::hash(bytes).to_hex().as_str(), "readonly": readonly})
    }).collect();
    let encoded = serde_json::to_vec_pretty(&rows).unwrap();
    assert!(
        encoded.len() <= 256 * 1024,
        "fixture artifact inventory exceeded its byte bound"
    );
    fs::write(project.join(format!("artifacts-{phase}.json")), encoded).unwrap();
}

fn assert_artifacts(original: &Files, restored: &Files, profile: &str) {
    assert_eq!(
        original.keys().collect::<Vec<_>>(),
        restored.keys().collect::<Vec<_>>(),
        "{profile}: artifact paths differ; see retained inventories"
    );
    for (path, (bytes, readonly)) in original {
        let (other, other_readonly) = &restored[path];
        assert_eq!(
            readonly,
            other_readonly,
            "{profile}: {} read-only attribute differs",
            path.display()
        );
        assert!(
            bytes == other,
            "{profile}: {} bytes differ: lengths {}/{}, BLAKE3 {}/{}",
            path.display(),
            bytes.len(),
            other.len(),
            blake3::hash(bytes),
            blake3::hash(other)
        );
    }
}

fn run(
    sb: &Sandbox,
    auditor: &Auditor,
    project: &Path,
    target: &Path,
    operation: &str,
    phase: &str,
) -> Vec<Vec<OsString>> {
    let audit = project.join(format!("audit-{phase}"));
    fs::create_dir(&audit).unwrap();
    let mut command = if operation == "clean" {
        sb.cargo()
    } else {
        sb.bin("fcargo")
    };
    command
        .arg(operation)
        .current_dir(project)
        .env("CARGO_TARGET_DIR", target);
    auditor.configure(&mut command, &audit);
    if operation == "build" {
        command
            .args(["--release", "--offline", "--message-format=json"])
            .env("FRUST_AUTO_COMPACT", "off")
            .env("FRUST_TUNE", "off")
            .env("FRUST_LINK", "off");
    }
    assert_success(&command.output().unwrap());
    records(&audit)
        .into_iter()
        .filter(|args| compilation(args))
        .collect()
}

fn edit(project: &Path, value: u32) {
    let source = project.join("leaf/src/lib.rs");
    fs::write(
        &source,
        format!("#[inline(never)]\npub fn value() -> u32 {{ {value} }}\n"),
    )
    .unwrap();
    settle(&source);
}

fn runtime(target: &Path, value: u32) {
    let output = run_binary(target, "release", "app");
    assert_success(&output);
    assert_eq!(output.stdout, format!("{value}\n").as_bytes());
}

fn native_sessions(target: &Path) {
    let root = target.join("release/incremental");
    for name in ["app", "leaf"] {
        let present = fs::read_dir(&root).unwrap().any(|entry| {
            let entry = entry.unwrap();
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{name}-"))
                && fs::read_dir(entry.path()).unwrap().any(|session| {
                    let session = session.unwrap();
                    let name = session.file_name();
                    let name = name.to_string_lossy();
                    session.file_type().unwrap().is_dir()
                        && name.starts_with("s-")
                        && !name.ends_with("-working")
                })
        });
        assert!(
            present,
            "{name} must publish a genuine finalized native session"
        );
    }
}

#[test]
fn release_thin_and_fat_incremental_restore_all_windows_artifacts_then_compile_a_real_edit() {
    let sb = Sandbox::new("windows-optimized-incremental");
    let auditor = Auditor::build(&sb);
    for (tag, lto) in [
        ("release", "false"),
        ("thin", "\"thin\""),
        ("fat", "\"fat\""),
    ] {
        let project = sb.project(tag, &[], "fn main() { println!(\"{}\", leaf::value()); }\n");
        fs::create_dir_all(project.join("leaf/src")).unwrap();
        fs::write(
            project.join("leaf/Cargo.toml"),
            "[package]\nname = \"leaf\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let manifest = project.join("Cargo.toml");
        fs::write(&manifest, format!("{}leaf = {{ path = \"leaf\" }}\n\n[profile.release]\nopt-level = 3\nlto = {lto}\ncodegen-units = 2\nincremental = true\ndebug = 2\n", fs::read_to_string(&manifest).unwrap())).unwrap();
        let target = sb.root.join(format!("target-{tag}"));
        edit(&project, 41);
        let before = sb.cache_events().len();
        check_compilations(&run(&sb, &auditor, &project, &target, "build", "initial"));
        runtime(&target, 41);
        native_sessions(&target);
        let events = sb.cache_events();
        assert_eq!(events.len() - before, 2, "{tag}: {events:#?}");
        for name in ["app", "leaf"] {
            assert!(
                events[before..].iter().any(|event| event["crate"] == name
                    && event["result"] == "miss"
                    && event["bytes"].as_u64().is_some_and(|bytes| bytes > 0)),
                "{tag}: {events:#?}"
            );
        }
        // Include every emitted PDB and import-library companion; fingerprints and native sessions are not restored outputs.
        let original = artifacts(&target.join("release"));
        inventory(&project, "initial", &original);
        for extension in ["exe", "rlib", "rmeta"] {
            assert!(
                original
                    .keys()
                    .any(|path| path.extension().is_some_and(|ext| ext == extension)),
                "{tag}: missing {extension}"
            );
        }
        if cfg!(target_env = "msvc") {
            assert!(
                original
                    .keys()
                    .any(|path| path.extension().is_some_and(|ext| ext == "pdb")),
                "full Windows MSVC debug info must actually emit a PDB"
            );
        }
        assert!(run(&sb, &auditor, &project, &target, "clean", "clean").is_empty());
        let before = sb.cache_events().len();
        assert!(
            run(&sb, &auditor, &project, &target, "build", "restore").is_empty(),
            "{tag}: restored units must not compile"
        );
        runtime(&target, 41);
        let restored = artifacts(&target.join("release"));
        inventory(&project, "restored", &restored);
        assert_artifacts(&original, &restored, tag);
        let events = sb.cache_events();
        assert_eq!(events.len() - before, 2, "{tag}: {events:#?}");
        assert!(
            !events[before..]
                .iter()
                .any(|event| event["result"] == "miss")
        );
        for name in ["app", "leaf"] {
            assert!(
                events[before..].iter().any(|event| event["crate"] == name
                    && event["result"] == "hit"
                    && event["bytes"].as_u64().is_some_and(|bytes| bytes > 0)),
                "{tag}: {events:#?}"
            );
        }
        edit(&project, 42);
        let before = sb.cache_events().len();
        check_compilations(&run(&sb, &auditor, &project, &target, "build", "edited"));
        runtime(&target, 42);
        native_sessions(&target);
        let events = sb.cache_events();
        assert_eq!(events.len() - before, 2, "{tag}: {events:#?}");
        assert!(
            events[before..]
                .iter()
                .all(|event| event["result"] == "miss")
        );
        // Synchronous stores finish before return; a final Cargo no-op may not create another compile or cache event.
        let before = sb.cache_events().len();
        assert!(run(&sb, &auditor, &project, &target, "build", "settled").is_empty());
        assert_eq!(sb.cache_events().len(), before);
    }
}
