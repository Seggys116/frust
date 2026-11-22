#!/usr/bin/env python3
"""Turn the raw JSON lines of qualification/ci-bench.py into a decision report, tables and SVG charts.

Usage: make-charts.py INPUT... --out DIR [--run TEXT] [--min-pairs N] [--bootstrap N]

INPUT is a results file or a directory searched for *.jsonl (and, for the resource comparison with
the previous fRust release, *.ndjson files of qualification/concurrent-builds.py). Each .jsonl file
is one job: a meta record followed by sample records. For every platform, workload and scenario the
report pairs the official and fRust build of the same round and states, with the number of pairs,
the fRust CPU as a percent of official CPU, a 95% interval and a verdict: win (the interval and an
exact sign test both say fRust used less CPU), loss (both say more), parity (the data do not separate
the toolchains) or insufficient (too few pairs to judge). The statistics are in paired_stats.py and
are described in docs/benchmarks.md. A job may be split over several result files (shards): their
pairs are pooled per workload.

The decision section lists, per platform, the four cells of the release bar (cold build, clean
rebuild, no-op rebuild, leaf edit; all workloads together) and says whether the bar is met. The same
decision is written as verdict.json. The bar is met only when every cell of every required platform is
a win and no workload inside those cells is a loss. A cell also states how many more pairs the
observed spread would need before its interval stops containing parity.

Platforms are grouped by label. The two Linux variants have their own labels (linux-x86_64 and
linux-x86_64-v3), and so do the optional rustc-PGO candidates of macOS (macos-arm64-pgo and
macos-x86_64-pgo). Each is compared with the official toolchain of its own runner, and when a
variant and its baseline are both present the report adds a table with the two side by side. The
page-cache probe records of each job ("folio", see ci-bench.py) become one row per job, and a job a
runner skipped (a "skipped" record, written when the runner CPU cannot run the v3 toolchain) is
listed in the run notes.

The output is deterministic: the same input gives byte-identical files. Standard library only.
Written to DIR: results.md, summary.json, verdict.json and one chart-<platform>.svg per platform.
"""
import argparse
import json
import math
import os
import pathlib
import re
import runpy
import statistics
import sys
from xml.sax.saxutils import escape

PAIRED = runpy.run_path(pathlib.Path(__file__).with_name("paired_stats.py"))
SCHEMA = "frust-compare-1"
VERDICT_SCHEMA = "frust-verdict-1"
SCENARIOS = ("cold", "clean", "noop", "edit-leaf", "edit-wide", "edit-holdout")
SCENARIO_NAMES = {
    "cold": "cold build",
    "clean": "clean rebuild",
    "noop": "no-op rebuild",
    "edit-leaf": "edit rebuild, leaf",
    "edit-wide": "edit rebuild, wide",
    "edit-holdout": "edit rebuild, holdout",
}
SCENARIO_NOTES = {
    "cold": "empty target; fRust starts with an empty unit cache store",
    "clean": "target removed; the fRust store holds that round's cold build",
    "noop": "immediate rebuilds, alternating between the toolchains; the sample of a round is the median of the adjacent pair ratios",
    "edit-leaf": "frozen leaf edit applied to a copy of the clean target",
    "edit-wide": "frozen wide edit applied to a copy of the clean target",
    "edit-holdout": "frozen holdout edit applied to a copy of the clean target",
}
WORKLOADS = ("ripgrep", "syn", "regex", "fd")
AGGREGATE = "all workloads"
# The release bar: every one of these cells, on every one of these platforms, must be a win. A cell is
# all of the listed workloads together; the fd edit is a different kind of edit (holdout) and is reported
# beside the bar and not in it.
REQUIRED_PLATFORMS = ("linux-x86_64", "linux-x86_64-v3", "macos-arm64", "macos-x86_64")
REQUIRED_SCENARIOS = ("cold", "clean", "noop", "edit-leaf")
EXPECTED_WORKLOADS = {
    "cold": WORKLOADS, "clean": WORKLOADS, "noop": WORKLOADS, "edit-leaf": ("ripgrep", "syn", "regex"),
    "edit-wide": ("ripgrep", "syn", "regex"), "edit-holdout": ("fd",),
}
VERDICT_ORDER = ("win", "parity", "loss", "insufficient", "recompiled")


def read_jsonl(path):
    records = []
    with open(path, encoding="utf-8") as handle:
        for number, line in enumerate(handle, 1):
            line = line.strip()
            if not line:
                continue
            try:
                records.append(json.loads(line))
            except ValueError as error:
                sys.exit(f"{path}:{number}: not JSON ({error})")
    return records


def find_files(inputs, suffix=".jsonl"):
    files = []
    for item in inputs:
        if os.path.isdir(item):
            for root, dirs, names in os.walk(item):
                dirs.sort()
                files.extend(os.path.join(root, name) for name in sorted(names) if name.endswith(suffix))
        elif os.path.isfile(item):
            if item.endswith(suffix) or suffix == ".jsonl":
                files.append(item)
        else:
            sys.exit(f"no such input: {item}")
    return files


def load_runs(files):
    """Return (runs, skipped, omitted).

    runs has one entry per file with a meta record of this schema. omitted holds the records of
    jobs that declined to run (type "skipped"). Any other file is listed in skipped."""
    runs, skipped, omitted = [], [], []
    for path in files:
        records = [r for r in read_jsonl(path) if r.get("schema") == SCHEMA]
        metas = [r for r in records if r.get("type") == "meta"]
        if not metas:
            declined = [r for r in records if r.get("type") == "skipped"]
            if declined:
                omitted.extend(declined)
            else:
                skipped.append(path)
            continue
        runs.append(dict(
            path=path, meta=metas[0],
            samples=[r for r in records if r.get("type") == "sample"],
            stops=[r for r in records if r.get("type") == "stop"],
            folio=[r for r in records if r.get("type") == "folio"],
        ))
    return runs, skipped, omitted


def first_line(text):
    return (text or "").splitlines()[0] if text else "unknown"


def sample_ratio(official, frust):
    """(ratio, official CPU, fRust CPU) of one round, or None when the round cannot be paired.

    A no-op round of an alternating run carries the CPU of every build in cpu_samples, in the order
    they ran, so entry i of the two toolchains is an adjacent pair. The round's ratio is the median of
    those pair ratios. Records without that (older runs) pair the medians the records already hold."""
    if official["cpu"] <= 0 or frust["cpu"] < 0:
        return None
    mine, theirs = official.get("cpu_samples"), frust.get("cpu_samples")
    if official.get("interleaved") and frust.get("interleaved") and mine and theirs and len(mine) == len(theirs):
        ratios = [b / a for a, b in zip(mine, theirs) if a > 0 and b > 0]
        if ratios:
            return statistics.median(ratios), statistics.median(mine), statistics.median(theirs)
    return frust["cpu"] / official["cpu"], official["cpu"], frust["cpu"]


def collect(runs):
    """Return (pairs, failures, notes, requested). pairs[(label, workload, scenario)] is a list of paired
    samples in time order (result file, then round); files of one platform and workload are pooled."""
    pairs, failures, notes, requested = {}, {}, [], {}
    for index, run in enumerate(sorted(runs, key=lambda item: item["path"])):
        meta = run["meta"]
        label, workload = meta["label"], meta["workload"]
        group = (label, workload)
        requested[group] = requested.get(group, 0) + (meta.get("rounds_requested") or 0)
        by_key = {}
        for sample in run["samples"]:
            if sample.get("status") != 0:
                failures[group] = failures.get(group, 0) + 1
                continue
            by_key[(sample["scenario"], sample["round"], sample["toolchain"])] = sample
        for scenario, round_number, toolchain in sorted(by_key):
            if toolchain != "official":
                continue
            official = by_key[(scenario, round_number, "official")]
            frust = by_key.get((scenario, round_number, "frust"))
            if frust is None:
                continue
            measured = sample_ratio(official, frust)
            if measured is None or measured[0] <= 0:
                continue
            ratio, official_cpu, frust_cpu = measured
            first = None
            if official.get("position") is not None and frust.get("position") is not None:
                first = official["position"] < frust["position"]
            pairs.setdefault((label, workload, scenario), []).append(dict(
                run=index, round=round_number, official=official_cpu, frust=frust_cpu, ratio=ratio,
                log=math.log(ratio), official_first=first, events=frust.get("cache_events") or {},
                compiled_official=official.get("compiled") or 0, compiled_frust=frust.get("compiled") or 0,
            ))
        for stop in run["stops"]:
            notes.append(f"{label} {workload}: stopped after {stop['rounds_completed']} of "
                         f"{stop['rounds_requested']} rounds ({stop['reason']}).")
    return pairs, failures, notes, requested


def median_events(items, key):
    values = [item["events"].get(key, 0) for item in items]
    return statistics.median(values) if values else 0


def geometric_mean(ratios):
    return math.exp(statistics.fmean(map(math.log, ratios))) if all(ratios) else 0.0


def analyse(label, scenario, groups, min_pairs, reps):
    """Run the paired analysis on one cell. groups maps a workload to its pair list."""
    strata = {name: [item["log"] for item in items] for name, items in groups.items()}
    order = {name: [item["official_first"] for item in items] for name, items in groups.items()}
    if any(flag is None for flags in order.values() for flag in flags):
        order = None
    key = f"{label}/{scenario}/{','.join(sorted(groups))}"
    return PAIRED["analyze_cell"](strata, key, min_pairs, reps=reps, order=order)


def summarize(pairs, requested, min_pairs, reps=PAIRED["BOOTSTRAP_REPS"]):
    """One row per platform, workload and scenario, and one row per platform and scenario that pools the
    workloads (all workloads with that scenario, weighted equally)."""
    rows = []
    for (label, workload, scenario), items in sorted(pairs.items()):
        result = analyse(label, scenario, {workload: items}, min_pairs, reps)
        verdict, flags = result["verdict"], flag_list(result, min_pairs)
        if scenario == "noop":
            rebuilt = max(max(item["compiled_official"], item["compiled_frust"]) for item in items)
            if rebuilt:
                flags.append(f"no-op compiled up to {rebuilt} units")
                verdict = "recompiled"
        cache = None
        if scenario in ("cold", "clean") and any(item["events"] for item in items):
            cache = f"{median_events(items, 'hit'):g} / {median_events(items, 'miss'):g}"
        rows.append(dict(
            label=label, workload=workload, scenario=scenario, aggregate=False, pairs=len(items),
            official_cpu=statistics.median(item["official"] for item in items),
            frust_cpu=statistics.median(item["frust"] for item in items),
            cache_hit_miss=cache, faster_text=f"{result['faster']} of {len(items)}",
            verdict=verdict, flags=flags, requested=requested.get((label, workload), 0), **detail(result)))
    grouped = {}
    for row in rows:
        grouped.setdefault((row["label"], row["scenario"]), []).append(row)
    for (label, scenario), members in sorted(grouped.items()):
        if len(members) < 2:
            continue
        names = [row["workload"] for row in members]
        result = analyse(label, scenario, {name: pairs[label, name, scenario] for name in names}, min_pairs, reps)
        verdict, flags = result["verdict"], flag_list(result, min_pairs)
        if any(row["verdict"] == "recompiled" for row in members):
            verdict = "recompiled"
            flags.append("a no-op compiled units")
        rows.append(dict(
            label=label, workload=AGGREGATE, scenario=scenario, aggregate=True, pairs=result["pairs"],
            official_cpu=None, frust_cpu=None, cache_hit_miss=None,
            faster_text=f"{result['faster']} of {result['pairs']}", verdict=verdict, flags=flags,
            members=names, member_verdicts={row["workload"]: row["verdict"] for row in members},
            member_pairs={row["workload"]: row["pairs"] for row in members}, requested=0, **detail(result)))
    return rows


def detail(result):
    """The numeric fields of an analysis that every row carries."""
    return dict(
        ratio=result.get("ratio"), mean_ratio=result.get("mean_ratio"), median_ratio=result.get("median_ratio"),
        ratio_min=result.get("ratio_min"), ratio_max=result.get("ratio_max"),
        interval=result.get("interval"), yuen=result.get("yuen"), bootstrap=result.get("bootstrap"),
        sign_p=result.get("sign_p"), faster=result.get("faster", 0), slower=result.get("slower", 0),
        lag1=result.get("lag1"), serial_inflation=result.get("serial_inflation", 1.0),
        order_effect=result.get("order_effect"), reason=result.get("reason"),
        pairs_needed_to_exclude_parity=result.get("pairs_needed_to_exclude_parity"),
        pairs_needed_for_1pct=result.get("pairs_needed_for_1pct"),
        pairs_per_stratum=result.get("pairs_per_stratum"))


def flag_list(result, min_pairs):
    flags = []
    if result["verdict"] == "insufficient":
        flags.append(result.get("reason") or f"fewer than {min_pairs} pairs")
    if result.get("serial_inflation", 1.0) > 1.0:
        flags.append(f"serial correlation {result['lag1']:.2f} widened the interval")
    effect = result.get("order_effect")
    if effect is not None and abs(math.log(effect)) > 0.02:
        flags.append(f"run order shifts the ratio by {abs(effect - 1) * 100:.1f}%")
    return flags


def format_interval(interval):
    return "n/a" if interval is None else " to ".join(percent(bound) for bound in interval)


def order_key(row):
    workload = WORKLOADS.index(row["workload"]) if row["workload"] in WORKLOADS else len(WORKLOADS)
    if row["aggregate"]:
        workload = len(WORKLOADS) + 1
    scenario = SCENARIOS.index(row["scenario"]) if row["scenario"] in SCENARIOS else len(SCENARIOS)
    return row["label"], scenario, workload, row["workload"]


def seconds(value):
    if value is None:
        return "-"
    return f"{value * 1000:.1f} ms" if value < 1.0 else f"{value:.2f} s"


def percent(ratio):
    return f"{ratio * 100:.1f}%"


def host_summary(runs):
    hosts = {}
    for run in runs:
        meta, host = run["meta"], run["meta"].get("host") or {}
        tools = meta.get("toolchains") or {}
        cores = "/".join(str(host.get(key) or "?") for key in ("physical_cpus", "logical_cpus"))
        if host.get("performance_cpus"):
            cores += f" ({host['performance_cpus']} performance, {host.get('efficiency_cpus') or '0'} efficiency)"
        key = (
            meta["label"], host.get("cpu_model") or "unknown", cores, host.get("os") or host.get("system") or "unknown",
            (meta.get("pinning") or {}).get("note") or "unknown",
            first_line((tools.get("official") or {}).get("rustc")), first_line((tools.get("official") or {}).get("cargo")),
            first_line((tools.get("frust") or {}).get("rustc")), first_line((tools.get("frust") or {}).get("cargo")),
            meta.get("commit") or "unknown", meta.get("unit_cache") or "unknown",
            meta.get("cache_state") or "not recorded",
            json.dumps(meta.get("frust_env") or {}, sort_keys=True),
            str(meta.get("jobs") or "unknown"),
        )
        hosts[key] = hosts.get(key, 0) + 1
    return sorted(hosts.items())


def cell(text):
    return str(text).replace("|", "/")


VARIANT_SUFFIXES = ("-v3", "-pgo")


def paired_variants(rows):
    """(baseline label, variant label) for every platform measured with its baseline build and a variant
    build: the Linux v3 build (-v3) or the rustc-PGO candidate (-pgo)."""
    labels = {row["label"] for row in rows}
    return [(label[:-len(suffix)], label) for label in sorted(labels) for suffix in VARIANT_SUFFIXES
            if label.endswith(suffix) and label[:-len(suffix)] in labels]


def variant_table(rows, baseline, variant):
    """Markdown lines putting the two builds of one platform side by side."""
    by_key = {(row["label"], row["scenario"], row["workload"]): row for row in rows}
    keys = sorted({key[1:] for key in by_key if key[0] in (baseline, variant)},
                  key=lambda key: order_key(dict(by_key.get((baseline, *key)) or by_key[(variant, *key)], label="")))
    out = ["", f"## {baseline} and {variant} side by side", "",
           "Each percentage is fRust CPU as a percent of the official CPU measured on the same runner as that "
           "variant, so every column is a comparison with official. The two variants ran on different runners, "
           "possibly with different CPU models (see Hosts), which is why the CPU times are not set against each "
           "other here.", "",
           f"| Workload | Scenario | {baseline}: fRust as % of official | Pairs | {variant}: fRust as % of official "
           f"| Pairs | Difference (percentage points) |",
           "| --- | --- | --- | --- | --- | --- | --- |"]
    for scenario, workload in keys:
        first, second = by_key.get((baseline, scenario, workload)), by_key.get((variant, scenario, workload))
        difference = f"{(second['ratio'] - first['ratio']) * 100:+.1f}" if first and second else "-"
        out.append("| " + " | ".join(cell(part) for part in (
            workload, SCENARIO_NAMES.get(scenario, scenario),
            percent(first["ratio"]) if first else "-", first["pairs"] if first else "-",
            percent(second["ratio"]) if second else "-", second["pairs"] if second else "-", difference)) + " |")
    return out


def folio_summary(run, toolchain, phase):
    """Text for one cell of the page-cache table: PMD-mapped MiB of the driver and of libLLVM."""
    records = [r for r in run.get("folio", []) if r.get("toolchain") == toolchain and r.get("phase") == phase]
    if not records:
        return "-"
    record = records[-1]
    if "mib" not in record:
        return "probe failed"
    mib = record["mib"]
    return f"{mib.get('librustc_driver_FilePmdMapped', 0)} / {mib.get('libLLVM_FilePmdMapped', 0)}"


def folio_table(runs):
    """Markdown lines with one row per job that recorded the page-cache state of its toolchains."""
    members = [run for run in runs if run.get("folio")]
    if not members:
        return []
    members.sort(key=lambda run: (run["meta"]["label"],
                                  WORKLOADS.index(run["meta"]["workload"]) if run["meta"]["workload"] in WORKLOADS
                                  else len(WORKLOADS), run["meta"]["workload"]))
    out = ["", "## Page-cache state of the compilers", "",
           "MiB of `librustc_driver` / `libLLVM` text that a probe compile mapped with 2 MiB pages "
           "(`FilePmdMapped` in `/proc/PID/smaps`), taken before the first timed build and after the last. 0 means "
           "4 KiB pages. This depends on how the files entered the page cache, so a toolchain at 0 against one with "
           "large pages is slower for that reason alone. With cache state `evicted` the pages of both toolchains "
           "are dropped before every build and the probe follows an eviction.", "",
           "| Platform | Workload | Cache state | Official before | Official after | fRust before | fRust after |",
           "| --- | --- | --- | --- | --- | --- | --- |"]
    for run in members:
        meta = run["meta"]
        out.append("| " + " | ".join(cell(part) for part in (
            meta["label"], meta["workload"], meta.get("cache_state") or "not recorded",
            folio_summary(run, "official", "before"), folio_summary(run, "official", "after"),
            folio_summary(run, "frust", "before"), folio_summary(run, "frust", "after"))) + " |")
    return out


def platform_key(label):
    """Required platforms first, in bar order, then every other label alphabetically."""
    return (REQUIRED_PLATFORMS.index(label), label) if label in REQUIRED_PLATFORMS else (len(REQUIRED_PLATFORMS), label)


def pairs_text(count):
    return "more than 1000" if count is not None and count > 1000 else str(count)


def needed_text(row):
    needed = row.get("pairs_needed_to_exclude_parity")
    if row["verdict"] == "win" or row["verdict"] == "loss":
        return "enough"
    if needed is None:
        return "n/a"
    have = min((row.get("pairs_per_stratum") or {"": row["pairs"]}).values())
    if needed <= have:
        return f"at least {have} (sign test or order decides)"
    return f"{pairs_text(needed)} per workload"


def build_verdict(rows, required_platforms, required_scenarios, resources, run_text, min_pairs):
    """The machine-readable decision. A required cell is the all-workloads row of a platform and a
    scenario (or the workload row when that scenario has one workload only)."""
    by_key = {}
    for row in rows:
        if row["aggregate"] or row["workload"] not in WORKLOADS:
            by_key[(row["label"], row["scenario"], "all")] = row
        else:
            by_key.setdefault((row["label"], row["scenario"], "all"), None)
            by_key[(row["label"], row["scenario"], row["workload"])] = row
    cells, failing, missing = [], [], []
    for label in required_platforms:
        for scenario in required_scenarios:
            expected = EXPECTED_WORKLOADS.get(scenario, WORKLOADS)
            row = by_key.get((label, scenario, "all"))
            if row is None:
                missing.append(f"{label} {scenario}: no results")
                cells.append(dict(platform=label, scenario=scenario, verdict="missing"))
                continue
            present = row.get("members") or [row["workload"]]
            absent = [name for name in expected if name not in present]
            workloads = {name: by_key[(label, scenario, name)]["verdict"] for name in present if (label, scenario, name) in by_key}
            losses = sorted(name for name, verdict in workloads.items() if verdict == "loss")
            problems = []
            if absent:
                problems.append("missing workloads " + ", ".join(absent))
            if row["verdict"] != "win":
                problems.append(f"cell verdict {row['verdict']}")
            if losses:
                problems.append("loss in " + ", ".join(losses))
            if problems:
                failing.append(f"{label} {scenario}: " + "; ".join(problems))
            interval = row.get("interval")
            cells.append(dict(
                platform=label, scenario=scenario, verdict=row["verdict"], fRust_percent_of_official=round(row["ratio"] * 100, 3)
                if row.get("ratio") else None,
                interval_percent=[round(bound * 100, 3) for bound in interval] if interval else None,
                pairs=row["pairs"], pairs_per_workload=row.get("pairs_per_stratum"), workload_verdicts=workloads,
                workload_losses=losses, missing_workloads=absent, sign_p=row.get("sign_p"),
                pairs_needed_per_workload_to_exclude_parity=row.get("pairs_needed_to_exclude_parity"),
                pairs_needed_per_workload_for_1pct=row.get("pairs_needed_for_1pct"), flags=row["flags"]))
    measured = [cell_ for cell_ in cells if cell_["verdict"] != "missing"]
    cells_win = all(cell_["verdict"] == "win" and not cell_["workload_losses"] and not cell_["missing_workloads"]
                    for cell_ in measured)
    bar_met = bool(cells) and not missing and not failing
    out = dict(
        schema=VERDICT_SCHEMA, run=run_text or None, bar_met=bar_met,
        bar_met_for_measured_cells=bool(measured) and cells_win,
        rule=("bar_met is true only when every required platform has results for every required scenario, every "
              "such cell (all of its workloads together) has the verdict win, and no workload inside any of them "
              "has the verdict loss. win needs the 95% interval below 100% of official CPU, an exact sign test "
              f"below 0.05 in the same direction, and at least {min_pairs} pairs in every workload."),
        required_platforms=list(required_platforms), required_scenarios=list(required_scenarios),
        cells=cells, failing=failing, missing=missing)
    if resources is not None:
        out["resources"] = resources["verdict"]
        out["resources_met"] = resources["met"]
    return out


def verdict_lines(verdict):
    out = ["", "## Decision", "",
           f"Release bar (CPU time against official Rust 1.100.0, every cell a win): **{'MET' if verdict['bar_met'] else 'NOT MET'}**.",
           ""]
    if not verdict["bar_met"] and verdict["bar_met_for_measured_cells"]:
        out += ["Every cell that was measured is a win, but the run did not cover the whole bar.", ""]
    for item in verdict["failing"] + verdict["missing"]:
        out.append(f"- {item}")
    if "resources_met" in verdict:
        out += ["", f"Resource bar (locking, storage and memory against the previous fRust release): "
                    f"**{'MET' if verdict['resources_met'] else 'NOT MET'}**."]
    return out


def decision_tables(rows):
    """One table per platform with the four cells of the bar and the other edit cells beside them."""
    out = []
    labels = sorted({row["label"] for row in rows}, key=platform_key)
    for label in labels:
        members = {(row["scenario"]): row for row in rows
                   if row["label"] == label and (row["aggregate"] or row["workload"] not in WORKLOADS)}
        singles = {(row["scenario"]): row for row in rows if row["label"] == label and not row["aggregate"]
                   and (row["scenario"] not in members)}
        members.update(singles)
        out += ["", f"### {label}", "",
                "| Scenario | fRust as % of official | 95% interval | Verdict | Pairs | Faster in | Workloads (win / parity / loss) | More pairs needed to exclude parity |",
                "| --- | --- | --- | --- | --- | --- | --- | --- |"]
        for scenario in SCENARIOS:
            row = members.get(scenario)
            if row is None:
                continue
            name = SCENARIO_NAMES[scenario] + ("" if scenario in REQUIRED_SCENARIOS else " (not in the bar)")
            counts = row.get("member_verdicts") or {row["workload"]: row["verdict"]}
            tally = " / ".join(str(sum(1 for v in counts.values() if v == kind)) for kind in ("win", "parity", "loss"))
            out.append("| " + " | ".join(cell(part) for part in (
                name, percent(row["ratio"]) if row["ratio"] else "-", format_interval(row["interval"]),
                row["verdict"], row["pairs"], row["faster_text"], tally, needed_text(row))) + " |")
    return out


def short_name(path):
    """Platform label of a result file inside its artifact directory compare-<label>-<workload>[-s<shard>]."""
    parts = path.replace("\\", "/").split("/")
    for part in reversed(parts[:-1]):
        match = re.fullmatch(r"(?:compare-)?(.+?)-(?:ripgrep|syn|regex|fd)(?:-s\d+)?", part)
        if match:
            return match.group(1)
    return "unknown"


FAMILIES = {"cpu_total": "cpu", "max_rss_bytes": "memory", "total_bytes": "storage", "write_bytes": "storage",
            "apfs_footprint_bytes": "storage", "apfs_write_bytes": "storage", "lock_waits_total": "locking"}
FAMILY_NAMES = {"cpu": "CPU", "memory": "Peak memory", "storage": "Storage", "locking": "Lock waits"}
REQUIRED_FAMILIES = ("memory", "storage", "locking")


def load_resources(files):
    """Paired comparisons of qualification/concurrent-builds.py against the previous fRust release, read from
    its .ndjson summary records. Returns None when there are none. Each entry is one cell and metric with the
    paired median ratio (b over a, where a is the previous release) and the interval that run computed."""
    entries = []
    for path in files:
        platform = short_name(path)
        stem = pathlib.Path(path).stem
        with open(path, encoding="utf-8") as handle:
            for line in handle:
                line = line.strip()
                if not line:
                    continue
                try:
                    record = json.loads(line)
                except ValueError:
                    continue
                if record.get("kind") == "summary":
                    for item in record.get("cells", []):
                        for metric, value in sorted((item.get("ratios") or {}).items()):
                            family = FAMILIES.get(metric)
                            if family:
                                entries.append(dict(
                                    platform=platform, check=stem,
                                    cell=f"{item['scenario']} n={item['count']} {item['state']}", metric=metric,
                                    family=family, kind="ratio", median=value["median"], pairs=value["pairs"],
                                    interval=value.get("confidence95"), evidence=value.get("evidence")))
                elif record.get("kind") == "lock-summary":
                    for item in record.get("cells", []):
                        for lock, deltas in sorted((item.get("deltas") or {}).items()):
                            value = deltas.get("waits")
                            if value:
                                entries.append(dict(
                                    platform=platform, check=stem,
                                    cell=f"{item['scenario']} n={item['count']} {item['state']} {lock}",
                                    metric="lock waits (difference)", family="locking", kind="difference",
                                    median=value["median"], pairs=value["pairs"],
                                    interval=value.get("confidence95"), evidence=value.get("evidence")))
    return entries or None


def resource_decision(entries):
    """Per platform and family: how many cells are better (interval entirely below parity), unresolved and
    worse. A family is met when at least one cell is better and none is worse."""
    table, met = {}, True
    for platform in sorted({entry["platform"] for entry in entries}, key=platform_key):
        table[platform] = {}
        for family in FAMILY_NAMES:
            members = [e for e in entries if e["platform"] == platform and e["family"] == family]
            better = [e for e in members if e["evidence"] == "lower"]
            worse = [e for e in members if e["evidence"] == "higher"]
            table[platform][family] = dict(
                cells=len(members), better=len(better), unresolved=len(members) - len(better) - len(worse),
                worse=len(worse), worse_cells=[f"{e['check']}: {e['cell']} {e['metric']}" for e in worse],
                met=bool(better) and not worse)
            if family in REQUIRED_FAMILIES and not table[platform][family]["met"]:
                met = False
    return dict(verdict=table, met=met and bool(table))


def resource_lines(entries, decision):
    out = ["", "## Resources against the previous fRust release", "",
           "Paired comparison of the fRust archive under test with the previous release archive for the same "
           "platform, from `qualification/concurrent-builds.py`. A cell is better when the 95% interval of the "
           "paired median ratio (or of the paired difference in waits) lies entirely below parity, worse when "
           "entirely above. Each cell is judged on its own, without correction for the number of cells, so read the "
           "list of worse cells before treating one as real. A family is met when at least one cell is better and "
           "none is worse. CPU is shown for reference and is not part of the resource bar.", "",
           "| Platform | Family | Cells | Better | Unresolved | Worse | Met |", "| --- | --- | --- | --- | --- | --- | --- |"]
    for platform, families in decision["verdict"].items():
        for family, info in families.items():
            if info["cells"]:
                out.append("| " + " | ".join(cell(part) for part in (
                    platform, FAMILY_NAMES[family], info["cells"], info["better"], info["unresolved"], info["worse"],
                    "-" if family == "cpu" else ("yes" if info["met"] else "no"))) + " |")
    worse = [(platform, item) for platform, families in decision["verdict"].items()
             for info in families.values() for item in info["worse_cells"]]
    if worse:
        out += ["", "Worse cells:", ""] + [f"- {platform}: {item}" for platform, item in worse]
    out += ["", "| Platform | Check | Cell | Metric | Paired median | 95% interval | Pairs | Evidence |",
            "| --- | --- | --- | --- | --- | --- | --- | --- |"]
    for entry in sorted(entries, key=lambda e: (platform_key(e["platform"]), e["check"], e["cell"], e["metric"])):
        low_high = "n/a" if not entry["interval"] else " to ".join(f"{bound:.4g}" for bound in entry["interval"])
        out.append("| " + " | ".join(cell(part) for part in (
            entry["platform"], entry["check"], entry["cell"], entry["metric"], f"{entry['median']:.4g}",
            low_high, entry["pairs"], entry["evidence"])) + " |")
    return out


def markdown(runs, skipped, rows, failures, notes, args, verdict, resources, decision):
    labels = sorted({row["label"] for row in rows}, key=platform_key)
    versions = sorted({first_line((run["meta"].get("toolchains") or {}).get("official", {}).get("rustc"))
                       for run in runs})
    out = ["# fRust build CPU against official Rust", ""]
    out.append(f"Official toolchain: {'; '.join(versions)}.")
    if args.run:
        out.append(f"Source run: {args.run}")
    out += verdict_lines(verdict)
    out += decision_tables(rows)
    if resources is not None:
        out += resource_lines(resources, decision)
    out += [
        "",
        "## Method",
        "",
        "Generated by `qualification/make-charts.py` from the raw `frust-compare-1` results of that run. Nothing "
        "here was committed, and the raw files hold every sample. `verdict.json` has the decision in machine-readable form.",
        "",
        "CPU is user plus system seconds of the whole cargo process tree, from `wait4`. A pair is the official and "
        "the fRust build of the same scenario run back to back, with the order alternating so that every scenario "
        "has half of its pairs in each order. The value of a pair is the log of the ratio of fRust CPU to official "
        "CPU. The percentage is the geometric mean over workloads of each workload's 25% trimmed mean of those values "
        "(every workload counts equally), so 100% means equal CPU and lower means fRust used less. Trimming keeps "
        "a build that a noisy neighbour slowed from moving the estimate, and no sample is discarded: the plain mean "
        "and the median are in `summary.json`. The 95% interval is the wider, at each end, of Yuen's interval for "
        "a trimmed mean and a stratified bootstrap of the same estimator, and it is widened when consecutive rounds "
        "are clearly correlated. A cell is a win when that interval lies below 100%, an exact sign test over every "
        "pair agrees (p < 0.05) and every workload has at least "
        f"{args.min_pairs} pairs; a loss for the mirror image; insufficient below the pair minimum; otherwise parity. "
        "Both sides build the same frozen sources with the same release profile. The fRust cold build starts with an "
        "empty unit cache store, the clean rebuild with the store filled by that round's cold build, and the "
        "official toolchain has no store. The fRust toolchain is installed with `frust-install` from the "
        "release-format archive, the way users get it. The cache state column of the hosts table says whether the "
        "toolchains were left as installed or had their library pages evicted before every build.",
        "",
        "## Hosts",
        "",
        "| Platform | CPU | Cores (physical/logical) | OS | Pinning | Official rustc | Official cargo | fRust rustc "
        "| fRust cargo | Commit | Unit cache | Cache state | fRust env | Cargo -j | Result files |",
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for key, count in host_summary(runs):
        out.append("| " + " | ".join(cell(part) for part in key) + f" | {count} |")
    out += folio_table(runs)
    for baseline, variant in paired_variants(rows):
        out += variant_table(rows, baseline, variant)
    for label in labels:
        out += ["", f"## {label}", "",
                "| Workload | Scenario | Official CPU | fRust CPU | fRust as % of official | Median | Range over pairs "
                "| 95% interval | Verdict | Faster in | Pairs | fRust store hit / miss | Flags |",
                "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
        for row in sorted((r for r in rows if r["label"] == label), key=order_key):
            out.append("| " + " | ".join(cell(part) for part in (
                row["workload"], SCENARIO_NAMES.get(row["scenario"], row["scenario"]), seconds(row["official_cpu"]),
                seconds(row["frust_cpu"]), percent(row["ratio"]) if row["ratio"] else "-",
                percent(row["median_ratio"]) if row["median_ratio"] else "-",
                f"{percent(row['ratio_min'])} to {percent(row['ratio_max'])}" if row["ratio_min"] else "-",
                format_interval(row["interval"]), row["verdict"], row["faster_text"], row["pairs"],
                row["cache_hit_miss"] or "-", ", ".join(row["flags"]) or "none")) + " |")
    out += ["", "The `all workloads` row pools the workloads of the scenario with equal weight. Use the per-workload "
            "rows to check for regressions and missing data. Store hit / miss is the median number of unit cache "
            "decisions per fRust build, counted from the cache's debug output.", "", "## Flags", ""]
    flagged = [row for row in sorted(rows, key=order_key) if row["verdict"] != "win" or row["flags"]]
    if flagged:
        out.append("Scenarios below are not clean wins and must not be reported as speedups without these caveats.")
        out.append("")
        for row in flagged:
            ratio = percent(row["ratio"]) if row["ratio"] else "no ratio"
            out.append(f"- {row['label']}, {row['workload']}, {SCENARIO_NAMES.get(row['scenario'], row['scenario'])}: "
                       f"{ratio} of official over {row['pairs']} pairs, {row['verdict']}. "
                       f"{'; '.join(row['flags']) or 'No further flags.'}")
    else:
        out.append("Every cell is a win.")
    if failures:
        out += ["", "## Failed builds", ""]
        for (label, workload), count in sorted(failures.items()):
            out.append(f"- {label} {workload}: {count} build(s) exited non-zero and are excluded from the pairs.")
    if notes:
        out += ["", "## Run notes", ""] + [f"- {note}" for note in sorted(notes)]
    if skipped:
        out += ["", "## Skipped files", ""] + [f"- {path} has no `frust-compare-1` meta record" for path in skipped]
    out += ["", "## Scenarios", ""]
    out += [f"- {SCENARIO_NAMES[name]}: {SCENARIO_NOTES[name]}." for name in SCENARIOS]
    out.append("")
    return "\n".join(out)


FONT = "system-ui, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
STYLE = (
    ".bg{fill:#ffffff}.t{fill:#1f2328}.m{fill:#59636e}.grid{stroke:#d1d9e0}.ref{stroke:#1f2328}"
    ".ok{fill:#1f6feb}.bad{fill:#cf5a0a}.unresolved{fill:#8c959f}"
    "@media (prefers-color-scheme: dark){.bg{fill:#0d1117}.t{fill:#e6edf3}.m{fill:#9198a1}.grid{stroke:#30363d}"
    ".ref{stroke:#e6edf3}.ok{fill:#58a6ff}.bad{fill:#ff9b54}.unresolved{fill:#6e7681}}"
)


def chart(label, rows, versions):
    """One horizontal bar per scenario and workload: fRust CPU as a percent of official CPU."""
    rows = sorted((r for r in rows if r["label"] == label and r["ratio"]), key=order_key)
    top_pct = max((row["interval"][1] if row["interval"] else row["ratio"]) for row in rows) * 100
    axis_max = max(125, int(math.ceil(top_pct / 25.0)) * 25)
    left, plot_w, right = 250, 460, 250
    width = left + plot_w + right
    row_h, head_h, top = 24, 30, 96

    def x_of(pct):
        return left + plot_w * pct / axis_max

    body, y = [], top
    scenarios = [s for s in SCENARIOS if any(r["scenario"] == s for r in rows)]
    for scenario in scenarios:
        body.append(f'<text class="t" x="16" y="{y + 20}" font-size="13" font-weight="bold">'
                    f'{escape(SCENARIO_NAMES[scenario])}</text>')
        y += head_h
        for row in (r for r in rows if r["scenario"] == scenario):
            pct = row["ratio"] * 100
            colour = {"loss": "bad", "win": "ok"}.get(row["verdict"], "unresolved")
            name = AGGREGATE + " (geometric mean)" if row["aggregate"] else row["workload"]
            text = f"{pct:.1f}%  n={row['pairs']}  {row['verdict']}"
            end = x_of(pct)
            body.append(f'<text class="t" x="36" y="{y + 16}" font-size="12">{escape(name)}</text>')
            body.append(f'<rect class="{colour}" x="{left}" y="{y + 3}" width="{x_of(pct) - left:.2f}" height="16"/>')
            if row["interval"]:
                low, high = (x_of(bound * 100) for bound in row["interval"])
                body.append(f'<line class="ref" x1="{low:.2f}" y1="{y + 11}" x2="{high:.2f}" y2="{y + 11}" stroke-width="1.5"/>')
                body.append(f'<line class="ref" x1="{low:.2f}" y1="{y + 6}" x2="{low:.2f}" y2="{y + 16}" stroke-width="1.5"/>')
                body.append(f'<line class="ref" x1="{high:.2f}" y1="{y + 6}" x2="{high:.2f}" y2="{y + 16}" stroke-width="1.5"/>')
                end = max(end, high)
            body.append(f'<text class="t" x="{end + 6:.2f}" y="{y + 16}" font-size="12">{escape(text)}</text>')
            y += row_h
        y += 6
    bottom = y
    height = bottom + 28
    grid = []
    for pct in range(0, axis_max + 1, 25):
        if pct == 100:
            continue
        grid.append(f'<line class="grid" x1="{x_of(pct):.2f}" y1="{top - 8}" x2="{x_of(pct):.2f}" y2="{bottom}"/>')
        grid.append(f'<text class="m" x="{x_of(pct):.2f}" y="{top - 14}" font-size="11" text-anchor="middle">{pct}%</text>')
    ref = (f'<line class="ref" x1="{x_of(100):.2f}" y1="{top - 8}" x2="{x_of(100):.2f}" y2="{bottom}" '
           f'stroke-width="2" stroke-dasharray="5 4"/>'
           f'<text class="t" x="{x_of(100):.2f}" y="{top - 14}" font-size="11" font-weight="bold" '
           f'text-anchor="middle">100%</text>')
    title = f"fRust CPU as a percent of official {versions}, {label}"
    title_attr = escape(title, {'"': "&quot;"})
    head = [
        f'<text class="t" x="16" y="26" font-size="16" font-weight="bold">{escape(title)}</text>',
        f'<text class="m" x="16" y="46" font-size="12">Trimmed mean of paired log ratios, whiskers are the 95% interval, n = pairs. The dashed '
        f'line is 100% (equal CPU).</text>',
        f'<text class="m" x="16" y="62" font-size="12">Blue is a win, orange a loss, grey parity or too few pairs (see the verdict).</text>',
        f'<text class="m" x="16" y="78" font-size="12">CPU is user plus system time of the whole cargo process tree.</text>',
    ]
    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" '
        f'role="img" aria-label="{title_attr}" font-family="{FONT}">',
        f'<style>{STYLE}</style>',
        f'<title>{escape(title)}</title>',
        f'<rect class="bg" x="0" y="0" width="{width}" height="{height}"/>',
    ] + head + grid + body + [ref, "</svg>", ""]
    return "\n".join(parts)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("inputs", nargs="+")
    parser.add_argument("--out", required=True)
    parser.add_argument("--run", default="", help="text naming the source run, for example its URL")
    parser.add_argument("--min-pairs", type=int, default=10,
                        help="pairs every workload of a cell needs before the cell can be called a win or a loss")
    parser.add_argument("--bootstrap", type=int, default=PAIRED["BOOTSTRAP_REPS"], help="bootstrap resamples per interval")
    parser.add_argument("--require-platforms", default=" ".join(REQUIRED_PLATFORMS),
                        help="platform labels the release bar needs, space or comma separated")
    parser.add_argument("--require-scenarios", default=" ".join(REQUIRED_SCENARIOS),
                        help="scenarios of the release bar, space or comma separated")
    parser.add_argument("--enforce", action="store_true", help="exit 1 unless the bar is met")
    args = parser.parse_args()
    if args.min_pairs < 4:
        sys.exit("--min-pairs must be at least 4: the trimmed-mean interval needs four pairs per workload")

    files = find_files(args.inputs)
    runs, skipped, omitted = load_runs(files)
    if not runs:
        sys.exit("no frust-compare-1 results found in the inputs")
    pairs, failures, notes, requested = collect(runs)
    notes += [f"{item.get('label', 'unknown')} {item.get('workload', 'unknown')}: not run. "
              f"{item.get('reason', 'no reason recorded')}" for item in omitted]
    rows = summarize(pairs, requested, args.min_pairs, args.bootstrap)
    ndjson = [path for path in find_files(args.inputs, ".ndjson") if path.endswith(".ndjson")]
    entries = load_resources(ndjson) if ndjson else None
    decision = resource_decision(entries) if entries else None
    platforms = tuple(part for part in re.split(r"[,\s]+", args.require_platforms) if part)
    scenarios = tuple(part for part in re.split(r"[,\s]+", args.require_scenarios) if part)
    verdict = build_verdict(rows, platforms, scenarios, decision, args.run, args.min_pairs)
    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    (out / "results.md").write_text(markdown(runs, skipped, rows, failures, notes, args, verdict, entries, decision),
                                    encoding="utf-8")
    (out / "summary.json").write_text(json.dumps(
        dict(schema=SCHEMA, min_pairs=args.min_pairs, bootstrap=args.bootstrap, rows=sorted(rows, key=order_key)),
        indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (out / "verdict.json").write_text(json.dumps(verdict, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    versions = sorted({first_line((run["meta"].get("toolchains") or {}).get("official", {}).get("rustc"))
                       for run in runs})
    short = "; ".join(line.replace("rustc ", "Rust ").split(" (")[0] for line in versions)
    for label in sorted({row["label"] for row in rows if row["ratio"]}):
        (out / f"chart-{label}.svg").write_text(chart(label, rows, short), encoding="utf-8")
    print(f"wrote {out}/results.md, summary.json, verdict.json and {len({r['label'] for r in rows})} chart(s) from "
          f"{len(runs)} result file(s), {sum(len(v) for v in pairs.values())} pairs")
    print(f"release bar {'met' if verdict['bar_met'] else 'NOT met'}"
          + ("" if not verdict["failing"] else ": " + "; ".join(verdict["failing"][:3])
             + (" ..." if len(verdict["failing"]) > 3 else "")))
    if args.enforce and not verdict["bar_met"]:
        sys.exit(1)


if __name__ == "__main__":
    main()
