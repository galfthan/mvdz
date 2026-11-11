//! Metadata, census and forward compatibility. Synthetic
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
    assert_eq!(m.roster().len(), 2);
    assert_eq!(
        m.roster()[1],
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

/// `matchkey:` prints (older mods) are kept raw; a line split over several
/// prints is joined up to its newline, and only the first line is kept.
#[test]
fn matchkey_raw_joined() {
    let mut d = Demo(Vec::new());
    d.frame(0, 6, &s(9, &[&z("fullserverinfo \"\\map\\dm2\"\n")]));
    let mut m = Vec::new();
    for part in ["matchkey: 4-", "196677-", "9630\nThe match has begun\n"] {
        m.extend(s(8, &[&[2], &z(part)]));
    }
    d.frame(40, 6, &m);
    d.frame(20, 6, &s(8, &[&[2], &z("matchkey: 9-2005-1-9:1-7-10\n")]));
    let demo = d.0;
    for level in [Level::Normal, Level::Fast] {
        let c = mvdz::compress(&demo, &cfg(level, 0)).unwrap();
        assert_eq!(readers_agree(&c, "matchkey").unwrap(), demo);
        let m = mvdz::read_metadata(Cursor::new(&c)).unwrap().meta;
        assert_eq!(m.clock.matchkey, Some((40, b"4-196677-9630".to_vec())));
        assert_eq!(m.clock.matchdate, None);
        assert!(m.census.contains("\"matchkey\":[40,\"4-196677-9630\"]"));
    }
    // a line the demo ends inside is kept as far as it goes
    let mut d = Demo(Vec::new());
    d.frame(5, 6, &s(8, &[&[2], &z("matchkey: 8-2005-8-13:19-56-18")]));
    let c = mvdz::compress(&d.0, &cfg(Level::Normal, 0)).unwrap();
    let m = mvdz::read_metadata(Cursor::new(&c)).unwrap().meta;
    assert_eq!(
        m.clock.matchkey,
        Some((5, b"8-2005-8-13:19-56-18".to_vec()))
    );
}

fn userinfo(slot: u8, uid: u32, info: &str) -> Vec<u8> {
    s(40, &[&[slot], &uid.to_le_bytes(), &z(info)])
}

/// The player log (FORMAT.md §3 tag 16): every userinfo change in demo
/// order, with the roster rebuilt at the first svc_playerinfo; joins,
/// spectators, setinfo, drops and reconnects after it stay in the log.
#[test]
fn player_log() {
    let mut d = Demo(Vec::new());
    let mut p = Vec::new();
    p.extend(userinfo(0, 7, "\\name\\alice\\team\\red"));
    p.extend(userinfo(1, 8, "\\name\\watcher\\*spectator\\1"));
    p.extend(userinfo(2, 9, "\\name\\bob\\team\\red"));
    d.frame(0, 6, &p);
    // svc_playerinfo: slot, flags (none), frame
    d.frame(10, 6, &s(42, &[&[0], &[0, 0], &[0]]));
    d.frame(10, 6, &s(51, &[&[2], &z("team"), &z("blue")]));
    d.frame(10, 6, &userinfo(2, 9, ""));
    d.frame(10, 6, &userinfo(2, 12, "\\name\\bob\\team\\blue"));
    d.frame(10, 6, &userinfo(3, 13, "\\name\\carol\\team\\red"));
    let demo = d.0;
    for level in [Level::Normal, Level::Fast] {
        let c = mvdz::compress(&demo, &cfg(level, 0)).unwrap();
        assert_eq!(readers_agree(&c, "players").unwrap(), demo);
        let fi = mvdz::read_metadata(Cursor::new(&c)).unwrap();
        let pl = &fi.meta.players;
        assert!(!pl.truncated);
        assert_eq!(pl.start, 3);
        assert_eq!(pl.changes.len(), 7);
        assert_eq!(
            pl.changes[3],
            (
                20,
                2,
                mvdz::meta::InfoChange::Set {
                    key: b"team".to_vec(),
                    value: b"blue".to_vec()
                }
            )
        );
        let r = fi.meta.roster();
        assert_eq!(
            r.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(),
            vec![(0, 7), (1, 8), (2, 9)]
        );
        let end = pl.slots_at(pl.changes.len());
        assert_eq!(
            end.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(),
            vec![(0, 7), (1, 8), (2, 12), (3, 13)]
        );
        let j: serde_json::Value = serde_json::from_str(&fi.to_json()).unwrap();
        assert_eq!(j["players"]["start"], 3);
        assert_eq!(j["players"]["changes"][4]["userinfo"], "");
        assert_eq!(j["roster"][1]["userinfo"], "\\name\\watcher\\*spectator\\1");
    }
}

/// Past 65536 changes the player log stops and says so.
#[test]
fn player_log_truncated() {
    let mut d = Demo(Vec::new());
    let one = s(51, &[&[0], &z("k"), &z("v")]);
    for _ in 0..(mvdz::meta::PLAYER_LOG_MAX / 100 + 1) {
        d.frame(1, 6, &one.repeat(100));
    }
    let demo = d.0;
    let c = mvdz::compress(&demo, &c0()).unwrap();
    assert_eq!(readers_agree(&c, "players cap").unwrap(), demo);
    let pl = mvdz::read_metadata(Cursor::new(&c)).unwrap().meta.players;
    assert!(pl.truncated);
    assert_eq!(pl.changes.len(), mvdz::meta::PLAYER_LOG_MAX);
    assert_eq!(pl.start, pl.changes.len());
}

/// A `matchkey:` line joins only the prints that directly follow it in
/// its frame: a print in a later frame, or after another command, is not
/// part of it (FORMAT.md §3 tag 15).
#[test]
fn matchkey_join_stays_in_frame() {
    let mk = |frames: &[Vec<u8>]| {
        let mut d = Demo(Vec::new());
        for f in frames {
            d.frame(10, 6, f);
        }
        let c = mvdz::compress(&d.0, &c0()).unwrap();
        assert_eq!(readers_agree(&c, "matchkey frames").unwrap(), d.0);
        mvdz::read_metadata(Cursor::new(&c))
            .unwrap()
            .meta
            .clock
            .matchkey
            .map(|x| x.1)
    };
    let print = |t: &str| s(8, &[&[2], &z(t)]);
    // the line ends with its frame
    let got = mk(&[print("matchkey: 8-2005-8-13:19-56-18"), print("bob: hi\n")]);
    assert_eq!(got.as_deref(), Some(&b"8-2005-8-13:19-56-18"[..]));
    // another command ends it
    let f = [
        print("matchkey: 8-2005-"),
        s(51, &[&[0], &z("k"), &z("v")]),
        print("x\n"),
    ]
    .concat();
    assert_eq!(mk(&[f]).as_deref(), Some(&b"8-2005-"[..]));
    // an empty first line counts; later lines do not
    let got = mk(&[print("matchkey:\n"), print("matchkey: 1-2005-1-1:1-1-1\n")]);
    assert_eq!(got.as_deref(), Some(&b""[..]));
}

/// Truncation before the first svc_playerinfo: `start` = `count`, as the
/// spec says, so readers know the roster may be incomplete; the Writer
/// gives the same player log as one-shot compression.
#[test]
fn player_log_truncated_before_playerinfo_and_writer() {
    let mut d = Demo(Vec::new());
    let one = s(51, &[&[0], &z("k"), &z("v")]);
    for _ in 0..(mvdz::meta::PLAYER_LOG_MAX / 100 + 1) {
        d.frame(1, 6, &one.repeat(100));
    }
    d.frame(
        1,
        6,
        &s(40, &[&[1], &5u32.to_le_bytes(), &z("\\name\\late")]),
    );
    d.frame(1, 6, &s(42, &[&[0], &[0, 0], &[0]]));
    let demo = d.0;
    let c = mvdz::compress(&demo, &cfg(Level::Normal, 1 << 16)).unwrap();
    let pl = mvdz::read_metadata(Cursor::new(&c)).unwrap().meta.players;
    assert!(pl.truncated && pl.start == pl.changes.len());
    let mut w = mvdz::stream::StreamEncoder::new(Vec::new(), &cfg(Level::Normal, 1 << 16)).unwrap();
    for ch in demo.chunks(5000) {
        w.write_demo(ch).unwrap();
    }
    let wc = w.finish().unwrap();
    let wpl = mvdz::read_metadata(Cursor::new(&wc)).unwrap().meta.players;
    assert_eq!(wpl, pl);
}
