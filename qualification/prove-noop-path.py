#!/usr/bin/env python3
"""Prove the no-op build paths of fRust's Cargo on a vendored, offline synthetic workspace.

Two things are proven here, both against the real fcargo and frustc:

  * the resolve memo (patch 0028). A build that finds Cargo.lock current records a digest of the
    inputs of the whole-workspace resolution, and a later build with the same digest loads
    Cargo.lock instead of resolving the workspace again and leaves dev-dependencies out of its own
    resolve when it compiles no dev units. Every input that can change the answer must miss, and
    the compiler commands of a build that hit must equal those of a build with the memo off.
  * the content fingerprint (patches 0002 and 0029). A source file edited without changing its
    size, even with its modification time restored or through a memory mapping, must be rebuilt,
    and a file whose time changed without its content must not be.

Cases run in a scratch directory with a vendored directory source, so nothing needs the network.
"""

import argparse
import hashlib
import json
import mmap
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

DROPPED_ENV = ("CARGO_", "RUSTC", "RUSTFLAGS", "RUSTDOCFLAGS", "RUSTUP_TOOLCHAIN", "XDG_", "FRUST_",
               "SOURCE_DATE_EPOCH")
MEMO = re.compile(r"frust_resolve_memo: (hit|miss|recorded|not eligible)")
COMPILING = re.compile(r"^\s*Compiling (\S+)", re.M)
RUNNING = re.compile(r"^\s*Running `(.*)`$", re.M)


class Failure(Exception):
    pass


def expect(condition, message):
    if not condition:
        raise Failure(message)


VENDOR = {
    "leaf": ("", '[features]\nx = []\n', 'pub fn id() -> &\'static str { if cfg!(feature = "x") { "leaf+x" } else { "leaf" } }\n'),
    "devonly": ('leaf = { version = "1", features = ["x"] }\n', "", "pub fn id() -> &'static str { leaf::id() }\n"),
    "helper": ("", "", "pub fn id() -> &'static str { \"helper\" }\n"),
}


def write_checksums(base):
    files = {}
    for path in sorted(base.rglob("*")):
        if path.is_file() and path.name != ".cargo-checksum.json":
            files[path.relative_to(base).as_posix()] = hashlib.sha256(path.read_bytes()).hexdigest()
    (base / ".cargo-checksum.json").write_text(json.dumps({"files": files, "package": None}))


def write_vendor(vendor):
    for name, (dependencies, extra, source) in VENDOR.items():
        base = vendor / name
        (base / "src").mkdir(parents=True)
        (base / "src/lib.rs").write_text(source)
        (base / "Cargo.toml").write_text(
            f'[package]\nname = "{name}"\nversion = "1.0.0"\nedition = "2021"\n\n{extra}\n[dependencies]\n{dependencies}')
        write_checksums(base)


APP_MAIN = '''fn main() {
    println!("leaf={}", leaf::id());
    println!("member={}", member::name());
    println!("ext={}", ext::name());
    println!("data={}", include_str!("data.txt").trim());
}
'''
MEMBER_LIB = "pub fn name() -> &'static str { \"member\" }\n"
EXT_LIB = "pub fn name() -> &'static str { \"ext\" }\n"
BUILD_RS = "fn main() { println!(\"cargo:rerun-if-changed=build.rs\"); let _ = helper::id(); }\n"
APP_TEST = "#[test]\nfn dev_dependency() { assert_eq!(devonly::id(), \"leaf+x\"); }\n"


class Env:
    def __init__(self, args, work):
        self.fcargo, self.rustc, self.work = args.fcargo, args.rustc, work
        self.timeout = args.timeout
        self.previous = args.previous_fcargo
        self.vendor = work / "vendor"
        self.cargo_home = work / "cargo-home"

    def setup(self):
        self.cargo_home.mkdir(parents=True, exist_ok=True)
        write_vendor(self.vendor)

    def config(self):
        return ('[source.crates-io]\nreplace-with = "vendored"\n\n[source.vendored]\n'
                f'directory = {json.dumps(str(self.vendor))}\n')

    def project(self, name, resolver="2", extra_root="", features=True):
        """A workspace of the root package app and the member `member`, with ext outside it."""
        root = self.work / name / "app"
        if root.parent.exists():
            shutil.rmtree(root.parent)
        for directory in (root / ".cargo", root / "src", root / "tests", root / "member/src", root.parent / "ext/src"):
            directory.mkdir(parents=True)
        (root / ".cargo/config.toml").write_text(self.config())
        (root / "Cargo.toml").write_text(
            '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n'
            f'build = "build.rs"\n\n[workspace]\nmembers = ["member"]\nresolver = "{resolver}"\n\n'
            '[features]\nx = ["leaf/x"]\n\n[dependencies]\n'
            'leaf = "1"\nmember = { path = "member" }\next = { path = "../ext" }\n\n'
            '[build-dependencies]\nhelper = "1"\n\n[dev-dependencies]\ndevonly = "1"\n' + extra_root)
        (root / "src/main.rs").write_text(APP_MAIN)
        (root / "src/data.txt").write_text("data-one\n")
        (root / "build.rs").write_text(BUILD_RS)
        (root / "tests/dev.rs").write_text(APP_TEST)
        (root / "member/Cargo.toml").write_text(
            '[package]\nname = "member"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n')
        (root / "member/src/lib.rs").write_text(MEMBER_LIB)
        (root.parent / "ext/Cargo.toml").write_text(
            '[package]\nname = "ext"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n')
        (root.parent / "ext/src/lib.rs").write_text(EXT_LIB)
        return root

    def environment(self, target, extra=None):
        env = {key: value for key, value in os.environ.items()
               if key != "CARGO" and not key.startswith(DROPPED_ENV)}
        env.update(CARGO_HOME=str(self.cargo_home), CARGO_TARGET_DIR=str(target), RUSTC=str(self.rustc),
                   CARGO_TERM_COLOR="never", FRUST_PROGRESS="plain", FRUST_UNIT_CACHE="0",
                   FRUST_HOME=str(self.work / "frust-home"), CARGO_LOG="frust_resolve_memo=debug")
        for key, value in (extra or {}).items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        return env

    def run(self, root, *args, target=None, env=None, cargo=None, check=True):
        target = target or root.parent / "target"
        command = [str(cargo or self.fcargo), *args, "--offline"]
        done = subprocess.run(command, cwd=root, env=self.environment(target, env), stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, timeout=self.timeout)
        if check:
            expect(done.returncode == 0, f"{' '.join(command)} exited {done.returncode}:\n{done.stderr[-3000:]}")
        return done


def prime(env, root):
    """The first build creates Cargo.lock and the build directory, the second records the memo."""
    env.run(root, "build")
    done = env.run(root, "build")
    expect(memo_events(done) == ["miss", "recorded"], f"priming events {memo_events(done)}")


def memo_events(done):
    return MEMO.findall(done.stderr)


def compiled(done):
    return COMPILING.findall(done.stderr)


def memo_files(root):
    return sorted((root.parent / "target").glob(".frust-resolve-memo-*"))


CASES = []


def case(function):
    CASES.append(function)
    return function


@case
def memo_is_recorded_then_hit(env):
    root = env.project("lifecycle")
    first = env.run(root, "build")
    expect(memo_events(first) == [] and not memo_files(root), f"a build that creates Cargo.lock: {memo_events(first)}")
    second = env.run(root, "build")
    expect(memo_events(second) == ["miss", "recorded"], f"second build memo events {memo_events(second)}")
    expect(len(memo_files(root)) == 1, "the second build did not leave one memo file")
    third = env.run(root, "build")
    expect(memo_events(third) == ["hit"], f"third build memo events {memo_events(third)}")
    expect(not compiled(third), f"the no-op build compiled {compiled(third)}")
    locked = env.run(root, "build", "--locked")
    expect(memo_events(locked) == ["hit"], f"--locked and --offline are not part of the digest: {memo_events(locked)}")


@case
def memo_off_leaves_nothing(env):
    root = env.project("off")
    done = env.run(root, "build", env={"FRUST_RESOLVE_MEMO": "off"})
    expect(memo_events(done) == [] and not memo_files(root), f"memo ran when off: {memo_events(done)}")
    stock = env.run(root, "build", cargo=env.previous or env.fcargo, env={"FRUST_RESOLVE_MEMO": "off"})
    expect(stock.returncode == 0, "memo-off build failed")


@case
def stock_cargo_name_is_unchanged(env):
    root = env.project("stockname")
    cargo = env.fcargo.with_name("cargo")
    if not cargo.exists():
        raise Failure(f"no cargo next to fcargo: {cargo}")
    done = env.run(root, "build", cargo=cargo, env={"FRUST_INVOCATION": None})
    expect(memo_events(done) == [], f"a run as cargo used the memo: {memo_events(done)}")


@case
def verify_mode_agrees_with_a_recorded_memo(env):
    root = env.project("verify")
    prime(env, root)
    done = env.run(root, "build", env={"FRUST_RESOLVE_MEMO": "verify"})
    expect(memo_events(done) == ["hit"], f"verify run events {memo_events(done)}")


def lock_text(root):
    return (root / "Cargo.lock").read_text()


def expect_miss_then_hit(env, root, label):
    changed = env.run(root, "build")
    expect(memo_events(changed) == ["miss", "recorded"], f"{label}: expected a miss then a recording, got {memo_events(changed)}")
    again = env.run(root, "build")
    expect(memo_events(again) == ["hit"], f"{label}: expected a hit after recording, got {memo_events(again)}")
    return changed


@case
def root_manifest_edit_misses(env):
    root = env.project("manifest")
    prime(env, root)
    before = lock_text(root)
    (root / "member/Cargo.toml").write_text(
        (root / "member/Cargo.toml").read_text().replace("[dependencies]\n", '[dependencies]\nleaf = "1"\n'))
    expect_miss_then_hit(env, root, "member manifest edit")
    expect(lock_text(root) != before, "the lock file was not updated for the new dependency of member")
    expect("leaf" in lock_text(root).split('name = "member"')[1].split("[[package]]")[0],
           "member does not list leaf in Cargo.lock")


@case
def external_path_dependency_edit_misses(env):
    root = env.project("extedit")
    prime(env, root)
    ext = root.parent / "ext/Cargo.toml"
    ext.write_text(ext.read_text().replace("[dependencies]\n", '[dependencies]\nhelper = "1"\n'))
    expect_miss_then_hit(env, root, "external path dependency edit")
    ext_entry = lock_text(root).split('name = "ext"')[1].split("[[package]]")[0]
    expect("helper" in ext_entry, "the lock file does not list helper under ext")


@case
def lock_file_edit_misses(env):
    root = env.project("lockedit")
    prime(env, root)
    lock = root / "Cargo.lock"
    text = lock.read_text()
    lock.write_text(text.replace("[[package]]", "# edited\n[[package]]", 1) if "# edited" not in text else text)
    changed = env.run(root, "build")
    expect(memo_events(changed)[:1] == ["miss"], f"lock edit events {memo_events(changed)}")


@case
def config_edit_misses(env):
    root = env.project("config")
    prime(env, root)
    config = root / ".cargo/config.toml"
    config.write_text(config.read_text() + '\n[env]\nPROBE = "1"\n')
    expect_miss_then_hit(env, root, "config edit")
    env_change = env.run(root, "build", env={"CARGO_NET_RETRY": "9"})
    expect(memo_events(env_change)[:1] == ["miss"], f"CARGO_* change events {memo_events(env_change)}")


@case
def patch_is_never_eligible(env):
    root = env.project("patch")
    shutil.copytree(env.vendor / "leaf", root.parent / "leafpatch")
    (root.parent / "leafpatch/.cargo-checksum.json").unlink()
    manifest = root / "Cargo.toml"
    manifest.write_text(manifest.read_text() + '\n[patch.crates-io]\nleaf = { path = "../leafpatch" }\n')
    env.run(root, "build")
    for _ in range(2):
        done = env.run(root, "build")
        expect(memo_events(done) == ["not eligible"], f"patched workspace events {memo_events(done)}")


@case
def locked_flag_still_rejects_an_out_of_date_lock(env):
    root = env.project("locked")
    prime(env, root)
    (root / "member/Cargo.toml").write_text(
        (root / "member/Cargo.toml").read_text().replace("[dependencies]\n", '[dependencies]\nleaf = "1"\n'))
    off = env.run(root, "build", "--locked", env={"FRUST_RESOLVE_MEMO": "off"}, check=False)
    on = env.run(root, "build", "--locked", check=False)
    expect(off.returncode != 0 and on.returncode != 0, "--locked accepted a lock file that needs an update")
    expect("cannot update the lock file" in on.stderr and "cannot update the lock file" in off.stderr,
           f"--locked messages differ:\n{on.stderr[-800:]}\n---\n{off.stderr[-800:]}")


COMMANDS = [
    ("build",), ("build", "--features", "x"), ("build", "--release"), ("build", "-p", "member"),
    ("build", "--workspace"), ("test", "--no-run"), ("build", "--all-targets"), ("check", "--tests"),
    ("build", "--bins"), ("check",),
]


def compiler_lines(done, root):
    lines = []
    for command in RUNNING.findall(done.stderr):
        if " --crate-name " in command:
            lines.append(command.replace(str(root.parent), "$ROOT"))
    return sorted(lines)


def clear_outputs(root):
    target = root.parent / "target"
    for child in list(target.iterdir()):
        if child.name.startswith(".frust-resolve-memo-"):
            continue
        shutil.rmtree(child) if child.is_dir() else child.unlink()


@case
def compiler_commands_equal_with_and_without_the_memo(env):
    for resolver in ("2", "1"):
        root = env.project(f"equal{resolver}", resolver=resolver)
        prime(env, root)
        for command in COMMANDS:
            clear_outputs(root)
            off = env.run(root, *command, "-v", env={"FRUST_RESOLVE_MEMO": "off"})
            clear_outputs(root)
            on = env.run(root, *command, "-v")
            expect(memo_events(on)[:1] == ["hit"],
                   f"resolver {resolver} {command}: the second run did not hit: {memo_events(on)}")
            expect(compiler_lines(off, root) and compiler_lines(off, root) == compiler_lines(on, root),
                   f"resolver {resolver} {command}: compiler commands differ\n"
                   + "\n".join(set(compiler_lines(off, root)) ^ set(compiler_lines(on, root))))


@case
def tests_still_build_their_dev_dependency(env):
    root = env.project("devtests")
    prime(env, root)
    done = env.run(root, "test")
    expect(memo_events(done) == ["hit"], f"test run events {memo_events(done)}")
    expect(re.search(r"test dev_dependency \.\.\. ok", done.stdout), f"the dev dependency test did not run:\n{done.stdout}")


# ---- content fingerprint -------------------------------------------------------------------

FP_MAIN = '''static DATA: &[u8] = include_bytes!("data.bin");
fn main() {
    let sum: u64 = DATA.iter().map(|b| u64::from(*b) * 31 + 7).sum();
    println!("{} {}", DATA.len(), sum);
}
'''


def fp_project(env, name, data):
    root = env.work / name / "fp"
    if root.parent.exists():
        shutil.rmtree(root.parent)
    (root / "src").mkdir(parents=True)
    (root / "Cargo.toml").write_text('[package]\nname = "fp"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n')
    (root / "src/main.rs").write_text(FP_MAIN)
    (root / "src/data.bin").write_bytes(data)
    return root


def run_fp(env, root):
    done = env.run(root, "run")
    return done, done.stdout.strip()


def expected(data):
    return f"{len(data)} {sum(b * 31 + 7 for b in data)}"


def edit_keeping_mtime(path, edit):
    stat = path.stat()
    data = edit(path.read_bytes())
    path.write_bytes(data)
    os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))
    return data


def prove_same_size_edit(env, name, data, position):
    root = fp_project(env, name, data)
    first, out = run_fp(env, root)
    expect(out == expected(data), f"initial output {out!r}")
    time.sleep(0.05)
    noop, _ = run_fp(env, root)
    expect(not compiled(noop), f"the no-op build compiled {compiled(noop)}")
    changed = edit_keeping_mtime(root / "src/data.bin",
                                 lambda old: old[:position] + bytes([old[position] ^ 0x5A]) + old[position + 1:])
    expect(len(changed) == len(data), "the edit changed the size")
    done, out = run_fp(env, root)
    expect(compiled(done) == ["fp"], f"a same-size edit with the mtime restored was not rebuilt: {compiled(done)}")
    expect(out == expected(changed), f"output after the edit {out!r}, wanted {expected(changed)!r}")
    again, _ = run_fp(env, root)
    expect(not compiled(again), f"the build after the edit compiled {compiled(again)}")


def pattern_bytes(length):
    return bytes((index * 131 + 17) % 251 for index in range(length))


@case
def same_size_edit_with_restored_mtime(env):
    prove_same_size_edit(env, "fp-small", pattern_bytes(40), 17)


@case
def same_size_edit_in_a_file_read_in_pieces(env):
    # Larger than the size below which one read returns the whole file.
    for position in (0, 70000, 100000 - 1):
        prove_same_size_edit(env, f"fp-mid-{position}", pattern_bytes(100000), position)


@case
def same_size_edit_in_a_large_file(env):
    # Larger than the files fingerprinted with XXH3 (8 MiB), which are hashed by streaming.
    prove_same_size_edit(env, "fp-large", pattern_bytes(9 * 1024 * 1024 + 11), 5 * 1024 * 1024)


@case
def same_size_edit_at_the_hash_length_boundaries(env):
    for length in (1, 3, 4, 8, 9, 16, 17, 128, 129, 240, 241, 1023, 1024, 1025):
        prove_same_size_edit(env, f"fp-len-{length}", pattern_bytes(length), length // 2)


@case
def appended_and_truncated_files_are_rebuilt(env):
    for label, edit in (("append", lambda old: old + b"!"), ("truncate", lambda old: old[:-1]),
                        ("append-large", lambda old: old + b"!" * 70000), ("empty", lambda old: b"")):
        data = pattern_bytes(100000)
        root = fp_project(env, f"fp-size-{label}", data)
        run_fp(env, root)
        changed = edit_keeping_mtime(root / "src/data.bin", edit)
        done, out = run_fp(env, root)
        expect(compiled(done) == ["fp"], f"{label}: not rebuilt")
        expect(out == expected(changed), f"{label}: output {out!r}")


@case
def mapped_write_is_rebuilt(env):
    data = pattern_bytes(4096)
    root = fp_project(env, "fp-mmap", data)
    path = root / "src/data.bin"
    with path.open("r+b") as handle, mmap.mmap(handle.fileno(), 0, access=mmap.ACCESS_WRITE) as mapped:
        original = mapped[:]
        mapped[:] = original
        time.sleep(0.25)
        run_fp(env, root)
        changed = bytes([original[0] ^ 0x5A]) + original[1:]
        mapped[:] = changed
        done, out = run_fp(env, root)
        expect(compiled(done) == ["fp"], "a write through a memory mapping was not rebuilt")
        expect(out == expected(changed), f"output {out!r}")


@case
def touching_without_changing_is_not_rebuilt(env):
    root = fp_project(env, "fp-touch", pattern_bytes(1000))
    run_fp(env, root)
    time.sleep(0.05)
    os.utime(root / "src/data.bin")
    done, _ = run_fp(env, root)
    expect(not compiled(done), f"a touched but identical file was rebuilt: {compiled(done)}")


@case
def deleted_file_fails_the_build(env):
    root = fp_project(env, "fp-gone", pattern_bytes(1000))
    run_fp(env, root)
    (root / "src/data.bin").unlink()
    done = env.run(root, "run", check=False)
    expect(done.returncode != 0 and "data.bin" in done.stderr, f"a deleted include was not noticed:\n{done.stderr[-800:]}")


@case
def previous_release_dep_info_is_accepted(env):
    if not env.previous:
        raise Failure("SKIP no --previous-fcargo given")
    data = pattern_bytes(5000)
    root = fp_project(env, "fp-previous", data)
    done = env.run(root, "run", cargo=env.previous)
    expect(compiled(done) == ["fp"], "the previous release did not build")
    noop, _ = run_fp(env, root)
    expect(not compiled(noop), f"a target built by the previous release was rebuilt: {compiled(noop)}")
    changed = edit_keeping_mtime(root / "src/data.bin", lambda old: bytes([old[0] ^ 1]) + old[1:])
    done, out = run_fp(env, root)
    expect(compiled(done) == ["fp"] and out == expected(changed), "an edit after the previous release was missed")


def run_cases(env, only):
    counts = {"PASS": 0, "FAIL": 0, "SKIP": 0}
    for function in CASES:
        name = function.__name__.replace("_", "-")
        if only and not any(part in name for part in only):
            continue
        try:
            function(env)
            status, detail = "PASS", ""
        except Failure as failure:
            if str(failure).startswith("SKIP"):
                status, detail = "SKIP", f": {failure}"
            else:
                status, detail = "FAIL", f": {failure}"
        except Exception as error:
            status, detail = "FAIL", f": unexpected {type(error).__name__}: {error}"
        counts[status] += 1
        print(f"{status} {name}{detail}", flush=True)
    print(f"{counts['PASS']} passed, {counts['FAIL']} failed, {counts['SKIP']} skipped")
    return counts["FAIL"]


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fcargo", type=Path, required=True)
    parser.add_argument("--rustc", type=Path, required=True, help="frustc")
    parser.add_argument("--previous-fcargo", type=Path, help="fcargo of an earlier release for the dep-info case")
    parser.add_argument("--work", type=Path, required=True, help="empty or absent scratch directory")
    parser.add_argument("--timeout", type=float, default=600)
    parser.add_argument("--only", action="append", help="run cases whose name contains this text")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    args.fcargo, args.rustc = args.fcargo.absolute(), args.rustc.absolute()
    if args.previous_fcargo:
        args.previous_fcargo = args.previous_fcargo.absolute()
    work = args.work.resolve()
    if work.exists() and (not work.is_dir() or any(work.iterdir())):
        parser.error(f"--work {work} must be an empty directory or not exist")
    work.mkdir(parents=True, exist_ok=True)
    env = Env(args, work)
    try:
        env.setup()
        failures = run_cases(env, args.only)
    finally:
        if args.keep:
            print(f"kept {work}")
        else:
            shutil.rmtree(work, ignore_errors=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
