#![no_main]
//! The streaming writer on arbitrary demo bytes, written in chunks of
//! varying size, with a small segment size: never panics, refuses exactly
//! what `check_demo` refuses, and otherwise the file decodes to the input.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let level = if data[0] & 1 == 0 {
        mvdz::Level::Normal
    } else {
        mvdz::Level::Fast
    };
    let seg = 64 + (data[1] as usize) * 37;
    let demo = &data[2..];
    let cfg = mvdz::Config {
        level,
        segment: seg,
        ..Default::default()
    };
    let mut w = mvdz::Writer::new(Vec::new(), &cfg).unwrap();
    let mut i = 0;
    let mut step = 1 + (data[0] as usize >> 1);
    let r = (|| {
        while i < demo.len() {
            let j = (i + step).min(demo.len());
            w.write_demo(&demo[i..j])?;
            i = j;
            step = step * 7 % 997 + 1;
        }
        w.finish()
    })();
    // the writer refuses what the one-shot encoder refuses
    match r {
        Err(mvdz::Error::NotMvd(_)) => assert!(mvdz::check_demo(demo).is_err()),
        r => {
            assert!(mvdz::check_demo(demo).is_ok());
            assert!(mvdz::decompress(&r.unwrap(), 1).unwrap() == demo);
        }
    }
});
