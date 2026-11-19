//! Recompute the frozen Linux release budgets from raw paired samples.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use frust_core::{EditKind, EditPair, ResourcePair, RuntimeWorkload, SizePair, evaluate, report_json};

fn main() -> ExitCode {
    let mut samples = None;
    let mut output = None;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--samples" => samples = args.next().map(PathBuf::from),
            "--output" => output = args.next().map(PathBuf::from),
            other => {
                eprintln!("unknown argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    let (Some(samples), Some(output)) = (samples, output) else {
        eprintln!("usage: frust-gates --samples FILE --output FILE");
        return ExitCode::from(2);
    };
    let text = match fs::read_to_string(&samples) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("could not read {}: {err}", samples.display());
            return ExitCode::from(1);
        }
    };
    let parsed = match parse_samples(&text) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(1);
        }
    };
    let report = evaluate(&parsed.0, &parsed.1, &parsed.2, &parsed.3);
    let json = report_json(&report);
    if let Err(err) = fs::write(&output, &json) {
        eprintln!("could not write {}: {err}", output.display());
        return ExitCode::from(1);
    }
    print!("{json}");
    if report.gates.iter().any(|gate| gate.status != "passed") {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn parse_samples(text: &str) -> Result<(Vec<EditPair>, Vec<ResourcePair>, Vec<RuntimeWorkload>, Vec<SizePair>), String> {
    let mut edits = Vec::new();
    let mut resources = Vec::new();
    let mut runtime: Vec<RuntimeWorkload> = Vec::new();
    let mut sizes = Vec::new();
    for (line_number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let kind = parts.next().ok_or("empty record")?;
        match kind {
            "edit" => {
                let workload = parts.next().ok_or(format!("line {}: workload", line_number + 1))?.to_string();
                let edit_kind = parse_kind(parts.next().ok_or("edit kind")?)?;
                let frust_secs = parse_f64(parts.next().ok_or("frust")?)?;
                let stock_secs = parse_f64(parts.next().ok_or("stock")?)?;
                edits.push(EditPair { workload, kind: edit_kind, frust_secs, stock_secs });
            }
            "resource" => {
                let workload = req(&mut parts, "workload")?;
                resources.push(ResourcePair {
                    workload,
                    frust_peak_rss: parse_u64(parts.next().ok_or("rss")?)?,
                    stock_peak_rss: parse_u64(parts.next().ok_or("rss")?)?,
                    frust_bytes: parse_u64(parts.next().ok_or("bytes")?)?,
                    stock_bytes: parse_u64(parts.next().ok_or("bytes")?)?,
                });
            }
            "runtime" => {
                let workload = req(&mut parts, "workload")?;
                let identical_code = parts.next().ok_or("identical")? == "1";
                let frust_secs = parse_list(parts.next().ok_or("frust")?)?;
                let stock_secs = parse_list(parts.next().ok_or("stock")?)?;
                runtime.push(RuntimeWorkload { workload, frust_secs, stock_secs, identical_code });
            }
            "size" => {
                sizes.push(SizePair {
                    workload: req(&mut parts, "workload")?,
                    frust_stripped: parse_u64(parts.next().ok_or("frust")?)?,
                    stock_stripped: parse_u64(parts.next().ok_or("stock")?)?,
                });
            }
            other => return Err(format!("unknown record {other}")),
        }
    }
    Ok((edits, resources, runtime, sizes))
}

fn req(parts: &mut std::str::SplitWhitespace<'_>, name: &str) -> Result<String, String> {
    parts.next().map(str::to_string).ok_or_else(|| format!("missing {name}"))
}

fn parse_kind(text: &str) -> Result<EditKind, String> {
    match text {
        "leaf" => Ok(EditKind::Leaf),
        "wide" => Ok(EditKind::Wide),
        "cold" => Ok(EditKind::Cold),
        "noop" => Ok(EditKind::Noop),
        "holdout" => Ok(EditKind::Holdout),
        other => Err(format!("unknown edit kind {other}")),
    }
}

fn parse_f64(text: &str) -> Result<f64, String> {
    text.parse::<f64>().map_err(|_| format!("invalid number {text}"))
}

fn parse_u64(text: &str) -> Result<u64, String> {
    text.parse::<u64>().map_err(|_| format!("invalid integer {text}"))
}

fn parse_list(text: &str) -> Result<Vec<f64>, String> {
    text.split(',').map(parse_f64).collect()
}
