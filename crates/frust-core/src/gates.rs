//! Release-gate arithmetic from paired samples.
//!
//! Thresholds are the frozen Linux budgets. An inconclusive budget is a failure.

use std::collections::BTreeMap;

pub const EDIT_GEOMEAN_MAX: f64 = 0.80;
pub const COLD_MEDIAN_MAX: f64 = 1.05;
pub const NOOP_RATIO: f64 = 0.05;
pub const NOOP_ABS_SECS: f64 = 0.020;
pub const TAIL_P95_MAX: f64 = 1.05;
pub const MEMORY_MAX: f64 = 1.10;
pub const DISK_MAX: f64 = 1.20;
pub const RUNTIME_WORKLOAD_UPPER: f64 = 1.01;
pub const RUNTIME_AGGREGATE_MAX: f64 = 0.97;
pub const SIZE_AGGREGATE_MAX: f64 = 0.97;
pub const MIN_MEDIAN_PAIRS: usize = 30;
pub const MIN_TAIL_PAIRS: usize = 60;
const PRIMARY: &[&str] = &["ripgrep", "syn", "regex"];
const BUILD_COHORT: &[&str] = &["ripgrep", "syn", "regex", "fd"];
const RUNTIME_COHORT: &[&str] = &["ripgrep", "rustfmt", "oxipng"];

pub const BOOTSTRAP_REPLICATES: usize = 2000;
pub const BOOTSTRAP_SEED: u64 = 0xF005_7100_6A7E;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditKind {
    Leaf,
    Wide,
    Cold,
    Noop,
    Holdout,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditPair {
    pub workload: String,
    pub kind: EditKind,
    pub frust_secs: f64,
    pub stock_secs: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResourcePair {
    pub workload: String,
    pub frust_peak_rss: u64,
    pub stock_peak_rss: u64,
    pub frust_bytes: u64,
    pub stock_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeWorkload {
    pub workload: String,
    pub frust_secs: Vec<f64>,
    pub stock_secs: Vec<f64>,
    pub identical_code: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SizePair {
    pub workload: String,
    pub frust_stripped: u64,
    pub stock_stripped: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateRow {
    pub name: String,
    pub status: &'static str,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GateReport {
    pub gates: Vec<GateRow>,
    pub edit_geomean: Option<f64>,
    pub edit_upper: Option<f64>,
    pub cold_median: Option<f64>,
    pub noop_overhead_secs: Option<f64>,
    pub noop_allowance_secs: Option<f64>,
    pub tail_p95: Vec<(String, f64)>,
    pub memory_ratio: Option<f64>,
    pub disk_ratio: Option<f64>,
    pub runtime_geomean: Option<f64>,
    pub runtime_upper: Option<f64>,
    pub size_geomean: Option<f64>,
    pub holdout_median: Option<f64>,
}

pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let mut sorted = values.to_vec();
    Some(median_in_place(&mut sorted))
}

fn median_in_place(values: &mut [f64]) -> f64 {
    let len = values.len();
    let (lower, middle, _) = values.select_nth_unstable_by(len / 2, f64::total_cmp);
    if len % 2 == 0 {
        let previous = lower.iter().copied().max_by(f64::total_cmp).unwrap();
        previous.midpoint(*middle)
    } else {
        *middle
    }
}

/// Nearest-rank quantile. `probability` is in (0, 1].
pub fn quantile_nearest_rank(values: &[f64], probability: f64) -> Option<f64> {
    if values.is_empty() || !(probability > 0.0 && probability <= 1.0) || values.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (probability * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    Some(sorted[index])
}

pub fn geometric_mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite() || *value <= 0.0) {
        return None;
    }
    let mean = values.iter().map(|value| value.ln()).sum::<f64>() / values.len() as f64;
    Some(mean.exp())
}

pub fn paired_ratios(frust: &[f64], stock: &[f64]) -> Option<Vec<f64>> {
    if frust.len() != stock.len() || frust.is_empty() {
        return None;
    }
    let mut ratios = Vec::with_capacity(frust.len());
    for (left, right) in frust.iter().zip(stock) {
        if !valid_seconds(*left, *right) {
            return None;
        }
        ratios.push(left / right);
    }
    Some(ratios)
}

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn index(&mut self, len: usize) -> usize {
        (self.next_u64() as usize) % len
    }
}

/// One-sided 95% upper bound of the stratified bootstrap of the geometric mean
/// of per-workload medians. Groups are resampled independently.
pub fn stratified_geomean_upper(groups: &[Vec<f64>], replicates: usize, seed: u64) -> Option<f64> {
    if groups.is_empty() || replicates == 0 || groups.iter().any(|group| {
        group.is_empty() || group.iter().any(|ratio| !ratio.is_finite() || *ratio <= 0.0)
    }) {
        return None;
    }
    let mut rng = Rng(seed);
    let mut estimates = Vec::with_capacity(replicates);
    let mut sample = Vec::with_capacity(groups.iter().map(Vec::len).max().unwrap());
    for _ in 0..replicates {
        let mut log_sum = 0.0;
        for group in groups {
            sample.clear();
            sample.extend((0..group.len()).map(|_| group[rng.index(group.len())]));
            log_sum += median_in_place(&mut sample).ln();
        }
        estimates.push((log_sum / groups.len() as f64).exp());
    }
    quantile_nearest_rank(&estimates, 0.95)
}

pub fn evaluate(edits: &[EditPair], resources: &[ResourcePair], runtime: &[RuntimeWorkload], sizes: &[SizePair]) -> GateReport {
    let mut gates = Vec::new();
    let leaf = group_ratios(edits, EditKind::Leaf);
    let (edit_geomean, edit_upper) = summarize_groups(&leaf);
    gates.push(judge_edit(&leaf, edit_geomean, edit_upper));

    let holdout = group_ratios(edits, EditKind::Holdout);
    let holdout_median = holdout_median_ratio(&holdout);
    gates.push(judge_holdout(holdout_median, &holdout));

    let cold = group_ratios(edits, EditKind::Cold);
    let cold_median = single_median_ratio(&cold);
    gates.push(judge_cold(cold_median, &cold));

    let (noop_overhead_secs, noop_allowance_secs) = noop_overhead(edits);
    gates.push(judge_noop(noop_overhead_secs, noop_allowance_secs, edits));

    let tail_p95 = tail_estimates(edits);
    gates.push(judge_tail(&tail_p95, edits));

    let memory_ratio = max_ratio(resources.iter().map(|pair| ratio_u64(pair.frust_peak_rss, pair.stock_peak_rss)));
    let disk_ratio = max_ratio(resources.iter().map(|pair| ratio_u64(pair.frust_bytes, pair.stock_bytes)));
    let resources_complete = BUILD_COHORT.iter().all(|name| {
        resources.iter().any(|pair| pair.workload == *name)
    }) && resources.iter().all(|pair| BUILD_COHORT.contains(&pair.workload.as_str()));
    gates.push(judge_ratio("peak-memory", memory_ratio, MEMORY_MAX, resources_complete));
    gates.push(judge_ratio("disk-bytes", disk_ratio, DISK_MAX, resources_complete));

    let (runtime_geomean, runtime_upper, runtime_rows) = summarize_runtime(runtime);
    gates.push(judge_runtime_workloads(&runtime_rows));
    let runtime_complete = covers_cohort(
        runtime.iter().map(|workload| workload.workload.as_str()),
        RUNTIME_COHORT,
    );
    gates.push(judge_runtime_aggregate(runtime_geomean, runtime_upper, runtime_complete));

    let size_geomean = size_summary(sizes);
    gates.push(judge_sizes(sizes, size_geomean));

    GateReport {
        gates,
        edit_geomean,
        edit_upper,
        cold_median,
        noop_overhead_secs,
        noop_allowance_secs,
        tail_p95,
        memory_ratio,
        disk_ratio,
        runtime_geomean,
        runtime_upper,
        size_geomean,
        holdout_median,
    }
}

fn valid_seconds(frust: f64, stock: f64) -> bool {
    frust.is_finite() && stock.is_finite() && frust > 0.0 && stock > 0.0
}

fn complete_groups(groups: &BTreeMap<String, Vec<f64>>, cohort: &[&str], minimum: usize) -> bool {
    groups.len() == cohort.len()
        && cohort.iter().all(|&name| groups.get(name).is_some_and(|group| group.len() >= minimum))
}

fn complete_counts(counts: &BTreeMap<String, usize>, cohort: &[&str], minimum: usize) -> bool {
    counts.len() == cohort.len()
        && cohort.iter().all(|&name| counts.get(name).is_some_and(|count| *count >= minimum))
}

fn covers_cohort<'a>(names: impl Iterator<Item = &'a str>, cohort: &[&str]) -> bool {
    let names: Vec<_> = names.collect();
    names.len() == cohort.len() && cohort.iter().all(|name| names.contains(name))
}

fn judge_edit(groups: &BTreeMap<String, Vec<f64>>, geomean: Option<f64>, upper: Option<f64>) -> GateRow {
    if !complete_groups(groups, PRIMARY, MIN_MEDIAN_PAIRS) {
        return fail("edited-release-build", "inconclusive: need all three primary workloads with 30 leaf pairs each");
    }
    match (geomean, upper) {
        (Some(mean), Some(bound)) if mean <= EDIT_GEOMEAN_MAX && bound <= EDIT_GEOMEAN_MAX => {
            pass("edited-release-build", &format!("geomean {mean:.4} upper {bound:.4}"))
        }
        (Some(mean), Some(bound)) => fail("edited-release-build", &format!("geomean {mean:.4} upper {bound:.4} exceeds 0.80")),
        _ => fail("edited-release-build", "inconclusive: non-finite ratio"),
    }
}

fn judge_holdout(median_ratio: Option<f64>, groups: &BTreeMap<String, Vec<f64>>) -> GateRow {
    if !complete_groups(groups, &["fd"], MIN_MEDIAN_PAIRS) {
        return fail("holdout-build", "inconclusive: holdout needs 30 pairs");
    }
    match median_ratio {
        Some(ratio) if ratio < 1.0 => pass("holdout-build", &format!("median ratio {ratio:.4}")),
        Some(ratio) => fail("holdout-build", &format!("median ratio {ratio:.4} is not an improvement")),
        None => fail("holdout-build", "inconclusive: non-finite holdout ratio"),
    }
}

fn judge_cold(median_ratio: Option<f64>, groups: &BTreeMap<String, Vec<f64>>) -> GateRow {
    if !complete_groups(groups, BUILD_COHORT, MIN_MEDIAN_PAIRS) {
        return fail("cold-build", "inconclusive: cold builds need 30 pairs per workload");
    }
    match median_ratio {
        Some(ratio) if ratio <= COLD_MEDIAN_MAX => pass("cold-build", &format!("median ratio {ratio:.4}")),
        Some(ratio) => fail("cold-build", &format!("median ratio {ratio:.4} exceeds 1.05")),
        None => fail("cold-build", "inconclusive: non-finite cold ratio"),
    }
}

fn judge_noop(overhead: Option<f64>, allowance: Option<f64>, edits: &[EditPair]) -> GateRow {
    let counts = kind_counts(edits, EditKind::Noop);
    if !complete_counts(&counts, BUILD_COHORT, MIN_MEDIAN_PAIRS) {
        return fail("noop-overhead", "inconclusive: no-op builds need 30 pairs per workload");
    }
    match (overhead, allowance) {
        (Some(overhead), Some(allowance)) if overhead <= allowance => {
            pass("noop-overhead", &format!("median overhead {overhead:.4}s allowance {allowance:.4}s"))
        }
        (Some(overhead), Some(allowance)) => {
            fail("noop-overhead", &format!("median overhead {overhead:.4}s exceeds allowance {allowance:.4}s"))
        }
        _ => fail("noop-overhead", "inconclusive: non-finite no-op sample"),
    }
}

fn judge_tail(tails: &[(String, f64)], edits: &[EditPair]) -> GateRow {
    let leaf = kind_counts(edits, EditKind::Leaf);
    let wide = kind_counts(edits, EditKind::Wide);
    if !complete_counts(&leaf, PRIMARY, MIN_TAIL_PAIRS)
        || !complete_counts(&wide, PRIMARY, MIN_TAIL_PAIRS) {
        return fail("tail-latency", "inconclusive: primary and wider edits need 60 pairs");
    }
    if tails.len() != PRIMARY.len() * 2 {
        return fail("tail-latency", "inconclusive: missing p95");
    }
    if let Some((name, p95)) = tails.iter().find(|(_, p95)| *p95 > TAIL_P95_MAX) {
        return fail("tail-latency", &format!("{name} p95 {p95:.4} exceeds 1.05"));
    }
    pass("tail-latency", "primary and wider edit p95 are within 1.05")
}

fn judge_ratio(name: &str, ratio: Option<f64>, limit: f64, complete: bool) -> GateRow {
    if !complete {
        return fail(name, "inconclusive: need resource samples for the full build cohort");
    }
    match ratio {
        Some(ratio) if ratio <= limit => pass(name, &format!("max ratio {ratio:.4}")),
        Some(ratio) => fail(name, &format!("max ratio {ratio:.4} exceeds {limit:.2}")),
        None => fail(name, "inconclusive: non-finite resource ratio"),
    }
}

fn judge_runtime_workloads(rows: &[(String, Option<f64>, bool)]) -> GateRow {
    if !covers_cohort(rows.iter().map(|(name, _, _)| name.as_str()), RUNTIME_COHORT) {
        return fail("runtime-workloads", "inconclusive: need three runnable workloads");
    }
    for (name, upper, identical) in rows {
        if *identical {
            continue;
        }
        match upper {
            Some(bound) if *bound <= RUNTIME_WORKLOAD_UPPER => {}
            Some(bound) => {
                return fail("runtime-workloads", &format!("{name} upper {bound:.4} exceeds 1.01"));
            }
            None => return fail("runtime-workloads", &format!("{name} upper bound is inconclusive")),
        }
    }
    pass("runtime-workloads", "per-workload runtime bounds hold")
}

fn judge_runtime_aggregate(geomean: Option<f64>, upper: Option<f64>, complete: bool) -> GateRow {
    if !complete {
        return fail("runtime-aggregate", "inconclusive: need all three runtime workloads");
    }
    match (geomean, upper) {
        (Some(mean), Some(bound)) if mean <= RUNTIME_AGGREGATE_MAX && bound <= RUNTIME_AGGREGATE_MAX => {
            pass("runtime-aggregate", &format!("geomean {mean:.4} upper {bound:.4}"))
        }
        (Some(mean), Some(bound)) => fail("runtime-aggregate", &format!("geomean {mean:.4} upper {bound:.4} exceeds 0.97")),
        _ => fail("runtime-aggregate", "inconclusive: non-finite runtime ratio"),
    }
}

fn judge_sizes(sizes: &[SizePair], geomean: Option<f64>) -> GateRow {
    if !covers_cohort(sizes.iter().map(|pair| pair.workload.as_str()), RUNTIME_COHORT) {
        return fail("stripped-size", "inconclusive: need three stripped sizes");
    }
    for pair in sizes {
        if pair.stock_stripped == 0 || pair.frust_stripped > pair.stock_stripped {
            return fail(
                "stripped-size",
                &format!("{} grew from {} to {} bytes", pair.workload, pair.stock_stripped, pair.frust_stripped),
            );
        }
    }
    match geomean {
        Some(mean) if mean <= SIZE_AGGREGATE_MAX => pass("stripped-size", &format!("geomean {mean:.4}")),
        Some(mean) => fail("stripped-size", &format!("geomean {mean:.4} exceeds 0.97")),
        None => fail("stripped-size", "inconclusive: non-finite size ratio"),
    }
}

fn pass(name: &str, detail: &str) -> GateRow {
    GateRow { name: name.to_string(), status: "passed", detail: detail.to_string() }
}

fn fail(name: &str, detail: &str) -> GateRow {
    GateRow { name: name.to_string(), status: "failed", detail: detail.to_string() }
}

fn group_ratios(edits: &[EditPair], kind: EditKind) -> BTreeMap<String, Vec<f64>> {
    let mut groups = BTreeMap::<String, Vec<f64>>::new();
    for pair in edits.iter().filter(|pair| pair.kind == kind) {
        if valid_seconds(pair.frust_secs, pair.stock_secs) {
            groups.entry(pair.workload.clone()).or_default().push(pair.frust_secs / pair.stock_secs);
        } else {
            groups.entry(pair.workload.clone()).or_default().push(f64::NAN);
        }
    }
    groups
}

fn summarize_groups(groups: &BTreeMap<String, Vec<f64>>) -> (Option<f64>, Option<f64>) {
    let lists: Vec<Vec<f64>> = groups.values().cloned().collect();
    let medians: Vec<f64> = lists.iter().filter_map(|group| median(group)).collect();
    if medians.len() != lists.len() {
        return (None, None);
    }
    let mean = geometric_mean(&medians);
    let upper = stratified_geomean_upper(&lists, BOOTSTRAP_REPLICATES, BOOTSTRAP_SEED);
    (mean, upper)
}

fn holdout_median_ratio(groups: &BTreeMap<String, Vec<f64>>) -> Option<f64> {
    let medians: Vec<f64> = groups.values().filter_map(|group| median(group)).collect();
    if medians.len() != groups.len() {
        None
    } else {
        median(&medians)
    }
}

fn single_median_ratio(groups: &BTreeMap<String, Vec<f64>>) -> Option<f64> {
    let medians: Vec<f64> = groups.values().filter_map(|group| median(group)).collect();
    if medians.len() != groups.len() || medians.is_empty() {
        None
    } else {
        median(&medians)
    }
}

fn kind_counts(edits: &[EditPair], kind: EditKind) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::<String, usize>::new();
    for pair in edits.iter().filter(|pair| pair.kind == kind) {
        *counts.entry(pair.workload.clone()).or_default() += 1;
    }
    counts
}

fn noop_overhead(edits: &[EditPair]) -> (Option<f64>, Option<f64>) {
    let mut by_workload: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    for pair in edits.iter().filter(|pair| pair.kind == EditKind::Noop) {
        if !valid_seconds(pair.frust_secs, pair.stock_secs) {
            return (None, None);
        }
        by_workload.entry(pair.workload.clone()).or_default().push((pair.frust_secs, pair.stock_secs));
    }
    if by_workload.is_empty() {
        return (None, None);
    }
    let mut overheads = Vec::new();
    let mut allowances = Vec::new();
    for samples in by_workload.values() {
        let diffs: Vec<f64> = samples.iter().map(|(frust, stock)| frust - stock).collect();
        let stocks: Vec<f64> = samples.iter().map(|(_, stock)| *stock).collect();
        let Some(overhead) = median(&diffs) else { return (None, None) };
        let Some(stock) = median(&stocks) else { return (None, None) };
        overheads.push(overhead);
        allowances.push((stock * NOOP_RATIO).max(NOOP_ABS_SECS));
    }
    (median(&overheads), median(&allowances))
}

fn tail_estimates(edits: &[EditPair]) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for kind in [EditKind::Leaf, EditKind::Wide] {
        let groups = group_ratios(edits, kind);
        for (name, ratios) in groups {
            if let Some(p95) = quantile_nearest_rank(&ratios, 0.95) {
                let label = match kind {
                    EditKind::Leaf => format!("{name}/leaf"),
                    EditKind::Wide => format!("{name}/wide"),
                    _ => name,
                };
                out.push((label, p95));
            }
        }
    }
    out
}

fn ratio_u64(frust: u64, stock: u64) -> Option<f64> {
    if stock == 0 || frust == 0 { None } else { Some(frust as f64 / stock as f64) }
}

fn max_ratio(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut max = None;
    for value in values {
        let value = value?;
        if !value.is_finite() {
            return None;
        }
        max = Some(max.map_or(value, |current: f64| current.max(value)));
    }
    max
}

fn summarize_runtime(runtime: &[RuntimeWorkload]) -> (Option<f64>, Option<f64>, Vec<(String, Option<f64>, bool)>) {
    let mut groups = Vec::new();
    let mut rows = Vec::new();
    for workload in runtime {
        let ratios = (workload.frust_secs.len() >= MIN_MEDIAN_PAIRS)
            .then(|| paired_ratios(&workload.frust_secs, &workload.stock_secs))
            .flatten();
        let upper = ratios.as_ref().and_then(|ratios| median_upper(ratios));
        if let Some(ratios) = ratios {
            groups.push(ratios);
        }
        rows.push((workload.workload.clone(), upper, workload.identical_code && upper.is_some()));
    }
    let medians: Vec<f64> = groups.iter().filter_map(|group| median(group)).collect();
    let geomean = if medians.len() == groups.len() { geometric_mean(&medians) } else { None };
    let upper = if groups.len() == runtime.len() {
        stratified_geomean_upper(&groups, BOOTSTRAP_REPLICATES, BOOTSTRAP_SEED)
    } else {
        None
    };
    (geomean, upper, rows)
}

fn median_upper(ratios: &[f64]) -> Option<f64> {
    stratified_geomean_upper(&[ratios.to_vec()], BOOTSTRAP_REPLICATES, BOOTSTRAP_SEED)
}

fn size_summary(sizes: &[SizePair]) -> Option<f64> {
    let ratios = sizes
        .iter()
        .map(|pair| if pair.stock_stripped == 0 { None } else { Some(pair.frust_stripped as f64 / pair.stock_stripped as f64) })
        .collect::<Option<Vec<_>>>()?;
    geometric_mean(&ratios)
}

pub fn report_json(report: &GateReport) -> String {
    let mut out = String::from("{\n");
    out.push_str("  \"gates\": [\n");
    for (index, gate) in report.gates.iter().enumerate() {
        let comma = if index + 1 == report.gates.len() { "" } else { "," };
        out.push_str(&format!(
            "    {{\"name\":{},\"status\":{},\"detail\":{}}}{comma}\n",
            json_string(&gate.name),
            json_string(gate.status),
            json_string(&gate.detail)
        ));
    }
    out.push_str("  ],\n");
    push_opt(&mut out, "edit_geomean", report.edit_geomean);
    push_opt(&mut out, "edit_upper", report.edit_upper);
    push_opt(&mut out, "cold_median", report.cold_median);
    push_opt(&mut out, "noop_overhead_secs", report.noop_overhead_secs);
    push_opt(&mut out, "noop_allowance_secs", report.noop_allowance_secs);
    push_opt(&mut out, "memory_ratio", report.memory_ratio);
    push_opt(&mut out, "disk_ratio", report.disk_ratio);
    push_opt(&mut out, "runtime_geomean", report.runtime_geomean);
    push_opt(&mut out, "runtime_upper", report.runtime_upper);
    push_opt(&mut out, "size_geomean", report.size_geomean);
    push_opt(&mut out, "holdout_median", report.holdout_median);
    out.push_str("  \"tail_p95\": [\n");
    for (index, (name, value)) in report.tail_p95.iter().enumerate() {
        let comma = if index + 1 == report.tail_p95.len() { "" } else { "," };
        out.push_str(&format!("    {{\"name\":{},\"p95\":{value:.8}}}{comma}\n", json_string(name)));
    }
    out.push_str("  ]\n}\n");
    out
}

fn push_opt(out: &mut String, name: &str, value: Option<f64>) {
    match value {
        Some(value) => out.push_str(&format!("  \"{name}\": {value:.8},\n")),
        None => out.push_str(&format!("  \"{name}\": null,\n")),
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_geomean_and_quantile_match_hand_values() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), Some(2.5));
        let mean = geometric_mean(&[0.5, 0.8]).unwrap();
        assert!((mean - (0.5_f64 * 0.8).sqrt()).abs() < 1e-12);
        assert_eq!(quantile_nearest_rank(&[1.0, 2.0, 3.0, 4.0], 0.5), Some(2.0));
        assert_eq!(quantile_nearest_rank(&[1.0, 2.0, 3.0, 4.0], 1.0), Some(4.0));
    }

    #[test]
    fn zero_variance_upper_bound_matches_the_point() {
        let groups = vec![vec![0.5; 20], vec![0.5; 20]];
        let upper = stratified_geomean_upper(&groups, 200, 7).unwrap();
        assert!((upper - 0.5).abs() < 1e-12);
        let point = geometric_mean(&[median(&groups[0]).unwrap(), median(&groups[1]).unwrap()]).unwrap();
        assert!((point - upper).abs() < 1e-12);
    }

    #[test]
    fn spread_sample_upper_bound_is_at_least_the_point() {
        let groups = vec![vec![0.4, 0.6, 0.8, 1.0], vec![0.4, 0.6, 0.8, 1.0]];
        let point = geometric_mean(&[median(&groups[0]).unwrap(), median(&groups[1]).unwrap()]).unwrap();
        let upper = stratified_geomean_upper(&groups, 400, 11).unwrap();
        assert!(upper + 1e-9 >= point);
    }

    fn pairs(workload: &str, kind: EditKind, frust: f64, stock: f64, count: usize) -> Vec<EditPair> {
        (0..count)
            .map(|_| EditPair { workload: workload.into(), kind, frust_secs: frust, stock_secs: stock })
            .collect()
    }

    #[test]
    fn constant_improvement_passes_and_a_regression_fails() {
        let mut edits = Vec::new();
        for &name in PRIMARY {
            edits.extend(pairs(name, EditKind::Leaf, 0.70, 1.0, 60));
            edits.extend(pairs(name, EditKind::Wide, 0.90, 1.0, 60));
            edits.extend(pairs(name, EditKind::Cold, 1.0, 1.0, 30));
            edits.extend(pairs(name, EditKind::Noop, 0.20, 0.20, 30));
        }
        edits.extend(pairs("fd", EditKind::Cold, 1.0, 1.0, 30));
        edits.extend(pairs("fd", EditKind::Noop, 0.2, 0.2, 30));
        edits.extend(pairs("fd", EditKind::Holdout, 0.75, 1.0, 30));
        let resources: Vec<_> = BUILD_COHORT.iter().map(|name| ResourcePair {
            workload: (*name).into(),
            frust_peak_rss: 100,
            stock_peak_rss: 100,
            frust_bytes: 100,
            stock_bytes: 100,
        }).collect();
        let runtime = ["ripgrep", "rustfmt", "oxipng"].map(|name| RuntimeWorkload {
            workload: name.into(),
            frust_secs: vec![0.90; 30],
            stock_secs: vec![1.0; 30],
            identical_code: false,
        });
        let sizes = ["ripgrep", "rustfmt", "oxipng"].map(|name| SizePair {
            workload: name.into(),
            frust_stripped: 90,
            stock_stripped: 100,
        });
        let report = evaluate(&edits, &resources, &runtime, &sizes);
        assert!(report.gates.iter().all(|gate| gate.status == "passed"), "{:?}", report.gates);
        assert!(report.edit_geomean.unwrap() <= 0.80);
        assert!(report.edit_upper.unwrap() <= 0.80);

        edits.extend(pairs("slow", EditKind::Leaf, 1.2, 1.0, 60));
        let failed = evaluate(&edits, &resources, &runtime, &sizes);
        let edit = failed.gates.iter().find(|gate| gate.name == "edited-release-build").unwrap();
        assert_eq!(edit.status, "failed");
    }

    #[test]
    fn incomplete_and_invalid_samples_fail_the_relevant_gates() {
        let mut edits = Vec::new();
        for &name in PRIMARY {
            edits.extend(pairs(name, EditKind::Leaf, 0.7, 1.0, 60));
            edits.extend(pairs(name, EditKind::Wide, 0.9, 1.0, 60));
        }
        edits.last_mut().unwrap().frust_secs = f64::NAN;
        let runtime = ["ripgrep", "ripgrep", "oxipng"].map(|workload| RuntimeWorkload {
            workload: workload.into(),
            frust_secs: vec![0.9; 30],
            stock_secs: vec![1.0; 30],
            identical_code: false,
        });
        let report = evaluate(&edits, &[], &runtime, &[]);
        for name in ["tail-latency", "cold-build", "runtime-workloads", "runtime-aggregate"] {
            assert!(report.gates.iter().any(|gate| gate.name == name && gate.status == "failed"));
        }
    }

    #[test]
    fn identical_code_skips_the_one_percent_runtime_bound_only() {
        let runtime = ["ripgrep", "rustfmt", "oxipng"].map(|name| RuntimeWorkload {
            workload: name.into(),
            frust_secs: vec![1.0; 30],
            stock_secs: vec![1.0; 30],
            identical_code: true,
        });
        let report = evaluate(&[], &[], &runtime, &[]);
        let workloads = report.gates.iter().find(|gate| gate.name == "runtime-workloads").unwrap();
        assert_eq!(workloads.status, "passed");
        let aggregate = report.gates.iter().find(|gate| gate.name == "runtime-aggregate").unwrap();
        assert_eq!(aggregate.status, "failed");
    }
}
