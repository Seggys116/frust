#!/usr/bin/env python3
"""Exercise managed binary retention with cooperative checks and test outputs."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import time


def prove(root, fcargo, cargo):
    source = root / "src"
    source.mkdir()
    config = root / "config/frust"
    config.mkdir(parents=True)
    store = root / "store"
    (config / "config.json").write_text(json.dumps(dict(version=1, target_root=str(store))))
    (root / "Cargo.toml").write_text(
        '[package]\nname = "retention_probe"\nversion = "0.0.0"\nedition = "2024"\n'
        '[[bin]]\nname = "other"\npath = "src/other.rs"\n[features]\n'
        + "".join(f"variant{i} = []\n" for i in range(7))
    )
    branches = "".join(f'if cfg!(feature = "variant{i}") {{ println!("{i}"); }}\n'
                       for i in range(7))
    (source / "main.rs").write_text("fn main() {\n" + branches + "}\n")
    (source / "other.rs").write_text('fn main() { println!("other"); }\n')
    env = {key: value for key, value in os.environ.items() if key in ("HOME", "CARGO_HOME")}
    env.update(PATH=f"{cargo.parent}:/usr/bin:/bin", XDG_CONFIG_HOME=str(config.parent),
               FRUST_CARGO=str(cargo), FRUST_PROGRESS="plain", CARGO_TERM_COLOR="never")

    def command(action, revision, binary="retention_probe", extra=()):
        return [fcargo, action, "--offline", "-j", "1", "--bin", binary,
                "--features", f"variant{revision}", "--message-format=json", *extra]

    def run(action, revision, binary="retention_probe", extra=()):
        out = subprocess.check_output(command(action, revision, binary, extra),
                                      cwd=root, env=env, text=True)
        return [row for line in out.splitlines() if line.startswith("{")
                if (row := json.loads(line))["reason"] == "compiler-artifact"]

    for revision in range(6):
        run("check", revision)
    profile, = (store / "targets").glob("*/*/debug")
    package = profile / "build/retention_probe"
    assert len(list(package.glob("*/fingerprint"))) == 6

    def retained(binary):
        return list(package.glob(f"*/.frust-published-bin-v2-{binary}"))

    assert not retained("retention_probe")
    tests = [Path(record["executable"]) for record in run("test", 0, extra=("--no-run",))
             if record["profile"]["test"] and record["executable"]]
    assert tests and all(test.is_file() for test in tests)
    run("build", 0, "other")
    other, = retained("other")
    for revision in range(6):
        run("build", revision)
    assert len(retained("retention_probe")) == 4

    sentinel = package / "0123456789abcdef/fingerprint"
    sentinel.mkdir(parents=True)
    (sentinel / "keep").write_text("untracked by retention")
    log = root / "locked-build.log"
    with (profile / ".cargo-build-lock").open() as lock, log.open("w") as output:
        fcntl.flock(lock, fcntl.LOCK_SH)
        build = subprocess.Popen(command("build", 6), cwd=root, env=env, stdout=output,
                                 stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 10
            while "Blocking waiting for file lock" not in log.read_text():
                if build.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("exclusive build did not wait for the shared lock")
                time.sleep(0.02)
            assert len(retained("retention_probe")) == 4
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)
            try:
                assert build.wait(timeout=20) == 0, log.read_text()
            except subprocess.TimeoutExpired:
                build.kill()
                build.wait()
                raise

    assert (sentinel / "keep").read_text() == "untracked by retention"
    assert len(retained("retention_probe")) == 4
    assert other.is_file() and all(test.is_file() for test in tests)
    assert subprocess.check_output([profile / "retention_probe"], text=True).strip() == "6"
    previous = run("build", 5)
    assert previous and all(record["fresh"] for record in previous)
    assert subprocess.check_output([profile / "retention_probe"], text=True).strip() == "5"
    print("PASS managed retention waits for readers and preserves tests, other binaries and warm builds")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fcargo", type=Path, required=True)
    parser.add_argument("--cargo", type=Path, required=True)
    args = parser.parse_args()
    work = Path(__file__).resolve().parents[1] / "target/compiler-rewrite"
    work.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=work, prefix="managed-retention-") as scratch:
        prove(Path(scratch), args.fcargo.resolve(), args.cargo.resolve())


if __name__ == "__main__":
    main()
