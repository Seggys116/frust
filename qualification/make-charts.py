#!/usr/bin/env python3
"""Turn the raw JSON lines of qualification/ci-bench.py into a markdown report and SVG charts.

Usage: make-charts.py INPUT... --out DIR [--run TEXT] [--min-pairs N]

INPUT is a results file or a directory searched for *.jsonl. Each file is one job: a meta record
followed by sample records. For every platform, workload and scenario the report pairs the
official and fRust build of the same round and states the median of the paired CPU ratios with
the number of pairs and a confidence interval. Incomplete and inconclusive scenarios stay visible.

Platforms are grouped by label. The two Linux variants have their own labels (linux-x86_64 and
linux-x86_64-v3), and so do the optional rustc-PGO candidates of macOS (macos-arm64-pgo and
macos-x86_64-pgo). Each is compared with the official toolchain of its own runner, and when a
variant and its baseline are both present the report adds a table with the two side by side. The
page-cache probe records of each job
("folio", see ci-bench.py) become one row per job, and a job a runner skipped (a "skipped" record,
written when the runner CPU cannot run the v3 toolchain) is listed in the run notes.

The output is deterministic: the same input gives byte-identical files. Standard library only.
Written to DIR: results.md, summary.json and one chart-<platform>.svg per platform.
"""
import argparse
import json
import math
import os
import pathlib
import runpy
import statistics
import sys
from xml.sax.saxutils import escape

PAIRED = runpy.run_path(pathlib.Path(__file__).with_name("paired_stats.py"))
SCHEMA = "frust-compare-1"
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
    "noop": "immediate rebuild, median of several per round",
    "edit-leaf": "frozen leaf edit applied to a copy of the clean target",
    "edit-wide": "frozen wide edit applied to a copy of the clean target",
    "edit-holdout": "frozen holdout edit applied to a copy of the clean target",
}
WORKLOADS = ("ripgrep", "syn", "regex", "fd")
AGGREGATE = "all workloads"


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


def find_files(inputs):
    files = []
    for item in inputs:
        if os.path.isdir(item):
            for root, dirs, names in os.walk(item):
                dirs.sort()
                files.extend(os.path.join(root, name) for name in sorted(names) if name.endswith(".jsonl"))
        elif os.path.isfile(item):
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


def collect(runs):
    """Return (pairs, failures, notes). pairs[(label, workload, scenario)] is a list of paired samples."""
    pairs, failures, notes, requested = {}, {}, [], {}
    for run in runs:
        meta = run["meta"]
        label, workload = meta["label"], meta["workload"]
        group = (label, workload)
        requested[group] = max(requested.get(group, 0), meta.get("rounds_requested") or 0)
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
            if frust is None or official["cpu"] <= 0 or frust["cpu"] < 0:
                continue
            pairs.setdefault((label, workload, scenario), []).append(dict(
                round=round_number, official=official["cpu"], frust=frust["cpu"],
                ratio=frust["cpu"] / official["cpu"], events=frust.get("cache_events") or {},
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


def confidence(ratios, want, min_pairs):
    interval = PAIRED["median_interval"](ratios)
    result = PAIRED["interval_result"](interval, 1.0)
    if len(ratios) < max(min_pairs, want):
        result = "incomplete"
    return interval, result


def summarize(pairs, requested, min_pairs):
    rows = []
    for (label, workload, scenario), items in sorted(pairs.items()):
        ratios = [item["ratio"] for item in items]
        median = statistics.median(ratios)
        want = requested.get((label, workload), 0)
        interval, result = confidence(ratios, want, min_pairs)
        flags = [] if result == "lower" else [f"paired evidence {result}"]
        faster = sum(1 for ratio in ratios if ratio < 1.0)
        if len(ratios) < min_pairs:
            flags.append(f"few pairs (minimum {min_pairs})")
        if want and len(ratios) < want:
            flags.append(f"{len(ratios)} of {want} requested rounds")
        if scenario == "noop":
            rebuilt = max(max(item["compiled_official"], item["compiled_frust"]) for item in items)
            if rebuilt:
                flags.append(f"no-op compiled up to {rebuilt} units")
                result = "recompiled"
        cache = None
        if scenario in ("cold", "clean") and any(item["events"] for item in items):
            cache = f"{median_events(items, 'hit'):g} / {median_events(items, 'miss'):g}"
        rows.append(dict(
            label=label, workload=workload, scenario=scenario, pairs=len(ratios),
            official_cpu=statistics.median(item["official"] for item in items),
            frust_cpu=statistics.median(item["frust"] for item in items),
            ratio=median, ratio_min=min(ratios), ratio_max=max(ratios), faster=faster,
            confidence95=interval, evidence=result,
            faster_text=f"{faster} of {len(ratios)}", cache_hit_miss=cache, flags=flags, aggregate=False,
        ))
    # Geometric mean of the per-workload median ratios, one row per platform and scenario.
    grouped = {}
    for row in rows:
        grouped.setdefault((row["label"], row["scenario"]), []).append(row)
    for (label, scenario), members in sorted(grouped.items()):
        if len(members) < 2:
            continue
        ratios = [row["ratio"] for row in members]
        faster = sum(1 for ratio in ratios if ratio < 1.0)
        bounds = [
            PAIRED["median_interval"]([pair["ratio"] for pair in pairs[label, row["workload"], scenario]],
                                      simultaneous=len(members))
            for row in members
        ]
        interval = tuple(geometric_mean(side) for side in zip(*bounds)) if all(bounds) else None
        result = PAIRED["interval_result"](interval, 1.0)
        if any(row["evidence"] in ("incomplete", "recompiled") for row in members):
            result = "incomplete"
        flags = [] if result == "lower" else [f"paired evidence {result}"]
        rows.append(dict(
            label=label, workload=AGGREGATE, scenario=scenario, pairs=sum(row["pairs"] for row in members),
            official_cpu=None, frust_cpu=None, ratio=geometric_mean(ratios), ratio_min=min(ratios), ratio_max=max(ratios),
            confidence95=interval, evidence=result,
            faster=faster, faster_text=f"{faster} of {len(ratios)} workloads", cache_hit_miss=None,
            flags=flags, aggregate=True,
        ))
    return rows


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


def markdown(runs, skipped, rows, failures, notes, args):
    labels = sorted({row["label"] for row in rows})
    versions = sorted({first_line((run["meta"].get("toolchains") or {}).get("official", {}).get("rustc"))
                       for run in runs})
    out = ["# fRust build CPU against official Rust", ""]
    out.append(f"Official toolchain: {'; '.join(versions)}.")
    if args.run:
        out.append(f"Source run: {args.run}")
    out += [
        "",
        "Generated by `qualification/make-charts.py` from the raw `frust-compare-1` results of that run. Nothing "
        "here was committed. Each number below is a median over the stated number of pairs, and the raw files hold "
        "every sample.",
        "",
        "CPU is user plus system seconds of the whole cargo process tree, from `wait4`. A pair is one round: the "
        "official and the fRust build of the same scenario, run one after the other with the order alternating "
        "between rounds. The percentage is the median of the per-pair ratios of fRust CPU to official CPU, so "
        "100% means equal CPU and lower means fRust used less. Both sides build the same frozen sources with the "
        "same release profile. The fRust cold build starts with an empty unit cache store, the clean rebuild with "
        "the store filled by that round's cold build, and the official toolchain has no store. The fRust toolchain "
        "is installed with `frust-install` from the release-format archive, the way users get it. The cache state "
        "column of the hosts table says whether the toolchains were left as installed or had their library pages "
        "evicted before every build.",
        "",
        "## Hosts",
        "",
        "| Platform | CPU | Cores (physical/logical) | OS | Pinning | Official rustc | Official cargo | fRust rustc "
        "| fRust cargo | Commit | Unit cache | Cache state | fRust env | Jobs |",
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |",
    ]
    for key, count in host_summary(runs):
        out.append("| " + " | ".join(cell(part) for part in key) + f" | {count} |")
    out += folio_table(runs)
    for baseline, variant in paired_variants(rows):
        out += variant_table(rows, baseline, variant)
    for label in labels:
        out += ["", f"## {label}", "",
                "| Workload | Scenario | Official CPU | fRust CPU | fRust as % of official | Range over pairs "
                "| 95% paired interval | Faster in | Pairs | fRust store hit / miss | Flags |",
                "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"]
        for row in sorted((r for r in rows if r["label"] == label), key=order_key):
            out.append("| " + " | ".join(cell(part) for part in (
                row["workload"], SCENARIO_NAMES.get(row["scenario"], row["scenario"]), seconds(row["official_cpu"]),
                seconds(row["frust_cpu"]), percent(row["ratio"]),
                f"{percent(row['ratio_min'])} to {percent(row['ratio_max'])}",
                format_interval(row["confidence95"]), row["faster_text"], row["pairs"],
                row["cache_hit_miss"] or "-", ", ".join(row["flags"]) or "none")) + " |")
    out += ["", "The `all workloads` row is the geometric mean of the workload medians. Its interval combines "
            "Bonferroni-adjusted bounds on those medians. A lower result requires the upper bound to be below 100%. "
            "Use the per-workload rows to check for regressions and missing data. Store hit / miss is the median number of unit cache "
            "decisions per fRust build, counted from the cache's debug output.", "", "## Flags", ""]
    flagged = [row for row in sorted(rows, key=order_key) if row["flags"]]
    if flagged:
        out.append("Scenarios below are not clean wins and must not be reported as speedups without these caveats.")
        out.append("")
        for row in flagged:
            out.append(f"- {row['label']}, {row['workload']}, {SCENARIO_NAMES.get(row['scenario'], row['scenario'])}: "
                       f"{percent(row['ratio'])} of official over {row['pairs']} pairs. {'; '.join(row['flags'])}.")
    else:
        out.append("All measured intervals fall below 100%, with the requested number of pairs.")
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
    rows = sorted((r for r in rows if r["label"] == label), key=order_key)
    top_pct = max(row["ratio"] for row in rows) * 100
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
            colour = {"higher": "bad", "lower": "ok"}.get(row["evidence"], "unresolved")
            name = AGGREGATE + " (geometric mean)" if row["aggregate"] else row["workload"]
            text = f"{pct:.1f}%  n={row['pairs']}"
            if row["flags"]:
                text += "  " + row["flags"][0]
            body.append(f'<text class="t" x="36" y="{y + 16}" font-size="12">{escape(name)}</text>')
            body.append(f'<rect class="{colour}" x="{left}" y="{y + 3}" width="{x_of(pct) - left:.2f}" height="16"/>')
            body.append(f'<text class="t" x="{x_of(pct) + 6:.2f}" y="{y + 16}" font-size="12">{escape(text)}</text>')
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
        f'<text class="m" x="16" y="46" font-size="12">Median of paired ratios, n = pairs. The dashed line is 100% '
        f'(equal CPU).</text>',
        f'<text class="m" x="16" y="62" font-size="12">Colour indicates whether the 95% interval falls below or above parity.</text>',
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
    parser.add_argument("--min-pairs", type=int, default=3)
    args = parser.parse_args()

    runs, skipped, omitted = load_runs(find_files(args.inputs))
    if not runs:
        sys.exit("no frust-compare-1 results found in the inputs")
    pairs, failures, notes, requested = collect(runs)
    notes += [f"{item.get('label', 'unknown')} {item.get('workload', 'unknown')}: not run. "
              f"{item.get('reason', 'no reason recorded')}" for item in omitted]
    rows = summarize(pairs, requested, args.min_pairs)
    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    (out / "results.md").write_text(markdown(runs, skipped, rows, failures, notes, args), encoding="utf-8")
    (out / "summary.json").write_text(json.dumps(
        dict(schema=SCHEMA, min_pairs=args.min_pairs, rows=sorted(rows, key=order_key)),
        indent=2, sort_keys=True) + "\n", encoding="utf-8")
    versions = sorted({first_line((run["meta"].get("toolchains") or {}).get("official", {}).get("rustc"))
                       for run in runs})
    short = "; ".join(line.replace("rustc ", "Rust ").split(" (")[0] for line in versions)
    for label in sorted({row["label"] for row in rows}):
        (out / f"chart-{label}.svg").write_text(chart(label, rows, short), encoding="utf-8")
    print(f"wrote {out}/results.md, summary.json and {len({r['label'] for r in rows})} chart(s) from "
          f"{len(runs)} result file(s), {sum(len(v) for v in pairs.values())} pairs")


if __name__ == "__main__":
    main()
