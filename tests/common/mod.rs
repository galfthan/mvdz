//! Shared helpers of the integration tests: synthetic demos, a small RNG,
//! the sample demos (skipped when absent) and the every-reader check.
#![allow(dead_code)]

use mvdz::capi::*;
use mvdz::format::*;
use mvdz::{Config, Level};
use std::io::{Cursor, Read};

pub fn cfg(level: Level, seg: usize) -> Config {
    Config {
        level,
        segment: seg,
        threads: 1,
        verify: true,
        ext: Vec::new(),
    }
}

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

// ---------------------------------------------------------------- synthetic demos

pub struct Demo(pub Vec<u8>);

impl Demo {
    pub fn frame(&mut self, tb: u8, ty: u8, payload: &[u8]) {
        self.0.push(tb);
        self.0.push(ty);
        self.0
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.0.extend_from_slice(payload);
    }
    pub fn hidden(&mut self, tb: u8, blocks: &[(u16, &[u8])]) {
        let mut p = Vec::new();
        for (ty, data) in blocks {
            p.extend_from_slice(&(data.len() as u32).to_le_bytes());
            p.extend_from_slice(&ty.to_le_bytes());
            p.extend_from_slice(data);
        }
        self.0.push(tb);
        self.0.push(3);
        self.0.extend_from_slice(&0u32.to_le_bytes());
        self.0.extend_from_slice(&(p.len() as u32).to_le_bytes());
        self.0.extend_from_slice(&p);
    }
}

pub fn s(cmd: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut v = vec![cmd];
    for p in parts {
        v.extend_from_slice(p);
    }
    v
}

pub fn z(t: &str) -> Vec<u8> {
    let mut v = t.as_bytes().to_vec();
    v.push(0);
    v
}

/// KTX demoinfo block bodies (u16 block number, text).
pub fn dinfo_blocks(parts: &[(u16, &str)]) -> Vec<Vec<u8>> {
    parts
        .iter()
        .map(|(n, t)| {
            let mut b = n.to_le_bytes().to_vec();
            b.extend_from_slice(t.as_bytes());
            b
        })
        .collect()
}

/// A demo with prints, fragmented stufftext, userinfo, modellist,
/// serverinfo; optionally demoinfo blocks; optionally a truncated tail.
pub fn synthetic(demoinfo: &[(u16, &str)], truncate: bool) -> Vec<u8> {
    let mut d = Demo(Vec::new());
    let mut p = Vec::new();
    p.extend(s(
        9,
        &[&z(
            "fullserverinfo \"\\map\\dm4\\hostname\\test server\\deathmatch\\3\"\n",
        )],
    ));
    p.extend(s(
        40,
        &[
            &[0],
            &7u32.to_le_bytes(),
            &z("\\name\\alice\\team\\red\\*client\\ezQuake 7"),
        ],
    ));
    p.extend(s(
        40,
        &[
            &[3],
            &9u32.to_le_bytes(),
            &z("\\name\\bob\\team\\blue\\*client\\fte"),
        ],
    ));
    p.extend(s(
        45,
        &[&[0], &z("maps/dm4.bsp"), &z("progs/player.mdl"), &[0], &[0]],
    ));
    d.frame(0, 6, &p);
    for i in 0..50u8 {
        let mut p = Vec::new();
        p.extend(s(8, &[&[2], &z(&format!("message {i}\n"))]));
        // a command split over two stufftexts, and ';'-separated commands
        p.extend(s(9, &[&z("topcolor ")]));
        p.extend(s(
            9,
            &[&z(
                "10\n_cs;wait;_cs\n//ktx took 1 2 3\n//finalscores \"Oct 12\" 3\n",
            )],
        ));
        p.extend(s(52, &[&z("status"), &z(&format!("{i} min"))]));
        d.frame(13 + i % 3, 6, &p);
    }
    if !demoinfo.is_empty() {
        let bodies = dinfo_blocks(demoinfo);
        let refs: Vec<(u16, &[u8])> = bodies.iter().map(|b| (3u16, &b[..])).collect();
        d.hidden(0, &refs);
    }
    d.frame(5, 6, &s(8, &[&[3], &z("bye\n")]));
    if truncate {
        // a frame cut short: header and part of its length
        d.0.extend_from_slice(&[7, 6, 40, 0]);
    }
    d.0
}

/// One usercmd hidden-block body (mvdsv `SV_DebugClientCommand`, 23
/// bytes): player, dropnum, msec, pitch/yaw/roll, moves, buttons, impulse.
pub fn usercmd_block(
    pl: u8,
    drop: u8,
    msec: u8,
    a: [f32; 3],
    mv: [i16; 3],
    bi: [u8; 2],
) -> Vec<u8> {
    let mut b = vec![pl, drop, msec];
    for x in a {
        b.extend_from_slice(&x.to_le_bytes());
    }
    for m in mv {
        b.extend_from_slice(&m.to_le_bytes());
    }
    b.extend_from_slice(&bi);
    b
}

/// A demo with usercmd hidden blocks (type 1, 23 bytes) of four players:
/// turning view angles on the 360/65536 grid, a few angles off it (-0.0,
/// NaN, +180, 1.2345, an odd NaN payload), type-1 blocks of other lengths,
/// two usercmds in one payload, after the base synthetic demo.
pub fn usercmd_demo() -> Vec<u8> {
    let mut d = Demo(synthetic(&[], false));
    let mut rng = Rng(0xc0ffee);
    let mut ang = [[0i32; 2]; 4];
    let odd = [
        -0.0f32,
        f32::NAN,
        180.0,
        1.2345,
        f32::from_bits(0x7fc0_1234),
    ];
    let grid = |s: i32| (s as i16 as f32) * (45.0 / 8192.0);
    for i in 0..600u32 {
        let pl = (i % 4) as u8;
        let a = &mut ang[pl as usize];
        a[0] = (a[0] + rng.below(9) as i32 - 4).clamp(-2900, 2900);
        a[1] += 8 * (rng.below(7) as i32 - 3);
        let mut angles = [grid(a[0]), grid(a[1]), 0.0];
        if i % 53 == 7 {
            angles[(i % 3) as usize] = odd[(i / 53) as usize % odd.len()];
        }
        let fwd = [0i16, 320, 400, -400][(i / 40 % 4) as usize];
        let side = if i % 90 < 30 {
            0
        } else {
            508 * (1 - 2 * ((i / 90) % 2) as i16)
        };
        let bi = [
            (i / 25 % 4 == 0) as u8 | 2 * (i % 70 < 3) as u8,
            if i % 111 == 5 { 7 } else { 0 },
        ];
        let msec = if i % 97 == 0 { 12 } else { 13 };
        let b = usercmd_block(pl, (i % 150 == 149) as u8, msec, angles, [fwd, side, 0], bi);
        if i % 61 == 30 {
            let b2 = usercmd_block(pl ^ 1, 0, 13, angles, [0, 0, 0], [0, 0]);
            d.hidden(0, &[(1, &b), (1, &b2)]);
        } else {
            d.hidden(if pl == 0 { 13 } else { 0 }, &[(1, &b)]);
        }
        if i % 150 == 75 {
            d.hidden(0, &[(1, &b[..3]), (1, &[b.clone(), vec![9]].concat())]);
        }
    }
    d.frame(5, 6, &s(8, &[&[3], &z("bye\n")]));
    d.0
}

// ---------------------------------------------------------------- sample demos

/// Optional test data, from the directory in `MVDZ_TEST_DATA`:
/// `sample/` (real demos named `<sha256>.mvd`), `gen/` (constructed demos,
/// e.g. `float_mvd1.mvd`) and `poc/` (hostile model headers). Tests that
/// need it skip, with a note, when the variable is unset or the file is
/// missing; everything else runs on synthetic demos.
pub fn test_data(sub: &str) -> Option<std::path::PathBuf> {
    match std::env::var_os("MVDZ_TEST_DATA") {
        Some(d) => Some(std::path::PathBuf::from(d).join(sub)),
        None => {
            eprintln!("MVDZ_TEST_DATA is not set: skipping the parts that need {sub}/ demos");
            None
        }
    }
}

/// The first `n` sample demos (sorted by name), or none.
pub fn samples(n: usize) -> Vec<Vec<u8>> {
    let Some(dir) = test_data("sample") else {
        return Vec::new();
    };
    let mut v: Vec<_> = match std::fs::read_dir(dir) {
        Ok(d) => d
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "mvd"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    v.sort();
    v.into_iter()
        .take(n)
        .filter_map(|p| std::fs::read(p).ok())
        .collect()
}

/// A sample demo by its SHA-256.
pub fn sample(sha: &str) -> Option<Vec<u8>> {
    std::fs::read(test_data("sample")?.join(format!("{sha}.mvd"))).ok()
}

/// The duel the golden vectors are made from (hub demo 3ff1f82b...8d92).
pub fn duel() -> Option<Vec<u8>> {
    sample("3ff1f82b5f0274da0af3f1744e7660d602159c3edfb0089134b728d3f3408d92")
}

/// Compresses with the streaming writer, in uneven chunks.
pub fn stream(src: &[u8], c: &Config) -> Vec<u8> {
    let mut w = mvdz::Writer::new(Vec::new(), c).unwrap();
    let mut i = 0;
    let mut step = 1 + src.len() % 7;
    while i < src.len() {
        let j = (i + step).min(src.len());
        w.write_demo(&src[i..j]).unwrap();
        i = j;
        step = (step * 31 + 17) % 70_000 + 1;
    }
    w.finish().unwrap()
}

// ---------------------------------------------------------------- every reader

/// A reader that hands out at most 7 bytes per call.
pub struct Dribble<'a>(pub &'a [u8], pub usize);
impl Read for Dribble<'_> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = b.len().min(7).min(self.0.len() - self.1);
        b[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
        self.1 += n;
        Ok(n)
    }
}

unsafe extern "C" fn cb_read(
    user: *mut std::ffi::c_void,
    buf: *mut std::ffi::c_void,
    n: usize,
) -> std::ffi::c_longlong {
    let c = unsafe { &mut *(user as *mut Cursor<&[u8]>) };
    let b = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, n) };
    c.read(b).map_or(-1, |k| k as i64)
}

unsafe extern "C" fn cb_seek(
    user: *mut std::ffi::c_void,
    off: std::ffi::c_longlong,
    whence: std::ffi::c_int,
) -> std::ffi::c_longlong {
    use std::io::{Seek, SeekFrom};
    let c = unsafe { &mut *(user as *mut Cursor<&[u8]>) };
    let p = match whence {
        0 => SeekFrom::Start(off as u64),
        1 => SeekFrom::Current(off),
        _ => SeekFrom::End(off),
    };
    c.seek(p).map_or(-1, |v| v as i64)
}

fn capi_read_all(r: *mut std::ffi::c_void) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 50_000];
    loop {
        let n = unsafe { mvdz_read(r, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            unsafe { mvdz_close(r) };
            return Err(format!("capi {n}"));
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    unsafe { mvdz_close(r) };
    Ok(out)
}

/// Decodes `b` with every reader: (reader, result).
pub fn all_readers(b: &[u8]) -> Vec<(&'static str, Result<Vec<u8>, String>)> {
    let e = |x: mvdz::Error| x.to_string();
    let mut v = Vec::new();
    v.push(("whole 1t", mvdz::decompress(b, 1).map_err(e)));
    v.push(("whole 3t", mvdz::decompress(b, 3).map_err(e)));
    let seq = |r: Result<mvdz::Reader<mvdz::Seq<_>>, mvdz::Error>| -> Result<Vec<u8>, String> {
        let mut r = r.map_err(e)?;
        let mut out = Vec::new();
        r.read_to_end(&mut out).map_err(|x| x.to_string())?;
        Ok(out)
    };
    v.push((
        "sequential",
        seq(mvdz::Reader::new(Cursor::new(b), &Default::default())),
    ));
    let mut out = Vec::new();
    let r = mvdz::Reader::new(Dribble(b, 0), &Default::default())
        .map_err(e)
        .and_then(|mut r| {
            r.read_to_end(&mut out).map_err(|x| x.to_string())?;
            Ok(std::mem::take(&mut out))
        });
    v.push(("sequential dribble", r));
    let r = mvdz::Reader::open(Cursor::new(b), &Default::default())
        .map_err(e)
        .and_then(|mut r| {
            let mut out = Vec::new();
            r.read_to_end(&mut out).map_err(|x| x.to_string())?;
            Ok(out)
        });
    v.push(("seekable", r));
    let mut err = 0;
    let r = unsafe { mvdz_open_memory(b.as_ptr() as *const _, b.len(), 0, 0, &mut err) };
    v.push((
        "c memory",
        if r.is_null() {
            Err(format!("capi {err}"))
        } else {
            capi_read_all(r)
        },
    ));
    let mut cur = Cursor::new(b);
    let u = &mut cur as *mut Cursor<&[u8]> as *mut std::ffi::c_void;
    let r = unsafe { mvdz_open_callbacks(Some(cb_read), None, u, 0, 0, &mut err) };
    v.push((
        "c sequential",
        if r.is_null() {
            Err(format!("capi {err}"))
        } else {
            capi_read_all(r)
        },
    ));
    let mut cur = Cursor::new(b);
    let u = &mut cur as *mut Cursor<&[u8]> as *mut std::ffi::c_void;
    let r = unsafe { mvdz_open_callbacks(Some(cb_read), Some(cb_seek), u, 0, 0, &mut err) };
    v.push((
        "c seekable",
        if r.is_null() {
            Err(format!("capi {err}"))
        } else {
            capi_read_all(r)
        },
    ));
    let (mut out, mut n) = (std::ptr::null_mut(), 0usize);
    let rc = unsafe { mvdz_decompress(b.as_ptr() as *const _, b.len(), 2, 0, 0, &mut out, &mut n) };
    v.push((
        "c decompress",
        if rc != 0 {
            Err(format!("capi {rc}"))
        } else {
            let d = if n == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(out, n) }.to_vec()
            };
            unsafe { mvdz_free(out as *mut _, n) };
            Ok(d)
        },
    ));
    v
}

/// Every reader accepts `b` with the same bytes, or every reader refuses
/// it. Returns the bytes if accepted. Structural refusals are also
/// required of read_metadata (`structural`).
pub fn readers_agree(b: &[u8], what: &str) -> Option<Vec<u8>> {
    let rs = all_readers(b);
    let first = &rs[0].1;
    for (name, r) in &rs {
        match (first, r) {
            (Ok(a), Ok(x)) => assert!(a == x, "{what}: {name} decodes different bytes"),
            (Err(_), Err(_)) => {}
            _ => panic!(
                "{what}: readers disagree: {} {:?} vs {name} {:?}",
                rs[0].0,
                first.as_ref().map(|v| v.len()),
                r.as_ref().map(|v| v.len())
            ),
        }
    }
    let meta = mvdz::read_metadata(Cursor::new(b));
    if first.is_ok() {
        assert!(
            meta.is_ok(),
            "{what}: read_metadata refuses a file the decoders accept: {:?}",
            meta.err()
        );
    }
    if let Err(m) = &meta {
        assert!(
            first.is_err(),
            "{what}: decoders accept a file read_metadata refuses ({m})"
        );
    }
    first.as_ref().ok().cloned()
}

// ---------------------------------------------------------------- file surgery

/// A file as (kind, body) sections, without its trailer.
pub fn sections(c: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut v = Vec::new();
    let mut off = PREFIX_LEN;
    while off < c.len() {
        let s = sec_hdr(&c[off..off + SEC_HDR], off as u64);
        let b = c[off + SEC_HDR..off + SEC_HDR + s.len as usize].to_vec();
        if s.kind != K_TRAILER {
            v.push((s.kind, b));
        }
        off = s.end() as usize;
    }
    v
}

/// Assembles sections: the INDEX segment offsets and the trailer are
/// recomputed when `fix` is set (until the re-encoded INDEX no longer
/// changes length).
pub fn build(flags: u8, prefix_meta: bool, secs: &[(u8, Vec<u8>)], fix: bool) -> Vec<u8> {
    let mut secs = secs.to_vec();
    let offsets = |secs: &[(u8, Vec<u8>)]| -> (Vec<u64>, u64) {
        let mut offs = Vec::new();
        let mut off = PREFIX_LEN as u64;
        for (_, b) in secs {
            offs.push(off);
            off += (SEC_HDR + b.len()) as u64;
        }
        (offs, off)
    };
    loop {
        let (offs, _) = offsets(&secs);
        if !fix {
            break;
        }
        let of_kind = |k: u8| -> Vec<u64> {
            secs.iter()
                .zip(&offs)
                .filter(|(s, _)| s.0 == k)
                .map(|(_, &o)| o)
                .collect()
        };
        let seg_offs = of_kind(K_SEGMENT);
        let mut changed = false;
        for (k, b) in secs.iter_mut() {
            if *k != K_INDEX {
                continue;
            }
            let mut ix = Index::decode(b).unwrap();
            for (s, o) in ix.segs.iter_mut().zip(&seg_offs) {
                s.off = *o;
            }
            let nb = ix.encode();
            changed |= nb != *b;
            *b = nb;
        }
        if !changed {
            break;
        }
    }
    let (offs, _) = offsets(&secs);
    let first = |k: u8| secs.iter().position(|s| s.0 == k).map_or(0, |i| offs[i]);
    let mut out = prefix(flags, if prefix_meta { first(K_META) } else { 0 });
    for (k, b) in &secs {
        section(&mut out, *k, b);
    }
    if fix {
        out.extend_from_slice(
            &Trailer {
                index_off: first(K_INDEX),
                meta_off: first(K_META),
            }
            .bytes(),
        );
    }
    out
}
