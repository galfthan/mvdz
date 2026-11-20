//! Metadata, census, attachments and forward compatibility. Synthetic
//! demos are built here, so most of these run without the demo cache.

mod common;
use common::*;
use mvdz::format::{self, K_TRAILER, TRAILER_LEN};
use mvdz::{Attachment, Level};
use std::io::Cursor;

fn c0() -> mvdz::Config {
    cfg(Level::Normal, 0)
}

// ---------------------------------------------------------------- metadata

#[test]
fn metadata_and_census() {
    let demo = synthetic(&[], false);
    let c = mvdz::compress(&demo, &c0()).unwrap();
    assert_eq!(mvdz::decompress(&c, 1).unwrap(), demo);
    let fi = mvdz::read_metadata(Cursor::new(&c)).unwrap();
    let json: serde_json::Value = serde_json::from_str(&fi.to_json()).unwrap();
    let m = fi.meta;
    assert_eq!(
        m.serverinfo,
        b"\\map\\dm4\\hostname\\test server\\deathmatch\\3"
    );
    assert_eq!(m.map, b"maps/dm4.bsp");
    assert_eq!(m.roster.len(), 2);
    assert_eq!(
        m.roster[1],
        (3, 9, b"\\name\\bob\\team\\blue\\*client\\fte".to_vec())
    );
    assert!(m.demoinfo.is_empty() && m.demoinfo_json().is_none());
    let cs: serde_json::Value = serde_json::from_str(&m.census).unwrap();
    let cmds = &cs["stufftext"]["commands"];
    assert_eq!(
        cmds["topcolor"]["count"], 50,
        "fragmented stufftext reassembled"
    );
    assert_eq!(cmds["_cs"]["count"], 100, "';' splits commands");
    assert_eq!(cmds["wait"]["count"], 50);
    assert_eq!(
        cmds["//ktx took"]["count"], 50,
        "a word after // is part of the key"
    );
    assert_eq!(cmds["//finalscores"]["count"], 50, "a quoted date is not");
    assert_eq!(cs["print"]["2"], 50);
    assert_eq!(cs["truncated"], false);
    assert_eq!(
        cs["client_values"]["*client"],
        serde_json::json!(["ezQuake 7", "fte"])
    );
    assert_eq!(json["map"], "maps/dm4.bsp");
    assert_eq!(json["census"]["frames"], cs["frames"]);
    assert_eq!(json["size"], demo.len());
}

#[test]
fn demoinfo_multi_block_round_trip() {
    // blocks 1, 2, 0; then a second, newer copy: the last block of each
    // number wins, as in the mvd_analyzer project
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
            assert_eq!(m.demoinfo_json().unwrap(), b"{\"a\":3,\"b\":4}");
            assert!(m.census.contains("\"0x0003\":[6,"));
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
fn truncated_demo_round_trips_and_is_flagged() {
    let demo = synthetic(&[], true);
    for seg in [0, 200] {
        let c = mvdz::compress(&demo, &cfg(Level::Normal, seg)).unwrap();
        assert_eq!(readers_agree(&c, "truncated").unwrap(), demo);
        let m = mvdz::read_metadata(Cursor::new(&c)).unwrap().meta;
        assert!(
            m.census.contains("\"truncated\":true,\"tail_bytes\":4"),
            "{}",
            m.census
        );
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

/// Newline-free stufftext used to make the census scan quadratic
/// (correctness review F6: 51 s for 5000 messages).
#[test]
fn census_scan_is_linear() {
    let mut d = Demo(Vec::new());
    let msg = s(9, &[&z(&"x".repeat(999))]);
    for _ in 0..20_000 {
        d.frame(0, 6, &msg);
    }
    let t = std::time::Instant::now();
    let sc = mvdz::meta::scan(&d.0);
    assert!(
        t.elapsed().as_secs_f64() < 5.0,
        "census scan took {:?}",
        t.elapsed()
    );
    assert!(sc.census.contains("\"messages\":20000"));
}

/// A census that is not a JSON object cannot inject keys into the JSON
/// rendering (robustness review 10).
#[test]
fn census_json_cannot_inject() {
    let demo = synthetic(&[], false);
    let c = mvdz::compress(&demo, &c0()).unwrap();
    let fi = mvdz::read_metadata(Cursor::new(&c)).unwrap();
    for census in ["{}, \"sha256\": \"00\", \"x\": {", "}", "[1]", "{\"ok\":1}"] {
        let mut fi = fi.clone();
        fi.meta.census = census.into();
        let j: serde_json::Value = serde_json::from_str(&fi.to_json()).unwrap();
        assert_eq!(j["sha256"], mvdz::api::hex(&fi.meta.sha256));
        if census.starts_with("{\"ok") {
            assert_eq!(j["census"]["ok"], 1);
        } else {
            assert!(j["census"].is_null());
        }
    }
}

// ---------------------------------------------------------------- attachments

fn att(name: &str, mime: &str, kv: &[(&str, &str)], data: &[u8]) -> Attachment {
    Attachment {
        name: name.into(),
        mime: mime.into(),
        kv: kv
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        data: data.to_vec(),
    }
}

fn body_end(c: &[u8]) -> (format::Trailer, &[u8]) {
    let t = format::Trailer::parse_body(&c[c.len() - 40..]).unwrap();
    (t, &c[..t.body_end as usize])
}

#[test]
fn attachments_round_trip_and_leave_the_demo_alone() {
    let demo = synthetic(&[(0, "{}")], false);
    let c = mvdz::compress(&demo, &c0()).unwrap();
    let (t0, body0) = body_end(&c);
    let mut rng = Rng(3);
    let voice: Vec<u8> = (0..300_000).map(|_| rng.next() as u8).collect();
    let vtt = b"WEBVTT\n\n00:00.000 --> 00:02.000\n<v alice>go quad\n".repeat(200);
    let adds = [
        att(
            "voice/alice.opus",
            "audio/ogg; codecs=opus",
            &[
                ("x-role", "voice"),
                ("x-player", "alice"),
                ("x-slot", "0"),
                ("x-time_offset_ms", "-1500"),
            ],
            &voice,
        ),
        att(
            "transcript/alice.vtt",
            "text/vtt",
            &[
                ("x-role", "transcript"),
                ("x-player", "alice"),
                ("x-time_offset_ms", "-1500"),
            ],
            &vtt,
        ),
        // unknown role, unknown keys and an unknown type are preserved
        att(
            "x/odd.bin",
            "application/x-unknown",
            &[("x-role", "hologram"), ("x-custom", "42")],
            b"\x00\x01\x02",
        ),
    ];
    let c2 = mvdz::update_attachments_bytes(&c, &[], &adds).unwrap();
    let (t2, body2) = body_end(&c2);
    assert_eq!(body0, body2, "demo part copied byte for byte");
    assert_eq!(
        (t2.index_off, t2.meta_off, t2.body_end),
        (t0.index_off, t0.meta_off, t0.body_end)
    );
    assert_eq!(readers_agree(&c2, "attachments").unwrap(), demo);
    let list = mvdz::list_attachments(Cursor::new(&c2), "").unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].kv, adds[0].kv);
    assert_eq!(list[2].kv, adds[2].kv);
    assert_eq!(list[2].mime, "application/x-unknown");
    assert_eq!(
        list[0].codec,
        format::A_STORE,
        "incompressible data is stored"
    );
    assert_eq!(list[1].codec, format::A_ZSTD, "text is stored compressed");
    assert_eq!(
        mvdz::list_attachments(Cursor::new(&c2), "transcript/")
            .unwrap()
            .len(),
        1
    );
    for a in &adds {
        assert_eq!(
            mvdz::read_attachment(Cursor::new(&c2), &a.name).unwrap(),
            a.data
        );
    }
    // replace one, remove one; the others are copied unchanged
    let v2 = att(
        "voice/alice.opus",
        "audio/ogg; codecs=opus",
        &[("x-role", "voice")],
        b"new",
    );
    let c3 =
        mvdz::update_attachments_bytes(&c2, &["x/odd.bin"], std::slice::from_ref(&v2)).unwrap();
    let l3 = mvdz::list_attachments(Cursor::new(&c3), "").unwrap();
    assert_eq!(
        l3.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        ["transcript/alice.vtt", "voice/alice.opus"]
    );
    assert_eq!(
        mvdz::read_attachment(Cursor::new(&c3), "voice/alice.opus").unwrap(),
        b"new"
    );
    assert_eq!(
        mvdz::read_attachment(Cursor::new(&c3), "transcript/alice.vtt").unwrap(),
        vtt
    );
    assert_eq!(body_end(&c3).1, body0);
    assert_eq!(mvdz::decompress(&c3, 1).unwrap(), demo);
    // remove all: back to the original file
    let c4 =
        mvdz::update_attachments_bytes(&c3, &["voice/alice.opus", "transcript/alice.vtt"], &[])
            .unwrap();
    assert_eq!(c4, c);
    assert!(matches!(
        mvdz::read_attachment(Cursor::new(&c4), "voice/alice.opus"),
        Err(mvdz::Error::NotFound)
    ));
    // duplicate or empty names are refused (correctness review F7)
    let dup = [att("a", "", &[], b"1"), att("a", "", &[], b"2")];
    assert!(matches!(
        mvdz::update_attachments_bytes(&c, &[], &dup),
        Err(mvdz::Error::Invalid(_))
    ));
    assert!(mvdz::update_attachments_bytes(&c, &[], &[att("", "", &[], b"1")]).is_err());
}

#[test]
fn readers_skip_attachments_without_reading_them() {
    // a large attachment: the sequential reader skips it unread
    let demo = synthetic(&[], false);
    let c = mvdz::compress(&demo, &c0()).unwrap();
    let big: Vec<u8> = (0..20_000_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 7) as u8)
        .collect();
    let c2 = mvdz::update_attachments_bytes(&c, &[], &[att("big", "", &[], &big)]).unwrap();
    let limits = mvdz::Limits {
        memory: 4 << 20,
        ..Default::default()
    };
    let mut out = Vec::new();
    let mut r = mvdz::Reader::new(Cursor::new(&c2), &limits.into()).unwrap();
    std::io::Read::read_to_end(&mut r, &mut out).unwrap();
    assert_eq!(out, demo);
    // reading the attachment itself needs the memory
    assert!(matches!(
        mvdz::read_attachment_with(Cursor::new(&c2), "big", &limits),
        Err(mvdz::Error::Limit(_))
    ));
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("mvdz-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

// the lock-and-rename update is POSIX behaviour (FORMAT.md §10, Updating)
#[cfg(unix)]
#[test]
fn attachment_update_on_disk() {
    let demo = synthetic(&[], false);
    let c = mvdz::compress(&demo, &c0()).unwrap();
    let dir = tmpdir("att");
    let p = dir.join("a.mvdz");
    std::fs::write(&p, &c).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    let a = att(
        "analysis/r.v1",
        "application/vnd.example.result+json",
        &[("x-role", "analysis")],
        &vec![7u8; 600_000],
    );
    mvdz::update_attachments(&p, &[], std::slice::from_ref(&a)).unwrap();
    let b = std::fs::read(&p).unwrap();
    assert_eq!(
        mvdz::read_attachment(Cursor::new(&b), "analysis/r.v1").unwrap(),
        a.data
    );
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "no temporary file left"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o640,
            "permissions kept"
        );
        // through a symlink: the target is updated, the link stays a link
        let l = dir.join("link.mvdz");
        std::os::unix::fs::symlink(&p, &l).unwrap();
        mvdz::update_attachments(&l, &["analysis/r.v1"], &[]).unwrap();
        assert!(
            std::fs::symlink_metadata(&l)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&p).unwrap(), c);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Concurrent updaters (threads of one process) do not lose each other's
/// attachments (robustness review 8: 44 of 60 updates were lost).
// replacing a file that another handle holds open is POSIX behaviour
#[cfg(unix)]
#[test]
fn concurrent_attachment_updates() {
    let demo = synthetic(&[], false);
    let c = mvdz::compress(&demo, &c0()).unwrap();
    let dir = tmpdir("race");
    let p = dir.join("r.mvdz");
    std::fs::write(&p, &c).unwrap();
    std::thread::scope(|s| {
        for t in 0..4 {
            let p = &p;
            s.spawn(move || {
                for k in 0..8 {
                    let a = att(
                        &format!("t{t}/{k}"),
                        "",
                        &[],
                        &vec![(t * 8 + k) as u8; 50_000 + k],
                    );
                    mvdz::update_attachments(p, &[], &[a]).unwrap();
                }
            });
        }
    });
    let b = std::fs::read(&p).unwrap();
    assert_eq!(
        mvdz::list_attachments(Cursor::new(&b), "").unwrap().len(),
        32,
        "no update lost"
    );
    assert_eq!(mvdz::decompress(&b, 1).unwrap(), demo);
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "no temporary file left"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_attachment_directory() {
    let demo = synthetic(&[], false);
    let c = mvdz::compress(&demo, &c0()).unwrap();
    let c2 = mvdz::update_attachments_bytes(
        &c,
        &[],
        &[att("a", "text/plain", &[("x-k", "v")], b"hello world")],
    )
    .unwrap();
    let (t, _) = body_end(&c2);
    let mut rng = Rng(99);
    for i in 0..200 {
        let mut b = c2.clone();
        let span = c2.len() - TRAILER_LEN - t.adir_off as usize;
        let k = t.adir_off as usize + rng.below(span);
        b[k] ^= 1 << (i % 8);
        let r = std::panic::catch_unwind(|| {
            let _ = mvdz::list_attachments(Cursor::new(&b), "");
            let _ = mvdz::read_attachment(Cursor::new(&b), "a");
            let _ = mvdz::update_attachments_bytes(&b, &["a"], &[]);
            // every reader applies the same directory rules
            readers_agree(&b, "adir");
        });
        assert!(r.is_ok(), "attachment directory byte {k}");
    }
}

// ---------------------------------------------------------------- forward compatibility

#[test]
fn unknown_sections_and_fields() {
    let demo = synthetic(&[(0, "{}")], false);
    let c = mvdz::update_attachments_bytes(
        &mvdz::compress(&demo, &c0()).unwrap(),
        &[],
        &[att("a", "text/plain", &[], b"x")],
    )
    .unwrap();
    // a section of an unknown kind before the trailer
    let cut = c.len() - TRAILER_LEN;
    assert_eq!(c[cut], K_TRAILER);
    for (kind, ok) in [(0x40u8, true), (0x7eu8, true), (0xc0u8, false)] {
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
            assert_eq!(mvdz::read_attachment(Cursor::new(&b), "a").unwrap(), b"x");
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
    let mut body = vec![1u8, 0];
    mvdz::util::put_uv(&mut body, payload.len() as u64);
    body.extend_from_slice(&mvdz::util::crc32(&payload).to_le_bytes());
    body.extend_from_slice(&payload);
    assert_eq!(
        mvdz::Metadata::decode(&body, &mvdz::budget::Budget::unlimited()).unwrap(),
        m
    );
    // an unknown attachment codec: listed, but not readable
    let mut e = mvdz::list_attachments(Cursor::new(&c), "")
        .unwrap()
        .remove(0);
    e.codec = 9;
    assert!(matches!(
        format::unpack_attachment(&e, b"x".to_vec(), &mvdz::budget::Budget::unlimited()),
        Err(mvdz::Error::Format(_))
    ));
}

/// Wall-clock sources and pauses (FORMAT.md §3.1): the 0x000B start stamp,
/// the serverinfo `epoch`, the KTX `matchdate:` print, and paused frames
/// in mvdsv's bare form and the framed form, merged into runs.
#[test]
fn wall_clock_sources() {
    let mut d = Demo(Vec::new());
    d.frame(
        0,
        6,
        &s(
            9,
            &[&z("fullserverinfo \"\\epoch\\1780776295\\map\\dm2\"\n")],
        ),
    );
    let mut ts = Vec::new();
    mvdz::util::put_uv(&mut ts, 1_780_776_295_225);
    d.hidden(98, &[(0x0b, &ts[..])]);
    d.frame(
        100,
        6,
        &s(8, &[&[2], &z("matchdate: 2026-06-06 20:05:05 UTC\n")]),
    );
    for ms in [100u8, 50] {
        // bare: [u16 0x000A][u8 ms]
        d.0.extend_from_slice(&[0, 3, 0, 0, 0, 0, 3, 0, 0, 0, 0x0a, 0, ms]);
    }
    d.frame(20, 6, &s(8, &[&[2], &z("x\n")]));
    d.hidden(0, &[(0x0a, &[7u8][..])]);
    d.frame(5, 6, &s(8, &[&[2], &z("y\n")]));
    let demo = d.0;
    for level in [Level::Normal, Level::Fast] {
        let c = mvdz::compress(&demo, &cfg(level, 0)).unwrap();
        assert_eq!(readers_agree(&c, "clock").unwrap(), demo);
        let m = mvdz::read_metadata(Cursor::new(&c)).unwrap().meta;
        assert_eq!(m.clock.start_ms, Some((98, 1_780_776_295_225)));
        assert_eq!(m.clock.epoch, Some((0, b"1780776295".to_vec())));
        assert_eq!(
            m.clock.matchdate,
            Some((198, b"2026-06-06 20:05:05 UTC".to_vec()))
        );
        assert_eq!(m.clock.pauses, vec![(198, 150), (218, 7)]);
    }
}
