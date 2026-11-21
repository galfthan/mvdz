//! Builds examples/c/example.c against the static library and runs it on
//! a demo (a sample demo if MVDZ_TEST_DATA is set, else a synthetic one).
//! Skips if no C compiler is available. Linux only (the link line).
#![cfg(target_os = "linux")]

mod common;
use std::path::PathBuf;
use std::process::Command;

#[test]
fn c_example_builds_and_runs() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let work = std::env::temp_dir().join(format!("mvdz-capi-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    let demo = work.join("demo.mvd");
    let d = common::samples(1).pop().unwrap_or_else(|| {
        (0..20)
            .flat_map(|_| common::synthetic(&[(1, "{\"a\":"), (0, "1}")], false))
            .collect()
    });
    std::fs::write(&demo, d).unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    if Command::new(&cc).arg("--version").output().is_err() {
        eprintln!("no C compiler, skipping");
        return;
    }
    // the static library of this crate, in the profile this test was built
    // with. The test binary is target/<profile>/deps/c_api-* up to cargo
    // 1.99 and target/<profile>/build/<pkg>/<hash>/out/c_api-* from 1.100,
    // so the profile directory is the parent of the nearest `deps` or
    // `build` ancestor (whatever CARGO_TARGET_DIR is)
    let me = std::env::current_exe().unwrap();
    let pdir = me
        .ancestors()
        .find(|d| d.file_name().is_some_and(|n| n == "deps" || n == "build"))
        .and_then(|d| d.parent())
        .expect("test binary is not under a cargo profile directory")
        .to_path_buf();
    let pname = pdir.file_name().unwrap().to_string_lossy().to_string();
    let profile = if pname == "debug" {
        "dev".to_string()
    } else {
        pname
    };
    let st = Command::new(env!("CARGO"))
        .args(["build", "--lib", "--quiet", "--profile", &profile])
        .env("CARGO_TARGET_DIR", pdir.parent().unwrap())
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(st.success());
    let lib = pdir.join("libmvdz.a");
    assert!(lib.exists(), "{lib:?}");
    let exe = work.join("example");
    let out = Command::new(&cc)
        .arg("-O1")
        .arg("-Wall")
        .arg(format!("-I{}", root.join("include").display()))
        .arg(root.join("examples/c/example.c"))
        .arg(&lib)
        .args(["-lpthread", "-ldl", "-lm"])
        .arg("-o")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cc failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = Command::new(&exe)
        .arg(&demo)
        .current_dir(&work)
        .output()
        .unwrap();
    let so = String::from_utf8_lossy(&run.stdout);
    println!("{so}");
    assert!(
        run.status.success(),
        "example failed: {so}\n{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(so.contains("decoded") && so.trim_end().ends_with("OK"));
    let _ = std::fs::remove_dir_all(&work);
}
