//! Build-size estimates fall back from specific to general keys and skip unchanged writes to avoid repeated temp-file rewrites.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::lock;
use crate::paths::Dirs;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    units: u32,
}

fn path(dirs: &Dirs, key: &str) -> PathBuf {
    dirs.history().join(format!("{key}.json"))
}

fn read_units(dirs: &Dirs, key: &str) -> Option<u32> {
    let record: Record = serde_json::from_slice(&fs::read(path(dirs, key)).ok()?).ok()?;
    (record.units > 0).then_some(record.units)
}

pub fn expected_units(dirs: &Dirs, keys: &[String]) -> Option<u32> {
    keys.iter().find_map(|key| read_units(dirs, key))
}

pub fn remember_units(dirs: &Dirs, keys: &[String], units: u32) {
    for key in keys {
        remember_one(dirs, key, units);
    }
}

fn remember_one(dirs: &Dirs, key: &str, units: u32) {
    if units == 0 || read_units(dirs, key) == Some(units) {
        return;
    }
    if let Ok(json) = serde_json::to_vec(&Record { units }) {
        let _ = lock::write_shared(&path(dirs, key), &json, lock::shared_mode(dirs));
    }
}
