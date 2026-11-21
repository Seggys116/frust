//! Extra targets and components: `target add|remove|list`, `component add|remove|list`, `show`, `which`,
//! re-applying the extras after an update, and `frustup`. Everything runs against a fake release channel
//! in a temporary directory: a channel manifest, its checksum and real `.tar.gz` packages made with the
//! system tar, reached through `FRUST_DIST_BASE` (or `Dist::with_base`) with a `file://` URL.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use frust_cli::dist::{self, Change, Dist, Spec, EXTRAS_FILE};
use frust_cli::install;
use frust_core::{hex32, sha256};

const HOST: &str = "x86_64-unknown-linux-gnu";
const RELEASE: &str = "1.100.0";
const COMMIT: &str = "0ef1bc386059596dada8fb65b50425aab2ea637a";
const OFFICIAL: &str = "https://static.rust-lang.org/dist/2026-11-12";

struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("frust-dist-{label}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn digest(path: &Path) -> String {
    hex32(&sha256(&fs::read(path).unwrap()))
}

fn tar(args: &[&str], cwd: &Path) {
    let status = Command::new("tar").args(args).current_dir(cwd).status().unwrap();
    assert!(status.success(), "tar {args:?}");
}

/// A toolchain archive in the layout the installer reads, with the host std and a manifest that names the
/// host, the Rust release and its commit. `extra` is a list of files (relative path, content) added to it.
fn toolchain(dir: &Path, id: &str, extra: &[(&str, &str)]) -> (PathBuf, PathBuf) {
    let root = dir.join(format!("tree-{id}"));
    for relative in ["bin/rustc", "bin/cargo", "bin/rustdoc", &format!("lib/rustlib/{HOST}/lib/libstd-0123.rlib")] {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("host-{id}")).unwrap();
    }
    for (relative, content) in extra {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }
    // A manifest passed in `extra` replaces the default one.
    if !extra.iter().any(|(relative, _)| *relative == "manifest.json") {
        fs::write(
            root.join("manifest.json"),
            format!("{{\"id\":\"{id}\",\"host\":\"{HOST}\",\"rust_release\":\"{RELEASE}\",\"rust_commit\":\"{COMMIT}\"}}\n"),
        )
        .unwrap();
    }
    let archive = dir.join(format!("{id}.tar"));
    tar(&["-cf", archive.to_str().unwrap(), "-C", root.to_str().unwrap(), "."], dir);
    let sum = PathBuf::from(format!("{}.sha256", archive.display()));
    fs::write(&sum, format!("{}\n", digest(&archive))).unwrap();
    (archive, sum)
}

fn install_toolchain(prefix: &Path, scratch: &Scratch, id: &str, extra: &[(&str, &str)]) {
    let (archive, sum) = toolchain(&scratch.root, id, extra);
    install::install(prefix, &archive, &sum).unwrap();
}

fn version_dir(prefix: &Path, id: &str) -> PathBuf {
    prefix.join("versions").join(id)
}

/// One official-style package: `<name>/{components,rust-installer-version,<component>/{manifest.in,files}}`.
struct Package {
    name: String,
    component: String,
    manifest_in: String,
    files: Vec<(String, String)>,
}

impl Package {
    fn std(triple: &str) -> Self {
        let component = format!("rust-std-{triple}");
        let files = vec![
            (format!("lib/rustlib/{triple}/lib/libstd-abcd.rlib"), format!("std-{triple}")),
            (format!("lib/rustlib/{triple}/lib/libcore-abcd.rlib"), format!("core-{triple}")),
        ];
        let manifest_in = files.iter().map(|(path, _)| format!("file:{path}\n")).collect();
        Self { name: format!("rust-std-{RELEASE}-{triple}"), component, manifest_in, files }
    }

    fn docs() -> Self {
        Self {
            name: format!("rust-docs-{RELEASE}-{HOST}"),
            component: "rust-docs".to_string(),
            manifest_in: "dir:share/doc/rust/html\n".to_string(),
            files: vec![
                ("share/doc/rust/html/index.html".to_string(), "<html>docs</html>".to_string()),
                ("share/doc/rust/html/std/index.html".to_string(), "<html>std</html>".to_string()),
            ],
        }
    }

    fn rust_src() -> Self {
        Self {
            name: format!("rust-src-{RELEASE}"),
            component: "rust-src".to_string(),
            manifest_in: "dir:lib/rustlib/src/rust\n".to_string(),
            files: vec![
                ("lib/rustlib/src/rust/library/core/src/lib.rs".to_string(), "// core".to_string()),
                ("lib/rustlib/src/rust/library/std/src/lib.rs".to_string(), "// std".to_string()),
            ],
        }
    }

    fn llvm_tools() -> Self {
        let tool = format!("lib/rustlib/{HOST}/bin/llvm-objdump");
        Self {
            name: format!("llvm-tools-{RELEASE}-{HOST}"),
            component: "llvm-tools-preview".to_string(),
            manifest_in: format!("file:{tool}\n"),
            files: vec![(tool, "#!/bin/sh\necho objdump\n".to_string())],
        }
    }

    /// Writes `<dir>/2026-11-12/<name>.tar.gz` and returns it.
    fn build(&self, dir: &Path) -> PathBuf {
        let tree = dir.join(format!("build-{}", self.name));
        let top = tree.join(&self.name);
        let component = top.join(&self.component);
        fs::create_dir_all(&component).unwrap();
        fs::write(top.join("components"), format!("{}\n", self.component)).unwrap();
        fs::write(top.join("rust-installer-version"), "3\n").unwrap();
        fs::write(component.join("manifest.in"), &self.manifest_in).unwrap();
        for (relative, content) in &self.files {
            let path = component.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            if relative.starts_with("bin/") || relative.contains("/bin/") {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let dated = dir.join("2026-11-12");
        fs::create_dir_all(&dated).unwrap();
        let archive = dated.join(format!("{}.tar.gz", self.name));
        tar(&["-czf", archive.to_str().unwrap(), "-C", tree.to_str().unwrap(), &self.name], dir);
        fs::remove_dir_all(&tree).unwrap();
        archive
    }
}

/// A channel directory: `channel-rust-<release>.toml`, its `.sha256` and the packages, with the URLs the
/// manifest lists pointing at static.rust-lang.org (the override rewrites them).
struct Channel {
    dir: PathBuf,
    entries: Vec<(String, String, String, String)>,
}

impl Channel {
    fn new(dir: PathBuf) -> Self {
        fs::create_dir_all(&dir).unwrap();
        Self { dir, entries: Vec::new() }
    }

    fn add(&mut self, pkg: &str, target: &str, package: &Package) {
        let archive = package.build(&self.dir);
        self.entries.push((pkg.to_string(), target.to_string(), format!("{OFFICIAL}/{}.tar.gz", package.name), digest(&archive)));
    }

    /// Lists the package with a hash that does not match the archive.
    fn add_with_hash(&mut self, pkg: &str, target: &str, package: &Package, hash: &str) {
        package.build(&self.dir);
        self.entries.push((pkg.to_string(), target.to_string(), format!("{OFFICIAL}/{}.tar.gz", package.name), hash.to_string()));
    }

    fn write(&self) -> String {
        let mut text = String::from("manifest-version = \"2\"\ndate = \"2026-11-12\"\n\n");
        for pkg in ["rust-std", "rust-docs", "rust-analyzer-preview", "rust-src", "llvm-tools-preview"] {
            text.push_str(&format!("[pkg.{pkg}]\nversion = \"{RELEASE} (0ef1bc386 2026-11-09)\"\ngit_commit_hash = \"{COMMIT}\"\n\n"));
        }
        for (pkg, target, url, hash) in &self.entries {
            let target = if target == "*" { "\"*\"".to_string() } else { target.clone() };
            text.push_str(&format!("[pkg.{pkg}.target.{target}]\navailable = true\nurl = \"{url}\"\nhash = \"{hash}\"\nxz_url = \"{url}\"\nxz_hash = \"{hash}\"\n\n"));
        }
        text.push_str("[pkg.rust-std.target.s390x-unknown-linux-gnu]\navailable = false\nurl = \"\"\nhash = \"\"\n\n");
        let path = self.dir.join(format!("channel-rust-{RELEASE}.toml"));
        fs::write(&path, &text).unwrap();
        fs::write(self.dir.join(format!("channel-rust-{RELEASE}.toml.sha256")), format!("{}  channel-rust-{RELEASE}.toml\n", digest(&path))).unwrap();
        text
    }

    fn dist(&self) -> Dist {
        Dist::with_base(&format!("file://{}", self.dir.display()))
    }
}

/// A prefix with one installed toolchain and a channel offering the usual packages.
struct World {
    scratch: Scratch,
    prefix: PathBuf,
    channel: Channel,
}

fn world(label: &str) -> World {
    let scratch = Scratch::new(label);
    let prefix = scratch.path("prefix");
    install_toolchain(&prefix, &scratch, "frust-a", &[]);
    let mut channel = Channel::new(scratch.path("dist"));
    for triple in ["wasm32-wasip1", "wasm32-unknown-unknown", "aarch64-unknown-linux-gnu"] {
        channel.add("rust-std", triple, &Package::std(triple));
    }
    channel.add("rust-std", HOST, &Package::std(HOST));
    channel.add("rust-docs", HOST, &Package::docs());
    channel.add("rust-src", "*", &Package::rust_src());
    channel.add("llvm-tools-preview", HOST, &Package::llvm_tools());
    channel.write();
    World { scratch, prefix, channel }
}

fn target(name: &str) -> Spec {
    Spec::target(name).unwrap()
}

fn component(name: &str) -> Spec {
    Spec::component(name).unwrap()
}

fn only_change(batch: dist::Batch) -> Change {
    assert!(batch.failed.is_empty(), "{:?}", batch.failed);
    assert_eq!(batch.done.len(), 1);
    batch.done[0].1
}

fn only_failure(batch: dist::Batch) -> String {
    assert!(batch.done.is_empty(), "{:?}", batch.done);
    assert_eq!(batch.failed.len(), 1);
    batch.failed[0].1.message.clone()
}

/// A dist that cannot be reached: proves an operation did not need the network.
fn unreachable_dist(scratch: &Scratch) -> Dist {
    Dist::with_base(&format!("file://{}", scratch.path("nowhere").display()))
}

fn leftovers(prefix: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for dir in [prefix.to_path_buf(), prefix.join("versions")] {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".partial") || name.starts_with(".state.json.next") || name.starts_with(".current-next") {
                found.push(name);
            }
        }
    }
    found
}

#[test]
fn a_target_is_installed_from_the_channel_and_recorded() {
    let w = world("add-target");
    let batch = dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap();
    assert_eq!(only_change(batch), Change::Installed);
    let dir = version_dir(&w.prefix, "frust-a");
    let lib = dir.join("lib/rustlib/wasm32-wasip1/lib");
    assert_eq!(fs::read_to_string(lib.join("libstd-abcd.rlib")).unwrap(), "std-wasm32-wasip1");
    assert_eq!(fs::read_to_string(lib.join("libcore-abcd.rlib")).unwrap(), "core-wasm32-wasip1");
    let record = fs::read_to_string(dir.join("lib/rustlib/manifest-rust-std-wasm32-wasip1")).unwrap();
    assert!(record.contains("file:lib/rustlib/wasm32-wasip1/lib/libstd-abcd.rlib"), "{record}");
    assert_eq!(dist::read_extras(&w.prefix), vec![target("wasm32-wasip1")]);
    // The host std fRust built is untouched.
    assert_eq!(fs::read_to_string(dir.join(format!("lib/rustlib/{HOST}/lib/libstd-0123.rlib"))).unwrap(), "host-frust-a");
    assert!(leftovers(&w.prefix).is_empty(), "{:?}", leftovers(&w.prefix));
}

#[test]
fn adding_twice_is_idempotent_and_needs_no_network_the_second_time() {
    let w = world("idempotent");
    assert_eq!(only_change(dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap()), Change::Installed);
    let record = fs::read(version_dir(&w.prefix, "frust-a").join("lib/rustlib/manifest-rust-std-wasm32-wasip1")).unwrap();
    let again = dist::add(&w.prefix, &unreachable_dist(&w.scratch), &[target("wasm32-wasip1")]).unwrap();
    assert_eq!(only_change(again), Change::AlreadyInstalled);
    assert_eq!(dist::read_extras(&w.prefix), vec![target("wasm32-wasip1")]);
    assert_eq!(fs::read(version_dir(&w.prefix, "frust-a").join("lib/rustlib/manifest-rust-std-wasm32-wasip1")).unwrap(), record);
}

#[test]
fn several_items_are_independent() {
    let w = world("several");
    let specs = [target("wasm32-wasip1"), target("no-such-target"), target("s390x-unknown-linux-gnu"), target("wasm32-unknown-unknown")];
    let batch = dist::add(&w.prefix, &w.channel.dist(), &specs).unwrap();
    assert_eq!(batch.done.iter().map(|(spec, _)| spec.clone()).collect::<Vec<_>>(), vec![target("wasm32-wasip1"), target("wasm32-unknown-unknown")]);
    assert_eq!(batch.failed.len(), 2);
    assert!(batch.failed[0].1.message.contains("not available for Rust 1.100.0"), "{}", batch.failed[0].1.message);
    assert!(batch.failed[1].1.message.contains("not available"), "{}", batch.failed[1].1.message);
    assert_eq!(dist::read_extras(&w.prefix), vec![target("wasm32-wasip1"), target("wasm32-unknown-unknown")]);
}

#[test]
fn removing_a_target_deletes_its_files_and_forgets_it() {
    let w = world("remove-target");
    dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1"), target("wasm32-unknown-unknown")]).unwrap();
    let dir = version_dir(&w.prefix, "frust-a");
    let removed = dist::remove(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap();
    assert_eq!(only_change(removed), Change::Removed);
    assert!(!dir.join("lib/rustlib/wasm32-wasip1").exists(), "the emptied target directory is pruned");
    assert!(!dir.join("lib/rustlib/manifest-rust-std-wasm32-wasip1").exists());
    assert!(dir.join("lib/rustlib/wasm32-unknown-unknown/lib/libstd-abcd.rlib").is_file());
    assert!(dir.join(format!("lib/rustlib/{HOST}/lib/libstd-0123.rlib")).is_file());
    assert!(dir.join("lib/rustlib").is_dir() && dir.join("bin/rustc").is_file());
    assert_eq!(dist::read_extras(&w.prefix), vec![target("wasm32-unknown-unknown")]);
    // Removing it again is not an error and needs no network.
    let again = dist::remove(&w.prefix, &unreachable_dist(&w.scratch), &[target("wasm32-wasip1")]).unwrap();
    assert_eq!(only_change(again), Change::NotInstalled);
    assert!(leftovers(&w.prefix).is_empty());
}

#[test]
fn the_host_target_is_never_replaced_or_removed() {
    let w = world("host");
    let dir = version_dir(&w.prefix, "frust-a");
    let host_lib = dir.join(format!("lib/rustlib/{HOST}/lib/libstd-0123.rlib"));
    // Present: adding it changes nothing, even though the channel has an official one.
    let added = dist::add(&w.prefix, &w.channel.dist(), &[target(HOST)]).unwrap();
    assert_eq!(only_change(added), Change::AlreadyInstalled);
    assert_eq!(fs::read_to_string(&host_lib).unwrap(), "host-frust-a");
    assert!(!dir.join(format!("lib/rustlib/{HOST}/lib/libstd-abcd.rlib")).exists());
    // Removing it is refused.
    let message = only_failure(dist::remove(&w.prefix, &w.channel.dist(), &[target(HOST)]).unwrap());
    assert!(message.contains("host target") && message.contains("cannot be removed"), "{message}");
    assert_eq!(fs::read_to_string(&host_lib).unwrap(), "host-frust-a");
    // Missing: the official one fills the gap.
    fs::remove_file(&host_lib).unwrap();
    let added = dist::add(&w.prefix, &w.channel.dist(), &[target(HOST)]).unwrap();
    assert_eq!(only_change(added), Change::Installed);
    assert!(dir.join(format!("lib/rustlib/{HOST}/lib/libstd-abcd.rlib")).is_file());
}

#[test]
fn a_package_that_does_not_match_its_listed_hash_is_rejected() {
    let mut w = world("bad-hash");
    let bad = Package::std("riscv64gc-unknown-linux-gnu");
    w.channel.add_with_hash("rust-std", "riscv64gc-unknown-linux-gnu", &bad, &"0".repeat(64));
    w.channel.write();
    let message = only_failure(dist::add(&w.prefix, &w.channel.dist(), &[target("riscv64gc-unknown-linux-gnu")]).unwrap());
    assert!(message.contains("checksum mismatch") && message.contains("before extraction"), "{message}");
    let dir = version_dir(&w.prefix, "frust-a");
    assert!(!dir.join("lib/rustlib/riscv64gc-unknown-linux-gnu").exists());
    assert!(!dir.join("lib/rustlib/manifest-rust-std-riscv64gc-unknown-linux-gnu").exists());
    assert!(dist::read_extras(&w.prefix).is_empty());
    assert!(leftovers(&w.prefix).is_empty(), "{:?}", leftovers(&w.prefix));
}

#[test]
fn a_tampered_package_is_rejected() {
    let w = world("tampered");
    let archive = w.scratch.path("dist/2026-11-12").join(format!("rust-std-{RELEASE}-wasm32-wasip1.tar.gz"));
    let mut bytes = fs::read(&archive).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    fs::write(&archive, bytes).unwrap();
    let message = only_failure(dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap());
    assert!(message.contains("checksum mismatch"), "{message}");
    assert!(!version_dir(&w.prefix, "frust-a").join("lib/rustlib/wasm32-wasip1").exists());
}

#[test]
fn a_channel_manifest_that_does_not_match_its_checksum_is_rejected() {
    let w = world("bad-channel");
    let manifest = w.scratch.path(&format!("dist/channel-rust-{RELEASE}.toml"));
    let mut text = fs::read_to_string(&manifest).unwrap();
    // Point a package at a different archive: a valid-looking edit that the checksum must catch.
    text.push_str("\n# tampered\n");
    fs::write(&manifest, text).unwrap();
    let message = only_failure(dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap());
    assert!(message.contains("checksum mismatch for the channel manifest"), "{message}");
    assert!(!version_dir(&w.prefix, "frust-a").join("lib/rustlib/wasm32-wasip1").exists());
    assert!(dist::read_extras(&w.prefix).is_empty());

    // A checksum file that is not a digest is rejected as well.
    fs::write(w.scratch.path(&format!("dist/channel-rust-{RELEASE}.toml.sha256")), "not a digest\n").unwrap();
    let message = only_failure(dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap());
    assert!(message.contains("sha256"), "{message}");
}

/// A raw ustar entry, for archives that the system tar will not make.
fn raw_entry(name: &str, kind: u8, link: &str, data: &[u8]) -> Vec<u8> {
    fn octal(field: &mut [u8], value: u64) {
        let text = format!("{value:0width$o}", width = field.len() - 1);
        field[..text.len()].copy_from_slice(text.as_bytes());
    }
    let mut block = [0u8; 512];
    block[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut block[100..108], 0o755);
    octal(&mut block[124..136], data.len() as u64);
    block[148..156].fill(b' ');
    block[156] = kind;
    block[157..157 + link.len()].copy_from_slice(link.as_bytes());
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");
    let sum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
    block[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut out = block.to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out
}

/// A hostile archive member: path, type flag, link target, content.
type RawCase = (&'static str, u8, &'static str, &'static str);

/// Replaces the package of `triple` in the channel with a hostile archive made of `entries`.
fn hostile_std(w: &mut World, triple: &str, entries: &[RawCase]) {
    let name = format!("rust-std-{RELEASE}-{triple}");
    let mut tar_bytes = Vec::new();
    for (path, kind, link, data) in entries {
        tar_bytes.extend(raw_entry(path, *kind, link, data.as_bytes()));
    }
    tar_bytes.extend([0u8; 1024]);
    let plain = w.scratch.path(&format!("{name}.tar"));
    fs::write(&plain, tar_bytes).unwrap();
    assert!(Command::new("gzip").arg("-f").arg(&plain).status().unwrap().success());
    let archive = w.channel.dir.join("2026-11-12").join(format!("{name}.tar.gz"));
    fs::create_dir_all(archive.parent().unwrap()).unwrap();
    fs::rename(w.scratch.path(&format!("{name}.tar.gz")), &archive).unwrap();
    w.channel.entries.push(("rust-std".to_string(), triple.to_string(), format!("{OFFICIAL}/{name}.tar.gz"), digest(&archive)));
    w.channel.write();
}

#[test]
fn archives_that_climb_out_of_the_tree_are_rejected_before_extraction() {
    let mut w = world("traversal");
    let cases: [(&str, [RawCase; 1]); 3] = [
        ("t-dotdot", [("../escape.txt", b'0', "", "x")]),
        ("t-absolute", [("/tmp/frust-escape.txt", b'0', "", "x")]),
        ("t-symlink", [("pkg/link", b'2', "../../../../etc", "")]),
    ];
    for (triple, entries) in cases {
        hostile_std(&mut w, triple, &entries);
        let message = only_failure(dist::add(&w.prefix, &w.channel.dist(), &[target(triple)]).unwrap());
        assert!(message.contains("rejected"), "{triple}: {message}");
    }
    assert!(!w.scratch.path("escape.txt").exists() && !Path::new("/tmp/frust-escape.txt").exists());
    assert!(!version_dir(&w.prefix, "frust-a").parent().unwrap().join("escape.txt").exists());
    assert!(leftovers(&w.prefix).is_empty());
}

#[test]
fn a_component_manifest_that_names_paths_outside_the_toolchain_is_rejected() {
    for (label, listed) in [("up", "file:../../escape\n"), ("abs", "file:/tmp/frust-listed-escape\n"), ("mix", "file:lib/rustlib/x/lib/libstd-1.rlib\ndir:lib/../../..\n")] {
        let mut w = world(label);
        let mut package = Package::std("bad-listing");
        package.manifest_in = listed.to_string();
        w.channel.add("rust-std", "bad-listing", &package);
        w.channel.write();
        let message = only_failure(dist::add(&w.prefix, &w.channel.dist(), &[target("bad-listing")]).unwrap());
        assert!(message.contains("unsafe path"), "{label}: {message}");
        assert!(!Path::new("/tmp/frust-listed-escape").exists());
        assert!(leftovers(&w.prefix).is_empty());
    }
}

#[test]
fn symlinks_inside_a_package_are_not_installed() {
    let mut w = world("symlink");
    let mut package = Package::std("linky");
    package.manifest_in = "dir:lib/rustlib/linky\n".to_string();
    package.files = vec![("lib/rustlib/linky/lib/libstd-1.rlib".to_string(), "x".to_string())];
    // Build the package by hand so that a symlink that stays inside the tree can be added to it.
    let archive = package.build(&w.channel.dir);
    let unpacked = w.scratch.path("repack");
    fs::create_dir_all(&unpacked).unwrap();
    tar(&["-xzf", archive.to_str().unwrap(), "-C", unpacked.to_str().unwrap()], &w.scratch.root);
    let inside = unpacked.join(&package.name).join(&package.component).join("lib/rustlib/linky");
    std::os::unix::fs::symlink("lib/libstd-1.rlib", inside.join("alias")).unwrap();
    tar(&["-czf", archive.to_str().unwrap(), "-C", unpacked.to_str().unwrap(), &package.name], &w.scratch.root);
    w.channel.entries.push(("rust-std".to_string(), "linky".to_string(), format!("{OFFICIAL}/{}.tar.gz", package.name), digest(&archive)));
    w.channel.write();
    let message = only_failure(dist::add(&w.prefix, &w.channel.dist(), &[target("linky")]).unwrap());
    assert!(message.contains("link or special file"), "{message}");
    assert!(!version_dir(&w.prefix, "frust-a").join("lib/rustlib/linky").exists());
}

#[test]
fn a_package_never_overwrites_existing_files() {
    let w = world("conflict");
    let dir = version_dir(&w.prefix, "frust-a");
    // A docs directory that frust-install did not put there, without an index page.
    fs::create_dir_all(dir.join("share/doc/rust/html")).unwrap();
    fs::write(dir.join("share/doc/rust/html/mine.txt"), "keep me").unwrap();
    let message = only_failure(dist::add(&w.prefix, &w.channel.dist(), &[component("rust-docs")]).unwrap());
    assert!(message.contains("already exists") && message.contains("refusing to replace"), "{message}");
    assert_eq!(fs::read_to_string(dir.join("share/doc/rust/html/mine.txt")).unwrap(), "keep me");
    assert!(!dir.join("share/doc/rust/html/index.html").exists());
    assert!(dist::read_extras(&w.prefix).is_empty());
}

#[test]
fn a_commit_that_fails_part_way_puts_everything_back() {
    let mut w = world("rollback");
    let mut package = Package::std("halfway");
    // The second entry lives inside the first, which is a directory listed as a file: it cannot be moved
    // once the first has been, so the commit fails after one entry is already in place.
    package.manifest_in = "file:lib/rustlib/halfway\nfile:lib/rustlib/halfway/lib/libstd-1.rlib\n".to_string();
    package.files = vec![("lib/rustlib/halfway/lib/libstd-1.rlib".to_string(), "x".to_string())];
    w.channel.add("rust-std", "halfway", &package);
    w.channel.write();
    let message = only_failure(dist::add(&w.prefix, &w.channel.dist(), &[target("halfway")]).unwrap());
    assert!(message.contains("could not install"), "{message}");
    let dir = version_dir(&w.prefix, "frust-a");
    assert!(!dir.join("lib/rustlib/halfway").exists());
    assert!(!dir.join("lib/rustlib/manifest-rust-std-halfway").exists());
    assert!(dist::read_extras(&w.prefix).is_empty());
    assert!(leftovers(&w.prefix).is_empty());
}

#[test]
fn components_are_added_and_removed() {
    let w = world("components");
    let dir = version_dir(&w.prefix, "frust-a");
    let specs = [component("rust-docs"), component("rust-src"), component("llvm-tools")];
    let batch = dist::add(&w.prefix, &w.channel.dist(), &specs).unwrap();
    assert!(batch.failed.is_empty(), "{:?}", batch.failed);
    assert!(batch.done.iter().all(|(_, change)| *change == Change::Installed));
    assert_eq!(fs::read_to_string(dir.join("share/doc/rust/html/std/index.html")).unwrap(), "<html>std</html>");
    assert_eq!(fs::read_to_string(dir.join("lib/rustlib/src/rust/library/core/src/lib.rs")).unwrap(), "// core");
    let objdump = dir.join(format!("lib/rustlib/{HOST}/bin/llvm-objdump"));
    assert_eq!(fs::metadata(&objdump).unwrap().permissions().mode() & 0o111, 0o111, "the tool stays executable");
    assert_eq!(dist::read_extras(&w.prefix), specs);
    // The host target directory gained a bin directory but its std is untouched.
    assert_eq!(fs::read_to_string(dir.join(format!("lib/rustlib/{HOST}/lib/libstd-0123.rlib"))).unwrap(), "host-frust-a");

    let removed = dist::remove(&w.prefix, &w.channel.dist(), &specs).unwrap();
    assert!(removed.failed.is_empty() && removed.done.iter().all(|(_, change)| *change == Change::Removed), "{:?}", removed.failed);
    assert!(!dir.join("share/doc/rust").exists() && !dir.join("lib/rustlib/src").exists() && !objdump.exists());
    assert!(!dir.join(format!("lib/rustlib/{HOST}/bin")).exists(), "the emptied bin directory is pruned");
    assert!(dir.join(format!("lib/rustlib/{HOST}/lib/libstd-0123.rlib")).is_file());
    assert!(dist::read_extras(&w.prefix).is_empty());
    assert!(leftovers(&w.prefix).is_empty());
}

#[test]
fn only_official_components_can_be_named() {
    for built_in in ["clippy", "rustfmt", "cargo", "rustc", "rustdoc", "rust-analyzer", "rust-analyzer-preview", "clippy-preview"] {
        let err = Spec::component(built_in).unwrap_err();
        assert!(err.message.contains("built into the fRust toolchain archive"), "{built_in}: {}", err.message);
    }
    assert!(Spec::component("rust-std").unwrap_err().message.contains("target add"));
    assert!(Spec::component("miri").unwrap_err().message.contains("unknown component"));
    assert_eq!(Spec::component("llvm-tools-preview").unwrap(), component("llvm-tools"));
}

#[test]
fn rust_analyzer_cannot_be_added_and_is_listed_as_built_in() {
    let w = world("analyzer");
    // An old extras file from a version that could add it is ignored, not an error.
    fs::write(w.prefix.join(EXTRAS_FILE), "component rust-analyzer\ntarget wasm32-wasip1\n").unwrap();
    assert_eq!(dist::read_extras(&w.prefix), vec![target("wasm32-wasip1")]);
    let out = installer_at(&frust_install(), &w.prefix, Some(&w.channel)).args(["component", "add", "rust-analyzer"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let (_, stderr) = text(&out);
    assert!(stderr.contains("rust-analyzer is built into the fRust toolchain archive"), "{stderr}");
}

#[test]
fn adding_without_a_toolchain_or_a_channel_is_a_clear_error() {
    let scratch = Scratch::new("empty");
    let err = dist::add(&scratch.path("nothing"), &Dist::official(), &[target("wasm32-wasip1")]).unwrap_err();
    assert!(err.message.contains("no toolchain is installed"), "{}", err.message);

    let prefix = scratch.path("prefix");
    install_toolchain(&prefix, &scratch, "frust-a", &[]);
    let message = only_failure(dist::add(&prefix, &unreachable_dist(&scratch), &[target("wasm32-wasip1")]).unwrap());
    assert!(message.contains("could not download"), "{message}");
}

#[test]
fn extras_are_added_again_after_an_update_unless_the_new_archive_has_them() {
    let w = world("persist");
    dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1"), target("wasm32-unknown-unknown"), component("rust-docs")]).unwrap();
    // The new archive bundles wasm32-unknown-unknown itself, with its own bytes.
    let (archive, sum) = toolchain(&w.scratch.root, "frust-b", &[("lib/rustlib/wasm32-unknown-unknown/lib/libstd-9999.rlib", "bundled")]);
    install::install(&w.prefix, &archive, &sum).unwrap();
    let dir = version_dir(&w.prefix, "frust-b");
    assert!(!dir.join("lib/rustlib/wasm32-wasip1").exists(), "a new version starts without the extras");

    let batch = dist::reapply(&w.prefix, &w.channel.dist()).unwrap();
    assert!(batch.failed.is_empty(), "{:?}", batch.failed);
    let change = |spec: &Spec| batch.done.iter().find(|(done, _)| done == spec).map(|(_, change)| *change);
    assert_eq!(change(&target("wasm32-wasip1")), Some(Change::Installed));
    assert_eq!(change(&target("wasm32-unknown-unknown")), Some(Change::AlreadyInstalled));
    assert_eq!(change(&component("rust-docs")), Some(Change::Installed));
    assert_eq!(fs::read_to_string(dir.join("lib/rustlib/wasm32-wasip1/lib/libstd-abcd.rlib")).unwrap(), "std-wasm32-wasip1");
    // The bundled one was kept as shipped, not replaced by the official package.
    assert_eq!(fs::read_to_string(dir.join("lib/rustlib/wasm32-unknown-unknown/lib/libstd-9999.rlib")).unwrap(), "bundled");
    assert!(!dir.join("lib/rustlib/wasm32-unknown-unknown/lib/libstd-abcd.rlib").exists());
    assert!(dir.join("share/doc/rust/html/index.html").is_file());
    assert_eq!(dist::read_extras(&w.prefix).len(), 3);
    // The previous version keeps what it had, so a rollback still has the targets.
    assert!(version_dir(&w.prefix, "frust-a").join("lib/rustlib/wasm32-wasip1/lib/libstd-abcd.rlib").is_file());
}

#[test]
fn a_failed_reapply_is_reported_per_item_and_does_not_fail() {
    let w = world("reapply-fail");
    dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap();
    install_toolchain(&w.prefix, &w.scratch, "frust-b", &[]);
    let batch = dist::reapply(&w.prefix, &unreachable_dist(&w.scratch)).unwrap();
    assert!(batch.done.is_empty());
    assert_eq!(batch.failed.len(), 1);
    assert_eq!(batch.failed[0].0, target("wasm32-wasip1"));
    // The wish stays recorded, so the next update tries again.
    assert_eq!(dist::read_extras(&w.prefix), vec![target("wasm32-wasip1")]);
    let batch = dist::reapply(&w.prefix, &w.channel.dist()).unwrap();
    assert_eq!(only_change(batch), Change::Installed);
}

#[test]
fn reapplying_with_nothing_recorded_does_nothing() {
    let w = world("reapply-empty");
    let batch = dist::reapply(&w.prefix, &unreachable_dist(&w.scratch)).unwrap();
    assert!(batch.done.is_empty() && batch.failed.is_empty());
    assert!(!dist::has_extras(&w.prefix));
}

#[test]
fn the_extras_file_survives_garbage_lines() {
    let w = world("garbage");
    fs::write(
        w.prefix.join(EXTRAS_FILE),
        "# comment\ntarget wasm32-wasip1\ntarget ../../etc\ncomponent clippy\ncomponent rust-docs\nnonsense\ntarget wasm32-wasip1\n",
    )
    .unwrap();
    assert_eq!(dist::read_extras(&w.prefix), vec![target("wasm32-wasip1"), component("rust-docs")]);
}

#[test]
fn listings_show_installed_and_available_targets() {
    let w = world("list");
    dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap();
    let list = dist::list_targets(&w.prefix, &w.channel.dist(), true).unwrap();
    assert!(list.unavailable.is_none());
    let status: Vec<(String, bool)> = list.targets.iter().map(|target| (target.name.clone(), target.installed)).collect();
    assert_eq!(
        status,
        vec![
            ("aarch64-unknown-linux-gnu".to_string(), false),
            ("wasm32-unknown-unknown".to_string(), false),
            ("wasm32-wasip1".to_string(), true),
            (HOST.to_string(), true),
        ]
    );
    // Offline, the installed ones are still listed and the reason is given.
    let offline = dist::list_targets(&w.prefix, &unreachable_dist(&w.scratch), true).unwrap();
    assert!(offline.unavailable.is_some());
    assert_eq!(offline.targets.iter().map(|target| target.name.as_str()).collect::<Vec<_>>(), vec!["wasm32-wasip1", HOST]);
    let installed_only = dist::list_targets(&w.prefix, &unreachable_dist(&w.scratch), false).unwrap();
    assert!(installed_only.unavailable.is_none());
    assert_eq!(installed_only.targets.len(), 2);
}

#[test]
fn component_listing_covers_built_in_and_official_components() {
    let scratch = Scratch::new("components-list");
    let prefix = scratch.path("prefix");
    install_toolchain(&prefix, &scratch, "frust-a", &[("bin/cargo-clippy", "c"), ("bin/rustfmt", "f"), ("bin/rust-analyzer", "a")]);
    let mut channel = Channel::new(scratch.path("dist"));
    channel.add("rust-docs", HOST, &Package::docs());
    channel.write();
    let list = dist::list_components(&prefix, &channel.dist(), true).unwrap();
    let status: Vec<(&str, bool)> = list.components.iter().map(|component| (component.name.as_str(), component.installed)).collect();
    assert_eq!(status, vec![
            ("cargo", true),
            ("clippy", true),
            ("rust-analyzer", true),
            ("rust-docs", false),
            ("rust-std", true),
            ("rustc", true),
            ("rustdoc", true),
            ("rustfmt", true)
        ]
    );
    // Without the channel every addable component is listed, as installed or not.
    let offline = dist::list_components(&prefix, &unreachable_dist(&scratch), false).unwrap();
    assert!(offline.components.iter().any(|component| component.name == "rust-src" && !component.installed));
    assert!(offline.components.iter().any(|component| component.name == "llvm-tools" && !component.installed));
}

#[test]
fn show_reports_the_installation() {
    let w = world("show");
    dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap();
    install_toolchain(&w.prefix, &w.scratch, "frust-b", &[]);
    let text = dist::show(&w.prefix).unwrap();
    assert!(text.contains("current: frust-b\n") && text.contains("previous: frust-a\n"), "{text}");
    assert!(text.contains(&format!("host: {HOST}\n")) && text.contains(&format!("rust release: {RELEASE}\n")), "{text}");
    assert!(text.contains(&format!("targets:\n  {HOST}\n")), "{text}");
    assert!(!text.contains("wasm32-wasip1"), "the new version does not have the target yet: {text}");
    assert!(dist::which(&w.prefix, "../etc/passwd").is_err());
    assert!(dist::which(&w.prefix, "missing-tool").is_err());
    assert_eq!(dist::which(&w.prefix, "rustc").unwrap(), w.prefix.join("current/bin/rustc"));
}

#[test]
fn host_and_release_come_from_rustc_when_the_manifest_lacks_them() {
    let scratch = Scratch::new("rustc-vv");
    let root = scratch.path("tree");
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(
        root.join("bin/rustc"),
        format!("#!/bin/sh\necho 'rustc {RELEASE} (0ef1bc386 2026-11-09)'\necho 'binary: rustc'\necho 'commit-hash: {COMMIT}'\necho 'host: {HOST}'\necho 'release: {RELEASE}'\n"),
    )
    .unwrap();
    fs::set_permissions(root.join("bin/rustc"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("bin/cargo"), "c").unwrap();
    fs::write(root.join("bin/rustdoc"), "d").unwrap();
    fs::write(root.join("manifest.json"), "{\"id\":\"frust-old\"}\n").unwrap();
    let archive = scratch.path("old.tar");
    tar(&["-cf", archive.to_str().unwrap(), "-C", root.to_str().unwrap(), "."], &scratch.root);
    let sum = PathBuf::from(format!("{}.sha256", archive.display()));
    fs::write(&sum, format!("{}\n", digest(&archive))).unwrap();
    let prefix = scratch.path("prefix");
    install::install(&prefix, &archive, &sum).unwrap();

    let mut channel = Channel::new(scratch.path("dist"));
    channel.add("rust-std", "wasm32-wasip1", &Package::std("wasm32-wasip1"));
    channel.write();
    let batch = dist::add(&prefix, &channel.dist(), &[target("wasm32-wasip1")]).unwrap();
    assert_eq!(only_change(batch), Change::Installed);
}

#[test]
fn a_package_built_from_another_commit_is_refused() {
    let scratch = Scratch::new("commit");
    let prefix = scratch.path("prefix");
    install_toolchain(&prefix, &scratch, "frust-a", &[]);
    let mut channel = Channel::new(scratch.path("dist"));
    channel.add("rust-std", "wasm32-wasip1", &Package::std("wasm32-wasip1"));
    let text = channel.write().replace(COMMIT, "1111111111111111111111111111111111111111");
    fs::write(channel.dir.join(format!("channel-rust-{RELEASE}.toml")), text).unwrap();
    let manifest = channel.dir.join(format!("channel-rust-{RELEASE}.toml"));
    fs::write(channel.dir.join(format!("channel-rust-{RELEASE}.toml.sha256")), format!("{}\n", digest(&manifest))).unwrap();
    let message = only_failure(dist::add(&prefix, &channel.dist(), &[target("wasm32-wasip1")]).unwrap());
    assert!(message.contains("refusing to mix"), "{message}");
}

#[test]
fn uninstall_forgets_the_extras() {
    let w = world("uninstall");
    dist::add(&w.prefix, &w.channel.dist(), &[target("wasm32-wasip1")]).unwrap();
    assert!(w.prefix.join(EXTRAS_FILE).is_file());
    install::uninstall(&w.prefix).unwrap();
    assert!(!w.prefix.join(EXTRAS_FILE).exists());
}

#[test]
fn concurrent_adds_of_different_targets_both_land() {
    let w = world("concurrent");
    let dist = w.channel.dist();
    std::thread::scope(|scope| {
        for triple in ["wasm32-wasip1", "wasm32-unknown-unknown", "aarch64-unknown-linux-gnu"] {
            let (prefix, dist) = (&w.prefix, &dist);
            scope.spawn(move || {
                let batch = dist::add(prefix, dist, &[target(triple)]).unwrap();
                assert!(batch.failed.is_empty(), "{triple}: {:?}", batch.failed);
            });
        }
    });
    let mut extras = dist::read_extras(&w.prefix);
    extras.sort_by_key(|spec| spec.to_string());
    assert_eq!(extras.len(), 3, "{extras:?}");
    for triple in ["wasm32-wasip1", "wasm32-unknown-unknown", "aarch64-unknown-linux-gnu"] {
        assert!(version_dir(&w.prefix, "frust-a").join(format!("lib/rustlib/{triple}/lib/libstd-abcd.rlib")).is_file(), "{triple}");
    }
    assert!(leftovers(&w.prefix).is_empty(), "{:?}", leftovers(&w.prefix));
}

/// What `package-release.sh` produces: clippy, rustfmt, rust-analyzer and its proc-macro server, rust-src
/// and rust-docs from the official packages, extra std targets, and `targets` / `components` in the manifest.
fn bundled_layout() -> Vec<(&'static str, &'static str)> {
    vec![
        ("bin/cargo-clippy", "c"),
        ("bin/clippy-driver", "d"),
        ("bin/rustfmt", "f"),
        ("bin/cargo-fmt", "f"),
        ("bin/rust-analyzer", "a"),
        ("libexec/rust-analyzer-proc-macro-srv", "p"),
        ("lib/rustlib/src/rust/library/core/src/lib.rs", "// core"),
        ("lib/rustlib/src/rust/library/std/src/lib.rs", "// std"),
        ("share/doc/rust/html/index.html", "<html></html>"),
        ("lib/rustlib/wasm32-wasip1/lib/libstd-77.rlib", "bundled-wasi"),
        ("lib/rustlib/wasm32-unknown-unknown/lib/libstd-77.rlib", "bundled-wasm"),
        (
            "manifest.json",
            "{\"id\":\"frust-bundled\",\"host\":\"x86_64-unknown-linux-gnu\",\"rust_release\":\"1.100.0\",\"rust_commit\":\"0ef1bc386059596dada8fb65b50425aab2ea637a\",\"targets\":[\"x86_64-unknown-linux-gnu\",\"wasm32-unknown-unknown\",\"wasm32-wasip1\"],\"components\":[\"cargo\",\"rustc\",\"rustdoc\",\"clippy\",\"rustfmt\",\"rust-analyzer\",\"rust-docs\",\"rust-src\"]}\n",
        ),
    ]
}

#[test]
fn a_bundled_toolchain_lists_what_its_archive_holds_and_adding_it_again_changes_nothing() {
    let scratch = Scratch::new("bundled");
    let prefix = scratch.path("prefix");
    install_toolchain(&prefix, &scratch, "frust-bundled", &bundled_layout());
    let dir = version_dir(&prefix, "frust-bundled");
    let nowhere = unreachable_dist(&scratch);

    let run = |args: &[&str]| installer_at(&frust_install(), &prefix, None).args(args).output().unwrap();
    let installed = text(&run(&["component", "list", "--installed"])).0;
    assert_eq!(installed, "cargo\nclippy\nrust-analyzer\nrust-docs\nrust-src\nrust-std\nrustc\nrustdoc\nrustfmt\n");
    assert_eq!(text(&run(&["target", "list", "--installed"])).0, format!("wasm32-unknown-unknown\nwasm32-wasip1\n{HOST}\n"));
    let shown = text(&run(&["show"])).0;
    for component in ["cargo", "clippy", "rust-analyzer", "rust-docs", "rust-src", "rust-std", "rustc", "rustdoc", "rustfmt"] {
        assert!(shown.contains(&format!("\n  {component}\n")), "{component} missing from show: {shown}");
    }
    assert!(!shown.contains("llvm-tools"), "{shown}");
    for target in ["wasm32-unknown-unknown", "wasm32-wasip1", HOST] {
        assert!(shown.contains(&format!("\n  {target}\n")), "{target} missing from show: {shown}");
    }

    // Adding what the archive already holds is a no-op that needs no network and rewrites nothing.
    let batch = dist::add(&prefix, &nowhere, &[component("rust-src"), component("rust-docs"), target("wasm32-wasip1")]).unwrap();
    assert!(batch.failed.is_empty(), "{:?}", batch.failed);
    assert!(batch.done.iter().all(|(_, change)| *change == Change::AlreadyInstalled), "{:?}", batch.done);
    assert_eq!(fs::read_to_string(dir.join("lib/rustlib/src/rust/library/core/src/lib.rs")).unwrap(), "// core");
    assert_eq!(fs::read_to_string(dir.join("lib/rustlib/wasm32-wasip1/lib/libstd-77.rlib")).unwrap(), "bundled-wasi");
    assert!(!dir.join("lib/rustlib/manifest-rust-src").exists());

    // The built-in components are refused with a message that says why.
    for built_in in ["rust-analyzer", "clippy", "rustfmt"] {
        let out = run(&["component", "add", built_in]);
        assert_eq!(out.status.code(), Some(2), "{built_in}");
        assert!(text(&out).1.contains(&format!("{built_in} is built into the fRust toolchain archive")), "{built_in}: {}", text(&out).1);
        let out = run(&["component", "remove", built_in]);
        assert_eq!(out.status.code(), Some(2), "{built_in}");
    }
    assert!(dir.join("bin/rust-analyzer").is_file() && dir.join("libexec/rust-analyzer-proc-macro-srv").is_file());
}

#[test]
fn bundled_rust_src_and_docs_can_be_removed_and_come_back_from_the_channel() {
    let w = world("bundled-remove");
    install_toolchain(&w.prefix, &w.scratch, "frust-bundled", &bundled_layout());
    let dir = version_dir(&w.prefix, "frust-bundled");
    let batch = dist::remove(&w.prefix, &w.channel.dist(), &[component("rust-src"), component("rust-docs"), target("wasm32-wasip1")]).unwrap();
    assert!(batch.failed.is_empty() && batch.done.iter().all(|(_, change)| *change == Change::Removed), "{:?}", batch.failed);
    assert!(!dir.join("lib/rustlib/src").exists() && !dir.join("share/doc/rust").exists() && !dir.join("lib/rustlib/wasm32-wasip1").exists());
    assert!(dir.join("lib/rustlib/wasm32-unknown-unknown/lib/libstd-77.rlib").is_file() && dir.join("bin/rust-analyzer").is_file());
    let batch = dist::add(&w.prefix, &w.channel.dist(), &[component("rust-src"), component("rust-docs"), target("wasm32-wasip1")]).unwrap();
    assert!(batch.failed.is_empty() && batch.done.iter().all(|(_, change)| *change == Change::Installed), "{:?}", batch.failed);
    assert!(dir.join("lib/rustlib/src/rust/library/std/src/lib.rs").is_file() && dir.join("share/doc/rust/html/index.html").is_file());
}

// ---- the binary: frust-install and frustup ----

fn installer_at(program: &Path, prefix: &Path, channel: Option<&Channel>) -> Command {
    let mut command = Command::new(program);
    command.env_remove("FRUST_RELEASE_BASE").env("FRUST_PREFIX", prefix).env_remove("FRUST_VARIANT").stdin(Stdio::null());
    match channel {
        Some(channel) => command.env("FRUST_DIST_BASE", format!("file://{}", channel.dir.display())),
        None => command.env("FRUST_DIST_BASE", "file:///nonexistent-frust-dist"),
    };
    command
}

fn frust_install() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_frust-install"))
}

/// The installer under another name, as the toolchain archive ships `frustup`.
fn as_name(scratch: &Scratch, name: &str) -> PathBuf {
    let dir = scratch.path(&format!("bin-{name}"));
    fs::create_dir_all(&dir).unwrap();
    let link = dir.join(name);
    if !link.exists() {
        std::os::unix::fs::symlink(frust_install(), &link).unwrap();
    }
    link
}

fn text(output: &Output) -> (String, String) {
    (String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned())
}

#[test]
fn the_binary_adds_lists_and_removes_targets_and_components() {
    let w = world("cli");
    let run = |args: &[&str]| installer_at(&frust_install(), &w.prefix, Some(&w.channel)).args(args).output().unwrap();

    let out = run(&["target", "add", "wasm32-wasip1", "wasm32-unknown-unknown"]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert!(stdout.is_empty() && stderr.contains("target wasm32-wasip1: installed"), "{stdout} {stderr}");
    assert!(version_dir(&w.prefix, "frust-a").join("lib/rustlib/wasm32-wasip1/lib/libstd-abcd.rlib").is_file());

    let out = run(&["target", "list"]);
    let (stdout, _) = text(&out);
    assert!(out.status.success());
    assert!(stdout.contains("wasm32-wasip1 (installed)\n") && stdout.contains("aarch64-unknown-linux-gnu\n"), "{stdout}");
    assert!(!stdout.contains("aarch64-unknown-linux-gnu (installed)"), "{stdout}");

    let out = run(&["target", "list", "--installed"]);
    let (stdout, _) = text(&out);
    assert_eq!(stdout, format!("wasm32-unknown-unknown\nwasm32-wasip1\n{HOST}\n"));

    let out = run(&["component", "add", "rust-docs", "rust-src"]);
    assert!(out.status.success(), "{}", text(&out).1);
    let out = run(&["component", "list"]);
    let (stdout, _) = text(&out);
    assert!(stdout.contains("rust-docs (installed)\n") && stdout.contains("rust-src (installed)\n") && stdout.contains("llvm-tools\n"), "{stdout}");

    let out = run(&["target", "remove", "wasm32-wasip1"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(!version_dir(&w.prefix, "frust-a").join("lib/rustlib/wasm32-wasip1").exists());
    let out = run(&["component", "remove", "rust-docs", "rust-src"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(!version_dir(&w.prefix, "frust-a").join("share/doc/rust").exists());

    let out = run(&["show"]);
    let (stdout, _) = text(&out);
    assert!(out.status.success() && stdout.contains("current: frust-a\n") && stdout.contains("wasm32-unknown-unknown"), "{stdout}");
    let out = run(&["which", "rustc"]);
    assert_eq!(text(&out).0, format!("{}\n", w.prefix.join("current/bin/rustc").display()));
    assert!(!run(&["which", "nonexistent"]).status.success());
}

#[test]
fn the_binary_reports_failures_and_usage_mistakes() {
    let w = world("cli-errors");
    let run = |args: &[&str]| installer_at(&frust_install(), &w.prefix, Some(&w.channel)).args(args).output().unwrap();

    let out = run(&["target", "add", "wasm32-wasip1", "nope-nope"]);
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("target wasm32-wasip1: installed") && stderr.contains("error: target nope-nope: target nope-nope is not available"), "{stderr}");
    // The target that could be installed stays installed.
    assert!(version_dir(&w.prefix, "frust-a").join("lib/rustlib/wasm32-wasip1").is_dir());

    for args in [
        &["target"][..],
        &["target", "bogus"],
        &["target", "add"],
        &["target", "add", "../x"],
        &["target", "list", "extra"],
        &["target", "add", "--installed", "x"],
        &["component", "add", "clippy"],
        &["component", "add", "rust-std"],
        &["component", "remove"],
        &["which"],
        &["show", "extra"],
        &["target", "add", "--flag"],
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out).1);
        assert!(text(&out).1.contains("usage: frust-install"), "{args:?}");
    }
    let out = run(&["target", "remove", HOST]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).1.contains("cannot be removed"));
}

#[test]
fn the_binary_re_adds_extras_after_an_update_and_survives_a_failure() {
    let w = world("cli-update");
    let run = |args: &[&str]| installer_at(&frust_install(), &w.prefix, Some(&w.channel)).args(args).output().unwrap();
    assert!(run(&["target", "add", "wasm32-wasip1"]).status.success());
    assert!(run(&["component", "add", "rust-docs"]).status.success());

    let (archive, sum) = toolchain(&w.scratch.root, "frust-b", &[]);
    let out = run(&["update", "--archive", archive.to_str().unwrap(), "--checksum", sum.to_str().unwrap()]);
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(stdout, "current=frust-b\n");
    assert!(stderr.contains("target wasm32-wasip1: installed") && stderr.contains("component rust-docs: installed"), "{stderr}");
    let dir = version_dir(&w.prefix, "frust-b");
    assert!(dir.join("lib/rustlib/wasm32-wasip1/lib/libstd-abcd.rlib").is_file() && dir.join("share/doc/rust/html/index.html").is_file());

    // A third version with the channel unreachable: the update succeeds, the failure is loud and names the retry.
    let (archive, sum) = toolchain(&w.scratch.root, "frust-c", &[]);
    let out = installer_at(&frust_install(), &w.prefix, None)
        .args(["update", "--archive", archive.to_str().unwrap(), "--checksum", sum.to_str().unwrap()])
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(stdout, "current=frust-c\n");
    assert!(stderr.contains("WARNING: could not add target wasm32-wasip1") && stderr.contains("retry with: frust-install target add wasm32-wasip1"), "{stderr}");
    assert!(stderr.contains("WARNING: could not add component rust-docs"), "{stderr}");
    assert_eq!(install::current_version(&w.prefix).unwrap().as_deref(), Some("frust-c"));
    // With the channel back, running the retry command works.
    assert!(run(&["target", "add", "wasm32-wasip1"]).status.success());
    assert!(version_dir(&w.prefix, "frust-c").join("lib/rustlib/wasm32-wasip1/lib/libstd-abcd.rlib").is_file());
}

#[test]
fn frustup_is_the_installer_under_another_name() {
    let w = world("frustup");
    let frustup = as_name(&w.scratch, "frustup");
    let run = |args: &[&str]| installer_at(&frustup, &w.prefix, Some(&w.channel)).args(args).output().unwrap();

    let out = run(&["help"]);
    let (stdout, _) = text(&out);
    assert!(out.status.success() && stdout.starts_with("usage: frustup ") && !stdout.contains("frust-install"), "{stdout}");

    // Without a command frustup shows its usage and does not install anything.
    let out = installer_at(&frustup, &w.scratch.path("empty-prefix"), Some(&w.channel)).output().unwrap();
    let (_, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr.contains("frustup: a command is required") && stderr.contains("usage: frustup "), "{stderr}");
    assert!(!w.scratch.path("empty-prefix").exists());

    let out = run(&["target", "add", "wasm32-wasip1"]);
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(text(&out).1.contains("frustup: target wasm32-wasip1: installed"), "{}", text(&out).1);
    let out = run(&["show"]);
    assert!(out.status.success() && text(&out).0.contains("wasm32-wasip1"), "{}", text(&out).0);
    let out = run(&["bogus"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("frustup: unknown command bogus"), "{}", text(&out).1);

    // rollback and uninstall work under the new name too.
    let (archive, sum) = toolchain(&w.scratch.root, "frust-b", &[]);
    assert!(run(&["update", "--archive", archive.to_str().unwrap(), "--checksum", sum.to_str().unwrap()]).status.success());
    let out = run(&["rollback"]);
    assert_eq!(text(&out).0, "current=frust-a\n");
    assert!(run(&["uninstall"]).status.success());
    assert!(!w.prefix.join("current").exists());
}

#[test]
fn the_other_name_still_behaves_as_frust_install() {
    let w = world("other-name");
    let other = as_name(&w.scratch, "something-else");
    let out = installer_at(&other, &w.prefix, Some(&w.channel)).arg("help").output().unwrap();
    assert!(text(&out).0.starts_with("usage: frust-install "));
}

// ---- the real channel ----

fn online() -> bool {
    if std::env::var_os("FRUST_SKIP_ONLINE_TESTS").is_some() {
        return false;
    }
    Command::new("curl")
        .args(["--silent", "--fail", "--max-time", "15", "--output", "/dev/null", &format!("{}/channel-rust-{RELEASE}.toml.sha256", dist::OFFICIAL_DIST)])
        .status()
        .is_ok_and(|status| status.success())
}

/// Adds and removes a real std package and rust-src from static.rust-lang.org. Skipped without network, and when
/// FRUST_SKIP_ONLINE_TESTS is set.
#[test]
fn the_official_channel_serves_a_std_package_and_rust_src() {
    if !online() {
        eprintln!("skipped: static.rust-lang.org is not reachable");
        return;
    }
    let scratch = Scratch::new("online");
    let prefix = scratch.path("prefix");
    install_toolchain(&prefix, &scratch, "frust-a", &[]);
    let batch = dist::add(&prefix, &Dist::official(), &[target("wasm32-wasip1")]).unwrap();
    assert_eq!(only_change(batch), Change::Installed);
    let lib = version_dir(&prefix, "frust-a").join("lib/rustlib/wasm32-wasip1/lib");
    let has_std = fs::read_dir(&lib).unwrap().flatten().any(|entry| entry.file_name().to_string_lossy().starts_with("libstd-"));
    assert!(has_std, "no libstd in {}", lib.display());
    let list = dist::list_targets(&prefix, &Dist::official(), true).unwrap();
    assert!(list.targets.iter().any(|target| target.name == "wasm32-unknown-unknown" && !target.installed));
    assert!(list.targets.iter().any(|target| target.name == "wasm32-wasip1" && target.installed));
    assert_eq!(only_change(dist::remove(&prefix, &Dist::official(), &[target("wasm32-wasip1")]).unwrap()), Change::Removed);
    assert!(!version_dir(&prefix, "frust-a").join("lib/rustlib/wasm32-wasip1").exists());

    let batch = dist::add(&prefix, &Dist::official(), &[component("rust-src")]).unwrap();
    assert_eq!(only_change(batch), Change::Installed);
    assert!(version_dir(&prefix, "frust-a").join("lib/rustlib/src/rust/library/core/src/lib.rs").is_file());
}
