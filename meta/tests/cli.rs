//! The mvdz-meta command: .mvdz, .mvd and .mvd.gz inputs agree, text and
//! JSON output, refused inputs, and a closed standard output.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_mvdz-meta");

fn vector(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/vectors")
        .join(name)
}

fn tmp(test: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mvdz-meta-cli-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&Path]) -> std::process::Output {
    Command::new(BIN).args(args).output().unwrap()
}

/// A demo reads the same as .mvdz, .mvd and .mvd.gz (a directory means the
/// demos in it), in text and JSON.
#[test]
fn inputs_agree() {
    let d = tmp("inputs");
    let z = std::fs::read(vector("demoinfo-multi.normal.mvdz")).unwrap();
    let raw = mvdz::decompress(&z, 1).unwrap();
    std::fs::write(d.join("a.mvdz"), &z).unwrap();
    std::fs::write(d.join("a.mvd"), &raw).unwrap();
    std::fs::write(
        d.join("a.mvd.gz"),
        mvdz::write_gzip(&raw, Vec::new()).unwrap(),
    )
    .unwrap();
    std::fs::write(d.join("notes.txt"), b"not a demo").unwrap();
    let r = run(&[Path::new("--json"), &d]);
    assert!(r.status.success(), "{r:?}");
    let lines: Vec<serde_json::Value> = String::from_utf8(r.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    for l in &lines {
        assert!(l["demoinfo"].is_object(), "{l}");
        for k in ["sha256", "census", "players", "roster", "clock", "map"] {
            assert_eq!(l[k], lines[0][k], "{k}");
        }
    }
    let r = run(&[&d.join("a.mvdz")]);
    assert!(r.status.success());
    let t = String::from_utf8(r.stdout).unwrap();
    assert!(
        t.contains("players at start:") && t.contains("duration"),
        "{t}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A file that is not a demo is reported and the exit status says so; the
/// other files are still shown.
#[test]
fn non_demo_refused() {
    let d = tmp("nondemo");
    std::fs::write(d.join("x.mvd"), b"hello, not a demo at all").unwrap();
    let r = run(&[&d.join("x.mvd"), &vector("duel.normal.mvdz")]);
    assert_eq!(r.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&r.stderr).contains("not an MVD demo"));
    assert!(String::from_utf8_lossy(&r.stdout).contains("duel.normal.mvdz"));
    let _ = std::fs::remove_dir_all(&d);
}

/// The output stops quietly when the reader goes away.
#[test]
fn closed_stdout_is_quiet() {
    let v = vector("");
    let dirs: Vec<&Path> = (0..200).map(|_| v.as_path()).collect();
    for json in [false, true] {
        let mut args: Vec<&Path> = dirs.clone();
        if json {
            args.insert(0, Path::new("--json"));
        }
        let mut c = Command::new(BIN)
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut buf = [0u8; 16];
        c.stdout.as_mut().unwrap().read_exact(&mut buf).unwrap();
        drop(c.stdout.take());
        let out = c.wait_with_output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("panicked"), "{err}");
    }
}
