//! Memoised toolchain and workspace answers must be dropped the moment what they describe changes.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::thread;
use std::time::Duration;

use common::{Sandbox, assert_success, stderr, stdout};

/// `cargo test` exports `RUSTUP_TOOLCHAIN`, which would override `rust-toolchain.toml`, so commands here run without it.
fn fcargo(sb: &Sandbox, dir: &Path, args: &[&str], envs: &[(&str, &Path)]) -> Output {
    let mut cmd = sb.bin("fcargo");
    cmd.args(args).env("FRUST_DEBUG", "1").env_remove("RUSTUP_TOOLCHAIN").current_dir(dir);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn where_(sb: &Sandbox, dir: &Path, envs: &[(&str, &Path)]) -> PathBuf {
    let mut cmd = sb.bin("frust");
    cmd.args(["where", "--fresh"]).env_remove("RUSTUP_TOOLCHAIN").current_dir(dir);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert_success(&out);
    PathBuf::from(stdout(&out).trim())
}

fn traced(out: &Output, what: &str, outcome: &str) -> bool {
    stderr(out).contains(&format!("({what} cache {outcome})"))
}

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn executable(path: &Path, text: &str) {
    write(path, text);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn replace(path: &Path, text: &str) {
    let staged = path.with_extension("staged");
    executable(&staged, text);
    fs::rename(&staged, path).unwrap();
}

#[test]
fn second_build_uses_both_caches() {
    let sb = Sandbox::new("startup-hit");
    let app = sb.project("app", &[], "fn main() {}\n");
    let first = fcargo(&sb, &app, &["build"], &[]);
    assert_success(&first);
    assert!(traced(&first, "probe", "miss"), "{}", stderr(&first));
    assert!(traced(&first, "locate", "miss"), "{}", stderr(&first));
    assert!(sb.home().join("probe").read_dir().unwrap().count() == 1, "one probe entry");

    let second = fcargo(&sb, &app, &["build"], &[]);
    assert_success(&second);
    assert!(traced(&second, "probe", "hit"), "{}", stderr(&second));
    assert!(traced(&second, "locate", "hit"), "{}", stderr(&second));
    let dir = where_(&sb, &app, &[]);
    assert!(dir.join("debug/app").is_file(), "the cached answers pick the same directory as fresh ones: {}", dir.display());
}

#[test]
fn probe_cache_follows_toolchain_file_and_plus_toolchain() {
    let sb = Sandbox::new("startup-tc");
    let app = sb.project("app", &[], "fn main() {}\n");
    assert_success(&fcargo(&sb, &app, &["build"], &[]));
    let warm = fcargo(&sb, &app, &["build"], &[]);
    assert!(traced(&warm, "probe", "hit"), "{}", stderr(&warm));
    let stable = where_(&sb, &app, &[]);

    write(&app.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.98.1\"\n");
    let pinned_run = fcargo(&sb, &app, &["build"], &[]);
    assert_success(&pinned_run);
    assert!(traced(&pinned_run, "probe", "miss"), "a new toolchain file must re-probe: {}", stderr(&pinned_run));
    let pinned = where_(&sb, &app, &[]);
    assert_ne!(pinned, stable, "1.98.1 has its own toolchain id");
    assert!(pinned.join("debug/app").is_file(), "built with the pinned toolchain into {}", pinned.display());

    write(&app.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"stable\"\n");
    let back = fcargo(&sb, &app, &["build"], &[]);
    assert_success(&back);
    assert!(traced(&back, "probe", "miss"), "{}", stderr(&back));
    assert_eq!(where_(&sb, &app, &[]), stable);

    write(&app.join("rust-toolchain.toml"), "[toolchain]\nchannel = \"1.98.1\"\n");
    assert_success(&fcargo(&sb, &app, &["build"], &[]));
    let plus = fcargo(&sb, &app, &["+stable", "build"], &[]);
    assert_success(&plus);
    assert!(traced(&plus, "probe", "miss"), "+stable is a different selection: {}", stderr(&plus));
    let plus_again = fcargo(&sb, &app, &["+stable", "build"], &[]);
    assert!(traced(&plus_again, "probe", "hit"), "{}", stderr(&plus_again));
    assert!(stable.join("debug/app").is_file());
    let pinned_again = fcargo(&sb, &app, &["build"], &[]);
    assert!(traced(&pinned_again, "probe", "hit"), "each selection keeps its own entry: {}", stderr(&pinned_again));
}

/// The proxy never changes while the binary behind it is replaced, which a key built from the proxy alone would get wrong.
struct FakeToolchain {
    proxy_rustc: PathBuf,
    proxy_cargo: PathBuf,
    toolchain: PathBuf,
}

impl FakeToolchain {
    fn new(sb: &Sandbox) -> FakeToolchain {
        let base = sb.root.join("fake-rustup");
        let toolchain = base.join("toolchains").join("fake");
        let proxy_rustc = base.join("bin").join("rustc");
        let proxy_cargo = base.join("bin").join("cargo");
        executable(&proxy_rustc, &format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", toolchain.join("bin/rustc").display()));
        executable(
            &proxy_cargo,
            "#!/bin/sh\ncase \"$1\" in\n  -V) echo \"cargo 9.9.9 (fake)\" ;;\n  locate-project) echo \"$(pwd -P)/Cargo.toml\" ;;\n  *) echo \"FAKE-BUILD id=$FRUST_RUSTC_ID target=$CARGO_TARGET_DIR\" ;;\nesac\n",
        );
        write(&toolchain.join("lib/rustlib/multirust-channel-manifest.toml"), "fake\n");
        let fake = FakeToolchain { proxy_rustc, proxy_cargo, toolchain };
        executable(&fake.toolchain.join("bin/rustc"), &fake.rustc_script("9.9.1"));
        fake
    }

    fn rustc_script(&self, release: &str) -> String {
        format!(
            "#!/bin/sh\ncase \"$1\" in\n  -vV) printf 'rustc {release} (fake)\\nbinary: rustc\\nhost: fake-host\\nrelease: {release}\\n' ;;\n  --print) echo \"{}\" ;;\n  *) exit 1 ;;\nesac\n",
            self.toolchain.display()
        )
    }

    fn update(&self, release: &str) {
        replace(&self.toolchain.join("bin/rustc"), &self.rustc_script(release));
    }

    fn envs(&self) -> Vec<(&'static str, &Path)> {
        vec![("RUSTC", &self.proxy_rustc), ("FRUST_RUSTC", &self.proxy_rustc), ("FRUST_CARGO", &self.proxy_cargo)]
    }
}

fn fake_build_id(out: &Output) -> String {
    let text = stdout(out);
    let line = text.lines().find(|l| l.starts_with("FAKE-BUILD")).unwrap_or_else(|| panic!("no fake build in {text}"));
    line.split_whitespace().find_map(|w| w.strip_prefix("id=")).unwrap().to_string()
}

#[test]
fn probe_cache_notices_a_toolchain_update_behind_an_unchanged_proxy() {
    let sb = Sandbox::new("startup-update");
    let app = sb.project("app", &[], "fn main() {}\n");
    let fake = FakeToolchain::new(&sb);
    let envs = fake.envs();
    // Results are not memoised while the toolchain's files may be mid-install; let them age past that window.
    thread::sleep(Duration::from_millis(2500));

    let first = fcargo(&sb, &app, &["build"], &envs);
    assert_success(&first);
    assert!(traced(&first, "probe", "miss"), "{}", stderr(&first));
    let second = fcargo(&sb, &app, &["build"], &envs);
    assert!(traced(&second, "probe", "hit"), "{}", stderr(&second));
    let old_id = fake_build_id(&second);
    assert_eq!(where_(&sb, &app, &envs).file_name().unwrap().to_string_lossy(), old_id);

    fake.update("9.9.2");
    let updated = fcargo(&sb, &app, &["build"], &envs);
    assert_success(&updated);
    assert!(traced(&updated, "probe", "miss"), "a replaced toolchain binary must re-probe: {}", stderr(&updated));
    let new_id = fake_build_id(&updated);
    assert_ne!(new_id, old_id, "the update changes the toolchain id");
    assert_eq!(where_(&sb, &app, &envs).file_name().unwrap().to_string_lossy(), new_id, "and matches a fresh probe");
}

#[test]
fn probe_cache_follows_rustup_toolchain_env() {
    let sb = Sandbox::new("startup-env");
    let app = sb.project("app", &[], "fn main() {}\n");
    assert_success(&fcargo(&sb, &app, &["build"], &[]));
    let stable = where_(&sb, &app, &[]);
    let out = sb
        .bin("fcargo")
        .arg("build")
        .env("FRUST_DEBUG", "1")
        .env("RUSTUP_TOOLCHAIN", "1.98.1")
        .current_dir(&app)
        .output()
        .unwrap();
    assert_success(&out);
    assert!(traced(&out, "probe", "miss"), "{}", stderr(&out));
    let pinned = stable.parent().unwrap().read_dir().unwrap().flatten().map(|e| e.path()).find(|p| p.is_dir() && *p != stable).expect("a second toolchain dir");
    assert!(pinned.join("debug/app").is_file(), "RUSTUP_TOOLCHAIN=1.98.1 builds into {}", pinned.display());
}

#[test]
fn locate_cache_follows_workspace_membership() {
    let sb = Sandbox::new("startup-member");
    let outer = sb.root.join("outer");
    let inner = outer.join("inner");
    write(&inner.join("Cargo.toml"), "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&inner.join("src/main.rs"), "fn main() {}\n");
    assert_success(&fcargo(&sb, &inner, &["build"], &[]));
    let alone = fcargo(&sb, &inner, &["build"], &[]);
    assert!(traced(&alone, "locate", "hit"), "{}", stderr(&alone));
    let alone_dir = where_(&sb, &inner, &[]);

    write(&outer.join("Cargo.toml"), "[workspace]\nmembers = [\"inner\"]\nresolver = \"2\"\n");
    let joined = fcargo(&sb, &inner, &["build"], &[]);
    assert_success(&joined);
    assert!(traced(&joined, "locate", "miss"), "{}", stderr(&joined));
    let ws_dir = where_(&sb, &inner, &[]);
    assert_ne!(ws_dir, alone_dir);
    assert!(ws_dir.join("debug/inner").is_file(), "built in the outer workspace's directory {}", ws_dir.display());
    assert!(traced(&fcargo(&sb, &inner, &["build"], &[]), "locate", "hit"));

    write(&inner.join("Cargo.toml"), "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n");
    write(&outer.join("Cargo.toml"), "[workspace]\nmembers = []\nexclude = [\"inner\"]\nresolver = \"2\"\n");
    let left = fcargo(&sb, &inner, &["build"], &[]);
    assert_success(&left);
    assert!(traced(&left, "locate", "miss"), "{}", stderr(&left));
    assert_eq!(where_(&sb, &inner, &[]), alone_dir);
    assert!(alone_dir.join("debug/inner").is_file());
}

#[test]
fn locate_cache_follows_a_moved_workspace_behind_a_symlink() {
    let sb = Sandbox::new("startup-move");
    let a = sb.project("a", &[], "fn main() {}\n");
    let b = sb.project("b", &[], "fn main() {}\n");
    let link = sb.root.join("current");
    std::os::unix::fs::symlink(&a, &link).unwrap();
    assert_success(&fcargo(&sb, &link, &["build"], &[]));
    assert!(traced(&fcargo(&sb, &link, &["build"], &[]), "locate", "hit"));
    let a_dir = where_(&sb, &a, &[]);

    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&b, &link).unwrap();
    let moved = fcargo(&sb, &link, &["build"], &[]);
    assert_success(&moved);
    assert!(traced(&moved, "locate", "miss"), "{}", stderr(&moved));
    let b_dir = where_(&sb, &b, &[]);
    assert_ne!(a_dir, b_dir);
    assert!(b_dir.join("debug/app").is_file(), "built for the new target of the link: {}", b_dir.display());
}

#[test]
fn broken_manifest_is_never_memoised() {
    let sb = Sandbox::new("startup-broken");
    let app = sb.project("app", &[], "fn main() {}\n");
    write(&app.join("Cargo.toml"), "[package\nname = \"app\"\n");
    for _ in 0..2 {
        let out = fcargo(&sb, &app, &["build"], &[]);
        assert!(!out.status.success(), "cargo reports the broken manifest");
        assert!(traced(&out, "locate", "miss"), "{}", stderr(&out));
    }
    assert!(!sb.home().join("locate").exists() || sb.home().join("locate").read_dir().unwrap().count() == 0);
}

#[test]
fn corrupt_cache_files_are_ignored_and_replaced() {
    let sb = Sandbox::new("startup-corrupt");
    let app = sb.project("app", &[], "fn main() {}\n");
    assert_success(&fcargo(&sb, &app, &["build"], &[]));
    let entries: Vec<PathBuf> = ["probe", "locate"]
        .iter()
        .flat_map(|d| sb.home().join(d).read_dir().unwrap().flatten().map(|e| e.path()).collect::<Vec<_>>())
        .collect();
    assert_eq!(entries.len(), 2, "{entries:?}");

    let garbage: [&[u8]; 3] = [b"", b"{\"format\":\"frust-probe-v1\",\"key\":", b"\xff\xfe not json at all"];
    for junk in garbage {
        for entry in &entries {
            fs::write(entry, junk).unwrap();
        }
        let out = fcargo(&sb, &app, &["build"], &[]);
        assert_success(&out);
        assert!(traced(&out, "probe", "miss") && traced(&out, "locate", "miss"), "{}", stderr(&out));
    }
    // Well-formed JSON with a forged answer but the wrong key is just as useless.
    let probe_entry = entries.iter().find(|p| p.parent().unwrap().ends_with("probe")).unwrap();
    let mut forged: serde_json::Value = serde_json::from_slice(&fs::read(probe_entry).unwrap()).unwrap();
    forged["key"] = serde_json::json!("0000");
    forged["probe"]["id"] = serde_json::json!("forged");
    fs::write(probe_entry, serde_json::to_vec(&forged).unwrap()).unwrap();
    let out = fcargo(&sb, &app, &["build"], &[]);
    assert_success(&out);
    assert!(traced(&out, "probe", "miss"), "{}", stderr(&out));

    let healed = fcargo(&sb, &app, &["build"], &[]);
    assert!(traced(&healed, "probe", "hit") && traced(&healed, "locate", "hit"), "entries rewritten: {}", stderr(&healed));
    assert!(where_(&sb, &app, &[]).join("debug/app").is_file());
    assert!(!sb.home().join("targets").read_dir().unwrap().flatten().any(|ws| ws.path().join("forged").exists()));
}
