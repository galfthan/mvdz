//! One-shot compression: walk, code, assemble, and check that the file
//! decodes to the input (falling back per segment to zstd or stored bytes).

use crate::budget::{Budget, Limits};
use crate::coder::BE_FILE;
use crate::coder::rans::Shared;
use crate::decode::{SliceSrc, decode_segment, scan_layout};
use crate::error::{Error, Result};
use crate::format::*;
use crate::proto::Proto;
use crate::segment::{self, DEFAULT_SEGMENT, Level, StreamStat, Walked, cut_points, frame_scan};
use crate::util::{crc32, par_map};

/// Compression settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub level: Level,
    /// Independent, seekable segments of about this many raw bytes
    /// (default 256 KiB). 0: one segment for the whole demo; the streaming
    /// writer takes 0 as the default instead.
    pub segment: usize,
    /// Worker threads of one-shot compression, one segment each. The
    /// streaming writer codes on the calling thread.
    pub threads: usize,
    /// Decode the result and compare (falling back per segment on a
    /// mismatch). On by default; off only for experiments.
    pub verify: bool,
    /// Extension metadata key/values (not part of the demo).
    pub ext: Vec<(String, Vec<u8>)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            level: Level::Normal,
            segment: DEFAULT_SEGMENT,
            threads: 1,
            verify: true,
            ext: Vec::new(),
        }
    }
}

/// What the encoder did.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub segments: usize,
    /// Segments stored with zstd or raw instead of the transform.
    pub fallback_segments: usize,
    /// Input bytes the transform stored raw (escapes and unframeable tail).
    pub escaped: usize,
    pub streams: Vec<StreamStat>,
}

/// Checks what can be checked of a configuration before any work: the
/// extension metadata (FORMAT.md §3, tag 10).
pub fn check_config(cfg: &Config) -> Result<()> {
    check_ext(&cfg.ext)
}

/// One segment as the encoder sees it.
pub struct Planned {
    pub walked: Option<Walked>,
    pub t_start: u64,
}

/// Walks every segment in order (the protocol state carries over). KTX
/// demoinfo is shared through the metadata while the store stays within
/// its limit.
pub fn plan(
    src: &[u8],
    cuts: &[usize],
    proto: &mut Proto,
    t: &mut u64,
    dinfo_total: &mut usize,
) -> Vec<Planned> {
    let mut out = Vec::with_capacity(cuts.len());
    for w in cuts.windows(2) {
        let s = &src[w[0]..w[1]];
        let wk = walk_one(s, proto, dinfo_total);
        out.push(Planned {
            walked: wk,
            t_start: *t,
        });
        *t += frame_scan(s).1;
    }
    out
}

/// Walks one segment; updates the protocol state and the share total.
pub fn walk_one(s: &[u8], proto: &mut Proto, dinfo_total: &mut usize) -> Option<Walked> {
    let (wk, pend) = match segment::walk(s, *proto, true)? {
        (wk, pend) if *dinfo_total + wk.dinfo().len() <= DEMOINFO_MAX => (wk, pend),
        _ => segment::walk(s, *proto, false)?,
    };
    *proto = pend;
    *dinfo_total += wk.dinfo().len();
    Some(wk)
}

/// The SEGMENT section body of a segment stored without the transform:
/// zstd when the ratio limit allows it and it is smaller, else stored.
pub fn raw_body(s: &[u8], level: Level, raw_off: u64, t_start: u64) -> Vec<u8> {
    let crc = crc32(s);
    let z = crate::zst::compress(s, level.zstd());
    let mut b = seg_hdr_bytes(raw_off, s.len() as u64, t_start, crc, C_ZSTD);
    if z.len() < s.len() && seg_ratio_ok(s.len() as u64, (b.len() + z.len()) as u64) {
        b.extend_from_slice(&z);
        return b;
    }
    let mut b = seg_hdr_bytes(raw_off, s.len() as u64, t_start, crc, C_STORE);
    b.extend_from_slice(s);
    b
}

/// The SEGMENT section body of a walked segment, or None if it breaks the
/// ratio limit; `shared`: the file's shared models (empty: none).
#[allow(clippy::too_many_arguments)]
pub fn transform_body(
    s: &[u8],
    wk: &Walked,
    level: Level,
    raw_off: u64,
    t_start: u64,
    dinfo_off: u64,
    st: &mut Vec<StreamStat>,
    shared: &[Option<Shared>],
) -> Option<Vec<u8>> {
    let mut b = seg_hdr_bytes(raw_off, s.len() as u64, t_start, crc32(s), C_TRANSFORM);
    b.extend_from_slice(&segment::encode(s, wk, level, dinfo_off, st, shared));
    seg_ratio_ok(s.len() as u64, b.len() as u64).then_some(b)
}

/// Compresses a demo.
pub fn compress(src: &[u8], cfg: &Config) -> Result<(Vec<u8>, Info)> {
    compress_inner(src, cfg, false)
}

/// Compresses a demo with every segment stored without the transform
/// (zstd or raw): the layout the encoder falls back to. For test vectors.
#[doc(hidden)]
pub fn compress_untransformed(src: &[u8], cfg: &Config) -> Result<(Vec<u8>, Info)> {
    compress_inner(src, cfg, true)
}

fn compress_inner(src: &[u8], cfg: &Config, force_raw: bool) -> Result<(Vec<u8>, Info)> {
    check_config(cfg)?;
    let threads = cfg.threads.max(1);
    let cuts = cut_points(src, cfg.segment);
    let nseg = cuts.len() - 1;
    if nseg > MAX_SEGMENTS {
        return Err(Error::Invalid(
            "too many segments: use a larger segment size",
        ));
    }
    if cuts.windows(2).any(|w| (w[1] - w[0]) as u64 > MAX_SEG_RAW) {
        return Err(Error::Invalid(
            "segment over 4 GiB: use a smaller segment size",
        ));
    }
    let md0 = Metadata::from_scan(crate::meta::scan(src), cfg.ext.clone());
    let (mut proto, mut t, mut dt) = (Proto::default(), 0u64, 0usize);
    let plans = plan(src, &cuts, &mut proto, &mut t, &mut dt);
    // a segmented file at the normal level: one model per rANS stream from
    // its symbols in every segment, in the TABLES section (FORMAT.md §12)
    let shared = if cfg.level == Level::Normal && nseg > 1 && !force_raw {
        let segs: Vec<(&[u8], &Walked)> = plans
            .iter()
            .enumerate()
            .filter_map(|(i, p)| Some((&src[cuts[i]..cuts[i + 1]], p.walked.as_ref()?)))
            .collect();
        segment::train(&segs, threads)
    } else {
        Vec::new()
    };
    // segments the transform could not code: raw from the start
    let mut raw: Vec<bool> = plans
        .iter()
        .map(|p| force_raw || p.walked.is_none())
        .collect();
    for _round in 0..2 {
        let (out, info) = assemble(src, &cuts, &plans, &raw, &md0, cfg.level, &shared, threads)?;
        if !cfg.verify {
            return Ok((out, info));
        }
        let bad = failing_segments(&out, src, &cuts, threads);
        match bad {
            Some(b) if b.iter().all(|&x| !x) => return Ok((out, info)),
            Some(b) => {
                for (r, x) in raw.iter_mut().zip(b) {
                    *r |= x;
                }
            }
            // the container itself does not decode: nothing to fall back to
            None => break,
        }
    }
    Err(Error::Internal(
        "the encoder's output does not decode to its input",
    ))
}

/// Builds the file from the plans; `raw[i]` stores segment i without the
/// transform.
#[allow(clippy::too_many_arguments)]
fn assemble(
    src: &[u8],
    cuts: &[usize],
    plans: &[Planned],
    raw: &[bool],
    md0: &Metadata,
    level: Level,
    shared: &[Option<Shared>],
    threads: usize,
) -> Result<(Vec<u8>, Info)> {
    let nseg = plans.len();
    let mut md = md0.clone();
    let mut doff = Vec::with_capacity(nseg);
    for (p, &r) in plans.iter().zip(raw) {
        doff.push(md.demoinfo.len() as u64);
        if let (Some(wk), false) = (&p.walked, r) {
            md.demoinfo.extend_from_slice(wk.dinfo());
        }
    }
    let bodies: Vec<(Vec<u8>, bool, Vec<StreamStat>)> = par_map(threads, nseg, |i| {
        let s = &src[cuts[i]..cuts[i + 1]];
        let mut st = Vec::new();
        if let (Some(wk), false) = (&plans[i].walked, raw[i]) {
            if let Some(b) = transform_body(
                s,
                wk,
                level,
                cuts[i] as u64,
                plans[i].t_start,
                doff[i],
                &mut st,
                shared,
            ) {
                return (b, false, st);
            }
        }
        (
            raw_body(s, level, cuts[i] as u64, plans[i].t_start),
            true,
            Vec::new(),
        )
    });
    // a segment that fell back to raw no longer shares its demoinfo
    let mut md_final = md0.clone();
    for (i, p) in plans.iter().enumerate() {
        if let (Some(wk), false) = (&p.walked, bodies[i].1) {
            md_final.demoinfo.extend_from_slice(wk.dinfo());
        }
    }
    if md_final.demoinfo != md.demoinfo {
        let raw2: Vec<bool> = bodies.iter().map(|b| b.1).collect();
        return assemble(src, cuts, plans, &raw2, md0, level, shared, threads);
    }
    let mut out = prefix(0, PREFIX_LEN as u64);
    section(&mut out, K_META, &md.encode_for(level)?);
    // TABLES: the shared model of every stream some segment codes with it
    let mut used = vec![false; shared.len()];
    for st in bodies.iter().flat_map(|b| &b.2) {
        if st.be == BE_FILE {
            used[st.id] = true;
        }
    }
    let ids: Vec<usize> = (0..used.len()).filter(|&sid| used[sid]).collect();
    if !ids.is_empty() {
        section(&mut out, K_TABLES, &segment::tables_body(shared, &ids));
    }
    let mut index = Index {
        size: src.len() as u64,
        crc: crc32(src),
        duration_ms: md.duration_ms,
        segs: Vec::new(),
    };
    let mut info = Info {
        segments: nseg,
        ..Default::default()
    };
    for (i, (b, fb, st)) in bodies.into_iter().enumerate() {
        index.segs.push(IndexSeg {
            off: out.len() as u64,
            raw_off: cuts[i] as u64,
            raw_len: (cuts[i + 1] - cuts[i]) as u64,
            t_start: plans[i].t_start,
        });
        section(&mut out, K_SEGMENT, &b);
        info.fallback_segments += fb as usize;
        info.escaped += plans[i]
            .walked
            .as_ref()
            .filter(|_| !fb)
            .map_or(0, |w| w.escaped());
        info.streams.extend(st);
    }
    let index_off = out.len() as u64;
    section(&mut out, K_INDEX, &index.encode());
    let t = Trailer {
        index_off,
        meta_off: PREFIX_LEN as u64,
        adir_off: 0,
        body_end: out.len() as u64,
    };
    out.extend_from_slice(&t.bytes());
    Ok((out, info))
}

/// Decodes `file` and compares it with `src`: None if the container does
/// not parse, else per segment whether it fails. Every segment passing
/// means the whole file decodes (the whole-file checks are run too).
pub fn failing_segments(
    file: &[u8],
    src: &[u8],
    cuts: &[usize],
    threads: usize,
) -> Option<Vec<bool>> {
    if let Ok(back) = crate::decode::decompress(file, threads, &Limits::unlimited().into()) {
        if back == src {
            return Some(vec![false; cuts.len() - 1]);
        }
    }
    let budget = Budget::unlimited();
    let lay = scan_layout(&mut SliceSrc(file), &budget).ok()?;
    if lay.segs.len() != cuts.len() - 1 {
        return None;
    }
    let bad = par_map(threads, lay.segs.len(), |i| {
        let (s, h) = &lay.segs[i];
        let want = &src[cuts[i]..cuts[i + 1]];
        let mut dst = vec![0u8; want.len()];
        let body = &file[s.body_off() as usize..s.end() as usize];
        decode_segment(&lay, body, h, &mut dst, &budget).is_err() || dst != want
    });
    // a file whose segments all decode but which still fails is not
    // fixable by falling back
    if bad.iter().all(|&x| !x) {
        return None;
    }
    Some(bad)
}
