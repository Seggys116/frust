//! Each test runs in a [`Sandbox`] with isolated frust state and a vendored registry, removed on drop.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);
const MIN_FREE: u64 = 4 * 1024 * 1024 * 1024;

pub struct Sandbox {
    pub root: PathBuf,
}

impl Sandbox {
    pub fn new(tag: &str) -> Sandbox {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("frust-test-{tag}-{}-{n}", std::process::id()));
        frust::fsx::require_free_space(&std::env::temp_dir(), MIN_FREE).expect("not enough free disk space for tests");
        fs::create_dir_all(&root).unwrap();
        let sandbox = Sandbox { root };
        fs::create_dir_all(sandbox.home()).unwrap();
        sandbox.write_cargo_config();
        sandbox
    }

    pub fn home(&self) -> PathBuf {
        self.root.join("frust-home")
    }

    pub fn vendor(&self) -> PathBuf {
        self.root.join("vendor")
    }

    pub fn cargo_home(&self) -> PathBuf {
        self.root.join("cargo-home")
    }

    /// Replace crates-io with a local directory source so tests run offline.
    fn write_cargo_config(&self) {
        fs::create_dir_all(self.cargo_home()).unwrap();
        fs::create_dir_all(self.vendor()).unwrap();
        let config = format!(
            "[source.crates-io]\nreplace-with = \"vendored\"\n\n[source.vendored]\ndirectory = \"{}\"\n\n[net]\noffline = true\n",
            self.vendor().display()
        );
        fs::write(self.cargo_home().join("config.toml"), config).unwrap();
    }

    pub fn vendor_crate(&self, name: &str, version: &str, source: &str) {
        let dir = self.vendor().join(format!("{name}-{version}"));
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n"),
        )
        .unwrap();
        fs::write(dir.join("src").join("lib.rs"), source).unwrap();
        fs::write(dir.join(".cargo-checksum.json"), "{\"files\":{},\"package\":null}").unwrap();
    }

    pub fn replace_vendor_source(&self, name: &str, version: &str, source: &str) {
        fs::write(self.vendor().join(format!("{name}-{version}")).join("src").join("lib.rs"), source).unwrap();
    }

    pub fn project(&self, dir: &str, deps: &[(&str, &str)], main_rs: &str) -> PathBuf {
        let path = self.root.join(dir);
        fs::create_dir_all(path.join("src")).unwrap();
        let mut manifest = String::from(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n[dependencies]\n",
        );
        for (name, version) in deps {
            manifest.push_str(&format!("{name} = \"={version}\"\n"));
        }
        fs::write(path.join("Cargo.toml"), manifest).unwrap();
        fs::write(path.join("src").join("main.rs"), main_rs).unwrap();
        path
    }

    pub fn bin(&self, name: &str) -> Command {
        let exe = match name {
            "frust" => env!("CARGO_BIN_EXE_frust"),
            "fcargo" => env!("CARGO_BIN_EXE_fcargo"),
            "frustc" => env!("CARGO_BIN_EXE_frustc"),
            "fclippy" => env!("CARGO_BIN_EXE_fclippy"),
            other => panic!("unknown binary {other}"),
        };
        let mut cmd = Command::new(exe);
        self.isolate(&mut cmd);
        cmd
    }

    pub fn cargo(&self) -> Command {
        let mut cmd = Command::new("cargo");
        self.isolate(&mut cmd);
        cmd.env_remove("RUSTC_WRAPPER");
        cmd
    }

    fn isolate(&self, cmd: &mut Command) {
        cmd.env("FRUST_HOME", self.home())
            .env("CARGO_HOME", self.cargo_home())
            .env("FRUST_RENDER", "off")
            .env("FRUST_LOCK_TIMEOUT_SECS", "120")
            .env("NO_COLOR", "1")
            .env("CARGO_TERM_COLOR", "never")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_INCREMENTAL")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .env_remove("FRUST_EVENTS");
    }

    pub fn cache_events(&self) -> Vec<serde_json::Value> {
        let text = fs::read_to_string(self.home().join("ledger.jsonl")).unwrap_or_default();
        text.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "cache")
            .collect()
    }

    pub fn count_cache(&self, result: &str, krate: &str) -> usize {
        self.cache_events().iter().filter(|e| e["result"] == result && e["crate"] == krate).count()
    }

    pub fn central_target(&self, project: &Path) -> PathBuf {
        let out = self.bin("frust").arg("where").current_dir(project).output().unwrap();
        assert!(out.status.success(), "frust where failed: {}", String::from_utf8_lossy(&out.stderr));
        PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if self.root.starts_with(std::env::temp_dir())
            && self.root.file_name().is_some_and(|n| n.to_string_lossy().starts_with("frust-test-"))
        {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub fn assert_success(out: &Output) {
    assert!(
        out.status.success(),
        "command failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        out.status,
        stdout(out),
        stderr(out)
    );
}

pub fn run_binary(target: &Path, profile: &str, name: &str) -> Output {
    Command::new(target.join(profile).join(name)).output().unwrap()
}

impl Sandbox {
    pub fn bin_path(&self, exe: &str) -> Command {
        let mut cmd = Command::new(exe);
        self.isolate(&mut cmd);
        cmd
    }
}
