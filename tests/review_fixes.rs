//! Regressions for the final reviews (robustness R-*, correctness C-*,
//! spec D-*): each test is a reviewer's reproducer, rebuilt here, that
//! failed before the fix.

mod common;
use common::*;
use mvdz::budget::{Budget, Limits, ReadOptions};
use mvdz::capi::*;
use mvdz::format::*;
use mvdz::util::{crc32, put_uv};
use mvdz::{Attachment, Config, Error, Level};
use std::io::Cursor;
use std::time::{Duration, Instant};

fn base() -> (Vec<u8>, Vec<(u8, Vec<u8>)>) {
    let demo = synthetic(&[(1, "{\"a\":"), (0, "1}")], false);
    let c = mvdz::compress(&demo, &cfg(Level::Normal, 0)).unwrap();
    (demo, sections(&c))
}

fn key(i: usize, w: usize) -> String {
    const A: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut x = i;
    (0..w)
        .map(|_| {
            let c = A[x % 62] as char;
            x /= 62;
            c
        })
        .collect()
}

fn sha(b: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(b).into()
}

fn meta_fields(body: &[u8]) -> Vec<u8> {
    let mut r = mvdz::util::Rd::new(body);
    let (_, codec, n, _) = (
        r.u8().unwrap(),
        r.u8().unwrap(),
        r.uz().unwrap(),
        r.u32le().unwrap(),
    );
    if codec == 1 {
        mvdz::zst::decompress(r.rest(), n).unwrap()
    } else {
        r.rest().to_vec()
    }
}

fn zstd_meta(p: &[u8]) -> Vec<u8> {
    let mut out = vec![1u8, 1];
    put_uv(&mut out, p.len() as u64);
    out.extend_from_slice(&crc32(p).to_le_bytes());
    out.extend_from_slice(&mvdz::zst::compress(p, 19));
    out
}

/// R-H1: 300 000 extension keys in META: linear, not hours.
#[test]
fn r_h1_meta_ext_keys_linear() {
    let (demo, mut s) = base();
    let mut p = meta_fields(&s[0].1);
    let n = 300_000;
    let mut v = Vec::new();
    put_uv(&mut v, n as u64);
    for i in 0..n {
        let k = key(i, 4);
        put_uv(&mut v, 4);
        v.extend_from_slice(k.as_bytes());
        put_uv(&mut v, 0);
    }
    put_uv(&mut p, T_EXT);
    put_uv(&mut p, v.len() as u64);
    p.extend_from_slice(&v);
    s[0].1 = zstd_meta(&p);
    let f = build(0, true, &s, true);
    let t = Instant::now();
    let fi = mvdz::read_metadata(Cursor::new(&f)).unwrap();
    assert_eq!(fi.meta.ext.len(), n);
    assert_eq!(mvdz::decompress(&f, 1).unwrap(), demo);
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

/// R-H2: 40 000 attachment names and 100 000 keys in one entry: linear.
#[test]
fn r_h2_adir_linear() {
    let (_, s0) = base();
    let mut s = s0.clone();
    let mut d = Vec::new();
    for i in 0..40_000 {
        s.push((K_ATTACH, Vec::new()));
        d.push(AttachInfo {
            name: key(i, 5),
            sha256: sha(&[]),
            ..Default::default()
        });
    }
    s.push((K_ADIR, adir_body(&d)));
    let f = build(0, true, &s, true);
    let t = Instant::now();
    assert_eq!(
        mvdz::read_metadata(Cursor::new(&f))
            .unwrap()
            .attachments
            .len(),
        40_000
    );
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    let mut s = s0;
    let kv: Vec<(String, String)> = (0..100_000).map(|i| (key(i, 4), String::new())).collect();
    s.push((K_ATTACH, Vec::new()));
    s.push((
        K_ADIR,
        adir_body(&[AttachInfo {
            name: "a".into(),
            sha256: sha(&[]),
            kv,
            ..Default::default()
        }]),
    ));
    let f = build(0, true, &s, true);
    let t = Instant::now();
    assert_eq!(
        mvdz::read_metadata(Cursor::new(&f)).unwrap().attachments[0]
            .kv
            .len(),
        100_000
    );
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

/// R-H3, R-M1: the zstd frame rules apply in every build (native libzstd
/// and the pure-Rust decoder of wasm): a window larger than the data, a
/// legacy frame, a dictionary id, the reserved bit, an oversized block.
#[test]
fn r_h3_m1_zstd_frames() {
    let refused = |b: &[u8], n: usize| {
        assert_eq!(
            mvdz::zst::decompress(b, n).unwrap_err(),
            Error::Corrupt("zstd data"),
            "{b:x?}"
        )
    };
    // window 2^40, RLE blocks of 128 KiB: the bomb of the review, cut short
    let mut bomb = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0xF0];
    for _ in 0..64 {
        bomb.extend_from_slice(&((131072u32 << 3) | (1 << 1)).to_le_bytes()[..3]);
        bomb.push(7);
    }
    bomb.extend_from_slice(&(1u32 | (1 << 1) | (1 << 3)).to_le_bytes()[..3]);
    bomb.push(7);
    let t = Instant::now();
    refused(&bomb, 1000);
    assert!(t.elapsed() < Duration::from_secs(1));
    let ok = mvdz::zst::compress(&[5u8; 3000], 3);
    assert_eq!(mvdz::zst::decompress(&ok, 3000).unwrap(), vec![5u8; 3000]);
    // legacy v0.7 magic with otherwise the same frame
    let mut legacy = ok.clone();
    legacy[0] = 0x27;
    refused(&legacy, 3000);
    // the reserved bit
    let mut r = ok.clone();
    r[4] |= 0x08;
    refused(&r, 3000);
    // a dictionary id
    let mut dict = vec![0x28, 0xB5, 0x2F, 0xFD, 0x21, 1, 3];
    dict.extend_from_slice(&(1u32 | (1 << 3)).to_le_bytes()[..3]);
    dict.push(9);
    refused(&dict, 3);
    // a raw block larger than the window (single segment, content size 3)
    let mut big = vec![0x28, 0xB5, 0x2F, 0xFD, 0x20, 3];
    big.extend_from_slice(&(1u32 | (4 << 3)).to_le_bytes()[..3]);
    big.extend_from_slice(b"abcd");
    refused(&big, 4);
    // a window of 1 MiB is fine for small data (the pure-Rust encoder
    // writes 128 KiB windows)
    let mut w = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 10 << 3];
    w.extend_from_slice(&(1u32 | (3 << 3)).to_le_bytes()[..3]);
    w.extend_from_slice(b"xyz");
    assert_eq!(mvdz::zst::decompress(&w, 3).unwrap(), b"xyz");
}

/// R-M2: a model of 65535 tables whose header is padded to satisfy rule
/// M2 is refused before any table is built (the header must be consumed
/// exactly).
#[test]
fn r_m2_padded_model() {
    use mvdz::coder::rc::{Enc, IntModel};
    const NQC: usize = 17 * 18;
    let n = 65535u32;
    let mut e = Enc::new();
    let mut im = IntModel::new(NQC + 8);
    im.put(&mut e, NQC, 256);
    im.put(&mut e, NQC + 1, 2);
    im.put(&mut e, NQC + 2, n);
    let mut prev = 0usize;
    for _ in 0..256 {
        im.put(&mut e, 17 * 17 + prev, 0);
        prev = 0;
    }
    for i in 0..n {
        im.put(&mut e, NQC + 3, (i > 0) as u32);
        for _ in 0..256 {
            im.put(&mut e, 0, 0);
        }
    }
    let dec = e.decisions;
    let mut hdr = e.finish();
    hdr.resize(
        hdr.len().max((dec.saturating_sub(1024) / 128 + 1) as usize),
        0,
    );
    let mut st = Vec::new();
    put_uv(&mut st, hdr.len() as u64);
    st.extend_from_slice(&hdr);
    put_uv(&mut st, 0);
    let raw_len = n as u64 + 100;
    let mut body = Vec::new();
    for v in [
        0u64,
        0,
        0,
        0,
        0,
        0,
        0,
        1,
        0,
        2,
        n as u64 + 16,
        st.len() as u64,
    ] {
        put_uv(&mut body, v);
    }
    body.extend_from_slice(&st);
    let mut b = seg_hdr_bytes(0, raw_len, 0, 0, C_TRANSFORM);
    b.extend_from_slice(&body);
    let (_, s0) = base();
    let mut ix = Index {
        size: raw_len,
        ..Default::default()
    };
    ix.segs.push(IndexSeg {
        raw_len,
        ..Default::default()
    });
    let mut m = Metadata::decode(&s0[0].1, &Budget::unlimited()).unwrap();
    m.size = raw_len;
    m.demoinfo.clear();
    let f = build(
        0,
        true,
        &[
            (K_META, m.encode().unwrap()),
            (K_SEGMENT, b),
            (K_INDEX, ix.encode()),
        ],
        true,
    );
    let t = Instant::now();
    let l: ReadOptions = Limits {
        memory: 64 << 20,
        ..Default::default()
    }
    .into();
    assert_eq!(
        mvdz::decompress_with(&f, 1, &l).unwrap_err(),
        Error::Corrupt("model")
    );
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

/// R-M3: the JSON rendering can be capped.
#[test]
fn r_m3_json_limit() {
    let (_, s) = base();
    let f = build(0, true, &s, true);
    let fi = mvdz::read_metadata(Cursor::new(&f)).unwrap();
    let j = fi.to_json();
    assert_eq!(fi.to_json_with(j.len()).unwrap(), j);
    assert_eq!(
        fi.to_json_with(j.len() - 1).unwrap_err(),
        Error::Limit("metadata rendering")
    );
    let (mut out, mut n) = (std::ptr::null_mut(), 0usize);
    assert_eq!(
        unsafe { mvdz_info_json_memory(f.as_ptr() as *const _, f.len(), 64, &mut out, &mut n) },
        MVDZ_E_LIMIT
    );
}

/// R-M4: a long run of empty segments is charged to the budget: a
/// sequential reader stops with a limit error instead of growing.
#[test]
fn r_m4_section_bookkeeping() {
    let (demo, s0) = base();
    let n = 100_000;
    let mut s = vec![s0[0].clone()];
    for _ in 0..n {
        s.push((K_SEGMENT, seg_hdr_bytes(0, 0, 0, crc32(&[]), C_STORE)));
    }
    for x in &s0[1..] {
        if x.0 == K_INDEX {
            let mut ix = Index::decode(&x.1).unwrap();
            let mut segs = vec![IndexSeg::default(); n];
            segs.extend(ix.segs);
            ix.segs = segs;
            s.push((K_INDEX, ix.encode()));
        } else {
            s.push(x.clone());
        }
    }
    let f = build(0, true, &s, true);
    assert_eq!(mvdz::decompress(&f, 1).unwrap(), demo);
    let l: ReadOptions = Limits {
        memory: 4 << 20,
        ..Default::default()
    }
    .into();
    assert_eq!(
        mvdz::decompress_with(&f, 1, &l).unwrap_err(),
        Error::Limit("memory")
    );
    let mut r = mvdz::Reader::new(Cursor::new(&f), 1, &l).unwrap();
    let mut out = Vec::new();
    assert!(std::io::Read::read_to_end(&mut r, &mut out).is_err());
}

/// R-M5: an attachment update keeps the file's mode, and the temporary
/// file never has wider permissions than the original.
#[cfg(unix)]
#[test]
fn r_m5_update_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let (_, s) = base();
    let dir = std::env::temp_dir().join(format!("mvdz-perm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("g.mvdz");
    std::fs::write(&p, build(0, true, &s, true)).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    let a = Attachment {
        name: "voice/x".into(),
        mime: "audio/ogg".into(),
        kv: vec![],
        data: vec![1; 1000],
    };
    mvdz::update_attachments(&p, &[], &[a]).unwrap();
    assert_eq!(
        std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
    mvdz::update_attachments(&p, &["voice/x"], &[]).unwrap();
    assert_eq!(
        std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let _ = std::fs::remove_dir_all(&dir);
}

struct Reent {
    c: Cursor<Vec<u8>>,
    handle: *mut std::ffi::c_void,
    inner: Vec<i64>,
}

unsafe extern "C" fn re_read(
    user: *mut std::ffi::c_void,
    buf: *mut std::ffi::c_void,
    n: usize,
) -> std::ffi::c_longlong {
    let u = unsafe { &mut *(user as *mut Reent) };
    if !u.handle.is_null() {
        // calls on the handle from inside its own callback
        u.inner.push(unsafe { mvdz_raw_size(u.handle) });
        unsafe { mvdz_close(u.handle) };
    }
    let b = unsafe { std::slice::from_raw_parts_mut(buf as *mut u8, n) };
    std::io::Read::read(&mut u.c, b).map_or(-1, |k| k as i64)
}

/// R-M6: a callback that calls back into its own handle gets an error
/// instead of a deadlock; closing from inside is ignored.
#[test]
fn r_m6_reentrant_callbacks() {
    let (demo, s) = base();
    let f = build(0, true, &s, true);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut u = Box::new(Reent {
            c: Cursor::new(f),
            handle: std::ptr::null_mut(),
            inner: Vec::new(),
        });
        let up = &mut *u as *mut Reent as *mut std::ffi::c_void;
        let mut err = 0;
        let r = unsafe { mvdz_open_callbacks(Some(re_read), None, up, 1, 0, 0, &mut err) };
        assert!(!r.is_null());
        u.handle = r;
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        loop {
            let n = unsafe { mvdz_read(r, buf.as_mut_ptr() as *mut _, buf.len()) };
            assert!(n >= 0, "read {n}");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        unsafe { mvdz_close(r) };
        tx.send((out, u.inner.clone())).unwrap();
    });
    let (out, inner) = rx.recv_timeout(Duration::from_secs(60)).expect("deadlock");
    assert_eq!(out, demo);
    assert!(
        !inner.is_empty() && inner.iter().all(|&x| x == MVDZ_E_INVALID as i64),
        "{inner:?}"
    );
}

/// C-H1: an update whose directory would exceed 2^24 bytes is refused and
/// the file is left as it was.
#[test]
fn c_h1_oversized_directory_refused() {
    let (demo, s) = base();
    let f = build(0, true, &s, true);
    let att = Attachment {
        name: "transcript/a".into(),
        mime: "text/plain".into(),
        kv: vec![("x-note".into(), "x".repeat(17 << 20))],
        data: b"hi".to_vec(),
    };
    assert_eq!(
        mvdz::update_attachments_bytes(&f, &[], std::slice::from_ref(&att)).unwrap_err(),
        Error::Invalid("attachment directory over 16 MiB")
    );
    let dir = std::env::temp_dir().join(format!("mvdz-adir-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("g.mvdz");
    std::fs::write(&p, &f).unwrap();
    assert!(mvdz::update_attachments(&p, &[], &[att]).is_err());
    assert_eq!(std::fs::read(&p).unwrap(), f);
    assert_eq!(mvdz::decompress(&f, 1).unwrap(), demo);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A demo of a dem_read frame holding one svc_nop, then `n` dem_set
/// frames (10 bytes each).
fn sets(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n * 10 + 7);
    v.extend_from_slice(&[0, 1, 1, 0, 0, 0, 1]);
    for i in 0..n {
        v.push(1);
        v.push(2);
        v.extend_from_slice(&(i as u64).to_le_bytes());
    }
    v
}

/// C-M1, C-L2: more segments than the writers allow is refused up front
/// by the one-shot encoder and at the first segment over the limit by the
/// streaming writer, never by writing an INDEX readers refuse.
#[test]
fn c_m1_segment_limit() {
    let demo = sets(MAX_SEGMENTS + 10);
    let t = Instant::now();
    let e = mvdz::compress(&demo, &cfg(Level::Fast, 1)).unwrap_err();
    assert_eq!(
        e,
        Error::Invalid("too many segments: use a larger segment size")
    );
    assert!(t.elapsed() < Duration::from_secs(5));
    let mut w = mvdz::Writer::new(Vec::new(), &cfg(Level::Fast, 1)).unwrap();
    let r = demo.chunks(1 << 16).try_for_each(|c| w.write_demo(c));
    assert_eq!(
        r.unwrap_err(),
        Error::Invalid("too many segments: use a larger segment size")
    );
    // at the limit itself, the file is written and decodes
    let demo = sets(2000);
    let c = mvdz::compress(&demo, &cfg(Level::Fast, 1)).unwrap();
    assert_eq!(mvdz::decompress(&c, 1).unwrap(), demo);
}

/// C-L1: extension metadata is checked when the writer is created.
#[test]
fn c_l1_writer_checks_ext() {
    let big = Config {
        ext: vec![("k".into(), vec![b'v'; 2 << 20])],
        ..cfg(Level::Fast, 4096)
    };
    assert_eq!(
        mvdz::Writer::new(Vec::new(), &big).err(),
        Some(Error::Invalid("extension metadata over 1 MiB"))
    );
    let dup = Config {
        ext: vec![("k".into(), b"a".to_vec()), ("k".into(), b"b".to_vec())],
        ..cfg(Level::Fast, 4096)
    };
    assert_eq!(
        mvdz::Writer::new(Vec::new(), &dup).err(),
        Some(Error::Invalid("duplicate extension key"))
    );
    assert!(mvdz::compress(b"", &dup).is_err());
}

/// Attachment keys need a prefix (`x-` or `vendor.`): unprefixed keys are
/// reserved for future versions (FORMAT.md §10.1). Readers keep any key.
#[test]
fn key_namespace() {
    let (_, s) = base();
    let f = build(0, true, &s, true);
    let att = |k: &str| Attachment {
        name: "x".into(),
        mime: String::new(),
        kv: vec![(k.into(), "v".into())],
        data: vec![1],
    };
    for k in ["role", "visibility", "", ".x", "x_y", "X-upper"] {
        assert!(
            mvdz::update_attachments_bytes(&f, &[], &[att(k)]).is_err(),
            "{k:?} accepted"
        );
    }
    for k in ["x-role", "mvda.schema", "a.b.c", "x-"] {
        assert!(
            mvdz::update_attachments_bytes(&f, &[], &[att(k)]).is_ok(),
            "{k:?} refused"
        );
    }
    // an unprefixed key written by someone else is read and preserved
    let mut s2 = s.clone();
    s2.push((K_ATTACH, vec![1]));
    s2.push((
        K_ADIR,
        adir_body(&[AttachInfo {
            name: "old".into(),
            kv: vec![("role".into(), "voice".into())],
            size: 1,
            stored: 1,
            sha256: sha(&[1]),
            ..Default::default()
        }]),
    ));
    let g = build(0, true, &s2, true);
    let g2 = mvdz::update_attachments_bytes(&g, &[], &[att("x-new")]).unwrap();
    let fi = mvdz::read_metadata(Cursor::new(&g2)).unwrap();
    assert_eq!(
        fi.attachments[0].kv,
        vec![("role".to_string(), "voice".to_string())]
    );
}

/// D3, D9: the transform flags are checked by every reader at open.
#[test]
fn d3_flags_at_open() {
    let b = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/vectors/hostile-segment-flags.mvdz"
    ))
    .unwrap();
    assert_eq!(
        mvdz::read_metadata(Cursor::new(&b)).unwrap_err(),
        Error::Corrupt("segment flags")
    );
    let b = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/vectors/hostile-share-empty.mvdz"
    ))
    .unwrap();
    assert_eq!(
        mvdz::read_metadata(Cursor::new(&b)).unwrap_err(),
        Error::Corrupt("demoinfo share")
    );
}

/// Trusted decoding skips the SHA-256 only: a forged identity passes, a
/// damaged segment does not, in every reader.
#[test]
fn trusted_mode() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    let forged = std::fs::read(dir.join("hostile-forged-identity.mvdz")).unwrap();
    let t = ReadOptions {
        trusted: true,
        ..Default::default()
    };
    assert_eq!(
        mvdz::decompress(&forged, 1).unwrap_err(),
        Error::Corrupt("identity mismatch")
    );
    let d = mvdz::decompress_with(&forged, 1, &t).unwrap();
    let mut r = mvdz::Reader::new(Cursor::new(&forged), 1, &t).unwrap();
    let mut o = Vec::new();
    std::io::Read::read_to_end(&mut r, &mut o).unwrap();
    assert_eq!(o, d);
    let (mut out, mut n) = (std::ptr::null_mut(), 0usize);
    assert_eq!(
        unsafe {
            mvdz_decompress(
                forged.as_ptr() as *const _,
                forged.len(),
                1,
                0,
                MVDZ_TRUSTED,
                &mut out,
                &mut n,
            )
        },
        MVDZ_OK
    );
    assert_eq!(unsafe { std::slice::from_raw_parts(out, n) }, &d[..]);
    unsafe { mvdz_free(out as *mut _, n) };
    // one byte of a stored segment changed: the segment CRC catches it
    let demo = synthetic(&[], false);
    let (c, _) = mvdz::encode::compress_untransformed(
        &[demo.clone(), vec![0; 5000]].concat(),
        &cfg(Level::Normal, 0),
    )
    .unwrap();
    let ok = mvdz::decompress_with(&c, 1, &t).unwrap();
    let mut bad = c.clone();
    let i = (0..bad.len())
        .rev()
        .find(|&i| bad[i] != 0 && i > 200)
        .unwrap();
    bad[i - 100] ^= 0x40;
    assert!(mvdz::decompress_with(&bad, 1, &t).is_err());
    assert!(!ok.is_empty());
}

/// The bundle hash: independent of level, segmenting and attachment
/// order; changes with any attachment name, MIME, content or key; matches
/// the canonical encoding of FORMAT.md §10.2.
#[test]
fn bundle_hash() {
    let demo = synthetic(&[], false);
    let a = Attachment {
        name: "voice/a".into(),
        mime: "audio/ogg".into(),
        kv: vec![
            ("x-role".into(), "voice".into()),
            ("x-player".into(), "a".into()),
        ],
        data: vec![7; 5000],
    };
    let b = Attachment {
        name: "transcript/a".into(),
        mime: "text/vtt".into(),
        kv: vec![("x-ref".into(), "voice/a".into())],
        data: b"WEBVTT\n".to_vec(),
    };
    let bh = |f: &[u8]| mvdz::read_metadata(Cursor::new(f)).unwrap().bundle_hash();
    let x = mvdz::update_attachments_bytes(
        &mvdz::compress(&demo, &cfg(Level::Normal, 0)).unwrap(),
        &[],
        &[b.clone(), a.clone()],
    )
    .unwrap();
    let y = mvdz::compress(&demo, &cfg(Level::Fast, 1000)).unwrap();
    let y = mvdz::update_attachments_bytes(
        &y,
        &[],
        &[Attachment {
            kv: vec![a.kv[1].clone(), a.kv[0].clone()],
            ..a.clone()
        }],
    )
    .unwrap();
    let y = mvdz::update_attachments_bytes(&y, &[], std::slice::from_ref(&b)).unwrap();
    assert_ne!(x, y);
    assert_eq!(bh(&x), bh(&y));
    let z = mvdz::update_attachments_bytes(
        &x,
        &[],
        &[Attachment {
            mime: "audio/opus".into(),
            ..a.clone()
        }],
    )
    .unwrap();
    assert_ne!(bh(&x), bh(&z));
    let none = mvdz::compress(&demo, &cfg(Level::Normal, 0)).unwrap();
    // canonical encoding, by hand
    let mut m = b"MVDZBND1".to_vec();
    m.extend_from_slice(&sha(&demo));
    m.push(2);
    for (name, mime, data, kv) in [
        (
            "transcript/a",
            "text/vtt",
            &b.data,
            vec![("x-ref", "voice/a")],
        ),
        (
            "voice/a",
            "audio/ogg",
            &a.data,
            vec![("x-player", "a"), ("x-role", "voice")],
        ),
    ] {
        m.push(name.len() as u8);
        m.extend_from_slice(name.as_bytes());
        m.push(mime.len() as u8);
        m.extend_from_slice(mime.as_bytes());
        m.extend_from_slice(&sha(data));
        m.push(kv.len() as u8);
        for (k, v) in kv {
            m.push(k.len() as u8);
            m.extend_from_slice(k.as_bytes());
            m.push(v.len() as u8);
            m.extend_from_slice(v.as_bytes());
        }
    }
    assert_eq!(bh(&x), sha(&m));
    let mut e = b"MVDZBND1".to_vec();
    e.extend_from_slice(&sha(&demo));
    e.push(0);
    assert_eq!(bh(&none), sha(&e));
    let mut h = [0u8; 32];
    assert_eq!(
        unsafe { mvdz_bundle_hash_memory(x.as_ptr() as *const _, x.len(), 0, h.as_mut_ptr()) },
        MVDZ_OK
    );
    assert_eq!(h, bh(&x));
    assert!(
        mvdz::read_metadata(Cursor::new(&x))
            .unwrap()
            .to_json()
            .contains(&mvdz::api::hex(&h))
    );
}

unsafe extern "C" fn w_vec(
    user: *mut std::ffi::c_void,
    buf: *const std::ffi::c_void,
    n: usize,
) -> std::ffi::c_longlong {
    let v = unsafe { &mut *(user as *mut Vec<u8>) };
    v.extend_from_slice(unsafe { std::slice::from_raw_parts(buf as *const u8, n) });
    n as i64
}

unsafe extern "C" fn r_cur(
    user: *mut std::ffi::c_void,
    buf: *mut std::ffi::c_void,
    n: usize,
) -> std::ffi::c_longlong {
    let c = unsafe { &mut *(user as *mut Cursor<Vec<u8>>) };
    std::io::Read::read(c, unsafe {
        std::slice::from_raw_parts_mut(buf as *mut u8, n)
    })
    .map_or(-1, |k| k as i64)
}

unsafe extern "C" fn s_cur(
    user: *mut std::ffi::c_void,
    off: std::ffi::c_longlong,
    wh: std::ffi::c_int,
) -> std::ffi::c_longlong {
    use std::io::{Seek, SeekFrom};
    let c = unsafe { &mut *(user as *mut Cursor<Vec<u8>>) };
    let p = match wh {
        0 => SeekFrom::Start(off as u64),
        1 => SeekFrom::Current(off),
        _ => SeekFrom::End(off),
    };
    c.seek(p).map_or(-1, |v| v as i64)
}

/// Export: a copy with only some attachments, selected by name and name
/// prefix; the demo part and the kept attachments byte for byte; the
/// bundle hash of the output; keeping none gives the bare file; keeping
/// all gives the source; every reader accepts the result.
#[test]
fn export_attachments() {
    let demo = synthetic(&[(1, "{\"a\":"), (0, "1}")], false);
    let bare = mvdz::compress(&demo, &cfg(Level::Normal, 2000)).unwrap();
    let att = |name: &str, n: u8| Attachment {
        name: name.into(),
        mime: "application/octet-stream".into(),
        kv: vec![("x-n".into(), n.to_string())],
        data: vec![n; 3000 + n as usize],
    };
    let atts = [
        att("big/a", 1),
        att("big/b", 2),
        att("mvda.result", 3),
        att("notes.txt", 4),
        att("shot.png", 5),
    ];
    let full = mvdz::update_attachments_bytes(&bare, &[], &atts).unwrap();
    let src = full.clone();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let sel = |x: mvdz::Selection| {
        let mut o = Vec::new();
        let h = mvdz::export_attachments(Cursor::new(&full), &mut o, |e| x.matches(e)).unwrap();
        (o, h)
    };
    let names = |o: &[u8]| {
        mvdz::read_metadata(Cursor::new(o))
            .unwrap()
            .attachments
            .into_iter()
            .map(|a| a.name)
            .collect::<Vec<_>>()
    };
    // no condition: everything, byte for byte
    assert_eq!(sel(mvdz::Selection::default()).0, full);
    // drop by prefix and name
    let (out, h) = sel(mvdz::Selection {
        drop_prefix: s(&["big/"]),
        drop: s(&["shot.png"]),
        ..Default::default()
    });
    assert_eq!(full, src, "the source is only read");
    assert_eq!(names(&out), s(&["mvda.result", "notes.txt"]));
    let fi = mvdz::read_metadata(Cursor::new(&out)).unwrap();
    assert_eq!(h, fi.bundle_hash());
    let kept: Vec<AttachInfo> = mvdz::read_metadata(Cursor::new(&full))
        .unwrap()
        .attachments
        .into_iter()
        .filter(|a| a.name == "mvda.result" || a.name == "notes.txt")
        .collect();
    assert_eq!(h, mvdz::api::bundle_hash(&sha(&demo), &kept));
    assert_eq!(readers_agree(&out, "export").unwrap(), demo);
    for a in &atts {
        let r = mvdz::read_attachment(Cursor::new(&out), &a.name);
        if fi.attachments.iter().any(|x| x.name == a.name) {
            assert_eq!(r.unwrap(), a.data);
        } else {
            assert_eq!(r.unwrap_err(), Error::NotFound);
        }
    }
    // the same as removing the others in place
    assert_eq!(
        out,
        mvdz::update_attachments_bytes(&full, &["big/a", "big/b", "shot.png"], &[]).unwrap()
    );
    // keep by name and prefix, then drop within them
    assert_eq!(
        names(
            &sel(mvdz::Selection {
                keep: s(&["shot.png"]),
                keep_prefix: s(&["big/"]),
                drop: s(&["big/b"]),
                ..Default::default()
            })
            .0
        ),
        s(&["big/a", "shot.png"])
    );
    // keeping none: the bare file, byte for byte
    let (none, hn) = sel(mvdz::Selection {
        drop_prefix: s(&[""]),
        ..Default::default()
    });
    assert_eq!(none, bare);
    assert_eq!(
        hn,
        mvdz::read_metadata(Cursor::new(&bare))
            .unwrap()
            .bundle_hash()
    );
    // the C interface
    let mut cur = Cursor::new(full.clone());
    let mut cout: Vec<u8> = Vec::new();
    let kp = [c"big/".as_ptr()];
    let dr = [c"big/b".as_ptr()];
    let cs = MvdzSelection {
        keep: std::ptr::null(),
        nkeep: 0,
        keep_prefix: kp.as_ptr(),
        nkeep_prefix: 1,
        drop: dr.as_ptr(),
        ndrop: 1,
        drop_prefix: std::ptr::null(),
        ndrop_prefix: 0,
    };
    let mut hb = [0u8; 32];
    let rc = unsafe {
        mvdz_attachments_export(
            Some(r_cur),
            Some(s_cur),
            &mut cur as *mut _ as *mut _,
            Some(w_vec),
            &mut cout as *mut _ as *mut _,
            &cs,
            hb.as_mut_ptr(),
        )
    };
    assert_eq!(rc, MVDZ_OK);
    assert_eq!(names(&cout), s(&["big/a"]));
    assert_eq!(
        hb,
        mvdz::read_metadata(Cursor::new(&cout))
            .unwrap()
            .bundle_hash()
    );
    let mut cur = Cursor::new(full.clone());
    let mut all: Vec<u8> = Vec::new();
    let rc = unsafe {
        mvdz_attachments_export(
            Some(r_cur),
            Some(s_cur),
            &mut cur as *mut _ as *mut _,
            Some(w_vec),
            &mut all as *mut _ as *mut _,
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, MVDZ_OK);
    assert_eq!(all, full);
}

/// Pre-release review: unknown sections and non-minimal encodings in the
/// attachment part. Keep-all export and a no-op update copy the file as it
/// is; an export or update that changes the attachment set rebuilds the
/// attachment part (dropping unknown sections there); unknown sections in
/// the demo part always survive.
#[test]
fn attachment_part_rebuild_rule() {
    let demo = synthetic(&[(1, "{\"a\":"), (0, "1}")], false);
    let bare = mvdz::compress(&demo, &cfg(Level::Normal, 2000)).unwrap();
    let att = |name: &str, n: u8| Attachment {
        name: name.into(),
        mime: "m".into(),
        kv: vec![("x-n".into(), n.to_string())],
        data: vec![n; 3000],
    };
    let full = mvdz::update_attachments_bytes(&bare, &[], &[att("a", 1), att("b", 2)]).unwrap();
    let export = |f: &[u8], keep: &dyn Fn(&AttachInfo) -> bool| {
        let mut o = Vec::new();
        mvdz::export_attachments(Cursor::new(f), &mut o, keep).unwrap();
        o
    };
    let names = |f: &[u8]| {
        mvdz::read_metadata(Cursor::new(f))
            .unwrap()
            .attachments
            .into_iter()
            .map(|a| a.name)
            .collect::<Vec<_>>()
    };
    let kinds = |f: &[u8]| sections(f).into_iter().map(|s| s.0).collect::<Vec<_>>();
    let mut s = sections(&full);
    // unknown sections: one in the demo part, one between the ATTACHes,
    // one after the ADIR
    let ia = s.iter().position(|x| x.0 == K_ATTACH).unwrap();
    s.insert(ia + 1, (0x40, b"between".to_vec()));
    let id = s.iter().position(|x| x.0 == K_ADIR).unwrap();
    s.insert(id + 1, (0x41, b"after the directory".to_vec()));
    let ix = s.iter().position(|x| x.0 == K_INDEX).unwrap();
    s.insert(ix, (0x42, b"demo part".to_vec()));
    let f = build(full[5], true, &s, true);
    assert_eq!(readers_agree(&f, "unknown sections").unwrap(), demo);
    // keep-all export and no-op updates: byte for byte
    assert_eq!(export(&f, &|_| true), f);
    assert_eq!(mvdz::update_attachments_bytes(&f, &[], &[]).unwrap(), f);
    assert_eq!(
        mvdz::update_attachments_bytes(&f, &["absent"], &[]).unwrap(),
        f
    );
    // a change rebuilds the attachment part: 0x40 and 0x41 go, 0x42 stays
    for out in [
        export(&f, &|e| e.name == "a"),
        mvdz::update_attachments_bytes(&f, &["b"], &[]).unwrap(),
    ] {
        assert_eq!(names(&out), ["a"]);
        let k = kinds(&out);
        assert!(
            k.contains(&0x42) && !k.contains(&0x40) && !k.contains(&0x41),
            "{k:x?}"
        );
        assert_eq!(readers_agree(&out, "rebuilt").unwrap(), demo);
    }
    // a non-minimal varint in the directory: kept as is by keep-all, and
    // re-encoded by a change
    let mut s = sections(&full);
    let id = s.iter().position(|x| x.0 == K_ADIR).unwrap();
    let mut d = vec![0x82, 0x00];
    d.extend_from_slice(&s[id].1[1..]);
    s[id].1 = d;
    let mut g = build(full[5], true, &s, false);
    g.extend_from_slice(&full[full.len() - TRAILER_LEN..]);
    assert_eq!(names(&g), ["a", "b"]);
    assert_eq!(export(&g, &|_| true), g);
    let o = export(&g, &|e| e.name == "b");
    assert_eq!(names(&o), ["b"]);
    assert_eq!(readers_agree(&o, "re-encoded").unwrap(), demo);
}

/// C decode calls refuse flag bits they do not define.
#[test]
fn c_unknown_flags() {
    let (_, s) = base();
    let f = build(0, true, &s, true);
    let (mut out, mut n) = (std::ptr::null_mut(), 0usize);
    assert_eq!(
        unsafe { mvdz_decompress(f.as_ptr() as *const _, f.len(), 1, 0, 2, &mut out, &mut n) },
        MVDZ_E_INVALID
    );
    assert_eq!(
        unsafe {
            mvdz_decompress(
                f.as_ptr() as *const _,
                f.len(),
                1,
                0,
                MVDZ_TRUSTED,
                &mut out,
                &mut n,
            )
        },
        MVDZ_OK
    );
    unsafe { mvdz_free(out as *mut _, n) };
    let mut err = 0;
    assert!(
        unsafe { mvdz_open_memory(f.as_ptr() as *const _, f.len(), 1, 0, 1 << 31, &mut err) }
            .is_null()
    );
    assert_eq!(err, MVDZ_E_INVALID);
}
