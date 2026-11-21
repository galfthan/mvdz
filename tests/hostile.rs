//! Hostile files: each one is refused (or accepted) by every reader alike,
//! with the expected error, quickly and within the memory budget. These
//! are the reviewers' proofs of concept rebuilt for this format.

mod common;
use common::*;
use mvdz::format::*;
use mvdz::util::put_uv;
use mvdz::{Error, Level};
use std::io::Cursor;
use std::time::Instant;

fn base(level: Level, seg: usize) -> (Vec<u8>, Vec<u8>) {
    let demo = synthetic(&[(1, "{\"a\":"), (0, "1}")], false);
    let c = mvdz::compress(&demo, &cfg(level, seg)).unwrap();
    (demo, c)
}

fn meta_body(m: &Metadata) -> Vec<u8> {
    m.encode().unwrap()
}

/// Refused by every reader within `secs`, with `want` matching the error.
fn refused(b: &[u8], what: &str, want: impl Fn(&Error) -> bool) {
    let t = Instant::now();
    assert!(readers_agree(b, what).is_none(), "{what}: accepted");
    let e = mvdz::decompress(b, 2).unwrap_err();
    assert!(want(&e), "{what}: unexpected error {e}");
    assert!(
        t.elapsed().as_secs_f64() < 5.0,
        "{what}: took {:?}",
        t.elapsed()
    );
}

fn corrupt(e: &Error) -> bool {
    matches!(e, Error::Corrupt(_))
}

#[test]
fn structure_rules() {
    let (demo, c) = base(Level::Normal, 300);
    assert_eq!(readers_agree(&c, "base").unwrap(), demo);
    let secs = sections(&c);
    let kinds: Vec<u8> = secs.iter().map(|s| s.0).collect();
    assert_eq!(kinds[0], K_META);
    let meta = secs[0].clone();
    let iix = kinds.iter().position(|&k| k == K_INDEX).unwrap();
    // rebuilt unchanged: accepted
    assert_eq!(
        readers_agree(&build(0, true, &secs, true), "rebuilt").unwrap(),
        demo
    );
    // metadata pointer 0 in the prefix: the trailer's is used
    assert_eq!(
        readers_agree(&build(0, false, &secs, true), "prefix meta 0").unwrap(),
        demo
    );
    // a second META (forged identity) anywhere: refused (robustness 9)
    let mut forged = Metadata::decode(&meta.1, &mvdz::budget::Budget::unlimited()).unwrap();
    forged.sha256 = [0xff; 32];
    let mut s2 = secs.clone();
    s2.insert(iix, (K_META, meta_body(&forged)));
    refused(&build(0, true, &s2, true), "second META", |e| {
        *e == Error::Corrupt("duplicate META")
    });
    // the only META replaced by a forged one: the structure is fine, the
    // identity check of a full decode fails
    let mut s3 = secs.clone();
    s3[0] = (K_META, meta_body(&forged));
    let b = build(0, true, &s3, true);
    refused(&b, "forged identity", |e| {
        *e == Error::Corrupt("identity mismatch")
    });
    assert!(
        mvdz::read_metadata(Cursor::new(&b)).is_ok(),
        "read_metadata does not decode the demo"
    );
    // a second INDEX
    let mut s4 = secs.clone();
    s4.insert(iix + 1, secs[iix].clone());
    refused(&build(0, true, &s4, true), "second INDEX", corrupt);
    // a segment dropped by flipping its kind to an unknown non-critical one
    // (spec review 3)
    let mut s5 = secs.clone();
    s5[2].0 = 0x08;
    refused(&build(0, true, &s5, true), "dropped segment", corrupt);
    // segments swapped
    let mut s6 = secs.clone();
    s6.swap(1, 2);
    refused(&build(0, true, &s6, true), "swapped segments", corrupt);
    // INDEX before the last segment
    let mut s7 = secs.clone();
    let ix = s7.remove(iix);
    s7.insert(2, ix);
    refused(&build(0, true, &s7, true), "index first", corrupt);
    // no trailer, with an index (spec review 4): incomplete everywhere
    let b = build(0, true, &secs, false);
    refused(&b, "no trailer", |e| {
        *e == Error::Corrupt("incomplete file (no trailer)")
    });
    // the trailer cut short
    refused(&c[..c.len() - 3], "trailer cut", corrupt);
    // data after the trailer
    let mut b = c.clone();
    b.push(0);
    refused(&b, "data after trailer", corrupt);
    // a TRAILER section in the middle
    let mut s8 = secs.clone();
    s8.insert(1, (K_TRAILER, vec![0; 24]));
    refused(&build(0, true, &s8, true), "early trailer", corrupt);
    // a trailer that names the wrong sections
    let t = c.len() - TRAILER_LEN + SEC_HDR;
    for (at, what) in [(0, "wrong index_off"), (8, "wrong meta_off")] {
        let mut b = c.clone();
        b[t + at] ^= 1;
        refused(&b, what, corrupt);
    }
    // the 40-byte trailer of earlier release candidates
    let mut b = c[..c.len() - TRAILER_LEN].to_vec();
    let mut body = c[t..t + 16].to_vec();
    body.extend_from_slice(&[0; 16]);
    body.extend_from_slice(TRAILER_MAGIC);
    section(&mut b, K_TRAILER, &body);
    refused(&b, "40-byte trailer", corrupt);
    // an unknown critical section
    let mut s9 = secs.clone();
    s9.insert(1, (0x99, b"x".to_vec()));
    refused(&build(0, true, &s9, true), "unknown critical", |e| {
        matches!(e, Error::Format(_))
    });
    // a segment that needs the demoinfo before the META
    let mut s10 = secs.clone();
    let m = s10.remove(0);
    s10.insert(iix - 1, m);
    refused(
        &build(0, true, &s10, true),
        "META after a sharing segment",
        corrupt,
    );
    // META without the shared demoinfo, CRC recomputed
    let mut nodi = Metadata::decode(&meta.1, &mvdz::budget::Budget::unlimited()).unwrap();
    nodi.demoinfo.clear();
    let mut s11 = secs.clone();
    s11[0] = (K_META, meta_body(&nodi));
    refused(&build(0, true, &s11, true), "demoinfo missing", corrupt);
    // empty file, magic only, other versions
    for (b, what) in [
        (&b""[..], "empty"),
        (b"MVDZ", "magic"),
        (b"MVDZ\x03\x00\x00\x00", "v3"),
        (b"MVDZ\x01\x00", "prefix cut"),
    ] {
        assert!(readers_agree(b, what).is_none());
    }
}

/// Declared sizes never reach the allocator unbounded (robustness 3, 4,
/// 12; correctness F4): each is refused by a format rule or the budget.
#[test]
fn declared_sizes() {
    let (_, c) = base(Level::Fast, 0);
    let secs = sections(&c);
    let iseg = secs.iter().position(|s| s.0 == K_SEGMENT).unwrap();
    let iix = secs.iter().position(|s| s.0 == K_INDEX).unwrap();
    // a 46-byte segment that claims 4 GiB: zstd coding, raw_len 2^32 - 2^16
    let mut s = secs.clone();
    let mut b = seg_hdr_bytes(0, MAX_SEG_RAW, 0, 0, C_ZSTD);
    b.extend_from_slice(&[0x28, 0xb5, 0x2f, 0xfd, 0, 0, 0, 0, 0]);
    s[iseg] = (K_SEGMENT, b);
    refused(&build(0, true, &s, true), "4 GiB segment", |e| {
        *e == Error::Corrupt("segment size")
    });
    // an INDEX claiming 60 GB
    let mut s = secs.clone();
    let mut ix = Index::decode(&s[iix].1).unwrap();
    ix.size = 60 << 30;
    ix.segs[0].raw_len = 60 << 30;
    s[iix].1 = ix.encode();
    refused(&build(0, true, &s, true), "60 GB index", corrupt);
    // META of 256 MiB declared in a few bytes
    let mut mb = vec![1u8, 1];
    put_uv(&mut mb, 256 << 20);
    mb.extend_from_slice(&[0; 4]);
    mb.extend_from_slice(&mvdz::zst::compress(&[0u8; 1000], 3));
    let mut s = secs.clone();
    s[0] = (K_META, mb);
    refused(&build(0, true, &s, true), "256 MiB META", corrupt);
    // the caller's memory budget
    let (demo, c) = base(Level::Normal, 0);
    let small: mvdz::ReadOptions = mvdz::Limits {
        memory: demo.len() as u64 / 2,
        ..Default::default()
    }
    .into();
    assert!(matches!(
        mvdz::decompress_with(&c, 1, &small),
        Err(Error::Limit(_))
    ));
    assert!(
        mvdz::Reader::open(Cursor::new(&c), &small)
            .and_then(|mut r| r.read_demo(&mut [0; 10]))
            .is_err()
    );
}

/// The reviewer's amplifying v2 model headers (robustness 5: a 2.3 KB
/// header took 1.3 GB and 3.6 s) inside a segment: refused at once by the
/// model rules (tables <= symbols, decisions <= 128 per header byte + 1024).
#[test]
fn model_amplification() {
    let Some(dir) = test_data("poc") else { return };
    let demo = synthetic(&[], false);
    let c = mvdz::compress(&demo, &cfg(Level::Normal, 0)).unwrap();
    let secs = sections(&c);
    let iseg = secs.iter().position(|s| s.0 == K_SEGMENT).unwrap();
    for name in ["v2model_1000.bin", "v2model_16000.bin", "v2model_65535.bin"] {
        // a rANS stream body: header length, header, rANS data
        let Ok(st) = std::fs::read(dir.join(name)) else {
            continue;
        };
        // a segment whose only stream is a time stream with this body
        let mut body = Vec::new();
        for v in [0u64, 0, 0, 0, 0, 0, 0] {
            put_uv(&mut body, v);
        }
        put_uv(&mut body, 1);
        for v in [0u64, 2, demo.len() as u64, st.len() as u64] {
            put_uv(&mut body, v);
        }
        body.extend_from_slice(&st);
        let mut b = seg_hdr_bytes(
            0,
            demo.len() as u64,
            0,
            mvdz::util::crc32(&demo),
            C_TRANSFORM,
        );
        b.extend_from_slice(&body);
        let mut s = secs.clone();
        s[iseg] = (K_SEGMENT, b);
        refused(&build(0, true, &s, true), name, |e| {
            *e == Error::Corrupt("model")
        });
    }
}

/// Mutated files: every reader decides the same way (spec review items
/// 2-4, robustness 13).
#[test]
fn mutations_agree() {
    let mut rng = Rng(0x5eed);
    for (level, seg) in [(Level::Normal, 0), (Level::Normal, 300), (Level::Fast, 0)] {
        let (_, c) = base(level, seg);
        // with a section every reader skips, before the trailer
        let cut = c.len() - TRAILER_LEN;
        let mut with = c[..cut].to_vec();
        section(&mut with, 0x40, &b"future".repeat(25));
        with.extend_from_slice(&c[cut..]);
        let c = with;
        for _ in 0..150 {
            let mut b = c.clone();
            match rng.below(3) {
                0 => {
                    for _ in 0..1 + rng.below(3) {
                        let k = rng.below(b.len());
                        b[k] ^= 1 << rng.below(8);
                    }
                }
                1 => b.truncate(rng.below(b.len())),
                _ => {
                    let k = rng.below(b.len());
                    b.insert(k, rng.next() as u8);
                }
            }
            readers_agree(&b, "mutation");
        }
    }
}

/// A model header (FORMAT.md §8.3) of `n` own tables and the fallback,
/// each with all 256 symbols present and pseudo-random q values: legal
/// (rule M2 holds), and every table takes a 16 KiB slot LUT once built.
fn wide_model(n: u32) -> Vec<u8> {
    use mvdz::coder::rc::{Enc, IntModel};
    const NQC: usize = 306;
    let mut e = Enc::new();
    let mut im = IntModel::new(NQC + 8);
    let mut x = 1u32;
    let mut q = || {
        x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        1 + (x >> 16) % 12
    };
    im.put(&mut e, NQC, 256);
    im.put(&mut e, NQC + 1, 2);
    im.put(&mut e, NQC + 2, n);
    let fb: Vec<u32> = (0..256).map(|_| q()).collect();
    let mut prev = 0;
    for &v in &fb {
        im.put(&mut e, 17 * 17 + prev.min(16) as usize, v);
        prev = v;
    }
    for t in 0..n {
        im.put(&mut e, NQC + 3, (t > 0) as u32);
        let mut prev = 0u32;
        for &f in &fb {
            let v = q();
            im.put(&mut e, f.min(16) as usize * 17 + prev.min(16) as usize, v);
            prev = v;
        }
    }
    e.finish()
}

/// A large, legal TABLES section: the metadata path checks
/// its grammar only (quickly, without building models: a 32 MiB budget is
/// enough), while a decode builds its models under the reader's budget,
/// which refuses them.
#[test]
fn large_tables() {
    let demo = usercmd_demo();
    let c = mvdz::compress(&demo, &cfg(Level::Normal, 8192)).unwrap();
    let mut secs = sections(&c);
    assert_eq!(secs[1].0, K_TABLES);
    let m = wide_model(512);
    let mut t = Vec::new();
    put_uv(&mut t, mvdz::streams::N_LAZY as u64);
    for sid in 0..mvdz::streams::N_LAZY {
        put_uv(&mut t, sid as u64);
        put_uv(&mut t, m.len() as u64);
        t.extend_from_slice(&m);
    }
    secs[1].1 = t;
    let b = build(0, true, &secs, true);
    let small = mvdz::Limits {
        memory: 32 << 20,
        ..Default::default()
    };
    let t0 = Instant::now();
    mvdz::read_metadata_with(Cursor::new(&b), &small).unwrap();
    assert!(
        t0.elapsed().as_secs_f64() < 10.0,
        "TABLES checks took {:?}",
        t0.elapsed()
    );
    // the models: 35 x 513 LUTs of 16 KiB, about 280 MiB
    let lim = mvdz::Limits {
        memory: 128 << 20,
        ..Default::default()
    };
    let e = mvdz::decompress_with(&b, 1, &lim.into()).unwrap_err();
    assert_eq!(e, Error::Limit("memory"));
}
