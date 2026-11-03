//! The command-line tool: gzip input and output, inputs that are not
//! demos, the census of raw demos, and a closed standard output.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_mvdz");

fn vector(name: &str) -> Vec<u8> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/vectors")
        .join(name);
    std::fs::read(p).unwrap()
}

/// The raw demo of a valid vector.
fn demo(name: &str) -> Vec<u8> {
    mvdz::decompress(&vector(name), 1).unwrap()
}

fn gzip(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

/// A fresh, empty directory for one test.
fn tmp(test: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mvdz-cli-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&Path]) -> std::process::Output {
    Command::new(BIN).args(args).output().unwrap()
}

#[test]
fn gzip_input_is_the_raw_demo() {
    let d = tmp("gzip");
    let raw = demo("duel.normal.mvdz");
    let half = raw.len() / 2;
    // a single member, and two members (as `cat a.gz b.gz` makes)
    let mut two = gzip(&raw[..half]);
    two.extend_from_slice(&gzip(&raw[half..]));
    let inputs = [
        ("duel.mvd", raw.clone()),
        ("duel.mvd.gz", gzip(&raw)),
        ("two.mvd.gz", two),
    ];
    let mut outs = Vec::new();
    for (name, bytes) in &inputs {
        let (i, o) = (d.join(name), d.join(format!("{name}.mvdz")));
        std::fs::write(&i, bytes).unwrap();
        let r = run(&[Path::new("compress"), &i, &o]);
        assert!(r.status.success(), "{name}: {r:?}");
        outs.push(std::fs::read(&o).unwrap());
    }
    // the same file from every input: the identity is the raw demo's
    assert!(outs.iter().all(|o| *o == outs[0]));
    assert_eq!(mvdz::decompress(&outs[0], 1).unwrap(), raw);
    let _ = std::fs::remove_dir_all(&d);
}

/// Input that is not a demo, plain or gzipped, and corrupt gzip: an
/// error, and no output file (not even a temporary one).
#[test]
fn non_demos_are_refused() {
    let d = tmp("notdemo");
    let raw = demo("duel.normal.mvdz");
    let mut cut_gzip = gzip(&raw);
    cut_gzip.truncate(cut_gzip.len() / 2);
    let mut bad_crc = gzip(&raw);
    let n = bad_crc.len();
    bad_crc[n - 6] ^= 1;
    let tiff = b"II*\x00\x08\x00\x00\x00".repeat(200);
    let cases: [(&str, Vec<u8>, &str); 7] = [
        ("empty.mvd", Vec::new(), "not an MVD demo"),
        (
            "text.mvd",
            b"# not a demo\njust some text\n".repeat(100),
            "not an MVD demo",
        ),
        ("tiff.mvd", tiff.clone(), "not an MVD demo"),
        ("tiff.mvd.gz", gzip(&tiff), "not an MVD demo"),
        ("empty.mvd.gz", gzip(b""), "not an MVD demo"),
        ("cut.mvd.gz", cut_gzip, "corrupt gzip"),
        ("crc.mvd.gz", bad_crc, "corrupt gzip"),
    ];
    for (name, bytes, msg) in &cases {
        let (i, o) = (d.join(name), d.join(format!("{name}.mvdz")));
        std::fs::write(&i, bytes).unwrap();
        let r = run(&[Path::new("compress"), &i, &o]);
        let err = String::from_utf8_lossy(&r.stderr);
        assert_eq!(r.status.code(), Some(1), "{name}: {r:?}");
        assert!(err.contains(msg), "{name}: {err}");
        assert!(!o.exists(), "{name}: wrote an output");
    }
    let left: Vec<_> = std::fs::read_dir(&d).unwrap().collect();
    assert_eq!(left.len(), cases.len(), "temporary files left");
    // --force stores a non-demo anyway, and it round-trips
    let (i, o) = (d.join("tiff.mvd"), d.join("forced.mvdz"));
    let r = run(&[Path::new("compress"), Path::new("--force"), &i, &o]);
    assert!(r.status.success(), "{r:?}");
    assert_eq!(
        mvdz::decompress(&std::fs::read(&o).unwrap(), 1).unwrap(),
        tiff
    );
    std::fs::remove_file(&o).unwrap();
    // through the C interface: the same refusal
    for (_, bytes, _) in &cases {
        let (mut out, mut n) = (std::ptr::null_mut(), 0usize);
        let rc = unsafe {
            mvdz::capi::mvdz_compress(
                bytes.as_ptr() as *const _,
                bytes.len(),
                0,
                0,
                1,
                &mut out,
                &mut n,
            )
        };
        assert_eq!(rc, mvdz::capi::MVDZ_E_NOTMVD);
        assert!(out.is_null() && n == 0);
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// gzip input through the C interface gives the same file as the raw demo.
#[test]
fn c_compress_gzip() {
    let raw = demo("duel.normal.mvdz");
    let c = |b: &[u8]| unsafe {
        let (mut out, mut n) = (std::ptr::null_mut(), 0usize);
        let rc =
            mvdz::capi::mvdz_compress(b.as_ptr() as *const _, b.len(), 1, 0, 1, &mut out, &mut n);
        assert_eq!(rc, 0);
        let v = std::slice::from_raw_parts(out, n).to_vec();
        mvdz::capi::mvdz_free(out as *mut _, n);
        v
    };
    assert_eq!(c(&raw), c(&gzip(&raw)));
}

/// decompress --gzip (and an output named .gz) writes the demo gzipped:
/// compress from .mvd.gz, decompress to .gz, gunzip: the raw demo.
#[test]
fn gzip_output() {
    let d = tmp("gzout");
    let raw = demo("duel.normal.mvdz");
    let gz_in = d.join("in.mvd.gz");
    std::fs::write(&gz_in, gzip(&raw)).unwrap();
    let z = d.join("in.mvdz");
    assert!(run(&[Path::new("compress"), &gz_in, &z]).status.success());
    let gunz = |b: &[u8]| mvdz::gunzip(b, 1 << 30).unwrap();
    // inferred from the name, and explicit
    let o1 = d.join("out.mvd.gz");
    assert!(run(&[Path::new("decompress"), &z, &o1]).status.success());
    assert_eq!(gunz(&std::fs::read(&o1).unwrap()), raw);
    let o2 = d.join("out2.bin");
    assert!(
        run(&[Path::new("decompress"), Path::new("--gzip"), &z, &o2])
            .status
            .success()
    );
    assert_eq!(gunz(&std::fs::read(&o2).unwrap()), raw);
    // --no-gzip wins over the name
    let o3 = d.join("raw.gz");
    assert!(
        run(&[Path::new("decompress"), Path::new("--no-gzip"), &z, &o3])
            .status
            .success()
    );
    assert_eq!(std::fs::read(&o3).unwrap(), raw);
    // to standard output; raw without the flag
    let r = run(&[
        Path::new("decompress"),
        Path::new("--gzip"),
        &z,
        Path::new("-"),
    ]);
    assert!(r.status.success());
    assert_eq!(gunz(&r.stdout), raw);
    let r = run(&[Path::new("decompress"), &z, Path::new("-")]);
    assert_eq!(r.stdout, raw);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn gunzip_limit() {
    let raw = demo("duel.normal.mvdz");
    let gz = gzip(&raw);
    assert!(mvdz::is_gzip(&gz) && !mvdz::is_gzip(&raw));
    assert_eq!(mvdz::gunzip(&gz, raw.len() as u64).unwrap(), raw);
    assert!(matches!(
        mvdz::gunzip(&gz, raw.len() as u64 - 1),
        Err(mvdz::Error::Limit(_))
    ));
}

/// The census of a raw demo (plain or gzip) agrees with the census of its
/// .mvdz, the demoinfo column included.
#[test]
fn census_of_raw_demos() {
    let d = tmp("census");
    let z = vector("demoinfo-multi.normal.mvdz");
    let raw = mvdz::decompress(&z, 1).unwrap();
    std::fs::write(d.join("a.mvdz"), &z).unwrap();
    std::fs::write(d.join("a.mvd"), &raw).unwrap();
    std::fs::write(d.join("a.mvd.gz"), gzip(&raw)).unwrap();
    let r = run(&[Path::new("census"), &d]);
    assert!(r.status.success(), "{r:?}");
    let lines: Vec<serde_json::Value> = String::from_utf8(r.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    for l in &lines {
        assert_eq!(l["demoinfo"], true, "{l}");
        assert_eq!(l["sha256"], lines[0]["sha256"]);
        assert_eq!(l["census"], lines[0]["census"]);
    }
    // a demo without KTX demoinfo still says false
    std::fs::write(d.join("a.mvd"), demo("float-fte.normal.mvdz")).unwrap();
    let r = run(&[Path::new("census"), &d.join("a.mvd")]);
    let l: serde_json::Value = serde_json::from_slice(&r.stdout).unwrap();
    assert_eq!(l["demoinfo"], false);
    let _ = std::fs::remove_dir_all(&d);
}

/// Every command that writes to standard output stops quietly when the
/// reader goes away.
#[test]
fn closed_stdout_is_quiet() {
    let vectors = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    let duel = vectors.join("duel.normal.mvdz");
    let d = tmp("pipe");
    let raw = d.join("duel.mvd");
    std::fs::write(&raw, demo("duel.normal.mvdz")).unwrap();
    let dirs: Vec<&Path> = (0..200).map(|_| vectors.as_path()).collect();
    let infos: Vec<&Path> = (0..2000).map(|_| duel.as_path()).collect();
    let cases: Vec<Vec<&Path>> = vec![
        [&[Path::new("census")][..], &dirs].concat(),
        [&[Path::new("census"), Path::new("--tsv")][..], &dirs].concat(),
        [&[Path::new("info")][..], &infos].concat(),
        vec![Path::new("decompress"), &duel, Path::new("-")],
        vec![Path::new("compress"), &raw, Path::new("-")],
    ];
    for args in &cases {
        let mut c = Command::new(BIN)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut buf = [0u8; 16];
        c.stdout.take().unwrap().read_exact(&mut buf).ok();
        // the read end is closed here
        let r = c.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&r.stderr);
        assert!(r.status.success(), "{:?}: {:?} {err}", args[0], r.status);
        assert!(err.is_empty(), "{:?}: {err}", args[0]);
    }
    let _ = std::fs::remove_dir_all(&d);
}
