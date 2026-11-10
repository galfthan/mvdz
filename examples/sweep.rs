//! Round-trip sweep: every file at both levels, whole-file and segmented,
//! one-shot and streamed; decoded by the whole-file and the sequential
//! reader and compared by SHA-256. Usage: sweep [-s size] files...
//! (`size` takes K, M, G suffixes, binary, as the command-line tool does).
use sha2::{Digest, Sha256};
use std::io::Read;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut seg = 1usize << 20;
    if args.first().is_some_and(|a| a == "-s") {
        seg = size(&args[1]);
        args.drain(..2);
    }
    let configs: Vec<(mvdz::Level, usize, bool)> = [mvdz::Level::Normal, mvdz::Level::Fast]
        .into_iter()
        .flat_map(|l| [(l, 0, false), (l, seg, false), (l, 0, true), (l, seg, true)])
        .collect();
    let res = mvdz::util::par_map(4, args.len(), |i| {
        let src = std::fs::read(&args[i]).unwrap();
        let want = Sha256::digest(&src);
        let mut fails = Vec::new();
        let mut fb = 0;
        for &(level, s, streamed) in &configs {
            let cfg = mvdz::Config {
                level,
                segment: s,
                ..Default::default()
            };
            let c = if streamed {
                let mut w = mvdz::Writer::new(Vec::new(), &cfg).unwrap();
                for ch in src.chunks(65_536) {
                    w.write_demo(ch).unwrap();
                }
                w.finish().unwrap()
            } else {
                let (c, info) = mvdz::compress_info(&src, &cfg).unwrap();
                fb += info.fallback_segments;
                c
            };
            let ok1 = mvdz::decompress(&c, 1)
                .map(|d| Sha256::digest(&d) == want)
                .unwrap_or(false);
            let mut d = Vec::new();
            let ok2 = mvdz::Reader::new(std::io::Cursor::new(&c), &Default::default())
                .map(|mut r| r.read_to_end(&mut d).is_ok() && Sha256::digest(&d) == want)
                .unwrap_or(false);
            if !(ok1 && ok2) {
                fails.push(format!("{level:?} seg {s} streamed {streamed}"));
            }
        }
        (args[i].clone(), src.len(), fails, fb)
    });
    let (mut n, mut raw, mut bad, mut fbs) = (0, 0usize, 0, 0);
    for (f, len, fails, fb) in res {
        n += 1;
        raw += len;
        fbs += fb;
        if !fails.is_empty() {
            bad += 1;
            println!("FAIL {f}: {fails:?}");
        }
    }
    println!(
        "files {n}  raw {:.1} MB  configs {}  round trips {}  failed files {bad}  fallback segments {fbs}",
        raw as f64 / 1e6,
        configs.len(),
        n * configs.len() * 2
    );
}

/// A size with an optional binary K, M or G suffix (as `mvdz -s`).
fn size(s: &str) -> usize {
    let (num, shift) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 10),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 20),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    let n: usize = num.parse().unwrap_or_else(|_| panic!("bad size {s}"));
    n.checked_mul(1 << shift)
        .unwrap_or_else(|| panic!("size {s} too large"))
}
