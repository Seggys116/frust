#!/usr/bin/env python3
"""Tests of pgo-rustc-bundle.py with a fake llvm-profdata: a bundle that fits is accepted, and each
way a stored profile can be stale or damaged is refused. Run: python3 -I support/tools/test_pgo_rustc_bundle.py"""

import json
import os
import platform
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

TOOL = Path(__file__).resolve().with_name("pgo-rustc-bundle.py")
COMMIT = "0ef1bc386059596dada8fb65b50425aab2ea637a"
LLVM = "23.1.5"
MACHINE = {"x86_64": "x86_64-apple-darwin", "amd64": "x86_64-apple-darwin", "arm64": "aarch64-apple-darwin",
           "aarch64": "aarch64-apple-darwin"}.get(platform.machine().lower())

FAKE_PROFDATA = """#!/bin/sh
case "$1" in
  --version) printf 'LLVM (http://llvm.org/):\\n  LLVM version %s-rust-1.100.0-stable\\n' "${FAKE_LLVM:-23.1.5}" ;;
  show)
    printf 'Instrumentation level: %s\\nTotal functions: %s\\nMaximum function count: 7\\n' \\
      "${FAKE_LEVEL:-IR}" "${FAKE_FUNCTIONS:-30000}" ;;
  *) exit 2 ;;
esac
"""


@unittest.skipIf(MACHINE is None, "bundle checks only exist for the two macOS machines")
class BundleTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp(prefix="frust-bundle-test-"))
        self.addCleanup(shutil.rmtree, self.dir, True)
        self.root = self.dir / "root"
        (self.root / "support/patches/rustc").mkdir(parents=True)
        for name, text in (("bootstrap.toml", "[build]\njobs = 12\n"), ("bootstrap-common.toml", "# c\nrust.codegen-units = 1\n"),
                           ("bootstrap-macos.toml", "# nothing\n")):
            (self.root / "support" / name).write_text(text)
        (self.root / "support/patches/0001-a.patch").write_text("a\n")
        (self.root / "support/patches/rustc/0001-b.patch").write_text("b\n")
        self.tool = self.dir / "llvm-profdata"
        self.tool.write_text(FAKE_PROFDATA)
        self.tool.chmod(self.tool.stat().st_mode | stat.S_IXUSR)
        self.profile = self.dir / "rustc.profdata"
        self.profile.write_bytes(b"profile bytes")
        self.evidence = self.dir / "evidence.json"
        self.evidence.write_text(json.dumps({"train": {
            "host": MACHINE, "profiles": ["Check", "Opt"], "scenarios": "All", "crates_complete": ["a", "b"],
            "raw_files": 12, "seconds": 3.5, "gates_failed": []}}))
        self.bundle = self.dir / "bundle"

    def run_tool(self, *args, env=None):
        merged = dict(os.environ)
        merged.update(env or {})
        return subprocess.run([sys.executable, "-I", str(TOOL), *args], capture_output=True, text=True, env=merged)

    def export(self, **env):
        return self.run_tool("export", str(self.root), str(self.bundle), "--host", MACHINE, "--rust-commit", COMMIT,
                             "--llvm-version", LLVM, "--llvm-profdata", str(self.tool), "--evidence", str(self.evidence),
                             "--component", f"rustc={self.profile}", env=env)

    def check(self, commit=COMMIT, host=MACHINE, llvm=LLVM, **env):
        evidence = self.dir / "use-evidence.json"
        return self.run_tool("check", str(self.root), str(self.bundle), "--host", host, "--rust-commit", commit,
                             "--llvm-version", llvm, "--llvm-profdata", str(self.tool), "--evidence", str(evidence),
                             env=env)

    def test_roundtrip_and_evidence(self):
        self.assertEqual(self.export().returncode, 0)
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        evidence = json.loads((self.dir / "use-evidence.json").read_text())["train"]
        self.assertEqual(evidence["source"], "stored")
        self.assertEqual(evidence["merged"]["functions"], 30000)
        self.assertEqual(evidence["merged"]["level"], "IR")

    def test_settings_hash_ignores_comments_and_jobs(self):
        before = self.run_tool("settings", str(self.root), MACHINE).stdout
        (self.root / "support/bootstrap.toml").write_text("[build]\njobs = 3\n")
        (self.root / "support/bootstrap-macos.toml").write_text("# other comment\n\n")
        self.assertEqual(self.run_tool("settings", str(self.root), MACHINE).stdout, before)
        (self.root / "support/bootstrap-common.toml").write_text("rust.codegen-units = 16\n")
        self.assertNotEqual(self.run_tool("settings", str(self.root), MACHINE).stdout, before)

    def refused(self, result, text):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(text, result.stdout + result.stderr)

    def test_refuses_other_commit(self):
        self.export()
        self.refused(self.check(commit="1" * 40), "rust-commit")

    def test_refuses_other_target(self):
        self.export()
        other = "aarch64-apple-darwin" if MACHINE.startswith("x86_64") else "x86_64-apple-darwin"
        self.refused(self.check(host=other), "target")

    def test_refuses_other_llvm(self):
        self.export()
        self.refused(self.check(llvm="24.0.0"), "llvm-version")

    def test_refuses_other_settings(self):
        self.export()
        (self.root / "support/bootstrap-common.toml").write_text("rust.codegen-units = 16\n")
        self.refused(self.check(), "settings-sha256")

    def test_refuses_profile_tool_of_another_llvm(self):
        self.export()
        self.refused(self.check(FAKE_LLVM="22.1.0"), "is not LLVM")

    def test_refuses_tampered_profile(self):
        self.export()
        (self.bundle / "rustc-pgo.profdata").write_bytes(b"other bytes")
        self.refused(self.check(), "does not match")

    def test_refuses_missing_manifest_and_wrong_version(self):
        self.export()
        manifest = self.bundle / "MANIFEST"
        text = manifest.read_text()
        manifest.write_text(text.replace("bundle-version 1", "bundle-version 9"))
        self.refused(self.check(), "bundle-version")
        manifest.unlink()
        self.refused(self.check(), "no MANIFEST")

    def test_refuses_profile_that_is_not_ir_or_too_small(self):
        self.refused(self.export(FAKE_LEVEL="FE"), "not a usable IR profile")
        shutil.rmtree(self.bundle, ignore_errors=True)
        self.refused(self.export(FAKE_FUNCTIONS="100"), "not a usable IR profile")
        shutil.rmtree(self.bundle, ignore_errors=True)
        self.assertEqual(self.export().returncode, 0)
        self.refused(self.check(FAKE_LEVEL="FE"), "instrumentation level")
        self.refused(self.check(FAKE_FUNCTIONS="25000"), "holds 25000 functions")

    def test_changed_patches_are_reported_not_refused(self):
        self.export()
        (self.root / "support/patches/0002-new.patch").write_text("n\n")
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("0002-new.patch", result.stdout)

    def test_export_refuses_failed_training_and_foreign_evidence(self):
        data = json.loads(self.evidence.read_text())
        data["train"]["gates_failed"] = ["only 1 crate"]
        self.evidence.write_text(json.dumps(data))
        self.refused(self.export(), "failed gates")
        data["train"].update(gates_failed=[], host="x86_64-unknown-linux-gnu")
        self.evidence.write_text(json.dumps(data))
        self.refused(self.export(), "not " + MACHINE)


if __name__ == "__main__":
    unittest.main()
