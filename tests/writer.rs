//! The streaming writer under failing sinks, aborts, and C API handle
//! abuse.

mod common;
use common::*;
use mvdz::Level;
use mvdz::capi::*;
use std::io::{Cursor, Read, Write};

/// A sink that accepts `ok` bytes, then fails `fails` times (accepting
/// part of the write first), then works again.
struct Flaky {
    out: Vec<u8>,
    ok: usize,
    fails: usize,
}

impl Write for Flaky {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.out.len() >= self.ok && self.fails > 0 {
            self.fails -= 1;
            return Err(std::io::Error::other("sink failed"));
        }
        let n = b
            .len()
            .min((self.ok - self.out.len().min(self.ok)).max(1))
            .min(b.len());
        self.out.extend_from_slice(&b[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The duel, or a synthetic demo of similar size whose frames carry
/// incompressible payloads (the sink-failure tests need ~100 KB of output).
fn src() -> Vec<u8> {
    duel().unwrap_or_else(|| {
        let mut d = Demo(Vec::new());
        let mut rng = Rng(7);
        for i in 0..40 {
            d.0.extend(synthetic(&[], false));
            let noise: Vec<u8> = (0..4000).map(|_| rng.next() as u8).collect();
            d.frame(1 + (i % 3) as u8, 6, &noise);
        }
        d.0
    })
}

/// One transient sink error poisons the writer: every later write and
/// finish fail, and what was written is an exact prefix of a valid stream
/// (robustness review 6: finish used to return Ok on a corrupt file).
#[test]
fn sink_error_is_sticky() {
    let src = src();
    for level in [Level::Normal, Level::Fast] {
        let sink = Flaky {
            out: Vec::new(),
            ok: 30_000,
            fails: 1,
        };
        let mut w = mvdz::Writer::new(sink, &cfg(level, 20_000)).unwrap();
        let mut first_err = None;
        for c in src.chunks(4000) {
            if let Err(e) = w.write_demo(c) {
                first_err.get_or_insert(e);
            }
        }
        let e = first_err.expect("the sink error is reported");
        assert!(matches!(e, mvdz::Error::Io(_)));
        assert!(w.write_demo(b"x").is_err(), "sticky");
        assert!(w.finish().is_err(), "finish after an error fails");
    }
    // the bytes that reached a sink before an error decode sequentially to
    // a prefix of the demo
    let mut full = mvdz::Writer::new(Vec::new(), &cfg(Level::Normal, 20_000)).unwrap();
    full.write_demo(&src).unwrap();
    let f = full.finish().unwrap();
    for cut in [16, 1000, f.len() / 3, f.len() - 50] {
        let mut r = mvdz::Reader::new(Cursor::new(&f[..cut]), 1, &Default::default()).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0u8; 8192];
        let err = loop {
            match r.read(&mut buf) {
                Ok(0) => break None,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) => break Some(e),
            }
        };
        assert!(err.is_some(), "a cut file is incomplete");
        assert!(r.read(&mut buf).is_err(), "reader errors are sticky");
        assert!(got == src[..got.len()]);
    }
}

#[test]
fn abort_leaves_a_readable_prefix() {
    let src = src();
    let mut w = mvdz::Writer::new(Vec::new(), &cfg(Level::Fast, 20_000)).unwrap();
    w.write_demo(&src[..src.len() / 2]).unwrap();
    let out = w.abort();
    assert!(mvdz::decompress(&out, 1).is_err(), "no trailer: incomplete");
    let mut r = mvdz::Reader::new(Cursor::new(&out), 1, &Default::default()).unwrap();
    let mut got = Vec::new();
    let _ = r.read_to_end(&mut got);
    assert!(got.len() > 20_000 && got == src[..got.len()]);
}

/// Demoinfo early in a stream: later segments are held back (coded) and
/// written after the metadata.
#[test]
fn demoinfo_early_in_a_stream() {
    let mut d = Demo(Vec::new());
    let b = dinfo_blocks(&[(1, "{\"x\":"), (0, "1}")]);
    d.hidden(0, &[(3, &b[0][..]), (3, &b[1][..])]);
    let mut demo = d.0;
    for _ in 0..30 {
        demo.extend(synthetic(&[], false));
    }
    let st = stream(&demo, &cfg(Level::Normal, 4096));
    assert_eq!(readers_agree(&st, "early demoinfo").unwrap(), demo);
    let m = mvdz::read_metadata(Cursor::new(&st)).unwrap().meta;
    assert_eq!(m.demoinfo_json().unwrap(), b"{\"x\":1}");
}

// ---------------------------------------------------------------- C API handles

#[test]
fn c_handles() {
    let src = synthetic(&[], false);
    let c = mvdz::compress(&src, &cfg(Level::Normal, 300)).unwrap();
    unsafe {
        let mut err = 0;
        let r = mvdz_open_memory(c.as_ptr() as *const _, c.len(), 1, 0, 0, &mut err);
        assert!(!r.is_null() && err == 0);
        let n = mvdz_segment_count(r);
        assert!(n > 1);
        let mut off = 0u64;
        assert_eq!(
            mvdz_seek_segment(r, n as usize, &mut off),
            MVDZ_E_INVALID,
            "out of range"
        );
        assert_eq!(mvdz_seek_segment(r, 1, &mut off), MVDZ_OK);
        // one handle used from several threads: calls are serialised
        let rr = r as usize;
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(move || {
                    let mut buf = [0u8; 100];
                    for _ in 0..50 {
                        let k = mvdz_read(rr as *mut _, buf.as_mut_ptr() as *mut _, buf.len());
                        assert!(k >= 0);
                    }
                });
            }
        });
        mvdz_close(r);
        mvdz_close(r); // harmless
        let mut buf = [0u8; 10];
        assert_eq!(
            mvdz_read(r, buf.as_mut_ptr() as *mut _, 10),
            MVDZ_E_INVALID as i64
        );
        assert_eq!(
            mvdz_read(12345 as *mut _, buf.as_mut_ptr() as *mut _, 10),
            MVDZ_E_INVALID as i64
        );
        // a sticky read error
        let mut bad = c.clone();
        let k = bad.len() / 2;
        bad[k] ^= 0x40;
        let r = mvdz_open_memory(bad.as_ptr() as *const _, bad.len(), 1, 0, 0, &mut err);
        if !r.is_null() {
            let mut big = vec![0u8; 1 << 20];
            let mut first = 0;
            for _ in 0..100 {
                let k = mvdz_read(r, big.as_mut_ptr() as *mut _, big.len());
                if k < 0 {
                    first = k;
                    break;
                }
            }
            assert!(first < 0);
            assert_eq!(
                mvdz_read(r, big.as_mut_ptr() as *mut _, big.len()),
                first,
                "sticky"
            );
            mvdz_close(r);
        }
        // empty results are NULL
        let (mut out, mut on) = (std::ptr::null_mut(), 1usize);
        let empty = mvdz::encode::compress(b"", &cfg(Level::Normal, 0))
            .unwrap()
            .0;
        assert_eq!(
            mvdz_decompress(
                empty.as_ptr() as *const _,
                empty.len(),
                1,
                0,
                0,
                &mut out,
                &mut on
            ),
            MVDZ_OK
        );
        assert!(out.is_null() && on == 0);
        mvdz_free(out as *mut _, on);
        // the JSON is valid
        let (mut j, mut jn) = (std::ptr::null_mut(), 0usize);
        assert_eq!(
            mvdz_info_json_memory(c.as_ptr() as *const _, c.len(), 0, &mut j, &mut jn),
            MVDZ_OK
        );
        let s = std::slice::from_raw_parts(j as *const u8, jn);
        assert_eq!(*j.add(jn), 0, "NUL-terminated");
        serde_json::from_slice::<serde_json::Value>(s).unwrap();
        mvdz_free(j as *mut _, 12345); // the length is not trusted
    }
}

unsafe extern "C" fn w_cb(
    user: *mut std::ffi::c_void,
    buf: *const std::ffi::c_void,
    n: usize,
) -> std::ffi::c_longlong {
    let v = unsafe { &mut *(user as *mut (Vec<u8>, usize)) };
    if v.1 > 0 && v.0.len() >= v.1 {
        return -1;
    }
    v.0.extend_from_slice(unsafe { std::slice::from_raw_parts(buf as *const u8, n) });
    n as i64
}

#[test]
fn c_writer() {
    let src = src();
    unsafe {
        // complete
        let mut sink = (Vec::<u8>::new(), 0usize);
        let mut err = 0;
        let w = mvdz_writer_open(
            Some(w_cb),
            &mut sink as *mut _ as *mut _,
            0,
            50_000,
            1,
            &mut err,
        );
        assert!(!w.is_null());
        assert_eq!(
            mvdz_writer_write(w, src.as_ptr() as *const _, src.len()),
            MVDZ_OK
        );
        assert_eq!(mvdz_writer_finish(w), MVDZ_OK);
        assert_eq!(mvdz_writer_finish(w), MVDZ_E_INVALID, "finished handle");
        assert_eq!(mvdz::decompress(&sink.0, 1).unwrap(), src);
        // a failing sink: sticky
        let mut sink = (Vec::<u8>::new(), 40_000usize);
        let w = mvdz_writer_open(
            Some(w_cb),
            &mut sink as *mut _ as *mut _,
            1,
            20_000,
            1,
            &mut err,
        );
        let mut first = MVDZ_OK;
        for c in src.chunks(5000) {
            let rc = mvdz_writer_write(w, c.as_ptr() as *const _, c.len());
            if rc != MVDZ_OK && first == MVDZ_OK {
                first = rc;
            }
        }
        assert_eq!(first, MVDZ_E_IO);
        assert_eq!(
            mvdz_writer_write(w, src.as_ptr() as *const _, 10),
            MVDZ_E_IO
        );
        assert_eq!(mvdz_writer_finish(w), MVDZ_E_IO);
        // a failed finish freed the handle too
        assert_eq!(
            mvdz_writer_write(w, src.as_ptr() as *const _, 10),
            MVDZ_E_INVALID
        );
        assert_eq!(mvdz_writer_finish(w), MVDZ_E_INVALID);
        // abort
        let mut sink = (Vec::<u8>::new(), 0usize);
        let w = mvdz_writer_open(
            Some(w_cb),
            &mut sink as *mut _ as *mut _,
            0,
            20_000,
            1,
            &mut err,
        );
        assert_eq!(
            mvdz_writer_write(w, src.as_ptr() as *const _, src.len() / 2),
            MVDZ_OK
        );
        assert_eq!(mvdz_writer_abort(w), MVDZ_OK);
        assert_eq!(mvdz_writer_abort(w), MVDZ_E_INVALID);
        assert!(mvdz::decompress(&sink.0, 1).is_err());
    }
}

/// Extension metadata set on a streaming writer: a later value replaces an
/// earlier one, and a refused key leaves the writer working.
#[test]
fn writer_ext() {
    let src = src();
    let mut w = mvdz::Writer::new(Vec::new(), &cfg(Level::Fast, 50_000)).unwrap();
    w.set_ext("source", b"first").unwrap();
    w.write_demo(&src[..src.len() / 2]).unwrap();
    w.set_ext("match", b"42").unwrap();
    w.set_ext("source", b"mvdsv").unwrap();
    let big = vec![b'x'; 1 << 20];
    assert!(matches!(
        w.set_ext("big", &big),
        Err(mvdz::Error::Invalid(_))
    ));
    w.write_demo(&src[src.len() / 2..]).unwrap();
    let out = w.finish().unwrap();
    assert_eq!(mvdz::decompress(&out, 1).unwrap(), src);
    let m = mvdz::read_metadata(Cursor::new(&out)).unwrap().meta;
    assert_eq!(
        m.ext,
        vec![
            ("source".to_string(), b"mvdsv".to_vec()),
            ("match".to_string(), b"42".to_vec())
        ]
    );
    // through the C interface
    unsafe {
        let mut sink = (Vec::<u8>::new(), 0usize);
        let mut err = 0;
        let w = mvdz_writer_open(
            Some(w_cb),
            &mut sink as *mut _ as *mut _,
            1,
            50_000,
            1,
            &mut err,
        );
        let v = b"hub";
        assert_eq!(
            mvdz_writer_set_ext(w, c"source".as_ptr(), v.as_ptr() as *const _, v.len()),
            MVDZ_OK
        );
        assert_eq!(
            mvdz_writer_set_ext(w, c"big".as_ptr(), big.as_ptr() as *const _, big.len()),
            MVDZ_E_INVALID
        );
        assert_eq!(
            mvdz_writer_set_ext(w, std::ptr::null(), v.as_ptr() as *const _, 3),
            MVDZ_E_INVALID
        );
        assert_eq!(
            mvdz_writer_write(w, src.as_ptr() as *const _, src.len()),
            MVDZ_OK
        );
        assert_eq!(mvdz_writer_finish(w), MVDZ_OK);
        let m = mvdz::read_metadata(Cursor::new(&sink.0)).unwrap().meta;
        assert_eq!(m.ext, vec![("source".to_string(), b"hub".to_vec())]);
        assert_eq!(mvdz::decompress(&sink.0, 1).unwrap(), src);
    }
}
