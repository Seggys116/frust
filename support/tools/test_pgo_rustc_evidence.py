#!/usr/bin/env python3
"""Tests of the profile-use checks of pgo-rustc-evidence.py on synthetic build logs.
Run: python3 -I support/tools/test_pgo_rustc_evidence.py"""

import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

TOOL = Path(__file__).resolve().with_name("pgo-rustc-evidence.py")
RUSTC = "/stored/rustc-pgo.profdata"
CARGO = "/stored/cargo-pgo.profdata"


class EvidenceTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp(prefix="frust-evidence-test-"))
        self.addCleanup(shutil.rmtree, self.dir, True)
        self.evidence = self.dir / "evidence.json"
        self.log = self.dir / "build.log"

    def write_log(self, rustc=60, cargo=30, other=0, mismatches=0, missing=0):
        lines = []
        for index in range(rustc):
            lines.append(f"Running `rustc --crate-name c{index} -Cprofile-use={RUSTC} -Cllvm-args=-pgo-warn-missing-function`")
        for index in range(cargo):
            lines.append(f"Running `rustc --crate-name d{index} -Cprofile-use={CARGO} -Cllvm-args=-pgo-warn-missing-function`")
        for index in range(other):
            lines.append(f"Running `rustc --crate-name e{index} -Cprofile-use=/elsewhere.profdata`")
        for index in range(mismatches):
            lines.append(f"warning: Function f{index}: hash mismatch) f{index}")
        for index in range(missing):
            lines.append(f"warning: no profile data available for function g{index}")
        self.log.write_text("\n".join(lines) + "\n")

    def run_tool(self, *args):
        return subprocess.run([sys.executable, "-I", str(TOOL), str(self.evidence), *args], capture_output=True, text=True)

    def seed(self, rustc_functions=1000, cargo_functions=0):
        data = {"train": {"merged": {"functions": rustc_functions}}}
        if cargo_functions:
            data["train_cargo"] = {"merged": {"functions": cargo_functions}}
        self.evidence.write_text(json.dumps(data))

    def test_both_profiles_used(self):
        self.write_log()
        result = self.run_tool("use-flags", str(self.log), RUSTC, "--also", CARGO)
        self.assertEqual(result.returncode, 0, result.stdout)
        detail = json.loads(self.evidence.read_text())["final_build"]
        self.assertEqual(detail["profile_use_commands"], 60)
        self.assertEqual(detail["also_profile_use_commands"], {"cargo-pgo.profdata": 30})
        self.assertEqual(detail["other_profile_commands"], 0)

    def test_cargo_profile_must_be_used_when_named(self):
        self.write_log(cargo=3)
        result = self.run_tool("use-flags", str(self.log), RUSTC, "--also", CARGO)
        self.assertEqual(result.returncode, 1)
        self.assertIn("cargo-pgo.profdata", result.stdout + result.stderr)

    def test_a_foreign_profile_is_refused(self):
        self.write_log(other=2)
        result = self.run_tool("use-flags", str(self.log), RUSTC, "--also", CARGO)
        self.assertEqual(result.returncode, 1)
        self.assertIn("different -Cprofile-use", result.stdout + result.stderr)

    def test_unlisted_cargo_profile_counts_as_foreign(self):
        self.write_log()
        result = self.run_tool("use-flags", str(self.log), RUSTC)
        self.assertEqual(result.returncode, 1)

    def test_mismatch_percent_counts_the_functions_of_every_profile(self):
        self.seed(rustc_functions=1000, cargo_functions=1000)
        self.write_log(mismatches=80)
        self.assertEqual(self.run_tool("warnings", str(self.log)).returncode, 0)
        self.seed(rustc_functions=1000)
        self.assertEqual(self.run_tool("warnings", str(self.log)).returncode, 1)

    def test_stored_components_are_summed(self):
        self.evidence.write_text(json.dumps({"train": {"components": {"rustc": {"functions": 600},
                                                                      "cargo": {"functions": 400}}}}))
        self.write_log(mismatches=40)
        self.assertEqual(self.run_tool("warnings", str(self.log)).returncode, 0)
        self.write_log(mismatches=60)
        self.assertEqual(self.run_tool("warnings", str(self.log)).returncode, 1)


if __name__ == "__main__":
    unittest.main()
