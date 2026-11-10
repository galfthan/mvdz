//! The structural walker: one routine that runs in both directions.
//!
//! On encode it reads the demo and appends every structural byte to its
//! stream (with a side context); on decode it pulls the same bytes from the
//! streams and writes them to the output. Numeric fields (origins, angles,
//! sound/TE coordinates, long stats) are not touched: the walker only
//! reserves their bytes and records where they are ([`Recs`]); the numeric
//! groups (`numeric.rs`) fill them in afterwards. The structure
//! of a demo never depends on a numeric value, so the walk and the numeric
//! groups are independent.
//!
//! Every read and write is bounds-checked and sets a sticky `err` instead of
//! panicking, so corrupt input ends the walk early and fails the checksum.

use crate::proto::*;
use crate::streams::*;

pub const NONE: u32 = u32::MAX;

/// Structural symbol I/O. Encode: record and return `v`. Decode: return the
/// decoded symbol (`v` is ignored). Every `ctx` fits in 16 bits, the rans
/// table key (FORMAT.md §8.3); both implementations debug-assert it.
pub trait SymIO {
    fn byte(&mut self, sid: usize, ctx: u32, v: u8) -> u8;
    fn put_blob(&mut self, sid: usize, b: &[u8]);
    fn take_blob(&mut self, sid: usize, n: usize) -> Option<&[u8]>;
    fn blob_rest(&self, sid: usize) -> &[u8];
}

#[derive(Clone, Copy, Default, Debug)]
#[repr(C)]
pub struct PiRec {
    pub off: u32,
    pub t: u32,
    pub flags: u16,
    pub num: u8,
    pub fl: u8, // bit0: float coords
}

pub const PE_O1: u16 = 1;
pub const PE_A1: u16 = 2;
pub const PE_O2: u16 = 4;
pub const PE_A2: u16 = 8;
pub const PE_O3: u16 = 16;
pub const PE_A3: u16 = 32;
pub const PE_INPREV: u16 = 64;
pub const PE_FLOAT: u16 = 128;
pub const PE_WIDE: u16 = 256;

#[derive(Clone, Copy, Default, Debug)]
#[repr(C)]
pub struct PeRec {
    pub off: u32,
    pub t: u32,
    pub num: u16,
    pub bits: u16,
    pub model: u8,
    pub ubits: u8, // U_* bits 9..15 of the entity word
}

impl PeRec {
    /// Byte offsets of the three origin coords (NONE if not sent).
    #[inline]
    pub fn origin_offs(&self) -> [u32; 3] {
        let cw = if self.bits & PE_FLOAT != 0 { 4 } else { 2 };
        let aw = if self.bits & PE_WIDE != 0 { 2 } else { 1 };
        let mut p = self.off;
        let mut o = [NONE; 3];
        for (i, oo) in o.iter_mut().enumerate() {
            if self.bits & (1 << (2 * i)) != 0 {
                *oo = p;
                p += cw;
            }
            if self.bits & (2 << (2 * i)) != 0 {
                p += aw;
            }
        }
        o
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SndRec {
    pub off: u32,
    pub refs: [u32; 3],
    pub kind: u8,
    pub num: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct TeRec {
    pub off: u32,
    pub refs: [u32; 3],
    pub ty: u8,
}

/// A usercmd block with angles on the grid: `off` is the pitch's offset
/// (yaw and roll follow), `grid` bit a set for each axis a on the grid.
#[derive(Clone, Copy, Debug)]
pub struct UcRec {
    pub off: u32,
    pub num: u8,
    pub grid: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct StlRec {
    pub off: u32,
    pub key: u16,
}

/// An append-only record list. A fixed list (decode side) is sized from the
/// stream table and refuses records beyond that; a growable one (encode
/// side) takes any number.
pub struct RecBuf<T> {
    v: Vec<T>,
    cap: usize,
}

impl<T> RecBuf<T> {
    /// A list of at most `cap` records; allocation failure is an error.
    pub fn fixed(cap: usize) -> crate::error::Result<RecBuf<T>> {
        Ok(RecBuf {
            v: crate::budget::with_capacity(cap)?,
            cap,
        })
    }
    pub fn growable(cap: usize) -> RecBuf<T> {
        RecBuf {
            v: Vec::with_capacity(cap),
            cap: usize::MAX,
        }
    }
    /// Appends a record; false if a fixed list is full.
    #[inline(always)]
    pub fn push(&mut self, v: T) -> bool {
        if self.v.len() == self.cap {
            return false;
        }
        self.v.push(v);
        true
    }
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.v.iter()
    }
}

pub struct Recs {
    pub pi: RecBuf<PiRec>,
    pub pe: RecBuf<PeRec>,
    pub snd: RecBuf<SndRec>,
    pub te: RecBuf<TeRec>,
    pub stl: RecBuf<StlRec>,
    pub uc: RecBuf<UcRec>,
}

impl Recs {
    /// Encode side: growable, sized by typical densities.
    pub fn growable(n: usize) -> Recs {
        Recs {
            pi: RecBuf::growable(n / 18),
            pe: RecBuf::growable(n / 32),
            snd: RecBuf::growable(n / 400),
            te: RecBuf::growable(n / 400),
            stl: RecBuf::growable(n / 2000),
            uc: RecBuf::growable(0),
        }
    }
    /// Decode side: exact capacities from the stream table.
    pub fn fixed(
        pi: usize,
        pe: usize,
        snd: usize,
        te: usize,
        stl: usize,
        uc: usize,
    ) -> crate::error::Result<Recs> {
        Ok(Recs {
            pi: RecBuf::fixed(pi)?,
            pe: RecBuf::fixed(pe)?,
            snd: RecBuf::fixed(snd)?,
            te: RecBuf::fixed(te)?,
            stl: RecBuf::fixed(stl)?,
            uc: RecBuf::fixed(uc)?,
        })
    }
}

#[derive(Clone, Copy, Default)]
struct PlState {
    flags: u16,
    frame: u8,
    wf: u8,
}

/// The previous usercmd fields of one player (§6.11).
#[derive(Clone, Copy, Default)]
struct UcState {
    drop: u8,
    msec: u8,
    grid: u8,
    buttons: u8,
    impulse: u8,
    mv: [u16; 3],
}

#[derive(Clone, Copy, Default)]
struct EntState {
    a: [u8; 3],
    ares: [u8; 3],
    frame: u8,
    model: u8,
    bits: u8,
    last_pkt: u32,
}

pub struct Walker<'a, const ENC: bool, IO: SymIO> {
    pub src: &'a [u8],
    dst: *mut u8,
    dst_len: usize,
    pub p: usize,
    pub err: bool,
    pub io: IO,
    pub proto: Proto,
    pub recs: &'a mut Recs,
    pl: Box<[PlState; 256]>,
    ent: Box<[EntState; 2048]>,
    st: Box<[[u8; 256]; 32]>,
    pl_off: Box<[[u32; 3]; 256]>,
    ent_off: Box<[[u32; 3]; 2048]>,
    t: u32,
    target: u8,
    last_type: u8,
    prev_cmd: u8,
    prev_pkt: Vec<u16>,
    cur_pkt: Vec<u16>,
    pkt_ptr: usize,
    pkt_no: u32,
    last_tb: u8,
    first_by_ty: [u8; 8],
    pi_prev_num: u8,
    st_prev_idx: u8,
    st_prev_val: u8,
    last_hdr: u8,
    prev_te: u8,
    pe_prev_d: u8,
    /// KTX demoinfo blocks (hidden type 3) go to the DINFO pseudo-stream,
    /// the segment's share of the metadata's demoinfo store
    dinfo: bool,
    uc_prev: [u8; 2],
    ucs: Box<[UcState; 256]>,
    /// demo time at the end of the walk (ms)
    pub t_end: u32,
}

impl<'a, const ENC: bool, IO: SymIO> Walker<'a, ENC, IO> {
    /// `dst`/`dst_len`: the output segment (decode; empty on encode). The
    /// walker writes only structural bytes; the numeric groups write the
    /// reserved bytes afterwards, through their own raw pointer.
    pub fn new(
        src: &'a [u8],
        dst: *mut u8,
        dst_len: usize,
        recs: &'a mut Recs,
        io: IO,
        proto: Proto,
        dinfo: bool,
    ) -> Self {
        Walker {
            src,
            dst,
            dst_len,
            p: 0,
            err: false,
            io,
            proto,
            recs,
            pl: Box::new([PlState::default(); 256]),
            ent: Box::new([EntState::default(); 2048]),
            st: Box::new([[0u8; 256]; 32]),
            pl_off: Box::new([[NONE; 3]; 256]),
            ent_off: Box::new([[NONE; 3]; 2048]),
            t: 0,
            target: 0,
            last_type: 0,
            prev_cmd: 0,
            prev_pkt: Vec::new(),
            cur_pkt: Vec::new(),
            pkt_ptr: 0,
            pkt_no: 1,
            last_tb: 0,
            first_by_ty: [0; 8],
            pi_prev_num: 255,
            st_prev_idx: 255,
            st_prev_val: 0,
            last_hdr: 0,
            prev_te: 0,
            pe_prev_d: 0,
            dinfo,
            uc_prev: [0; 2],
            ucs: Box::new([UcState::default(); 256]),
            t_end: 0,
        }
    }

    #[inline(always)]
    fn limit(&self) -> usize {
        if ENC { self.src.len() } else { self.dst_len }
    }

    #[inline(always)]
    fn rd(&mut self) -> u8 {
        match self.src.get(self.p) {
            Some(&v) => {
                self.p += 1;
                v
            }
            None => {
                self.err = true;
                0
            }
        }
    }

    #[inline(always)]
    fn wr(&mut self, v: u8) {
        if self.p < self.dst_len {
            // SAFETY: in bounds; this byte is structural (walker-owned)
            unsafe { *self.dst.add(self.p) = v };
        } else {
            self.err = true;
        }
        self.p += 1;
    }

    /// A plain byte.
    #[inline(always)]
    fn b8(&mut self, sid: usize, ctx: u32) -> u8 {
        if ENC {
            let v = self.rd();
            self.io.byte(sid, ctx, v)
        } else {
            let v = self.io.byte(sid, ctx, 0);
            self.wr(v);
            v
        }
    }

    /// A byte coded as (v - pred) mod 256.
    #[inline(always)]
    fn r8(&mut self, sid: usize, ctx: u32, pred: u8) -> u8 {
        if ENC {
            let v = self.rd();
            self.io.byte(sid, ctx, v.wrapping_sub(pred));
            v
        } else {
            let v = self.io.byte(sid, ctx, 0).wrapping_add(pred);
            self.wr(v);
            v
        }
    }

    /// n little-endian bytes, the byte index in context bits 14-15.
    #[inline]
    fn bn(&mut self, sid: usize, ctx: u32, n: usize) -> u32 {
        let mut v = 0u32;
        for k in 0..n {
            v |= (self.b8(sid, ctx | (k as u32) << 14) as u32) << (8 * k);
        }
        v
    }

    fn copy_blob(&mut self, sid: usize, n: usize) {
        if ENC {
            if n > self.src.len() - self.p.min(self.src.len()) {
                self.err = true;
                return;
            }
            self.io.put_blob(sid, &self.src[self.p..self.p + n]);
            self.p += n;
        } else {
            let p = self.p;
            match self.io.take_blob(sid, n) {
                Some(b) if p + n <= self.dst_len => {
                    // SAFETY: in bounds, walker-owned bytes
                    unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), self.dst.add(p), n) };
                    self.p += n;
                }
                _ => self.err = true,
            }
        }
    }

    /// Reserve n bytes for a numeric field; returns their offset.
    #[inline(always)]
    fn reserve(&mut self, n: usize) -> u32 {
        let off = self.p;
        self.p += n;
        if self.p > self.limit() {
            self.err = true;
        }
        off as u32
    }

    /// Walks the whole segment. Encode: returns the tail length. Decode:
    /// `tail` is the stored tail length.
    pub fn run(&mut self, tail: usize) -> usize {
        if ENC {
            while !self.err && frame_len(self.src, self.p).is_some() {
                self.frame();
            }
            if self.err {
                return 0;
            }
            let t = self.src.len() - self.p;
            self.copy_blob(TAIL, t);
            self.t_end = self.t;
            t
        } else {
            let stop = self.dst_len.saturating_sub(tail);
            while !self.err && self.p < stop {
                self.frame();
            }
            if self.p != stop {
                self.err = true;
            }
            self.copy_blob(TAIL, tail);
            self.t_end = self.t;
            tail
        }
    }

    fn frame(&mut self) {
        let lt = self.last_type as u32;
        let tb = self.b8(TIME, lt | (self.last_tb as u32) << 8);
        self.last_tb = tb;
        self.t = self.t.wrapping_add(tb as u32);
        let ty = self.b8(TYPE, lt | (tb.min(32) as u32) << 8);
        self.last_type = ty;
        self.target = ty >> 3;
        match ty & 7 {
            2 => self.copy_blob(SET, 8),
            3 => {
                let mask = self.bn(MASK, 0, 4);
                self.length_prefixed(ty, mask == 0);
            }
            1 | 4 | 5 | 6 => self.length_prefixed(ty, false),
            _ => self.err = true,
        }
    }

    fn length_prefixed(&mut self, ty: u8, hidden: bool) {
        if ENC {
            if self.src.len() - self.p < 4 {
                self.err = true;
                return;
            }
            let n = le32(&self.src[self.p..]) as usize;
            self.p += 4;
            let end = self.p + n;
            if end > self.src.len() {
                self.err = true;
                return;
            }
            if hidden {
                self.hidden(end)
            } else {
                self.payload(end, ty)
            }
        } else {
            let at = self.p;
            self.reserve(4);
            if self.err {
                return;
            }
            if hidden {
                self.hidden(0)
            } else {
                self.payload(0, ty)
            }
            let n = (self.p - at - 4) as u32;
            if at + 4 <= self.dst_len {
                // SAFETY: in bounds, walker-owned bytes
                unsafe {
                    std::ptr::copy_nonoverlapping(n.to_le_bytes().as_ptr(), self.dst.add(at), 4)
                };
            }
        }
    }

    fn escape(&mut self, end: usize) {
        if ENC {
            let mut n = (end - self.p) as u32;
            let mut k = 0u32;
            loop {
                let b = (n & 0x7f) as u8 | if n >= 0x80 { 0x80 } else { 0 };
                self.io.byte(ESCLEN, k, b);
                k += 1;
                n >>= 7;
                if n == 0 {
                    break;
                }
            }
            self.copy_blob(RAW, end - self.p);
        } else {
            let mut n = 0u32;
            for k in 0..5u32 {
                let b = self.io.byte(ESCLEN, k, 0);
                n |= ((b & 0x7f) as u32) << (7 * k);
                if b < 0x80 {
                    break;
                }
                if k == 4 {
                    self.err = true;
                    return;
                }
            }
            self.copy_blob(RAW, n as usize);
        }
    }

    fn payload(&mut self, end: usize, ty: u8) {
        self.prev_cmd = 0;
        self.pi_prev_num = 255;
        self.st_prev_idx = 255;
        self.st_prev_val = 0;
        let mut idx = 0u32;
        let tyi = (ty & 7) as usize;
        let first_prev = cmd_class(self.first_by_ty[tyi]) as u32;
        let mut first = 255u8;
        loop {
            if self.err {
                return;
            }
            // Bit 15 splits the space: a known previous command is named by
            // its class, an "other" one (class 15) by its own number.
            let cl = cmd_class(self.prev_cmd) as u32;
            let ctx = if cl == 15 {
                0x8000 | self.prev_cmd as u32 | (tyi as u32) << 8 | idx.min(7) << 11
            } else {
                cl | idx.min(15) << 4 | first_prev << 8 | (tyi as u32) << 12
            };
            idx += 1;
            if idx == 2 {
                first = self.prev_cmd;
            }
            if idx >= 2 {
                self.first_by_ty[tyi] = first;
            } else {
                self.first_by_ty[tyi] = 255;
            }
            if ENC {
                if self.p == end {
                    self.io.byte(CMD, ctx, 255);
                    return;
                }
                let cmd = self.src[self.p];
                let sz = self.proto.cmd_size(cmd, &self.src[self.p + 1..end]);
                let sz = match sz {
                    Some(s) if cmd != 0 => s,
                    _ => {
                        self.io.byte(CMD, ctx, 0);
                        self.escape(end);
                        return;
                    }
                };
                self.io.byte(CMD, ctx, cmd);
                self.p += 1;
                let start = self.p;
                self.command(cmd);
                if self.p != start + sz {
                    self.err = true;
                    return;
                }
                self.prev_cmd = cmd;
            } else {
                let cmd = self.io.byte(CMD, ctx, 0);
                match cmd {
                    255 => return,
                    0 => {
                        self.escape(0);
                        return;
                    }
                    _ => {}
                }
                self.wr(cmd);
                self.command(cmd);
                self.prev_cmd = cmd;
            }
        }
    }

    fn generic(&mut self, sid: usize, size_cmd: u8) -> usize {
        let start = self.p;
        let n = if ENC {
            self.proto
                .cmd_size(size_cmd, &self.src[self.p.min(self.src.len())..])
        } else {
            self.proto.cmd_size(size_cmd, self.io.blob_rest(sid))
        };
        match n {
            Some(n) => self.copy_blob(sid, n),
            None => self.err = true,
        }
        start
    }

    fn command(&mut self, cmd: u8) {
        match cmd {
            42 => self.playerinfo(),
            47 | 48 => self.packet_entities(cmd == 48),
            3 => {
                let t = (self.target & 31) as usize;
                let i = self.b8(
                    ST_IDX,
                    self.st_prev_idx as u32 | (self.prev_cmd as u32) << 8,
                );
                let prev = self.st[t][i as usize];
                let v = self.r8(ST_VAL, i as u32 | (self.st_prev_val as u32) << 8, prev);
                self.st_prev_idx = i;
                self.st_prev_val = v.wrapping_sub(prev);
                self.st[t][i as usize] = v;
            }
            38 => {
                let t = (self.target & 31) as u16;
                let i = self.b8(STL_IDX, self.prev_cmd as u32);
                let off = self.reserve(4);
                if !self.recs.stl.push(StlRec {
                    off,
                    key: t << 8 | i as u16,
                }) {
                    self.err = true;
                }
            }
            6 => self.sound(),
            23 => self.temp_entity(),
            8 => {
                self.b8(PR_LEVEL, 0);
                self.generic(TEXT, 9);
            }
            9 | 26 => {
                self.generic(TEXT, 9);
            }
            14 => {
                self.bn(FRAGS, 0, 3);
            }
            36 | 37 | 53 => {
                self.generic(PING, cmd);
            }
            43 | 54 => {
                self.generic(NAILS, cmd);
            }
            11 => {
                let start = self.generic(GEN_BASE + 11, 11);
                if !self.err {
                    let end = self.p;
                    // SAFETY (decode): the serverdata body was just written by the walker
                    let b: &[u8] = if ENC {
                        &self.src[start..end]
                    } else {
                        unsafe { std::slice::from_raw_parts(self.dst.add(start), end - start) }
                    };
                    let mut pr = self.proto;
                    pr.observe_server_data(b);
                    self.proto = pr;
                }
            }
            _ => {
                self.generic(GEN_BASE + cmd as usize, cmd);
            }
        }
    }

    /// The DF_* flags as one symbol in the common case: x = flags ^ prev;
    /// the often-toggling bits (sent axes 0-5, weaponframe 10) plus an
    /// escape bit for "a rare bit changed" form one byte; only on escape do
    /// bits 6-9, 11-14 and then bit 15 follow.
    fn pi_flags(&mut self, prev: u16) -> u16 {
        // Bit 15 keeps the main symbol's contexts clear of b1's (0x100-0x1ff)
        // and b2's (0x200); prev's own bit 15 is left out.
        let cx = 0x8000 | (prev & 0x7fff) as u32;
        let x = if ENC {
            let lo = self.rd() as u16;
            let x = (lo | (self.rd() as u16) << 8) ^ prev;
            let rare = x & !0x43f;
            let a = (x & 0x3f) as u8 | (((x >> 10) & 1) as u8) << 6 | ((rare != 0) as u8) << 7;
            self.io.byte(PI_FLAGS, cx, a);
            if rare != 0 {
                let b1 = ((x >> 6) & 0xf) as u8 | (((x >> 11) & 0xf) as u8) << 4;
                self.io
                    .byte(PI_FLAGS, 0x100 | ((prev >> 6) & 0xff) as u32, b1);
                self.io.byte(PI_FLAGS, 0x200, (x >> 15) as u8);
            }
            x
        } else {
            let a = self.io.byte(PI_FLAGS, cx, 0) as u16;
            let mut x = (a & 0x3f) | ((a >> 6) & 1) << 10;
            if a & 0x80 != 0 {
                let b1 = self
                    .io
                    .byte(PI_FLAGS, 0x100 | ((prev >> 6) & 0xff) as u32, 0)
                    as u16;
                let b2 = self.io.byte(PI_FLAGS, 0x200, 0) as u16;
                x |= (b1 & 0xf) << 6 | (b1 >> 4) << 11 | (b2 & 1) << 15;
            }
            let f = x ^ prev;
            self.wr(f as u8);
            self.wr((f >> 8) as u8);
            x
        };
        x ^ prev
    }

    fn playerinfo(&mut self) {
        let num = self.b8(PI_NUM, self.pi_prev_num as u32);
        self.pi_prev_num = num;
        let ps = self.pl[num as usize];
        let flags = self.pi_flags(ps.flags);
        let frame = self.r8(PI_FRAME, ps.frame as u32 | (flags & 0x300) as u32, ps.frame);
        let cw = self.proto.coord();
        let nco = (flags & 7).count_ones() as usize;
        let nan = (flags & 0x38).count_ones() as usize;
        let off = self.reserve(nco * cw + nan * 2);
        if !self.recs.pi.push(PiRec {
            off,
            t: self.t,
            flags,
            num,
            fl: self.proto.float_coords as u8,
        }) {
            self.err = true;
        }
        let mut o = off;
        for i in 0..3 {
            if flags & (1 << i) != 0 {
                self.pl_off[num as usize][i] = o;
                o += cw as u32;
            }
        }
        if flags & DF_MODEL != 0 {
            self.b8(PI_MODEL, 0);
        }
        if flags & DF_SKIN != 0 {
            self.b8(PI_SKIN, 0);
        }
        if flags & DF_EFFECTS != 0 {
            self.b8(PI_EFF, 0);
        }
        let mut wf = ps.wf;
        if flags & DF_WEAPONFRAME != 0 {
            wf = self.r8(PI_WFRAME, ps.wf as u32, ps.wf);
        }
        self.pl[num as usize] = PlState { flags, frame, wf };
    }

    /// Next entity number in this packet as predicted from the previous
    /// packet's list.
    fn predict_ent(&mut self, prev: i32) -> i32 {
        while self.pkt_ptr < self.prev_pkt.len() && (self.prev_pkt[self.pkt_ptr] as i32) <= prev {
            self.pkt_ptr += 1;
        }
        if self.pkt_ptr < self.prev_pkt.len() {
            self.prev_pkt[self.pkt_ptr] as i32
        } else {
            prev + 1
        }
    }

    fn packet_entities(&mut self, delta: bool) {
        if delta {
            let h = self.r8(PE_HDR, 0, self.last_hdr);
            self.last_hdr = h;
        }
        self.pe_prev_d = 0;
        let mut prev_ent: i32 = -1;
        self.pkt_ptr = 0;
        self.cur_pkt.clear();
        self.pkt_no = self.pkt_no.wrapping_add(1);
        loop {
            if self.err {
                break;
            }
            let pe = self.predict_ent(prev_ent);
            let ectx = (pe - prev_ent).min(255) as u32 | (self.pe_prev_d.min(15) as u32) << 8;
            let w: u16;
            let ent: usize;
            if ENC {
                if self.src.len() - self.p < 2 {
                    self.err = true;
                    return;
                }
                w = u16::from_le_bytes([self.src[self.p], self.src[self.p + 1]]);
                self.p += 2;
                if w == 0 {
                    self.io.byte(PE_ENT, ectx, 0);
                    break;
                }
                ent = (w & 511) as usize;
                let d = ((ent as i32 - prev_ent - 1) & 511) + 1;
                if d < 255 {
                    self.io.byte(PE_ENT, ectx, d as u8);
                } else {
                    self.io.byte(PE_ENT, ectx, 255);
                    self.io.byte(PE_ENT, 1 << 12, d as u8);
                    self.io.byte(PE_ENT, 2 << 12, (d >> 8) as u8);
                }
                let es = self.ent[ent];
                self.io.byte(
                    PE_BITS,
                    es.bits as u32 | (es.model as u32) << 7,
                    (w >> 9) as u8 ^ es.bits,
                );
            } else {
                let mut d = self.io.byte(PE_ENT, ectx, 0) as i32;
                if d == 255 {
                    d = self.io.byte(PE_ENT, 1 << 12, 0) as i32;
                    d |= (self.io.byte(PE_ENT, 2 << 12, 0) as i32) << 8;
                }
                if d == 0 {
                    self.wr(0);
                    self.wr(0);
                    break;
                }
                ent = ((prev_ent + d) & 511) as usize;
                let es = self.ent[ent];
                let b = self
                    .io
                    .byte(PE_BITS, es.bits as u32 | (es.model as u32) << 7, 0)
                    ^ es.bits;
                w = (b as u16) << 9 | ent as u16;
                if w == 0 {
                    // a word of zero would end the packet; the encoder never emits it
                    self.err = true;
                    break;
                }
                self.wr(w as u8);
                self.wr((w >> 8) as u8);
            }
            self.pe_prev_d = (ent as i32 - prev_ent).clamp(0, 255) as u8;
            prev_ent = ent as i32;
            self.cur_pkt.push(ent as u16);
            self.ent[ent].bits = (w >> 9) as u8;
            self.entity_delta(w as u32);
        }
        std::mem::swap(&mut self.prev_pkt, &mut self.cur_pkt);
    }

    fn entity_delta(&mut self, w: u32) {
        let mut bits = w & !511;
        let mut more = 0u32;
        let mut num = (w & 511) as usize;
        let m0 = self.ent[num].model as u32;
        if bits & U_MOREBITS != 0 {
            bits |= self.b8(PE_MORE, (bits >> 9) | m0 << 8) as u32;
        }
        if bits & U_EVENMORE != 0 && self.proto.fte != 0 {
            more = self.b8(PE_MORE, 0x8000) as u32;
            if more & X_YETMORE != 0 {
                more |= (self.b8(PE_MORE, 0x8001) as u32) << 8;
            }
        }
        if more & X_ENTITYDBL != 0 {
            num += 512;
        }
        if more & X_ENTITYDBL2 != 0 {
            num += 1024;
        }
        let in_prev = self.ent[num].last_pkt == self.pkt_no.wrapping_sub(1);
        self.ent[num].last_pkt = self.pkt_no;
        let mut es = self.ent[num];
        if bits & U_MODEL != 0 {
            es.model = self.b8(PE_MODEL, es.model as u32);
        } else if more & X_MODELDBL != 0 {
            self.bn(PE_MODEL, 0x100, 2);
        }
        let model = es.model as u32;
        if bits & U_FRAME != 0 {
            es.frame = self.r8(PE_FRAME, model, es.frame);
        }
        if bits & U_COLORMAP != 0 {
            self.b8(PE_MISC, model);
        }
        if bits & U_SKIN != 0 {
            self.b8(PE_MISC, model | 1 << 8);
        }
        if bits & U_EFFECTS != 0 {
            self.b8(PE_MISC, model | 2 << 8);
        }
        let mut rb = 0u16;
        for (i, (o, a)) in [
            (U_ORIGIN1, U_ANGLE1),
            (U_ORIGIN2, U_ANGLE2),
            (U_ORIGIN3, U_ANGLE3),
        ]
        .into_iter()
        .enumerate()
        {
            if bits & o != 0 {
                rb |= 1 << (2 * i);
            }
            if bits & a != 0 {
                rb |= 2 << (2 * i);
            }
        }
        if in_prev {
            rb |= PE_INPREV;
        }
        if self.proto.float_coords {
            rb |= PE_FLOAT;
        }
        if self.proto.msg_wide {
            rb |= PE_WIDE;
        }
        let rec = PeRec {
            off: self.p as u32,
            t: self.t,
            num: num as u16,
            bits: rb,
            model: es.model,
            ubits: (w >> 9) as u8,
        };
        let cw = self.proto.coord();
        for (i, (o, a)) in [
            (U_ORIGIN1, U_ANGLE1),
            (U_ORIGIN2, U_ANGLE2),
            (U_ORIGIN3, U_ANGLE3),
        ]
        .into_iter()
        .enumerate()
        {
            if bits & o != 0 {
                let off = self.reserve(cw);
                self.ent_off[num][i] = off;
            }
            if bits & a != 0 {
                if self.proto.msg_wide {
                    self.bn(PE_A, 3 | model << 6, 2);
                } else {
                    let qa = qb(es.ares[i]).min(15) as u32;
                    let v = self.r8(PE_A, i as u32 | qa << 2 | model << 6, es.a[i]);
                    es.ares[i] = v.wrapping_sub(es.a[i]);
                    es.a[i] = v;
                }
            }
        }
        if !self.recs.pe.push(rec) {
            self.err = true;
        }
        if more & X_TRANS != 0 && self.proto.fte & FTE_TRANS != 0 {
            self.b8(PE_MISC, 0x3000);
        }
        if more & X_SCALE != 0 {
            self.b8(PE_MISC, 0x3001);
        }
        if more & X_FATNESS != 0 {
            self.b8(PE_MISC, 0x3002);
        }
        if more & X_COLOURMOD != 0 && self.proto.fte & FTE_COLOURMOD != 0 {
            self.bn(PE_MISC, 0x3003, 3);
        }
        self.ent[num] = es;
    }

    fn sound(&mut self) {
        let lo = self.b8(SND_CH, self.prev_cmd as u32) as u32;
        let ch = lo | (self.b8(SND_CH, 1 << 8 | lo) as u32) << 8;
        if ch & 0x8000 != 0 {
            self.b8(SND_MISC, 0);
        }
        if ch & 0x4000 != 0 {
            self.b8(SND_MISC, 1);
        }
        let ent = ((ch >> 3) & 1023) as usize;
        let num = self.b8(SND_NUM, (ch & 7) | (ent.min(33) as u32) << 3);
        if self.proto.coord() != 2 {
            self.copy_blob(SNDF, 12);
            return;
        }
        let (refs, kind) = if (1..=32).contains(&ent) {
            (self.pl_off[ent - 1], 1)
        } else if ent < 2048 {
            (self.ent_off[ent], 2)
        } else {
            ([NONE; 3], 0)
        };
        let off = self.reserve(6);
        if !self.recs.snd.push(SndRec {
            off,
            refs,
            kind,
            num,
        }) {
            self.err = true;
        }
    }

    fn temp_entity(&mut self) {
        let t = self.b8(
            TE_TYPE,
            self.prev_te as u32 | ((self.prev_cmd == 23) as u32) << 8,
        );
        self.prev_te = t;
        let c = self.proto.coord();
        if c != 2 {
            match t {
                5 | 6 | 9 => {
                    self.bn(TE_MISC, t as u32, 2);
                    self.copy_blob(TEF, 6 * c);
                }
                2 | 12 => {
                    self.b8(TE_MISC, t as u32 | 0x100);
                    self.copy_blob(TEF, 3 * c);
                }
                _ => self.copy_blob(TEF, 3 * c),
            }
            return;
        }
        match t {
            5 | 6 | 9 => {
                let e = self.bn(TE_MISC, t as u32, 2) as usize;
                let refs = if (1..=32).contains(&e) {
                    self.pl_off[e - 1]
                } else {
                    [NONE; 3]
                };
                let off = self.reserve(12);
                if !self.recs.te.push(TeRec { off, refs, ty: t }) {
                    self.err = true;
                }
            }
            2 | 12 => {
                self.b8(TE_MISC, t as u32 | 0x100);
                let off = self.reserve(6);
                if !self.recs.te.push(TeRec {
                    off,
                    refs: [NONE; 3],
                    ty: t,
                }) {
                    self.err = true;
                }
            }
            _ => {
                let off = self.reserve(6);
                if !self.recs.te.push(TeRec {
                    off,
                    refs: [NONE; 3],
                    ty: t,
                }) {
                    self.err = true;
                }
            }
        }
    }

    /// dem_multiple with mask 0: blocks of [u32 len][u16 type][len bytes].
    /// Tags: 1 block, 2 end, 0 escape (rest raw).
    fn hidden(&mut self, end: usize) {
        loop {
            if self.err {
                return;
            }
            if ENC {
                let rem = end - self.p;
                if rem == 0 {
                    self.io.byte(HIDTAG, 0, 2);
                    return;
                }
                let mut ok = rem >= 6;
                if ok {
                    let bl = le32(&self.src[self.p..]) as u64;
                    ok = bl >= 1 && bl <= (rem - 6) as u64;
                }
                if !ok {
                    self.io.byte(HIDTAG, 0, 0);
                    self.escape(end);
                    return;
                }
                self.io.byte(HIDTAG, 0, 1);
            } else {
                match self.io.byte(HIDTAG, 0, 0) {
                    0 => {
                        self.escape(0);
                        return;
                    }
                    1 => {}
                    2 => return,
                    _ => {
                        self.err = true;
                        return;
                    }
                }
            }
            let bl = self.bn(HIDLEN, 0, 4);
            let ty = self.bn(HIDTYPE, 0, 2);
            if ty == 7 && bl == 8 {
                for k in 0..4 {
                    self.bn(HIDDMG, k, 2);
                }
            } else if ty == 1 && bl == 23 {
                self.ucmd();
            } else if ty == 3 && self.dinfo {
                // KTX demoinfo: kept once, in the metadata
                if ENC {
                    self.io.put_blob(DINFO, &bl.to_le_bytes());
                    self.copy_blob(DINFO, bl as usize);
                } else {
                    match self.io.take_blob(DINFO, 4) {
                        Some(b) if le32(b) == bl => self.copy_blob(DINFO, bl as usize),
                        _ => self.err = true,
                    }
                }
            } else {
                self.copy_blob(HIDMISC, bl as usize);
            }
        }
    }

    /// One usercmd block (mvdsv `SV_DebugClientCommand`): player, dropnum,
    /// msec, pitch/yaw/roll as f32, forward/side/up as i16, buttons,
    /// impulse. Angles on the 360/65536 grid are left to the numeric group
    /// (`UcRec`); the rest goes to UC, context kind in bits 12-15.
    fn ucmd(&mut self) {
        let [p1, p2] = self.uc_prev;
        let pl = self.b8(UC, (p1.min(31) as u32) | (p2.min(31) as u32) << 5);
        self.uc_prev = [pl, p1];
        let mut s = self.ucs[pl as usize];
        s.drop = self.b8(UC, 1 << 12 | s.drop as u32);
        s.msec = self.b8(UC, 2 << 12 | s.msec as u32);
        // hidden() saw all 23 bytes inside the payload: the angles follow
        let mut grid = 0u8;
        if ENC {
            for a in 0..3 {
                if on_grid(le32(&self.src[self.p + 4 * a..])).is_some() {
                    grid |= 1 << a;
                }
            }
        }
        grid = self.io.byte(UC, 3 << 12 | s.grid as u32, grid);
        if grid > 7 {
            self.err = true;
            return;
        }
        s.grid = grid;
        let off = self.p as u32;
        for a in 0..3u32 {
            if grid & (1 << a) != 0 {
                self.reserve(4);
            } else {
                for k in 0..4 {
                    self.b8(UC, 4 << 12 | a << 2 | k);
                }
            }
        }
        if grid != 0 && !self.recs.uc.push(UcRec { off, num: pl, grid }) {
            self.err = true;
        }
        for m in 0..3 {
            let lo = self.b8(UC, 5 << 12 | (m as u32) << 8 | (s.mv[m] & 0xff) as u32);
            let hi = self.b8(UC, 6 << 12 | (m as u32) << 8 | lo as u32);
            s.mv[m] = u16::from_le_bytes([lo, hi]);
        }
        s.buttons = self.b8(UC, 7 << 12 | s.buttons as u32);
        s.impulse = self.b8(UC, 8 << 12 | s.impulse as u32);
        self.ucs[pl as usize] = s;
    }
}

/// Small class of an svc number for the cmd context.
#[inline(always)]
fn cmd_class(c: u8) -> u8 {
    match c {
        0 => 0,
        42 => 1,
        48 => 2,
        47 => 3,
        6 => 4,
        23 => 5,
        3 => 6,
        38 => 7,
        8 => 8,
        51 => 9,
        14 => 10,
        36 => 11,
        9 => 12,
        43 | 54 => 13,
        19 => 14,
        _ => 15,
    }
}

/// Sign/magnitude bucket of a byte residual (as i8).
#[inline(always)]
pub fn qb(v: u8) -> u8 {
    crate::numeric::q(v as i8 as i32)
}

/// The grid value of a usercmd angle: the s in -32768..=32767 whose
/// f32(s * 45/8192) has exactly these bits (s * 45/8192 is exact in f32),
/// or None (-0.0, NaN, +180 and anything else off the grid).
#[inline]
pub fn on_grid(bits: u32) -> Option<u16> {
    // f * 8192 is exact and the division correctly rounded: an integer
    // quotient comes out exactly
    let x = f32::from_bits(bits) as f64 * 8192.0 / 45.0;
    if x.fract() != 0.0 || !(-32768.0..=32767.0).contains(&x) {
        return None;
    }
    let s = x as i32 as i16;
    (grid_f32(s as u16) == bits).then_some(s as u16)
}

/// The f32 bits of grid value `s` (as i16): s * 45/8192, exact.
#[inline]
pub fn grid_f32(s: u16) -> u32 {
    ((s as i16 as f32) * (45.0 / 8192.0)).to_bits()
}
