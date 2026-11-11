//! Structure-aware MVD generator for the review fuzz targets: turns a
//! byte string into a demo whose frames and svc commands are mostly
//! well-formed for the protocol state in force (so the walker reaches its
//! deep paths), with occasional garbage, lies and escapes.
#![allow(dead_code)]

use mvdz::proto::Proto;

pub struct Ch<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Ch<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Ch { b, i: 0 }
    }
    pub fn done(&self) -> bool {
        self.i >= self.b.len()
    }
    pub fn u8(&mut self) -> u8 {
        let v = self.b.get(self.i).copied().unwrap_or(0);
        self.i += 1;
        v
    }
    pub fn u16(&mut self) -> u16 {
        self.u8() as u16 | (self.u8() as u16) << 8
    }
    pub fn u32(&mut self) -> u32 {
        self.u16() as u32 | (self.u16() as u32) << 16
    }
    pub fn below(&mut self, n: usize) -> usize {
        self.u8() as usize % n.max(1)
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.u8()).collect()
    }
    pub fn text(&mut self, n: usize) -> Vec<u8> {
        const A: &[u8] = b"abc \\\";\n/xyz0129matchdate:matchkey:epoch";
        (0..n).map(|_| A[self.below(A.len())]).collect()
    }
}

fn z(v: &mut Vec<u8>, s: &[u8]) {
    v.extend(s.iter().map(|&c| if c == 0 { b'.' } else { c }));
    v.push(0);
}

fn delta(c: &mut Ch, p: &Proto, out: &mut Vec<u8>, ent: u16) {
    let hi = c.u8() as u16 & 0x7f;
    let mut w = (hi << 9) | (ent & 511);
    if w == 0 {
        w = 1 << 9;
    }
    out.extend_from_slice(&w.to_le_bytes());
    let mut bits = (w as u32) & !511;
    let mut more = 0u32;
    if bits & (1 << 15) != 0 {
        let m = c.u8();
        bits |= m as u32;
        out.push(m);
    }
    if bits & (1 << 7) != 0 && p.fte != 0 {
        let m = c.u8();
        more = m as u32;
        out.push(m);
        if m & 0x80 != 0 {
            let m2 = c.u8() & 0x07;
            more |= (m2 as u32) << 8;
            out.push(m2);
        }
    }
    if bits & (1 << 2) != 0 {
        out.push(c.u8());
    } else if more & (1 << 3) != 0 {
        out.extend(c.bytes(2));
    }
    for m in [1 << 13, 1 << 3, 1 << 4, 1 << 5] {
        if bits & m != 0 {
            out.push(c.u8());
        }
    }
    for (o, a) in [(1 << 9, 1), (1 << 10, 1 << 12), (1 << 11, 1 << 1)] {
        if bits & o != 0 {
            out.extend(c.bytes(p.coord()));
        }
        if bits & a != 0 {
            out.extend(c.bytes(p.angle()));
        }
    }
    if more & 2 != 0 && p.fte & 8 != 0 {
        out.push(c.u8());
    }
    if more & 1 != 0 {
        out.push(c.u8());
    }
    if more & 4 != 0 {
        out.push(c.u8());
    }
    if more & (1 << 10) != 0 && p.fte & 0x80000 != 0 {
        out.extend(c.bytes(3));
    }
}

/// One svc command (cmd byte + body) for the protocol state `p`; updates
/// `p` on serverdata.
fn command(c: &mut Ch, p: &mut Proto, out: &mut Vec<u8>) {
    let k = c.below(40);
    let cw = p.coord();
    match k {
        0..=6 => {
            // playerinfo
            out.push(42);
            out.push(c.below(34) as u8);
            let f = c.u16() & 0x1fff | if c.below(8) == 0 { 0x8000 } else { 0 };
            out.extend_from_slice(&f.to_le_bytes());
            out.push(c.u8());
            for i in 0..3 {
                if f & (1 << i) != 0 {
                    out.extend(c.bytes(cw));
                }
            }
            for i in 0..3 {
                if f & (8 << i) != 0 {
                    out.extend(c.bytes(2));
                }
            }
            for m in [1u16 << 11, 1 << 7, 1 << 6, 1 << 10] {
                if f & m != 0 {
                    out.push(c.u8());
                }
            }
        }
        7..=11 => {
            // packet entities
            let delta_pkt = c.below(2) == 1;
            out.push(if delta_pkt { 48 } else { 47 });
            if delta_pkt {
                out.push(c.u8());
            }
            let n = c.below(12);
            let mut e = c.below(40) as u16;
            for _ in 0..n {
                delta(c, p, out, e);
                let lim = if c.below(16) == 0 { 400 } else { 4 };
                e = e.wrapping_add(1 + c.below(lim) as u16);
            }
            out.extend_from_slice(&[0, 0]);
        }
        12..=14 => {
            out.push(6);
            let ch = c.u16();
            out.extend_from_slice(&ch.to_le_bytes());
            if ch & 0x8000 != 0 {
                out.push(c.u8());
            }
            if ch & 0x4000 != 0 {
                out.push(c.u8());
            }
            out.push(c.u8());
            out.extend(c.bytes(3 * cw));
        }
        15..=17 => {
            out.push(23);
            let t = c.below(15) as u8;
            out.push(t);
            match t {
                5 | 6 | 9 => {
                    let e = if c.below(2) == 0 {
                        1 + c.below(32) as u16
                    } else {
                        c.u16()
                    };
                    out.extend_from_slice(&e.to_le_bytes());
                    out.extend(c.bytes(6 * cw));
                }
                2 | 12 => {
                    out.push(c.u8());
                    out.extend(c.bytes(3 * cw));
                }
                14 => out.extend(c.bytes(3 * cw)), // unknown TE: escape
                _ => out.extend(c.bytes(3 * cw)),
            }
        }
        18 => {
            out.push(3);
            out.extend(c.bytes(2));
        }
        19 => {
            out.push(38);
            out.extend(c.bytes(5));
        }
        20 => {
            out.push(14);
            out.extend(c.bytes(3));
        }
        21 => {
            out.push(8);
            out.push(c.below(5) as u8);
            let n = c.below(40);
            let mut t = if c.below(4) == 0 {
                b"matchdate: 2024-01-02 03:04:05 CET\n".to_vec()
            } else if c.below(4) == 0 {
                b"matchkey: 8-2005-".to_vec()
            } else {
                Vec::new()
            };
            t.extend(c.text(n));
            z(out, &t);
        }
        22 => {
            out.push(9);
            let n = c.below(60);
            let mut t = if c.below(3) == 0 {
                b"fullserverinfo \"\\epoch\\1700000000\\map\\dm4\"\n".to_vec()
            } else {
                Vec::new()
            };
            t.extend(c.text(n));
            z(out, &t);
        }
        23 => {
            let cmd = [36u8, 37, 53][c.below(3)];
            out.push(cmd);
            out.extend(c.bytes(match cmd {
                36 => 3,
                37 => 5,
                _ => 2,
            }));
        }
        24 => {
            let cmd = if c.below(2) == 0 { 43 } else { 54 };
            out.push(cmd);
            let n = c.below(6);
            out.push(n as u8);
            out.extend(c.bytes(n * if cmd == 43 { 6 } else { 7 }));
        }
        25 => {
            // serverdata with protocol extensions
            out.push(11);
            let mut b = Vec::new();
            for _ in 0..c.below(4) {
                let (tag, v) = match c.below(4) {
                    0 => (
                        0x5845_5446u32,
                        [0u32, 0x8000, 0x8 | 0x8000 | 0x80000, 0x8 | 0x80000, c.u32()][c.below(5)],
                    ),
                    1 => (0x3245_5446, c.u32()),
                    2 => (0x3144_564D, [0u32, 1, c.u32()][c.below(3)]),
                    _ => (c.u32() | 0x100, c.u32()),
                };
                b.extend_from_slice(&tag.to_le_bytes());
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(&28u32.to_le_bytes());
            b.extend(c.bytes(4));
            z(&mut b, b"qw");
            b.extend(c.bytes(4));
            let n = c.below(12);
            let t = c.text(n);
            z(&mut b, &t);
            b.extend(c.bytes(40));
            p.observe_server_data(&b);
            out.extend(b);
        }
        26 => {
            out.push(40);
            out.push(c.below(40) as u8);
            out.extend(c.bytes(4));
            let n = c.below(30);
            let mut t = b"\\name\\".to_vec();
            t.extend(c.text(n));
            z(out, &t);
        }
        27 => {
            out.push(if c.below(2) == 0 { 45 } else { 46 });
            out.push(c.u8());
            for _ in 0..c.below(4) {
                let n = 1 + c.below(10);
                let t = c.text(n);
                z(out, &t);
            }
            out.push(0);
            out.push(c.u8());
        }
        28 => {
            out.push(52);
            z(out, if c.below(2) == 0 { b"epoch" } else { b"k" });
            let n = c.below(10);
            let t = c.text(n);
            z(out, &t);
        }
        29 => {
            out.push(51);
            out.push(c.u8());
            z(out, b"k");
            z(out, b"v");
        }
        30 => {
            // FTE spawnbaseline2 / spawnbaseline
            if c.below(2) == 0 {
                out.push(22);
                out.extend(c.bytes(6 + 3 * (cw + p.angle())));
            } else {
                out.push(66);
                let e = c.below(512) as u16;
                delta(c, p, out, e);
            }
        }
        31 => {
            out.push(41);
            let sz = c.below(20) as u16;
            out.extend_from_slice(&sz.to_le_bytes());
            out.push(c.u8());
            out.extend(c.bytes(sz as usize));
        }
        32 => {
            out.push(10);
            out.extend(c.bytes(1 + 3 * p.angle()));
        }
        33 => {
            out.push(30);
            out.extend(c.bytes(3 * p.wcoord() + 3 * p.angle()));
        }
        34 => {
            // a random command byte with random bytes
            out.push(c.u8());
            let n = c.below(8);
            out.extend(c.bytes(n));
        }
        35 => {
            out.push([1u8, 27, 28, 34, 35][c.below(5)]);
        }
        _ => {
            out.push([24u8, 32, 44, 49, 50, 12, 19, 20, 29, 39][c.below(10)]);
            let last = *out.last().unwrap();
            match last {
                24 | 32 | 44 => out.push(c.u8()),
                49 | 50 => out.extend(c.bytes(4)),
                12 => {
                    out.push(c.u8());
                    z(out, b"m");
                }
                19 => out.extend(c.bytes(2 + 3 * cw)),
                20 => out.extend(c.bytes(4 + 3 * (cw + p.angle()))),
                29 => out.extend(c.bytes(3 * cw + 3)),
                _ => out.extend(c.bytes(2)),
            }
        }
    }
}

fn hidden(c: &mut Ch, out: &mut Vec<u8>) {
    if c.below(6) == 0 {
        // mvdsv's bare paused-duration payload
        out.extend_from_slice(&[0x0a, 0, c.u8()]);
        return;
    }
    for _ in 0..1 + c.below(4) {
        let (ty, body): (u16, Vec<u8>) = match c.below(8) {
            0 | 1 => {
                let mut b = (c.below(4) as u16).to_le_bytes().to_vec();
                let n = c.below(50);
                b.extend(c.text(n));
                (3, b)
            }
            2 => (7, c.bytes(8)),
            3 => (0x0a, vec![c.u8()]),
            4 => (0x0b, vec![0x80 | c.u8(), 0x80 | c.u8(), c.u8() & 0x7f]),
            5 => {
                let n = c.below(10);
                (c.u16(), c.bytes(n))
            }
            7 => {
                // a usercmd: angles mostly on the 360/65536 grid
                let mut b = vec![c.below(8) as u8, c.below(2) as u8, 13];
                for _ in 0..3 {
                    let a = if c.below(8) == 0 {
                        c.u32()
                    } else {
                        ((c.u16() as i16 as f32) * (45.0 / 8192.0)).to_bits()
                    };
                    b.extend_from_slice(&a.to_le_bytes());
                }
                b.extend(c.bytes(8));
                (1, b)
            }
            _ => {
                let n = c.below(10);
                (7, c.bytes(n))
            }
        };
        let lie = c.below(16) == 0;
        let bl = if lie { c.u32() % 64 } else { body.len() as u32 };
        out.extend_from_slice(&bl.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend(body);
    }
    if c.below(8) == 0 {
        let n = c.below(5);
        out.extend(c.bytes(n));
    }
}

/// A demo from the choice stream.
pub fn demo(data: &[u8]) -> Vec<u8> {
    let mut c = Ch::new(data);
    let mut p = Proto::default();
    let mut out = Vec::new();
    while !c.done() {
        let tb = if c.below(4) == 0 {
            c.u8()
        } else {
            c.below(30) as u8
        };
        let kind = c.below(20);
        let target = (c.below(8) as u8) << 3;
        match kind {
            0 => {
                // dem_set
                out.push(tb);
                out.push(2 | target);
                out.extend(c.bytes(8));
            }
            1..=3 => {
                // hidden
                let mut pl = Vec::new();
                hidden(&mut c, &mut pl);
                out.push(tb);
                out.push(3);
                out.extend_from_slice(&0u32.to_le_bytes());
                out.extend_from_slice(&(pl.len() as u32).to_le_bytes());
                out.extend(pl);
            }
            4 => {
                // raw garbage (ends framing, or not)
                let n = c.below(16);
                out.extend(c.bytes(n));
            }
            5 => {
                // empty payload frame / zero-length
                out.push(tb);
                out.push([1u8, 4, 5, 6, 3][c.below(5)] | target);
                if out.last().unwrap() & 7 == 3 {
                    out.extend(c.bytes(4));
                }
                out.extend_from_slice(&0u32.to_le_bytes());
            }
            _ => {
                let ty = [1u8, 3, 4, 5, 6, 6, 6, 4][c.below(8)];
                let mut pl = Vec::new();
                for _ in 0..1 + c.below(8) {
                    command(&mut c, &mut p, &mut pl);
                }
                out.push(tb);
                if ty == 3 {
                    out.push(3);
                    let mask = c.u32() | 1;
                    out.extend_from_slice(&mask.to_le_bytes());
                } else {
                    out.push(ty | target);
                }
                out.extend_from_slice(&(pl.len() as u32).to_le_bytes());
                out.extend(pl);
            }
        }
    }
    out
}
