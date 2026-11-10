//! Transform-coded segments: the structural walk, the numeric groups and
//! the per-stream back ends (FORMAT.md, "Transform segment body").
//!
//! ```text
//! flags uv | dinfo_off uv | dinfo_len uv | fte uv | fte2 uv | mvd1 uv | tail uv
//! nstreams uv | nstreams x { id uv, backend uv, count uv, len uv }
//! stream bodies, concatenated in table order
//! ```

use crate::budget::Budget;
use crate::coder::rans::FileTables;
use crate::coder::{self, BE_FILE, BE_RANS, BE_STORE, BE_ZSTD, LazyDec, NumDec, rans};
use crate::error::{Error, Result, bad};
use crate::format::TABLES_MAX;
use crate::numeric::{Buf, Group, NCtx, NumIO, WAVE1, WAVE2};
use crate::proto::{Proto, frame_len};
use crate::streams::*;
use crate::util::{Rd, par_map, put_uv};
use crate::walk::{Recs, SymIO, Walker};
use std::borrow::Cow;

/// Segment flag: KTX demoinfo blocks come from the metadata's store.
pub const SF_DINFO: u64 = 1;

/// Compression level: which back ends the encoder may use per stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    /// Static rANS with per-context tables for structural and numeric
    /// streams; blobs stored or zstd.
    #[default]
    Normal,
    /// Every stream stored or zstd.
    Fast,
}

/// Default segment size, in raw bytes (the writer's too).
pub const DEFAULT_SEGMENT: usize = 256 << 10;

/// zstd level of the normal level's zstd streams, untransformed segments
/// and META: 19 is 0.05 % smaller than 17 but about 10 % slower to
/// compress, and no level changes decode speed.
pub const ZSTD_NORMAL: i32 = 17;
/// zstd level of the fast level's streams, untransformed segments and
/// META, so that the fast level is fast both ways: about 15 MB/s to
/// compress (2.2 at 19, 12.5 at 9) for a file about 5 % larger than at 19
/// (0.7 % larger than at 9); decode speed is the same.
pub const ZSTD_FAST: i32 = 6;
/// zstd level of an attachment's probe: an attachment that does not shrink
/// by 1/32 at this level is stored without trying `ZSTD_ATTACH` (opus
/// voice at 19 costs about 0.2 s/MB and is then stored anyway).
pub const ZSTD_ATTACH_PROBE: i32 = 1;
/// zstd level of attachments that pass the probe: text such as analysis
/// JSON is about 4 % smaller at 19 than at 17.
pub const ZSTD_ATTACH: i32 = 19;

impl Level {
    /// The zstd level of this level's streams, untransformed segments and
    /// META.
    pub fn zstd(self) -> i32 {
        match self {
            Level::Normal => ZSTD_NORMAL,
            Level::Fast => ZSTD_FAST,
        }
    }
}

/// Per-stream statistics for reporting.
#[derive(Clone, Debug, Default)]
pub struct StreamStat {
    pub id: usize,
    pub be: u8,
    pub count: usize,
    pub packed: usize,
}

// ---------------------------------------------------------------- encode side I/O

pub struct EncIO {
    pub syms: Vec<Vec<u8>>,
    pub ctxs: Vec<Vec<u32>>,
    pub blobs: Vec<Vec<u8>>,
}

impl EncIO {
    fn new() -> EncIO {
        EncIO {
            syms: (0..N_LAZY).map(|_| Vec::new()).collect(),
            ctxs: (0..N_LAZY).map(|_| Vec::new()).collect(),
            blobs: (0..N_STREAMS).map(|_| Vec::new()).collect(),
        }
    }
}

impl SymIO for EncIO {
    #[inline(always)]
    fn byte(&mut self, sid: usize, ctx: u32, v: u8) -> u8 {
        debug_assert!(ctx <= 0xffff, "stream {sid}: context {ctx:#x} over 16 bits");
        self.syms[sid].push(v);
        self.ctxs[sid].push(ctx);
        v
    }
    fn put_blob(&mut self, sid: usize, b: &[u8]) {
        self.blobs[sid].extend_from_slice(b);
    }
    fn take_blob(&mut self, _: usize, _: usize) -> Option<&[u8]> {
        None
    }
    fn blob_rest(&self, _: usize) -> &[u8] {
        &[]
    }
}

struct NumEnc {
    vals: Vec<Vec<(NCtx, i32)>>,
}

impl NumIO for NumEnc {
    #[inline(always)]
    fn code(&mut self, k: usize, ctx: NCtx, v: i32) -> i32 {
        self.vals[k].push((ctx, v));
        v
    }
}

// ---------------------------------------------------------------- decode side I/O

/// A structural-stream decoder (one per stream).
pub trait LazyCoder: Sized {
    fn get(&mut self, ctx: u32) -> Option<u8>;
    fn done(&self) -> bool;
    fn empty() -> Self;
}

impl LazyCoder for LazyDec<'_> {
    #[inline(always)]
    fn get(&mut self, ctx: u32) -> Option<u8> {
        LazyDec::get(self, ctx)
    }
    fn done(&self) -> bool {
        LazyDec::done(self)
    }
    fn empty() -> Self {
        LazyDec::Empty
    }
}

impl LazyCoder for rans::ByteDec<'_> {
    #[inline(always)]
    fn get(&mut self, ctx: u32) -> Option<u8> {
        rans::ByteDec::get(self, ctx)
    }
    fn done(&self) -> bool {
        rans::ByteDec::done(self)
    }
    fn empty() -> Self {
        rans::ByteDec::empty()
    }
}

/// A numeric-stream decoder (one per stream).
pub trait NumCoder: Sized {
    fn get(&mut self, c: NCtx) -> Option<i32>;
    fn done(&self) -> bool;
    fn empty() -> Self;
}

impl NumCoder for NumDec<'_> {
    #[inline(always)]
    fn get(&mut self, c: NCtx) -> Option<i32> {
        NumDec::get(self, c)
    }
    fn done(&self) -> bool {
        NumDec::done(self)
    }
    fn empty() -> Self {
        NumDec::Empty
    }
}

impl NumCoder for rans::NumDec<'_> {
    #[inline(always)]
    fn get(&mut self, c: NCtx) -> Option<i32> {
        rans::NumDec::get(self, c)
    }
    fn done(&self) -> bool {
        rans::NumDec::done(self)
    }
    fn empty() -> Self {
        rans::NumDec::empty()
    }
}

pub struct DecIO<'a, L> {
    lazy: [L; N_LAZY],
    blobs: Vec<(Cow<'a, [u8]>, usize)>,
    pub over: bool,
}

impl<L: LazyCoder> SymIO for DecIO<'_, L> {
    #[inline(always)]
    fn byte(&mut self, sid: usize, ctx: u32, _: u8) -> u8 {
        debug_assert!(ctx <= 0xffff, "stream {sid}: context {ctx:#x} over 16 bits");
        match self.lazy[sid].get(ctx) {
            Some(v) => v,
            None => {
                self.over = true;
                // an exhausted stream reads as end tokens so every loop stops
                255
            }
        }
    }
    fn put_blob(&mut self, _: usize, _: &[u8]) {}
    fn take_blob(&mut self, sid: usize, n: usize) -> Option<&[u8]> {
        let (b, i) = &mut self.blobs[sid];
        let s = b.get(*i..i.checked_add(n)?)?;
        *i += n;
        Some(s)
    }
    fn blob_rest(&self, sid: usize) -> &[u8] {
        let (b, i) = &self.blobs[sid];
        &b[(*i).min(b.len())..]
    }
}

const MAXK: usize = 2;

struct NumDecIO<N> {
    d: [N; MAXK],
    over: bool,
}

impl<N: NumCoder> NumIO for NumDecIO<N> {
    #[inline(always)]
    fn code(&mut self, k: usize, ctx: NCtx, _: i32) -> i32 {
        match self.d[k].get(ctx) {
            Some(v) => v,
            None => {
                self.over = true;
                0
            }
        }
    }
}

/// (back end, body, count) of a present stream.
type StreamEnt<'a> = Option<(u8, &'a [u8], usize)>;

fn walk_dec<L: LazyCoder>(
    lazy: [L; N_LAZY],
    blobs: Vec<(Cow<'_, [u8]>, usize)>,
    dst: Buf,
    recs: &mut Recs,
    proto: Proto,
    tail: usize,
    dinfo: bool,
) -> Result<()> {
    let io = DecIO {
        lazy,
        blobs,
        over: false,
    };
    let mut w: Walker<false, DecIO<L>> =
        Walker::new(&[], dst.ptr(), dst.len(), recs, io, proto, dinfo);
    w.run(tail);
    if w.err || w.io.over {
        return bad("structure");
    }
    if !w.io.lazy.iter().all(|d| d.done()) || !w.io.blobs.iter().all(|(b, i)| *i == b.len()) {
        return bad("unconsumed stream data");
    }
    Ok(())
}

fn run_group<'a, N: NumCoder>(
    g: Group,
    recs: &Recs,
    buf: Buf,
    ents: &[StreamEnt<'a>],
    mk: impl Fn(u8, usize, &'a [u8], usize) -> Result<N>,
) -> Result<()> {
    let mut d: [N; MAXK] = std::array::from_fn(|_| N::empty());
    for (k, &sid) in g.streams().iter().enumerate() {
        if let Some((be, b, c)) = ents[sid] {
            d[k] = mk(be, sid, b, c)?;
        }
    }
    let mut io = NumDecIO { d, over: false };
    g.run::<false, NumDecIO<N>>(recs, buf, &mut io);
    if io.over || !io.d.iter().all(|d| d.done()) {
        return bad("numeric stream");
    }
    Ok(())
}

// ---------------------------------------------------------------- segment encode

/// The encode-direction walk of one segment.
pub struct Walked {
    io: EncIO,
    recs: Recs,
    tail: usize,
    proto0: Proto,
    dinfo: bool,
}

impl Walked {
    /// This segment's share of the demoinfo store (empty without sharing).
    pub fn dinfo(&self) -> &[u8] {
        &self.io.blobs[DINFO]
    }
    /// Raw bytes the transform could not structure (escapes and tail).
    pub fn escaped(&self) -> usize {
        self.io.blobs[RAW].len() + self.io.blobs[TAIL].len()
    }
}

/// Walks one segment starting in protocol state `proto`; returns the walk
/// and the protocol state at its end, or None if the transform cannot
/// represent the segment.
pub fn walk(src: &[u8], proto: Proto, dinfo: bool) -> Option<(Walked, Proto)> {
    let mut recs = Recs::growable(src.len());
    let (io, tail, pend, err) = {
        let mut w: Walker<true, EncIO> = Walker::new(
            src,
            std::ptr::null_mut(),
            0,
            &mut recs,
            EncIO::new(),
            proto,
            dinfo,
        );
        let tail = w.run(0);
        (w.io, tail, w.proto, w.err)
    };
    if err {
        return None;
    }
    Some((
        Walked {
            io,
            recs,
            tail,
            proto0: proto,
            dinfo,
        },
        pend,
    ))
}

#[derive(Clone, Copy)]
enum Kind {
    Lazy,
    Num,
    Blob,
}

/// Back ends the encoder tries, smallest output wins; if none of them can
/// code the stream, the fallback list is tried. `file`: the file has a
/// shared model for the stream.
fn candidates(level: Level, kind: Kind, file: bool) -> &'static [u8] {
    match (level, kind) {
        (Level::Normal, Kind::Lazy | Kind::Num) if file => &[BE_FILE],
        (Level::Normal, Kind::Lazy | Kind::Num) => &[BE_RANS],
        // Miri cannot run libzstd (C)
        _ if cfg!(miri) => &[BE_STORE],
        _ => &[BE_STORE, BE_ZSTD],
    }
}

fn pack_one(
    kind: Kind,
    be: u8,
    sid: usize,
    level: Level,
    wk: &Walked,
    num: &[(NCtx, i32)],
    file: Option<&rans::Shared>,
) -> Option<Vec<u8>> {
    let z = level.zstd();
    match kind {
        Kind::Lazy => coder::encode_lazy(be, &wk.io.syms[sid], &wk.io.ctxs[sid], z, file),
        Kind::Num => coder::encode_num(be, sid, num, z, file),
        Kind::Blob => coder::encode_blob(be, &wk.io.blobs[sid], z),
    }
}

fn pack_best(
    kind: Kind,
    sid: usize,
    level: Level,
    wk: &Walked,
    num: &[(NCtx, i32)],
    file: Option<&rans::Shared>,
) -> (u8, Vec<u8>) {
    for list in [
        candidates(level, kind, file.is_some()),
        if cfg!(miri) {
            &[BE_STORE]
        } else {
            &[BE_STORE, BE_ZSTD]
        },
    ] {
        let mut best: Option<(u8, Vec<u8>)> = None;
        for &be in list {
            if let Some(c) = pack_one(kind, be, sid, level, wk, num, file) {
                if best.as_ref().is_none_or(|b| c.len() < b.1.len()) {
                    best = Some((be, c));
                }
            }
        }
        if let Some(b) = best {
            return b;
        }
    }
    unreachable!("store always codes a stream")
}

/// The numeric groups in coding order.
fn groups() -> Vec<Group> {
    WAVE1.iter().chain(WAVE2.iter()).copied().collect()
}

/// The numeric values of a walked segment, per group (in [`groups`] order)
/// and stream of the group.
fn numerics(src: &[u8], wk: &Walked) -> Vec<Vec<Vec<(NCtx, i32)>>> {
    let buf = Buf::ro(src);
    groups()
        .into_iter()
        .map(|g| {
            let mut io = NumEnc {
                vals: g.streams().iter().map(|_| Vec::new()).collect(),
            };
            g.run::<true, NumEnc>(&wk.recs, buf, &mut io);
            io.vals
        })
        .collect()
}

/// The shared models of a file (FORMAT.md §12 rule 11): one per
/// structural and numeric stream, from its per-context counts summed over
/// every walked segment `(bytes, walk)`, up to `threads` segments at a
/// time.
pub fn train(segs: &[(&[u8], &Walked)], threads: usize) -> Vec<Option<rans::Shared>> {
    let mut tally: Vec<Option<rans::Tally>> = (0..N_STREAMS).map(|_| None).collect();
    let mut add = |sid: usize, alpha: usize, v: &[(u32, u16)]| {
        if !v.is_empty() {
            tally[sid]
                .get_or_insert_with(|| rans::Tally::new(alpha))
                .add(v);
        }
    };
    for chunk in segs.chunks(threads.max(1)) {
        let nums = par_map(threads, chunk.len(), |i| numerics(chunk[i].0, chunk[i].1));
        for (&(_, wk), nums) in chunk.iter().zip(nums) {
            for sid in 0..N_LAZY {
                add(
                    sid,
                    256,
                    &rans::byte_syms(&wk.io.syms[sid], &wk.io.ctxs[sid]),
                );
            }
            for (g, vals) in groups().iter().zip(nums) {
                for (&sid, x) in g.streams().iter().zip(vals) {
                    add(sid, rans::NALPHA, &rans::num_syms(sid, &x).0);
                }
            }
        }
    }
    let mut models: Vec<Option<rans::Shared>> = par_map(threads, N_STREAMS, |sid| {
        tally[sid].as_ref().and_then(|t| t.shared())
    });
    fit_tables(&mut models, TABLES_MAX);
    models
}

/// The TABLES body (FORMAT.md §2.6) of the models of streams `ids`
/// (ascending, each with a model).
pub fn tables_body(models: &[Option<rans::Shared>], ids: &[usize]) -> Vec<u8> {
    let mut t = Vec::new();
    put_uv(&mut t, ids.len() as u64);
    for &sid in ids {
        let h = &models[sid].as_ref().expect("a model").hdr;
        put_uv(&mut t, sid as u64);
        put_uv(&mut t, h.len() as u64);
        t.extend_from_slice(h);
    }
    t
}

/// Drops the models with the largest headers until their TABLES body is
/// at most `max` bytes (the section's limit, FORMAT.md §9); those streams
/// then code with a model in each segment (back end 2).
fn fit_tables(models: &mut [Option<rans::Shared>], max: u64) {
    loop {
        let ids: Vec<usize> = (0..models.len()).filter(|&i| models[i].is_some()).collect();
        if ids.is_empty() || tables_body(models, &ids).len() as u64 <= max {
            return;
        }
        let big = ids.into_iter().max_by_key(|&i| {
            let h = models[i].as_ref().map_or(0, |m| m.hdr.len());
            (h, i)
        });
        models[big.expect("a model")] = None;
    }
}

/// The transform body of a walked segment; `dinfo_off` is the offset of its
/// demoinfo share in the metadata store. A stream with a model in `shared`
/// (by stream id; empty: none) uses it.
pub fn encode(
    src: &[u8],
    wk: &Walked,
    level: Level,
    dinfo_off: u64,
    stats: &mut Vec<StreamStat>,
    shared: &[Option<rans::Shared>],
) -> Vec<u8> {
    let groups = groups();
    let nums = numerics(src, wk);
    // every non-empty stream is a job: (sid, kind, group, index in group)
    let mut jobs: Vec<(usize, Kind, usize, usize)> = Vec::new();
    for sid in 0..N_LAZY {
        if !wk.io.syms[sid].is_empty() {
            jobs.push((sid, Kind::Lazy, 0, 0));
        }
    }
    for (gi, g) in groups.iter().enumerate() {
        for (k, &sid) in g.streams().iter().enumerate() {
            if !nums[gi][k].is_empty() {
                jobs.push((sid, Kind::Num, gi, k));
            }
        }
    }
    for sid in 0..N_STREAMS {
        if is_blob(sid) && !wk.io.blobs[sid].is_empty() {
            jobs.push((sid, Kind::Blob, 0, 0));
        }
    }
    let packed: Vec<(u8, Vec<u8>)> = jobs
        .iter()
        .map(|&(sid, kind, gi, i)| {
            let num: &[(NCtx, i32)] = if matches!(kind, Kind::Num) {
                &nums[gi][i]
            } else {
                &[]
            };
            let file = shared.get(sid).and_then(|m| m.as_ref());
            pack_best(kind, sid, level, wk, num, file)
        })
        .collect();
    let mut ids: Vec<usize> = (0..jobs.len()).collect();
    ids.sort_by_key(|&i| jobs[i].0);
    let count = |i: usize| match jobs[i].1 {
        Kind::Lazy => wk.io.syms[jobs[i].0].len(),
        Kind::Num => nums[jobs[i].2][jobs[i].3].len(),
        Kind::Blob => wk.io.blobs[jobs[i].0].len(),
    };
    // the flag only when the segment has demoinfo blocks: without any,
    // the walk is the same either way and needs no metadata
    let share = wk.dinfo && !wk.dinfo().is_empty();
    let mut out = Vec::new();
    put_uv(&mut out, if share { SF_DINFO } else { 0 });
    put_uv(&mut out, if share { dinfo_off } else { 0 });
    put_uv(&mut out, wk.dinfo().len() as u64);
    put_uv(&mut out, wk.proto0.fte as u64);
    put_uv(&mut out, wk.proto0.fte2 as u64);
    put_uv(&mut out, wk.proto0.mvd1 as u64);
    put_uv(&mut out, wk.tail as u64);
    put_uv(&mut out, ids.len() as u64);
    for &i in &ids {
        let (be, body) = &packed[i];
        put_uv(&mut out, jobs[i].0 as u64);
        put_uv(&mut out, *be as u64);
        put_uv(&mut out, count(i) as u64);
        put_uv(&mut out, body.len() as u64);
        stats.push(StreamStat {
            id: jobs[i].0,
            be: *be,
            count: count(i),
            packed: body.len(),
        });
    }
    for &i in &ids {
        out.extend_from_slice(&packed[i].1);
    }
    out
}

/// Cut points: frame boundaries about every `seg` bytes (0: one segment).
pub fn cut_points(src: &[u8], seg: usize) -> Vec<usize> {
    let mut cuts = vec![0];
    if seg > 0 {
        let mut p = 0;
        let mut start = 0;
        while let Some(n) = frame_len(src, p) {
            p += n;
            if p - start >= seg && p < src.len() {
                cuts.push(p);
                start = p;
            }
        }
    }
    cuts.push(src.len());
    cuts
}

/// Frames, summed frame time (ms) and the length of the whole frames at
/// the start of `b` (the rest is the unframeable tail).
pub fn frame_scan(b: &[u8]) -> (u64, u64, usize) {
    let (mut frames, mut ms, mut p) = (0u64, 0u64, 0usize);
    while let Some(n) = frame_len(b, p) {
        frames += 1;
        ms += b[p] as u64;
        p += n;
    }
    (frames, ms, p)
}

// ---------------------------------------------------------------- decode

/// The parsed header of a transform body.
pub struct Header {
    pub flags: u64,
    pub dinfo_off: u64,
    pub dinfo_len: u64,
    proto: Proto,
    tail: usize,
    ents: Vec<(usize, u8, usize, usize)>,
    body_off: usize,
}

/// Parses the header and stream table of a transform body that decodes to
/// `raw_len` bytes.
pub fn parse_header(body: &[u8], raw_len: usize) -> Result<Header> {
    let e = Error::Corrupt("segment header");
    let mut r = Rd::new(body);
    let flags = r.uv().ok_or(e)?;
    let dinfo_off = r.uv().ok_or(e)?;
    let dinfo_len = r.uv().ok_or(e)?;
    let fte = r.u32v().ok_or(e)?;
    let fte2 = r.u32v().ok_or(e)?;
    let mvd1 = r.u32v().ok_or(e)?;
    let tail = r.uz().ok_or(e)?;
    let n = r.uz().ok_or(e)?;
    if flags & !SF_DINFO != 0 {
        return bad("segment flags");
    }
    if flags & SF_DINFO == 0 && (dinfo_off != 0 || dinfo_len != 0) {
        return bad("demoinfo share");
    }
    if tail > raw_len {
        return bad("tail length");
    }
    if n > N_STREAMS {
        return bad("stream count");
    }
    let te = Error::Corrupt("stream table");
    let mut ents = Vec::with_capacity(n);
    let mut seen = [false; N_STREAMS];
    let mut total = 0usize;
    for _ in 0..n {
        let id = r.uz().ok_or(te)?;
        let be = r.uv().ok_or(te)?;
        let count = r.uz().ok_or(te)?;
        let len = r.uz().ok_or(te)?;
        if id >= N_STREAMS || !(is_lazy(id) || is_num(id) || is_blob(id)) {
            return bad("stream id");
        }
        let ok_be = if is_blob(id) {
            be == BE_STORE as u64 || be == BE_ZSTD as u64
        } else {
            be <= BE_FILE as u64
        };
        if !ok_be {
            return bad("stream back end");
        }
        if seen[id] {
            return bad("duplicate stream");
        }
        seen[id] = true;
        // no stream holds more symbols than the output has bytes (plus slack)
        if count > raw_len.saturating_add(16) {
            return bad("stream count");
        }
        total = total.checked_add(len).ok_or(te)?;
        ents.push((id, be as u8, count, len));
    }
    if total != body.len() - r.i {
        return bad("stream bodies");
    }
    Ok(Header {
        flags,
        dinfo_off,
        dinfo_len,
        proto: Proto::new(fte, fte2, mvd1),
        tail,
        ents,
        body_off: r.i,
    })
}

/// Decodes a transform body into `dst` (its exact raw length). `dinfo` is
/// the segment's share of the demoinfo store. The CRC is checked by the
/// caller.
pub fn decode(
    body: &[u8],
    h: &Header,
    dst: &mut [u8],
    dinfo: &[u8],
    tables: Option<&FileTables>,
    budget: &Budget,
) -> Result<()> {
    let n = h.ents.len();
    // the file's models, built for the first segment that uses them
    let fm = match tables {
        Some(t) if h.ents.iter().any(|e| e.1 == BE_FILE) => Some(t.models(budget)?),
        _ => None,
    };
    let mut bodies: Vec<&[u8]> = Vec::with_capacity(n);
    let mut r = Rd {
        b: body,
        i: h.body_off,
    };
    for &(_, _, _, len) in &h.ents {
        bodies.push(r.take(len).ok_or(Error::Corrupt("stream body"))?);
    }
    let cnt = |sid: usize| h.ents.iter().find(|e| e.0 == sid).map_or(0, |e| e.2);
    // scratch: decoded blobs and stored/zstd streams, the record lists
    let recs_bytes = 12 * cnt(PI_NUM)
        + 16 * cnt(PE_BITS)
        + 20 * cnt(SND_NUM)
        + 20 * cnt(TE_TYPE)
        + 8 * cnt(STL_IDX)
        + 8 * (cnt(UC) / UC_MIN_SYMS);
    let mut scratch = recs_bytes as u64;
    for &(id, be, count, _) in &h.ents {
        scratch += if is_blob(id) && be == BE_ZSTD {
            count as u64
        } else {
            coder::scratch(be, is_num(id), count)
        };
    }
    let _charge = budget.alloc(scratch)?;
    // blob streams first
    let mut blob_bufs: Vec<(Cow<[u8]>, usize)> = (0..N_STREAMS)
        .map(|_| (Cow::Borrowed(&[][..]), 0))
        .collect();
    for i in (0..n).filter(|&i| is_blob(h.ents[i].0)) {
        let (id, be, count, _) = h.ents[i];
        blob_bufs[id].0 = coder::decode_blob(be, bodies[i], count)?;
    }
    blob_bufs[DINFO].0 = Cow::Borrowed(dinfo);
    let mut num_ents: Vec<StreamEnt> = vec![None; N_STREAMS];
    let mut lazy_ents: Vec<StreamEnt> = vec![None; N_LAZY];
    for i in 0..n {
        let (id, be, count, _) = h.ents[i];
        if is_lazy(id) {
            lazy_ents[id] = Some((be, bodies[i], count));
        } else if is_num(id) {
            num_ents[id] = Some((be, bodies[i], count));
        }
    }
    let mut recs = Recs::fixed(
        cnt(PI_NUM),
        cnt(PE_BITS),
        cnt(SND_NUM),
        cnt(TE_TYPE),
        cnt(STL_IDX),
        cnt(UC) / UC_MIN_SYMS,
    )?;
    let buf = Buf::new(dst);
    let dinfo_on = h.flags & SF_DINFO != 0;
    // the walk; a monomorphic fast path when every structural stream is rANS
    if lazy_ents
        .iter()
        .all(|e| e.is_none_or(|e| e.0 == BE_RANS || e.0 == BE_FILE))
    {
        let mut lazy: [rans::ByteDec; N_LAZY] = std::array::from_fn(|_| rans::ByteDec::empty());
        for (id, e) in lazy_ents.iter().enumerate() {
            if let Some((be, b, c)) = *e {
                lazy[id] = rans::ByteDec::new(b, c, coder::file_model(be, fm, id)?, budget)?;
            }
        }
        walk_dec(lazy, blob_bufs, buf, &mut recs, h.proto, h.tail, dinfo_on)?;
    } else {
        let mut lazy: [LazyDec; N_LAZY] = std::array::from_fn(|_| LazyDec::Empty);
        for (id, e) in lazy_ents.iter().enumerate() {
            if let Some((be, b, c)) = *e {
                lazy[id] = LazyDec::new(be, id, b, c, fm, budget)?;
            }
        }
        walk_dec(lazy, blob_bufs, buf, &mut recs, h.proto, h.tail, dinfo_on)?;
    }
    // wave 2 reads coordinates wave 1 wrote
    for g in WAVE1.into_iter().chain(WAVE2) {
        let all_rans = g
            .streams()
            .iter()
            .all(|&sid| num_ents[sid].is_none_or(|e| e.0 == BE_RANS || e.0 == BE_FILE));
        if all_rans {
            run_group(g, &recs, buf, &num_ents, |be, sid, b, c| {
                rans::NumDec::new(sid, b, c, coder::file_model(be, fm, sid)?, budget)
            })?;
        } else {
            run_group(g, &recs, buf, &num_ents, |be, sid, b, c| {
                NumDec::new(be, sid, b, c, fm, budget)
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Budget;

    /// The segment `body` decoded (without demoinfo sharing).
    fn decoded(body: &[u8], h: &Header, len: usize, tables: Option<&FileTables>) -> Vec<u8> {
        let mut dst = vec![0u8; len];
        decode(body, h, &mut dst, &[], tables, &Budget::unlimited()).unwrap();
        dst
    }

    /// A tiny demo: playerinfo, packet entities, sounds, stats, a print.
    fn demo() -> Vec<u8> {
        let mut d = Vec::new();
        for f in 0..12u8 {
            let mut p = vec![42, 0];
            p.extend_from_slice(&0x3fu16.to_le_bytes());
            p.push(f);
            for k in 0..6u16 {
                p.extend_from_slice(&(100 * k + f as u16 * 7).to_le_bytes());
            }
            p.extend_from_slice(&[47, 1, 2]);
            p.extend_from_slice(&(1u16 << 9 | 5).to_le_bytes());
            p.extend_from_slice(&(10 + f as u16).to_le_bytes());
            p.extend_from_slice(&[0, 0]);
            p.extend_from_slice(&[6, 9, 0, 1]);
            p.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
            p.extend_from_slice(&[3, 1, f, 8, 2]);
            p.extend_from_slice(b"hi\0");
            d.push(13);
            d.push(6);
            d.extend_from_slice(&(p.len() as u32).to_le_bytes());
            d.extend_from_slice(&p);
        }
        d
    }

    #[test]
    fn segment_round_trip() {
        let src = demo();
        // Miri cannot run libzstd (C): the normal level only
        let levels: &[Level] = if cfg!(miri) {
            &[Level::Normal]
        } else {
            &[Level::Normal, Level::Fast]
        };
        for &level in levels {
            let (wk, _) = walk(&src, Proto::default(), true).unwrap();
            let mut st = Vec::new();
            let body = encode(&src, &wk, level, 0, &mut st, &[]);
            let h = parse_header(&body, src.len()).unwrap();
            let dst = decoded(&body, &h, src.len(), None);
            assert!(dst == src, "{level:?}");
        }
    }
    /// usercmd hidden blocks (type 1): on and off the grid, other lengths,
    /// two in one payload; streams UC and UC_A only where a segment has one.
    #[test]
    fn usercmd_round_trip() {
        let mut src = demo();
        let odd = [-0.0f32, f32::NAN, 180.0, -180.0, 1.2345, 359.99];
        for i in 0..300u32 {
            let mut p = Vec::new();
            for j in 0..1 + (i % 37 == 0) as u32 {
                let mut b = vec![((i + j) % 5) as u8, (i % 50 == 0) as u8, 13];
                let yaw = ((i * 97) as u16 as i16 as f32) * (45.0 / 8192.0);
                let pitch = if i % 11 == 0 {
                    odd[(i as usize / 11) % odd.len()]
                } else {
                    -(i as f32) * (45.0 / 8192.0)
                };
                for a in [pitch, yaw, 0.0] {
                    b.extend_from_slice(&a.to_le_bytes());
                }
                for m in [if i % 3 == 0 { 400i16 } else { 0 }, -508, 0] {
                    b.extend_from_slice(&m.to_le_bytes());
                }
                b.extend_from_slice(&[(i % 4) as u8, (i % 90 == 0) as u8]);
                p.extend_from_slice(&23u32.to_le_bytes());
                p.extend_from_slice(&1u16.to_le_bytes());
                p.extend_from_slice(&b);
            }
            if i % 50 == 7 {
                p.extend_from_slice(&3u32.to_le_bytes());
                p.extend_from_slice(&1u16.to_le_bytes());
                p.extend_from_slice(&[1, 2, 3]);
            }
            src.push(if i % 3 == 0 { 13 } else { 0 });
            src.push(3);
            src.extend_from_slice(&0u32.to_le_bytes());
            src.extend_from_slice(&(p.len() as u32).to_le_bytes());
            src.extend_from_slice(&p);
        }
        src.extend(demo());
        let levels: &[Level] = if cfg!(miri) {
            &[Level::Normal]
        } else {
            &[Level::Normal, Level::Fast]
        };
        for &level in levels {
            let (wk, _) = walk(&src, Proto::default(), true).unwrap();
            let body = encode(&src, &wk, level, 0, &mut Vec::new(), &[]);
            let h = parse_header(&body, src.len()).unwrap();
            let uc = h.ents.iter().find(|e| e.0 == UC).unwrap().2;
            assert!(uc >= (300 + 9) * UC_MIN_SYMS, "{uc}");
            let dst = decoded(&body, &h, src.len(), None);
            assert!(dst == src, "{level:?}");
        }
        // without usercmd blocks neither stream is in the table
        let plain = demo();
        let (wk, _) = walk(&plain, Proto::default(), true).unwrap();
        let body = encode(&plain, &wk, Level::Normal, 0, &mut Vec::new(), &[]);
        let h = parse_header(&body, plain.len()).unwrap();
        assert!(h.ents.iter().all(|e| e.0 != UC && e.0 != UC_A));
    }

    /// Models a TABLES section over its limit would hold are dropped,
    /// largest header first; their streams then code with back end 2, and
    /// the segment still decodes.
    #[test]
    fn tables_limit() {
        let src = demo();
        let (wk, _) = walk(&src, Proto::default(), true).unwrap();
        let mut models = train(&[(&src[..], &wk), (&src[..], &wk)], 1);
        let ids = |m: &[Option<rans::Shared>]| -> Vec<usize> {
            (0..m.len()).filter(|&i| m[i].is_some()).collect()
        };
        let all = ids(&models);
        assert!(all.iter().any(|&sid| is_num(sid)) && all.iter().any(|&sid| is_lazy(sid)));
        let len = tables_body(&models, &all).len() as u64;
        let hdr = |sid: usize| models[sid].as_ref().unwrap().hdr.len();
        let big = *all.iter().max_by_key(|&&sid| (hdr(sid), sid)).unwrap();
        fit_tables(&mut models, len);
        assert_eq!(ids(&models), all);
        fit_tables(&mut models, len - 1);
        let kept = ids(&models);
        assert_eq!(
            kept,
            all.iter()
                .copied()
                .filter(|&s| s != big)
                .collect::<Vec<_>>()
        );
        let ft = FileTables::new(tables_body(&models, &kept), &Budget::unlimited()).unwrap();
        let mut st = Vec::new();
        let body = encode(&src, &wk, Level::Normal, 0, &mut st, &models);
        let be = |sid: usize| st.iter().find(|s| s.id == sid).unwrap().be;
        assert_eq!(be(big), BE_RANS);
        assert!(kept.iter().all(|&sid| be(sid) == BE_FILE));
        let h = parse_header(&body, src.len()).unwrap();
        assert!(decoded(&body, &h, src.len(), Some(&ft)) == src);
        // nothing fits: no model is kept
        fit_tables(&mut models, 0);
        assert!(ids(&models).is_empty());
    }
}
