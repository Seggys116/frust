#!/usr/bin/env python3
"""Prove the fRust Cargo unit artifact cache on a vendored, offline synthetic workspace."""

import argparse
from collections import namedtuple
import hashlib
import json
import mmap
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import time

# Registry units that must be cached; BOUND ones embed an OUT_DIR path (or depend on a unit that
# does) and are only reusable in the build directory they were made in.
BOUND = ("outdep", "heavy", "heavyuser")
REGISTRY = ("leaf", "featdep", "layer", "envdep", "outdep", "heavy", "heavyuser", "pm", "usepm")
SHARED = tuple(crate for crate in REGISTRY if crate not in BOUND)
CACHE_LINE = re.compile(r"frust-unit-cache: (hit|miss|store|skip) ([^\r\n]*)")
SCRIPT_LINE = re.compile(r"frust-unit-cache: script-(hit|miss|store|skip) ([^\r\n]*)")
SCRIPT_KIND = "build-script"
SCRIPT_MIN_MS = "FRUST_UNIT_CACHE_SCRIPT_MIN_MS"
DROPPED_ENV = ("CARGO_", "RUSTC", "RUSTFLAGS", "RUSTDOCFLAGS", "RUSTUP_TOOLCHAIN", "XDG_",
               "FRUST_UNIT_CACHE", "FRUST_PROBE_ENV", "FRUST_TARGET", "SOURCE_DATE_EPOCH")

Event = namedtuple("Event", "kind key crate reason")
Result = namedtuple("Result", "returncode stdout stderr events elapsed scripts")


class Failure(Exception):
    """A property the cache must provide did not hold."""


class Skip(Exception):
    """A case cannot run in this environment."""


LEAF_SRC = r'''pub fn id() -> &'static str {
    "leaf-v1"
}
'''
FEATDEP_SRC = r'''pub fn mode() -> String {
    let kind = if cfg!(feature = "fancy") { "fancy" } else { "plain" };
    format!("{}+{}", kind, leaf::id())
}
'''
LAYER_SRC = r'''pub fn describe() -> String {
    format!("layer({})", featdep::mode())
}
'''
ENVDEP_SRC = r'''pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn probe() -> &'static str {
    match option_env!("FRUST_PROBE_ENV") {
        Some(value) => value,
        None => "unset",
    }
}
'''
OUTDEP_BUILD = r'''use std::{env, fs, path::Path};

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    fs::write(
        Path::new(&out).join("generated.rs"),
        "pub fn generated() -> &'static str { \"generated-by-build-script\" }\n",
    )
    .unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
'''
OUTDEP_SRC = r'''include!(concat!(env!("OUT_DIR"), "/generated.rs"));

pub fn out_dir() -> &'static str {
    env!("OUT_DIR")
}

pub fn source_file() -> &'static str {
    file!()
}
'''
HEAVY_BUILD = r'''use std::{env, fs, io::Write, path::Path, thread, time::Duration};

fn main() {
    // One line per real execution lets the probe count how often the script actually ran.
    if let Ok(log) = env::var("FRUST_PROBE_SCRIPT_LOG") {
        let mut file = fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
        writeln!(file, "heavy script ran").unwrap();
    }
    thread::sleep(Duration::from_millis(300));
    let out = env::var("OUT_DIR").unwrap();
    let out = Path::new(&out);
    let value = env::var("FRUST_PROBE_ENV2").unwrap_or_else(|_| "none".to_string());
    fs::write(
        out.join("generated.rs"),
        format!("pub fn generated() -> &'static str {{ {value:?} }}\n"),
    )
    .unwrap();
    fs::create_dir_all(out.join("data")).unwrap();
    fs::write(out.join("data/blob.txt"), "blob-v1").unwrap();
    println!("cargo:rustc-check-cfg=cfg(heavy_built)");
    println!("cargo:rustc-cfg=heavy_built");
    println!("cargo:rustc-env=PROBE_SCRIPT=from-script");
    println!("cargo:probe=1");
    println!("cargo:rerun-if-env-changed=FRUST_PROBE_ENV2");
}
'''
HEAVY_SRC = r'''include!(concat!(env!("OUT_DIR"), "/generated.rs"));

pub fn blob() -> &'static str {
    include_str!(concat!(env!("OUT_DIR"), "/data/blob.txt"))
}

pub fn script() -> &'static str {
    env!("PROBE_SCRIPT")
}

pub fn cfg_set() -> bool {
    cfg!(heavy_built)
}

pub fn out_dir() -> &'static str {
    env!("OUT_DIR")
}
'''
HEAVYUSER_BUILD = r'''fn main() {
    let seen = std::env::var("DEP_HEAVY_PROBE").unwrap();
    println!("cargo:rustc-env=SEEN_PROBE={seen}");
}
'''
HEAVYUSER_SRC = r'''pub fn seen_probe() -> &'static str {
    env!("SEEN_PROBE")
}
'''
PM_SRC = r'''use proc_macro::TokenStream;

#[proc_macro]
pub fn make_answer(_input: TokenStream) -> TokenStream {
    "pub fn answer() -> u32 { 42 }".parse().unwrap()
}
'''
USEPM_SRC = "pm::make_answer!();\n"
LOCALDEP_SRC = r'''pub fn name() -> &'static str {
    "localdep"
}
'''
MAIN_SRC = r'''fn main() {
    println!("leaf={}", leaf::id());
    println!("feat={}", featdep::mode());
    println!("layer={}", layer::describe());
    println!("version={}", envdep::version());
    println!("probe={}", envdep::probe());
    println!("generated={}", outdep::generated());
    println!("out_dir={}", outdep::out_dir());
    println!("source={}", outdep::source_file());
    println!("heavy_generated={}", heavy::generated());
    println!("heavy_blob={}", heavy::blob());
    println!("heavy_script={}", heavy::script());
    println!("heavy_cfg={}", heavy::cfg_set());
    println!("heavy_out_dir={}", heavy::out_dir());
    println!("seen_probe={}", heavyuser::seen_probe());
    println!("answer={}", usepm::answer());
    println!("local={}", localdep::name());
    println!("tag=@TAG@");
}
'''

# name -> (dependencies, extra manifest sections, source files)
VENDOR_CRATES = {
    "leaf": ("", "", {"src/lib.rs": LEAF_SRC}),
    "featdep": ('leaf = "1"\n', '[features]\nfancy = []\n', {"src/lib.rs": FEATDEP_SRC}),
    "layer": ('featdep = "1"\n', "", {"src/lib.rs": LAYER_SRC}),
    "envdep": ("", "", {"src/lib.rs": ENVDEP_SRC}),
    "outdep": ("", 'build = "build.rs"\n',
               {"src/lib.rs": OUTDEP_SRC, "build.rs": OUTDEP_BUILD}),
    "heavy": ("", 'build = "build.rs"\nlinks = "heavy"\n',
              {"src/lib.rs": HEAVY_SRC, "build.rs": HEAVY_BUILD}),
    "heavyuser": ('heavy = "1"\n', 'build = "build.rs"\n',
                  {"src/lib.rs": HEAVYUSER_SRC, "build.rs": HEAVYUSER_BUILD}),
    "pm": ("", "[lib]\nproc-macro = true\n", {"src/lib.rs": PM_SRC}),
    "usepm": ('pm = "1"\n', "", {"src/lib.rs": USEPM_SRC}),
}


def write_vendor(vendor):
    for name, (dependencies, extra, files) in VENDOR_CRATES.items():
        base = vendor / name
        header = f'[package]\nname = "{name}"\nversion = "1.0.0"\nedition = "2021"\n'
        if extra.startswith("build"):
            header += extra
            extra = ""
        for relative, body in files.items():
            (base / relative).parent.mkdir(parents=True, exist_ok=True)
            (base / relative).write_text(body)
        (base / "Cargo.toml").write_text(f"{header}\n{extra}\n[dependencies]\n{dependencies}")
        write_checksums(base)


def write_checksums(base):
    """Write a real directory-source checksum file covering every file of the crate."""
    files = {}
    for path in sorted(base.rglob("*")):
        if path.is_file() and path.name != ".cargo-checksum.json":
            files[path.relative_to(base).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    (base / ".cargo-checksum.json").write_text(json.dumps({"files": files, "package": None}))


def write_project(root, vendor, name, debug):
    (root / ".cargo").mkdir(parents=True)
    (root / "src").mkdir()
    (root / "localdep/src").mkdir(parents=True)
    (root / ".cargo/config.toml").write_text(
        '[source.crates-io]\nreplace-with = "vendored"\n\n[source.vendored]\n'
        f"directory = {json.dumps(str(vendor))}\n")
    profile = "" if debug else "\n[profile.dev]\ndebug = 0\n"
    dependencies = "".join(f'{crate} = "1"\n' for crate in VENDOR_CRATES)
    (root / "Cargo.toml").write_text(
        f'[package]\nname = "app_{name}"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n\n'
        f'[dependencies]\n{dependencies}localdep = {{ path = "localdep" }}\n\n'
        f'[features]\nfancy = ["featdep/fancy"]\n{profile}')
    (root / "src/main.rs").write_text(MAIN_SRC.replace("@TAG@", name))
    (root / "localdep/Cargo.toml").write_text(
        '[package]\nname = "localdep"\nversion = "0.1.0"\nedition = "2021"\n')
    (root / "localdep/src/lib.rs").write_text(LOCALDEP_SRC)


def set_modes(path, mode):
    for base, _dirs, _files in os.walk(path, topdown=False):
        os.chmod(base, mode)


def remove_tree(path, root):
    target = Path(path).resolve()
    if target != root and root not in target.parents:
        raise RuntimeError(f"refusing to remove {target} outside {root}")
    if target.exists():
        set_modes(target, 0o700)
        shutil.rmtree(target)


def stop(process):
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def parse_events(stderr, pattern=CACHE_LINE):
    events = []
    for match in pattern.finditer(stderr):
        kind, rest = match.groups()
        if kind == "skip":
            crate, _, reason = rest.partition(" ")
            key = ""
        else:
            key, _, tail = rest.partition(" ")
            crate, _, reason = tail.partition(" ")
        events.append(Event(kind, key, re.split(r"[-@]", crate)[0], reason.strip()))
    return events


class Job:
    """One running Cargo invocation whose output goes to log files."""

    def __init__(self, command, root, env, stem, timeout):
        self.timeout = timeout
        self.paths = (stem.with_suffix(".out"), stem.with_suffix(".err"))
        self.started = time.monotonic()
        with self.paths[0].open("wb") as out, self.paths[1].open("wb") as err:
            self.process = subprocess.Popen(command, cwd=root, env=env, stdout=out, stderr=err,
                                            stdin=subprocess.DEVNULL, start_new_session=True)

    def finish(self):
        suffix = ""
        try:
            code = self.process.wait(timeout=self.timeout)
        except subprocess.TimeoutExpired:
            stop(self.process)
            code, suffix = -1, "\n(killed after timeout)"
        elapsed = time.monotonic() - self.started
        stdout = self.paths[0].read_text(errors="replace")
        stderr = self.paths[1].read_text(errors="replace") + suffix
        return Result(code, stdout, stderr, parse_events(stderr), elapsed,
                      parse_events(stderr, SCRIPT_LINE))


class Context:
    def __init__(self, args, work):
        self.fcargo, self.rustc, self.rustc2, self.cargo = (
            args.fcargo, args.rustc, args.rustc2, args.cargo)
        self.timeout = args.timeout
        self.work = work
        self.cache_home = work / "xdg-cache"
        self.units = self.cache_home / "frust/artifacts/units/v1"
        self.vendor = work / "vendor"
        self.logs = work / "logs"
        self.script_log = self.logs / "script-runs.log"
        self.jobs = []

    def setup(self):
        for name in ("xdg-config", "cargo-home", "logs", "targets", "projects"):
            (self.work / name).mkdir(parents=True, exist_ok=True)
        write_vendor(self.vendor)

    def reset(self):
        self.remove(self.cache_home)
        self.remove(self.work / "targets")
        self.cache_home.mkdir()
        (self.work / "targets").mkdir()

    def remove(self, path):
        remove_tree(path, self.work)

    def stop_all(self):
        for job in self.jobs:
            if job.process.poll() is None:
                stop(job.process)

    def target(self, name):
        return self.work / "targets" / name

    def clean(self, name):
        self.remove(self.target(name))

    def project(self, name):
        root = self.work / "projects" / name
        if not root.exists():
            write_project(root, self.vendor, name, debug=name == "main")
        return root

    def environment(self, rustc, target, extra):
        env = {key: value for key, value in os.environ.items()
               if key != "CARGO" and not key.startswith(DROPPED_ENV)}
        path = [str(self.fcargo.parent)] + ([str(self.cargo.parent)] if self.cargo else [])
        env.update(
            PATH=os.pathsep.join(path + [os.environ.get("PATH", "")]),
            CARGO_HOME=str(self.work / "cargo-home"), XDG_CACHE_HOME=str(self.cache_home),
            XDG_CONFIG_HOME=str(self.work / "xdg-config"), RUSTC=str(rustc),
            CARGO_TERM_COLOR="never", CARGO_INCREMENTAL="0", FRUST_PROGRESS="plain",
            FRUST_UNIT_CACHE_DEBUG="1")
        if self.cargo:
            env["FRUST_CARGO"] = str(self.cargo)
        if target:
            env["CARGO_BUILD_BUILD_DIR"] = str(target)
        for key, value in (extra or {}).items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        return env

    def start(self, project, target=None, features=(), env=None, rustc=None, gate=None):
        directory = self.target(target) if target else None
        command = [str(self.fcargo), "run", "--offline"]
        if features:
            command += ["--features", ",".join(features)]
        if directory:
            command += ["--target-dir", str(directory)]
        if gate:
            # Hold the process until every concurrent build has been spawned.
            command = ["sh", "-c", 'while [ ! -e "$0" ]; do sleep 0.01; done; exec "$@"',
                       str(gate)] + command
        stem = self.logs / f"job-{len(self.jobs)}"
        job = Job(command, self.project(project),
                  self.environment(rustc or self.rustc, directory, env), stem, self.timeout)
        self.jobs.append(job)
        return job

    def build(self, project, target=None, **options):
        result = self.start(project, target, **options).finish()
        expect(result.returncode == 0,
               f"build of {project} exited {result.returncode}:\n{result.stderr[-3000:]}")
        return result

    def all_entries(self):
        return sorted(path.parent for path in self.units.glob("*/*/entry.json"))

    def entries(self):
        return [entry for entry in self.all_entries() if entry_kind(entry) != SCRIPT_KIND]

    def script_entries(self):
        return [entry for entry in self.all_entries() if entry_kind(entry) == SCRIPT_KIND]

    def script_env(self, extra=None):
        """Environment that lets scripts be cached and counted; starts a fresh run log."""
        self.script_log.unlink(missing_ok=True)
        env = {SCRIPT_MIN_MS: "100", "FRUST_PROBE_SCRIPT_LOG": str(self.script_log)}
        env.update(extra or {})
        return env

    def script_runs(self):
        try:
            return len(self.script_log.read_text().splitlines())
        except FileNotFoundError:
            return 0

    def entry_dir(self, key):
        found = [path for path in self.units.glob(f"*/{key}*") if path.is_dir()]
        expect(len(found) == 1, f"expected one entry directory for key {key}, found {found}")
        return found[0]

    def store_files(self):
        return sorted(str(path) for path in self.units.rglob("*") if path.is_file())

    def store_problems(self):
        return [problem for entry in self.entries() for problem in entry_problems(entry)]


def entry_kind(entry):
    """Entries written before build script caching carry no kind field and are unit entries."""
    try:
        return json.loads((entry / "entry.json").read_text()).get("kind", "unit")
    except (OSError, ValueError, AttributeError):
        return "unit"


def entry_problems(entry):
    try:
        manifest = json.loads((entry / "entry.json").read_text())
        outputs = manifest["outputs"]
    except (OSError, ValueError, KeyError, TypeError) as error:
        return [f"{entry.name}: unreadable entry.json ({error})"]
    problems = [f"{entry.name}: no outputs listed"] if not outputs else []
    for output in outputs:
        blob = entry / str(output.get("blake3"))
        if not blob.is_file():
            problems.append(f"{entry.name}: missing blob {output.get('blake3')}")
        elif blob.stat().st_size != output.get("size"):
            problems.append(f"{entry.name}: blob {blob.name} has the wrong size")
    problems += [f"{entry.name}: leftover temporary file {path.name}"
                 for path in entry.iterdir() if path.name.startswith("tmp-")]
    return problems


def expect(condition, message):
    if not condition:
        raise Failure(message)


def describe(result):
    lines = [f"  {e.kind} {e.key} {e.crate} {e.reason}".rstrip() for e in result.events]
    lines += [f"  script-{e.kind} {e.key} {e.crate} {e.reason}".rstrip() for e in result.scripts]
    return "cache lines:\n" + ("\n".join(lines) if lines else "  (none on stderr)")


def outcome(result, crate):
    kinds = {event.kind for event in result.events if event.crate == crate}
    if "miss" in kinds:
        return "miss"
    return "hit" if "hit" in kinds else "none"


def expect_outcomes(result, crates, want, label):
    got = {crate: outcome(result, crate) for crate in crates}
    wrong = {crate: kind for crate, kind in got.items() if kind != want}
    expect(not wrong, f"{label}: expected {want} for {sorted(crates)}, got {wrong}\n"
                      f"{describe(result)}")


def expect_no_local_lines(result, tag):
    for crate in ("localdep", f"app_{tag}"):
        lines = [e for e in result.events if e.crate == crate and e.kind != "skip"]
        expect(not lines, f"{crate} must never be cached but reported {lines}")


def stored(result):
    return [event for event in result.events if event.kind == "store"]


def script_outcome(result, crate):
    kinds = {event.kind for event in result.scripts if event.crate == crate}
    return next((kind for kind in ("miss", "hit", "skip") if kind in kinds), "none")


def script_stores(result, crate):
    return [e for e in result.scripts if e.kind == "store" and e.crate == crate]


def expect_script(result, crate, want, label):
    got = script_outcome(result, crate)
    expect(got == want, f"{label}: expected script {want} for {crate}, got {got}\n{describe(result)}")


def expect_runs(ctx, want, label):
    got = ctx.script_runs()
    expect(got == want, f"{label}: the heavy script ran {got} times in total, want {want}")


def program_output(result):
    return dict(re.findall(r"^(\w+)=(.*)$", result.stdout, re.M))


def check_output(result, tag, target=None, fancy=False, probe="unset", env2="none"):
    got = program_output(result)
    kind = "fancy" if fancy else "plain"
    want = {"leaf": "leaf-v1", "feat": f"{kind}+leaf-v1", "layer": f"layer({kind}+leaf-v1)",
            "version": "1.0.0", "probe": probe, "generated": "generated-by-build-script",
            "heavy_generated": env2, "heavy_blob": "blob-v1", "heavy_script": "from-script",
            "heavy_cfg": "true", "seen_probe": "1",
            "answer": "42", "local": "localdep", "tag": tag}
    for key, value in want.items():
        expect(got.get(key) == value, f"program printed {key}={got.get(key)!r}, want {value!r}")
    expect(got.get("source", "").endswith("outdep/src/lib.rs"),
           f"program printed source={got.get('source')!r}")
    if target is not None:
        for key in ("out_dir", "heavy_out_dir"):
            out_dir = got.get(key, "")
            expect(out_dir.startswith(f"{target}{os.sep}") and Path(out_dir).is_dir(),
                   f"program reports a stale or missing {key} {out_dir!r}, expected under {target}")
    return got


def snapshot(target, crates):
    root = target / "debug"
    expect(root.is_dir(), f"no debug directory at {root}")
    found = {}
    # Walk both the flat deps layout and the per-unit build layout.
    for path in root.rglob("lib*"):
        stem = path.name[3:].split("-")[0]
        if stem in crates and path.suffix in (".rlib", ".rmeta") and path.is_file():
            found[path.name] = path.read_bytes()
    for crate in set(crates) - {"pm"}:
        expect(any(name.startswith(f"lib{crate}-") and name.endswith(".rlib") for name in found),
               f"no rlib for {crate} under {root}")
    return found


def expect_identical(left, right, label):
    expect(left.keys() == right.keys(),
           f"{label}: file sets differ: {sorted(left.keys() ^ right.keys())}")
    different = sorted(name for name in left if left[name] != right[name])
    expect(not different, f"{label}: contents differ for {different}")


CASES = []


def case(function):
    CASES.append(function)
    return function


@case
def cold_store(ctx):
    result = ctx.build("main", "t1")
    expect(result.events, "no frust-unit-cache lines on stderr; is FRUST_UNIT_CACHE_DEBUG "
                          "supported by this Cargo?\n" + result.stderr[-1500:])
    expect_outcomes(result, REGISTRY, "miss", "cold build")
    names = sorted(event.crate for event in stored(result))
    expect(names == sorted(REGISTRY), f"stored {names}, want {sorted(REGISTRY)}\n{describe(result)}")
    expect_no_local_lines(result, "main")
    expect(len(ctx.entries()) == len(REGISTRY),
           f"{len(ctx.entries())} entries on disk for {len(REGISTRY)} stores")
    expect(not ctx.store_problems(), f"invalid entries: {ctx.store_problems()}")
    check_output(result, "main", ctx.target("t1"))


@case
def clean_rebuild_hit(ctx):
    first = ctx.build("main", "t1")
    ctx.clean("t1")
    second = ctx.build("main", "t1")
    expect_outcomes(second, REGISTRY, "hit", "rebuild after clean")
    hits = [event for event in second.events if event.kind == "hit"]
    expect(len(hits) == len(stored(first)),
           f"{len(hits)} hits after {len(stored(first))} stores\n{describe(second)}")
    expect(not [e for e in second.events if e.kind in ("miss", "store")],
           f"a pure hit build must not miss or store\n{describe(second)}")
    expect_no_local_lines(second, "main")
    expect(program_output(first) == program_output(second), "program output changed after restore")
    check_output(second, "main", ctx.target("t1"))
    print(f"INFO cold build {first.elapsed:.1f}s, restored build {second.elapsed:.1f}s", flush=True)


@case
def restored_build_is_fresh(ctx):
    ctx.build("main", "t1")
    ctx.clean("t1")
    restored = ctx.build("main", "t1")
    expect_outcomes(restored, REGISTRY, "hit", "restore build")
    for round_number in (1, 2):
        again = ctx.build("main", "t1")
        compiled = re.findall(r"Compiling (\S+)", again.stderr)
        expect(not compiled, f"no-op build {round_number} after a restore recompiled {compiled}\n"
                             f"{describe(again)}")
        expect(not again.events or all(e.kind == "skip" for e in again.events),
               f"no-op build {round_number} consulted the cache\n{describe(again)}")


@case
def mapped_source_edit_rebuilds(ctx):
    source = ctx.project("mmap-edit") / "src" / "main.rs"
    source.write_bytes(b'fn main() { println!("one"); }\n')
    options = {"env": {"FRUST_UNIT_CACHE": "0"}}
    with source.open("r+b") as handle, mmap.mmap(handle.fileno(), 0, access=mmap.ACCESS_WRITE) as mapped:
        original = mapped[:]
        mapped[:] = original
        time.sleep(0.25)
        first = ctx.build("mmap-edit", "t1", **options)
        expect(first.stdout.strip() == "one", "initial build did not print one")
        mapped[:] = original.replace(b'"one"', b'"two"')
        edited = ctx.build("mmap-edit", "t1", **options)
        expect(edited.stdout.strip() == "two", "the second build reused the old executable")


@case
def vendor_edit_miss(ctx):
    ctx.build("main", "t1")
    leaf = ctx.vendor / "leaf" / "src" / "lib.rs"
    original = leaf.read_text()
    leaf.write_text(original + "\n// edited vendored source\n")
    write_checksums(ctx.vendor / "leaf")
    try:
        ctx.clean("t1")
        edited = ctx.build("main", "t1")
        expect_outcomes(edited, ("leaf",), "miss", "edited vendored crate")
    finally:
        leaf.write_text(original)
        write_checksums(ctx.vendor / "leaf")


@case
def byte_identical(ctx):
    ctx.build("main", "t1")
    ctx.clean("t1")
    restored_build = ctx.build("main", "t1")
    expect_outcomes(restored_build, REGISTRY, "hit", "restore build")
    restored = snapshot(ctx.target("t1"), REGISTRY)
    # Debuginfo embeds the working directory, so compare in the same target path first.
    ctx.clean("t1")
    plain = ctx.build("main", "t1", env={"FRUST_UNIT_CACHE": "0"})
    expect(not plain.events, f"disabled build printed cache lines\n{describe(plain)}")
    expect_identical(restored, snapshot(ctx.target("t1"), REGISTRY), "same target path")
    # Crates without OUT_DIR paths must also match a fresh build in another target directory.
    ctx.build("main", "t2", env={"FRUST_UNIT_CACHE": "0"})
    moved = {name: data for name, data in restored.items()
             if not name.startswith(tuple(f"lib{crate}-" for crate in BOUND))}
    expect_identical(moved, snapshot(ctx.target("t2"), SHARED), "different target path")


@case
def feature_miss(ctx):
    ctx.build("main", "t1")
    ctx.clean("t1")
    fancy = ctx.build("main", "t1", features=("fancy",))
    expect_outcomes(fancy, ("featdep", "layer"), "miss", "feature change")
    unrelated = tuple(c for c in REGISTRY if c not in ("featdep", "layer"))
    expect_outcomes(fancy, unrelated, "hit", "unrelated units")
    check_output(fancy, "main", ctx.target("t1"), fancy=True)
    ctx.clean("t1")
    plain = ctx.build("main", "t1")
    expect_outcomes(plain, REGISTRY, "hit", "return to default features")
    check_output(plain, "main", ctx.target("t1"))


@case
def rustflags_miss(ctx):
    ctx.build("main", "t1")
    ctx.clean("t1")
    flagged = ctx.build("main", "t1", env={"RUSTFLAGS": "--cfg probe_flag"})
    expect_outcomes(flagged, REGISTRY, "miss", "changed RUSTFLAGS")
    check_output(flagged, "main", ctx.target("t1"))
    ctx.clean("t1")
    expect_outcomes(ctx.build("main", "t1"), REGISTRY, "hit", "RUSTFLAGS restored")


@case
def env_dep(ctx):
    one, two = {"FRUST_PROBE_ENV": "one"}, {"FRUST_PROBE_ENV": "two"}
    check_output(ctx.build("main", "t1", env=one), "main", probe="one")
    ctx.clean("t1")
    same = ctx.build("main", "t1", env=one)
    expect_outcomes(same, REGISTRY, "hit", "unchanged environment")
    check_output(same, "main", probe="one")
    ctx.clean("t1")
    changed = ctx.build("main", "t1", env=two)
    expect_outcomes(changed, ("envdep",), "miss", "changed FRUST_PROBE_ENV")
    expect_outcomes(changed, tuple(c for c in REGISTRY if c != "envdep"), "hit", "other units")
    check_output(changed, "main", probe="two")
    ctx.clean("t1")
    unset = ctx.build("main", "t1")
    expect_outcomes(unset, ("envdep",), "miss", "unset FRUST_PROBE_ENV")
    check_output(unset, "main", probe="unset")


@case
def outdir_bound(ctx):
    first, second = ctx.target("t1"), ctx.target("t2")
    ctx.build("main", "t1")
    ctx.clean("t1")
    again = ctx.build("main", "t1")
    expect_outcomes(again, ("outdep",), "hit", "same target path after clean")
    check_output(again, "main", first)
    moved = check_output(ctx.build("main", "t2"), "main", second)
    expect(str(first) not in moved["out_dir"], f"stale OUT_DIR {moved['out_dir']!r} after move")
    ctx.clean("t2")
    check_output(ctx.build("main", "t2"), "main", second)


@case
def proc_macro(ctx):
    cold = ctx.build("main", "t1")
    expect({"pm", "usepm"} <= {event.crate for event in stored(cold)},
           f"proc-macro units were not stored\n{describe(cold)}")
    ctx.clean("t1")
    warm = ctx.build("main", "t1")
    expect_outcomes(warm, ("pm", "usepm"), "hit", "proc-macro rebuild")
    check_output(warm, "main", ctx.target("t1"))


@case
def toolchain_miss(ctx):
    if ctx.rustc2 is None:
        raise Skip("no --rustc2 given")
    if ctx.rustc2 == ctx.rustc:
        raise Skip("--rustc2 is the same path as --rustc")
    ctx.build("main", "t1")
    ctx.clean("t1")
    other = ctx.build("main", "t1", rustc=ctx.rustc2)
    expect_outcomes(other, REGISTRY, "miss", "different rustc")
    check_output(other, "main", ctx.target("t1"))
    ctx.clean("t1")
    expect_outcomes(ctx.build("main", "t1"), REGISTRY, "hit", "original rustc again")


@case
def concurrent(ctx):
    gate = ctx.work / "start-gate"
    jobs = [ctx.start("a", "ta", gate=gate), ctx.start("b", "tb", gate=gate)]
    gate.write_text("go")
    results = [job.finish() for job in jobs]
    for tag, result in zip("ab", results):
        expect(result.returncode == 0, f"concurrent build {tag} failed:\n{result.stderr[-3000:]}")
        check_output(result, tag, ctx.target(f"t{tag}"))
    expect(len(ctx.entries()) >= len(REGISTRY), f"only {len(ctx.entries())} entries stored")
    expect(not ctx.store_problems(), f"invalid entries: {ctx.store_problems()}")
    ctx.clean("ta")
    third = ctx.build("a", "ta")
    expect_outcomes(third, SHARED, "hit", "third run after concurrent builds")
    check_output(third, "a", ctx.target("ta"))


@case
def corrupt_entry(ctx):
    first = ctx.build("main", "t1")
    keys = {event.crate: event.key for event in stored(first)}
    for crate, damage in (("leaf", "truncate"), ("envdep", "flip")):
        entry = ctx.entry_dir(keys[crate])
        blobs = [path for path in entry.iterdir() if path.is_file() and path.name != "entry.json"
                 and not path.name.startswith("tmp-") and path.suffix != ".lock"]
        expect(blobs, f"entry {entry} has no blobs")
        blob = max(blobs, key=lambda path: path.stat().st_size)
        size = blob.stat().st_size
        blob.chmod(0o644)
        with blob.open("r+b") as handle:
            if damage == "truncate":
                handle.truncate(size // 2)
            else:
                handle.seek(size // 2)
                byte = handle.read(1)
                handle.seek(size // 2)
                handle.write(bytes([byte[0] ^ 0xFF]))
    ctx.clean("t1")
    second = ctx.build("main", "t1")
    expect_outcomes(second, ("leaf", "envdep"), "miss", "corrupted entries")
    check_output(second, "main", ctx.target("t1"))
    ctx.clean("t1")
    third = ctx.build("main", "t1")
    expect_outcomes(third, REGISTRY, "hit", "after repair")
    expect(not ctx.store_problems(), f"invalid entries: {ctx.store_problems()}")


@case
def read_only_store(ctx):
    if os.geteuid() == 0:
        raise Skip("running as root, permissions are not enforced")
    ctx.units.mkdir(parents=True)
    try:
        ctx.units.chmod(0o555)
        result = ctx.build("main", "t1")
        check_output(result, "main", ctx.target("t1"))
        expect(not ctx.entries(), f"entries were written into a read-only store: {ctx.entries()}")
        set_modes(ctx.cache_home, 0o755)
        ctx.clean("t1")
        ctx.build("main", "t1")
        expect(ctx.entries(), "populating the writable store stored nothing")
        set_modes(ctx.units, 0o555)
        ctx.clean("t1")
        check_output(ctx.build("main", "t1"), "main", ctx.target("t1"))
    finally:
        set_modes(ctx.cache_home, 0o755)


@case
def disabled(ctx):
    for value in ("0", "off"):
        result = ctx.build("main", "t1", env={"FRUST_UNIT_CACHE": value})
        expect("frust-unit-cache" not in result.stderr,
               f"FRUST_UNIT_CACHE={value} still printed cache lines\n{describe(result)}")
        expect(not ctx.store_files(), f"FRUST_UNIT_CACHE={value} wrote {ctx.store_files()}")
        check_output(result, "main", ctx.target("t1"))
        ctx.clean("t1")
    quiet = ctx.build("main", "t1", env={"FRUST_UNIT_CACHE_DEBUG": None})
    expect("frust-unit-cache" not in quiet.stderr,
           f"cache lines printed without FRUST_UNIT_CACHE_DEBUG\n{quiet.stderr[-1500:]}")
    expect(len(ctx.entries()) == len(REGISTRY),
           f"the silent build stored {len(ctx.entries())} entries, want {len(REGISTRY)}")


@case
def default_target(ctx):
    first = ctx.build("d1")
    expect_outcomes(first, REGISTRY, "miss", "cold build in the default target layout")
    expect(len(stored(first)) == len(REGISTRY), f"stores were incomplete\n{describe(first)}")
    # A second project has a new, cold target directory but identical dependencies.
    second = ctx.build("d2")
    expect_outcomes(second, SHARED, "hit", "second project in the default target layout")
    check_output(second, "d2")


def heavy_blobs(ctx, relative):
    """Every stored blob holding the contents of one OUT_DIR file of the heavy script entry."""
    digests = set()
    for entry in ctx.script_entries():
        manifest = json.loads((entry / "entry.json").read_text())
        digests |= {item["blake3"] for item in manifest.get("files", [])
                    if item.get("path") == relative and item.get("kind") == "file"}
    expect(digests, f"no stored script entry lists {relative}; entries: {ctx.script_entries()}")
    return [path for path in ctx.units.rglob("*") if path.is_file() and path.suffix != ".lock"
            and any(path.name.startswith(digest) for digest in digests)]


# Entries are bound to their target directory path, so two builds that share a store but not a
# path never share a script run. There is no concurrent script case for that reason: it would
# have to build the same target directory twice at once, which Cargo itself serializes.
@case
def script_hit_after_clean(ctx):
    env = ctx.script_env()
    first = ctx.build("main", "t1", env=env)
    expect_script(first, "heavy", "miss", "cold build")
    expect(len(script_stores(first, "heavy")) == 1,
           f"want exactly one script-store for heavy\n{describe(first)}")
    expect(not script_stores(first, "heavyuser"),
           f"the cheap heavyuser script must not be cached\n{describe(first)}")
    expect_runs(ctx, 1, "cold build")
    expect(ctx.script_entries(), "no build-script entry was written to the store")
    expect(len(ctx.entries()) == len(REGISTRY),
           f"{len(ctx.entries())} unit entries on disk, want {len(REGISTRY)}")
    expect(not ctx.store_problems(), f"invalid entries: {ctx.store_problems()}")
    check_output(first, "main", ctx.target("t1"))
    ctx.clean("t1")
    second = ctx.build("main", "t1", env=env)
    expect_script(second, "heavy", "hit", "rebuild after clean")
    heavy = [e for e in second.scripts if e.crate == "heavy"]
    expect([e.kind for e in heavy] == ["hit"], f"heavy script lines {heavy}\n{describe(second)}")
    expect_runs(ctx, 1, "rebuild after clean")
    expect_outcomes(second, REGISTRY, "hit", "units after the script restore")
    expect(program_output(first) == program_output(second), "program output changed after restore")
    check_output(second, "main", ctx.target("t1"))


@case
def script_env_miss(ctx):
    base = ctx.script_env()
    alpha = {**base, "FRUST_PROBE_ENV2": "alpha"}
    check_output(ctx.build("main", "t1", env=base), "main", ctx.target("t1"))
    expect_runs(ctx, 1, "cold build")
    ctx.clean("t1")
    changed = ctx.build("main", "t1", env=alpha)
    expect_script(changed, "heavy", "miss", "changed FRUST_PROBE_ENV2")
    expect_runs(ctx, 2, "changed FRUST_PROBE_ENV2")
    check_output(changed, "main", ctx.target("t1"), env2="alpha")
    ctx.clean("t1")
    restored = ctx.build("main", "t1", env=base)
    expect_script(restored, "heavy", "hit", "FRUST_PROBE_ENV2 unset again")
    expect_runs(ctx, 2, "FRUST_PROBE_ENV2 unset again")
    check_output(restored, "main", ctx.target("t1"))
    ctx.clean("t1")
    again = ctx.build("main", "t1", env=alpha)
    expect_script(again, "heavy", "hit", "FRUST_PROBE_ENV2 back to alpha")
    expect_runs(ctx, 2, "FRUST_PROBE_ENV2 back to alpha")
    check_output(again, "main", ctx.target("t1"), env2="alpha")


@case
def script_path_bound(ctx):
    env = ctx.script_env()
    first, second = ctx.target("t1"), ctx.target("t2")
    ctx.build("main", "t1", env=env)
    moved = ctx.build("main", "t2", env=env)
    expect_script(moved, "heavy", "miss", "different target path")
    expect_runs(ctx, 2, "different target path")
    got = check_output(moved, "main", second)
    stale = f"{first}{os.sep}"
    expect(stale not in got["heavy_out_dir"], f"stale OUT_DIR {got['heavy_out_dir']!r} after move")
    expect(stale not in moved.stdout and stale not in moved.stderr,
           f"output of the moved build mentions the first target directory {first}")
    ctx.clean("t2")
    again = ctx.build("main", "t2", env=env)
    expect_script(again, "heavy", "hit", "same moved path after clean")
    expect_runs(ctx, 2, "same moved path after clean")
    check_output(again, "main", second)


@case
def script_threshold(ctx):
    env = ctx.script_env({SCRIPT_MIN_MS: None})
    first = ctx.build("main", "t1", env=env)
    # A miss is logged before the duration check, so only a store or a hit would be wrong.
    expect(not script_stores(first, "heavy")
           and not [e for e in first.scripts if e.crate == "heavy" and e.kind == "hit"],
           f"a 0.3 s script was stored under the default threshold\n{describe(first)}")
    expect(not ctx.script_entries(), f"script entries stored: {ctx.script_entries()}")
    expect(len(ctx.entries()) == len(REGISTRY),
           f"{len(ctx.entries())} unit entries on disk, want {len(REGISTRY)}")
    check_output(first, "main", ctx.target("t1"))
    ctx.clean("t1")
    second = ctx.build("main", "t1", env=env)
    expect(script_outcome(second, "heavy") != "hit", f"script hit below the threshold\n{describe(second)}")
    expect_runs(ctx, 2, "rebuild below the threshold")
    expect_outcomes(second, REGISTRY, "hit", "units are cached regardless of the script threshold")
    check_output(second, "main", ctx.target("t1"))


@case
def script_fresh_noop(ctx):
    env = ctx.script_env()
    ctx.build("main", "t1", env=env)
    ctx.clean("t1")
    restored = ctx.build("main", "t1", env=env)
    expect_script(restored, "heavy", "hit", "restore build")
    for round_number in (1, 2):
        again = ctx.build("main", "t1", env=env)
        compiled = re.findall(r"Compiling (\S+)", again.stderr)
        expect(not compiled, f"no-op build {round_number} after a script restore recompiled "
                             f"{compiled}\n{describe(again)}")
        consulted = [e for e in again.events + again.scripts if e.kind != "skip"]
        expect(not consulted, f"no-op build {round_number} consulted the cache\n{describe(again)}")
        expect_runs(ctx, 1, f"no-op build {round_number}")
        check_output(again, "main", ctx.target("t1"))


@case
def script_dep_metadata(ctx):
    env = ctx.script_env()
    ctx.build("main", "t1", env=env)
    ctx.clean("t1")
    restored = ctx.build("main", "t1", env=env)
    expect_script(restored, "heavy", "hit", "restore build")
    expect_runs(ctx, 1, "restore build")
    expect(program_output(restored).get("seen_probe") == "1",
           f"DEP_HEAVY_PROBE did not reach heavyuser: {program_output(restored)}")
    expect(program_output(restored).get("heavy_script") == "from-script",
           "cargo:rustc-env from the restored script did not reach heavy")
    expect(not [e for e in restored.scripts
                if e.crate == "heavyuser" and e.kind in ("hit", "store")],
           f"the cheap heavyuser script was cached\n{describe(restored)}")
    check_output(restored, "main", ctx.target("t1"))


@case
def script_corrupt_blob(ctx):
    env = ctx.script_env()
    ctx.build("main", "t1", env=env)
    blobs = heavy_blobs(ctx, "data/blob.txt")
    expect(blobs, "no stored blob holds the contents of data/blob.txt")
    for blob in blobs:
        size = blob.stat().st_size
        expect(size > 0, f"blob {blob} is empty")
        blob.chmod(0o644)
        with blob.open("r+b") as handle:
            handle.seek(size // 2)
            byte = handle.read(1)
            handle.seek(size // 2)
            handle.write(bytes([byte[0] ^ 0xFF]))
    ctx.clean("t1")
    second = ctx.build("main", "t1", env=env)
    expect_runs(ctx, 2, "build over a corrupted script blob")
    check_output(second, "main", ctx.target("t1"))
    ctx.clean("t1")
    third = ctx.build("main", "t1", env=env)
    expect_script(third, "heavy", "hit", "after repair")
    expect_runs(ctx, 2, "after repair")
    check_output(third, "main", ctx.target("t1"))
    expect(not ctx.store_problems(), f"invalid entries: {ctx.store_problems()}")


def run_cases(ctx):
    counts = {"PASS": 0, "FAIL": 0, "SKIP": 0}
    for function in CASES:
        name = function.__name__.replace("_", "-")
        ctx.reset()
        try:
            function(ctx)
            status, detail = "PASS", ""
        except Skip as skip:
            status, detail = "SKIP", f": {skip}"
        except Failure as failure:
            status, detail = "FAIL", f": {failure}"
        except Exception as error:
            status, detail = "FAIL", f": unexpected {type(error).__name__}: {error}"
        counts[status] += 1
        print(f"{status} {name}{detail}", flush=True)
    print(f"{counts['PASS']} passed, {counts['FAIL']} failed, {counts['SKIP']} skipped")
    return counts["FAIL"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fcargo", type=Path, required=True, help="fcargo or cargo to drive")
    parser.add_argument("--rustc", type=Path, required=True, help="frustc or rustc to use")
    parser.add_argument("--rustc2", type=Path, help="a different toolchain for toolchain-miss")
    parser.add_argument("--cargo", type=Path, help="patched cargo for fcargo (FRUST_CARGO)")
    parser.add_argument("--work", type=Path, required=True, help="empty or absent scratch directory")
    parser.add_argument("--keep", action="store_true", help="keep the scratch directory")
    parser.add_argument("--timeout", type=float, default=900, help="seconds allowed per build")
    args = parser.parse_args()
    for name in ("fcargo", "rustc", "rustc2", "cargo"):
        value = getattr(args, name)
        if value is not None:
            setattr(args, name, value.absolute())
    work = args.work.resolve()
    if work.exists() and (not work.is_dir() or any(work.iterdir())):
        parser.error(f"--work {work} must be an empty directory or not exist")
    work.mkdir(parents=True, exist_ok=True)
    ctx = Context(args, work)
    try:
        ctx.setup()
        failures = run_cases(ctx)
    finally:
        ctx.stop_all()
        if args.keep:
            print(f"kept {work}")
        else:
            remove_tree(work, work)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
