//! Round trips through every reader: both levels, whole-file and
//! segmented, one-shot and streamed; seeking; odd and non-MVD input;
//! determinism.

mod common;
use common::*;
use mvdz::{Config, Level};
use std::io::{Cursor, Read};

const LEVELS: [Level; 2] = [Level::Normal, Level::Fast];

/// The one-shot encoder without the input check (any bytes round-trip).
fn unchecked(src: &[u8], c: &Config) -> (Vec<u8>, mvdz::Info) {
    mvdz::encode::compress(src, c).unwrap()
}

fn check_all_ways(src: &[u8], what: &str, expect_transform: bool) {
    for level in LEVELS {
        for seg in [0usize, 65536] {
            let c = cfg(level, seg);
            let (one, info) = unchecked(src, &c);
            if expect_transform {
                assert_eq!(
                    info.fallback_segments, 0,
                    "{what}: {level:?} seg {seg} fell back"
                );
            }
            let back = readers_agree(&one, what)
                .unwrap_or_else(|| panic!("{what}: {level:?} seg {seg} refused"));
            assert!(back == src, "{what}: {level:?} seg {seg} one-shot");
            let st = if mvdz::check_demo(src).is_ok() {
                stream(
                    src,
                    &Config {
                        segment: if seg == 0 { 100_000 } else { seg },
                        ..c.clone()
                    },
                )
            } else {
                // the streaming writer refuses non-demos (see above)
                one.clone()
            };
            let back = readers_agree(&st, what)
                .unwrap_or_else(|| panic!("{what}: {level:?} seg {seg} streamed refused"));
            assert!(back == src, "{what}: {level:?} seg {seg} streamed");
        }
    }
}

#[test]
fn synthetic_demos() {
    check_all_ways(&synthetic(&[], false), "plain", true);
    check_all_ways(
        &synthetic(&[(1, "{\"a\":"), (0, "1}")], false),
        "demoinfo",
        true,
    );
    check_all_ways(&synthetic(&[], true), "truncated", true);
}

#[test]
fn sample_demos() {
    for (i, src) in samples(3).iter().enumerate() {
        check_all_ways(src, &format!("sample {i}"), true);
    }
}

/// Input that is not an MVD demo is refused by every encoder, before
/// anything is written by the one-shot one.
#[test]
fn empty_and_tiny_inputs() {
    let mut rng = Rng(7);
    let random: Vec<u8> = (0..10_000).map(|_| rng.next() as u8).collect();
    let text = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    let inputs: [&[u8]; 7] = [
        b"",
        b"\x00",
        b"MVDZ\x01",
        b"hello world",
        b"II*\x00\x08\x00\x00\x00\x00\x00\x00\x00", // a TIFF header
        &random,
        &text,
    ];
    for src in inputs {
        for level in LEVELS {
            let e = mvdz::compress(src, &cfg(level, 0)).unwrap_err();
            assert!(matches!(e, mvdz::Error::NotMvd(_)), "{e}");
            let mut w = mvdz::Writer::new(Vec::new(), &cfg(level, 1000)).unwrap();
            let r = w.write_demo(src).and_then(|_| w.finish().map(|_| ()));
            assert!(matches!(r, Err(mvdz::Error::NotMvd(_))), "{r:?}");
        }
        // the unchecked encoder still round-trips any bytes
        check_all_ways(src, &format!("{src:?}"), false);
    }
}

/// Input that does not frame from the first byte: the streaming writer
/// used to panic on it (correctness review F2, robustness 7).
#[test]
fn non_mvd_input_streams() {
    let mut rng = Rng(7);
    let random: Vec<u8> = (0..10_000).map(|_| rng.next() as u8).collect();
    let text = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    let mut inputs = vec![vec![1u8], random, text];
    if let Some(d) = duel() {
        inputs.push(d[..300].to_vec());
        // the first frame cut by one byte
        let n = 6 + u32::from_le_bytes(d[2..6].try_into().unwrap()) as usize;
        let mut cut = d[..n - 1].to_vec();
        cut.extend_from_slice(&d[n..200_000]);
        inputs.push(cut);
    }
    for (i, src) in inputs.iter().enumerate() {
        // the streaming writer refuses what is not a demo
        if mvdz::check_demo(src).is_err() {
            let mut w = mvdz::Writer::new(Vec::new(), &cfg(Level::Fast, 1000)).unwrap();
            let r = w.write_demo(src).and_then(|_| w.finish().map(|_| ()));
            assert!(matches!(r, Err(mvdz::Error::NotMvd(_))), "input {i}");
            continue;
        }
        for level in LEVELS {
            for seg in [1000usize, 1 << 20] {
                let st = stream(src, &cfg(level, seg));
                assert!(
                    readers_agree(&st, "non-mvd").unwrap() == *src,
                    "input {i} {level:?} seg {seg}"
                );
            }
        }
    }
}

/// A flood of svc_nop compresses beyond the ratio limit: the encoder must
/// store such segments in a form every reader accepts (correctness F4).
#[test]
fn ratio_limit_fallback() {
    let mut d = Demo(Vec::new());
    for _ in 0..400 {
        d.frame(0, 1, &[1u8; 10_000]);
    }
    let src = d.0;
    for level in LEVELS {
        for seg in [0usize, 1 << 20] {
            let (c, info) = mvdz::compress_info(&src, &cfg(level, seg)).unwrap();
            assert!(info.fallback_segments > 0);
            assert!(readers_agree(&c, "nop").unwrap() == src);
            let st = stream(&src, &cfg(level, 4 << 20));
            assert!(readers_agree(&st, "nop streamed").unwrap() == src);
        }
    }
}

#[test]
fn seeking() {
    let Some(src) = duel() else { return };
    for level in LEVELS {
        for c in [
            mvdz::compress(&src, &cfg(level, 40_000)).unwrap(),
            stream(&src, &cfg(level, 50_000)),
        ] {
            let mut r = mvdz::Reader::open(Cursor::new(&c), 2, &Default::default()).unwrap();
            let ix = r.index().unwrap().clone();
            assert!(ix.segs.len() > 3);
            for (i, s) in ix.segs.iter().enumerate() {
                assert_eq!(r.seek_segment(i).unwrap(), s.raw_off);
                let mut tail = Vec::new();
                r.read_to_end(&mut tail).unwrap();
                assert!(tail == src[s.raw_off as usize..], "segment {i}");
            }
            assert!(matches!(
                r.seek_segment(ix.segs.len()),
                Err(mvdz::Error::Invalid(_))
            ));
            let dur = ix.duration_ms;
            for ms in [0, 1, 1000, dur / 2, dur, dur + 1] {
                let (i, t, off) = r.seek_time(ms).unwrap();
                assert!(i == 0 || t < ms, "segment {i} starts before {ms}");
                assert!(i + 1 == ix.segs.len() || ix.segs[i + 1].t_start >= ms);
                let mut tail = Vec::new();
                r.read_to_end(&mut tail).unwrap();
                assert!(tail == src[off as usize..]);
            }
        }
    }
}

/// Edited demos (frames dropped, duplicated, bytes changed, cuts) take the
/// escape and tail paths; all of them round-trip.
#[test]
fn edited_demos_round_trip() {
    let base = duel().unwrap_or_else(|| synthetic(&[(1, "{\"a\":"), (0, "1}")], false));
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut refused = 0;
    for case in 0..30 {
        let mut d = base.clone();
        for _ in 0..1 + rng.below(4) {
            let i = rng.below(d.len());
            match rng.below(4) {
                0 => {
                    let j = (i + rng.below(2000)).min(d.len());
                    d.drain(i..j);
                }
                1 => {
                    let j = (i + rng.below(2000)).min(d.len());
                    let chunk = d[i..j].to_vec();
                    let k = rng.below(d.len());
                    d.splice(k..k, chunk);
                }
                2 => {
                    for _ in 0..1 + rng.below(16) {
                        let k = rng.below(d.len());
                        d[k] = rng.next() as u8;
                    }
                }
                _ => d.truncate(i),
            }
        }
        let level = LEVELS[case % 2];
        let seg = [0, 4096, 65536][case % 3];
        // an edit that destroys the first frame makes it a non-demo
        if mvdz::check_demo(&d).is_err() {
            refused += 1;
            continue;
        }
        let c = mvdz::compress(&d, &cfg(level, seg)).unwrap();
        assert!(mvdz::decompress(&c, 1).unwrap() == d, "case {case}");
        let st = stream(&d, &cfg(level, seg.max(4096)));
        assert!(
            mvdz::decompress(&st, 2).unwrap() == d,
            "case {case} streamed"
        );
    }
    // edits within the demo keep it a demo: only cuts into the first frame
    // are refused
    assert!(refused <= 6, "{refused} of 30 refused");
}

/// The constructed demos of the correctness review (float coordinates,
/// demoinfo variants, odd masks, ...), when present.
#[test]
fn constructed_demos() {
    let Some(gdir) = test_data("gen") else { return };
    let Ok(dir) = std::fs::read_dir(gdir) else {
        return;
    };
    let mut files: Vec<_> = dir.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    files.sort();
    for p in files {
        let src = std::fs::read(&p).unwrap();
        if src.len() > 20 << 20 {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        // a few have no MVD start (only dem_set frames, a cut first
        // frame): refused, and still round-trip through the unchecked
        // encoder
        if mvdz::check_demo(&src).is_err() {
            assert!(matches!(
                mvdz::compress(&src, &cfg(Level::Fast, 0)),
                Err(mvdz::Error::NotMvd(_))
            ));
            for level in LEVELS {
                let (c, _) = mvdz::encode::compress(&src, &cfg(level, 50_000)).unwrap();
                assert!(mvdz::decompress(&c, 2).unwrap() == src, "{name}");
            }
            continue;
        }
        for level in LEVELS {
            for seg in [0usize, 50_000] {
                let c = mvdz::compress(&src, &cfg(level, seg)).unwrap();
                assert!(
                    mvdz::decompress(&c, 2).unwrap() == src,
                    "{name} {level:?} {seg}"
                );
                let st = stream(&src, &cfg(level, seg.max(50_000)));
                assert!(
                    mvdz::decompress(&st, 1).unwrap() == src,
                    "{name} {level:?} streamed"
                );
            }
        }
    }
}

#[test]
fn deterministic_output() {
    let src = duel().unwrap_or_else(|| synthetic(&[(0, "{}")], false));
    for level in LEVELS {
        for seg in [0usize, 30_000] {
            let a = mvdz::compress(&src, &cfg(level, seg)).unwrap();
            let b = mvdz::compress(
                &src,
                &Config {
                    threads: 3,
                    ..cfg(level, seg)
                },
            )
            .unwrap();
            assert!(a == b, "{level:?} {seg}: threads change the output");
            let s1 = stream(&src, &cfg(level, seg.max(30_000)));
            let s2 = stream(
                &src,
                &Config {
                    threads: 3,
                    ..cfg(level, seg.max(30_000))
                },
            );
            assert!(s1 == s2);
        }
    }
}

#[test]
fn extension_metadata() {
    let src = synthetic(&[], false);
    let c = Config {
        ext: vec![
            ("source".into(), b"hub".to_vec()),
            ("hub_game_id".into(), b"123".to_vec()),
        ],
        ..cfg(Level::Normal, 0)
    };
    let f = mvdz::compress(&src, &c).unwrap();
    let m = mvdz::read_metadata(Cursor::new(&f)).unwrap().meta;
    assert_eq!(m.ext, c.ext);
    let st = stream(&src, &c);
    assert_eq!(
        mvdz::read_metadata(Cursor::new(&st)).unwrap().meta.ext,
        c.ext
    );
    // over the limit, or a duplicate key: refused at compress time
    let big = Config {
        ext: vec![("x".into(), vec![0; 2 << 20])],
        ..cfg(Level::Normal, 0)
    };
    assert!(matches!(
        mvdz::compress(&src, &big),
        Err(mvdz::Error::Invalid(_))
    ));
    let dup = Config {
        ext: vec![("x".into(), vec![1]), ("x".into(), vec![2])],
        ..cfg(Level::Normal, 0)
    };
    assert!(matches!(
        mvdz::compress(&src, &dup),
        Err(mvdz::Error::Invalid(_))
    ));
}
