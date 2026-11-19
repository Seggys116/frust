//! The shipped binaries, exercised against real cargo and real archives.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fcargo_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_fcargo"))
}

fn gates_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_frust-gates"))
}

fn cargo_110() -> PathBuf {
    let output = Command::new("rustup")
        .args(["which", "--toolchain", "1.100.0", "cargo"])
        .output()
        .expect("rustup which cargo");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim().to_string())
}

fn write_project(root: &Path, source: &str) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"frust_probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), source).unwrap();
}

fn run(bin: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(bin);
    command.args(args).env("PATH", "/home/seggy116/.cargo/bin:/usr/bin:/bin");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("spawn")
}

#[test]
fn plain_and_dumb_match_cargo_and_static_keeps_diagnostics() {
    let cargo = cargo_110();
    let root = std::env::temp_dir().join(format!("frust-fcargo-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let project = root.join("hello");
    let target = root.join("target");
    write_project(&project, "fn main() {\n    println!(\"frust-hello\");\n}\n");
    let manifest = project.join("Cargo.toml");
    let common = [
        "build",
        "--offline",
        "--manifest-path",
        manifest.to_str().unwrap(),
    ];
    let envs = [
        ("FRUST_CARGO", cargo.to_str().unwrap()),
        ("CARGO_TARGET_DIR", target.to_str().unwrap()),
        ("CARGO_TERM_COLOR", "never"),
    ];

    let plain = run(
        fcargo_bin(),
        &common,
        &[envs[0], envs[1], envs[2], ("FRUST_PROGRESS", "plain"), ("TERM", "xterm")],
    );
    let stock = run(&cargo, &common, &[envs[1], envs[2], ("TERM", "dumb")]);
    assert_eq!(plain.status.code(), stock.status.code());
    assert!(plain.status.success(), "{}", String::from_utf8_lossy(&plain.stderr));
    assert_eq!(plain.stdout, stock.stdout);
    let plain_err = String::from_utf8_lossy(&plain.stderr);
    let stock_err = String::from_utf8_lossy(&stock.stderr);
    assert!(plain_err.contains("Compiling"), "{plain_err}");
    assert!(stock_err.contains("Finished"), "{stock_err}");
    assert!(!plain_err.contains("fRust"), "{plain_err}");
    let program = target.join("debug/frust_probe");
    let ran = Command::new(&program).output().unwrap();
    assert_eq!(String::from_utf8(ran.stdout).unwrap(), "frust-hello\n");

    let dumb = run(
        fcargo_bin(),
        &common,
        &[envs[0], envs[1], envs[2], ("FRUST_PROGRESS", "static"), ("TERM", "dumb")],
    );
    assert!(dumb.status.success());
    assert!(!String::from_utf8_lossy(&dumb.stderr).contains("fRust"));

    let again = run(
        fcargo_bin(),
        &common,
        &[envs[0], envs[1], envs[2], ("FRUST_PROGRESS", "static"), ("TERM", "xterm")],
    );
    let frame = String::from_utf8(again.stderr).unwrap();
    assert!(again.status.success(), "{frame}");
    assert!(frame.contains("[--]"), "{frame}");
    assert!(frame.contains("Reused cached units"), "{frame}");
    assert!(!frame.contains('\u{1b}'));

    let broken = root.join("broken");
    write_project(&broken, "fn main() {\n    let value: i32 = \"no\";\n    let _ = value;\n}\n");
    let broken_manifest = broken.join("Cargo.toml");
    let broken_target = root.join("broken-target");
    let failed = run(
        fcargo_bin(),
        &["build", "--offline", "--manifest-path", broken_manifest.to_str().unwrap()],
        &[
            ("FRUST_CARGO", cargo.to_str().unwrap()),
            ("CARGO_TARGET_DIR", broken_target.to_str().unwrap()),
            ("CARGO_TERM_COLOR", "always"),
            ("FRUST_PROGRESS", "static"),
            ("TERM", "xterm"),
        ],
    );
    let diag = String::from_utf8_lossy(&failed.stderr);
    assert_ne!(failed.status.code(), Some(0), "{diag}");
    assert!(diag.contains("expected `i32`") || diag.contains("expected i32"), "{diag}");
    assert!(diag.contains('\u{1b}'), "{diag}");
    assert!(diag.contains("[!!]"), "{diag}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn gates_binary_recomputes_a_passing_and_failing_cohort() {
    let root = std::env::temp_dir().join(format!("frust-gates-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let samples = root.join("samples.txt");
    let output = root.join("gates.json");
    fs::write(&samples, passing_samples()).unwrap();
    let ok = run(
        gates_bin(),
        &["--samples", samples.to_str().unwrap(), "--output", output.to_str().unwrap()],
        &[],
    );
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    let json = fs::read_to_string(&output).unwrap();
    assert!(json.contains("\"status\": \"passed\"") || json.contains("\"status\":\"passed\""), "{json}");
    assert!(!json.contains("\"status\": \"failed\"") && !json.contains("\"status\":\"failed\""), "{json}");

    fs::write(&samples, "edit only leaf 2.0 1.0\n").unwrap();
    let bad = run(
        gates_bin(),
        &["--samples", samples.to_str().unwrap(), "--output", output.to_str().unwrap()],
        &[],
    );
    assert_eq!(bad.status.code(), Some(1));
    let failed = fs::read_to_string(&output).unwrap();
    assert!(failed.contains("failed"), "{failed}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn plain_fcargo_selects_sibling_frustc_unless_rustc_is_set() {
    let root = std::env::temp_dir().join(format!("frust-sibling-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    fs::write(
        &cargo,
        "#!/bin/sh\nprintf 'RUSTC=%s\\n' \"${RUSTC-unset}\"\nprintf 'INV=%s\\n' \"${FRUST_INVOCATION-unset}\"\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&cargo).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&cargo, perms).unwrap();
    let frustc = bin.join("frustc");
    fs::write(&frustc, "").unwrap();

    let selected = Command::new(fcargo_bin())
        .args(["--version"])
        .env("PATH", "/usr/bin:/bin")
        .env("FRUST_CARGO", &cargo)
        .env("FRUST_PROGRESS", "plain")
        .env("TERM", "dumb")
        .env_remove("RUSTC")
        .output()
        .unwrap();
    let text = String::from_utf8(selected.stdout).unwrap();
    assert!(selected.status.success(), "{}", String::from_utf8_lossy(&selected.stderr));
    assert!(text.contains(&format!("RUSTC={}\n", frustc.display())), "{text}");
    assert!(text.contains("INV=fcargo\n"), "{text}");

    let kept = Command::new(fcargo_bin())
        .args(["--version"])
        .env("PATH", "/usr/bin:/bin")
        .env("FRUST_CARGO", &cargo)
        .env("FRUST_PROGRESS", "plain")
        .env("TERM", "dumb")
        .env("RUSTC", "keep-rustc")
        .output()
        .unwrap();
    let kept_text = String::from_utf8(kept.stdout).unwrap();
    assert!(kept.status.success(), "{}", String::from_utf8_lossy(&kept.stderr));
    assert!(kept_text.contains("RUSTC=keep-rustc\n"), "{kept_text}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn interactive_pty_repaints_before_a_slow_build_finishes() {
    let root = std::env::temp_dir().join(format!("frust-pty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let cargo = root.join("cargo");
    fs::write(
        &cargo,
        "#!/bin/sh\nsleep 0.5\nprintf '%s\\n' '{\"reason\":\"compiler-artifact\",\"fresh\":false}'\nsleep 0.2\nprintf '%s\\n' '{\"reason\":\"build-finished\",\"success\":true}'\n",
    )
    .unwrap();
    let mut perms = fs::metadata(&cargo).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(&cargo, perms).unwrap();
    let driver = root.join("drive_terminal.py");
    fs::write(
        &driver,
        r#"import os, pty, select, sys
pid, fd = pty.fork()
if pid == 0:
    os.environ["FRUST_CARGO"] = sys.argv[2]
    os.environ["FRUST_PROGRESS"] = "interactive"
    os.environ["FRUST_PROGRESS_COLOR"] = "never"
    os.environ["TERM"] = "xterm"
    os.environ["COLUMNS"] = "80"
    os.environ["PATH"] = "/usr/bin:/bin"
    os.execv(sys.argv[1], [sys.argv[1], "build"])
data = b""
while True:
    ready, _, _ = select.select([fd], [], [], 3)
    if not ready:
        break
    try:
        chunk = os.read(fd, 8192)
    except OSError:
        break
    if not chunk:
        break
    data += chunk
_, status = os.waitpid(pid, 0)
sys.stdout.buffer.write(data)
sys.exit(os.WEXITSTATUS(status) if os.WIFEXITED(status) else 1)
"#,
    )
    .unwrap();
    let output = Command::new("python3").arg(&driver).arg(fcargo_bin()).arg(&cargo).output().unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}{}", String::from_utf8_lossy(&output.stderr));
    assert!(text.contains("fRust"), "{text}");
    assert!(text.contains("[ok]"), "{text}");
    assert!(text.contains('\u{1b}'), "{text}");
    let _ = fs::remove_dir_all(&root);
}

fn patched_cargo() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/compiler-rewrite/cargo-dev/debug/cargo");
    assert!(path.is_file(), "patched cargo is missing at {}", path.display());
    path
}

fn target_directory(output: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&output.stdout);
    let marker = "\"target_directory\":";
    let start = text.find(marker).unwrap_or_else(|| panic!("no target_directory in {text}"));
    let rest = text[start + marker.len()..].trim_start();
    assert!(rest.starts_with('"'), "{rest}");
    let rest = &rest[1..];
    let end = rest.find('"').unwrap();
    rest[..end].replace("\\/", "/")
}

fn cargo_metadata(cargo: &Path, manifest: &Path, envs: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    let mut command = Command::new(cargo);
    command
        .arg("metadata")
        .arg("--offline")
        .arg("--no-deps")
        .arg("--format-version")
        .arg("1")
        .arg("--manifest-path")
        .arg(manifest)
        .args(args)
        .env("PATH", "/usr/bin:/bin")
        .env_remove("FRUST_INVOCATION")
        .env_remove("FRUST_CENTRAL_TARGET")
        .env_remove("FRUST_TOOLCHAIN_ID")
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_DIR");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("spawn patched cargo")
}

#[test]
fn patched_cargo_namespaces_fcargo_and_preserves_overrides() {
    let cargo = patched_cargo();
    let root = std::env::temp_dir().join(format!("frust-central-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let project = root.join("app");
    write_project(&project, "fn main() {}\n");
    let manifest = project.join("Cargo.toml");
    let xdg_config = root.join("config");
    let xdg_cache = root.join("cache");
    fs::create_dir_all(&xdg_config).unwrap();
    fs::create_dir_all(&xdg_cache).unwrap();
    let xdg = [
        ("XDG_CONFIG_HOME", xdg_config.to_str().unwrap()),
        ("XDG_CACHE_HOME", xdg_cache.to_str().unwrap()),
    ];

    let stock = cargo_metadata(&cargo, &manifest, &[], &[]);
    assert!(stock.status.success(), "{}", String::from_utf8_lossy(&stock.stderr));
    let stock_target = target_directory(&stock);
    assert!(stock_target.ends_with("/app/target"), "{stock_target}");

    let namespaced = cargo_metadata(
        &cargo,
        &manifest,
        &[xdg[0], xdg[1], ("FRUST_INVOCATION", "fcargo"), ("FRUST_TOOLCHAIN_ID", "tool-a")],
        &[],
    );
    assert!(namespaced.status.success(), "{}", String::from_utf8_lossy(&namespaced.stderr));
    let namespaced_target = target_directory(&namespaced);
    assert!(namespaced_target.contains("/frust/targets/"), "{namespaced_target}");
    assert!(!namespaced_target.contains("/app/target"), "{namespaced_target}");
    let marker = xdg_cache.join("frust/artifacts/MARKER");
    assert_eq!(fs::read_to_string(&marker).unwrap(), "frust-cache-v1");

    let other = root.join("other");
    write_project(&other, "fn main() {}\n");
    let other_meta = cargo_metadata(
        &cargo,
        &other.join("Cargo.toml"),
        &[xdg[0], xdg[1], ("FRUST_INVOCATION", "fcargo"), ("FRUST_TOOLCHAIN_ID", "tool-a")],
        &[],
    );
    assert_ne!(target_directory(&other_meta), namespaced_target);

    let relinked = root.join("app-link");
    std::os::unix::fs::symlink(&project, &relinked).unwrap();
    let via_link = cargo_metadata(
        &cargo,
        &relinked.join("Cargo.toml"),
        &[xdg[0], xdg[1], ("FRUST_INVOCATION", "fcargo"), ("FRUST_TOOLCHAIN_ID", "tool-a")],
        &[],
    );
    assert_eq!(target_directory(&via_link), namespaced_target);

    let other_tool = cargo_metadata(
        &cargo,
        &manifest,
        &[xdg[0], xdg[1], ("FRUST_INVOCATION", "fcargo"), ("FRUST_TOOLCHAIN_ID", "tool-b")],
        &[],
    );
    assert_ne!(target_directory(&other_tool), namespaced_target);

    let explicit = root.join("explicit-target");
    let overridden = cargo_metadata(
        &cargo,
        &manifest,
        &[
            xdg[0],
            xdg[1],
            ("FRUST_INVOCATION", "fcargo"),
            ("FRUST_TOOLCHAIN_ID", "tool-a"),
            ("CARGO_TARGET_DIR", explicit.to_str().unwrap()),
        ],
        &[],
    );
    assert!(overridden.status.success(), "{}", String::from_utf8_lossy(&overridden.stderr));
    assert_eq!(target_directory(&overridden), explicit.to_str().unwrap());

    let off = cargo_metadata(
        &cargo,
        &manifest,
        &[xdg[0], xdg[1], ("FRUST_INVOCATION", "fcargo"), ("FRUST_CENTRAL_TARGET", "off")],
        &[],
    );
    assert!(target_directory(&off).ends_with("/app/target"), "{}", target_directory(&off));

    let config_dir = xdg_config.join("frust");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.json"), "{\"version\":1").unwrap();
    let malformed = cargo_metadata(
        &cargo,
        &manifest,
        &[xdg[0], xdg[1], ("FRUST_INVOCATION", "fcargo"), ("FRUST_TOOLCHAIN_ID", "tool-a")],
        &[],
    );
    let malformed_err = String::from_utf8_lossy(&malformed.stderr);
    assert!(!malformed.status.success(), "{malformed_err}");
    assert!(malformed_err.contains("fRust rejected"), "{malformed_err}");
    assert!(malformed_err.contains("end of JSON"), "{malformed_err}");

    let custom_root = root.join("custom-root");
    fs::write(
        config_dir.join("config.json"),
        format!("{{\"version\":1,\"target_root\":\"{}\"}}\n", custom_root.display()),
    )
    .unwrap();
    let custom = cargo_metadata(
        &cargo,
        &manifest,
        &[xdg[0], xdg[1], ("FRUST_INVOCATION", "fcargo"), ("FRUST_TOOLCHAIN_ID", "tool-a")],
        &[],
    );
    assert!(custom.status.success(), "{}", String::from_utf8_lossy(&custom.stderr));
    assert!(target_directory(&custom).starts_with(&format!("{}/targets/", custom_root.display())));

    fs::write(xdg_cache.join("frust/artifacts/MARKER"), "frust-cache-v0").unwrap();
    let corrupt = cargo_metadata(
        &cargo,
        &manifest,
        &[xdg[0], xdg[1], ("FRUST_INVOCATION", "fcargo"), ("FRUST_TOOLCHAIN_ID", "tool-a")],
        &[],
    );
    assert!(corrupt.status.success(), "{}", String::from_utf8_lossy(&corrupt.stderr));
    assert_eq!(fs::read_to_string(xdg_cache.join("frust/artifacts/MARKER")).unwrap(), "frust-cache-v0");

    let slashed = cargo_metadata(
        &cargo,
        &manifest,
        &[xdg[0], xdg[1], ("FRUST_INVOCATION", "fcargo"), ("FRUST_TOOLCHAIN_ID", "bad/id")],
        &[],
    );
    assert!(!slashed.status.success());
    assert!(String::from_utf8_lossy(&slashed.stderr).contains("rejected toolchain id"));

    let _ = fs::remove_dir_all(&root);
}

fn passing_samples() -> String {
    let mut out = String::new();
    for name in ["ripgrep", "syn", "regex"] {
        push_edits(&mut out, name, "leaf", 0.70, 1.0, 60);
        push_edits(&mut out, name, "wide", 0.90, 1.0, 60);
        push_edits(&mut out, name, "cold", 1.0, 1.0, 30);
        push_edits(&mut out, name, "noop", 0.20, 0.20, 30);
    }
    push_edits(&mut out, "fd", "holdout", 0.75, 1.0, 30);
    push_edits(&mut out, "fd", "cold", 1.0, 1.0, 30);
    push_edits(&mut out, "fd", "noop", 0.20, 0.20, 30);
    for name in ["ripgrep", "syn", "regex", "fd"] {
        out.push_str(&format!("resource {name} 100 100 100 100\n"));
    }
    for name in ["ripgrep", "rustfmt", "oxipng"] {
        let frust = vec!["0.90"; 30].join(",");
        let stock = vec!["1.0"; 30].join(",");
        out.push_str(&format!("runtime {name} 0 {frust} {stock}\n"));
        out.push_str(&format!("size {name} 90 100\n"));
    }
    out
}

fn install_bin() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_frust-install"))
}

fn sha256_file(path: &Path) -> String {
    let output = Command::new("sha256sum").arg(path).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap().split_whitespace().next().unwrap().to_string()
}

fn toy_archive(dir: &Path, id: &str, marker: &str) -> (PathBuf, PathBuf) {
    let root = dir.join(format!("tree-{id}"));
    fs::create_dir_all(root.join("bin")).unwrap();
    for name in ["rustc", "cargo", "rustdoc"] {
        fs::write(root.join("bin").join(name), marker).unwrap();
    }
    fs::write(root.join("manifest.json"), format!("{{\"id\":\"{id}\"}}\n")).unwrap();
    let archive = dir.join(format!("{id}.tar"));
    let status = Command::new("tar")
        .args(["-c", "-f"])
        .arg(&archive)
        .arg("-C")
        .arg(&root)
        .arg(".")
        .status()
        .unwrap();
    assert!(status.success());
    let sum = dir.join(format!("{id}.sha256"));
    fs::write(&sum, format!("{}\n", sha256_file(&archive))).unwrap();
    (archive, sum)
}

#[test]
fn install_binary_refuses_a_bad_checksum_and_keeps_an_interrupted_tree_inactive() {
    let bin = install_bin();
    let dir = std::env::temp_dir().join(format!("frust-install-cli-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let prefix = dir.join("prefix");
    let (first, first_sum) = toy_archive(&dir, "first", "one");
    let (second, second_sum) = toy_archive(&dir, "second", "two");
    let bad = dir.join("bad.sha256");
    fs::write(&bad, "0000000000000000000000000000000000000000000000000000000000000000\n").unwrap();

    let refused = Command::new(bin)
        .args(["install", "--prefix", prefix.to_str().unwrap(), "--archive", first.to_str().unwrap(), "--checksum", bad.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("checksum mismatch"));
    assert!(!prefix.join("current").exists());
    assert!(!prefix.join("versions/first").exists());

    let installed = Command::new(bin)
        .args(["install", "--prefix", prefix.to_str().unwrap(), "--archive", first.to_str().unwrap(), "--checksum", first_sum.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(installed.status.success(), "{}", String::from_utf8_lossy(&installed.stderr));
    assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "one");

    let staged = Command::new(bin)
        .args(["stage", "--prefix", prefix.to_str().unwrap(), "--archive", second.to_str().unwrap(), "--checksum", second_sum.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(staged.status.success(), "{}", String::from_utf8_lossy(&staged.stderr));
    let staged_out = String::from_utf8(staged.stdout).unwrap();
    assert!(staged_out.contains("current=first"), "{staged_out}");
    assert!(!prefix.join("versions/second").exists());
    assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "one");

    let upgraded = Command::new(bin)
        .args(["upgrade", "--prefix", prefix.to_str().unwrap(), "--archive", second.to_str().unwrap(), "--checksum", second_sum.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(upgraded.status.success(), "{}", String::from_utf8_lossy(&upgraded.stderr));
    assert_eq!(String::from_utf8(upgraded.stdout).unwrap().trim(), "current=second");
    assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "two");

    let rolled = Command::new(bin).args(["rollback", "--prefix", prefix.to_str().unwrap()]).output().unwrap();
    assert!(rolled.status.success(), "{}", String::from_utf8_lossy(&rolled.stderr));
    assert_eq!(fs::read_to_string(prefix.join("current/bin/rustc")).unwrap(), "one");

    let removed = Command::new(bin).args(["uninstall", "--prefix", prefix.to_str().unwrap()]).output().unwrap();
    assert!(removed.status.success(), "{}", String::from_utf8_lossy(&removed.stderr));
    assert!(!prefix.join("current").exists());
    assert!(!prefix.join("versions").exists());
    let _ = fs::remove_dir_all(&dir);
}

fn push_edits(out: &mut String, workload: &str, kind: &str, frust: f64, stock: f64, count: usize) {
    for _ in 0..count {
        out.push_str(&format!("edit {workload} {kind} {frust} {stock}\n"));
    }
}
