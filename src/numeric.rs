//! Numeric groups: predictors and side contexts for the numeric fields.
//!
//! Each group walks one record list (written by the walker) in order,
//! predicts each numeric field from group-local state, and codes the
//! residual through a [`NumIO`]. The same code runs in both directions: on
//! encode it reads the value from the input buffer, on decode it writes the
//! reconstructed value into the output buffer at the recorded offset.
//! Groups in wave 1 are independent of each other; wave 2 (sounds, temp
//! entities) reads player and entity origins that wave 1 wrote.

use crate::streams::*;
use crate::walk::*;

/// Side features of one numeric value. Backends pick which to use.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct NCtx {
    pub f: [u8; 6],
}

pub trait NumIO {
    /// Encode: record `v` on local stream `k`, return it. Decode: return the
    /// decoded residual.
    fn code(&mut self, k: usize, ctx: NCtx, v: i32) -> i32;
}

/// View of the segment buffer. On encode it is the input (never written);
/// on decode the output. Every offset comes from the walker's monotonic
/// cursor, so no two records share a byte.
#[derive(Clone, Copy)]
pub struct Buf {
    ptr: *mut u8,
    len: usize,
}

impl Buf {
    pub fn new(b: &mut [u8]) -> Buf {
        Buf {
            ptr: b.as_mut_ptr(),
            len: b.len(),
        }
    }
    pub fn ro(b: &[u8]) -> Buf {
        Buf {
            ptr: b.as_ptr() as *mut u8,
            len: b.len(),
        }
    }
    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline(always)]
    pub fn rd16(&self, off: u32) -> u16 {
        let o = off as usize;
        if o.wrapping_add(2) <= self.len && o < self.len {
            unsafe { u16::from_le_bytes([*self.ptr.add(o), *self.ptr.add(o + 1)]) }
        } else {
            0
        }
    }
    #[inline(always)]
    pub fn rd32(&self, off: u32) -> u32 {
        let o = off as usize;
        if o.wrapping_add(4) <= self.len && o < self.len {
            unsafe {
                (self.ptr.add(o) as *const [u8; 4])
                    .read_unaligned()
                    .map(|x| x)
                    .into_le()
            }
        } else {
            0
        }
    }
    #[inline(always)]
    pub fn wr16(&self, off: u32, v: u16) {
        let o = off as usize;
        if o.wrapping_add(2) <= self.len && o < self.len {
            unsafe {
                let b = v.to_le_bytes();
                *self.ptr.add(o) = b[0];
                *self.ptr.add(o + 1) = b[1];
            }
        }
    }
    #[inline(always)]
    pub fn wr32(&self, off: u32, v: u32) {
        let o = off as usize;
        if o.wrapping_add(4) <= self.len && o < self.len {
            unsafe { (self.ptr.add(o) as *mut [u8; 4]).write_unaligned(v.to_le_bytes()) }
        }
    }
}

trait IntoLe {
    fn into_le(self) -> u32;
}
impl IntoLe for [u8; 4] {
    #[inline(always)]
    fn into_le(self) -> u32 {
        u32::from_le_bytes(self)
    }
}

/// Sign and bit-length bucket of a residual: 0 for zero, else
/// 2*bitlen(|v|) - 1 + sign (0..=64).
#[inline(always)]
pub fn q(v: i32) -> u8 {
    if v == 0 {
        return 0;
    }
    let neg = (v < 0) as u32;
    let m = v.unsigned_abs();
    let n = 32 - m.leading_zeros();
    (2 * n - 1 + neg) as u8
}

/// floor(2^40 / d) + 1: n * RECIP[d] >> 40 == n / d exactly for
/// 0 <= n < 2^31, 1 <= d <= 255 (the error n/2^40 < 2^-9 < 1/d).
static RECIP: [u64; 256] = {
    let mut t = [0u64; 256];
    let mut d = 1;
    while d < 256 {
        t[d] = (1u64 << 40) / d as u64 + 1;
        d += 1;
    }
    t
};

/// Rounded division a / b for b > 0 (halves away from zero).
#[inline(always)]
fn div_round(a: i32, b: i32) -> i32 {
    let n = a.unsigned_abs() + (b as u32) / 2;
    let q = if b < 256 {
        ((n as u128 * RECIP[b as usize] as u128) >> 40) as u32
    } else {
        n / b as u32
    } as i32;
    if a < 0 { -q } else { q }
}

/// Frame intervals are clamped so the scaled velocity fits 32 bits.
pub const DT_MAX: i32 = 32767;

/// dt-scaled linear prediction: last + (last - prev) * dt_now / dt_prev,
/// rounded (wrapping 16-bit arithmetic). Intervals are clamped to DT_MAX.
#[inline(always)]
fn pred_dt(last: u16, prev: u16, dt_now: i32, dt_prev: i32) -> u16 {
    let d = (last.wrapping_sub(prev)) as i16 as i32;
    if dt_now == dt_prev || dt_prev <= 0 || dt_now < 0 {
        last.wrapping_add(d as u16)
    } else {
        last.wrapping_add(div_round(d * dt_now.min(DT_MAX), dt_prev.min(DT_MAX)) as u16)
    }
}

#[inline(always)]
fn x16<const ENC: bool, IO: NumIO>(
    buf: Buf,
    off: u32,
    pred: u16,
    io: &mut IO,
    k: usize,
    c: NCtx,
) -> (u16, i32) {
    if ENC {
        let v = buf.rd16(off);
        let r = v.wrapping_sub(pred) as i16 as i32;
        io.code(k, c, r);
        (v, r)
    } else {
        let r = io.code(k, c, 0);
        let v = pred.wrapping_add(r as u16);
        buf.wr16(off, v);
        (v, r as u16 as i16 as i32)
    }
}

#[inline(always)]
fn x32<const ENC: bool, IO: NumIO>(
    buf: Buf,
    off: u32,
    pred: u32,
    io: &mut IO,
    k: usize,
    c: NCtx,
) -> (u32, i32) {
    if ENC {
        let v = buf.rd32(off);
        let r = v.wrapping_sub(pred) as i32;
        io.code(k, c, r);
        (v, r)
    } else {
        let r = io.code(k, c, 0);
        let v = pred.wrapping_add(r as u32);
        buf.wr32(off, v);
        (v, r)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    PiO(u8),
    PiA(u8),
    PeO(u8),
    Stl,
    /// usercmd angles on the grid
    Uc,
    Snd,
    Te,
}

/// Wave 1, per axis.
pub const WAVE1: [Group; 11] = [
    Group::PiA(1),
    Group::PiA(0),
    Group::PiO(0),
    Group::PiO(1),
    Group::PiO(2),
    Group::PeO(0),
    Group::PeO(1),
    Group::PeO(2),
    Group::PiA(2),
    Group::Stl,
    Group::Uc,
];
pub const WAVE2: [Group; 2] = [Group::Snd, Group::Te];

impl Group {
    /// Stream ids by local index.
    pub fn streams(self) -> &'static [usize] {
        match self {
            Group::PiO(0) => &[PI_OX, PI_F0],
            Group::PiO(1) => &[PI_OX + 1, PI_F0 + 1],
            Group::PiO(_) => &[PI_OX + 2, PI_F0 + 2],
            Group::PiA(0) => &[PI_AP],
            Group::PiA(1) => &[PI_AP + 1],
            Group::PiA(_) => &[PI_AP + 2],
            Group::PeO(0) => &[PE_OX, PE_F0],
            Group::PeO(1) => &[PE_OX + 1, PE_F0 + 1],
            Group::PeO(_) => &[PE_OX + 2, PE_F0 + 2],
            Group::Stl => &[STL_VAL],
            Group::Uc => &[UC_A],
            Group::Snd => &[SND_O],
            Group::Te => &[TE_O],
        }
    }

    pub fn run<const ENC: bool, IO: NumIO>(self, recs: &Recs, buf: Buf, io: &mut IO) {
        match self {
            Group::PiO(a) => {
                let mut st = vec![Track::default(); 256];
                let mut tm = vec![(0u32, 0u32); 256];
                for r in recs.pi.iter() {
                    let (dn, dp) = times(&mut tm[r.num as usize], r.t);
                    pi_origin_step::<ENC, IO>(
                        a as usize,
                        &mut st[r.num as usize],
                        r,
                        dn,
                        dp,
                        buf,
                        io,
                    );
                }
            }
            Group::PiA(a) => {
                let mut st = vec![Track::default(); 256];
                let mut tm = vec![(0u32, 0u32); 256];
                for r in recs.pi.iter() {
                    let (dn, dp) = times(&mut tm[r.num as usize], r.t);
                    pi_angle_step::<ENC, IO>(
                        a as usize,
                        &mut st[r.num as usize],
                        r,
                        dn,
                        dp,
                        buf,
                        io,
                    );
                }
            }
            Group::PeO(a) => {
                let mut st = vec![Track::default(); 2048];
                for r in recs.pe.iter() {
                    pe_origin_step::<ENC, IO>(
                        a as usize,
                        &mut st[(r.num & 2047) as usize],
                        r,
                        buf,
                        io,
                    );
                }
            }
            Group::Stl => stl::<ENC, IO>(recs, buf, io),
            Group::Uc => uc::<ENC, IO>(recs, buf, io),
            Group::Snd => snd::<ENC, IO>(recs, buf, io),
            Group::Te => te::<ENC, IO>(recs, buf, io),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Track {
    last: u16,
    prev: u16,
    prev2: u16,
    res: i32,
    // float-coord state (IEEE bit patterns)
    flast: u32,
    fprev: u32,
    fres: i32,
    // entity times
    t_last: u32,
}

/// Per-player frame times: returns (dt_now, dt_prev) and advances.
#[inline(always)]
fn times(tm: &mut (u32, u32), t: u32) -> (i32, i32) {
    let (t_last, t_prev) = *tm;
    *tm = (t, t_last);
    (
        t.wrapping_sub(t_last) as i32,
        t_last.wrapping_sub(t_prev) as i32,
    )
}

#[inline(always)]
fn dd(now: i32, prev: i32) -> u8 {
    ((now - prev).clamp(-3, 3) + 3) as u8
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn pi_origin_step<const ENC: bool, IO: NumIO>(
    axis: usize,
    s: &mut Track,
    r: &PiRec,
    dt_now: i32,
    dt_prev: i32,
    buf: Buf,
    io: &mut IO,
) {
    let sent = r.flags & (1 << axis) != 0;
    if r.fl & 1 != 0 {
        if sent {
            let off = r.off + 4 * (r.flags & ((1 << axis) - 1)).count_ones();
            let pred = s.flast.wrapping_add(s.flast.wrapping_sub(s.fprev));
            let c = NCtx {
                f: [q(s.fres), axis as u8, 0, 0, 0, 0],
            };
            let (v, d) = x32::<ENC, IO>(buf, off, pred, io, 1, c);
            s.fprev = s.flast;
            s.flast = v;
            s.fres = d;
        } else {
            s.fprev = s.flast;
            s.fres = 0;
        }
    } else {
        let v = if sent {
            let pred = pred_dt(s.last, s.prev, dt_now, dt_prev);
            let off = r.off + 2 * (r.flags & ((1 << axis) - 1)).count_ones();
            let vel = s.last.wrapping_sub(s.prev) as i16 as i32;
            let c = NCtx {
                f: [
                    q(s.res),
                    q(vel),
                    dt_now.clamp(0, 63) as u8,
                    (r.flags & 7) as u8,
                    dd(dt_now, dt_prev),
                    0,
                ],
            };
            let (v, d) = x16::<ENC, IO>(buf, off, pred, io, 0, c);
            s.res = d;
            v
        } else {
            s.res = 0;
            s.last
        };
        s.prev = s.last;
        s.last = v;
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn pi_angle_step<const ENC: bool, IO: NumIO>(
    axis: usize,
    s: &mut Track,
    r: &PiRec,
    dt_now: i32,
    dt_prev: i32,
    buf: Buf,
    io: &mut IO,
) {
    let v = if r.flags & (8 << axis) != 0 {
        let pred = pred_dt(s.last, s.prev, dt_now, dt_prev);
        let cw = if r.fl & 1 != 0 { 4 } else { 2 };
        let off = r.off
            + cw * (r.flags & 7).count_ones()
            + 2 * ((r.flags >> 3) & ((1 << axis) - 1)).count_ones();
        let dprev = s.prev.wrapping_sub(s.prev2) as i16 as i32;
        let c = NCtx {
            f: [
                q(s.res),
                q(dprev),
                dt_now.clamp(0, 63) as u8,
                ((r.flags >> 3) & 7) as u8,
                dd(dt_now, dt_prev),
                0,
            ],
        };
        let (v, d) = x16::<ENC, IO>(buf, off, pred, io, 0, c);
        s.res = d;
        v
    } else {
        s.res = 0;
        s.last
    };
    s.prev2 = s.prev;
    s.prev = s.last;
    s.last = v;
}

#[inline(always)]
fn pe_origin_step<const ENC: bool, IO: NumIO>(
    axis: usize,
    s: &mut Track,
    r: &PeRec,
    buf: Buf,
    io: &mut IO,
) {
    let dt_now = r.t.wrapping_sub(s.t_last) as i32;
    s.t_last = r.t;
    let sent = r.bits & (1 << (2 * axis)) != 0;
    if sent {
        let off = r.origin_offs()[axis];
        if r.bits & PE_FLOAT != 0 {
            let pred = s.flast.wrapping_add(s.flast.wrapping_sub(s.fprev));
            let c = NCtx {
                f: [q(s.fres), axis as u8, 0, 0, 0, 0],
            };
            let (v, d) = x32::<ENC, IO>(buf, off, pred, io, 1, c);
            s.fprev = s.flast;
            s.flast = v;
            s.fres = d;
        } else {
            let vel = s.last.wrapping_sub(s.prev) as i16 as i32;
            let pred = s.last.wrapping_add(vel as u16);
            let ip = (r.bits & PE_INPREV != 0) as u8;
            let c = NCtx {
                f: [
                    q(s.res),
                    q(vel),
                    r.model,
                    ip | (r.ubits & 7) << 1,
                    dt_now.clamp(0, 63) as u8,
                    0,
                ],
            };
            let (v, d) = x16::<ENC, IO>(buf, off, pred, io, 0, c);
            s.res = d;
            s.prev = s.last;
            s.last = v;
        }
    } else {
        s.prev = s.last;
        s.res = 0;
        s.fprev = s.flast;
        s.fres = 0;
    }
}

fn stl<const ENC: bool, IO: NumIO>(recs: &Recs, buf: Buf, io: &mut IO) {
    let mut last = vec![0u32; 1 << 13];
    let mut lres = vec![0i32; 1 << 13];
    for r in recs.stl.iter() {
        let k = (r.key & 0x1fff) as usize;
        let c = NCtx {
            f: [r.key as u8, q(lres[k]), 0, 0, 0, 0],
        };
        let (v, d) = x32::<ENC, IO>(buf, r.off, last[k], io, 0, c);
        last[k] = v;
        lres[k] = d;
    }
}

/// usercmd angles (FORMAT.md §7.6): per player and axis, the grid value
/// against the linear prediction from that axis's last two grid values.
fn uc<const ENC: bool, IO: NumIO>(recs: &Recs, buf: Buf, io: &mut IO) {
    let mut st = vec![[Track::default(); 3]; 256];
    for r in recs.uc.iter() {
        let s = &mut st[r.num as usize];
        for (a, t) in s.iter_mut().enumerate() {
            if r.grid & (1 << a) == 0 {
                continue;
            }
            let off = r.off + 4 * a as u32;
            let d = t.last.wrapping_sub(t.prev) as i16 as i32;
            let d2 = t.prev.wrapping_sub(t.prev2) as i16 as i32;
            let pred = t.last.wrapping_add(d as u16);
            let c = NCtx {
                f: [q(t.res), q(d), q(d2), a as u8, 0, 0],
            };
            let v = if ENC {
                let v = crate::walk::on_grid(buf.rd32(off)).expect("the walker saw it on the grid");
                io.code(0, c, v.wrapping_sub(pred) as i16 as i32);
                v
            } else {
                let v = pred.wrapping_add(io.code(0, c, 0) as u16);
                buf.wr32(off, crate::walk::grid_f32(v));
                v
            };
            t.res = v.wrapping_sub(pred) as i16 as i32;
            t.prev2 = t.prev;
            t.prev = t.last;
            t.last = v;
        }
    }
}

#[inline(always)]
fn rd_ref(buf: Buf, r: u32) -> u16 {
    if r == NONE { 0 } else { buf.rd16(r) }
}

fn snd<const ENC: bool, IO: NumIO>(recs: &Recs, buf: Buf, io: &mut IO) {
    for r in recs.snd.iter() {
        for i in 0..3u32 {
            let pred = rd_ref(buf, r.refs[i as usize]);
            let c = NCtx {
                f: [i as u8 | r.kind << 2, r.num, 0, 0, 0, 0],
            };
            x16::<ENC, IO>(buf, r.off + 2 * i, pred, io, 0, c);
        }
    }
}

fn te<const ENC: bool, IO: NumIO>(recs: &Recs, buf: Buf, io: &mut IO) {
    let mut last = [0u16; 3];
    for r in recs.te.iter() {
        if matches!(r.ty, 5 | 6 | 9) {
            let mut start = [0u16; 3];
            for i in 0..3u32 {
                let pred = rd_ref(buf, r.refs[i as usize]);
                let c = NCtx {
                    f: [i as u8 | 1 << 2, r.ty, 0, 0, 0, 0],
                };
                start[i as usize] = x16::<ENC, IO>(buf, r.off + 2 * i, pred, io, 0, c).0;
            }
            for i in 0..3u32 {
                let c = NCtx {
                    f: [i as u8 | 2 << 2, r.ty, 0, 0, 0, 0],
                };
                last[i as usize] =
                    x16::<ENC, IO>(buf, r.off + 6 + 2 * i, start[i as usize], io, 0, c).0;
            }
        } else {
            for i in 0..3u32 {
                let c = NCtx {
                    f: [i as u8, r.ty, 0, 0, 0, 0],
                };
                last[i as usize] = x16::<ENC, IO>(buf, r.off + 2 * i, last[i as usize], io, 0, c).0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn div_round_ref(a: i64, b: i64) -> i64 {
        if (a < 0) != (b < 0) {
            -((-a + b / 2) / b)
        } else {
            (a + b / 2) / b
        }
    }
    #[test]
    fn div_round_matches_reference() {
        let mut x = 12345u64;
        for b in 1..600i32 {
            for _ in 0..2000 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let a = (x % (2 * 32768 * 32767) as u64) as i64 - 32768 * 32767;
                assert_eq!(
                    div_round(a as i32, b) as i64,
                    div_round_ref(a, b as i64),
                    "{a} / {b}"
                );
            }
            for a in [
                -32768 * 32767,
                32767 * 32767,
                0,
                1,
                -1,
                b / 2,
                -(b / 2),
                b,
                -b,
            ] {
                assert_eq!(
                    div_round(a, b) as i64,
                    div_round_ref(a as i64, b as i64),
                    "{a} / {b}"
                );
            }
        }
    }
}
