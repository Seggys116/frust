#!/usr/bin/env python3
"""Bisect where fRust loses to official Rust on macOS, with paired, interleaved, order-rotated rounds.

One run measures one cohort application on one host with up to seven configurations of the same
release build (the frozen profile of qualification/PROTOCOL.md, the command line and the clean
environment of qualification/ci-bench.py):

  A  official cargo and official rustc
  B  fcargo, every default (central target directory, unit cache), frustc
  C  as B with FRUST_UNIT_CACHE=0
  D  as B with the central target off (FRUST_CENTRAL_TARGET=off, and an explicit CARGO_TARGET_DIR as
     ci-bench.py uses it, so D is the configuration the benchmark workflow measures)
  E  as B with both the unit cache and the central target off
  F  official cargo driving frustc (separates cargo from the compiler)
  G  fcargo defaults driving the official rustc (separates the compiler from cargo)

Scenarios. Round r of a scenario builds every configuration once, in an order rotated by r:

  noop   median of --noops immediate rebuilds of a restored clean target
  edit   rebuild of a restored clean target after the frozen edit of qualification/edits
  cold   empty target; for fcargo configurations an empty store in a probe-owned FRUST_HOME

The clean target of a configuration is built once, unmeasured, and kept as a copy-on-write snapshot. Every
noop and edit round restores it first (APFS clone with cp -c, so a restore costs almost nothing) and deletes
the live target afterwards, which keeps the disk use at one snapshot per configuration. A restore is placed at
the exact path of the original target, so the central target keeps its hashed location. A no-op that
recompiles something is recorded (compiled) and reported.

Metrics per timed build. CPU is user plus system seconds from wait4 as in ci-bench.py, wall is the
monotonic wall time. On macOS every build runs under /usr/bin/time -l, whose stderr is parsed for
"instructions retired", "cycles elapsed", "peak memory footprint", "maximum resident set size" and the block
and context-switch counters. Those fields are the resource usage of the process that time waited for. Whether
they include the whole process tree (cargo, rustc, build scripts, the linker) or only the direct child depends
on the macOS release, so the run starts with a self-test: it prints the raw `time -l` output of a trivial
command, of an `sh -c` with a trivial child and of an `sh -c` whose grandchild does the work, and compares
them. If the grandchild work shows in the counters the instruction count is treated as a tree metric and is
the primary metric of the report. If it does not, the counts are recorded and shown but the report falls back
to CPU time as the primary metric, and says so. If the fields are missing or zero (a runner without access to
the counters), instructions are recorded as absent and CPU time is primary. Instruction counts are much less
noisy than CPU time on shared runners. Process counts are not recorded: macOS has no cheap, unprivileged
per-tree process counter. The block input and output operation counts of time -l are the file system figure.

A configuration whose build fails is dropped from the later rounds and listed in the report. A failure of A
ends the run. The time budget stops the run cleanly between builds, and the report says how many rounds each
scenario completed.

Output: JSON lines (one meta record, the self-test, one baseline record per configuration, one sample per
timed build or no-op median, stop records) and a markdown report with a table per scenario, the single
switch contrasts and a bisection summary generated from the data. `report` regenerates the markdown from the
JSON lines alone and runs on any host.
"""
import argparse
import collections
import datetime
import hashlib
import importlib.util
import json
import math
import os
import pathlib
import re
import shutil
import statistics
import subprocess
import sys
import time

if sys.version_info < (3, 9):
    sys.exit("probe-bench.py needs Python 3.9 or newer (os.waitstatus_to_exitcode)")

SCHEMA = "frust-probe-1"
TIME_BIN = "/usr/bin/time"
SCENARIOS = ("noop", "edit", "cold")
# Order inside one iteration of the round loop. Cold rounds are the slowest and fewest.
SCENARIO_ORDER = ("noop", "edit", "cold")

# key: description, which cargo and rustc, whether the central target and the unit cache are expected, extra env.
CONFIGS = {
    "A": dict(cargo="official", rustc="official", central=False, cache=None, env={},
              desc="official cargo and rustc", target="explicit", unit_cache="none"),
    "B": dict(cargo="frust", rustc="frust", central=True, cache=True, env={},
              desc="fcargo, all defaults", target="central", unit_cache="default (on)"),
    "C": dict(cargo="frust", rustc="frust", central=True, cache=False, env={"FRUST_UNIT_CACHE": "0"},
              desc="fcargo, unit cache off", target="central", unit_cache="off"),
    "D": dict(cargo="frust", rustc="frust", central=False, cache=True, env={"FRUST_CENTRAL_TARGET": "off"},
              desc="fcargo, central target off (explicit target, as the benchmark)", target="explicit",
              unit_cache="default (on)"),
    "E": dict(cargo="frust", rustc="frust", central=False, cache=False,
              env={"FRUST_CENTRAL_TARGET": "off", "FRUST_UNIT_CACHE": "0"},
              desc="fcargo, central target and unit cache off", target="explicit", unit_cache="off"),
    "F": dict(cargo="official", rustc="frust", central=False, cache=None, env={},
              desc="official cargo driving frustc", target="explicit", unit_cache="none"),
    "G": dict(cargo="frust", rustc="official", central=True, cache=True, env={},
              desc="fcargo defaults driving the official rustc", target="central", unit_cache="default (on)"),
}

# Single switch contrasts: (name, numerator, denominator, what it isolates). A value above 1 means the
# numerator costs more.
CONTRASTS = (
    ("B vs A", "B", "A", "total: fRust default against official"),
    ("F vs A", "F", "A", "compiler: frustc against rustc under official cargo"),
    ("G vs A", "G", "A", "cargo: fcargo defaults against official cargo under the official rustc"),
    ("B vs G", "B", "G", "compiler: frustc against rustc under fcargo defaults"),
    ("B vs F", "B", "F", "cargo: fcargo defaults against official cargo under frustc"),
    ("B vs C", "B", "C", "unit cache on against off, central target"),
    ("D vs E", "D", "E", "unit cache on against off, local target"),
    ("B vs D", "B", "D", "central against local target, unit cache on"),
    ("C vs E", "C", "E", "central against local target, unit cache off"),
    ("B vs E", "B", "E", "both fcargo switches default against both off"),
    ("E vs F", "E", "F", "rest of patched cargo: cache and central target off against official cargo, under frustc"),
    ("D vs A", "D", "A", "the benchmark-like configuration against official"),
    ("E vs A", "E", "A", "everything but the switches: fcargo with both off and frustc against official"),
)

# Smallest relative effect that is called real, and the pair spread (interquartile range of the per-pair
# ratio, in percentage points) above which a row is flagged noisy.
FLOOR = {"instructions": 0.01, "cpu": 0.03}
NOISE_PP = {"instructions": 2.0, "cpu": 5.0}
MIN_PAIRS = 3

TIME_HEADER = re.compile(r"^\s*[\d.]+ real\s+[\d.]+ user\s+[\d.]+ sys\s*$", re.M)
TIME_FIELD = re.compile(r"^\s*(\d+)\s+([A-Za-z][A-Za-z ]*?)\s*$", re.M)

CI = None  # qualification/ci-bench.py, loaded by `run` only


class BudgetExceeded(Exception):
    pass


class Fatal(Exception):
    pass


def load_ci_bench():
    """Import ci-bench.py, whose name is not a valid module name. Its helpers (Edit, copy_tree, host_info,
    the frozen profile) are reused so both drivers build and edit the same way."""
    sys.dont_write_bytecode = True
    path = pathlib.Path(__file__).resolve().with_name("ci-bench.py")
    spec = importlib.util.spec_from_file_location("ci_bench", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def wipe(*paths):
    for path in paths:
        shutil.rmtree(path, ignore_errors=True)


def median_or_none(values):
    values = [value for value in values if value is not None]
    return statistics.median(values) if values else None


def parse_time_l(text):
    """Fields of the report that `time -l` prints after the real/user/sys line, the last report in text."""
    headers = list(TIME_HEADER.finditer(text))
    if not headers:
        return {}
    fields = {}
    for value, label in TIME_FIELD.findall(text[headers[-1].end():]):
        fields[label] = int(value)
    return fields


def timed(command, cwd, env, log, use_time_l):
    """Run command, return its CPU and wall time, wait4 peak RSS in bytes, exit status and the time -l fields."""
    full = [TIME_BIN, "-l"] + command if use_time_l else command
    start = time.monotonic()
    with open(log, "wb") as sink:
        child = subprocess.Popen(full, cwd=cwd, env=env, stdout=sink, stderr=subprocess.STDOUT)
        _, status, usage = os.wait4(child.pid, 0)
    wall = time.monotonic() - start
    child.returncode = os.waitstatus_to_exitcode(status)
    fields = parse_time_l(pathlib.Path(log).read_text(errors="replace")) if use_time_l else {}
    maxrss = usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024)  # bytes on macOS, KiB on Linux
    return dict(cpu=usage.ru_utime + usage.ru_stime, wall=wall, wait4_maxrss=maxrss,
                status=child.returncode, fields=fields)


def metrics_of(run):
    fields = run["fields"]
    return dict(
        cpu=run["cpu"], wall=run["wall"],
        instructions=fields.get("instructions retired") or None,
        cycles=fields.get("cycles elapsed") or None,
        peak_footprint=fields.get("peak memory footprint") or None,
        max_rss=fields.get("maximum resident set size") or run["wait4_maxrss"],
        blocks_in=fields.get("block input operations"),
        blocks_out=fields.get("block output operations"),
        ctx_vol=fields.get("voluntary context switches"),
        ctx_invol=fields.get("involuntary context switches"),
        page_faults=fields.get("page faults"),
    )


METRIC_NAMES = ("cpu", "wall", "instructions", "cycles", "peak_footprint", "max_rss", "blocks_in",
                "blocks_out", "ctx_vol", "ctx_invol", "page_faults")


def time_l_selftest():
    """Print the raw `time -l` output of three commands and decide whether instructions cover the child tree."""
    cases = (
        ("trivial command", ["/usr/bin/true"]),
        ("sh -c with a trivial child", ["/bin/sh", "-c", "/usr/bin/true; exit 0"]),
        ("sh -c with a heavy grandchild", ["/bin/sh", "-c", '"$0" -c "sum(range(10000000))"; exit 0', sys.executable]),
    )
    results = {}
    for name, command in cases:
        done = subprocess.run([TIME_BIN, "-l"] + command, capture_output=True, text=True)
        print(f"--- time -l: {name}: {' '.join(command)} (exit {done.returncode})", flush=True)
        print(done.stderr.rstrip(), flush=True)
        fields = parse_time_l(done.stderr)
        results[name] = dict(status=done.returncode, instructions=fields.get("instructions retired") or None,
                             cycles=fields.get("cycles elapsed") or None, raw=done.stderr)
    trivial = results[cases[0][0]]["instructions"]
    light = results[cases[1][0]]["instructions"]
    heavy = results[cases[2][0]]["instructions"]
    if not trivial or not light or not heavy:
        mode, note = "none", "time -l did not report a non-zero instruction count; CPU time is the primary metric"
    elif heavy >= 20 * light:
        mode, note = "tree", ("the work of a grandchild shows in the count (%d against %d for a trivial child): "
                              "instructions cover the whole process tree" % (heavy, light))
    else:
        mode, note = "direct", ("the work of a grandchild does not show in the count (%d against %d for a trivial "
                                "child): instructions cover only the process time started, so CPU time is the "
                                "primary metric" % (heavy, light))
    print(f"time -l self-test: {note}", flush=True)
    return dict(mode=mode, note=note, cases=results)


class Config:
    """One configuration: which cargo and rustc, what environment, where its target and store live."""

    def __init__(self, key, spec, work, cargo, rustc, jobs):
        self.key, self.spec, self.work, self.cargo, self.rustc, self.jobs = key, spec, pathlib.Path(work), cargo, rustc, jobs
        self.fcargo = spec["cargo"] == "frust"
        self.central = spec["central"]
        self.snapshot = self.work / f"{key}-snapshot"

    def place(self, cold):
        suffix = "-cold" if cold else ""
        return dict(home=self.work / f"{self.key}{suffix}-home", target=self.work / f"{self.key}{suffix}-target",
                    xdg=self.work / f"{self.key}{suffix}-xdg")

    def target_root(self, place):
        """The directory that holds this configuration's target, which is what a snapshot restores."""
        return place["home"] / "targets" if self.central else place["target"]

    def store(self, place):
        return place["home"] / "artifacts" / "units"

    def env(self, place):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("CARGO", "RUSTC", "RUSTFLAGS", "FRUST", "XDG"))}
        env.update(CI.PROFILE)
        env.update(CARGO_HOME=str(pathlib.Path.home() / ".cargo"), CARGO_TERM_COLOR="never", FRUST_PROGRESS="plain",
                   XDG_CACHE_HOME=str(place["xdg"]), XDG_CONFIG_HOME=str(self.work / "xdg-config"),
                   PATH=os.pathsep.join([str(pathlib.Path(self.cargo).parent), env.get("PATH", "")]),
                   RUSTC=str(self.rustc))
        if not self.central:
            env["CARGO_TARGET_DIR"] = str(place["target"])
        if self.fcargo:
            # FRUST_HOME moves the store and the central target; XDG_CACHE_HOME is ignored on macOS.
            env["FRUST_HOME"] = str(place["home"])
        env.update(self.spec["env"])
        return env

    def command(self):
        return [str(self.cargo), "build", "--locked", "--offline", "-j", str(self.jobs), "--release"]

    def reset(self, place):
        wipe(place["home"], place["target"], place["xdg"])
        if self.fcargo:
            place["home"].mkdir(parents=True)


class Probe:
    def __init__(self, args, configs, edit, selftest, use_time_l):
        self.args, self.configs, self.edit, self.selftest, self.use_time_l = args, configs, edit, selftest, use_time_l
        self.app = pathlib.Path(args.app).resolve()
        self.work = pathlib.Path(args.work).resolve()
        self.logs = self.work / "logs"
        self.logs.mkdir(parents=True, exist_ok=True)
        self.workload = self.app.name
        self.started = time.monotonic()
        self.budget = args.time_budget_minutes * 60
        self.rounds = dict(noop=args.noop_rounds, edit=args.edit_rounds, cold=args.cold_rounds)
        self.durations = {scenario: [] for scenario in SCENARIOS}
        self.stopped = set()
        self.disabled = {}

    def emit(self, record):
        with open(self.args.out, "a") as handle:
            handle.write(json.dumps(record, sort_keys=True) + "\n")

    def elapsed(self):
        return time.monotonic() - self.started

    def check_deadline(self):
        if self.budget and self.elapsed() > self.budget:
            raise BudgetExceeded()

    def active(self):
        return [cfg for cfg in self.configs if cfg.key not in self.disabled]

    def build_once(self, cfg, place, log):
        self.check_deadline()
        run = timed(cfg.command(), self.app, cfg.env(place), log, self.use_time_l)
        run["compiled"] = CI.log_counts(log)[0]
        return run

    def drop(self, cfg, scenario, reason, log):
        self.disabled[cfg.key] = reason
        print(f"WARNING: configuration {cfg.key} dropped after a failed {scenario} build: {reason}; see {log}", flush=True)
        if cfg.key == "A":
            raise Fatal(f"the official configuration A failed ({scenario}); see {log}")

    def restore(self, cfg, place):
        root = cfg.target_root(place)
        wipe(root)
        CI.copy_tree(cfg.snapshot, root)

    def prepare(self, cfg):
        """Unmeasured clean build, a no-op check, and a snapshot of the clean target."""
        place = cfg.place(False)
        wipe(cfg.snapshot)
        cfg.reset(place)
        log = self.logs / f"{self.workload}-baseline-{cfg.key}.log"
        began = time.monotonic()
        run = self.build_once(cfg, place, log)
        if run["status"] != 0:
            self.drop(cfg, "baseline", f"exit {run['status']}", log)
            return
        check = self.build_once(cfg, place, self.logs / f"{self.workload}-baseline-{cfg.key}-noop.log")
        if check["status"] != 0:
            self.drop(cfg, "baseline no-op check", f"exit {check['status']}", log)
            return
        root = cfg.target_root(place)
        central_bytes = CI.STORE.allocated(place["home"] / "targets") if cfg.fcargo else 0
        target_bytes = CI.STORE.allocated(root)
        store_bytes = CI.STORE.allocated(cfg.store(place)) if cfg.fcargo else 0
        issues = []
        if cfg.spec["central"] and central_bytes == 0:
            issues.append("the central target directory is empty: the central target was not in effect")
        if cfg.fcargo and not cfg.spec["central"] and central_bytes != 0:
            issues.append("a central target was written although it is off for this configuration")
        if cfg.spec["cache"] is True and store_bytes == 0:
            issues.append("the unit cache store is empty after a clean build: the cache was not in effect")
        if cfg.spec["cache"] is False and store_bytes != 0:
            issues.append("the unit cache store is not empty although the cache is off")
        if target_bytes == 0:
            issues.append("the target is empty after the clean build")
        if check["compiled"] != 0:
            issues.append(f"the baseline no-op recompiled {check['compiled']} units")
        CI.copy_tree(root, cfg.snapshot)
        wipe(root)
        self.emit(dict(type="baseline", schema=SCHEMA, config=cfg.key, seconds=time.monotonic() - began,
                       compiled=run["compiled"], noop_compiled=check["compiled"], target_bytes=target_bytes,
                       central_bytes=central_bytes, store_bytes=store_bytes, issues=issues,
                       **{name: value for name, value in metrics_of(run).items() if name in ("cpu", "wall", "instructions")}))
        for issue in issues:
            print(f"WARNING: configuration {cfg.key}: {issue}", flush=True)
        print(f"baseline {cfg.key} cpu={run['cpu']:8.2f} compiled={run['compiled']} target={target_bytes} "
              f"central={central_bytes} store={store_bytes}", flush=True)

    def sample(self, cfg, scenario, round_number, position, runs, extra=None):
        per = [metrics_of(run) for run in runs]
        record = dict(type="sample", schema=SCHEMA, config=cfg.key, scenario=scenario, round=round_number,
                      position=position, app=self.workload, profile="release", status=0,
                      compiled=max(run["compiled"] for run in runs), runs=len(runs))
        for name in METRIC_NAMES:
            record[name] = median_or_none([item[name] for item in per])
        if len(runs) > 1:
            record["cpu_samples"] = [item["cpu"] for item in per]
            record["instructions_samples"] = [item["instructions"] for item in per]
        record.update(extra or {})
        self.emit(record)
        instructions = record["instructions"]
        shown = f"{instructions / 1e9:8.3f}G" if instructions else "     n/a"
        print(f"{cfg.key} {scenario:5} r{round_number} pos{position} instr={shown} cpu={record['cpu']:8.3f} "
              f"wall={record['wall']:7.2f} compiled={record['compiled']}", flush=True)

    def failed_sample(self, cfg, scenario, round_number, position, run):
        self.emit(dict(type="sample", schema=SCHEMA, config=cfg.key, scenario=scenario, round=round_number,
                       position=position, app=self.workload, profile="release", status=run["status"],
                       cpu=run["cpu"], wall=run["wall"]))

    def do_noop(self, cfg, round_number, position):
        place = cfg.place(False)
        self.restore(cfg, place)
        log = self.logs / f"{self.workload}-noop-r{round_number}-{cfg.key}.log"
        runs = []
        try:
            for _ in range(self.args.noops):
                run = self.build_once(cfg, place, log)
                if run["status"] != 0:
                    self.failed_sample(cfg, "noop", round_number, position, run)
                    self.drop(cfg, "noop", f"exit {run['status']}", log)
                    return
                runs.append(run)
        finally:
            wipe(cfg.target_root(place))
        self.sample(cfg, "noop", round_number, position, runs)

    def do_edit(self, cfg, round_number, position):
        place = cfg.place(False)
        self.restore(cfg, place)
        log = self.logs / f"{self.workload}-edit-r{round_number}-{cfg.key}.log"
        try:
            with self.edit.applied():
                run = self.build_once(cfg, place, log)
        finally:
            wipe(cfg.target_root(place))
        if run["status"] != 0:
            self.failed_sample(cfg, "edit", round_number, position, run)
            self.drop(cfg, "edit", f"exit {run['status']}", log)
            return
        self.sample(cfg, "edit", round_number, position, [run])

    def do_cold(self, cfg, round_number, position):
        place = cfg.place(True)
        cfg.reset(place)
        if cfg.fcargo and CI.STORE.allocated(cfg.store(place)) != 0:
            raise Fatal(f"could not empty the store of configuration {cfg.key}")
        log = self.logs / f"{self.workload}-cold-r{round_number}-{cfg.key}.log"
        try:
            run = self.build_once(cfg, place, log)
            target_bytes = CI.STORE.allocated(cfg.target_root(place))
            store_bytes = CI.STORE.allocated(cfg.store(place)) if cfg.fcargo else 0
        finally:
            wipe(place["home"], place["target"], place["xdg"])
        if run["status"] != 0:
            self.failed_sample(cfg, "cold", round_number, position, run)
            self.drop(cfg, "cold", f"exit {run['status']}", log)
            return
        self.sample(cfg, "cold", round_number, position, [run],
                    dict(target_bytes=target_bytes, store_bytes=store_bytes))

    def scenario_round(self, scenario, round_number):
        active = self.active()
        offset = round_number % len(active)
        for position, cfg in enumerate(active[offset:] + active[:offset]):
            if cfg.key in self.disabled:
                continue
            getattr(self, f"do_{scenario}")(cfg, round_number, position)

    def fits(self, scenario):
        """False, with a stop record, when the next round of this scenario would pass the budget."""
        durations = self.durations[scenario]
        if self.budget and durations and self.elapsed() + 1.15 * max(durations) > self.budget:
            self.stop(scenario, "time budget: the next round would not fit")
            return False
        return True

    def stop(self, scenario, reason):
        self.stopped.add(scenario)
        self.emit(dict(type="stop", schema=SCHEMA, scenario=scenario, reason=reason,
                       rounds_completed=len(self.durations[scenario]), rounds_requested=self.rounds[scenario]))
        print(f"stopping {scenario} after {len(self.durations[scenario])} of {self.rounds[scenario]} rounds: {reason}",
              flush=True)

    def run(self):
        try:
            try:
                for cfg in self.configs:
                    self.prepare(cfg)
                self.loop()
            except BudgetExceeded:
                for scenario in SCENARIOS:
                    if len(self.durations[scenario]) < self.rounds[scenario] and scenario not in self.stopped:
                        self.stop(scenario, "time budget: the budget ended during a round; a round that was cut "
                                  "short only counts in the pairs where both configurations finished")
        finally:
            for cfg in self.configs:
                for cold in (False, True):
                    place = cfg.place(cold)
                    wipe(place["home"], place["target"], place["xdg"])
                wipe(cfg.snapshot)

    def loop(self):
        for round_number in range(max(self.rounds.values())):
            for scenario in SCENARIO_ORDER:
                if round_number >= self.rounds[scenario] or scenario in self.stopped:
                    continue
                if not self.fits(scenario):
                    continue
                began = time.monotonic()
                self.scenario_round(scenario, round_number)
                self.durations[scenario].append(time.monotonic() - began)


# ---------------------------------------------------------------------------------------------------------
# Analysis and report. Nothing below needs ci-bench.py, so `report` runs anywhere.

Effect = collections.namedtuple("Effect", "value iqr n")


def quartile_spread(values):
    if len(values) < 2:
        return 0.0
    quartiles = statistics.quantiles(values, n=4, method="inclusive")
    return quartiles[2] - quartiles[0]


def clears(effect, floor):
    """True when the effect is a real difference: enough pairs, and larger than the floor and the pair spread."""
    return effect is not None and effect.n >= MIN_PAIRS and abs(effect.value - 1) > max(floor, effect.iqr)


def mean_effect(effects):
    """Geometric mean of the effects, None when any is missing."""
    if not effects or any(effect is None for effect in effects):
        return None
    return Effect(math.exp(statistics.mean(math.log(effect.value) for effect in effects)),
                  max(effect.iqr for effect in effects), min(effect.n for effect in effects))


def rel(value):
    return "n/a" if value is None else f"{(value - 1) * 100:+.1f}%"


def pct_of(value):
    return "n/a" if value is None else f"{value * 100:.1f}%"


def fmt_count(value):
    if value is None:
        return "n/a"
    for scale, unit in ((1e9, "G"), (1e6, "M"), (1e3, "K")):
        if value >= scale:
            return f"{value / scale:.3f} {unit}"
    return f"{value:.0f}"


def fmt_float(value, digits=3):
    return "n/a" if value is None else f"{value:.{digits}f}"


def fmt_mib(value):
    return "n/a" if value is None else f"{value / 1048576:.0f}"


def read_run(path):
    run = dict(path=str(path), meta=None, selftest=None, baselines=[], samples=[], stops=[])
    for line in pathlib.Path(path).read_text().splitlines():
        if not line.strip():
            continue
        record = json.loads(line)
        kind = record.get("type")
        if kind == "meta":
            run["meta"] = record
        elif kind == "selftest":
            run["selftest"] = record
        elif kind == "baseline":
            run["baselines"].append(record)
        elif kind == "sample":
            run["samples"].append(record)
        elif kind == "stop":
            run["stops"].append(record)
    return run


class Analysis:
    def __init__(self, run):
        self.run = run
        self.cells = {}
        for sample in run["samples"]:
            if sample.get("status") == 0:
                self.cells.setdefault((sample["scenario"], sample["config"]), {})[sample["round"]] = sample
        self.mode = (run.get("selftest") or {}).get("mode", "none")

    def scenarios(self):
        return [scenario for scenario in SCENARIOS if any(key[0] == scenario for key in self.cells)]

    def configs(self, scenario):
        return sorted(key[1] for key in self.cells if key[0] == scenario)

    def usable(self, scenario, metric):
        return any(sample.get(metric) for (name, _), rounds in self.cells.items() if name == scenario
                   for sample in rounds.values())

    def primary(self, scenario):
        if self.mode == "tree" and self.usable(scenario, "instructions"):
            return "instructions"
        return "cpu"

    def values(self, scenario, config, metric):
        rounds = self.cells.get((scenario, config), {})
        return [rounds[number][metric] for number in sorted(rounds) if rounds[number].get(metric) is not None]

    def ratios(self, scenario, metric, numerator, denominator):
        top = self.cells.get((scenario, numerator), {})
        bottom = self.cells.get((scenario, denominator), {})
        out = []
        for number in sorted(set(top) & set(bottom)):
            x, y = top[number].get(metric), bottom[number].get(metric)
            if x and y:
                out.append(x / y)
        return out

    def effect(self, scenario, metric, numerator, denominator):
        ratios = self.ratios(scenario, metric, numerator, denominator)
        if not ratios:
            return None
        return Effect(statistics.median(ratios), quartile_spread(ratios), len(ratios))

    def table(self, scenario):
        metric = self.primary(scenario)
        instr_title = "instructions (median)" + (" (primary)" if metric == "instructions" else "")
        cpu_title = "CPU s (median)" + (" (primary)" if metric == "cpu" else "")
        lines = [f"| config | {instr_title} | ratio to A | {cpu_title} | ratio to A | pairs | spread (pp) | flag |",
                 "| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |"]
        for key in self.configs(scenario):
            instr = self.values(scenario, key, "instructions")
            cpu = self.values(scenario, key, "cpu")
            if key == "A":
                lines.append(f"| A | {fmt_count(median_or_none(instr))} | 100.0% | {fmt_float(median_or_none(cpu))} "
                             f"| 100.0% | - | - | reference |")
                continue
            instr_effect = self.effect(scenario, "instructions", key, "A")
            cpu_effect = self.effect(scenario, "cpu", key, "A")
            main = instr_effect if metric == "instructions" else cpu_effect
            pairs = main.n if main else 0
            spread = f"{main.iqr * 100:.1f}" if main else "n/a"
            flags = []
            if pairs < MIN_PAIRS:
                flags.append("few pairs")
            if main and main.iqr * 100 > NOISE_PP[metric]:
                flags.append("noise")
            lines.append(f"| {key} | {fmt_count(median_or_none(instr))} | {pct_of(instr_effect and instr_effect.value)} "
                         f"| {fmt_float(median_or_none(cpu))} | {pct_of(cpu_effect and cpu_effect.value)} "
                         f"| {pairs} | {spread} | {', '.join(flags) or 'ok'} |")
        return lines

    def other_table(self, scenario):
        lines = ["| config | cycles (median) | ratio to A | wall s (median) | ratio to A | peak footprint MiB | "
                 "max RSS MiB | blocks in / out | compiled (max) |",
                 "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"]
        for key in self.configs(scenario):
            cycles_effect = self.effect(scenario, "cycles", key, "A") if key != "A" else Effect(1.0, 0.0, 0)
            wall_effect = self.effect(scenario, "wall", key, "A") if key != "A" else Effect(1.0, 0.0, 0)
            blocks_in = median_or_none(self.values(scenario, key, "blocks_in"))
            blocks_out = median_or_none(self.values(scenario, key, "blocks_out"))
            compiled = max(self.values(scenario, key, "compiled") or [0])
            lines.append(f"| {key} | {fmt_count(median_or_none(self.values(scenario, key, 'cycles')))} "
                         f"| {pct_of(cycles_effect and cycles_effect.value)} "
                         f"| {fmt_float(median_or_none(self.values(scenario, key, 'wall')), 2)} "
                         f"| {pct_of(wall_effect and wall_effect.value)} "
                         f"| {fmt_mib(median_or_none(self.values(scenario, key, 'peak_footprint')))} "
                         f"| {fmt_mib(median_or_none(self.values(scenario, key, 'max_rss')))} "
                         f"| {fmt_float(blocks_in, 0)} / {fmt_float(blocks_out, 0)} | {compiled} |")
        return lines

    def contrast_table(self, scenario):
        metric = self.primary(scenario)
        floor = FLOOR[metric]
        lines = [f"| contrast | isolates | ratio ({metric}) | change | spread (pp) | pairs | reading |",
                 "| --- | --- | ---: | ---: | ---: | ---: | --- |"]
        for name, top, bottom, meaning in CONTRASTS:
            effect = self.effect(scenario, metric, top, bottom)
            if effect is None:
                continue
            if not clears(effect, floor):
                reading = "within noise" if effect.n >= MIN_PAIRS else "too few pairs"
            else:
                reading = f"{top} costs more" if effect.value > 1 else f"{top} costs less"
            lines.append(f"| {name} | {meaning} | {effect.value:.3f} | {rel(effect.value)} | {effect.iqr * 100:.1f} "
                         f"| {effect.n} | {reading} |")
        return lines

    def bisection(self, scenario):
        """The bisection summary in words, from the ratios of B, C, D, E, F and G against A and each other."""
        metric = self.primary(scenario)
        floor = FLOOR[metric]

        def effect(top, bottom):
            return self.effect(scenario, metric, top, bottom)

        total = effect("B", "A")
        if total is None:
            return ["B or A has no complete pair, so there is nothing to bisect."]
        lines = [f"fRust default (B) is {pct_of(total.value)} of official (A) on {metric}, spread "
                 f"{total.iqr * 100:.1f} points over {total.n} pairs."]
        if metric == "instructions":
            cpu_total = self.effect(scenario, "cpu", "B", "A")
            lines.append(f"On CPU time B is {pct_of(cpu_total and cpu_total.value)} of A"
                         + (f", spread {cpu_total.iqr * 100:.1f} points over {cpu_total.n} pairs." if cpu_total else "."))

        compiler = mean_effect([effect("F", "A"), effect("B", "G")])
        cargo = mean_effect([effect("G", "A"), effect("B", "F")])
        cache = mean_effect([effect("B", "C"), effect("D", "E")])
        target = mean_effect([effect("B", "D"), effect("C", "E")])
        residual = effect("E", "F")

        def detail(parts):
            return ", ".join(f"{label} {rel(item.value)}" for label, item in parts if item is not None)

        if compiler is not None or cargo is not None:
            text = "Compiler against cargo (A, F, G, B):"
            if compiler is not None:
                text += (f" switching only the compiler to frustc changes {metric} by {rel(compiler.value)} "
                         f"({detail([('F vs A', effect('F', 'A')), ('B vs G', effect('B', 'G'))])});")
            if cargo is not None:
                text += (f" switching only cargo to fcargo defaults changes it by {rel(cargo.value)} "
                         f"({detail([('G vs A', effect('G', 'A')), ('B vs F', effect('B', 'F'))])}).")
            lines.append(text)
        else:
            lines.append("Compiler against cargo: F and G were not measured in pairs, so this split is not available.")
        if cache is not None or target is not None:
            text = "Inside fcargo (B, C, D, E):"
            if cache is not None:
                text += (f" the unit cache changes {metric} by {rel(cache.value)} "
                         f"({detail([('B vs C', effect('B', 'C')), ('D vs E', effect('D', 'E'))])});")
            if target is not None:
                text += (f" the central target changes it by {rel(target.value)} "
                         f"({detail([('B vs D', effect('B', 'D')), ('C vs E', effect('C', 'E'))])});")
            if residual is not None:
                text += f" with both off and frustc on both sides, fcargo against official cargo is {rel(residual.value)} (E vs F)."
            lines.append(text)
        else:
            lines.append("Inside fcargo: C, D and E were not measured in pairs, so the unit cache and the central "
                         "target cannot be separated.")

        named = [("the compiler", compiler), ("cargo", cargo), ("the unit cache", cache),
                 ("the central target", target), ("the rest of the patched cargo (E against F)", residual)]
        real = [f"{name} {rel(item.value)}" for name, item in named if clears(item, floor)]
        lines.append("Switches that clear the noise on their own: " + (", ".join(real) if real else "none") + ".")

        if clears(total, floor) and total.value > 1:
            who = self.attribute([("the compiler", compiler), ("cargo", cargo)], floor)
            verdict = f"Verdict: fRust loses {rel(total.value)} on {metric}; the loss is {who}."
            if who.startswith("not separable"):
                verdict = (f"Verdict: fRust loses {rel(total.value)} on {metric}, and neither the compiler switch nor the "
                           "cargo switch alone clears the noise: the loss is not separable at this noise level, or it is "
                           "an interaction between them.")
            elif "cargo" in who:
                inner = self.attribute([("the unit cache", cache), ("the central target", target),
                                        ("the rest of the patched cargo", residual)], floor)
                if inner.startswith("not separable"):
                    verdict += " Inside cargo no single switch (unit cache, central target, rest) clears the noise."
                else:
                    verdict += f" Inside cargo the cost is {inner}."
            lines.append(verdict)
        elif clears(total, floor):
            lines.append(f"Verdict: no loss here; fRust is ahead of official by {abs(total.value - 1) * 100:.1f}% on {metric}.")
        else:
            lines.append(f"Verdict: B and A are within the noise on {metric} in this scenario, so there is no loss to bisect.")
        return lines

    @staticmethod
    def attribute(items, floor):
        """Name the switches that cost more beyond the noise. 'mainly X' when one has at least twice the log cost
        of the next, 'X and Y together' otherwise, 'not separable' when none clears."""
        ranked = sorted(((math.log(item.value), name) for name, item in items
                         if clears(item, floor) and item.value > 1), reverse=True)
        if not ranked:
            return "not separable: none of them clears the noise"
        if len(ranked) == 1 or ranked[0][0] >= 2 * ranked[1][0]:
            return f"mainly {ranked[0][1]}"
        names = [name for size, name in ranked if size >= 0.5 * ranked[0][0]]
        return " and ".join(names) + " together"


def render(run):
    meta = run["meta"] or {}
    analysis = Analysis(run)
    host = meta.get("host") or {}
    selftest = run.get("selftest") or {}
    out = [f"# fRust macOS probe: {meta.get('label', '?')} {meta.get('workload', '?')}", ""]
    out.append(f"- host: {host.get('cpu_model')} / {host.get('os') or host.get('system')} / "
               f"{host.get('logical_cpus')} logical CPUs, cargo -j {meta.get('jobs')}")
    out.append(f"- toolchains: official {((meta.get('toolchains') or {}).get('official') or {}).get('cargo')}; "
               f"fRust {((meta.get('toolchains') or {}).get('frust') or {}).get('cargo')}")
    out.append(f"- commit {meta.get('commit')}, cohort commit {(meta.get('cohort') or {}).get('commit')}, "
               f"run {(meta.get('run') or {}).get('id')}, {meta.get('utc')}")
    out.append(f"- rounds requested: no-op {meta.get('noop_rounds')}, edit {meta.get('edit_rounds')} "
               f"({meta.get('edit_kind')}), cold {meta.get('cold_rounds')}; {meta.get('noops')} no-op builds per round; "
               f"budget {meta.get('time_budget_minutes')} minutes")
    out.append(f"- instruction counter: {selftest.get('note', 'no self-test record')}")
    out.append("")
    out.append("Ratios are medians of per-pair ratios (the configuration over A in the same round), shown as a percent "
               "of A. Spread is the interquartile range of the per-pair ratio of the primary metric in percentage points; "
               f"a row is flagged noise above {NOISE_PP['instructions']:.0f} points for instructions and "
               f"{NOISE_PP['cpu']:.0f} points for CPU time, and an effect is called real only above "
               f"{FLOOR['instructions'] * 100:.0f}% (instructions) or {FLOOR['cpu'] * 100:.0f}% (CPU) and above its own "
               f"spread, with at least {MIN_PAIRS} pairs. Pairs are rounds in which both configurations completed.")
    out.append("")
    out.append("## Configurations")
    out.append("")
    out.append("| config | what | cargo | rustc | target directory | unit cache |")
    out.append("| --- | --- | --- | --- | --- | --- |")
    for key in sorted(meta.get("configs") or {}):
        spec = meta["configs"][key]
        out.append(f"| {key} | {spec['desc']} | {spec['cargo']} | {spec['rustc']} | {spec['target']} | {spec['unit_cache']} |")
    out.append("")

    titles = {"noop": "No-op rebuild", "edit": f"Edit rebuild ({meta.get('edit_kind') or 'leaf'})", "cold": "Cold build"}
    for scenario in analysis.scenarios():
        out.append(f"## {titles[scenario]}")
        out.append("")
        out.extend(analysis.table(scenario))
        out.append("")
        out.extend(analysis.other_table(scenario))
        out.append("")
        out.append("Single switch contrasts:")
        out.append("")
        out.extend(analysis.contrast_table(scenario))
        out.append("")
        out.append("Bisection:")
        out.append("")
        out.extend(f"- {line}" for line in analysis.bisection(scenario))
        out.append("")

    notes = []
    for record in run["baselines"]:
        for issue in record.get("issues") or []:
            notes.append(f"Configuration {record['config']}: {issue}.")
    failed = collections.OrderedDict()
    for sample in run["samples"]:
        if sample.get("status") != 0:
            failed.setdefault(sample["config"], (sample["scenario"], sample["status"]))
    for key, (scenario, status) in failed.items():
        notes.append(f"Configuration {key} failed a {scenario} build with exit {status} and was dropped from later rounds.")
    for stop in run["stops"]:
        notes.append(f"{stop['scenario']} stopped after {stop['rounds_completed']} of {stop['rounds_requested']} rounds: "
                     f"{stop['reason']}.")
    out.append("## Run notes")
    out.append("")
    out.extend([f"- {note}" for note in notes] or ["- none"])
    out.append("")
    return "\n".join(out)


def render_all(paths):
    return "\n".join(render(read_run(path)) for path in paths)


# ---------------------------------------------------------------------------------------------------------


def command_run(args):
    global CI
    CI = load_ci_bench()
    app = pathlib.Path(args.app).resolve()
    if not app.is_dir():
        sys.exit(f"no cohort application at {app}")
    if not CI.tree_clean(app):
        sys.exit(f"{app} has uncommitted changes; the cohort must be the frozen tree")
    keys = []
    for key in args.configs.split():
        if key not in CONFIGS:
            sys.exit(f"unknown configuration {key!r}, expected letters of {''.join(CONFIGS)}")
        if key not in keys:
            keys.append(key)
    if "A" not in keys:
        sys.exit("configuration A (official) is the reference of every ratio and must be selected")
    if max(args.noop_rounds, args.edit_rounds, args.cold_rounds) < 1:
        sys.exit("at least one scenario needs a round")
    if pathlib.Path(args.frust_cargo).name != "fcargo":
        sys.exit("--frust-cargo must be named fcargo: the central target and the unit cache are on only under that name")

    work = pathlib.Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    pathlib.Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    jobs = args.jobs or os.cpu_count() or 1
    cargos = {"official": args.official_cargo, "frust": args.frust_cargo}
    rustcs = {"official": args.official_rustc, "frust": args.frust_rustc}
    configs = [Config(key, CONFIGS[key], work, cargos[CONFIGS[key]["cargo"]], rustcs[CONFIGS[key]["rustc"]], jobs)
               for key in keys]
    versions = {"official": CI.tool_versions(args.official_cargo, args.official_rustc),
                "frust": CI.tool_versions(args.frust_cargo, args.frust_rustc)}
    release_line = re.search(r"^release: (\S+)$", versions["official"]["rustc"] or "", re.M)
    if not release_line or release_line.group(1) != args.expect_official_release:
        sys.exit(f"official rustc reports release {release_line and release_line.group(1)}, "
                 f"expected {args.expect_official_release}")

    use_time_l = args.time_l == "auto" and sys.platform == "darwin" and os.path.exists(TIME_BIN)
    if use_time_l:
        selftest = time_l_selftest()
    else:
        selftest = dict(mode="none", note="/usr/bin/time -l is used on macOS only; CPU time is the primary metric",
                        cases={})
    workload = app.name
    edit = CI.Edit(app, workload, args.edit_kind) if args.edit_rounds > 0 else None
    lock = app / "Cargo.lock"
    meta = dict(
        type="meta", schema=SCHEMA, label=args.label, workload=workload, host=CI.host_info(), jobs=jobs,
        profile=CI.PROFILE, noop_rounds=args.noop_rounds, edit_rounds=args.edit_rounds, cold_rounds=args.cold_rounds,
        noops=args.noops, edit_kind=args.edit_kind, time_budget_minutes=args.time_budget_minutes,
        configs={key: {name: CONFIGS[key][name] for name in ("desc", "cargo", "rustc", "target", "unit_cache", "env")}
                 for key in keys},
        toolchains=versions, python=sys.version.split()[0],
        toolchain_prefixes=dict(official=str(pathlib.Path(args.official_cargo).parent.parent),
                                frust=str(pathlib.Path(args.frust_cargo).parent.parent)),
        commit=os.environ.get("GITHUB_SHA") or CI.git_head(CI.ROOT),
        run=dict(id=os.environ.get("GITHUB_RUN_ID"), attempt=os.environ.get("GITHUB_RUN_ATTEMPT"),
                 repository=os.environ.get("GITHUB_REPOSITORY"), ref=os.environ.get("GITHUB_REF")),
        cohort=dict(commit=CI.git_head(app), cargo_lock_sha256=hashlib.sha256(lock.read_bytes()).hexdigest()),
        utc=datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    )
    probe = Probe(args, configs, edit, selftest, use_time_l)
    probe.emit(meta)
    probe.emit(dict(type="selftest", schema=SCHEMA, mode=selftest["mode"], note=selftest["note"],
                    cases=selftest["cases"]))
    print(f"workload {workload}, configurations {' '.join(keys)}, cargo -j {jobs}, budget "
          f"{args.time_budget_minutes} minutes, instruction counter mode {selftest['mode']}", flush=True)

    status = 0
    try:
        probe.run()
    except Fatal as error:
        print(f"ERROR: {error}", file=sys.stderr, flush=True)
        status = 1
    finally:
        if args.report:
            pathlib.Path(args.report).parent.mkdir(parents=True, exist_ok=True)
            pathlib.Path(args.report).write_text(render_all([args.out]))
    if not CI.tree_clean(app):
        print(f"ERROR: {app} was left modified", file=sys.stderr, flush=True)
        status = 1
    return status


def command_report(args):
    text = render_all(args.samples)
    if args.out:
        pathlib.Path(args.out).write_text(text)
    else:
        sys.stdout.write(text)
    return 0


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)

    run = commands.add_parser("run", help="measure one cohort application")
    run.add_argument("--official-cargo", required=True)
    run.add_argument("--official-rustc", required=True)
    run.add_argument("--expect-official-release", default="1.100.0",
                     help="the release line rustc -Vv must report for the official toolchain")
    run.add_argument("--frust-cargo", required=True, help="fcargo of the fRust toolchain")
    run.add_argument("--frust-rustc", required=True, help="frustc of the fRust toolchain")
    run.add_argument("--app", required=True, help="cohort application directory, named after its workload")
    run.add_argument("--label", required=True, help="platform label recorded in the results")
    run.add_argument("--configs", default="A B C D E F G", help="configurations to measure, A is required")
    run.add_argument("--noop-rounds", type=int, default=15, help="rounds of the no-op scenario, 0 skips it")
    run.add_argument("--edit-rounds", type=int, default=15, help="rounds of the edit scenario, 0 skips it")
    run.add_argument("--cold-rounds", type=int, default=5, help="rounds of the cold scenario, 0 skips it")
    run.add_argument("--noops", type=int, default=5, help="no-op builds per round, the median is the sample")
    run.add_argument("--edit-kind", default="leaf", help="frozen edit of qualification/edits")
    run.add_argument("--jobs", type=int, default=0, help="cargo -j, 0 for one per logical CPU")
    run.add_argument("--time-budget-minutes", type=float, default=120,
                     help="stop cleanly after this many minutes, 0 for no limit")
    run.add_argument("--time-l", choices=("auto", "off"), default="auto",
                     help="auto: run builds under /usr/bin/time -l on macOS; off: CPU and wall time only")
    run.add_argument("--work", required=True, help="scratch directory for targets, stores, snapshots and logs")
    run.add_argument("--out", required=True, help="JSON lines file, appended to")
    run.add_argument("--report", help="markdown report, written at the end and also when the run fails")
    run.set_defaults(handler=command_run)

    report = commands.add_parser("report", help="regenerate the markdown report from JSON lines")
    report.add_argument("samples", nargs="+", help="JSON lines files written by run")
    report.add_argument("--out", help="markdown file, default standard output")
    report.set_defaults(handler=command_report)
    return parser.parse_args()


def main():
    args = parse_args()
    if getattr(args, "noops", 1) < 1:
        sys.exit("--noops must be at least 1")
    sys.exit(args.handler(args))


if __name__ == "__main__":
    main()
