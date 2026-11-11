#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use mvdz::api::{self, Config};
use mvdz::{Level, Limits, ReadOptions};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::time::Instant;

/// `println!` that exits quietly when standard output is closed (a pipe
/// into `head`, say) instead of panicking.
macro_rules! out {
    ($($t:tt)*) => {
        writeln!(std::io::stdout().lock(), $($t)*).unwrap_or_else(|e| stdout_failed(e))
    };
}

fn usage() -> ! {
    eprintln!(
        "usage:
  mvdz compress   [--fast] [-s size] [-j threads] [--ext key=value]... [--allow-non-demo]
                  [-f] [--rm] files... | in out.mvdz | -o out.mvdz in
                  game.mvd or game.mvd.gz -> game.mvdz beside it
  mvdz decompress [-j threads] [--max-memory size] [--trusted] [--gzip | --no-gzip]
                  [-f] [--rm] files... | in.mvdz out | -o out in.mvdz
                  game.mvdz -> game.mvd (game.mvd.gz with --gzip) beside it
  mvdz info       file.mvdz...                     identity, timing and index as JSON
                  (what a demo contains: the separate mvdz-meta tool)
  mvdz bench      [--levels normal,fast] [-s size] [-r reps] [-j threads] [--no-verify]
                  [--skip-compress] [--trusted] [--files out.tsv] [--streams out.tsv] files...
'-' as a file name is standard input or output. compress and decompress name
each output after its input, keep the inputs (--rm deletes each one after its
output is written), refuse to overwrite an existing file (-f overwrites), and
with several files work on -j of them at once; two names where the second is
an output (compress: *.mvdz or -; decompress: not *.mvdz) are input and
output, as is -o. compress takes gzip input (.mvd.gz), decompressed first;
input that is not an MVD demo is refused (--allow-non-demo stores it anyway).
decompress --gzip writes the demo gzip-compressed (the default for an output
named *.gz; --no-gzip writes it raw). Sizes take K, M, G suffixes
(binary: 1M = 1048576). --fast selects per-stream zstd: faster to compress
and decode, larger (default: normal).
-s cuts the demo into independently decodable segments (default: 256K;
0: one segment for the whole demo, which cannot seek).
--trusted skips the SHA-256 identity check (CRC-32C checks only): only for
files whose SHA-256 was verified before."
    );
    std::process::exit(2)
}

struct Args {
    levels: Vec<Level>,
    seg: usize,
    threads: usize,
    reps: usize,
    max_memory: Option<u64>,
    streams: Option<String>,
    perfile: Option<String>,
    files: Vec<String>,
    no_verify: bool,
    ext: Vec<(String, Vec<u8>)>,
    skip_compress: bool,
    trusted: bool,
    gzip: Option<bool>,
    any_input: bool,
    output: Option<String>,
    overwrite: bool,
    rm: bool,
}

fn size(s: &str) -> u64 {
    let (num, mul) = match s.as_bytes().last() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1u64 << 10),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mul))
        .unwrap_or_else(|| usage())
}

fn level_name(l: Level) -> &'static str {
    match l {
        Level::Normal => "normal",
        Level::Fast => "fast",
    }
}

fn parse(args: &[String]) -> Args {
    let mut a = Args {
        levels: vec![Level::Normal],
        seg: mvdz::DEFAULT_SEGMENT,
        threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        reps: 3,
        max_memory: None,
        streams: None,
        perfile: None,
        files: Vec::new(),
        no_verify: false,
        ext: Vec::new(),
        skip_compress: false,
        trusted: false,
        gzip: None,
        any_input: false,
        output: None,
        overwrite: false,
        rm: false,
    };
    let mut i = 0;
    while i < args.len() {
        let s = &args[i];
        let mut val = || {
            i += 1;
            args.get(i).cloned().unwrap_or_else(|| usage())
        };
        match s.as_str() {
            "--fast" => a.levels = vec![Level::Fast],
            "--levels" => {
                a.levels = val()
                    .split(',')
                    .map(|l| match l {
                        "normal" => Level::Normal,
                        "fast" => Level::Fast,
                        _ => usage(),
                    })
                    .collect()
            }
            "-s" | "--segment" => a.seg = usize::try_from(size(&val())).unwrap_or_else(|_| usage()),
            "-j" | "--threads" => a.threads = val().parse().unwrap_or_else(|_| usage()),
            "-r" | "--reps" => a.reps = val().parse().unwrap_or_else(|_| usage()),
            "--max-memory" => a.max_memory = Some(size(&val())),
            "--streams" => a.streams = Some(val()),
            "--files" => a.perfile = Some(val()),
            "--no-verify" => a.no_verify = true,
            "--skip-compress" => a.skip_compress = true,
            "--trusted" => a.trusted = true,
            "--gzip" => a.gzip = Some(true),
            "--no-gzip" => a.gzip = Some(false),
            "--allow-non-demo" => a.any_input = true,
            "-o" | "--output" => a.output = Some(val()),
            "-f" | "--force" => a.overwrite = true,
            "--rm" => a.rm = true,
            "--ext" => {
                let v = val();
                let (k, x) = v.split_once('=').unwrap_or_else(|| usage());
                a.ext.push((k.into(), x.as_bytes().to_vec()));
            }
            "-" => a.files.push(s.clone()),
            _ if s.starts_with('-') => usage(),
            _ => a.files.push(s.clone()),
        }
        i += 1;
    }
    a
}

/// A failed write to standard output: a closed pipe ends the program
/// quietly (the reader wanted no more), anything else is an error.
fn stdout_failed(e: std::io::Error) -> ! {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(0)
    }
    die(e)
}

fn die(e: impl std::fmt::Display) -> ! {
    eprintln!("error: {e}");
    std::process::exit(1)
}

/// Reads a whole input file (or standard input), once.
fn try_read_input(p: &str) -> Result<Vec<u8>, String> {
    if p == "-" {
        let mut b = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut b)
            .map_err(|e| e.to_string())?;
        Ok(b)
    } else {
        std::fs::read(p).map_err(|e| e.to_string())
    }
}

fn read_input(p: &str) -> Vec<u8> {
    try_read_input(p).unwrap_or_else(|e| die(format!("{p}: {e}")))
}

/// Reads a raw demo: gzip input (`.mvd.gz`) is decompressed.
fn read_demo(p: &str) -> Vec<u8> {
    let b = read_input(p);
    if !mvdz::is_gzip(&b) {
        return b;
    }
    mvdz::gunzip(&b, Limits::default().memory).unwrap_or_else(|e| die(format!("{p}: {e}")))
}

/// Writes `data`, gzip-compressed if `gzip`, to `w`.
fn put(w: &mut impl Write, data: &[u8], gzip: bool) -> std::io::Result<()> {
    if gzip {
        mvdz::write_gzip(data, &mut *w)?;
    } else {
        w.write_all(data)?;
    }
    w.flush()
}

/// Writes an output (gzip-compressed if `gzip`): standard output, a
/// device or pipe directly, a regular file atomically (temporary file and
/// rename).
fn try_write_output(p: &str, data: &[u8], gzip: bool) -> Result<(), String> {
    if p == "-" {
        put(&mut std::io::stdout().lock(), data, gzip).unwrap_or_else(|e| stdout_failed(e));
        return Ok(());
    }
    let path = std::path::Path::new(p);
    match std::fs::metadata(path) {
        Ok(m) if !m.is_file() => {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|e| e.to_string())?;
            put(&mut f, data, gzip).map_err(|e| e.to_string())
        }
        _ => api::write_file_atomic_with(path, |f| put(f, data, gzip)).map_err(|e| e.to_string()),
    }
}

/// Whether `name` ends with the ASCII suffix `suf`, ignoring case.
fn has_suffix(name: &str, suf: &str) -> bool {
    name.len() >= suf.len()
        && name.as_bytes()[name.len() - suf.len()..].eq_ignore_ascii_case(suf.as_bytes())
}

/// The output named after input `i`: game.mvd / game.mvd.gz -> game.mvdz;
/// game.mvdz -> game.mvd (game.mvd.gz if `gzip`).
fn output_name(i: &str, compress: bool, gzip: bool) -> Result<String, String> {
    if i == "-" {
        return Ok("-".into());
    }
    if compress {
        if has_suffix(i, ".mvdz") {
            return Err("already .mvdz".into());
        }
        let stem = if has_suffix(i, ".mvd.gz") {
            &i[..i.len() - 7]
        } else if has_suffix(i, ".mvd") {
            &i[..i.len() - 4]
        } else {
            i
        };
        Ok(format!("{stem}.mvdz"))
    } else if has_suffix(i, ".mvdz") {
        Ok(format!(
            "{}.mvd{}",
            &i[..i.len() - 5],
            if gzip { ".gz" } else { "" }
        ))
    } else {
        Err("not named *.mvdz (-o names the output)".into())
    }
}

/// The (input, output) pairs of a compress or decompress command line.
fn jobs(a: &Args, compress: bool) -> Vec<(String, Result<String, String>)> {
    let f = &a.files;
    if let Some(o) = &a.output {
        if f.len() != 1 {
            usage();
        }
        return vec![(f[0].clone(), Ok(o.clone()))];
    }
    if f.is_empty() {
        usage();
    }
    // the two-name form `in out`: the second names an output
    let second_is_output = |o: &str| o == "-" || has_suffix(o, ".mvdz") == compress;
    if f.len() == 2 && second_is_output(&f[1]) {
        return vec![(f[0].clone(), Ok(f[1].clone()))];
    }
    if f.len() > 1 && f.iter().any(|x| x == "-") {
        usage();
    }
    let mut seen = std::collections::HashMap::new();
    f.iter()
        .map(|i| {
            let o = output_name(i, compress, a.gzip == Some(true)).and_then(|o| {
                match seen.insert(o.clone(), i.clone()) {
                    Some(prev) => Err(format!("{o} is also the output of {prev}")),
                    None => Ok(o),
                }
            });
            (i.clone(), o)
        })
        .collect()
}

/// Runs every job of a compress or decompress command line: several files
/// on -j threads, one file at a time each. Errors are reported per file;
/// the exit status is 1 if any file failed.
fn run_jobs(a: &Args, compress: bool) {
    let jobs = jobs(a, compress);
    let (workers, threads) = if jobs.len() > 1 {
        (a.threads, 1)
    } else {
        (1, a.threads)
    };
    let failed = mvdz::util::par_map(workers, jobs.len(), |k| {
        let (i, o) = &jobs[k];
        let r = o.clone().and_then(|o| {
            if o != "-" && !a.overwrite && std::fs::metadata(&o).is_ok_and(|m| m.is_file()) {
                return Err(format!("{o} exists (-f overwrites)"));
            }
            if compress {
                compress_one(a, i, &o, threads)
            } else {
                decompress_one(a, i, &o, threads)
            }?;
            if a.rm && i != "-" && o != "-" {
                std::fs::remove_file(i).map_err(|e| format!("--rm: {e}"))?;
            }
            Ok(())
        });
        if let Err(e) = &r {
            eprintln!("error: {i}: {e}");
        }
        r.is_err()
    });
    if failed.contains(&true) {
        std::process::exit(1);
    }
}

fn compress_one(a: &Args, i: &str, o: &str, threads: usize) -> Result<(), String> {
    // gzip input is decompressed and non-demos refused by the library
    let src = try_read_input(i)?;
    let cfg = Config {
        level: a.levels[0],
        segment: a.seg,
        threads,
        verify: !a.no_verify,
        ext: a.ext.clone(),
    };
    let r = if a.any_input {
        // store whatever it is (it still round-trips)
        let raw = if mvdz::is_gzip(&src) {
            mvdz::gunzip(&src, Limits::default().memory)
        } else {
            Ok(src)
        };
        raw.and_then(|r| mvdz::encode::compress(&r, &cfg))
    } else {
        api::compress_info(&src, &cfg)
    };
    let (out, info) = r.map_err(|e| match e {
        mvdz::Error::NotMvd(s) if !s.starts_with("corrupt gzip") => {
            format!("{e} (--allow-non-demo compresses it anyway, mostly stored)")
        }
        _ => e.to_string(),
    })?;
    if info.fallback_segments > 0 {
        eprintln!(
            "note: {i}: {} of {} segments stored without the transform",
            info.fallback_segments, info.segments
        );
    }
    try_write_output(o, &out, false).map_err(|e| format!("{o}: {e}"))
}

fn decompress_one(a: &Args, i: &str, o: &str, threads: usize) -> Result<(), String> {
    let src = try_read_input(i)?;
    let out = api::decompress_with(&src, threads, &read_options(a)).map_err(|e| e.to_string())?;
    drop(src);
    // gzip output when asked, or by default for an output named .gz
    let gzip = a.gzip.unwrap_or(has_suffix(o, ".gz"));
    try_write_output(o, &out, gzip).map_err(|e| format!("{o}: {e}"))
}

fn read_options(a: &Args) -> ReadOptions {
    let mut l = Limits::default();
    if let Some(m) = a.max_memory {
        l.memory = m;
    }
    ReadOptions {
        limits: l,
        trusted: a.trusted,
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        usage();
    }
    #[cfg(all(feature = "sampler", target_os = "linux", target_arch = "x86_64"))]
    let _sampler = std::env::var("MVDZ_SAMPLE")
        .ok()
        .map(mvdz::sampler::Guard::start);
    let a = parse(&argv[1..]);
    match argv[0].as_str() {
        "compress" | "c" => run_jobs(&a, true),
        "decompress" | "d" => run_jobs(&a, false),
        "bench" => bench(&a),
        "info" => info(&a),
        _ => usage(),
    }
}

/// (median, min, max)
fn stats(v: &mut [f64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    let med = if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    };
    (med, v[0], v[n - 1])
}

fn fmt(s: (f64, f64, f64)) -> String {
    format!("{:.2} [{:.2}-{:.2}]", s.0, s.1, s.2)
}

fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// In-memory library benchmark: every figure is MB/s of raw demo
/// (MB = 10^6 bytes), the median of `reps` runs with [min-max].
fn bench(a: &Args) {
    let files: Vec<(String, Vec<u8>)> = a.files.iter().map(|f| (f.clone(), read_demo(f))).collect();
    let raw: usize = files.iter().map(|f| f.1.len()).sum();
    let mb = raw as f64 / 1e6;
    let nt = a.threads.max(1);
    let reps = a.reps.max(3);
    let lim = read_options(a);
    let dec = |c: &[u8], t: usize| api::decompress_with(c, t, &lim);
    out!(
        "files {}  raw {:.2} MB (10^6 bytes)  reps {}  segment {}  threads {}  verify {}  trusted {}  loadavg {}",
        files.len(),
        mb,
        reps,
        a.seg,
        nt,
        !a.no_verify,
        a.trusted,
        loadavg()
    );
    out!(
        "MB/s of raw demo: median [min-max] over {reps} runs; 'batch' = files in parallel on {nt} threads, 1 thread each"
    );
    let mut stream_rows: BTreeMap<(String, String), (usize, usize)> = BTreeMap::new();
    let mut perfile =
        String::from("level\tfile\traw\tpacked\tsha_ok\tfallback_segments\tescaped\n");
    for &level in &a.levels {
        let ln = level_name(level);
        let cfg = |threads: usize| Config {
            level,
            segment: a.seg,
            threads,
            verify: !a.no_verify,
            ext: Vec::new(),
        };
        let c1 = cfg(1);
        let comp: Vec<(Vec<u8>, api::Info)> = mvdz::util::par_map(nt, files.len(), |i| {
            api::compress_info(&files[i].1, &c1).unwrap_or_else(|e| die(e))
        });
        let bytes: usize = comp.iter().map(|c| c.0.len()).sum();
        let fb: usize = comp.iter().map(|c| c.1.fallback_segments).sum();
        let oks: Vec<bool> =
            mvdz::util::par_map(nt, files.len(), |i| match api::decompress(&comp[i].0, 1) {
                Ok(back) => Sha256::digest(&back) == Sha256::digest(&files[i].1),
                Err(_) => false,
            });
        let ok = oks.iter().filter(|&&x| x).count();
        for (i, f) in files.iter().enumerate() {
            let base = std::path::Path::new(&f.0)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            perfile += &format!(
                "{ln}\t{base}\t{}\t{}\t{}\t{}\t{}\n",
                f.1.len(),
                comp[i].0.len(),
                oks[i],
                comp[i].1.fallback_segments,
                comp[i].1.escaped
            );
            if !oks[i] {
                eprintln!("ROUND TRIP FAILED: level {ln} file {}", f.0);
            }
        }
        for (_, info) in &comp {
            for s in &info.streams {
                let e = stream_rows
                    .entry((ln.to_string(), mvdz::streams::name(s.id)))
                    .or_default();
                e.0 += s.count;
                e.1 += s.packed;
            }
        }
        let (mut tc1, mut td1, mut tcn, mut tdn, mut tcb, mut tdb) =
            (vec![], vec![], vec![], vec![], vec![], vec![]);
        let cn = cfg(nt);
        for _ in 0..reps {
            if !a.skip_compress {
                let t = Instant::now();
                for f in &files {
                    std::hint::black_box(api::compress(&f.1, &c1).ok());
                }
                tc1.push(mb / t.elapsed().as_secs_f64());
            }
            let t = Instant::now();
            for c in &comp {
                std::hint::black_box(dec(&c.0, 1).ok());
            }
            td1.push(mb / t.elapsed().as_secs_f64());
            if nt > 1 && !a.skip_compress {
                let t = Instant::now();
                for f in &files {
                    std::hint::black_box(api::compress(&f.1, &cn).ok());
                }
                tcn.push(mb / t.elapsed().as_secs_f64());
                let t = Instant::now();
                mvdz::util::par_map(nt, files.len(), |i| {
                    std::hint::black_box(api::compress(&files[i].1, &c1).map(|v| v.len()).ok())
                });
                tcb.push(mb / t.elapsed().as_secs_f64());
            }
            if nt > 1 {
                let t = Instant::now();
                for c in &comp {
                    std::hint::black_box(dec(&c.0, nt).ok());
                }
                tdn.push(mb / t.elapsed().as_secs_f64());
                let t = Instant::now();
                mvdz::util::par_map(nt, comp.len(), |i| {
                    std::hint::black_box(dec(&comp[i].0, 1).map(|v| v.len()).ok())
                });
                tdb.push(mb / t.elapsed().as_secs_f64());
            }
        }
        out!(
            "{ln:<7} bytes {bytes}  ratio {:.4}  sha ok {ok}/{}  fallback segments {fb}",
            bytes as f64 / raw as f64,
            files.len()
        );
        out!(
            "  compress   1t {}   {nt}t {}   batch {}",
            fmt(stats(&mut tc1)),
            fmt(stats(&mut tcn)),
            fmt(stats(&mut tcb))
        );
        out!(
            "  decompress 1t {}   {nt}t {}   batch {}",
            fmt(stats(&mut td1)),
            fmt(stats(&mut tdn)),
            fmt(stats(&mut tdb))
        );
    }
    out!("loadavg after: {}", loadavg());
    if let Some(path) = &a.perfile {
        std::fs::write(path, &perfile).unwrap_or_else(|e| die(e));
    }
    if let Some(path) = &a.streams {
        let mut s = String::from("level\tstream\tcount\tpacked\n");
        for ((l, name), (count, packed)) in &stream_rows {
            s += &format!("{l}\t{name}\t{count}\t{packed}\n");
        }
        std::fs::write(path, s).unwrap_or_else(|e| die(e));
    }
}

fn info(a: &Args) {
    for f in &a.files {
        let r = std::fs::File::open(f).unwrap_or_else(|e| die(e));
        match api::read_metadata(std::io::BufReader::new(r)) {
            Ok(fi) => out!("{}", fi.to_json()),
            Err(e) => die(format!("{f}: {e}")),
        }
    }
}
