//! Each test runs in a [`Sandbox`] with isolated frust state and a vendored registry, removed on drop.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
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
            "[source.crates-io]\nreplace-with = \"vendored\"\n\n[source.vendored]\ndirectory = '{}'\n\n[net]\noffline = true\n",
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
        settle(&dir.join("src").join("lib.rs"));
    }

    pub fn replace_vendor_source(&self, name: &str, version: &str, source: &str) {
        let path = self.vendor().join(format!("{name}-{version}")).join("src").join("lib.rs");
        fs::write(&path, source).unwrap();
        settle(&path);
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
        settle(&path.join("src").join("main.rs"));
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
            .env("FRUST_STORE_SYNC", "1")
            // A detached collector would run its own compaction while a test runs one, so the spawned commands collect inline.
            .env("FRUST_GC_SYNC", "1")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CI")
            .env_remove("CARGO_INCREMENTAL")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_BUILD_RUSTFLAGS")
            .env_remove("FRUST_EVENTS");
        #[cfg(windows)]
        {
            // Point the user profile at the sandbox, keeping rustup's real home so the toolchains stay found.
            let rustup = std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".rustup")));
            if let Some(rustup) = rustup {
                cmd.env("RUSTUP_HOME", rustup);
            }
            cmd.env("USERPROFILE", &self.root).env("HOME", &self.root);
        }
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

/// Waits until a just-written file passes frust's "file too new" rule, which would otherwise decline to cache a build started right away.
pub fn settle(path: &Path) {
    let stamp = frust::cache::hashing::Stamp::read(path).unwrap();
    while stamp.too_new_for(frust::cache::hashing::now_ns()) {
        std::thread::sleep(std::time::Duration::from_millis(10));
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
    Command::new(target.join(profile).join(exe(name))).output().unwrap()
}

/// A binary's file name on this platform.
pub fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// Creates a directory symlink; on Windows this needs developer mode or elevation, so callers skip on `Err`.
pub fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)
    }
}

/// Puts the child in its own process group (unix) or console process group (Windows) so `kill_tree` and `interrupt` reach only it.
pub fn own_group(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP)
    }
}

/// Kills the process and everything it started.
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: signalling the process group created by `own_group` for this child.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
    }
}

/// Sends the child an interrupt; the child exits 130 on both platforms.
pub fn interrupt(child: &Child) {
    #[cfg(unix)]
    {
        // SAFETY: signalling only the given child process.
        unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
        // CTRL_BREAK reaches a child started with `own_group` (frust treats it as Ctrl-C); without a shared console, taskkill without /F is the nearest graceful request.
        // SAFETY: plain FFI call with a process group id and no pointers.
        let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
        if sent == 0 {
            let _ = Command::new("taskkill").args(["/PID", &child.id().to_string()]).output();
        }
    }
}

/// A child that has already exited or exits at once, for tests that need a reaped process id.
pub fn spawn_exiting() -> Child {
    #[cfg(unix)]
    {
        Command::new("true").spawn().unwrap()
    }
    #[cfg(windows)]
    {
        Command::new("cmd").args(["/c", "exit", "0"]).spawn().unwrap()
    }
}

/// Writes an executable fake tool: the shell script on unix, the batch file on Windows (`path` gets a `.cmd` extension unless it has one); returns the written path.
pub fn write_fake_tool(path: &Path, unix_script: &str, windows_cmd: &str) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = windows_cmd;
        fs::write(path, unix_script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        path.to_path_buf()
    }
    #[cfg(windows)]
    {
        let _ = unix_script;
        let path = if path.extension().is_some_and(|e| e == "cmd") { path.to_path_buf() } else { path.with_extension("cmd") };
        fs::write(&path, windows_cmd).unwrap();
        path
    }
}

impl Sandbox {
    pub fn bin_path(&self, exe: &str) -> Command {
        let mut cmd = Command::new(exe);
        self.isolate(&mut cmd);
        cmd
    }
}

pub fn age(path: &Path, by: std::time::Duration) {
    #[cfg(unix)]
    // A read-only descriptor suffices for the owner, and works for directories too.
    let file = std::fs::File::open(path).unwrap();
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        // Windows needs write access to set times, and backup semantics to open a directory at all.
        std::fs::OpenOptions::new().write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path).unwrap()
    };
    file.set_modified(std::time::SystemTime::now() - by).unwrap();
}
