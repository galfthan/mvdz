//! Demo metadata: identity, verbatim server/roster strings, timing, and a
//! census of what the demo contains. Collected by a scan of the raw demo
//! (independent of the transform), so it also runs on plain `.mvd` files.
//!
//! Everything here is raw fact: names, counts, verbatim strings. There is
//! no interpretation (no mode classification, team assignment, identity
//! folding).

use crate::proto::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Caps on every census list.
pub const CAP_KEYS: usize = 64;
pub const CAP_LIST: usize = 256;
pub const CAP_VALUES: usize = 16;
pub const EXAMPLE_LEN: usize = 80;
/// Census strings (list entries, info keys and values, serverdata names)
/// are cut to this many bytes; stufftext keys to `KEY_LEN`.
pub const STR_LEN: usize = 1024;
pub const KEY_LEN: usize = 64;
/// A stufftext line is cut to this many bytes (the rest of it, up to its
/// newline, is dropped).
pub const LINE_LEN: usize = 1 << 16;

/// Wall-clock sources and pauses (FORMAT.md §3.1): caps.
pub const CLOCK_STR_LEN: usize = 128;
pub const PAUSE_RUNS_MAX: usize = 1 << 16;

/// Wall-clock facts of a demo, each with the demo time (ms) it appeared at.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    /// hidden block 0x000B: Unix ms at demo start
    pub start_ms: Option<(u64, u64)>,
    /// serverinfo `epoch`: Unix seconds, raw text
    pub epoch: Option<(u64, Vec<u8>)>,
    /// KTX `matchdate:` print: the raw date text after the prefix
    pub matchdate: Option<(u64, Vec<u8>)>,
    /// paused runs (demo ms, real ms paused there), demo ms increasing
    pub pauses: Vec<(u64, u64)>,
    /// more than PAUSE_RUNS_MAX runs: the list stops there
    pub pauses_truncated: bool,
}

impl Clock {
    fn pause(&mut self, t: u64, ms: u64) {
        if let Some(l) = self.pauses.last_mut().filter(|l| l.0 == t) {
            l.1 += ms;
        } else if self.pauses.len() < PAUSE_RUNS_MAX {
            self.pauses.push((t, ms));
        } else {
            self.pauses_truncated = true;
        }
    }
}

fn cut(b: &[u8], n: usize) -> Vec<u8> {
    b[..b.len().min(n)].to_vec()
}

/// A destination's unfinished stufftext line.
#[derive(Default)]
struct Pending {
    line: Vec<u8>,
    /// the line passed LINE_LEN: drop bytes until its newline
    cut: bool,
}

pub const SVC_NAMES: [&str; 56] = [
    "svc_bad",
    "svc_nop",
    "svc_disconnect",
    "svc_updatestat",
    "svc_version",
    "svc_setview",
    "svc_sound",
    "svc_time",
    "svc_print",
    "svc_stufftext",
    "svc_setangle",
    "svc_serverdata",
    "svc_lightstyle",
    "svc_updatename",
    "svc_updatefrags",
    "svc_clientdata",
    "svc_stopsound",
    "svc_updatecolors",
    "svc_particle",
    "svc_damage",
    "svc_spawnstatic",
    "svc_spawnbinary",
    "svc_spawnbaseline",
    "svc_temp_entity",
    "svc_setpause",
    "svc_signonnum",
    "svc_centerprint",
    "svc_killedmonster",
    "svc_foundsecret",
    "svc_spawnstaticsound",
    "svc_intermission",
    "svc_finale",
    "svc_cdtrack",
    "svc_sellscreen",
    "svc_smallkick",
    "svc_bigkick",
    "svc_updateping",
    "svc_updateentertime",
    "svc_updatestatlong",
    "svc_muzzleflash",
    "svc_updateuserinfo",
    "svc_download",
    "svc_playerinfo",
    "svc_nails",
    "svc_chokecount",
    "svc_modellist",
    "svc_soundlist",
    "svc_packetentities",
    "svc_deltapacketentities",
    "svc_maxspeed",
    "svc_entgravity",
    "svc_setinfo",
    "svc_serverinfo",
    "svc_updatepl",
    "svc_nails2",
    "svc_qizmomsg",
];
pub const DEM_NAMES: [&str; 8] = [
    "dem_cmd",
    "dem_read",
    "dem_set",
    "dem_multiple",
    "dem_single",
    "dem_stats",
    "dem_all",
    "dem_7",
];

/// Keys whose distinct userinfo values are listed (client identification).
const CLIENT_KEYS: [&[u8]; 3] = [b"*client", b"*version", b"*z_ext"];

#[derive(Default, Clone, Copy)]
struct Cb {
    count: u64,
    bytes: u64,
}

/// Incremental scanner. Feed whole frames (segment by segment, in order);
/// the last feed may end in unframeable bytes, which count as the tail.
pub struct Scanner {
    sha: Sha256,
    size: u64,
    proto: Proto,
    pub frames: u64,
    pub t_ms: u64,
    svc: Vec<Cb>,
    dem: [Cb; 8],
    hidden: BTreeMap<u32, Cb>,
    escapes: u64,
    esc_bytes: u64,
    first_escape: Option<(String, u64)>,
    tail: u64,
    pending: HashMap<u64, Pending>,
    stuff: BTreeMap<Vec<u8>, (u64, Vec<u8>)>,
    stuff_overflow: u64,
    serverinfo_keys: BTreeSet<Vec<u8>>,
    userinfo_keys: BTreeSet<Vec<u8>>,
    client_vals: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
    setinfo_updates: u64,
    serverinfo_updates: u64,
    fullserverinfo: u64,
    modellist: Vec<Vec<u8>>,
    modellist_dropped: u64,
    soundlist: Vec<Vec<u8>>,
    soundlist_dropped: u64,
    print: [u64; 5],
    centerprint: u64,
    stufftext: u64,
    max_msg: u64,
    serverdata: u64,
    sd_first: Option<(u32, u32, u32, u32, Vec<u8>, Vec<u8>)>,
    /// first fullserverinfo string, verbatim
    pub serverinfo: Option<Vec<u8>>,
    roster: BTreeMap<u8, (u32, Vec<u8>)>,
    roster_start: Option<BTreeMap<u8, (u32, Vec<u8>)>>,
    demo_start_ts: Option<u64>,
    clock: Clock,
    /// mvdhidden_demoinfo blocks in file order, each `[u32 len][body]`
    pub demoinfo: Vec<u8>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

fn str_at(b: &[u8]) -> &[u8] {
    let n = b.iter().position(|&x| x == 0).unwrap_or(b.len());
    &b[..n]
}

fn info_pairs(s: &[u8]) -> Vec<(&[u8], &[u8])> {
    // "\key\value\key\value"
    let mut out = Vec::new();
    let parts: Vec<&[u8]> = s.split(|&c| c == b'\\').collect();
    let mut i = 1;
    while i + 1 < parts.len() + 1 && i < parts.len() {
        let k = parts[i];
        let v = parts.get(i + 1).copied().unwrap_or(&[]);
        if !k.is_empty() {
            out.push((k, v));
        }
        i += 2;
    }
    out
}

impl Scanner {
    pub fn new() -> Scanner {
        Scanner {
            sha: Sha256::new(),
            size: 0,
            proto: Proto::default(),
            frames: 0,
            t_ms: 0,
            svc: vec![Cb::default(); 256],
            dem: [Cb::default(); 8],
            hidden: BTreeMap::new(),
            escapes: 0,
            esc_bytes: 0,
            first_escape: None,
            tail: 0,
            pending: HashMap::new(),
            stuff: BTreeMap::new(),
            stuff_overflow: 0,
            serverinfo_keys: BTreeSet::new(),
            userinfo_keys: BTreeSet::new(),
            client_vals: BTreeMap::new(),
            setinfo_updates: 0,
            serverinfo_updates: 0,
            fullserverinfo: 0,
            modellist: Vec::new(),
            modellist_dropped: 0,
            soundlist: Vec::new(),
            soundlist_dropped: 0,
            print: [0; 5],
            centerprint: 0,
            stufftext: 0,
            max_msg: 0,
            serverdata: 0,
            sd_first: None,
            serverinfo: None,
            roster: BTreeMap::new(),
            roster_start: None,
            demo_start_ts: None,
            clock: Clock::default(),
            demoinfo: Vec::new(),
        }
    }

    /// Scans `b` (whole frames; a final partial frame counts as tail).
    pub fn feed(&mut self, b: &[u8]) {
        self.sha.update(b);
        let base = self.size;
        self.size += b.len() as u64;
        let mut p = 0;
        while let Some(n) = frame_len(b, p) {
            self.frame(&b[p..p + n], base + p as u64);
            p += n;
        }
        self.tail += (b.len() - p) as u64;
    }

    fn escape(&mut self, what: String, off: u64, n: usize) {
        self.escapes += 1;
        self.esc_bytes += n as u64;
        if self.first_escape.is_none() {
            self.first_escape = Some((what, off));
        }
    }

    fn frame(&mut self, f: &[u8], off: u64) {
        self.frames += 1;
        self.t_ms += f[0] as u64;
        let ty = f[1];
        let d = &mut self.dem[(ty & 7) as usize];
        d.count += 1;
        d.bytes += f.len() as u64;
        let (dest, body, boff) = match ty & 7 {
            3 => {
                let mask = le32(&f[2..]);
                // older mvdsv builds wrote the paused-duration block without
                // its length field: a bare [u16 0x000A][u8 ms] payload
                // (current sv_demo.c SV_MVDWritePausedTime writes it framed)
                if mask == 0 && f.len() == 13 && f[10] == 0x0a && f[11] == 0 {
                    self.clock.pause(self.t_ms, f[12] as u64);
                }
                if mask == 0 {
                    self.hidden_payload(&f[10..], off + 10);
                    return;
                }
                (3u64 << 32 | mask as u64, &f[10..], off + 10)
            }
            1 | 4 | 5 | 6 => (((ty & 7) as u64) << 32 | (ty >> 3) as u64, &f[6..], off + 6),
            _ => return,
        };
        self.max_msg = self.max_msg.max(body.len() as u64);
        let mut i = 0;
        while i < body.len() {
            let cmd = body[i];
            let sz = match self.proto.cmd_size(cmd, &body[i + 1..]) {
                Some(s) if cmd != 0 => s,
                _ => {
                    self.escape(format!("svc {cmd}"), boff + i as u64, body.len() - i);
                    return;
                }
            };
            let c = &mut self.svc[cmd as usize];
            c.count += 1;
            c.bytes += 1 + sz as u64;
            self.command(cmd, &body[i + 1..i + 1 + sz], dest);
            i += 1 + sz;
        }
    }

    fn hidden_payload(&mut self, b: &[u8], off: u64) {
        // (the 0x000B body: see `demo_start_ms`)
        self.max_msg = self.max_msg.max(b.len() as u64);
        let mut i = 0;
        while i < b.len() {
            let rem = b.len() - i;
            let ok = rem >= 6 && {
                let bl = le32(&b[i..]) as u64;
                bl >= 1 && bl <= (rem - 6) as u64
            };
            if !ok {
                self.escape("hidden".into(), off + i as u64, rem);
                return;
            }
            let bl = le32(&b[i..]) as usize;
            let ty = u16::from_le_bytes([b[i + 4], b[i + 5]]) as u32;
            let data = &b[i + 6..i + 6 + bl];
            let h = self.hidden.entry(ty).or_default();
            h.count += 1;
            h.bytes += (6 + bl) as u64;
            if ty == 3 {
                self.demoinfo.extend_from_slice(&(bl as u32).to_le_bytes());
                self.demoinfo.extend_from_slice(data);
            }
            if ty == 0x0a && bl == 1 {
                self.clock.pause(self.t_ms, data[0] as u64);
            }
            if ty == 0x0b && self.demo_start_ts.is_none() {
                let v = demo_start_ms(data);
                self.demo_start_ts = Some(v);
                self.clock.start_ms = Some((self.t_ms, v));
            }
            i += 6 + bl;
        }
    }

    fn command(&mut self, cmd: u8, b: &[u8], dest: u64) {
        match cmd {
            8 => {
                self.print[(b[0] as usize).min(4)] += 1;
                let text = str_at(&b[1..]);
                if self.clock.matchdate.is_none() {
                    if let Some(rest) = text.strip_prefix(b"matchdate:") {
                        let line = rest
                            .split(|&c| c == b'\n')
                            .next()
                            .unwrap_or(&[])
                            .trim_ascii();
                        self.clock.matchdate = Some((self.t_ms, cut(line, CLOCK_STR_LEN)));
                    }
                }
            }
            9 => {
                self.stufftext += 1;
                // only the new bytes are scanned; a line is capped, so the
                // scan is linear in the stufftext
                let mut p = self.pending.remove(&dest).unwrap_or_default();
                let mut rest = str_at(b);
                loop {
                    let nl = rest.iter().position(|&c| c == b'\n');
                    let part = &rest[..nl.unwrap_or(rest.len())];
                    if !p.cut {
                        let room = LINE_LEN - p.line.len();
                        p.line.extend_from_slice(&part[..part.len().min(room)]);
                        p.cut = part.len() > room;
                    }
                    match nl {
                        Some(k) => {
                            let line = std::mem::take(&mut p.line);
                            self.stuff_line(&line);
                            p.cut = false;
                            rest = &rest[k + 1..];
                        }
                        None => break,
                    }
                }
                self.pending.insert(dest, p);
            }
            26 => self.centerprint += 1,
            11 => {
                self.serverdata += 1;
                self.proto.observe_server_data(b);
                if self.sd_first.is_none() {
                    // protocol extensions, then protocol, servercount, gamedir,
                    // demo time, level name
                    let mut i = 0;
                    let mut prot = 0;
                    while i + 4 <= b.len() {
                        let v = le32(&b[i..]);
                        i += 4;
                        if v == PROTOCOL_QW {
                            prot = v;
                            break;
                        }
                        i += 4;
                    }
                    i += 4;
                    let gd = str_at(b.get(i..).unwrap_or(&[])).to_vec();
                    i += gd.len() + 1 + 4;
                    let lv = str_at(b.get(i..).unwrap_or(&[])).to_vec();
                    self.sd_first = Some((
                        prot,
                        self.proto.fte,
                        self.proto.fte2,
                        self.proto.mvd1,
                        gd,
                        lv,
                    ));
                }
            }
            40 => {
                let slot = b[0];
                let uid = le32(&b[1..]);
                let s = str_at(&b[5..]).to_vec();
                for (k, v) in info_pairs(&s) {
                    if self.userinfo_keys.len() < CAP_KEYS {
                        self.userinfo_keys.insert(cut(k, STR_LEN));
                    }
                    if CLIENT_KEYS.contains(&k) {
                        let e = self.client_vals.entry(k.to_vec()).or_default();
                        if e.len() < CAP_VALUES {
                            e.insert(cut(v, STR_LEN));
                        }
                    }
                }
                self.roster.insert(slot, (uid, s));
            }
            42 => {
                if self.roster_start.is_none() {
                    self.roster_start = Some(self.roster.clone());
                }
            }
            45 | 46 => {
                let mut i = 1;
                loop {
                    let s = str_at(&b[i..]);
                    if s.is_empty() {
                        break;
                    }
                    let (list, dropped) = if cmd == 45 {
                        (&mut self.modellist, &mut self.modellist_dropped)
                    } else {
                        (&mut self.soundlist, &mut self.soundlist_dropped)
                    };
                    if list.len() < CAP_LIST {
                        list.push(cut(s, STR_LEN));
                    } else {
                        *dropped += 1;
                    }
                    i += s.len() + 1;
                }
            }
            51 => self.setinfo_updates += 1,
            52 => {
                self.serverinfo_updates += 1;
                let k = str_at(b);
                if k == b"epoch" && self.clock.epoch.is_none() {
                    let v = str_at(&b[k.len() + 1..]);
                    self.clock.epoch = Some((self.t_ms, cut(v, CLOCK_STR_LEN)));
                }
                if self.serverinfo_keys.len() < CAP_KEYS {
                    self.serverinfo_keys.insert(cut(k, STR_LEN));
                }
            }
            _ => {}
        }
    }

    fn stuff_line(&mut self, line: &[u8]) {
        // split on ';' outside double quotes, as the console does
        let mut cmds = Vec::new();
        let mut q = false;
        let mut s = 0;
        for (i, &c) in line.iter().enumerate() {
            if c == b'"' {
                q = !q;
            } else if c == b';' && !q {
                cmds.push(&line[s..i]);
                s = i + 1;
            }
        }
        cmds.push(&line[s..]);
        for c in cmds {
            let c = c.trim_ascii();
            if c.is_empty() {
                continue;
            }
            let mut toks = c
                .split(|x| x.is_ascii_whitespace())
                .filter(|t| !t.is_empty());
            let first = toks.next().unwrap_or(&[]);
            // a `//` directive takes its second token into the key when that
            // is a plain lowercase word (`//ktx took`), not data such as a
            // date or a quoted string
            let word = |t: &[u8]| {
                (1..=32).contains(&t.len())
                    && t[0].is_ascii_lowercase()
                    && t.iter()
                        .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
            };
            let key: Vec<u8> = match toks.next() {
                Some(t) if first.starts_with(b"//") && word(t) => [first, b" ", t].concat(),
                _ => first.to_vec(),
            };
            let key = cut(&key, KEY_LEN);
            if key == b"fullserverinfo" {
                self.fullserverinfo += 1;
                let rest = c[first.len()..].trim_ascii();
                let v = rest.strip_prefix(b"\"").unwrap_or(rest);
                let v = v.strip_suffix(b"\"").unwrap_or(v);
                for (k, x) in info_pairs(v) {
                    if k == b"epoch" && self.clock.epoch.is_none() {
                        self.clock.epoch = Some((self.t_ms, cut(x, CLOCK_STR_LEN)));
                    }
                    if self.serverinfo_keys.len() < CAP_KEYS {
                        self.serverinfo_keys.insert(cut(k, STR_LEN));
                    }
                }
                if self.serverinfo.is_none() {
                    self.serverinfo = Some(v.to_vec());
                }
            }
            let n = self.stuff.len();
            match self.stuff.get_mut(&key) {
                Some(e) => e.0 += 1,
                None if n < CAP_KEYS => {
                    self.stuff
                        .insert(key, (1, c[..c.len().min(EXAMPLE_LEN)].to_vec()));
                }
                None => self.stuff_overflow += 1,
            }
        }
    }

    pub fn finish(mut self) -> Scan {
        let digest: [u8; 32] = std::mem::take(&mut self.sha).finalize().into();
        let census = self.census_json();
        let roster = self
            .roster_start
            .clone()
            .unwrap_or_else(|| self.roster.clone());
        let (map, level) = {
            let m = self.modellist.first().cloned().unwrap_or_default();
            (
                m,
                self.sd_first
                    .as_ref()
                    .map(|s| s.5.clone())
                    .unwrap_or_default(),
            )
        };
        Scan {
            sha256: digest,
            size: self.size,
            frames: self.frames,
            duration_ms: self.t_ms,
            truncated: self.tail > 0,
            serverinfo: self.serverinfo.take().unwrap_or_default(),
            levelname: level,
            map,
            roster: roster.into_iter().map(|(s, (u, i))| (s, u, i)).collect(),
            demoinfo: std::mem::take(&mut self.demoinfo),
            census,
            clock: std::mem::take(&mut self.clock),
        }
    }

    fn census_json(&self) -> String {
        let mut j = Json::default();
        j.obj_open();
        j.key("census_version");
        j.num(1);
        j.key("size");
        j.num(self.size);
        j.key("frames");
        j.num(self.frames);
        j.key("duration_ms");
        j.num(self.t_ms);
        j.key("truncated");
        j.raw(if self.tail > 0 { "true" } else { "false" });
        j.key("tail_bytes");
        j.num(self.tail);
        j.key("max_message");
        j.num(self.max_msg);
        j.key("serverdata");
        j.obj_open();
        j.key("count");
        j.num(self.serverdata);
        if let Some((p, fte, fte2, mvd1, gd, lv)) = &self.sd_first {
            j.key("protocol");
            j.num(*p as u64);
            j.key("fte_pext");
            j.str(format!("0x{fte:08x}").as_bytes());
            j.key("fte_pext2");
            j.str(format!("0x{fte2:08x}").as_bytes());
            j.key("mvd_pext");
            j.str(format!("0x{mvd1:08x}").as_bytes());
            j.key("gamedir");
            j.str(&cut(gd, STR_LEN));
            j.key("levelname");
            j.str(&cut(lv, STR_LEN));
        }
        j.obj_close();
        j.key("svc");
        j.obj_open();
        for (i, c) in self.svc.iter().enumerate() {
            if c.count > 0 {
                let name = SVC_NAMES
                    .get(i)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("svc_{i}"));
                j.key(&name);
                j.pair(c.count, c.bytes);
            }
        }
        j.obj_close();
        j.key("dem");
        j.obj_open();
        for (i, c) in self.dem.iter().enumerate() {
            if c.count > 0 {
                j.key(DEM_NAMES[i]);
                j.pair(c.count, c.bytes);
            }
        }
        j.obj_close();
        j.key("hidden");
        j.obj_open();
        for (t, c) in &self.hidden {
            j.key(&format!("0x{t:04x}"));
            j.pair(c.count, c.bytes);
        }
        j.obj_close();
        if let Some(ts) = self.demo_start_ts {
            j.key("demo_start_timestamp_ms");
            j.num(ts);
        }
        j.key("clock");
        j.obj_open();
        if let Some((t, v)) = self.clock.start_ms {
            j.key("start_timestamp_ms");
            j.raw(&format!("[{t},{v}]"));
        }
        if let Some((t, v)) = &self.clock.epoch {
            j.key("epoch");
            j.arr_open();
            j.num(*t);
            j.str(v);
            j.arr_close();
        }
        if let Some((t, v)) = &self.clock.matchdate {
            j.key("matchdate");
            j.arr_open();
            j.num(*t);
            j.str(v);
            j.arr_close();
        }
        j.key("pause_runs");
        j.num(self.clock.pauses.len() as u64);
        j.key("paused_ms");
        j.num(self.clock.pauses.iter().map(|p| p.1).sum());
        j.obj_close();
        j.key("raw_path");
        j.obj_open();
        j.key("escapes");
        j.num(self.escapes);
        j.key("bytes");
        j.num(self.esc_bytes);
        if let Some((w, o)) = &self.first_escape {
            j.key("first");
            j.str(w.as_bytes());
            j.key("first_offset");
            j.num(*o);
        }
        j.obj_close();
        j.key("stufftext");
        j.obj_open();
        j.key("messages");
        j.num(self.stufftext);
        j.key("commands");
        j.obj_open();
        for (k, (n, ex)) in &self.stuff {
            j.key_bytes(k);
            j.obj_open();
            j.key("count");
            j.num(*n);
            j.key("example");
            j.str(ex);
            j.obj_close();
        }
        j.obj_close();
        j.key("overflow");
        j.num(self.stuff_overflow);
        j.obj_close();
        j.key("serverinfo_keys");
        j.list(self.serverinfo_keys.iter());
        j.key("userinfo_keys");
        j.list(self.userinfo_keys.iter());
        j.key("client_values");
        j.obj_open();
        for (k, v) in &self.client_vals {
            j.key_bytes(k);
            j.list(v.iter());
        }
        j.obj_close();
        j.key("updates");
        j.obj_open();
        j.key("setinfo");
        j.num(self.setinfo_updates);
        j.key("serverinfo");
        j.num(self.serverinfo_updates);
        j.key("fullserverinfo");
        j.num(self.fullserverinfo);
        j.obj_close();
        j.key("modellist");
        j.list(self.modellist.iter());
        j.key("modellist_dropped");
        j.num(self.modellist_dropped);
        j.key("soundlist");
        j.list(self.soundlist.iter());
        j.key("soundlist_dropped");
        j.num(self.soundlist_dropped);
        j.key("print");
        j.obj_open();
        for (lv, &n) in self.print.iter().enumerate() {
            if n > 0 {
                j.key(if lv < 4 {
                    ["0", "1", "2", "3"][lv]
                } else {
                    "other"
                });
                j.num(n);
            }
        }
        j.obj_close();
        j.key("centerprint");
        j.num(self.centerprint);
        j.obj_close();
        j.s
    }
}

/// Result of a scan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub sha256: [u8; 32],
    pub size: u64,
    pub frames: u64,
    pub duration_ms: u64,
    pub truncated: bool,
    pub serverinfo: Vec<u8>,
    pub levelname: Vec<u8>,
    pub map: Vec<u8>,
    /// (slot, userid, userinfo) at the first playerinfo (else at the end)
    pub roster: Vec<(u8, u32, Vec<u8>)>,
    pub demoinfo: Vec<u8>,
    pub census: String,
    pub clock: Clock,
}

/// Whether `b` starts like an MVD demo: `Some(true)` or `Some(false)`
/// once the start of `b` decides it, `None` while more bytes are needed.
/// A demo starts with a whole frame that carries messages (a non-empty
/// `dem_read`/`dem_all`/... payload, or hidden or multi-target data; not a
/// bare `dem_set`), and either its messages parse or, if that first frame
/// is damaged, the next 16 frames are whole too. A frame's length field
/// must fit the input, so other files almost never frame: text, gzip,
/// PNG, TIFF and random bytes fail on the first frame.
pub fn mvd_start(b: &[u8]) -> Option<bool> {
    let bad = |b: &[u8], p: usize| b.len() >= p + 2 && matches!(b[p + 1] & 7, 0 | 7);
    if b.len() < 2 {
        return None;
    }
    let head = match b[1] & 7 {
        0 | 2 | 7 => return Some(false),
        3 => 10,
        _ => 6,
    };
    let n = frame_len(b, 0)?;
    if n <= head {
        return Some(false);
    }
    let mut s = Scanner::new();
    s.feed(&b[..n]);
    if s.escapes == 0 {
        return Some(true);
    }
    let mut p = n;
    for _ in 0..16 {
        if bad(b, p) {
            return Some(false);
        }
        p += frame_len(b, p)?;
    }
    Some(true)
}

/// `mvd_start` of a whole input: a start that is not decided by the end of
/// the input is not a demo.
pub fn starts_like_mvd(b: &[u8]) -> bool {
    mvd_start(b) == Some(true)
}

pub fn scan(b: &[u8]) -> Scan {
    let mut s = Scanner::new();
    s.feed(b);
    s.finish()
}

/// Minimal JSON writer. Strings are bytes: printable ASCII is written as
/// is, every other byte as \u00XX (so the bytes are recoverable exactly,
/// read as Latin-1).
#[derive(Default)]
pub struct Json {
    pub s: String,
    first: Vec<bool>,
}

impl Json {
    fn sep(&mut self) {
        if let Some(f) = self.first.last_mut() {
            if !*f {
                self.s.push(',');
            }
            *f = false;
        }
    }
    pub fn obj_open(&mut self) {
        self.sep();
        self.s.push('{');
        self.first.push(true);
    }
    pub fn obj_close(&mut self) {
        self.first.pop();
        self.s.push('}');
    }
    pub fn arr_open(&mut self) {
        self.sep();
        self.s.push('[');
        self.first.push(true);
    }
    pub fn arr_close(&mut self) {
        self.first.pop();
        self.s.push(']');
    }
    pub fn key(&mut self, k: &str) {
        self.key_bytes(k.as_bytes());
    }
    pub fn key_bytes(&mut self, k: &[u8]) {
        self.sep();
        esc(&mut self.s, k);
        self.s.push(':');
        // the value follows without a separator
        if let Some(f) = self.first.last_mut() {
            *f = true;
        }
    }
    pub fn num(&mut self, v: u64) {
        self.sep();
        self.s += &v.to_string();
    }
    pub fn raw(&mut self, v: &str) {
        self.sep();
        self.s += v;
    }
    pub fn str(&mut self, v: &[u8]) {
        self.sep();
        esc(&mut self.s, v);
    }
    pub fn pair(&mut self, a: u64, b: u64) {
        self.sep();
        self.s += &format!("[{a},{b}]");
    }
    pub fn list<'a>(&mut self, it: impl Iterator<Item = &'a Vec<u8>>) {
        self.arr_open();
        for v in it {
            self.str(v);
        }
        self.arr_close();
    }
}

pub fn esc(out: &mut String, b: &[u8]) {
    out.push('"');
    for &c in b {
        match c {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(c as char),
            _ => out.push_str(&format!("\\u{:04x}", c)),
        }
    }
    out.push('"');
}

/// Reassembles the KTX demoinfo JSON from the block store (`[u32 len]
/// [body]` per block, body = `[u16 block number][text]`) as the mvd_analyzer project
/// does: for each block number the last block wins (a demo can carry the
/// demoinfo more than once), then blocks 1, 2, ..., n and block 0 (the
/// last) are concatenated. None if the store does not parse.
pub fn demoinfo_json(raw: &[u8]) -> Option<Vec<u8>> {
    let mut blocks: BTreeMap<u16, &[u8]> = BTreeMap::new();
    let mut i = 0;
    while i < raw.len() {
        let n = le32(raw.get(i..i + 4)?) as usize;
        let b = raw.get(i + 4..i + 4 + n)?;
        if b.len() < 2 {
            return None;
        }
        blocks.insert(u16::from_le_bytes([b[0], b[1]]), &b[2..]);
        i += 4 + n;
    }
    if blocks.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for (_, b) in blocks.range(1..) {
        out.extend_from_slice(b);
    }
    if let Some(b) = blocks.get(&0) {
        out.extend_from_slice(b);
    }
    Some(out)
}

/// The value of a hidden 0x000B block (mvdhidden_demo_start_timestamp_ms,
/// FORMAT.md §3 tag 11). Current mvdsv writes a ULEB128
/// (`SV_MVDEmbedStartTimestamp`); earlier builds wrote a fixed 8-byte
/// little-endian u64. An 8-byte body that the LEB128 does not consume
/// exactly is the u64; anything else is read as LEB128 (at most 10 bytes,
/// stopping at the first byte below 0x80 or at the end).
pub fn demo_start_ms(b: &[u8]) -> u64 {
    let leb_len = b.iter().position(|&x| x < 0x80).map_or(b.len(), |i| i + 1);
    if b.len() == 8 && leb_len != 8 {
        return u64::from_le_bytes(b.try_into().unwrap());
    }
    let mut v = 0u64;
    for (k, &x) in b.iter().take(10).enumerate() {
        v |= ((x & 0x7f) as u64) << (7 * k);
        if x < 0x80 {
            break;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    #[test]
    fn demo_start_forms() {
        // the u64le form of older mvdsv builds (final spec review D1)
        assert_eq!(
            super::demo_start_ms(&[0x8a, 0xdd, 0x59, 0x21, 0x9e, 0x01, 0, 0]),
            1778675998090
        );
        // the ULEB128 form of current mvdsv
        let mut b = Vec::new();
        crate::util::put_uv(&mut b, 1778675998090);
        assert_eq!(b.len(), 6);
        assert_eq!(super::demo_start_ms(&b), 1778675998090);
        // an 8-byte LEB128 is read as LEB128
        let mut b = Vec::new();
        crate::util::put_uv(&mut b, 1 << 50);
        assert_eq!(b.len(), 8);
        assert_eq!(super::demo_start_ms(&b), 1 << 50);
    }
}
