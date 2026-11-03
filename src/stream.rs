//! Streaming compression and decompression.
//!
//! [`StreamEncoder`] compresses a demo that is still growing: it cuts a
//! segment whenever enough whole frames have arrived, checks it, and
//! writes it at once. The metadata (identity, census, demoinfo) is known
//! only at the end, so it follows the segments and the trailer points at
//! it. A segment that shares KTX demoinfo, and every segment after it, is
//! held back (coded) and written after the metadata, so a sequential
//! reader always has the demoinfo before a segment that needs it.
//!
//! [`Reader`] decodes sequentially from any `io::Read` (pipe, socket, VFS
//! callback), or with `io::Seek` by segment and demo time.

use crate::budget::{Budget, Charge, ReadOptions, zeroed};
use crate::decode::{
    DemoCheck, SeekSrc, body_limit, decode_segment, decode_segment_share, read_full, scan_layout,
};
use crate::encode::{Config, check_config, raw_body, transform_body, walk_one};
use crate::error::{Error, Result, bad};
use crate::format::*;
use crate::meta::Scanner;
use crate::proto::{Proto, frame_len};
use crate::segment::frame_scan;
use crate::util::{Crc, usz};
use std::io::{self, Read, Seek, SeekFrom, Write};

// ---------------------------------------------------------------- encoder

/// Default segment size of the streaming encoder.
pub const STREAM_SEGMENT: usize = 1 << 20;

pub struct StreamEncoder<W: Write> {
    w: W,
    cfg: Config,
    pos: u64,
    pending: Vec<u8>,
    /// bytes of `pending` that are whole frames already scanned
    framed: usize,
    proto: Proto,
    scan: Scanner,
    dinfo: Vec<u8>,
    /// the start of the demo passed the input check
    started: bool,
    /// a segment shared demoinfo: hold every later segment until the end
    hold: bool,
    held: Vec<(Vec<u8>, IndexSeg)>,
    index: Index,
    t: u64,
    raw: u64,
    crc: Crc,
    /// the first error; every later call returns it
    failed: Option<Error>,
}

impl<W: Write> StreamEncoder<W> {
    /// `cfg.segment` is the segment size (0 = 1 MiB). The configuration
    /// (extension metadata) is checked before anything is written.
    pub fn new(mut w: W, cfg: &Config) -> Result<Self> {
        check_config(cfg)?;
        let mut cfg = cfg.clone();
        if cfg.segment == 0 {
            cfg.segment = STREAM_SEGMENT;
        }
        let p = prefix(F_STREAMED, 0);
        w.write_all(&p)?;
        Ok(StreamEncoder {
            w,
            cfg,
            pos: p.len() as u64,
            pending: Vec::new(),
            framed: 0,
            proto: Proto::default(),
            scan: Scanner::new(),
            dinfo: Vec::new(),
            started: false,
            hold: false,
            held: Vec::new(),
            index: Index::default(),
            t: 0,
            raw: 0,
            crc: Crc::default(),
            failed: None,
        })
    }

    /// As `new`, without the input check: any bytes are stored (they
    /// round-trip). For test vectors of non-demo input.
    #[doc(hidden)]
    pub fn new_unchecked(w: W, cfg: &Config) -> Result<Self> {
        let mut e = Self::new(w, cfg)?;
        e.started = true;
        Ok(e)
    }

    fn check(&self) -> Result<()> {
        match self.failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Runs `f`; its first error poisons the encoder.
    fn guard<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.check()?;
        let r = f(self);
        if let Err(e) = r {
            self.failed = Some(e);
        }
        r
    }

    fn put(&mut self, kind: u8, body: &[u8]) -> Result<u64> {
        let off = self.pos;
        let mut b = Vec::with_capacity(body.len() + SEC_HDR);
        section(&mut b, kind, body);
        self.w.write_all(&b)?;
        self.pos += b.len() as u64;
        Ok(off)
    }

    /// Appends demo bytes; writes every segment that has filled.
    pub fn write_demo(&mut self, data: &[u8]) -> Result<()> {
        self.guard(|s| {
            s.pending.extend_from_slice(data);
            // the input check of the one-shot encoder, as soon as the start
            // of the demo decides it; nothing is cut before
            if !s.started {
                if crate::api::is_gzip(&s.pending) {
                    crate::api::check_demo(&s.pending)?;
                }
                match crate::meta::mvd_start(&s.pending) {
                    None => return Ok(()),
                    Some(false) => crate::api::check_demo(&s.pending)?,
                    Some(true) => s.started = true,
                }
            }
            loop {
                while let Some(n) = frame_len(&s.pending, s.framed) {
                    s.framed += n;
                    if s.framed >= s.cfg.segment {
                        break;
                    }
                }
                if s.framed < s.cfg.segment {
                    return Ok(());
                }
                let seg: Vec<u8> = s.pending.drain(..s.framed).collect();
                s.framed = 0;
                s.segment(&seg)?;
            }
        })
    }

    fn segment(&mut self, seg: &[u8]) -> Result<()> {
        if seg.len() as u64 > MAX_SEG_RAW {
            return Err(Error::Invalid("segment over 4 GiB"));
        }
        if self.index.segs.len() + self.held.len() >= MAX_SEGMENTS {
            return Err(Error::Invalid(
                "too many segments: use a larger segment size",
            ));
        }
        self.scan.feed(seg);
        self.crc.update(seg);
        let t_start = self.t;
        self.t += frame_scan(seg).1;
        let mut dtotal = self.dinfo.len();
        let wk = walk_one(seg, &mut self.proto, &mut dtotal);
        let mut st = Vec::new();
        let doff = self.dinfo.len() as u64;
        let mut share: &[u8] = &[];
        let mut body = None;
        if let Some(wk) = &wk {
            if let Some(b) = transform_body(
                seg,
                wk,
                self.cfg.level,
                self.raw,
                t_start,
                doff,
                self.cfg.threads.max(1),
                &mut st,
            ) {
                share = wk.dinfo();
                body = Some(b);
            }
        }
        // check the segment decodes to its input; fall back if it does not
        if let (Some(b), true) = (&body, self.cfg.verify) {
            let ok = parse_seg_hdr(b).ok().is_some_and(|h| {
                let mut dst = vec![0u8; seg.len()];
                decode_segment_share(share, b, &h, &mut dst, 1, &Budget::unlimited()).is_ok()
                    && dst == seg
            });
            if !ok {
                body = None;
            }
        }
        let body = match body {
            Some(b) => {
                self.dinfo.extend_from_slice(share);
                b
            }
            None => {
                share = &[];
                raw_body(seg, self.raw, t_start)
            }
        };
        let e = IndexSeg {
            off: 0,
            raw_off: self.raw,
            raw_len: seg.len() as u64,
            t_start,
        };
        self.raw += seg.len() as u64;
        self.hold |= !share.is_empty();
        if self.hold {
            self.held.push((body, e));
        } else {
            let off = self.put(K_SEGMENT, &body)?;
            self.index.segs.push(IndexSeg { off, ..e });
        }
        Ok(())
    }

    /// Sets an extension metadata key (FORMAT.md §3, tag 10), replacing
    /// its value if the key is set already. The metadata is written by
    /// `finish`, so this may be called at any time before it. A key the
    /// metadata cannot take (all keys and values over 1 MiB) is an error
    /// that leaves the encoder as it was; after an earlier error it returns
    /// that error.
    pub fn set_ext(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.check()?;
        let mut ext = self.cfg.ext.clone();
        match ext.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = value.to_vec(),
            None => ext.push((key.to_string(), value.to_vec())),
        }
        check_ext(&ext)?;
        self.cfg.ext = ext;
        Ok(())
    }

    /// Ends the demo: writes the remaining segments, the metadata, the
    /// index and the trailer. Returns the writer. After an earlier error
    /// it returns that error and writes nothing more.
    pub fn finish(mut self) -> Result<W> {
        self.guard(|s| {
            if !s.started {
                crate::api::check_demo(&s.pending)?;
            }
            if !s.pending.is_empty() {
                let seg = std::mem::take(&mut s.pending);
                s.segment(&seg)?;
            }
            let scan = std::mem::take(&mut s.scan).finish();
            let mut md = Metadata::from_scan(scan, std::mem::take(&mut s.cfg.ext));
            md.demoinfo = std::mem::take(&mut s.dinfo);
            let meta_off = s.put(K_META, &md.encode()?)?;
            for (b, e) in std::mem::take(&mut s.held) {
                let off = s.put(K_SEGMENT, &b)?;
                s.index.segs.push(IndexSeg { off, ..e });
            }
            s.index.size = md.size;
            s.index.crc = s.crc.finish();
            s.index.duration_ms = md.duration_ms;
            let ib = s.index.encode();
            let index_off = s.put(K_INDEX, &ib)?;
            let t = Trailer {
                index_off,
                meta_off,
                adir_off: 0,
                body_end: s.pos,
            };
            s.w.write_all(&t.bytes())?;
            s.w.flush()?;
            Ok(())
        })?;
        Ok(self.w)
    }

    /// Stops without finishing the file and returns the writer. What was
    /// written is a valid prefix: its complete segments decode
    /// sequentially, but the file has no trailer (it is incomplete).
    pub fn abort(self) -> W {
        self.w
    }
}

// ---------------------------------------------------------------- reader

/// The byte source of a [`Reader`].
pub trait Source: Read {
    /// Positions the source at file offset `off` (random access only).
    fn seek_to(&mut self, off: u64) -> Result<()>;
}

/// A source read strictly in order.
pub struct Seq<R>(pub R);

impl<R: Read> Read for Seq<R> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}

impl<R: Read> Source for Seq<R> {
    fn seek_to(&mut self, _: u64) -> Result<()> {
        Err(Error::Invalid("not a seekable reader"))
    }
}

/// A source with random access.
pub struct Seekable<R>(pub R);

impl<R: Read> Read for Seekable<R> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}

impl<R: Read + Seek> Source for Seekable<R> {
    fn seek_to(&mut self, off: u64) -> Result<()> {
        if self.0.seek(SeekFrom::Start(off))? != off {
            return Err(Error::Io(io::ErrorKind::InvalidData));
        }
        Ok(())
    }
}

enum Mode {
    /// sections in file order, checked as they arrive
    Seq { pos: u64 },
    /// a complete file whose layout was checked when it was opened
    Random { next: usize },
}

/// Sequential (and, over `Seek`, random-access) decoder. Yields the
/// original demo bytes through `io::Read`. The first error is sticky.
pub struct Reader<S: Source> {
    r: S,
    lay: Layout,
    mode: Mode,
    cur: Vec<u8>,
    _cur_charge: Option<Charge>,
    at: usize,
    done: bool,
    failed: Option<Error>,
    threads: usize,
    budget: Budget,
    /// whole-demo checks while reading from the first segment on
    check: Option<DemoCheck>,
}

impl<R: Read> Reader<Seq<R>> {
    /// Sequential decoding from any reader.
    pub fn new(r: R, threads: usize, opts: &ReadOptions) -> Result<Self> {
        let mut r = Seq(r);
        let mut p = [0u8; PREFIX_LEN];
        let mut n = 0;
        while n < PREFIX_LEN {
            match r.read(&mut p[n..]) {
                Ok(0) => break,
                Ok(k) => n += k.min(PREFIX_LEN - n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        let budget = Budget::reading(opts);
        let lay = Layout::new(&p[..n], &budget)?;
        budget.input(PREFIX_LEN as u64);
        let check = Some(DemoCheck::new(&budget));
        Ok(Reader {
            r,
            lay,
            mode: Mode::Seq {
                pos: PREFIX_LEN as u64,
            },
            cur: Vec::new(),
            _cur_charge: None,
            at: 0,
            done: false,
            failed: None,
            threads: threads.max(1),
            budget,
            check,
        })
    }
}

impl<R: Read + Seek> Reader<Seekable<R>> {
    /// Opens a complete file for random access: checks its whole section
    /// structure (the same rules as every other reader) and positions at
    /// the first segment.
    pub fn open(r: R, threads: usize, opts: &ReadOptions) -> Result<Self> {
        let budget = Budget::reading(opts);
        let mut src = SeekSrc(r);
        let len = crate::decode::Src::len(&mut src)?;
        budget.input(len);
        let lay = scan_layout(&mut src, &budget)?;
        let check = Some(DemoCheck::new(&budget));
        Ok(Reader {
            r: Seekable(src.0),
            lay,
            mode: Mode::Random { next: 0 },
            cur: Vec::new(),
            _cur_charge: None,
            at: 0,
            done: false,
            failed: None,
            threads: threads.max(1),
            budget,
            check,
        })
    }

    /// Positions the reader at the start of segment `i`; returns its raw
    /// byte offset.
    pub fn seek_segment(&mut self, i: usize) -> Result<u64> {
        self.check_failed()?;
        let Some((_, h)) = self.lay.segs.get(i) else {
            return Err(Error::Invalid("segment out of range"));
        };
        let raw_off = h.raw_off;
        self.mode = Mode::Random { next: i };
        self.cur.clear();
        self._cur_charge = None;
        self.at = 0;
        self.done = false;
        // whole-demo checks apply when reading from the start
        self.check = (i == 0).then(|| DemoCheck::new(&self.budget));
        Ok(raw_off)
    }

    /// Positions the reader at the segment that holds demo time `ms`;
    /// returns (segment, its start time in ms, its raw byte offset).
    pub fn seek_time(&mut self, ms: u64) -> Result<(usize, u64, u64)> {
        self.check_failed()?;
        let ix = self.index().ok_or(Error::Invalid("no index"))?;
        if ix.segs.is_empty() {
            return Err(Error::Invalid("no segments"));
        }
        let i = ix.seg_at_time(ms);
        let (t, o) = (ix.segs[i].t_start, ix.segs[i].raw_off);
        self.seek_segment(i)?;
        Ok((i, t, o))
    }
}

impl<S: Source> Reader<S> {
    /// Metadata, once its section has been read (at the start of a file
    /// written in one go, near the end of a streamed one; always for a
    /// random-access reader).
    pub fn metadata(&self) -> Option<&Metadata> {
        self.lay.meta.as_ref().map(|m| &m.1)
    }

    /// The index (random-access readers, or once read).
    pub fn index(&self) -> Option<&Index> {
        self.lay.index.as_ref().map(|i| &i.1)
    }

    /// Number of segments (seek points); 0 for a sequential reader.
    pub fn segments(&self) -> usize {
        match self.mode {
            Mode::Random { .. } => self.lay.segs.len(),
            Mode::Seq { .. } => 0,
        }
    }

    fn check_failed(&self) -> Result<()> {
        match self.failed {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// A section body, charged to the budget while the charge lives.
    fn read_body(&mut self, s: &Sec) -> Result<(Vec<u8>, Charge)> {
        let c = self.budget.alloc(s.len)?;
        // the data arrives before the buffer grows: a lying length on a
        // short stream allocates nothing
        let mut v = Vec::new();
        (&mut self.r).take(s.len).read_to_end(&mut v)?;
        if v.len() as u64 != s.len {
            return bad("truncated");
        }
        self.budget.input(s.len);
        Ok((v, c))
    }

    fn deliver(&mut self, h: &SegHdr, body: &[u8]) -> Result<()> {
        let n = usz(h.raw_len).ok_or(Error::Limit("segment size"))?;
        self.cur = Vec::new();
        self._cur_charge = None;
        let charge = self.budget.alloc(h.raw_len)?;
        self.budget.work(h.raw_len)?;
        let mut out = zeroed(n)?;
        let tm = decode_segment(&self.lay, body, h, &mut out, self.threads, &self.budget)?;
        if let Some(c) = self.check.as_mut() {
            c.segment(h, &out, tm)?;
        }
        self.cur = out;
        self._cur_charge = Some(charge);
        self.at = 0;
        Ok(())
    }

    /// Reads sections until a segment's bytes are available or the file
    /// ends; returns the offset of the next section.
    fn advance_seq(&mut self, mut pos: u64) -> Result<u64> {
        while !self.done && self.at == self.cur.len() {
            let mut h = [0u8; SEC_HDR];
            let mut got = 0;
            while got < SEC_HDR {
                match self.r.read(&mut h[got..]) {
                    Ok(0) => break,
                    Ok(k) => got += k.min(SEC_HDR - got),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if got < SEC_HDR {
                // the stream ends without its trailer: every complete
                // segment has been delivered
                return bad("incomplete file (no trailer)");
            }
            let s = sec_hdr(&h, pos);
            self.lay.section(&s)?;
            if s.len > body_limit(s.kind) {
                return bad("section too large");
            }
            match s.kind {
                K_META => {
                    let (b, _c) = self.read_body(&s)?;
                    let m = Metadata::decode(&b, &self.budget)?;
                    self.lay.meta(&s, m)?;
                }
                K_INDEX => {
                    let (b, _c) = self.read_body(&s)?;
                    self.lay.index(&s, Index::decode(&b)?)?;
                }
                K_ADIR => {
                    let (b, _c) = self.read_body(&s)?;
                    self.lay.adir(&s, parse_adir(&b)?)?;
                }
                K_SEGMENT => {
                    let (b, _c) = self.read_body(&s)?;
                    let sh = parse_seg_hdr(&b[..b.len().min(SEG_HDR_MAX)])?;
                    self.lay.segment(&s, sh)?;
                    self.deliver(&sh, &b)?;
                }
                K_TRAILER => {
                    let (b, _c) = self.read_body(&s)?;
                    let t = Trailer::parse_body(&b)?;
                    // the trailer ends the file
                    let mut one = [0u8; 1];
                    loop {
                        match self.r.read(&mut one) {
                            Ok(0) => break,
                            Ok(_) => return bad("trailer not at the end"),
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) => return Err(e.into()),
                        }
                    }
                    self.lay.trailer(&s, t, s.end())?;
                    self.lay.finish()?;
                    if let Some(c) = self.check.take() {
                        c.finish(&self.lay)?;
                    }
                    self.done = true;
                }
                _ => {
                    // attachments and unknown sections are skipped unread
                    let n = io::copy(&mut (&mut self.r).take(s.len), &mut io::sink())?;
                    if n != s.len {
                        return bad("truncated");
                    }
                }
            }
            pos = s.end();
        }
        Ok(pos)
    }

    fn advance_random(&mut self, next: usize) -> Result<usize> {
        if next >= self.lay.segs.len() {
            if let Some(c) = self.check.take() {
                c.finish(&self.lay)?;
            }
            self.done = true;
            return Ok(next);
        }
        let (s, h) = self.lay.segs[next];
        let _c = self.budget.alloc(s.len)?;
        let mut b = zeroed(usz(s.len).ok_or(Error::Limit("section size"))?)?;
        self.r.seek_to(s.body_off())?;
        read_full(&mut self.r, &mut b)?;
        self.deliver(&h, &b)?;
        Ok(next + 1)
    }

    /// Reads demo bytes; returns 0 at the end. The first error is returned
    /// again by every later call.
    pub fn read_demo(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.check_failed()?;
        while self.at == self.cur.len() && !self.done {
            let r = match self.mode {
                Mode::Seq { pos } => self.advance_seq(pos).map(|p| Mode::Seq { pos: p }),
                Mode::Random { next } => {
                    self.advance_random(next).map(|n| Mode::Random { next: n })
                }
            };
            match r {
                Ok(m) => self.mode = m,
                Err(e) => {
                    self.failed = Some(e);
                    self.cur = Vec::new();
                    self._cur_charge = None;
                    self.at = 0;
                    return Err(e);
                }
            }
        }
        let n = buf.len().min(self.cur.len() - self.at);
        buf[..n].copy_from_slice(&self.cur[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl<S: Source> Read for Reader<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(self.read_demo(buf)?)
    }
}
