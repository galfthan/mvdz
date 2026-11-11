//! Metadata and forward compatibility. Synthetic
//! demos are built here, so most of these run without the demo cache.

mod common;
use common::*;
use mvdz::Level;
use mvdz::format::{self, K_TRAILER, TRAILER_LEN};
use std::io::Cursor;

fn c0() -> mvdz::Config {
    cfg(Level::Normal, 0)
}

// ---------------------------------------------------------------- metadata

/// The META section holds what decoding needs (identity, timing, the
/// shared demoinfo store) and the extension key/values; the JSON
/// rendering shows the same, and nothing of the demo's content.
#[test]
fn metadata_is_container_only() {
    let demo = synthetic(&[], false);
    let mut c0 = c0();
    c0.ext = vec![("match".into(), b"42".to_vec())];
    let c = mvdz::compress(&demo, &c0).unwrap();
    assert_eq!(mvdz::decompress(&c, 1).unwrap(), demo);
    let fi = mvdz::read_metadata(Cursor::new(&c)).unwrap();
    let id = mvdz::ident::Ident::of(&demo);
    let m = &fi.meta;
    assert_eq!(m.size, demo.len() as u64);
    assert_eq!((m.frames, m.duration_ms), (id.frames, id.duration_ms));
    assert!(m.frames > 0 && m.duration_ms > 0);
    assert_eq!(m.ext, vec![("match".to_string(), b"42".to_vec())]);
    assert!(m.demoinfo.is_empty());
    let j: serde_json::Value = serde_json::from_str(&fi.to_json()).unwrap();
    let mut keys: Vec<&str> = j.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "duration_ms",
            "ext",
            "file_size",
            "frames",
            "segments",
            "sha256",
            "size",
            "streamed",
            "version"
        ]
    );
    assert_eq!(j["size"], demo.len());
    assert_eq!(j["ext"]["match"], "42");
}

#[test]
fn demoinfo_store_round_trip() {
    let parts = [
        (1, "{\"a\":1,"),
        (2, "\"b\":2"),
        (0, "}"),
        (1, "{\"a\":3,"),
        (2, "\"b\":4"),
        (0, "}"),
    ];
    let demo = synthetic(&parts, false);
    for level in [Level::Normal, Level::Fast] {
        for seg in [0, 300] {
            let c = mvdz::compress(&demo, &cfg(level, seg)).unwrap();
            assert_eq!(readers_agree(&c, "demoinfo").unwrap(), demo);
            let m = mvdz::read_metadata(Cursor::new(&c)).unwrap().meta;
            // the six blocks, each [u32 len][u16 number][text]
            assert_eq!(m.demoinfo.len(), 6 * 6 + 26);
            // the demo stream references the metadata copy, which carries a
            // CRC: damaging it makes every reader refuse the file
            let meta_off = u64::from_le_bytes(c[8..16].try_into().unwrap()) as usize;
            let mut bad = c.clone();
            let n = bad.len();
            bad[meta_off + 9 + 7 + (n % 5)] ^= 0x55;
            assert!(readers_agree(&bad, "damaged META").is_none());
        }
    }
}

#[test]
fn truncated_demo_round_trips() {
    let demo = synthetic(&[], true);
    for seg in [0, 200] {
        let c = mvdz::compress(&demo, &cfg(Level::Normal, seg)).unwrap();
        assert_eq!(readers_agree(&c, "truncated").unwrap(), demo);
    }
    if let Some(d) = duel() {
        // one byte holds no frame: refused as a non-demo
        let r = mvdz::compress(&d[..1], &cfg(Level::Normal, 65536));
        assert!(matches!(r, Err(mvdz::Error::NotMvd(_))), "{r:?}");
        for cut in [1000, 123_457, d.len() / 2 + 3, d.len() - 1] {
            let c = mvdz::compress(&d[..cut], &cfg(Level::Normal, 65536)).unwrap();
            assert_eq!(mvdz::decompress(&c, 2).unwrap(), &d[..cut], "cut {cut}");
        }
    }
}

// ---------------------------------------------------------------- forward compatibility

#[test]
fn unknown_sections_and_fields() {
    let demo = synthetic(&[(0, "{}")], false);
    let c = mvdz::compress(&demo, &c0()).unwrap();
    // a section of an unknown kind before the trailer
    let cut = c.len() - TRAILER_LEN;
    assert_eq!(c[cut], K_TRAILER);
    for (kind, ok) in [
        (0x04u8, true),
        (0x05, true),
        (0x40, true),
        (0x7e, true),
        (0xc0, false),
    ] {
        let mut b = c[..cut].to_vec();
        format::section(&mut b, kind, b"future data");
        b.extend_from_slice(&c[cut..]);
        let r = readers_agree(&b, "unknown section");
        if ok {
            assert_eq!(
                r.unwrap(),
                demo,
                "unknown non-critical 0x{kind:02x} skipped"
            );
        } else {
            assert!(r.is_none(), "unknown critical kind refused");
            assert!(matches!(
                mvdz::decompress(&b, 1),
                Err(mvdz::Error::Format(_))
            ));
        }
    }
    // unknown prefix flag bits are ignored (robustness review 13)
    let mut b = c.clone();
    b[5] |= 0x80;
    b[6] = 0x12;
    assert_eq!(readers_agree(&b, "flags").unwrap(), demo);
    // unknown metadata tags are skipped
    let m = mvdz::read_metadata(Cursor::new(&c)).unwrap().meta;
    let enc = m.encode().unwrap();
    let mut r = mvdz::util::Rd::new(&enc);
    let (_, codec, n, _) = (
        r.u8().unwrap(),
        r.u8().unwrap(),
        r.uz().unwrap(),
        r.u32le().unwrap(),
    );
    let mut payload = if codec == 1 {
        mvdz::zst::decompress(r.rest(), n).unwrap()
    } else {
        r.rest().to_vec()
    };
    mvdz::util::put_uv(&mut payload, 999);
    mvdz::util::put_uv(&mut payload, 3);
    payload.extend_from_slice(b"new");
    // tags above 5 are skipped whatever their value, and may repeat
    for t in [6u64, 6, 8, 14] {
        mvdz::util::put_uv(&mut payload, t);
        mvdz::util::put_uv(&mut payload, 2);
        payload.extend_from_slice(&[0xff, 0xff]);
    }
    let mut body = vec![1u8, 0];
    mvdz::util::put_uv(&mut body, payload.len() as u64);
    body.extend_from_slice(&mvdz::util::crc32(&payload).to_le_bytes());
    body.extend_from_slice(&payload);
    assert_eq!(
        mvdz::Metadata::decode(&body, &mvdz::budget::Budget::unlimited()).unwrap(),
        m
    );
}

/// Readers skip an unknown non-critical section without buffering it, and
/// a long run of empty ones does not grow them.
#[test]
fn readers_skip_unknown_sections_without_reading_them() {
    let demo = synthetic(&[], false);
    let c = mvdz::compress(&demo, &c0()).unwrap();
    let cut = c.len() - TRAILER_LEN;
    let limits = mvdz::Limits {
        memory: 4 << 20,
        ..Default::default()
    };
    let big: Vec<u8> = (0..20_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 7) as u8)
        .collect();
    let mut large = c[..cut].to_vec();
    format::section(&mut large, 0x40, &big);
    large.extend_from_slice(&c[cut..]);
    let mut many = c[..cut].to_vec();
    for _ in 0..200_000 {
        format::section(&mut many, 0x40, b"");
    }
    many.extend_from_slice(&c[cut..]);
    for b in [&large, &many] {
        let mut out = Vec::new();
        let mut r = mvdz::Reader::new(Cursor::new(b), &limits.into()).unwrap();
        std::io::Read::read_to_end(&mut r, &mut out).unwrap();
        assert_eq!(out, demo);
        mvdz::read_metadata_with(Cursor::new(b), &limits).unwrap();
    }
}
