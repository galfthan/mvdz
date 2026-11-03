//! Golden vectors (tests/vectors, MANIFEST.tsv): every valid file must
//! decode to the recorded SHA-256 through every reader; every hostile file
//! must be refused by every reader with the recorded error. These are the
//! cross-implementation vectors (FORMAT.md).
//!
//! `vectors_reproduce` rebuilds every vector from its source (the sample
//! demos of the local cache, or the synthetic demos below) and requires the
//! encoder's output to equal the committed file byte for byte. With
//! `MVDZ_WRITE_VECTORS=1` it writes the files and the manifest instead.

mod common;
use common::*;
use mvdz::format::*;
use mvdz::util::put_uv;
use mvdz::{Attachment, Config, Level};
use sha2::{Digest, Sha256};

fn dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors")
}

fn manifest() -> Vec<Vec<String>> {
    std::fs::read_to_string(dir().join("MANIFEST.tsv"))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| l.split('\t').map(String::from).collect())
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn vectors_decode() {
    let m = manifest();
    assert!(m.len() >= 30, "the vector set is committed");
    for row in m {
        let (file, expect) = (&row[0], &row[1]);
        let b = std::fs::read(dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
        let got = readers_agree(&b, file);
        if let Some(err) = expect.strip_prefix("error: ") {
            assert!(got.is_none(), "{file}: accepted");
            assert_eq!(
                mvdz::decompress(&b, 1).unwrap_err().to_string(),
                err,
                "{file}: error"
            );
        } else {
            let d = got
                .unwrap_or_else(|| panic!("{file}: refused: {:?}", mvdz::decompress(&b, 1).err()));
            assert_eq!(&hex(&Sha256::digest(&d)), expect, "{file}: decoded SHA-256");
            assert_eq!(d.len().to_string(), row[2], "{file}: decoded size");
        }
    }
}

// ---------------------------------------------------------------- the vector set

/// (file, bytes, decoded demo or None for a hostile file, how it was made)
type Vector = (String, Vec<u8>, Option<Vec<u8>>, String);

fn c(level: Level, seg: usize) -> Config {
    cfg(level, seg)
}

/// A demo with escapes: unknown commands, a command that does not fit, and
/// hidden blocks with lying lengths.
fn escapes() -> Vec<u8> {
    let mut d = Demo(synthetic(&[], false));
    for i in 0..30u8 {
        let mut p = s(8, &[&[2], &z("before\n")]);
        p.extend_from_slice(&[0xEE, i, 1, 2, 3]);
        d.frame(1, 6, &p);
        d.frame(0, 6, &[14, 1, 2]);
        let mut h = Vec::new();
        h.extend_from_slice(&1000u32.to_le_bytes());
        h.extend_from_slice(&7u16.to_le_bytes());
        h.extend_from_slice(&[i; 9]);
        d.0.push(2);
        d.0.push(3);
        d.0.extend_from_slice(&0u32.to_le_bytes());
        d.0.extend_from_slice(&(h.len() as u32).to_le_bytes());
        d.0.extend_from_slice(&h);
    }
    d.0
}

fn multi_demoinfo() -> Vec<u8> {
    let parts = [
        (1, "{\"a\":1,"),
        (2, "\"b\":2"),
        (0, "}"),
        (1, "{\"a\":3,"),
        (2, "\"b\":4"),
        (0, "}"),
    ];
    let mut d = synthetic(&[], false);
    d.extend(synthetic(&parts[..3], false));
    d.extend(synthetic(&parts[3..], false));
    d
}

/// A model header of `n` tables with every symbol present: amplifies
/// cheaply under an adaptive coder (robustness review 1 and 5).
fn amplifying_model(n: u32) -> Vec<u8> {
    use mvdz::coder::rc::{Enc, IntModel};
    const NQC: usize = 306;
    let mut e = Enc::new();
    let mut im = IntModel::new(NQC + 8);
    im.put(&mut e, NQC, 256);
    im.put(&mut e, NQC + 1, 2);
    im.put(&mut e, NQC + 2, n);
    let mut prev = 0usize;
    for _ in 0..256 {
        im.put(&mut e, 17 * 17 + prev.min(16), 1);
        prev = 1;
    }
    for t in 0..n {
        im.put(&mut e, NQC + 3, (t > 0) as u32);
        let mut prev = 0usize;
        for _ in 0..256 {
            im.put(&mut e, 17 + prev.min(16), 1);
            prev = 1;
        }
    }
    let hdr = e.finish();
    let mut b = Vec::new();
    put_uv(&mut b, hdr.len() as u64);
    b.extend_from_slice(&hdr);
    put_uv(&mut b, 16);
    b.extend_from_slice(&[0; 16]);
    b
}

fn hostile(v: &mut Vec<Vector>) {
    let demo = synthetic(&[(1, "{\"a\":"), (0, "1}")], false);
    let base = mvdz::compress(&demo, &c(Level::Normal, 300)).unwrap();
    let secs = sections(&base);
    let iix = secs.iter().position(|s| s.0 == K_INDEX).unwrap();
    let iseg = secs.iter().position(|s| s.0 == K_SEGMENT).unwrap();
    let b0 = mvdz::budget::Budget::unlimited();
    let meta = Metadata::decode(&secs[0].1, &b0).unwrap();
    let mut add = |name: &str, b: Vec<u8>, how: &str| {
        v.push((format!("hostile-{name}.mvdz"), b, None, how.to_string()))
    };
    add(
        "base-cut",
        base[..base.len() / 2].to_vec(),
        "a small valid file cut in half (no trailer)",
    );
    add(
        "no-trailer",
        build(0, true, &secs, false),
        "complete sections, INDEX included, but no trailer",
    );
    let mut x = base.clone();
    x.push(0);
    add("after-trailer", x, "one byte after the trailer");
    let mut s2 = secs.clone();
    let mut forged = meta.clone();
    forged.sha256 = [0xff; 32];
    s2.insert(iix, (K_META, forged.encode().unwrap()));
    add(
        "dup-meta",
        build(0, true, &s2, true),
        "a second META with another identity, prefix and trailer pointing at the first",
    );
    let mut s3 = secs.clone();
    s3[0] = (K_META, forged.encode().unwrap());
    add(
        "forged-identity",
        build(0, true, &s3, true),
        "the META's sha256 replaced (CRC recomputed): only a full decode notices",
    );
    let mut s4 = secs.clone();
    s4.insert(1, (0x99, b"x".to_vec()));
    add(
        "unknown-critical",
        build(0, true, &s4, true),
        "an unknown critical section (kind 0x99)",
    );
    let mut s5 = secs.clone();
    s5[iseg + 1].0 = 0x08;
    add(
        "dropped-segment",
        build(0, true, &s5, true),
        "a segment's kind changed to an unknown non-critical kind",
    );
    let mut s6 = secs.clone();
    s6.swap(iseg, iseg + 1);
    add(
        "swapped-segments",
        build(0, true, &s6, true),
        "two segments swapped (index offsets fixed)",
    );
    // META stored raw (codec 0) with one bit of a field flipped
    let enc = meta.encode().unwrap();
    let mut r = mvdz::util::Rd::new(&enc);
    let (_, codec, n, crc) = (
        r.u8().unwrap(),
        r.u8().unwrap(),
        r.uz().unwrap(),
        r.u32le().unwrap(),
    );
    let mut p = if codec == 1 {
        mvdz::zst::decompress(r.rest(), n).unwrap()
    } else {
        r.rest().to_vec()
    };
    let k = p.len() - 3;
    p[k] ^= 1;
    let mut mb = vec![1, 0];
    put_uv(&mut mb, p.len() as u64);
    mb.extend_from_slice(&crc.to_le_bytes());
    mb.extend_from_slice(&p);
    let mut s7 = secs.clone();
    s7[0].1 = mb;
    add(
        "meta-crc",
        build(0, true, &s7, true),
        "META stored raw with one bit of its fields flipped (CRC not updated)",
    );
    let mut s8 = secs.clone();
    let mut big = vec![1u8, 1];
    put_uv(&mut big, 256 << 20);
    big.extend_from_slice(&[0; 4]);
    big.extend_from_slice(&mvdz::zst::compress(&[0u8; 1000], 3));
    s8[0] = (K_META, big);
    add(
        "meta-256m",
        build(0, true, &s8, true),
        "META declaring 256 MiB of fields",
    );
    let mut s9 = secs.clone();
    let mut b = seg_hdr_bytes(0, MAX_SEG_RAW, 0, 0, C_ZSTD);
    b.extend_from_slice(&mvdz::zst::compress(&[0u8; 10], 3));
    s9[iseg] = (K_SEGMENT, b);
    add(
        "segment-ratio",
        build(0, true, &s9, true),
        "a few-byte zstd segment declaring 4 GiB",
    );
    let mut s10 = secs.clone();
    let mut ix = Index::decode(&s10[iix].1).unwrap();
    ix.size += 60 << 30;
    ix.segs[0].raw_len += 60 << 30;
    s10[iix].1 = ix.encode();
    add(
        "index-size",
        build(0, true, &s10, true),
        "INDEX claiming 60 GB more than the segments",
    );
    let mut s11 = secs.clone();
    let mut nodi = meta.clone();
    nodi.demoinfo.clear();
    s11[0] = (K_META, nodi.encode().unwrap());
    add(
        "share-missing",
        build(0, true, &s11, true),
        "segments share demoinfo the META does not have",
    );
    let h = parse_seg_hdr(&secs[iseg].1).unwrap();
    let mut s12 = secs.clone();
    s12[iseg].1[h.body_off - 1] = 3;
    add(
        "segment-coding",
        build(0, true, &s12, true),
        "a segment with coding 3",
    );
    let mut s13 = secs.clone();
    s13[iseg].1[h.body_off] |= 2;
    add(
        "segment-flags",
        build(0, true, &s13, true),
        "a transform segment with flag bit 1 set",
    );
    // a transform segment whose only stream has an amplifying model
    let plain = synthetic(&[], false);
    let pc = mvdz::compress(&plain, &c(Level::Normal, 0)).unwrap();
    let mut ps = sections(&pc);
    let pi = ps.iter().position(|s| s.0 == K_SEGMENT).unwrap();
    let st = amplifying_model(2000);
    let mut body = Vec::new();
    for x in [
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
        plain.len() as u64,
        st.len() as u64,
    ] {
        put_uv(&mut body, x);
    }
    body.extend_from_slice(&st);
    let mut b = seg_hdr_bytes(
        0,
        plain.len() as u64,
        0,
        mvdz::util::crc32(&plain),
        C_TRANSFORM,
    );
    b.extend_from_slice(&body);
    ps[pi] = (K_SEGMENT, b);
    add(
        "model-decisions",
        build(0, true, &ps, true),
        "a model of 2000 full tables in a few bytes (rule M2)",
    );
    // the same layout with a valid small model whose header is padded with
    // zero bytes (more room under M2): the header must be consumed exactly
    let am = amplifying_model(1);
    let mut st = mvdz::util::Rd::new(&am);
    let hl = st.uz().unwrap();
    let mut hdr = st.take(hl).unwrap().to_vec();
    let rest = st.rest().to_vec();
    hdr.extend_from_slice(&[0; 64]);
    let mut padded = Vec::new();
    put_uv(&mut padded, hdr.len() as u64);
    padded.extend_from_slice(&hdr);
    padded.extend_from_slice(&rest);
    let mut body = Vec::new();
    for x in [
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
        plain.len() as u64,
        padded.len() as u64,
    ] {
        put_uv(&mut body, x);
    }
    body.extend_from_slice(&padded);
    let mut b = seg_hdr_bytes(
        0,
        plain.len() as u64,
        0,
        mvdz::util::crc32(&plain),
        C_TRANSFORM,
    );
    b.extend_from_slice(&body);
    let mut pp = sections(&pc);
    pp[pi] = (K_SEGMENT, b);
    add(
        "model-padding",
        build(0, true, &pp, true),
        "a model header followed by 64 zero bytes (not consumed exactly)",
    );
    // META fields as a zstd bomb: 16 MiB of serverinfo in a few KB (the
    // ratio limit)
    let mut bomb = meta.clone();
    bomb.serverinfo = vec![1u8; (1 << 24) - (1 << 16)];
    let mut sb = secs.clone();
    sb[0] = (K_META, meta_body(&meta_fields(&bomb), true));
    add(
        "meta-ratio",
        build(0, true, &sb, true),
        "META fields of 16 MiB stored as zstd in a few KB (ratio limit)",
    );
    // META as one legacy zstd v0.7 frame (raw blocks)
    let mut sl = secs.clone();
    sl[0] = (K_META, {
        let p = meta_fields(&meta);
        let mut f = vec![0x27, 0xB5, 0x2F, 0xFD, 0x80, 17 << 3];
        f.extend_from_slice(&(p.len() as u32).to_le_bytes());
        for c in p.chunks(1 << 16) {
            f.extend_from_slice(&[
                0x40 | (c.len() >> 16) as u8,
                (c.len() >> 8) as u8,
                c.len() as u8,
            ]);
            f.extend_from_slice(c);
        }
        f.extend_from_slice(&[0xC0, 0, 0]);
        let mut mb = vec![1, 1];
        put_uv(&mut mb, p.len() as u64);
        mb.extend_from_slice(&mvdz::util::crc32(&p).to_le_bytes());
        mb.extend_from_slice(&f);
        mb
    });
    add(
        "zstd-legacy",
        build(0, true, &sl, true),
        "META as a legacy (v0.7) zstd frame",
    );
    // a zstd segment whose frame declares a 2^40-byte window
    let seg0 = &demo[..h.raw_len as usize];
    let mut zb = seg_hdr_bytes(0, seg0.len() as u64, 0, mvdz::util::crc32(seg0), C_ZSTD);
    zb.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD, 0x00, 0xF0]);
    let blk = (seg0.len() as u32) << 3 | 1;
    zb.extend_from_slice(&blk.to_le_bytes()[..3]);
    zb.extend_from_slice(seg0);
    let mut sw = secs.clone();
    sw[iseg] = (K_SEGMENT, zb);
    add(
        "zstd-window",
        build(0, true, &sw, true),
        "a zstd segment whose frame declares a 2^40-byte window",
    );
    // a segment flagged as sharing demoinfo with an empty share, and no
    // demoinfo in the META
    let plain_c = mvdz::compress(&plain, &c(Level::Normal, 0)).unwrap();
    let mut se = sections(&plain_c);
    let ie = se.iter().position(|s| s.0 == K_SEGMENT).unwrap();
    let he = parse_seg_hdr(&se[ie].1).unwrap();
    assert_eq!(se[ie].1[he.body_off], 0);
    se[ie].1[he.body_off] = 1;
    add(
        "share-empty",
        build(0, true, &se, true),
        "a segment flagged as sharing demoinfo with an empty share",
    );
    // META without its timing tag
    let mut st8 = secs.clone();
    let f = meta_fields(&meta);
    let mut r = mvdz::util::Rd::new(&f);
    let mut nf = Vec::new();
    while !r.done() {
        let (t, v) = (r.uv().unwrap(), r.bytes().unwrap());
        if t != T_TIMING {
            put_uv(&mut nf, t);
            put_uv(&mut nf, v.len() as u64);
            nf.extend_from_slice(v);
        }
    }
    st8[0] = (K_META, meta_body(&nf, false));
    add(
        "meta-no-timing",
        build(0, true, &st8, true),
        "META without tag 8 (timing)",
    );
}

/// The raw fields of a META.
fn meta_fields(m: &Metadata) -> Vec<u8> {
    let enc = m.encode().unwrap();
    let mut r = mvdz::util::Rd::new(&enc);
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

/// A META section body holding `p`, raw or as zstd.
fn meta_body(p: &[u8], zstd: bool) -> Vec<u8> {
    let mut mb = vec![1, zstd as u8];
    put_uv(&mut mb, p.len() as u64);
    mb.extend_from_slice(&mvdz::util::crc32(p).to_le_bytes());
    if zstd {
        mb.extend_from_slice(&mvdz::zst::compress(p, 19));
    } else {
        mb.extend_from_slice(p);
    }
    mb
}

/// A synthetic demo carrying the 0x000B start timestamp in the 8-byte u64le
/// form of older mvdsv builds (FORMAT.md §3 tag 11).
fn start_u64() -> Vec<u8> {
    let mut d = Demo(Vec::new());
    d.hidden(0, &[(0x0b, &1_778_675_998_090u64.to_le_bytes())]);
    d.0.extend(synthetic(&[], false));
    d.0
}

fn all_vectors() -> Option<Vec<Vector>> {
    let duel = duel()?;
    let mvd1 = std::fs::read(test_data("gen")?.join("float_mvd1.mvd")).ok()?;
    let fte = sample("3fdd80182a39f773710e4468c509106926450a296c0c9e4a9d4d821f4a783ddb")?;
    let fte = fte[..300_000].to_vec();
    let mut v: Vec<Vector> = Vec::new();
    let mut add = |name: &str, b: Vec<u8>, d: &[u8], how: &str| {
        v.push((name.to_string(), b, Some(d.to_vec()), how.to_string()))
    };
    // without the input check: some vectors hold non-demo input
    let one = |d: &[u8], c: Config| mvdz::encode::compress(d, &c).unwrap().0;
    let st = |d: &[u8], c: Config| {
        let mut w = mvdz::stream::StreamEncoder::new_unchecked(Vec::new(), &c).unwrap();
        for ch in d.chunks(4000) {
            w.write_demo(ch).unwrap();
        }
        w.finish().unwrap()
    };
    let src = "duel = sample 3ff1f82b...8d92";
    add(
        "duel.normal.mvdz",
        one(&duel, c(Level::Normal, 0)),
        &duel,
        &format!("{src}; compress"),
    );
    add(
        "duel.fast.mvdz",
        one(&duel, c(Level::Fast, 0)),
        &duel,
        &format!("{src}; compress --fast"),
    );
    let ext = vec![("source".to_string(), b"hub".to_vec())];
    add(
        "duel.normal.seg64k.mvdz",
        one(
            &duel,
            Config {
                ext,
                ..c(Level::Normal, 65536)
            },
        ),
        &duel,
        &format!("{src}; compress -s 64K --ext source=hub"),
    );
    add(
        "duel.fast.seg64k.mvdz",
        one(&duel, c(Level::Fast, 65536)),
        &duel,
        &format!("{src}; compress --fast -s 64K"),
    );
    add(
        "duel.normal.streamed.mvdz",
        st(&duel, c(Level::Normal, 65536)),
        &duel,
        &format!("{src}; Writer, segment 64K, 4000-byte writes (META before the demoinfo segment)"),
    );
    add(
        "duel.fast.streamed.mvdz",
        st(&duel, c(Level::Fast, 65536)),
        &duel,
        &format!("{src}; Writer --fast, segment 64K"),
    );
    let (fb, _) = mvdz::encode::compress_untransformed(&duel, &c(Level::Normal, 65536)).unwrap();
    add(
        "duel.untransformed.mvdz",
        fb,
        &duel,
        &format!("{src}; every segment zstd (the fallback layout), segment 64K"),
    );
    add(
        "duel-truncated.normal.mvdz",
        one(&duel[..150_000], c(Level::Normal, 0)),
        &duel[..150_000],
        "the first 150000 bytes of duel (ends inside a frame)",
    );
    let mut rng = Rng(0x1234);
    let random: Vec<u8> = (0..3000).map(|_| rng.next() as u8).collect();
    let vtt = b"WEBVTT\n\n00:00.000 --> 00:02.000\n<v alice>go quad\n".repeat(40);
    let atts = [
        Attachment {
            name: "transcript/alice.vtt".into(),
            mime: "text/vtt".into(),
            kv: vec![
                ("x-kind".into(), "transcript".into()),
                ("example.offset_ms".into(), "2500".into()),
            ],
            data: vtt,
        },
        Attachment {
            name: "voice/alice.opus".into(),
            mime: "audio/ogg; codecs=opus".into(),
            kv: vec![
                ("x-kind".into(), "voice".into()),
                ("example.who".into(), "alice".into()),
            ],
            data: random.clone(),
        },
    ];
    let att = mvdz::update_attachments_bytes(&one(&duel, c(Level::Normal, 0)), &[], &atts).unwrap();
    add(
        "duel.attach.mvdz",
        att.clone(),
        &duel,
        "duel.normal + a zstd attachment (text/vtt) and a stored one (random bytes)",
    );
    // forward compatibility: unknown flag bits and reserved bytes, an
    // unknown section, an unknown META tag, an unknown attachment key and
    // codec
    let mut secs = sections(&att);
    let m = Metadata::decode(&secs[0].1, &mvdz::budget::Budget::unlimited()).unwrap();
    let enc = m.encode().unwrap();
    let mut r = mvdz::util::Rd::new(&enc);
    let (_, codec, n, _) = (
        r.u8().unwrap(),
        r.u8().unwrap(),
        r.uz().unwrap(),
        r.u32le().unwrap(),
    );
    let mut p = if codec == 1 {
        mvdz::zst::decompress(r.rest(), n).unwrap()
    } else {
        r.rest().to_vec()
    };
    put_uv(&mut p, 4711);
    put_uv(&mut p, 5);
    p.extend_from_slice(b"later");
    let mut mb = vec![1, 0];
    put_uv(&mut mb, p.len() as u64);
    mb.extend_from_slice(&mvdz::util::crc32(&p).to_le_bytes());
    mb.extend_from_slice(&p);
    secs[0].1 = mb;
    let ia = secs.iter().position(|s| s.0 == K_ADIR).unwrap();
    let mut d = parse_adir(&secs[ia].1).unwrap();
    d[1].kv.push(("x-future".into(), "1".into()));
    d[1].codec = 9;
    secs[ia].1 = adir_body(&d);
    let iix = secs.iter().position(|s| s.0 == K_INDEX).unwrap();
    secs.insert(iix, (0x40, b"future section".to_vec()));
    let mut fwd = build(0x80, true, &secs, true);
    fwd[6] = 0x12;
    fwd[7] = 0x34;
    add(
        "duel.forward.mvdz",
        fwd,
        &duel,
        "duel.attach + prefix flag 0x80 and reserved bytes, an unknown section 0x40, an unknown META tag, an unknown attachment key and codec",
    );
    add(
        "float-mvd1.normal.mvdz",
        one(&mvd1, c(Level::Normal, 0)),
        &mvd1,
        "rev-correct gen1/float_mvd1.mvd (MVD1 float coordinates, 3 escapes)",
    );
    add(
        "float-mvd1.fast.mvdz",
        one(&mvd1, c(Level::Fast, 0)),
        &mvd1,
        "the same, --fast",
    );
    add(
        "float-fte.normal.mvdz",
        one(&fte, c(Level::Normal, 0)),
        &fte,
        "the first 300000 bytes of sample 3fdd8018 (FTE float coordinates)",
    );
    add(
        "float-fte.normal.streamed.mvdz",
        st(&fte, c(Level::Normal, 65536)),
        &fte,
        "the same, Writer, segment 64K (no demoinfo: META written last)",
    );
    let esc = escapes();
    add(
        "escapes.normal.mvdz",
        one(&esc, c(Level::Normal, 0)),
        &esc,
        "synthetic demo with unknown commands and lying hidden blocks",
    );
    add(
        "escapes.fast.mvdz",
        one(&esc, c(Level::Fast, 0)),
        &esc,
        "the same, --fast",
    );
    let md = multi_demoinfo();
    add(
        "demoinfo-multi.normal.mvdz",
        one(&md, c(Level::Normal, 1000)),
        &md,
        "synthetic demo carrying a 3-block KTX demoinfo twice; segment 1000",
    );
    add(
        "demoinfo-multi.normal.streamed.mvdz",
        st(&md, c(Level::Normal, 1000)),
        &md,
        "the same, Writer, segment 1000",
    );
    add(
        "random.normal.mvdz",
        one(&random, c(Level::Normal, 0)),
        &random,
        "3000 random bytes (no frame: all tail)",
    );
    let (rs, _) = mvdz::encode::compress_untransformed(&random, &c(Level::Normal, 0)).unwrap();
    add(
        "random.stored.mvdz",
        rs,
        &random,
        "the same, untransformed: zstd does not pay, stored",
    );
    add(
        "empty.normal.mvdz",
        one(b"", c(Level::Normal, 0)),
        b"",
        "an empty demo",
    );
    add(
        "empty.streamed.mvdz",
        st(b"", c(Level::Normal, 0)),
        b"",
        "an empty demo, Writer (no segment)",
    );
    let su = start_u64();
    let sc = one(&su, c(Level::Normal, 0));
    let fi = mvdz::read_metadata(std::io::Cursor::new(&sc)).unwrap();
    assert_eq!(fi.meta.clock.start_ms.map(|x| x.1), Some(1_778_675_998_090));
    add(
        "start-u64.normal.mvdz",
        sc,
        &su,
        "synthetic demo with a 0x000B start timestamp in the 8-byte u64le form (META tag 11 = 1778675998090)",
    );
    hostile(&mut v);
    Some(v)
}

#[test]
fn vectors_reproduce() {
    let Some(v) = all_vectors() else {
        eprintln!("vector sources not cached, skipping");
        return;
    };
    if std::env::var_os("MVDZ_WRITE_VECTORS").is_some() {
        let _ = std::fs::create_dir_all(dir());
        for f in std::fs::read_dir(dir()).unwrap() {
            let p = f.unwrap().path();
            if p.extension().is_some_and(|x| x == "mvdz") {
                std::fs::remove_file(p).unwrap();
            }
        }
        let mut m = String::from(
            "# file\tdecoded_sha256 or \"error: <message>\"\tdecoded_size\thow it was made\n# Valid files must decode to the SHA-256; hostile files must be refused. The message is the reference's: another decoder should report the same category (format, corrupt, limit; FORMAT.md 9), in its own words.\n",
        );
        for (name, b, d, how) in &v {
            std::fs::write(dir().join(name), b).unwrap();
            match d {
                Some(d) => {
                    m += &format!("{name}\t{}\t{}\t{how}\n", hex(&Sha256::digest(d)), d.len())
                }
                None => {
                    m += &format!(
                        "{name}\terror: {}\t-\t{how}\n",
                        mvdz::decompress(b, 1).unwrap_err()
                    )
                }
            }
        }
        std::fs::write(dir().join("MANIFEST.tsv"), m).unwrap();
        return;
    }
    let rows = manifest();
    assert_eq!(rows.len(), v.len(), "the manifest lists every vector");
    for (name, b, _, _) in &v {
        let committed = std::fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            committed == *b,
            "{name}: the encoder no longer reproduces the committed vector"
        );
    }
}
