#!/usr/bin/env python3
"""Summarize CPU/disk benchmark JSONL as Markdown or a T3 chart."""

import argparse
from collections import defaultdict
import json
from pathlib import Path
import random
from statistics import median


TOOL_NAMES = {"official": "Official Rust", "control": "Source-built control", "frust": "fRust",
              "compressed": "Compressed DWARF", "candidate": "Candidate fRust"}
MIB = 1024 ** 2


def runtime_upper(pairs):
    if len(pairs) < 2:
        return None
    rng = random.Random(0)
    samples = sorted(median(rng.choices(pairs, k=len(pairs))) for _ in range(2000))
    return samples[1899]


def observations(lanes):
    groups = {}
    for lane in lanes:
        for line in (lane / "samples.jsonl").read_text().splitlines():
            row = json.loads(line)
            key = row["workload"], row["profile"]
            case = row["case"]
            kind = case.split("-")[0] if case.startswith(("runtime-", "noop-")) else case
            group = groups.setdefault(key, {})
            group.setdefault(kind, defaultdict(dict))[row["toolchain"]][
                (lane, row["round"], case)
            ] = row
    return groups


def metrics(groups, runtime_reference=None):
    result = {}
    for (workload, profile), cases in groups.items():
        result.setdefault(workload, {})[profile] = summary = {}
        seed = cases.get("cold", {})
        for case, by_tool in cases.items():
            summary[case] = values = {}
            for tool, samples in by_tool.items():
                rows = list(samples.values())
                item = {}
                for field in ("cpu", "wall", "user", "system", "peak_rss_kib", "compiled",
                              "stripped_bytes", "artifact_bytes"):
                    field_rows = [row[field] for row in rows if field in row]
                    if field_rows:
                        item[field] = median(field_rows)
                with_disk = [row for row in rows if "disk" in row]
                if with_disk:
                    for field in ("logical", "allocated", "entries"):
                        item[field] = median(row["disk"][field] for row in with_disk)
                    for directory, field in (("incremental", "incremental"), ("deps", "deps"),
                                             ("build", "build_scripts"),
                                             (".fingerprint", "fingerprints")):
                        item[field] = median(
                            sum(part["allocated"] for name, part in row["disk"]["parts"].items()
                                if name.endswith(f"/{directory}"))
                            for row in with_disk
                        )
                    if tool in seed:
                        item["delta"] = item["allocated"] - median(
                            row["disk"]["allocated"] for row in seed[tool].values()
                        )
                    if case == "clean":
                        item["reclaimed"] = median(
                            row["disk_before"]["allocated"] - row["disk"]["allocated"]
                            for row in with_disk
                        )
                item["samples"] = len(rows)
                compare_candidate = tool in ("compressed", "candidate") and "frust" in by_tool
                reference = runtime_reference or ("frust" if compare_candidate else "official")
                if case == "runtime" and tool != reference:
                    baseline = by_tool.get(reference, {})
                    paired = [row["cpu"] / baseline[key]["cpu"]
                              for key, row in samples.items() if key in baseline]
                    if paired:
                        item["ratio"] = median(paired)
                        item["upper95"] = runtime_upper(paired)
                        item["reference"] = reference
                        item["pairs"] = len(paired)
                values[tool] = item
    return result


def print_markdown(result):
    tools = [label for label in TOOL_NAMES if any(
        label in by_tool for profiles in result.values() for cases in profiles.values()
        for by_tool in cases.values()
    )]
    print("# Build CPU, disk and runtime detail\n")
    print("Each cell reports CPU seconds / wall seconds / allocated MiB / incremental MiB. "
          "Dashes indicate metrics outside the operation. Repeated timings use medians. "
          "The incremental column is the portion allocated to Cargo’s incremental directories.\n")
    for workload, profiles in result.items():
        print(f"## {workload}\n")
        for profile, cases in profiles.items():
            print(f"### {profile}\n")
            print("| Operation | " + " | ".join(TOOL_NAMES[tool] for tool in tools) + " |")
            print("| --- |" + " ---: |" * len(tools))
            for case, by_tool in cases.items():
                cells = []
                for tool in tools:
                    if tool not in by_tool:
                        cells.append("—")
                        continue
                    row = by_tool[tool]
                    values = [f"{row['cpu']:.4f}", f"{row['wall']:.4f}"]
                    values.extend(f"{row[key]/MIB:.2f}" if key in row else "—"
                                  for key in ("allocated", "incremental"))
                    cells.append(" / ".join(values))
                print(f"| {case} | " + " | ".join(cells) + " |")
            print()
    print("## Paired executable runtime CPU\n")
    print("Ratios use the median within-run toolchain/reference pair. "
          "Upper bounds are one-sided 95% paired bootstrap estimates.\n")
    print("| Workload | Profile | Toolchain | Reference | Median | Upper 95% | Pairs |")
    print("| --- | --- | --- | --- | ---: | ---: | ---: |")
    for workload, profiles in result.items():
        for profile, cases in profiles.items():
            for tool, row in cases.get("runtime", {}).items():
                if "ratio" not in row:
                    continue
                upper = f"{row['upper95']:.4f}" if row["upper95"] is not None else "—"
                print(f"| {workload} | {profile} | {TOOL_NAMES[tool]} | {TOOL_NAMES[row['reference']]} "
                      f"| {row['ratio']:.4f} | {upper} | {row['pairs']} |")


def print_html(result):
    data = json.dumps(result, separators=(",", ":")).replace("<", "\\u003c")
    print("""<!doctype html><meta charset="utf-8">
<style>
main{font:14px var(--font-sans);color:var(--foreground)}
form{display:flex;gap:10px;flex-wrap:wrap;margin-bottom:16px}
label{display:grid;gap:4px;font-size:12px;color:var(--muted-foreground)}
[hidden]{display:none}
select{background:var(--background);color:var(--foreground);border:1px solid var(--border);
border-radius:var(--radius);padding:5px;max-width:100%}
.row{margin:0 0 14px}
.caption{display:flex;justify-content:space-between;gap:12px;margin-bottom:5px}
.track{position:relative;height:16px;background:var(--secondary);border-radius:4px}
.bar{height:100%;border-radius:4px}
.center::before{content:'';position:absolute;left:50%;height:100%;border-left:1px solid var(--border)}
small{display:block;color:var(--muted-foreground);margin:10px 0}
</style>
<main><form>
<label>Workload<select id="workload"></select></label>
<label id="view-label">Build profile<select id="profile"></select></label>
<label>Operation<select id="operation"></select></label>
<label>Metric<select id="metric"></select></label>
<label>Compare<select id="compare"><option value="tools">Toolchains</option>
<option value="profiles">Profiles</option></select></label>
<label id="tool-label" hidden>Toolchain<select id="tool"></select></label>
</form><div id="chart" role="img"></div><small id="note"></small></main>
<script type="application/json" id="data">""" + data + """</script>
<script>
const data = JSON.parse(document.querySelector('#data').textContent);
const [workload, profile, operation, metric, compare, tool] = document.querySelectorAll('select');
const chart = document.querySelector('#chart'), note = document.querySelector('#note');
const labels = {official:'Official Rust', control:'Source-built Rust', frust:'fRust',
                compressed:'fRust with compressed DWARF',candidate:'Candidate fRust'};
const fields = {cpu:'CPU seconds',wall:'Wall seconds',user:'User CPU seconds',
system:'System CPU seconds',allocated:'Allocated MiB',logical:'Logical MiB',
incremental:'Incremental MiB',deps:'Dependency MiB',build_scripts:'Build script MiB',
fingerprints:'Fingerprint MiB',delta:'Change since cold seed (MiB)',
reclaimed:'Reclaimed by clean (MiB)',stripped_bytes:'Stripped output MiB',
artifact_bytes:'Executable MiB',peak_rss_kib:'Peak process RSS MiB',entries:'Filesystem entries'};
const isBytes = key => ['allocated','logical','incremental','deps','build_scripts','fingerprints','delta','reclaimed',
                         'stripped_bytes','artifact_bytes'].includes(key);
function fill(select, values, pretty = x => x) {
  const previous = select.value;
  select.replaceChildren(...values.map(value => new Option(pretty(value), value)));
  if (values.includes(previous)) select.value = previous;
}
function records() {
  const profiles = data[workload.value];
  if (compare.value === 'profiles') {
    return Object.entries(profiles)
      .filter(([, cases]) => cases[operation.value]?.[tool.value])
      .map(([profile, cases]) => [profile, cases[operation.value][tool.value]]);
  }
  const rows = profiles[profile.value][operation.value];
  return Object.keys(labels).filter(name => rows[name]).map(name => [labels[name], rows[name]]);
}
function show() {
  const field = metric.value;
  const rows = records().filter(([, row]) => field in row);
  const numbers = rows.map(([, row]) => row[field]);
  const extent = Math.max(1e-9, ...numbers.map(Math.abs));
  chart.replaceChildren();
  rows.forEach(([label], index) => {
    const value = numbers[index];
    const display = value / (isBytes(field) ? 1048576 : field === 'peak_rss_kib' ? 1024 : 1);
    const row = document.createElement('div');
    row.className = 'row';
    const caption = document.createElement('div');
    caption.className = 'caption';
    for (const text of [label, display.toFixed(field === 'entries' ? 0 : 3)]) {
      const item = document.createElement('span');
      item.textContent = text;
      caption.append(item);
    }
    const track = document.createElement('div'), bar = document.createElement('div');
    track.className = 'track';
    bar.className = 'bar';
    bar.style.width = 100 * Math.abs(value) / extent + '%';
    if (field === 'delta') {
      track.classList.add('center');
      bar.style.position = 'absolute';
      bar.style.left = (value < 0 ? 50 + 50 * value / extent : 50) + '%';
      bar.style.width = 50 * Math.abs(value) / extent + '%';
    }
    bar.style.background = 'var(--chart-' + (index % 5 + 1) + ')';
    track.append(bar);
    row.append(caption, track);
    chart.append(row);
  });
  chart.setAttribute('aria-label', [workload.value, operation.value, fields[field]].join(', '));
  if (compare.value === 'tools') {
    note.textContent = rows.map(([label, row]) => label + ' n=' + row.samples).join(' · ');
    const candidate = data[workload.value][profile.value][operation.value].frust;
    if (operation.value === 'runtime' && typeof candidate?.upper95 === 'number')
      note.textContent += ' · fRust/official paired CPU upper 95% ' + candidate.upper95.toFixed(3);
  } else {
    note.textContent = labels[tool.value] + ' · profile runs are separate sessions';
  }
}
function operations() {
  const profiles = data[workload.value];
  const cases = compare.value === 'profiles'
    ? [...new Set(Object.values(profiles).flatMap(Object.keys))]
    : Object.keys(profiles[profile.value]);
  fill(operation, cases);
  fieldOptions();
}
function fieldOptions() {
  const rows = records();
  fill(metric, Object.keys(fields).filter(key => rows.some(([, row]) => key in row)), key => fields[key]);
  show();
}
function profiles() {
  fill(profile, Object.keys(data[workload.value]));
  const cases = Object.values(data[workload.value]).flatMap(Object.values);
  fill(tool, Object.keys(labels).filter(name => cases.some(group => name in group)), name => labels[name]);
  operations();
}
fill(workload, Object.keys(data));
workload.onchange = profiles;
profile.onchange = operations;
operation.onchange = fieldOptions;
metric.onchange = show;
tool.onchange = fieldOptions;
compare.onchange = () => {
  const byProfile = compare.value === 'profiles';
  document.querySelector('#view-label').hidden = byProfile;
  document.querySelector('#tool-label').hidden = !byProfile;
  operations();
};
profiles();
if ([...profile.options].some(option => option.value === 'fat16-incremental')) {
  profile.value = 'fat16-incremental';
  operations();
  if ([...operation.options].some(option => option.value === 'edit')) {
    operation.value = 'edit';
    fieldOptions();
  }
}
</script>""")


def screen(groups, candidate):
    failures = 0
    checks = 0
    for (workload, profile), cases in groups.items():
        for case, per_tool in cases.items():
            if case.startswith("trace-"):
                continue
            label = f"{workload} {profile} {case}"
            checks += 1
            official, tested = per_tool.get("official", {}), per_tool.get(candidate, {})
            if not official or official.keys() != tested.keys():
                print(f"FAIL {label} missing paired samples")
                failures += 1
                continue
            ratios = [tested[key]["cpu"] / row["cpu"] for key, row in official.items()]
            if case == "runtime":
                bound = runtime_upper(ratios)
                failed = bound is None or bound > 1.01
                if failed:
                    print(f"FAIL {label} runtime upper {bound} exceeds 1.01 or is unavailable")
            else:
                ratio = median(ratios)
                failed = ratio >= 1
                if failed:
                    print(f"FAIL {label} build CPU ratio {ratio:.4f} is at least 1.00")
            failures += failed
            if all("disk" in row and "disk" in tested[key] for key, row in official.items()):
                disk = median(tested[key]["disk"]["allocated"] - row["disk"]["allocated"]
                              for key, row in official.items())
                checks += 1
                if disk > 0:
                    failures += 1
                    print(f"FAIL {label} retained {disk / MIB:.3f} MiB more than official Rust")
    if not checks:
        print("FAIL no paired build or runtime checks")
        return 1
    print(f"{failures} failures in {checks} measured checks")
    return int(failures > 0)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("lanes", nargs="+", type=Path)
    parser.add_argument("--runtime-reference", choices=TOOL_NAMES,
                        help="select the reference for reported paired runtime CPU")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--html", action="store_true")
    mode.add_argument("--screen", choices=("frust", "candidate"),
                      help="require faster paired build CPU and no storage or runtime regression")
    args = parser.parse_args()
    observed = observations(args.lanes)
    if not observed:
        parser.error("no samples found")
    if args.screen:
        return screen(observed, args.screen)
    (print_html if args.html else print_markdown)(metrics(observed, args.runtime_reference))


if __name__ == "__main__":
    raise SystemExit(main())
