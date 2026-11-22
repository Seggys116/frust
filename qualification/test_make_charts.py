#!/usr/bin/env python3
"""Checks qualification/make-charts.py and the helpers of qualification/ci-bench.py on synthetic results.

The benchmark inputs are generated here with known ratios, so every verdict can be checked against the truth
they were made from. Standard library only. Run with: python3 -I qualification/test_make_charts.py
"""

import importlib.util
import json
import math
import pathlib
import random
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
PLATFORMS = ("linux-x86_64", "linux-x86_64-v3", "macos-arm64", "macos-x86_64")
WORKLOADS = ("ripgrep", "syn", "regex", "fd")
EDITS = {"ripgrep": ("leaf", "wide"), "syn": ("leaf", "wide"), "regex": ("leaf", "wide"), "fd": ("holdout",)}


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, HERE / file)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def write_job(directory, label, workload, truth, rounds=20, seed=1, spread=0.02, shard=None, noop_runs=5):
    """One compare job. truth maps a scenario to the true fRust/official ratio. Noise: every sample has its own
    log-normal error, and 15 percent of builds are slowed by up to 50 percent (a neighbour)."""
    rng = random.Random(f"{seed}/{label}/{workload}/{shard}")

    def slowed():
        extra = rng.uniform(0.0, 0.5) if rng.random() < 0.15 else 0.0
        return math.exp(rng.gauss(0.0, spread) + extra)

    scenarios = ["cold", "clean", "noop"] + [f"edit-{kind}" for kind in EDITS[workload]]
    records = [dict(schema="frust-compare-1", type="meta", label=label, workload=workload, rounds_requested=rounds,
                    host=dict(cpu_model="synthetic", logical_cpus=4, os="synthetic"), jobs=4,
                    toolchains=dict(official=dict(rustc="rustc 1.100.0 (abc 2026-11-09)\nrelease: 1.100.0", cargo="cargo 1.100.0"),
                                    frust=dict(rustc="rustc 1.100.0 (abc 2026-11-09)", cargo="cargo 1.100.0")))]
    for round_number in range(rounds):
        for slot, scenario in enumerate(scenarios):
            official_first = (round_number + slot) % 2 == 0
            base = {"cold": 100.0, "clean": 100.0, "noop": 0.05}.get(scenario, 30.0)
            if scenario == "noop":
                mine = [base * slowed() for _ in range(noop_runs)]
                theirs = [base * truth[scenario] * slowed() for _ in range(noop_runs)]
                pair = {"official": mine, "frust": theirs}
                for position, name in enumerate(("official", "frust") if official_first else ("frust", "official")):
                    records.append(dict(schema="frust-compare-1", type="sample", toolchain=name, scenario=scenario,
                                        round=round_number, position=position, status=0, compiled=0,
                                        cpu=sorted(pair[name])[noop_runs // 2], cpu_samples=pair[name], interleaved=True))
                continue
            cpu = {"official": base * slowed(), "frust": base * truth.get(scenario, 1.0) * slowed()}
            for position, name in enumerate(("official", "frust") if official_first else ("frust", "official")):
                records.append(dict(schema="frust-compare-1", type="sample", toolchain=name, scenario=scenario,
                                    round=round_number, position=position, status=0, compiled=3,
                                    cpu=cpu[name], cache_events={"hit": 4, "miss": 1} if name == "frust" else {}))
    suffix = f"-s{shard}" if shard is not None else ""
    folder = pathlib.Path(directory) / f"compare-{label}-{workload}{suffix}"
    folder.mkdir(parents=True, exist_ok=True)
    (folder / "results.jsonl").write_text("".join(json.dumps(r, sort_keys=True) + "\n" for r in records))
    return folder


def run_charts(raw, out, *extra):
    done = subprocess.run([sys.executable, "-I", str(HERE / "make-charts.py"), str(raw), "--out", str(out),
                           "--bootstrap", "300", *extra], capture_output=True, text=True)
    return done


WIN = {"cold": 0.95, "clean": 0.5, "noop": 0.9, "edit-leaf": 0.93, "edit-wide": 0.97, "edit-holdout": 0.95}
PARITY = dict(WIN, cold=1.0)


class MakeCharts(unittest.TestCase):
    def build(self, truth_for, rounds=20, platforms=PLATFORMS):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        raw = pathlib.Path(self.tmp.name) / "raw"
        for label in platforms:
            for workload in WORKLOADS:
                write_job(raw, label, workload, truth_for(label, workload), rounds=rounds)
        return raw

    def report(self, raw, *extra):
        out = pathlib.Path(self.tmp.name) / "out"
        done = run_charts(raw, out, *extra)
        self.assertEqual(done.returncode, 0, done.stderr)
        return out, json.loads((out / "verdict.json").read_text())

    def test_bar_met_when_every_cell_is_a_real_win(self):
        out, verdict = self.report(self.build(lambda label, workload: WIN))
        self.assertTrue(verdict["bar_met"], verdict["failing"])
        self.assertEqual(len(verdict["cells"]), 16)
        self.assertTrue(all(cell["verdict"] == "win" for cell in verdict["cells"]))
        cold = next(cell for cell in verdict["cells"] if cell["platform"] == "macos-arm64" and cell["scenario"] == "cold")
        self.assertAlmostEqual(cold["fRust_percent_of_official"], 95.0, delta=2.0)
        text = (out / "results.md").read_text()
        self.assertIn("**MET**", text)
        for platform in PLATFORMS:
            self.assertTrue((out / f"chart-{platform}.svg").is_file())

    def test_one_parity_cell_fails_the_bar_and_is_named(self):
        def truth(label, workload):
            return PARITY if label == "macos-arm64" else WIN
        out, verdict = self.report(self.build(truth))
        self.assertFalse(verdict["bar_met"])
        self.assertTrue(verdict["bar_met_for_measured_cells"] is False)
        self.assertEqual(len(verdict["failing"]), 1, verdict["failing"])
        self.assertIn("macos-arm64 cold", verdict["failing"][0])
        self.assertIn("**NOT MET**", (out / "results.md").read_text())

    def test_a_workload_loss_fails_the_bar_even_when_the_cell_wins(self):
        def truth(label, workload):
            if label == "linux-x86_64" and workload == "fd":
                return dict(WIN, noop=1.3)
            return dict(WIN, noop=0.8)
        _, verdict = self.report(self.build(truth))
        cell = next(c for c in verdict["cells"] if c["platform"] == "linux-x86_64" and c["scenario"] == "noop")
        self.assertEqual(cell["workload_losses"], ["fd"])
        self.assertFalse(verdict["bar_met"])

    def test_a_missing_platform_is_not_a_pass(self):
        raw = self.build(lambda label, workload: WIN, platforms=PLATFORMS[:3])
        _, verdict = self.report(raw)
        self.assertFalse(verdict["bar_met"])
        self.assertTrue(verdict["bar_met_for_measured_cells"])
        self.assertEqual(len(verdict["missing"]), 4)

    def test_too_few_rounds_are_insufficient_not_a_win(self):
        _, verdict = self.report(self.build(lambda label, workload: WIN, rounds=6), "--min-pairs", "10")
        self.assertFalse(verdict["bar_met"])
        self.assertTrue(all(cell["verdict"] == "insufficient" for cell in verdict["cells"]))

    def test_output_is_byte_identical_for_the_same_input(self):
        raw = self.build(lambda label, workload: WIN, platforms=PLATFORMS[:1])
        first, _ = self.report(raw)
        again = pathlib.Path(self.tmp.name) / "again"
        self.assertEqual(run_charts(raw, again).returncode, 0)
        for name in ("results.md", "summary.json", "verdict.json", "chart-linux-x86_64.svg"):
            self.assertEqual((first / name).read_bytes(), (again / name).read_bytes(), name)

    def test_shards_of_a_job_are_pooled(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        raw = pathlib.Path(self.tmp.name) / "raw"
        for workload in WORKLOADS:
            for shard in (0, 1):
                write_job(raw, "linux-x86_64", workload, WIN, rounds=8, shard=shard, seed=shard + 5)
        _, verdict = self.report(raw, "--min-pairs", "10", "--require-platforms", "linux-x86_64")
        cold = next(c for c in verdict["cells"] if c["scenario"] == "cold")
        self.assertEqual(cold["pairs_per_workload"], {w: 16 for w in WORKLOADS})
        self.assertTrue(verdict["bar_met"], verdict["failing"])

    def test_enforce_exits_nonzero_when_the_bar_is_not_met(self):
        raw = self.build(lambda label, workload: PARITY, platforms=PLATFORMS[:1])
        done = run_charts(raw, pathlib.Path(self.tmp.name) / "out", "--enforce")
        self.assertEqual(done.returncode, 1)

    def test_noop_that_compiled_units_is_not_a_win(self):
        raw = self.build(lambda label, workload: WIN, platforms=PLATFORMS[:1])
        path = raw / "compare-linux-x86_64-syn" / "results.jsonl"
        records = [json.loads(line) for line in path.read_text().splitlines()]
        for record in records:
            if record.get("scenario") == "noop":
                record["compiled"] = 2
        path.write_text("".join(json.dumps(r, sort_keys=True) + "\n" for r in records))
        _, verdict = self.report(raw, "--require-platforms", "linux-x86_64")
        cell = next(c for c in verdict["cells"] if c["scenario"] == "noop")
        self.assertEqual(cell["verdict"], "recompiled")
        self.assertFalse(verdict["bar_met"])

    def test_resources_are_read_from_concurrent_builds_summaries(self):
        raw = self.build(lambda label, workload: WIN, platforms=PLATFORMS[:1])
        folder = raw / "compare-linux-x86_64-regex" / "contention"
        folder.mkdir()

        def ratio(median, low, high, evidence):
            return dict(median=median, confidence95=[low, high], evidence=evidence, pairs=12)
        summary = dict(kind="summary", cells=[
            dict(scenario="build-dev", count=3, state="cold", ratios=dict(
                max_rss_bytes=ratio(0.85, 0.84, 0.86, "lower"), total_bytes=ratio(0.7, 0.7, 0.7, "lower"),
                lock_waits_total=ratio(0.6, 0.55, 0.62, "lower"), cpu_total=ratio(1.0, 0.97, 1.03, "unresolved")))])
        (folder / "concurrent-builds.ndjson").write_text(json.dumps(summary) + "\n")
        out, verdict = self.report(raw, "--require-platforms", "linux-x86_64")
        self.assertTrue(verdict["resources_met"])
        self.assertEqual(verdict["resources"]["linux-x86_64"]["memory"]["better"], 1)
        worse = dict(summary)
        worse["cells"][0]["ratios"]["max_rss_bytes"] = ratio(1.2, 1.1, 1.3, "higher")
        (folder / "concurrent-builds.ndjson").write_text(json.dumps(worse) + "\n")
        _, verdict = self.report(raw, "--require-platforms", "linux-x86_64")
        self.assertFalse(verdict["resources_met"])
        self.assertIn("max_rss_bytes", verdict["resources"]["linux-x86_64"]["memory"]["worse_cells"][0])


class CiBenchHelpers(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bench = load("ci_bench", "ci-bench.py")

    def test_top_idle_is_read_from_the_last_sample(self):
        text = ("CPU usage: 50.0% user, 20.0% sys, 30.0% idle\nProcesses: 400 total\n"
                "CPU usage: 3.12% user, 4.00% sys, 92.88% idle\n")
        self.assertAlmostEqual(self.bench.parse_top_idle(text), 0.9288)
        self.assertIsNone(self.bench.parse_top_idle("nothing here"))

    def test_taskset_cpu_count(self):
        self.assertEqual(self.bench.count_cpus("0-3,6"), 5)

    def test_order_is_balanced_over_rounds_and_flips_between_slots(self):
        class Chain:
            def __init__(self, name):
                self.name = name
        bench = object.__new__(self.bench.Bench)
        bench.official, bench.frust = Chain("official"), Chain("frust")
        for slot in range(5):
            firsts = [bench.orders(round_number)(slot)[0].name for round_number in range(40)]
            self.assertEqual(firsts.count("official"), 20, slot)
            self.assertTrue(all(a != b for a, b in zip(firsts, firsts[1:])), "alternates every round")
        for round_number in range(6):
            order = bench.orders(round_number)
            self.assertNotEqual(order(0)[0].name, order(1)[0].name, "alternates between slots of a round")

    def test_sample_ratio_pairs_adjacent_noop_builds(self):
        loaded = load("make_charts", "make-charts.py")
        official = dict(cpu=0.05, cpu_samples=[0.05, 0.05, 0.10], interleaved=True)
        frust = dict(cpu=0.04, cpu_samples=[0.04, 0.05, 0.05], interleaved=True)
        ratio, off, fr = loaded.sample_ratio(official, frust)
        self.assertAlmostEqual(ratio, 0.8)  # median of 0.8, 1.0 and 0.5
        self.assertEqual((off, fr), (0.05, 0.05))
        plain = loaded.sample_ratio(dict(cpu=2.0), dict(cpu=1.0))
        self.assertEqual(plain, (0.5, 2.0, 1.0))
        self.assertIsNone(loaded.sample_ratio(dict(cpu=0.0), dict(cpu=1.0)))


if __name__ == "__main__":
    unittest.main()
