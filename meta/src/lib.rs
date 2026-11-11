//! mvdz-meta: what a QuakeWorld MVD demo contains (map, server info,
//! players over time, wall clock, a census of its messages), read from the
//! demo itself. A `.mvdz` is decoded first; `.mvd.gz` is gunzipped.
//!
//! None of this is stored in the `.mvdz` format, and the output is
//! informative: it may change between versions.

pub mod report;
pub mod scan;

use std::path::Path;

/// Reads a demo file as raw `.mvd` bytes: `.mvdz` decoded (by content,
/// whatever its name), gzip gunzipped, anything else as is.
pub fn load(path: &Path) -> Result<Vec<u8>, String> {
    let b = std::fs::read(path).map_err(|e| e.to_string())?;
    if mvdz::is_mvdz(&b) {
        return mvdz::decompress(&b, 1).map_err(|e| e.to_string());
    }
    if mvdz::is_gzip(&b) {
        return mvdz::gunzip(&b, mvdz::Limits::default().memory).map_err(|e| e.to_string());
    }
    Ok(b)
}
