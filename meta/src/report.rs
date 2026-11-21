//! Renderings of a scan: JSON (raw strings kept, as Latin-1) and text for
//! people (QuakeWorld names made readable, player changes described).

use crate::scan::{InfoChange, Scan, demoinfo_json, info_pairs};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write;

/// Latin-1 view of bytes: every byte becomes the code point of the same
/// value, so a reader recovers the bytes exactly.
pub fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// A QuakeWorld string for a terminal: the high (coloured) bit dropped,
/// the console's bracket and digit glyphs mapped to ASCII, and every
/// other control byte shown as `_` (so a demo cannot send terminal
/// escapes).
pub fn qw(b: &[u8]) -> String {
    b.iter()
        .map(|&c| match c & 0x7f {
            0x10 => '[',
            0x11 => ']',
            d @ 0x12..=0x1b => (b'0' + d - 0x12) as char,
            0x20..=0x7e => (c & 0x7f) as char,
            _ => '_',
        })
        .collect()
}

/// Demo time as m:ss.s (h:mm:ss.s from an hour).
pub fn clock(ms: u64) -> String {
    let (h, m, s) = (ms / 3_600_000, ms / 60_000 % 60, ms % 60_000);
    let s = format!("{:02}.{}", s / 1000, s % 1000 / 100);
    if h > 0 {
        format!("{h}:{m:02}:{s}")
    } else {
        format!("{m}:{s}")
    }
}

/// Unix ms as a UTC date and time.
pub fn utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// One userinfo string as key/value pairs (later keys win).
type Info = Vec<(Vec<u8>, Vec<u8>)>;

fn parse_info(b: &[u8]) -> Info {
    let mut v: Info = Vec::new();
    for (k, x) in info_pairs(b) {
        set_key(&mut v, k, x);
    }
    v
}

fn set_key(info: &mut Info, k: &[u8], x: &[u8]) {
    match info.iter_mut().find(|e| e.0 == k) {
        Some(e) => e.1 = x.to_vec(),
        None => info.push((k.to_vec(), x.to_vec())),
    }
}

fn get<'a>(info: &'a Info, k: &str) -> &'a [u8] {
    info.iter()
        .find(|e| e.0 == k.as_bytes())
        .map_or(&[][..], |e| &e.1[..])
}

fn spectator(info: &Info) -> bool {
    !matches!(get(info, "*spectator"), b"" | b"0")
}

fn info_json(info: &Info) -> Value {
    Value::Object(
        info.iter()
            .map(|(k, x)| (latin1(k), Value::String(latin1(x))))
            .collect(),
    )
}

/// A slot's player: userid and userinfo.
#[derive(Clone)]
struct Player {
    userid: u32,
    info: Info,
}

/// A player change after the start, described.
struct Event {
    t: u64,
    slot: u8,
    what: String,
}

/// Replays the player log: the slots at the first svc_playerinfo, and
/// every later change described (svc_setinfo applied too).
fn replay(s: &Scan) -> (BTreeMap<u8, Player>, Vec<Event>) {
    let log = &s.players;
    let mut slots: BTreeMap<u8, Player> = BTreeMap::new();
    let mut start = BTreeMap::new();
    let mut ev = Vec::new();
    for (n, (t, slot, c)) in log.changes.iter().enumerate() {
        if n == log.start {
            start = slots.clone();
        }
        let prev = slots.get(slot).cloned();
        let what = match c {
            InfoChange::User { info, .. } if info.is_empty() => {
                slots.remove(slot);
                match prev {
                    Some(p) => format!("{} left", qw(get(&p.info, "name"))),
                    None => "slot cleared".into(),
                }
            }
            InfoChange::User { userid, info } => {
                let new = parse_info(info);
                let name = qw(get(&new, "name"));
                let what = match &prev {
                    Some(p) if p.userid == *userid => {
                        let changed: Vec<String> = new
                            .iter()
                            .filter(|(k, x)| get(&p.info, &latin1(k)) != &x[..])
                            .map(|(k, x)| format!("{}={}", qw(k), qw(x)))
                            .collect();
                        let old = qw(get(&p.info, "name"));
                        if old != name {
                            format!("{old} renamed to {name}")
                        } else if changed.is_empty() {
                            format!("{name}: userinfo resent")
                        } else {
                            format!("{name}: {}", changed.join(" "))
                        }
                    }
                    _ => format!(
                        "{name} joined{} (userid {userid})",
                        if spectator(&new) { " as spectator" } else { "" }
                    ),
                };
                slots.insert(
                    *slot,
                    Player {
                        userid: *userid,
                        info: new,
                    },
                );
                what
            }
            InfoChange::Set { key, value } => {
                let p = slots.entry(*slot).or_insert(Player {
                    userid: 0,
                    info: Vec::new(),
                });
                set_key(&mut p.info, key, value);
                format!(
                    "{}: setinfo {}={}",
                    qw(get(&p.info, "name")),
                    qw(key),
                    qw(value)
                )
            }
        };
        if n >= log.start {
            ev.push(Event {
                t: *t,
                slot: *slot,
                what,
            });
        }
    }
    if log.start >= log.changes.len() {
        start = slots;
    }
    (start, ev)
}

/// The scan as one JSON object.
pub fn json(file: &str, s: &Scan) -> Value {
    let (start, _) = replay(s);
    let c = &s.clock;
    let at = |x: &Option<(u64, Vec<u8>)>| x.as_ref().map(|(t, v)| json!([t, latin1(v)]));
    let changes: Vec<Value> = s
        .players
        .changes
        .iter()
        .map(|(t, slot, c)| match c {
            InfoChange::User { userid, info } => {
                json!({"t": t, "slot": slot, "userid": userid, "userinfo": latin1(info)})
            }
            InfoChange::Set { key, value } => {
                json!({"t": t, "slot": slot, "key": latin1(key), "value": latin1(value)})
            }
        })
        .collect();
    let demoinfo = demoinfo_json(&s.demoinfo)
        .map(|d| serde_json::from_slice::<Value>(&d).unwrap_or_else(|_| Value::String(latin1(&d))));
    json!({
        "file": file,
        "sha256": s.sha256.iter().map(|x| format!("{x:02x}")).collect::<String>(),
        "size": s.size,
        "frames": s.frames,
        "duration_ms": s.duration_ms,
        "truncated": s.truncated,
        "map": latin1(&s.map),
        "levelname": latin1(&s.levelname),
        "serverinfo": info_json(&parse_info(&s.serverinfo)),
        "clock": {
            "start_timestamp_ms": c.start_ms.map(|(t, v)| [t, v]),
            "epoch": at(&c.epoch),
            "matchdate": at(&c.matchdate),
            "matchkey": at(&c.matchkey),
            "pauses": c.pauses.iter().map(|p| [p.0, p.1]).collect::<Vec<_>>(),
            "pauses_truncated": c.pauses_truncated,
        },
        "roster": start.iter().map(|(slot, p)| json!({
            "slot": slot,
            "userid": p.userid,
            "name": latin1(get(&p.info, "name")),
            "team": latin1(get(&p.info, "team")),
            "spectator": spectator(&p.info),
            "userinfo": info_json(&p.info),
        })).collect::<Vec<_>>(),
        "players": {
            "truncated": s.players.truncated,
            "start": s.players.start,
            "changes": changes,
        },
        "demoinfo": demoinfo,
        "census": serde_json::from_str::<Value>(&s.census).unwrap_or(Value::Null),
    })
}

/// The scan as text for people.
pub fn text(file: &str, s: &Scan) -> String {
    let mut o = String::new();
    let (start, ev) = replay(s);
    let si = parse_info(&s.serverinfo);
    let c = &s.clock;
    let mut line = |k: &str, v: String| {
        let _ = writeln!(o, "  {k:<11}{v}");
    };
    line(
        "duration",
        format!(
            "{} ({} frames){}",
            clock(s.duration_ms),
            s.frames,
            if s.truncated { ", truncated" } else { "" }
        ),
    );
    line(
        "demo",
        format!(
            "{} bytes, sha256 {}",
            s.size,
            s.sha256
                .iter()
                .map(|x| format!("{x:02x}"))
                .collect::<String>()
        ),
    );
    let map = s.map.strip_prefix(b"maps/").unwrap_or(&s.map);
    let map = map.strip_suffix(b".bsp").unwrap_or(map);
    line(
        "map",
        format!("{} \"{}\"", qw(map), qw(&s.levelname).trim()),
    );
    for k in [
        "hostname",
        "*gamedir",
        "deathmatch",
        "teamplay",
        "timelimit",
        "fraglimit",
    ] {
        let v = get(&si, k);
        if !v.is_empty() {
            line(k.trim_start_matches('*'), qw(v));
        }
    }
    if let Some((t, v)) = c.start_ms {
        line("started", format!("{} (recorded at {})", utc(v), clock(t)));
    }
    for (k, x) in [
        ("epoch", &c.epoch),
        ("matchdate", &c.matchdate),
        ("matchkey", &c.matchkey),
    ] {
        if let Some((t, v)) = x {
            line(k, format!("{} (at {})", qw(v), clock(*t)));
        }
    }
    if !c.pauses.is_empty() {
        let ms: u64 = c.pauses.iter().map(|p| p.1).sum();
        line(
            "pauses",
            format!(
                "{} runs, {} paused{}",
                c.pauses.len(),
                clock(ms),
                if c.pauses_truncated {
                    " (truncated)"
                } else {
                    ""
                }
            ),
        );
    }
    let _ = writeln!(o, "  players at start:");
    for (slot, p) in &start {
        let _ = writeln!(
            o,
            "    {slot:>3}  {:<20} {:<6} userid {}{}",
            qw(get(&p.info, "name")),
            qw(get(&p.info, "team")),
            p.userid,
            if spectator(&p.info) {
                "  spectator"
            } else {
                ""
            }
        );
    }
    if !ev.is_empty() {
        let _ = writeln!(o, "  player changes:");
        for e in &ev {
            let _ = writeln!(o, "    {:>9}  slot {:<3} {}", clock(e.t), e.slot, e.what);
        }
    }
    if s.players.truncated {
        let _ = writeln!(o, "    (the log stops here: too many changes)");
    }
    if let Some(d) = demoinfo_json(&s.demoinfo) {
        let _ = writeln!(
            o,
            "  {:<11}KTX demoinfo, {} bytes (--json shows it)",
            "stats",
            d.len()
        );
    }
    let census: Value = serde_json::from_str(&s.census).unwrap_or(Value::Null);
    if let Some(n) = census["raw_path"]["escapes"].as_u64().filter(|&n| n > 0) {
        let _ = writeln!(o, "  {:<11}{n} unparsed spans", "warning");
    }
    let mut head = String::new();
    let _ = writeln!(head, "{}", qw(file.as_bytes()));
    head + &o
}
