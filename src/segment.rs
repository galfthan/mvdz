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
use crate::numeric::{Buf, Group, NCtx, NumIO, WAVE1, WAVE1_PIPE, WAVE2};
use crate::proto::{Proto, frame_len};
use crate::streams::*;
use crate::util::{HAS_THREADS, Rd, par_map, put_uv};
use crate::walk::{Recs, SymIO, Walker};
use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Segment flag: KTX demoinfo blocks come from the metadata's store.
pub const SF_DINFO: u64 = 1;
/// Segment flag: usercmd hidden blocks (type 1, 23 bytes) are coded field
/// by field (streams UC and UC_A).
pub const SF_UCMD: u64 = 4;

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

const MAXK: usize = 6;

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
    recs: &Recs,
    proto: Proto,
    tail: usize,
    dinfo: bool,
    ucmd: bool,
) -> Result<()> {
    let io = DecIO {
        lazy,
        blobs,
        over: false,
    };
    let mut w: Walker<false, DecIO<L>> =
        Walker::new(&[], dst.ptr(), dst.len(), recs, io, proto, dinfo, ucmd);
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
    /// usercmd blocks coded field by field (SF_UCMD when any)
    uc_n: usize,
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
    let recs = Recs::growable(src.len());
    let (io, tail, pend, err, uc_n) = {
        let mut w: Walker<true, EncIO> = Walker::new(
            src,
            std::ptr::null_mut(),
            0,
            &recs,
            EncIO::new(),
            proto,
            dinfo,
            true,
        );
        let tail = w.run(0);
        (w.io, tail, w.proto, w.err, w.uc_n)
    };
    recs.finish();
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
            uc_n,
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
/// code the stream, the fallback list is tried.
fn candidates(level: Level, kind: Kind) -> &'static [u8] {
    match (level, kind) {
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
) -> Option<Vec<u8>> {
    let z = level.zstd();
    match kind {
        Kind::Lazy => coder::encode_lazy(be, &wk.io.syms[sid], &wk.io.ctxs[sid], z),
        Kind::Num => coder::encode_num(be, sid, num, z),
        Kind::Blob => coder::encode_blob(be, &wk.io.blobs[sid], z),
    }
}

fn pack_best(
    kind: Kind,
    sid: usize,
    level: Level,
    wk: &Walked,
    num: &[(NCtx, i32)],
) -> (u8, Vec<u8>) {
    for list in [
        candidates(level, kind),
        if cfg!(miri) {
            &[BE_STORE]
        } else {
            &[BE_STORE, BE_ZSTD]
        },
    ] {
        let mut best: Option<(u8, Vec<u8>)> = None;
        for &be in list {
            if let Some(c) = pack_one(kind, be, sid, level, wk, num) {
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

/// The numeric groups in coding order (wave 1, then wave 2).
fn groups() -> Vec<Group> {
    WAVE1.iter().chain(WAVE2.iter()).copied().collect()
}

/// The numeric values of a walked segment, per group (in [`groups`]
/// order) and stream of the group.
pub struct Nums(Vec<Vec<Vec<(NCtx, i32)>>>);

/// Runs the numeric groups of a walked segment (encode direction).
pub fn numerics(src: &[u8], wk: &Walked, threads: usize) -> Nums {
    let groups = groups();
    let buf = Buf::ro(src);
    let recs = &wk.recs;
    Nums(par_map(threads, groups.len(), |gi| {
        let g = groups[gi];
        let mut io = NumEnc {
            vals: g.streams().iter().map(|_| Vec::new()).collect(),
        };
        g.run::<true, NumEnc>(recs, buf, &mut io);
        io.vals
    }))
}

/// One coded stream of a segment.
pub struct Coded {
    sid: usize,
    count: usize,
    /// the segment's own coding: back end and body
    own: (u8, Vec<u8>),
    /// the body coded with the file's shared model (back end 3), when the
    /// file has one for the stream that codes every symbol of it
    file: Option<Vec<u8>>,
}

impl Coded {
    /// Bytes back end 3 saves over the own coding in this segment.
    fn saving(&self) -> usize {
        self.file
            .as_ref()
            .map_or(0, |f| self.own.1.len().saturating_sub(f.len()))
    }
}

/// Shared models of a file, by stream id (None: no shared model).
pub type SharedSet = Vec<Option<rans::Shared>>;

/// Codes every non-empty stream of a walked segment, in stream id order.
/// With `shared`, the rANS streams that have a shared model are also coded
/// with it (normal level).
pub fn code_streams(
    wk: &Walked,
    nums: &Nums,
    level: Level,
    shared: Option<&SharedSet>,
    threads: usize,
) -> Vec<Coded> {
    let groups = groups();
    let nums = &nums.0;
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
    let count = |j: &(usize, Kind, usize, usize)| match j.1 {
        Kind::Lazy => wk.io.syms[j.0].len(),
        Kind::Num => nums[j.2][j.3].len(),
        Kind::Blob => wk.io.blobs[j.0].len(),
    };
    let size = |j: &(usize, Kind, usize, usize)| match j.1 {
        Kind::Num => 2 * count(j),
        _ => count(j),
    };
    // biggest first for load balance; output in stream id order
    let mut order: Vec<usize> = (0..jobs.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(size(&jobs[i])));
    let coded_sorted = par_map(threads, jobs.len(), |k| {
        let j = jobs[order[k]];
        let (sid, kind, gi, i) = j;
        let num: &[(NCtx, i32)] = if matches!(kind, Kind::Num) {
            &nums[gi][i]
        } else {
            &[]
        };
        let own = pack_best(kind, sid, level, wk, num);
        let sh = shared.and_then(|s| s[sid].as_ref());
        let file = match (level, kind, sh) {
            (Level::Normal, Kind::Lazy, Some(sh)) => {
                rans::encode_bytes_shared(sh, &wk.io.syms[sid], &wk.io.ctxs[sid])
            }
            (Level::Normal, Kind::Num, Some(sh)) => rans::encode_num_shared(sh, sid, num),
            _ => None,
        };
        Coded {
            sid,
            count: count(&j),
            own,
            file,
        }
    });
    let mut coded: Vec<Option<Coded>> = (0..jobs.len()).map(|_| None).collect();
    for (k, c) in coded_sorted.into_iter().enumerate() {
        coded[order[k]] = Some(c);
    }
    let mut coded: Vec<Coded> = coded.into_iter().map(|c| c.expect("coded")).collect();
    coded.sort_by_key(|c| c.sid);
    coded
}

/// Trains the shared models of a file on its walked segments: one per
/// rANS stream (structural and numeric) that has symbols, from its symbols
/// in every segment (FORMAT.md §12).
pub fn train(segs: &[(&Walked, &Nums)], threads: usize) -> SharedSet {
    let groups = groups();
    // (sid, group and index of a numeric stream), with its total count
    let mut cand: Vec<(usize, Option<(usize, usize)>, usize)> = Vec::new();
    for sid in 0..N_LAZY {
        let n: usize = segs.iter().map(|(w, _)| w.io.syms[sid].len()).sum();
        if n > 0 {
            cand.push((sid, None, n));
        }
    }
    for (gi, g) in groups.iter().enumerate() {
        for (k, &sid) in g.streams().iter().enumerate() {
            let n: usize = segs.iter().map(|(_, v)| v.0[gi][k].len()).sum();
            if n > 0 {
                cand.push((sid, Some((gi, k)), n));
            }
        }
    }
    cand.sort_by_key(|c| std::cmp::Reverse(c.2));
    let models = par_map(threads, cand.len(), |i| match cand[i] {
        (sid, None, _) => {
            let v: Vec<(&[u8], &[u32])> = segs
                .iter()
                .map(|(w, _)| (&w.io.syms[sid][..], &w.io.ctxs[sid][..]))
                .collect();
            rans::shared_bytes(&v)
        }
        (sid, Some((gi, k)), _) => {
            let v: Vec<&[(NCtx, i32)]> = segs.iter().map(|(_, n)| &n.0[gi][k][..]).collect();
            rans::shared_num(sid, &v)
        }
    });
    let mut set: SharedSet = (0..N_STREAMS).map(|_| None).collect();
    for (i, m) in models.into_iter().enumerate() {
        set[cand[i].0] = m;
    }
    set
}

/// Which shared models the file keeps: a stream's model goes into the
/// TABLES section when the bytes it saves over all segments exceed what
/// its entry costs; none at all when the section would not pay for its
/// own header (an empty result: no TABLES section).
pub fn choose_shared(shared: &SharedSet, coded: &[&[Coded]]) -> Vec<bool> {
    let mut save = vec![0usize; N_STREAMS];
    for cs in coded {
        for c in cs.iter() {
            save[c.sid] += c.saving();
        }
    }
    let mut keep = vec![false; N_STREAMS];
    let (mut net, mut n) = (0usize, 0u64);
    for sid in 0..N_STREAMS {
        if let Some(sh) = &shared[sid] {
            let cost = uv_len(sid as u64) + uv_len(sh.hdr().len() as u64) + sh.hdr().len();
            if save[sid] > cost {
                keep[sid] = true;
                net += save[sid] - cost;
                n += 1;
            }
        }
    }
    // the section header (kind, length) and the entry count
    if n == 0 || net <= 9 + uv_len(n) {
        return Vec::new();
    }
    keep
}

/// The TABLES section body of the models `keep` selects (FORMAT.md §2.6).
pub fn tables_body(shared: &SharedSet, keep: &[bool]) -> Vec<u8> {
    let ids: Vec<usize> = (0..keep.len()).filter(|&s| keep[s]).collect();
    let mut out = Vec::new();
    put_uv(&mut out, ids.len() as u64);
    for sid in ids {
        let h = shared[sid].as_ref().expect("kept model").hdr();
        put_uv(&mut out, sid as u64);
        put_uv(&mut out, h.len() as u64);
        out.extend_from_slice(h);
    }
    out
}

fn uv_len(v: u64) -> usize {
    let mut b = Vec::new();
    put_uv(&mut b, v);
    b.len()
}

/// The transform body of a walked segment from its coded streams. A
/// stream whose model the file keeps (`keep[sid]`) uses back end 3 where
/// that is smaller than its own coding. `dinfo_off` is the offset of the
/// segment's demoinfo share in the metadata store.
pub fn body(
    wk: &Walked,
    coded: &[Coded],
    keep: &[bool],
    dinfo_off: u64,
    stats: &mut Vec<StreamStat>,
) -> Vec<u8> {
    let pick = |c: &Coded| -> (u8, usize) {
        match &c.file {
            Some(f) if keep.get(c.sid) == Some(&true) && f.len() < c.own.1.len() => {
                (BE_FILE, f.len())
            }
            _ => (c.own.0, c.own.1.len()),
        }
    };
    // the flag only when the segment has demoinfo blocks: without any,
    // the walk is the same either way and needs no metadata
    let share = wk.dinfo && !wk.dinfo().is_empty();
    let mut out = Vec::new();
    // likewise SF_UCMD only when the segment has usercmd blocks
    let ucmd = if wk.uc_n > 0 { SF_UCMD } else { 0 };
    put_uv(&mut out, if share { SF_DINFO } else { 0 } | ucmd);
    put_uv(&mut out, if share { dinfo_off } else { 0 });
    put_uv(&mut out, wk.dinfo().len() as u64);
    put_uv(&mut out, wk.proto0.fte as u64);
    put_uv(&mut out, wk.proto0.fte2 as u64);
    put_uv(&mut out, wk.proto0.mvd1 as u64);
    put_uv(&mut out, wk.tail as u64);
    put_uv(&mut out, coded.len() as u64);
    for c in coded {
        let (be, len) = pick(c);
        put_uv(&mut out, c.sid as u64);
        put_uv(&mut out, be as u64);
        put_uv(&mut out, c.count as u64);
        put_uv(&mut out, len as u64);
        stats.push(StreamStat {
            id: c.sid,
            be,
            count: c.count,
            packed: len,
        });
    }
    for c in coded {
        match pick(c).0 {
            BE_FILE => out.extend_from_slice(c.file.as_ref().expect("file body")),
            _ => out.extend_from_slice(&c.own.1),
        }
    }
    out
}

/// The transform body of a walked segment with per-segment tables only;
/// `dinfo_off` is the offset of its demoinfo share in the metadata store.
pub fn encode(
    src: &[u8],
    wk: &Walked,
    level: Level,
    dinfo_off: u64,
    threads: usize,
    stats: &mut Vec<StreamStat>,
) -> Vec<u8> {
    let nums = numerics(src, wk, threads);
    let coded = code_streams(wk, &nums, level, None, threads);
    body(wk, &coded, &[], dinfo_off, stats)
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
    if flags & !(SF_DINFO | SF_UCMD) != 0 {
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
    ft: Option<&FileTables>,
    threads: usize,
    budget: &Budget,
) -> Result<()> {
    let n = h.ents.len();
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
        + 8 * (cnt(UC) / 12);
    let mut scratch = recs_bytes as u64;
    for &(id, be, count, _) in &h.ents {
        scratch += if is_blob(id) && be == BE_ZSTD {
            count as u64
        } else {
            coder::scratch(be, is_num(id), count)
        };
    }
    let _charge = budget.alloc(scratch)?;
    // blob streams first (in parallel)
    let blob_ix: Vec<usize> = (0..n).filter(|&i| is_blob(h.ents[i].0)).collect();
    let blobs = par_map(threads, blob_ix.len(), |j| {
        let i = blob_ix[j];
        let (_, be, count, _) = h.ents[i];
        coder::decode_blob(be, bodies[i], count)
    });
    let mut blob_bufs: Vec<(Cow<[u8]>, usize)> = (0..N_STREAMS)
        .map(|_| (Cow::Borrowed(&[][..]), 0))
        .collect();
    for (j, b) in blobs.into_iter().enumerate() {
        blob_bufs[h.ents[blob_ix[j]].0].0 = b?;
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
    let recs = Recs::fixed(
        cnt(PI_NUM),
        cnt(PE_BITS),
        cnt(SND_NUM),
        cnt(TE_TYPE),
        cnt(STL_IDX),
        // every usercmd block codes at least 12 UC symbols
        cnt(UC) / 12,
    )?;
    let buf = Buf::new(dst);
    let run_g = |g: Group| -> Result<()> {
        let all_rans = g
            .streams()
            .iter()
            .all(|&sid| num_ents[sid].is_none_or(|e| e.0 == BE_RANS || e.0 == BE_FILE));
        if all_rans {
            run_group(g, &recs, buf, &num_ents, |be, sid, b, c| {
                if be == BE_FILE {
                    rans::NumDec::new_file(sid, coder::file_model(ft, sid)?, b, c)
                } else {
                    rans::NumDec::new(sid, b, c, budget)
                }
            })
        } else {
            run_group(g, &recs, buf, &num_ents, |be, sid, b, c| {
                NumDec::new(be, sid, b, c, ft, budget)
            })
        }
    };
    let blob_cell = std::sync::Mutex::new(Some(blob_bufs));
    let dinfo_on = h.flags & SF_DINFO != 0;
    let ucmd_on = h.flags & SF_UCMD != 0;
    // the walk; a monomorphic fast path when every structural stream is rANS
    let walk = || -> Result<()> {
        let blobs = blob_cell
            .lock()
            .map_err(|_| Error::Internal("lock"))?
            .take()
            .ok_or(Error::Internal("walk"))?;
        if lazy_ents
            .iter()
            .all(|e| e.is_none_or(|e| e.0 == BE_RANS || e.0 == BE_FILE))
        {
            let mut lazy: [rans::ByteDec; N_LAZY] = std::array::from_fn(|_| rans::ByteDec::empty());
            for (id, e) in lazy_ents.iter().enumerate() {
                if let Some((be, b, c)) = *e {
                    lazy[id] = if be == BE_FILE {
                        rans::ByteDec::new_file(coder::file_model(ft, id)?, b, c)?
                    } else {
                        rans::ByteDec::new(b, c, budget)?
                    };
                }
            }
            walk_dec(lazy, blobs, buf, &recs, h.proto, h.tail, dinfo_on, ucmd_on)
        } else {
            let mut lazy: [LazyDec; N_LAZY] = std::array::from_fn(|_| LazyDec::Empty);
            for (id, e) in lazy_ents.iter().enumerate() {
                if let Some((be, b, c)) = *e {
                    lazy[id] = LazyDec::new(be, id, b, c, ft, budget)?;
                }
            }
            walk_dec(lazy, blobs, buf, &recs, h.proto, h.tail, dinfo_on, ucmd_on)
        }
    };
    if threads > 1 && HAS_THREADS {
        // pipelined: wave-1 groups follow the walker on worker threads
        let next = AtomicUsize::new(0);
        let (wres, gres) = std::thread::scope(|sc| {
            let workers: Vec<_> = (0..(threads - 1).min(WAVE1_PIPE.len()))
                .map(|_| {
                    sc.spawn(|| -> Result<()> {
                        let mut res = Ok(());
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= WAVE1_PIPE.len() {
                                break;
                            }
                            let r = run_g(WAVE1_PIPE[i]);
                            if res.is_ok() {
                                res = r;
                            }
                        }
                        res
                    })
                })
                .collect();
            // finish the record buffers on every exit, unwinding included,
            // so worker threads can never wait forever
            struct Finish<'r>(&'r Recs);
            impl Drop for Finish<'_> {
                fn drop(&mut self) {
                    self.0.finish();
                }
            }
            let guard = Finish(&recs);
            let wres = walk();
            drop(guard);
            let mut gres = Ok(());
            for h in workers {
                let r = h.join().unwrap_or(Err(Error::Internal("worker panicked")));
                if gres.is_ok() {
                    gres = r;
                }
            }
            (wres, gres)
        });
        wres?;
        gres?;
    } else {
        let r = walk();
        recs.finish();
        r?;
        for &g in &WAVE1 {
            run_g(g)?;
        }
    }
    for r in par_map(threads.min(2), WAVE2.len(), |i| run_g(WAVE2[i])) {
        r?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Budget;

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
            for threads in [1, 3] {
                let (wk, _) = walk(&src, Proto::default(), true).unwrap();
                let mut st = Vec::new();
                let body = encode(&src, &wk, level, 0, 1, &mut st);
                let h = parse_header(&body, src.len()).unwrap();
                let mut dst = vec![0u8; src.len()];
                decode(
                    &body,
                    &h,
                    &mut dst,
                    &[],
                    None,
                    threads,
                    &Budget::unlimited(),
                )
                .unwrap();
                assert!(dst == src, "{level:?} {threads}");
            }
        }
    }
    /// usercmd hidden blocks (type 1): on and off the grid, other lengths,
    /// two in one payload; SF_UCMD only where a segment has one.
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
            for threads in [1, 3] {
                let (wk, _) = walk(&src, Proto::default(), true).unwrap();
                assert_eq!(wk.uc_n, 300 + 9);
                let body = encode(&src, &wk, level, 0, 1, &mut Vec::new());
                let h = parse_header(&body, src.len()).unwrap();
                assert_eq!(h.flags, SF_UCMD);
                let mut dst = vec![0u8; src.len()];
                decode(
                    &body,
                    &h,
                    &mut dst,
                    &[],
                    None,
                    threads,
                    &Budget::unlimited(),
                )
                .unwrap();
                assert!(dst == src, "{level:?} {threads}");
            }
        }
        // without usercmd blocks the flag stays clear
        let plain = demo();
        let (wk, _) = walk(&plain, Proto::default(), true).unwrap();
        let body = encode(&plain, &wk, Level::Normal, 0, 1, &mut Vec::new());
        assert_eq!(parse_header(&body, plain.len()).unwrap().flags, 0);
    }
}
