//! The demo scan and its renderings, on synthetic demos (the helpers are
//! the core crate's test helpers).

#[path = "../../tests/common/mod.rs"]
mod common;
use common::*;
use mvdz::Level;
use mvdz_meta::report;
use mvdz_meta::scan::{InfoChange, PLAYER_LOG_MAX, scan};

#[test]
fn server_roster_and_census() {
    let demo = synthetic(&[], false);
    let sc = scan(&demo);
    assert_eq!(
        sc.serverinfo,
        b"\\map\\dm4\\hostname\\test server\\deathmatch\\3"
    );
    assert_eq!(sc.map, b"maps/dm4.bsp");
    let r = sc.players.roster();
    assert_eq!(r.len(), 2);
    assert_eq!(
        r[1],
        (3, 9, b"\\name\\bob\\team\\blue\\*client\\fte".to_vec())
    );
    assert!(sc.demoinfo.is_empty());
    let cs: serde_json::Value = serde_json::from_str(&sc.census).unwrap();
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
    let j = report::json("x.mvd", &sc);
    assert_eq!(j["map"], "maps/dm4.bsp");
    assert_eq!(j["serverinfo"]["hostname"], "test server");
    assert_eq!(j["census"]["frames"], cs["frames"]);
    assert_eq!(j["size"], demo.len());
    assert_eq!(j["roster"][1]["name"], "bob");
    let t = report::text("x.mvd", &sc);
    assert!(t.contains("map        dm4"), "{t}");
    assert!(t.contains("hostname   test server"), "{t}");
}

/// A .mvdz is decoded first: its scan is the scan of the demo.
#[test]
fn mvdz_input_is_decoded() {
    let demo = synthetic(&[(0, "{}")], false);
    let c = mvdz::compress(&demo, &cfg(Level::Normal, 300)).unwrap();
    let dir = std::env::temp_dir().join(format!("mvdz-meta-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("a.mvdz");
    std::fs::write(&p, &c).unwrap();
    assert_eq!(mvdz_meta::load(&p).unwrap(), demo);
    let gz = dir.join("a.mvd.gz");
    std::fs::write(&gz, mvdz::write_gzip(&demo, Vec::new()).unwrap()).unwrap();
    assert_eq!(mvdz_meta::load(&gz).unwrap(), demo);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn demoinfo_last_block_wins() {
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
    let sc = scan(&synthetic(&parts, false));
    assert_eq!(
        mvdz_meta::scan::demoinfo_json(&sc.demoinfo).unwrap(),
        b"{\"a\":3,\"b\":4}"
    );
    assert!(sc.census.contains("\"0x0003\":[6,"));
    assert_eq!(report::json("x", &sc)["demoinfo"]["b"], 4);
}

#[test]
fn truncated_demo_is_flagged() {
    let sc = scan(&synthetic(&[], true));
    assert!(sc.truncated);
    assert!(
        sc.census.contains("\"truncated\":true,\"tail_bytes\":4"),
        "{}",
        sc.census
    );
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
    let sc = scan(&d.0);
    assert!(
        t.elapsed().as_secs_f64() < 5.0,
        "census scan took {:?}",
        t.elapsed()
    );
    assert!(sc.census.contains("\"messages\":20000"));
}

/// Wall-clock sources and pauses: the 0x000B start stamp, the serverinfo
/// `epoch`, the KTX `matchdate:` print, and paused frames in mvdsv's bare
/// form and the framed form, merged into runs.
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
    let c = scan(&d.0).clock;
    assert_eq!(c.start_ms, Some((98, 1_780_776_295_225)));
    assert_eq!(c.epoch, Some((0, b"1780776295".to_vec())));
    assert_eq!(
        c.matchdate,
        Some((198, b"2026-06-06 20:05:05 UTC".to_vec()))
    );
    assert_eq!(c.pauses, vec![(198, 150), (218, 7)]);
    assert_eq!(report::utc(1_780_776_295_225), "2026-06-06 20:04:55 UTC");
    let t = report::text("x", &scan(&d.0));
    assert!(t.contains("started    2026-06-06 20:04:55 UTC"), "{t}");
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
    let sc = scan(&d.0);
    assert_eq!(sc.clock.matchkey, Some((40, b"4-196677-9630".to_vec())));
    assert_eq!(sc.clock.matchdate, None);
    assert!(sc.census.contains("\"matchkey\":[40,\"4-196677-9630\"]"));
    // a line the demo ends inside is kept as far as it goes
    let mut d = Demo(Vec::new());
    d.frame(5, 6, &s(8, &[&[2], &z("matchkey: 8-2005-8-13:19-56-18")]));
    assert_eq!(
        scan(&d.0).clock.matchkey,
        Some((5, b"8-2005-8-13:19-56-18".to_vec()))
    );
}

/// A `matchkey:` line joins only the prints that directly follow it in
/// its frame: a print in a later frame, or after another command, is not
/// part of it.
#[test]
fn matchkey_join_stays_in_frame() {
    let mk = |frames: &[Vec<u8>]| {
        let mut d = Demo(Vec::new());
        for f in frames {
            d.frame(10, 6, f);
        }
        scan(&d.0).clock.matchkey.map(|x| x.1)
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

fn userinfo(slot: u8, uid: u32, info: &str) -> Vec<u8> {
    s(40, &[&[slot], &uid.to_le_bytes(), &z(info)])
}

/// The player log: every userinfo change in demo order, with the roster
/// rebuilt at the first svc_playerinfo; joins, spectators, setinfo, drops
/// and reconnects after it stay in the log, and the text describes them.
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
    let sc = scan(&d.0);
    let pl = &sc.players;
    assert!(!pl.truncated);
    assert_eq!(pl.start, 3);
    assert_eq!(pl.changes.len(), 7);
    assert_eq!(
        pl.changes[3],
        (
            20,
            2,
            InfoChange::Set {
                key: b"team".to_vec(),
                value: b"blue".to_vec()
            }
        )
    );
    let r = pl.roster();
    assert_eq!(
        r.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(),
        vec![(0, 7), (1, 8), (2, 9)]
    );
    let end = pl.slots_at(pl.changes.len());
    assert_eq!(
        end.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>(),
        vec![(0, 7), (1, 8), (2, 12), (3, 13)]
    );
    let j = report::json("x", &sc);
    assert_eq!(j["players"]["start"], 3);
    assert_eq!(j["players"]["changes"][4]["userinfo"], "");
    assert_eq!(j["roster"][1]["spectator"], true);
    assert_eq!(j["roster"][0]["spectator"], false);
    assert_eq!(j["roster"][1]["userinfo"]["*spectator"], "1");
    let t = report::text("x", &sc);
    for want in [
        "watcher",
        "spectator",
        "bob: setinfo team=blue",
        "bob left",
        "bob joined (userid 12)",
        "carol joined (userid 13)",
    ] {
        assert!(t.contains(want), "{want:?} in\n{t}");
    }
}

/// Past 65536 changes the player log stops and says so; truncation before
/// the first svc_playerinfo leaves `start` = `count`.
#[test]
fn player_log_truncated() {
    let mut d = Demo(Vec::new());
    let one = s(51, &[&[0], &z("k"), &z("v")]);
    for _ in 0..(PLAYER_LOG_MAX / 100 + 1) {
        d.frame(1, 6, &one.repeat(100));
    }
    d.frame(1, 6, &userinfo(1, 5, "\\name\\late"));
    d.frame(1, 6, &s(42, &[&[0], &[0, 0], &[0]]));
    let pl = scan(&d.0).players;
    assert!(pl.truncated);
    assert_eq!(pl.changes.len(), PLAYER_LOG_MAX);
    assert_eq!(pl.start, pl.changes.len());
}

/// Names are made readable and cannot carry terminal escapes.
#[test]
fn qw_names() {
    assert_eq!(
        report::qw(b"\x10a\x11 \x12\x1b \xe1\x1b[2J\x7f"),
        "[a] 09 a9[2J_"
    );
    assert_eq!(report::clock(612_934), "10:12.9");
    assert_eq!(report::clock(3_723_000), "1:02:03.0");
}
