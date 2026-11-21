//! The MVDZ container (FORMAT.md, Part I): a 16-byte prefix and
//! length-prefixed sections (metadata, segments, index,
//! trailer), and the one set of layout rules every reader applies
//! ([`Layout`]).
//!
//! ```text
//! file    := prefix section*
//! prefix  := "MVDZ" | 0x01 | flags u8 | 0x00 0x00 | meta_off u64le      (16 bytes)
//! section := kind u8 | len u64le | body[len]
//! ```

use crate::budget::{Budget, Charge};
use crate::error::{Error, Result, bad};
use crate::util::{Rd, crc32, put_uv, usz};

pub const MAGIC: &[u8; 4] = b"MVDZ";
pub const VERSION: u8 = 1;
pub const PREFIX_LEN: usize = 16;
pub const SEC_HDR: usize = 9;
pub const TRAILER_MAGIC: &[u8; 8] = b"MVDZTRL1";
/// trailer section: header + 2 u64 + magic
pub const TRAILER_LEN: usize = SEC_HDR + 16 + 8;

pub const K_META: u8 = 0x01;
pub const K_SEGMENT: u8 = 0x82;
pub const K_INDEX: u8 = 0x03;
pub const K_TRAILER: u8 = 0x7f;
/// the file's shared rANS models (back end 3), before every segment
pub const K_TABLES: u8 = 0x86;
pub const CRITICAL: u8 = 0x80;

/// prefix flag: written by the streaming encoder (metadata near the end)
pub const F_STREAMED: u8 = 1;

/// Segment codings.
pub const C_TRANSFORM: u8 = 0;
pub const C_ZSTD: u8 = 1;
pub const C_STORE: u8 = 2;

// ---------------------------------------------------------------- limits (FORMAT.md, "Limits")

/// Largest raw length of one segment (record offsets are 32-bit).
pub const MAX_SEG_RAW: u64 = 0xFFFF_0000;
/// A segment decodes to at most `SEG_RATIO * section length + SEG_SLACK`
/// bytes.
pub const SEG_RATIO: u64 = 1024;
pub const SEG_SLACK: u64 = 4096;
/// Largest META raw payload and INDEX section.
pub const META_MAX: u64 = 1 << 24;
pub const DIR_MAX: u64 = 1 << 24;
/// Largest TABLES section body.
pub const TABLES_MAX: u64 = 1 << 24;
/// Encoder caps for metadata values (FORMAT.md, "Metadata").
pub const DEMOINFO_MAX: usize = 4 << 20;
pub const EXT_MAX: usize = 1 << 20;

/// Most segments the reference encoders write in one file (an encoder
/// rule, not a reader rule: it keeps the INDEX far below [`DIR_MAX`] and
/// the one-shot encoder's per-segment bookkeeping bounded).
pub const MAX_SEGMENTS: usize = 1 << 16;

/// The ratio limit: `raw` bytes may be coded in `len` bytes.
pub fn ratio_ok(raw: u64, len: u64) -> bool {
    raw <= len.saturating_mul(SEG_RATIO).saturating_add(SEG_SLACK)
}

/// True if a segment of `raw_len` bytes may be stored in a section body of
/// `len` bytes.
pub fn seg_ratio_ok(raw_len: u64, len: u64) -> bool {
    raw_len <= MAX_SEG_RAW && ratio_ok(raw_len, len)
}

/// Checks extension metadata (FORMAT.md §3, tag 5) before anything is
/// written: at most 1 MiB of keys and values, keys unique.
pub fn check_ext(ext: &[(String, Vec<u8>)]) -> Result<()> {
    let ext_len: usize = ext.iter().map(|(k, v)| k.len() + v.len()).sum();
    if ext_len > EXT_MAX {
        return Err(Error::Invalid("extension metadata over 1 MiB"));
    }
    let mut seen = std::collections::HashSet::new();
    if !ext.iter().all(|(k, _)| seen.insert(k.as_str())) {
        return Err(Error::Invalid("duplicate extension key"));
    }
    Ok(())
}

// ---------------------------------------------------------------- prefix, sections, trailer

pub fn prefix(flags: u8, meta_off: u64) -> Vec<u8> {
    let mut p = Vec::with_capacity(PREFIX_LEN);
    p.extend_from_slice(MAGIC);
    p.push(VERSION);
    p.push(flags);
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(&meta_off.to_le_bytes());
    p
}

pub fn section(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&(body.len() as u64).to_le_bytes());
    out.extend_from_slice(body);
}

/// Checks the prefix; returns (flags, meta_off). Unknown flag bits and
/// bytes 6-7 are ignored.
pub fn check_prefix(b: &[u8]) -> Result<(u8, u64)> {
    if b.len() < 5 || &b[..4] != MAGIC {
        return Err(Error::Format("not an MVDZ file"));
    }
    if b[4] != VERSION {
        return Err(Error::Format("unsupported MVDZ version"));
    }
    if b.len() < PREFIX_LEN {
        return bad("truncated prefix");
    }
    Ok((b[5], u64::from_le_bytes(b[8..16].try_into().unwrap())))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trailer {
    pub index_off: u64,
    pub meta_off: u64,
}

impl Trailer {
    pub fn bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(24);
        for v in [self.index_off, self.meta_off] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(TRAILER_MAGIC);
        let mut out = Vec::with_capacity(TRAILER_LEN);
        section(&mut out, K_TRAILER, &b);
        out
    }
    /// Parses a trailer section body (24 bytes).
    pub fn parse_body(b: &[u8]) -> Result<Trailer> {
        if b.len() != 24 || &b[16..24] != TRAILER_MAGIC {
            return bad("trailer");
        }
        let u = |i: usize| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap());
        Ok(Trailer {
            index_off: u(0),
            meta_off: u(1),
        })
    }
}

/// One section of a file.
#[derive(Clone, Copy, Debug)]
pub struct Sec {
    pub kind: u8,
    pub off: u64,
    pub len: u64,
}

impl Sec {
    pub fn body_off(&self) -> u64 {
        self.off + SEC_HDR as u64
    }
    pub fn end(&self) -> u64 {
        self.off + SEC_HDR as u64 + self.len
    }
}

/// Parses a 9-byte section header at file offset `off`.
pub fn sec_hdr(h: &[u8], off: u64) -> Sec {
    Sec {
        kind: h[0],
        off,
        len: u64::from_le_bytes(h[1..9].try_into().unwrap()),
    }
}

// ---------------------------------------------------------------- metadata

pub const T_SHA256: u64 = 1;
pub const T_SIZE: u64 = 2;
pub const T_DEMOINFO: u64 = 3;
pub const T_TIMING: u64 = 4;
pub const T_EXT: u64 = 5;
/// Tags 1 to 5 occur at most once.
const N_TAGS: usize = 6;

/// Demo metadata (the META section): what decoding needs and checks, and
/// the writer's extension key/values. No game content (FORMAT.md §3).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    /// SHA-256 of the original .mvd: the demo's identity
    pub sha256: [u8; 32],
    pub size: u64,
    /// KTX demoinfo blocks the segments share, `[u32 len][body]` each, in
    /// file order
    pub demoinfo: Vec<u8>,
    pub duration_ms: u64,
    pub frames: u64,
    /// extension key/values, not part of the demo bytes
    pub ext: Vec<(String, Vec<u8>)>,
}

fn put_bytes(o: &mut Vec<u8>, b: &[u8]) {
    put_uv(o, b.len() as u64);
    o.extend_from_slice(b);
}

/// True if `b` is a sequence of `u32le n | n bytes` blocks, exactly.
pub fn demoinfo_ok(b: &[u8]) -> bool {
    let mut i = 0usize;
    while i < b.len() {
        let Some(h) = b.get(i..i + 4) else {
            return false;
        };
        let n = u32::from_le_bytes(h.try_into().unwrap()) as usize;
        match (i + 4).checked_add(n) {
            Some(e) if e <= b.len() => i = e,
            _ => return false,
        }
    }
    true
}

impl Metadata {
    /// Metadata of a demo from its identity (the demoinfo store is filled
    /// in by the encoder).
    pub fn new(mut id: crate::ident::Ident, ext: Vec<(String, Vec<u8>)>) -> Metadata {
        Metadata {
            sha256: id.sha256(),
            size: id.size,
            demoinfo: Vec::new(),
            duration_ms: id.duration_ms,
            frames: id.frames,
            ext,
        }
    }

    /// The META section body, zstd at the normal level's setting.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.encode_for(crate::segment::Level::Normal)
    }

    /// The META section body, zstd at `level`'s setting (as its streams).
    pub fn encode_for(&self, level: crate::segment::Level) -> Result<Vec<u8>> {
        check_ext(&self.ext)?;
        let mut p = Vec::new();
        let mut field = |tag: u64, v: &[u8]| {
            put_uv(&mut p, tag);
            put_bytes(&mut p, v);
        };
        field(T_SHA256, &self.sha256);
        let mut v = Vec::new();
        put_uv(&mut v, self.size);
        field(T_SIZE, &v);
        if !self.demoinfo.is_empty() {
            field(T_DEMOINFO, &self.demoinfo);
        }
        let mut v = Vec::new();
        put_uv(&mut v, self.duration_ms);
        put_uv(&mut v, self.frames);
        field(T_TIMING, &v);
        if !self.ext.is_empty() {
            let mut v = Vec::new();
            put_uv(&mut v, self.ext.len() as u64);
            for (k, x) in &self.ext {
                put_bytes(&mut v, k.as_bytes());
                put_bytes(&mut v, x);
            }
            field(T_EXT, &v);
        }
        if p.len() as u64 > META_MAX {
            return Err(Error::Invalid("metadata over 16 MiB"));
        }
        // section body: meta version, codec, raw length, CRC-32C, payload;
        // zstd only when smaller and within the ratio limit
        let z = crate::zst::compress(&p, level.zstd());
        let mut out = vec![1u8, 0];
        put_uv(&mut out, p.len() as u64);
        out.extend_from_slice(&crc32(&p).to_le_bytes());
        let zstd = z.len() < p.len() && ratio_ok(p.len() as u64, (out.len() + z.len()) as u64);
        out[1] = zstd as u8;
        out.extend_from_slice(if zstd { &z } else { &p });
        Ok(out)
    }

    /// Parses a META section body (FORMAT.md, "Metadata").
    pub fn decode(b: &[u8], budget: &Budget) -> Result<Metadata> {
        let e = Error::Corrupt("metadata");
        let mut r = Rd::new(b);
        let version = r.u8().ok_or(e)?;
        let codec = r.u8().ok_or(e)?;
        let n = r.uv().ok_or(e)?;
        let crc = r.u32le().ok_or(e)?;
        if version != 1 {
            return Err(Error::Format("unsupported metadata version"));
        }
        // the ratio limit, as for segments: at most 1024 field bytes per
        // section byte
        if n > META_MAX || !ratio_ok(n, b.len() as u64) {
            return bad("metadata size");
        }
        let n = usz(n).ok_or(e)?;
        // the fields, and the copies the parse makes of them
        let _c = budget.alloc(2 * n as u64)?;
        let p = match codec {
            0 if r.rest().len() == n => r.rest().to_vec(),
            1 => crate::zst::decompress(r.rest(), n)?,
            0 => return Err(e),
            _ => return Err(Error::Format("unknown metadata codec")),
        };
        if crc32(&p) != crc {
            return bad("metadata checksum");
        }
        let mut m = Metadata::default();
        let mut have = [false; N_TAGS];
        let mut r = Rd::new(&p);
        while !r.done() {
            let tag = r.uv().ok_or(e)?;
            let v = r.bytes().ok_or(e)?;
            if (1..N_TAGS as u64).contains(&tag) {
                if have[tag as usize] {
                    return bad("duplicate metadata tag");
                }
                have[tag as usize] = true;
            }
            let mut f = Rd::new(v);
            match tag {
                T_SHA256 => m.sha256 = v.try_into().map_err(|_| e)?,
                T_SIZE => m.size = f.uv().ok_or(e)?,
                T_DEMOINFO => {
                    if v.is_empty() || !demoinfo_ok(v) {
                        return Err(e);
                    }
                    m.demoinfo = v.to_vec()
                }
                T_TIMING => {
                    m.duration_ms = f.uv().ok_or(e)?;
                    m.frames = f.uv().ok_or(e)?;
                }
                T_EXT => {
                    let n = f.uz().ok_or(e)?;
                    // every entry takes at least 2 bytes
                    if n > v.len() / 2 {
                        return Err(e);
                    }
                    let mut keys = std::collections::HashSet::with_capacity(n);
                    for _ in 0..n {
                        let k = std::str::from_utf8(f.bytes().ok_or(e)?).map_err(|_| e)?;
                        if !keys.insert(k) {
                            return bad("duplicate extension key");
                        }
                        m.ext.push((k.to_string(), f.bytes().ok_or(e)?.to_vec()));
                    }
                }
                _ => continue,
            }
            // a known tag's value is consumed exactly
            if matches!(tag, T_SIZE | T_TIMING | T_EXT) && !f.done() {
                return Err(e);
            }
        }
        if !have[T_SHA256 as usize] || !have[T_SIZE as usize] || !have[T_TIMING as usize] {
            return bad("metadata identity");
        }
        Ok(m)
    }

    /// Heap bytes this metadata holds (what a reader charges to its
    /// budget for keeping it).
    pub fn heap_size(&self) -> u64 {
        let ext: usize = self.ext.iter().map(|(k, v)| k.len() + v.len() + 48).sum();
        (self.demoinfo.len() + ext) as u64
    }
}

// ---------------------------------------------------------------- index

/// The INDEX section: whole-demo size, CRC and duration, and the segments.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    pub size: u64,
    pub crc: u32,
    pub duration_ms: u64,
    pub segs: Vec<IndexSeg>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexSeg {
    /// file offset of the SEGMENT section
    pub off: u64,
    /// offset of its first byte in the demo
    pub raw_off: u64,
    pub raw_len: u64,
    /// demo time before its first frame (ms)
    pub t_start: u64,
}

impl Index {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_uv(&mut b, self.size);
        b.extend_from_slice(&self.crc.to_le_bytes());
        put_uv(&mut b, self.duration_ms);
        put_uv(&mut b, self.segs.len() as u64);
        for s in &self.segs {
            put_uv(&mut b, s.off);
            put_uv(&mut b, s.raw_len);
            put_uv(&mut b, s.t_start);
        }
        b
    }
    pub fn decode(b: &[u8]) -> Result<Index> {
        let e = Error::Corrupt("index");
        let mut r = Rd::new(b);
        let mut x = Index {
            size: r.uv().ok_or(e)?,
            crc: r.u32le().ok_or(e)?,
            duration_ms: r.uv().ok_or(e)?,
            ..Default::default()
        };
        let ns = r.uz().ok_or(e)?;
        // every entry takes at least 3 bytes
        if ns > b.len() / 3 {
            return Err(e);
        }
        let mut raw_off = 0u64;
        for _ in 0..ns {
            let off = r.uv().ok_or(e)?;
            let raw_len = r.uv().ok_or(e)?;
            let t_start = r.uv().ok_or(e)?;
            x.segs.push(IndexSeg {
                off,
                raw_off,
                raw_len,
                t_start,
            });
            raw_off = raw_off.checked_add(raw_len).ok_or(e)?;
        }
        if !r.done() || raw_off != x.size {
            return Err(e);
        }
        Ok(x)
    }
    /// The segment a seek to demo time `ms` starts at: the last one whose
    /// start time is before `ms` (so every frame at `ms` or later is
    /// decoded), or the first.
    pub fn seg_at_time(&self, ms: u64) -> usize {
        self.segs
            .partition_point(|s| s.t_start < ms)
            .saturating_sub(1)
    }
}

// ---------------------------------------------------------------- segment header

/// The header of a SEGMENT section body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegHdr {
    pub raw_off: u64,
    pub raw_len: u64,
    pub t_start: u64,
    pub crc: u32,
    pub coding: u8,
    /// offset of the coded data in the section body
    pub body_off: usize,
    /// (offset, length) of the demoinfo share (transform segments with the
    /// demoinfo flag)
    pub share: Option<(u64, u64)>,
}

pub fn seg_hdr_bytes(raw_off: u64, raw_len: u64, t_start: u64, crc: u32, coding: u8) -> Vec<u8> {
    let mut b = Vec::new();
    put_uv(&mut b, raw_off);
    put_uv(&mut b, raw_len);
    put_uv(&mut b, t_start);
    b.extend_from_slice(&crc.to_le_bytes());
    b.push(coding);
    b
}

/// Bytes of a SEGMENT body that hold every header field [`parse_seg_hdr`]
/// reads.
pub const SEG_HDR_MAX: usize = 3 * 10 + 4 + 1 + 3 * 10;

/// Parses a segment header from the first bytes of its section body
/// (`b` may be cut at [`SEG_HDR_MAX`]).
pub fn parse_seg_hdr(b: &[u8]) -> Result<SegHdr> {
    let e = Error::Corrupt("segment header");
    let mut r = Rd::new(b);
    let raw_off = r.uv().ok_or(e)?;
    let raw_len = r.uv().ok_or(e)?;
    let t_start = r.uv().ok_or(e)?;
    let crc = r.u32le().ok_or(e)?;
    let coding = r.u8().ok_or(e)?;
    let body_off = r.i;
    let mut share = None;
    match coding {
        C_TRANSFORM => {
            // the share fields of the transform header are structural
            // (FORMAT.md §9 rule 6): every reader checks them at open
            let flags = r.uv().ok_or(e)?;
            let off = r.uv().ok_or(e)?;
            let len = r.uv().ok_or(e)?;
            if flags & !crate::segment::SF_DINFO != 0 {
                return bad("segment flags");
            }
            if flags & crate::segment::SF_DINFO != 0 {
                if len == 0 {
                    return bad("demoinfo share");
                }
                share = Some((off, len));
            } else if off != 0 || len != 0 {
                return bad("demoinfo share");
            }
        }
        C_ZSTD | C_STORE => {}
        _ => return Err(Error::Format("unknown segment coding")),
    }
    if raw_len > MAX_SEG_RAW {
        return bad("segment too large");
    }
    Ok(SegHdr {
        raw_off,
        raw_len,
        t_start,
        crc,
        coding,
        body_off,
        share,
    })
}

// ---------------------------------------------------------------- layout rules

/// What a reader has learned about a file's sections, fed in file order.
/// Every reader (whole-file, sequential, seekable, metadata) applies these
/// rules through this type, so all of them accept and refuse the same
/// files (FORMAT.md, "Validation").
#[derive(Default)]
pub struct Layout {
    pub prefix_meta_off: u64,
    pub flags: u8,
    pub meta: Option<(u64, Metadata)>,
    pub index: Option<(u64, Index)>,
    /// SEGMENT sections: (section, header)
    pub segs: Vec<(Sec, SegHdr)>,
    pub trailer: Option<Trailer>,
    /// the TABLES section
    pub tables: Option<crate::coder::rans::FileTables>,
    raw: u64,
    dinfo_next: u64,
    /// the budget charge for everything above (metadata, index, one record
    /// per segment)
    held: Option<Charge>,
}

/// Bytes charged per recorded SEGMENT section.
const SEG_BOOK: u64 = std::mem::size_of::<(Sec, SegHdr)>() as u64;

impl Layout {
    pub fn new(prefix: &[u8], budget: &Budget) -> Result<Layout> {
        let (flags, meta_off) = check_prefix(prefix)?;
        Ok(Layout {
            prefix_meta_off: meta_off,
            flags,
            held: Some(Charge::none(budget)),
            ..Default::default()
        })
    }

    /// Charges `n` more bytes for what the layout keeps.
    fn hold(&mut self, n: u64) -> Result<()> {
        match &mut self.held {
            Some(c) => c.grow(n),
            None => Ok(()),
        }
    }

    /// The next section header. Checks kinds, order and uniqueness.
    pub fn section(&mut self, s: &Sec) -> Result<()> {
        if self.trailer.is_some() {
            return bad("section after the trailer");
        }
        match s.kind {
            K_META if self.meta.is_some() => bad("duplicate META"),
            K_INDEX if self.index.is_some() => bad("duplicate INDEX"),
            K_TABLES if self.tables.is_some() => bad("duplicate TABLES"),
            K_SEGMENT if self.index.is_some() => bad("segment after the index"),
            K_TABLES if !self.segs.is_empty() || self.index.is_some() => {
                bad("TABLES after a segment or the index")
            }
            K_META | K_INDEX | K_SEGMENT | K_TABLES | K_TRAILER => Ok(()),
            k if k & CRITICAL != 0 => Err(Error::Format("unknown critical section")),
            _ => Ok(()),
        }
    }

    /// A META section (after [`Layout::section`]).
    pub fn meta(&mut self, s: &Sec, m: Metadata) -> Result<()> {
        self.hold(m.heap_size() + std::mem::size_of::<Metadata>() as u64)?;
        self.meta = Some((s.off, m));
        Ok(())
    }

    /// A TABLES section (after [`Layout::section`]).
    pub fn tables(&mut self, t: crate::coder::rans::FileTables) -> Result<()> {
        self.hold(t.len())?;
        self.tables = Some(t);
        Ok(())
    }

    pub fn index(&mut self, s: &Sec, ix: Index) -> Result<()> {
        self.hold((ix.segs.len() * std::mem::size_of::<IndexSeg>()) as u64)?;
        self.index = Some((s.off, ix));
        Ok(())
    }

    /// A SEGMENT section with its parsed header.
    pub fn segment(&mut self, s: &Sec, h: SegHdr) -> Result<()> {
        if h.raw_off != self.raw {
            return bad("segment out of order");
        }
        if !seg_ratio_ok(h.raw_len, s.len) {
            return bad("segment size");
        }
        if let Some((off, len)) = h.share {
            let Some((_, m)) = &self.meta else {
                return bad("segment before its metadata");
            };
            if off != self.dinfo_next
                || off
                    .checked_add(len)
                    .is_none_or(|e| e > m.demoinfo.len() as u64)
            {
                return bad("demoinfo share");
            }
            self.dinfo_next = off + len;
        }
        self.raw = self
            .raw
            .checked_add(h.raw_len)
            .ok_or(Error::Corrupt("segment sizes"))?;
        self.hold(SEG_BOOK)?;
        self.segs.push((*s, h));
        Ok(())
    }

    /// The trailer section; `file_len` is the length of the file (the
    /// trailer must end it).
    pub fn trailer(&mut self, s: &Sec, t: Trailer, file_len: u64) -> Result<()> {
        if s.len != 24 || s.end() != file_len {
            return bad("trailer not at the end");
        }
        self.trailer = Some(t);
        Ok(())
    }

    /// The demoinfo bytes of segment `i`'s share.
    pub fn share(&self, h: &SegHdr) -> &[u8] {
        match (h.share, &self.meta) {
            (Some((off, len)), Some((_, m))) => &m.demoinfo[off as usize..(off + len) as usize],
            _ => &[],
        }
    }

    /// Cross-checks of a complete file, once every section has been seen.
    pub fn finish(&self) -> Result<()> {
        let Some(t) = self.trailer else {
            return bad("incomplete file (no trailer)");
        };
        let Some((moff, m)) = &self.meta else {
            return bad("no metadata");
        };
        let Some((ioff, ix)) = &self.index else {
            return bad("no index");
        };
        if t.meta_off != *moff || (self.prefix_meta_off != 0 && self.prefix_meta_off != *moff) {
            return bad("metadata offset");
        }
        if t.index_off != *ioff {
            return bad("index offset");
        }
        if ix.segs.len() != self.segs.len() {
            return bad("index does not match segments");
        }
        for (e, (s, h)) in ix.segs.iter().zip(&self.segs) {
            if e.off != s.off || e.raw_len != h.raw_len || e.t_start != h.t_start {
                return bad("index does not match segments");
            }
        }
        if ix.size != self.raw || m.size != ix.size {
            return bad("demo size");
        }
        if self.dinfo_next != m.demoinfo.len() as u64 {
            return bad("demoinfo share");
        }
        Ok(())
    }
}
