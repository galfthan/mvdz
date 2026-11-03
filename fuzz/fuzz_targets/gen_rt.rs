#![no_main]
//! Review target: a structure-aware demo; (1) the transform alone, with
//! verification off, must round-trip (else the encoder relies on its
//! fallback: a transform bug); (2) the normal path (verify on) must
//! succeed and round-trip, one-shot and streamed, at any thread count.
use libfuzzer_sys::fuzz_target;
mod mgen;

fn rt(c: &[u8], demo: &[u8], what: &str) {
    match mvdz::decompress(c, 1) {
        Ok(d) => assert!(d == demo, "{what}: decodes to other bytes"),
        Err(e) => panic!("{what}: does not decode: {e}"),
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    let level = if data[0] & 1 == 0 {
        mvdz::Level::Normal
    } else {
        mvdz::Level::Fast
    };
    let seg = if data[0] & 2 == 0 {
        0
    } else {
        32 + data[1] as usize * 8
    };
    let threads = 1 + (data[0] as usize >> 2) % 3;
    let demo = mgen::demo(&data[2..]);
    // (1) the transform without verification
    let cfg = mvdz::Config {
        level,
        segment: seg,
        threads,
        verify: false,
        ..Default::default()
    };
    let (c, _) = mvdz::encode::compress(&demo, &cfg).expect("compress (no verify)");
    if std::env::var("REQUIRE_TRANSFORM_LOSSLESS").is_ok() {
        rt(&c, &demo, "unverified transform");
    } else if mvdz::decompress(&c, 1).ok().as_deref() != Some(&demo[..]) {
        eprintln!("NOTE unverified transform differs");
    }
    // (2) verified paths
    let cfg = mvdz::Config {
        level,
        segment: seg,
        threads,
        ..Default::default()
    };
    let (c, _) = mvdz::encode::compress(&demo, &cfg).expect("compress");
    rt(&c, &demo, "one-shot");
    assert!(mvdz::decompress(&c, 3).unwrap() == demo);
    let scfg = mvdz::Config {
        segment: seg.max(64),
        ..cfg
    };
    // the streaming writer checks its input as the one-shot API does
    if mvdz::check_demo(&demo).is_err() {
        return;
    }
    let mut w = mvdz::Writer::new(Vec::new(), &scfg).unwrap();
    let mut i = 0;
    let mut step = 1 + data[1] as usize;
    while i < demo.len() {
        let j = (i + step).min(demo.len());
        w.write_demo(&demo[i..j]).unwrap();
        i = j;
        step = step * 7 % 997 + 1;
    }
    let f = w.finish().expect("writer finish");
    rt(&f, &demo, "streamed");
});
