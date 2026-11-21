//! Decoding: the layout scan every random-access reader runs, the decode
//! of one SEGMENT, and whole-file decompression.

use crate::budget::{Budget, Charge, ReadOptions, zeroed};
use crate::coder::rans::FileTables;
use crate::error::{Error, Result, bad};
use crate::format::*;
use crate::segment::{self, frame_scan};
use crate::util::{Crc, crc32, par_map, usz};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

/// Random access to a file's bytes.
pub trait Src {
    fn len(&mut self) -> Result<u64>;
    /// Fills `buf` from file offset `off` (exactly).
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()>;
}

/// A file in memory.
pub struct SliceSrc<'a>(pub &'a [u8]);

impl Src for SliceSrc<'_> {
    fn len(&mut self) -> Result<u64> {
        Ok(self.0.len() as u64)
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        let s = usz(off)
            .and_then(|o| self.0.get(o..o.checked_add(buf.len())?))
            .ok_or(Error::Corrupt("truncated"))?;
        buf.copy_from_slice(s);
        Ok(())
    }
}

/// A file behind `Read + Seek`.
pub struct SeekSrc<R>(pub R);

impl<R: Read + Seek> Src for SeekSrc<R> {
    fn len(&mut self) -> Result<u64> {
        Ok(self.0.seek(SeekFrom::End(0))?)
    }
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        if self.0.seek(SeekFrom::Start(off))? != off {
            return Err(Error::Io(std::io::ErrorKind::InvalidData));
        }
        read_full(&mut self.0, buf)
    }
}

/// `read_exact` that reports a short read as a truncated file.
pub fn read_full<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> Result<()> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => return bad("truncated"),
            Ok(k) if k <= buf.len() - n => n += k,
            Ok(_) => return Err(Error::Io(std::io::ErrorKind::InvalidData)),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Reads `n` bytes at `off` into a buffer charged to `budget` (for as long
/// as the charge lives).
fn read_vec(src: &mut impl Src, off: u64, n: u64, budget: &Budget) -> Result<(Vec<u8>, Charge)> {
    let c = budget.alloc(n)?;
    let mut v = zeroed(usz(n).ok_or(Error::Limit("section size"))?)?;
    src.read_at(off, &mut v)?;
    Ok((v, c))
}

/// Section-body size limits of the sections readers hold in memory.
pub fn body_limit(kind: u8) -> u64 {
    match kind {
        K_META => META_MAX + 64,
        K_INDEX => DIR_MAX,
        K_TABLES => TABLES_MAX,
        K_TRAILER => 24,
        _ => u64::MAX,
    }
}

/// Reads and checks the whole section structure of a complete file:
/// every section header, the metadata, the index
/// and the header of every segment (FORMAT.md, "Validation"). Segment
/// payloads are not decoded.
pub fn scan_layout(src: &mut impl Src, budget: &Budget) -> Result<Layout> {
    let len = src.len()?;
    let mut p = [0u8; PREFIX_LEN];
    if len < PREFIX_LEN as u64 {
        let mut h = vec![0u8; len as usize];
        src.read_at(0, &mut h)?;
        check_prefix(&h)?;
        return bad("truncated prefix");
    }
    src.read_at(0, &mut p)?;
    let mut lay = Layout::new(&p, budget)?;
    let mut off = PREFIX_LEN as u64;
    while off < len {
        if len - off < SEC_HDR as u64 {
            break;
        }
        let mut h = [0u8; SEC_HDR];
        src.read_at(off, &mut h)?;
        let s = sec_hdr(&h, off);
        if s.len > len - off - SEC_HDR as u64 {
            // a truncated last section: the file is incomplete
            break;
        }
        lay.section(&s)?;
        if s.len > body_limit(s.kind) {
            return bad("section too large");
        }
        match s.kind {
            K_META => {
                let (b, _c) = read_vec(src, s.body_off(), s.len, budget)?;
                lay.meta(&s, Metadata::decode(&b, budget)?)?;
            }
            K_INDEX => {
                let (b, _c) = read_vec(src, s.body_off(), s.len, budget)?;
                lay.index(&s, Index::decode(&b)?)?;
            }
            K_TABLES => {
                let (b, _c) = read_vec(src, s.body_off(), s.len, budget)?;
                lay.tables(FileTables::new(b, budget)?)?;
            }
            K_SEGMENT => {
                let mut b = vec![0u8; s.len.min(SEG_HDR_MAX as u64) as usize];
                src.read_at(s.body_off(), &mut b)?;
                lay.segment(&s, parse_seg_hdr(&b)?)?;
            }
            K_TRAILER => {
                let mut b = [0u8; 24];
                if s.len != 24 {
                    return bad("trailer");
                }
                src.read_at(s.body_off(), &mut b)?;
                lay.trailer(&s, Trailer::parse_body(&b)?, len)?;
            }
            _ => {}
        }
        off = s.end();
    }
    lay.finish()?;
    Ok(lay)
}

/// Frames, frame time and whole-frame length of one decoded segment.
pub type SegTiming = (u64, u64, usize);

/// Decodes one SEGMENT section body into `dst` (its exact raw length) and
/// checks its CRC.
pub fn decode_segment(
    lay: &Layout,
    body: &[u8],
    h: &SegHdr,
    dst: &mut [u8],
    budget: &Budget,
) -> Result<SegTiming> {
    let tables = lay.tables.as_ref();
    decode_segment_share(lay.share(h), tables, body, h, dst, budget)
}

/// [`decode_segment`] with the demoinfo share and the TABLES section given
/// directly.
pub fn decode_segment_share(
    share: &[u8],
    tables: Option<&FileTables>,
    body: &[u8],
    h: &SegHdr,
    dst: &mut [u8],
    budget: &Budget,
) -> Result<SegTiming> {
    if dst.len() as u64 != h.raw_len {
        return bad("segment length");
    }
    let coded = body
        .get(h.body_off..)
        .ok_or(Error::Corrupt("segment header"))?;
    match h.coding {
        C_TRANSFORM => {
            let th = segment::parse_header(coded, dst.len())?;
            segment::decode(coded, &th, dst, share, tables, budget)?;
        }
        C_ZSTD => {
            let _c = budget.alloc(h.raw_len)?;
            dst.copy_from_slice(&crate::zst::decompress(coded, dst.len())?);
        }
        C_STORE if coded.len() == dst.len() => dst.copy_from_slice(coded),
        C_STORE => return bad("stored segment length"),
        _ => return Err(Error::Format("unknown segment coding")),
    }
    if crc32(dst) != h.crc {
        return bad("segment checksum");
    }
    Ok(frame_scan(dst))
}

/// Whole-demo checks over the segments' timings (all of them, in order).
pub struct DemoCheck {
    crc: Crc,
    /// None for a trusted decode (no identity check)
    sha: Option<Sha256>,
    size: u64,
    t: u64,
    frames: u64,
    /// the previous segment ended inside a frame
    open: bool,
}

impl DemoCheck {
    /// Checks for a decode under `budget` (trusted: no SHA-256).
    pub fn new(budget: &Budget) -> DemoCheck {
        DemoCheck {
            crc: Crc::default(),
            sha: (!budget.trusted()).then(Sha256::new),
            size: 0,
            t: 0,
            frames: 0,
            open: false,
        }
    }

    /// The next segment, decoded.
    pub fn segment(&mut self, h: &SegHdr, out: &[u8], tm: SegTiming) -> Result<()> {
        if self.open {
            return bad("segment ends inside a frame");
        }
        if h.t_start != self.t {
            return bad("segment start time");
        }
        self.crc.update(out);
        if let Some(s) = self.sha.as_mut() {
            s.update(out);
        }
        self.size += out.len() as u64;
        self.t += tm.1;
        self.frames += tm.0;
        self.open = tm.2 != out.len();
        Ok(())
    }
    /// After the last segment: the index and the metadata's identity.
    pub fn finish(self, lay: &Layout) -> Result<()> {
        let (Some((_, ix)), Some((_, m))) = (&lay.index, &lay.meta) else {
            return bad("incomplete file");
        };
        if self.crc.finish() != ix.crc || self.size != ix.size {
            return bad("checksum mismatch");
        }
        if self.t != ix.duration_ms || self.t != m.duration_ms || self.frames != m.frames {
            return bad("demo timing");
        }
        if self
            .sha
            .is_some_and(|s| <[u8; 32]>::from(s.finalize()) != m.sha256)
            || self.size != m.size
        {
            return bad("identity mismatch");
        }
        Ok(())
    }
}

/// Decompresses a whole file in memory.
pub fn decompress(b: &[u8], threads: usize, opts: &ReadOptions) -> Result<Vec<u8>> {
    let budget = Budget::reading(opts);
    budget.input(b.len() as u64);
    let lay = scan_layout(&mut SliceSrc(b), &budget)?;
    let size = lay.index.as_ref().map_or(0, |i| i.1.size);
    let _c = budget.alloc(size)?;
    budget.work(size)?;
    let mut out = zeroed(usz(size).ok_or(Error::Limit("demo size"))?)?;
    let nseg = lay.segs.len();
    let mut slices: Vec<std::sync::Mutex<&mut [u8]>> = Vec::with_capacity(nseg);
    let mut rest: &mut [u8] = &mut out;
    for (_, h) in &lay.segs {
        let (a, r) = rest.split_at_mut(h.raw_len as usize);
        slices.push(std::sync::Mutex::new(a));
        rest = r;
    }
    let res = par_map(threads.max(1), nseg, |i| {
        let (s, h) = &lay.segs[i];
        let body = &b[s.body_off() as usize..s.end() as usize];
        let mut dst = slices[i].lock().map_err(|_| Error::Internal("lock"))?;
        decode_segment(&lay, body, h, &mut dst, &budget)
    });
    drop(slices);
    let mut chk = DemoCheck::new(&budget);
    let mut at = 0usize;
    for (r, (_, h)) in res.into_iter().zip(&lay.segs) {
        let tm = r?;
        let n = h.raw_len as usize;
        chk.segment(h, &out[at..at + n], tm)?;
        at += n;
    }
    chk.finish(&lay)?;
    Ok(out)
}
