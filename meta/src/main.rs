//! mvdz-meta: what QuakeWorld demos contain, as text or JSON lines.

use std::io::Write;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "usage: mvdz-meta [--json] files-or-dirs...
Reads .mvdz, .mvd and .mvd.gz demos (a .mvdz is decoded, a .mvd.gz
gunzipped) and shows what each contains: map, server info, wall clock,
players at the start and every later player change. --json writes one JSON
object per demo and line, with the raw strings (as Latin-1) and a census of
the demo's messages. A directory means the demos directly in it. The output
is informative and may change between versions."
    );
    std::process::exit(2)
}

fn is_demo(p: &std::path::Path) -> bool {
    let n = p.to_string_lossy();
    n.ends_with(".mvdz") || n.ends_with(".mvd") || n.ends_with(".mvd.gz")
}

fn main() {
    let mut as_json = false;
    let mut files: Vec<PathBuf> = Vec::new();
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--json" => as_json = true,
            "-h" | "--help" => usage(),
            _ if a.starts_with('-') => usage(),
            _ => {
                let p = PathBuf::from(&a);
                if p.is_dir() {
                    let mut v: Vec<PathBuf> = std::fs::read_dir(&p)
                        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
                        .unwrap_or_default();
                    v.retain(|p| p.is_file() && is_demo(p));
                    v.sort();
                    files.extend(v);
                } else {
                    files.push(p);
                }
            }
        }
    }
    if files.is_empty() {
        usage();
    }
    let mut out = std::io::stdout().lock();
    let mut failed = false;
    for (i, p) in files.iter().enumerate() {
        let name = p.to_string_lossy();
        let b = match mvdz_meta::load(p) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{name}: {e}");
                failed = true;
                continue;
            }
        };
        if !mvdz::ident::starts_like_mvd(&b) {
            eprintln!("{name}: not an MVD demo");
            failed = true;
            continue;
        }
        let s = mvdz_meta::scan::scan(&b);
        let r = if as_json {
            writeln!(out, "{}", mvdz_meta::report::json(&name, &s))
        } else {
            let sep = if i > 0 { "\n" } else { "" };
            write!(out, "{sep}{}", mvdz_meta::report::text(&name, &s))
        };
        if r.is_err() {
            // a closed pipe (| head): stop quietly
            std::process::exit(if failed { 1 } else { 0 });
        }
    }
    std::process::exit(failed as i32)
}
