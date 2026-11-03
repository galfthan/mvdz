//! The library interface.

use crate::budget::{Budget, Limits, ReadOptions};
use crate::decode::{SeekSrc, SliceSrc, scan_layout};
use crate::error::{Error, Result};
use crate::format::*;
use crate::stream::{Reader, Seq, StreamEncoder};
use std::io::{Read, Seek, Write};

pub use crate::encode::{Config, Info};
pub use crate::format::{AttachInfo, Attachment, Index, Metadata};

/// Compresses a demo: a raw `.mvd`, or a gzip-compressed one (`.mvd.gz`),
/// which is decompressed first. The file holds the raw demo: its identity
/// is the raw demo's SHA-256 and decompressing gives the raw `.mvd` back.
/// Input that is not an MVD demo is `Error::NotMvd` (see `check_demo`).
pub fn compress(src: &[u8], cfg: &Config) -> Result<Vec<u8>> {
    compress_info(src, cfg).map(|r| r.0)
}

/// Compresses a demo (as `compress`) and reports what the encoder did.
pub fn compress_info(src: &[u8], cfg: &Config) -> Result<(Vec<u8>, Info)> {
    let raw;
    let src = if is_gzip(src) {
        raw = gunzip(src, Limits::default().memory)?;
        &raw[..]
    } else {
        src
    };
    check_demo(src)?;
    crate::encode::compress(src, cfg)
}

/// Checks that `src` is an MVD demo, as the encoders do before any work:
/// its first frame must be whole, carry messages (not only a `dem_set`)
/// and parse without an escape (FORMAT.md §6.2). Real demos start with
/// the server data; a truncated or partly damaged demo passes as long as
/// its start is intact, and what does not parse later is stored raw as
/// always. Empty input, gzip data and other files fail with
/// `Error::NotMvd`.
pub fn check_demo(src: &[u8]) -> Result<()> {
    if src.is_empty() {
        return Err(Error::NotMvd("empty input"));
    }
    if is_gzip(src) {
        return Err(Error::NotMvd("gzip-compressed data"));
    }
    if !crate::meta::starts_like_mvd(src) {
        return Err(Error::NotMvd("the first frame does not parse"));
    }
    Ok(())
}

/// True if `b` starts with the MVDZ magic and a version this library
/// reads (needs 5 bytes; the flags byte is not checked).
pub fn is_mvdz(b: &[u8]) -> bool {
    b.len() >= 5 && &b[..4] == MAGIC && b[4] == VERSION
}

/// True if `b` starts with the gzip magic (RFC 1952: `1f 8b`), as the
/// `.mvd.gz` files of most demo archives do.
pub fn is_gzip(b: &[u8]) -> bool {
    b.starts_with(&[0x1f, 0x8b])
}

/// Decompresses gzip data (every member, as `gzip -d` does), refusing
/// output over `max` bytes. A bad CRC, a truncated stream or anything
/// else gzip refuses is `Error::NotMvd`, never a partial result.
pub fn gunzip(b: &[u8], max: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(b)
        .take(max.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|_| Error::NotMvd("corrupt gzip data (bad CRC, truncated or damaged)"))?;
    if out.len() as u64 > max {
        return Err(Error::Limit(
            "gzip data decompresses to more than the limit",
        ));
    }
    Ok(out)
}

/// Decompresses a whole file with up to `threads` threads and the default
/// limits.
pub fn decompress(b: &[u8], threads: usize) -> Result<Vec<u8>> {
    crate::decode::decompress(b, threads, &ReadOptions::default())
}

/// Decompresses a whole file under the caller's options (limits, trusted).
pub fn decompress_with(b: &[u8], threads: usize, opts: &ReadOptions) -> Result<Vec<u8>> {
    crate::decode::decompress(b, threads, opts)
}

/// Decompresses from `r` into `w` segment by segment (sequential reading;
/// nothing is buffered beyond one segment). On an error the bytes already
/// written are a prefix of the demo.
pub fn decompress_stream<R: Read, W: Write>(
    r: R,
    mut w: W,
    threads: usize,
    opts: &ReadOptions,
) -> Result<u64> {
    let mut rd = Reader::new(r, threads, opts)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut n = 0u64;
    loop {
        let k = rd.read_demo(&mut buf)?;
        if k == 0 {
            break;
        }
        w.write_all(&buf[..k])?;
        n += k as u64;
    }
    w.flush()?;
    Ok(n)
}

/// Streaming compressor for a growing demo: `write` demo bytes as they
/// arrive, then `finish` (or `abort`). Segments are written as they fill.
/// The first error is sticky: every later call returns it and nothing more
/// is written.
pub struct Writer<W: Write>(StreamEncoder<W>);

impl<W: Write> Writer<W> {
    /// `cfg.segment`: segment size in raw bytes (0 = 1 MiB).
    pub fn new(w: W, cfg: &Config) -> Result<Self> {
        Ok(Writer(StreamEncoder::new(w, cfg)?))
    }
    /// Appends demo bytes.
    pub fn write_demo(&mut self, b: &[u8]) -> Result<()> {
        self.0.write_demo(b)
    }
    /// Sets an extension metadata key, replacing an earlier value of the
    /// same key; any time before `finish`. A refused key (the extension
    /// metadata over 1 MiB) leaves the writer as it was.
    pub fn set_ext(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.0.set_ext(key, value)
    }
    /// Ends the demo (metadata, index, trailer) and returns the writer.
    pub fn finish(self) -> Result<W> {
        self.0.finish()
    }
    /// Stops without finishing: the output is an incomplete file whose
    /// complete segments still decode sequentially.
    pub fn abort(self) -> W {
        self.0.abort()
    }
}

impl<W: Write> Write for Writer<W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.write_demo(b)?;
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- metadata

/// What a file says about itself, read without decoding the demo.
#[derive(Clone, Debug, Default)]
pub struct FileInfo {
    pub flags: u8,
    pub meta: Metadata,
    pub index: Index,
    pub attachments: Vec<AttachInfo>,
    /// compressed file size
    pub file_size: u64,
}

fn file_info(lay: crate::format::Layout, file_size: u64) -> FileInfo {
    FileInfo {
        flags: lay.flags,
        meta: lay.meta.map(|m| m.1).unwrap_or_default(),
        index: lay.index.map(|i| i.1).unwrap_or_default(),
        attachments: lay.adir.map(|a| a.1).unwrap_or_default(),
        file_size,
    }
}

/// Reads the metadata, index and attachment directory of a complete file,
/// after checking its section structure (one read per section header and
/// segment header; no segment is decoded). The metadata is checked against
/// its CRC, but its identity is verified only by a full decode.
pub fn read_metadata<R: Read + Seek>(r: R) -> Result<FileInfo> {
    read_metadata_with(r, &Limits::default())
}

pub fn read_metadata_with<R: Read + Seek>(r: R, limits: &Limits) -> Result<FileInfo> {
    let budget = Budget::new(limits);
    let mut src = SeekSrc(r);
    let len = crate::decode::Src::len(&mut src)?;
    budget.input(len);
    let lay = scan_layout(&mut src, &budget)?;
    Ok(file_info(lay, len))
}

/// Latin-1 view of bytes: every byte is the code point of the same value,
/// so a reader recovers the bytes exactly.
fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

#[derive(serde::Serialize)]
struct JRoster {
    slot: u8,
    userid: u32,
    userinfo: String,
}

#[derive(serde::Serialize)]
struct JSeg {
    t_start_ms: u64,
    raw_offset: u64,
    raw_len: u64,
}

#[derive(serde::Serialize)]
struct JAttach<'a> {
    name: &'a str,
    mime: &'a str,
    size: u64,
    stored: u64,
    sha256: String,
    kv: serde_json::Map<String, serde_json::Value>,
}

#[derive(serde::Serialize)]
struct JInfo<'a> {
    version: u8,
    streamed: bool,
    file_size: u64,
    bundle_sha256: String,
    sha256: String,
    size: u64,
    duration_ms: u64,
    frames: u64,
    map: String,
    levelname: String,
    serverinfo: String,
    roster: Vec<JRoster>,
    demoinfo: Option<String>,
    ext: serde_json::Map<String, serde_json::Value>,
    census: Option<Box<serde_json::value::RawValue>>,
    clock: serde_json::Value,
    segments: Vec<JSeg>,
    attachments: Vec<JAttach<'a>>,
}

/// The bundle hash (FORMAT.md §10.2): SHA-256 over the demo's identity
/// and the attachment directory's names, MIME types, SHA-256s and
/// key/values, sorted, in a canonical encoding. Independent of the level,
/// the segmenting, the attachment codecs and the section order. It is
/// computed from what the file claims (the META identity and the directory
/// SHA-256s); a full decode and reading each attachment verify those.
pub fn bundle_hash(demo_sha256: &[u8; 32], attachments: &[AttachInfo]) -> [u8; 32] {
    use crate::util::put_uv;
    use sha2::{Digest, Sha256};
    let bytes = |o: &mut Vec<u8>, b: &[u8]| {
        put_uv(o, b.len() as u64);
        o.extend_from_slice(b);
    };
    let mut o = Vec::new();
    o.extend_from_slice(b"MVDZBND1");
    o.extend_from_slice(demo_sha256);
    let mut atts: Vec<&AttachInfo> = attachments.iter().collect();
    atts.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    put_uv(&mut o, atts.len() as u64);
    for a in atts {
        bytes(&mut o, a.name.as_bytes());
        bytes(&mut o, a.mime.as_bytes());
        o.extend_from_slice(&a.sha256);
        let mut kv: Vec<&(String, String)> = a.kv.iter().collect();
        kv.sort_by(|x, y| x.0.as_bytes().cmp(y.0.as_bytes()));
        put_uv(&mut o, kv.len() as u64);
        for (k, v) in kv {
            bytes(&mut o, k.as_bytes());
            bytes(&mut o, v.as_bytes());
        }
    }
    Sha256::digest(&o).into()
}

/// A `Write` that refuses to grow past a limit.
struct Capped(Vec<u8>, usize);

impl Write for Capped {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.0.len() + b.len() > self.1 {
            return Err(std::io::Error::other("limit"));
        }
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl FileInfo {
    /// The bundle hash of this file (see [`bundle_hash`]).
    pub fn bundle_hash(&self) -> [u8; 32] {
        bundle_hash(&self.meta.sha256, &self.attachments)
    }

    /// JSON rendering (see [`FileInfo::to_json_with`]), without a size
    /// limit.
    pub fn to_json(&self) -> String {
        self.to_json_with(usize::MAX).expect("no limit")
    }

    /// JSON rendering, refused with a limit error once it would exceed
    /// `max` bytes: escaping can make it up to six times the metadata
    /// (FORMAT.md §3.2). Byte strings (map, serverinfo, userinfo, demoinfo,
    /// extension values) are Latin-1: each byte becomes the code point of
    /// the same value. The census is included only if it is a JSON object.
    pub fn to_json_with(&self, max: usize) -> Result<String> {
        let m = &self.meta;
        let census = serde_json::from_str::<Box<serde_json::value::RawValue>>(&m.census)
            .ok()
            .filter(|r| r.get().trim_start().starts_with('{'));
        let j = JInfo {
            version: VERSION,
            bundle_sha256: hex(&self.bundle_hash()),
            streamed: self.flags & F_STREAMED != 0,
            file_size: self.file_size,
            sha256: hex(&m.sha256),
            size: m.size,
            duration_ms: m.duration_ms,
            frames: m.frames,
            map: latin1(&m.map),
            levelname: latin1(&m.levelname),
            serverinfo: latin1(&m.serverinfo),
            roster: m
                .roster
                .iter()
                .map(|(s, u, i)| JRoster {
                    slot: *s,
                    userid: *u,
                    userinfo: latin1(i),
                })
                .collect(),
            demoinfo: m.demoinfo_json().map(|d| latin1(&d)),
            ext: m
                .ext
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(latin1(v))))
                .collect(),
            census,
            clock: {
                let c = &m.clock;
                serde_json::json!({
                    "start_timestamp_ms": c.start_ms.map(|(t, v)| [t, v]),
                    "epoch": c.epoch.as_ref().map(|(t, v)| serde_json::json!([t, latin1(v)])),
                    "matchdate": c.matchdate.as_ref().map(|(t, v)| serde_json::json!([t, latin1(v)])),
                    "pauses": c.pauses.iter().map(|p| [p.0, p.1]).collect::<Vec<_>>(),
                    "pauses_truncated": c.pauses_truncated,
                })
            },
            segments: self
                .index
                .segs
                .iter()
                .map(|s| JSeg {
                    t_start_ms: s.t_start,
                    raw_offset: s.raw_off,
                    raw_len: s.raw_len,
                })
                .collect(),
            attachments: self
                .attachments
                .iter()
                .map(|a| JAttach {
                    name: &a.name,
                    mime: &a.mime,
                    size: a.size,
                    stored: a.stored,
                    sha256: hex(&a.sha256),
                    kv: a
                        .kv
                        .iter()
                        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                        .collect(),
                })
                .collect(),
        };
        let mut w = Capped(Vec::new(), max);
        serde_json::to_writer(&mut w, &j).map_err(|_| Error::Limit("metadata rendering"))?;
        // serde_json writes UTF-8
        Ok(String::from_utf8(w.0).expect("JSON is UTF-8"))
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------- attachments

/// Lists the attachments of a file whose names start with `prefix`.
pub fn list_attachments<R: Read + Seek>(r: R, prefix: &str) -> Result<Vec<AttachInfo>> {
    let fi = read_metadata(r)?;
    Ok(fi
        .attachments
        .into_iter()
        .filter(|a| a.name.starts_with(prefix))
        .collect())
}

/// Reads one attachment by name (seeks to it; the demo is not read).
pub fn read_attachment<R: Read + Seek>(r: R, name: &str) -> Result<Vec<u8>> {
    read_attachment_with(r, name, &Limits::default())
}

pub fn read_attachment_with<R: Read + Seek>(r: R, name: &str, limits: &Limits) -> Result<Vec<u8>> {
    let budget = Budget::new(limits);
    let mut src = SeekSrc(r);
    let len = crate::decode::Src::len(&mut src)?;
    budget.input(len);
    let lay = scan_layout(&mut src, &budget)?;
    let fi = file_info(lay, len);
    let e = fi
        .attachments
        .iter()
        .find(|a| a.name == name)
        .ok_or(Error::NotFound)?;
    let _c = budget.alloc(e.stored)?;
    let mut body =
        crate::budget::zeroed(crate::util::usz(e.stored).ok_or(Error::Limit("attachment size"))?)?;
    crate::decode::Src::read_at(&mut src, e.off + SEC_HDR as u64, &mut body)?;
    unpack_attachment(e, body, &budget)
}

/// Returns the file `b` with the attachments named in `remove` dropped and
/// `add` appended (an added name replaces an existing one). The demo part
/// is copied byte for byte; kept attachments are copied without recoding.
/// An update that changes nothing returns the file as it is; one that
/// changes the attachment set rebuilds the attachment part, which drops
/// unknown sections there (FORMAT.md §10, Updating). Refused (nothing is
/// returned) if the result would break a rule every reader applies (the
/// directory over 2^24 bytes; checked first with every new entry's sizes
/// counted at their largest encoding, so a directory within about 20 bytes
/// per new attachment of the limit may be refused), or if a new attachment
/// uses an unprefixed key (FORMAT.md §10.1).
pub fn update_attachments_bytes(b: &[u8], remove: &[&str], add: &[Attachment]) -> Result<Vec<u8>> {
    check_new_attachments(add)?;
    let budget = Budget::unlimited();
    let lay = scan_layout(&mut SliceSrc(b), &budget)?;
    let (_, t) = lay.trailer.ok_or(Error::Corrupt("no trailer"))?;
    let gone: std::collections::HashSet<&str> = remove
        .iter()
        .copied()
        .chain(add.iter().map(|a| a.name.as_str()))
        .collect();
    let all: &[AttachInfo] = lay.adir.as_ref().map_or(&[][..], |d| &d.1[..]);
    let kept: Vec<&AttachInfo> = all
        .iter()
        .filter(|e| !gone.contains(e.name.as_str()))
        .collect();
    // no change to the attachment set: the file as it is
    if add.is_empty() && kept.len() == all.len() {
        return Ok(b.to_vec());
    }
    // the directory's size is known before any attachment is packed (its
    // offsets are fixed-width)
    let mut probe: Vec<AttachInfo> = kept.iter().map(|e| (*e).clone()).collect();
    probe.extend(add.iter().map(|a| AttachInfo {
        name: a.name.clone(),
        mime: a.mime.clone(),
        kv: a.kv.clone(),
        size: u64::MAX,
        stored: u64::MAX,
        ..Default::default()
    }));
    if adir_body(&probe).len() as u64 > DIR_MAX {
        return Err(Error::Invalid("attachment directory over 16 MiB"));
    }
    drop(probe);
    let body_end = t.body_end as usize;
    let mut out = b[..body_end].to_vec();
    let mut dir = Vec::new();
    if let Some((_, d)) = &lay.adir {
        for e in d {
            if gone.contains(e.name.as_str()) {
                continue;
            }
            let start = (e.off + SEC_HDR as u64) as usize;
            let stored = &b[start..start + e.stored as usize];
            let mut e = e.clone();
            e.off = out.len() as u64;
            section(&mut out, K_ATTACH, stored);
            dir.push(e);
        }
    }
    for a in add {
        let (e, stored) = pack_attachment(a, out.len() as u64);
        section(&mut out, K_ATTACH, &stored);
        dir.push(e);
    }
    let adir_off = if dir.is_empty() {
        0
    } else {
        let o = out.len() as u64;
        let ab = adir_body(&dir);
        if ab.len() as u64 > DIR_MAX {
            return Err(Error::Invalid("attachment directory over 16 MiB"));
        }
        section(&mut out, K_ADIR, &ab);
        o
    };
    out.extend_from_slice(&Trailer { adir_off, ..t }.bytes());
    Ok(out)
}

/// Which attachments an export keeps, by name: those that match a `keep`
/// name or a `keep_prefix` (all, if both are empty) and match no `drop`
/// name or `drop_prefix`.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub keep: Vec<String>,
    pub keep_prefix: Vec<String>,
    pub drop: Vec<String>,
    pub drop_prefix: Vec<String>,
}

impl Selection {
    /// True if attachment `e` is kept.
    pub fn matches(&self, e: &AttachInfo) -> bool {
        let n = e.name.as_str();
        let hit = |names: &[String], prefixes: &[String]| {
            names.iter().any(|x| x == n) || prefixes.iter().any(|p| n.starts_with(p.as_str()))
        };
        ((self.keep.is_empty() && self.keep_prefix.is_empty())
            || hit(&self.keep, &self.keep_prefix))
            && !hit(&self.drop, &self.drop_prefix)
    }
}

/// Writes to `w` a copy of the file `r` that keeps only the attachments
/// for which `keep(entry)` is true (FORMAT.md §10, Exporting; see
/// [`Selection`]):
/// the demo part byte for byte, the kept ATTACH sections as stored (no
/// recoding), a new ADIR and a new trailer (unknown sections in the
/// attachment part are dropped). Keeping every attachment copies the file
/// as it is. `r` is only read. Keeping none gives the file without
/// attachments, byte for byte; the result is what removing the others
/// with [`update_attachments`] gives. Returns the
/// bundle hash of the output (FORMAT.md §10.2). Memory is bounded by the
/// directory and a copy buffer, not by the file.
pub fn export_attachments<R: Read + Seek, W: Write>(
    r: R,
    mut w: W,
    keep: impl Fn(&AttachInfo) -> bool,
) -> Result<[u8; 32]> {
    let budget = Budget::new(&Limits::default());
    let mut src = SeekSrc(r);
    let len = crate::decode::Src::len(&mut src)?;
    budget.input(len);
    let lay = scan_layout(&mut src, &budget)?;
    let (_, t) = lay.trailer.ok_or(Error::Corrupt("no trailer"))?;
    let dir: &[AttachInfo] = lay.adir.as_ref().map_or(&[][..], |d| &d.1[..]);
    let kept: Vec<&AttachInfo> = dir.iter().filter(|e| keep(e)).collect();
    let mut buf = vec![0u8; 1 << 16];
    let mut copy = |src: &mut SeekSrc<R>, w: &mut W, mut off: u64, mut n: u64| -> Result<()> {
        while n > 0 {
            let k = n.min(buf.len() as u64) as usize;
            crate::decode::Src::read_at(src, off, &mut buf[..k])?;
            w.write_all(&buf[..k])?;
            off += k as u64;
            n -= k as u64;
        }
        Ok(())
    };
    // nothing to drop: the file as it is, byte for byte (unknown sections
    // and non-minimal encodings in the attachment part included)
    if kept.len() == dir.len() {
        copy(&mut src, &mut w, 0, len)?;
        w.flush()?;
        return Ok(bundle_hash(
            &lay.meta.as_ref().map(|m| m.1.sha256).unwrap_or_default(),
            dir,
        ));
    }
    copy(&mut src, &mut w, 0, t.body_end)?;
    let mut pos = t.body_end;
    let mut out_dir = Vec::with_capacity(kept.len());
    for e in &kept {
        let mut e = (*e).clone();
        let at = e.off;
        e.off = pos;
        copy(&mut src, &mut w, at, SEC_HDR as u64 + e.stored)?;
        pos += SEC_HDR as u64 + e.stored;
        out_dir.push(e);
    }
    let adir_off = if out_dir.is_empty() {
        0
    } else {
        let mut b = Vec::new();
        section(&mut b, K_ADIR, &adir_body(&out_dir));
        w.write_all(&b)?;
        pos
    };
    w.write_all(&Trailer { adir_off, ..t }.bytes())?;
    w.flush()?;
    let meta = lay.meta.as_ref().map(|m| m.1.sha256).unwrap_or_default();
    Ok(bundle_hash(&meta, &out_dir))
}

/// Updates the attachments of the file at `path` (see
/// [`update_attachments_bytes`]) safely: an advisory lock on the file
/// serialises concurrent updaters (threads and processes); the new file is
/// written to a fresh temporary file in the same directory with the
/// original's permissions, synced, renamed over the original, and the
/// directory is synced. A symlink is followed to the file it names.
/// Readers that have the old file open keep a consistent view (POSIX).
pub fn update_attachments(
    path: &std::path::Path,
    remove: &[&str],
    add: &[Attachment],
) -> Result<()> {
    let real = std::fs::canonicalize(path)?;
    let (mut f, m) = lock_file(&real)?;
    let mut b = Vec::new();
    f.read_to_end(&mut b)?;
    let out = update_attachments_bytes(&b, remove, add)?;
    replace_locked(&real, |f| f.write_all(&out), m.permissions(), gid(&m))?;
    drop(f);
    Ok(())
}

/// Opens and locks `path`; retries if another updater replaced the file
/// while this one waited for the lock.
fn lock_file(path: &std::path::Path) -> Result<(std::fs::File, std::fs::Metadata)> {
    for _ in 0..100 {
        let f = std::fs::File::open(path)?;
        f.lock()?;
        let a = f.metadata()?;
        let b = std::fs::metadata(path)?;
        if same_file(&a, &b) {
            return Ok((f, a));
        }
    }
    Err(Error::Io(std::io::ErrorKind::WouldBlock))
}

#[cfg(unix)]
fn gid(m: &std::fs::Metadata) -> Option<u32> {
    Some(std::os::unix::fs::MetadataExt::gid(m))
}

#[cfg(not(unix))]
fn gid(_: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_: &std::fs::Metadata, _: &std::fs::Metadata) -> bool {
    true
}

/// A temporary name nobody else uses: process, time, counter.
fn temp_name(name: &str, attempt: u32) -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(".{name}.{}.{t:x}.{n}.{attempt}.tmp", std::process::id())
}

/// Writes `data` to a new file next to `path` and renames it over `path`;
/// the new file gets permissions `perm` and (Unix, when allowed) group
/// `group`.
fn replace_locked(
    path: &std::path::Path,
    fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    perm: std::fs::Permissions,
    group: Option<u32>,
) -> Result<()> {
    #[cfg(not(unix))]
    let _ = group;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let name = path
        .file_name()
        .ok_or(Error::Invalid("path"))?
        .to_string_lossy()
        .into_owned();
    let mut attempt = 0;
    let (tmp, mut f) = loop {
        let tmp = dir.join(temp_name(&name, attempt));
        // create_new: never follows or reuses an existing path. Created
        // owner-only (0600 on Unix), then given the final permissions
        // (and group) before a byte is written: no other user can open it
        // while it holds data the original's permissions would not show
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        match o.open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 => attempt += 1,
            Err(e) => return Err(e.into()),
        }
    };
    let res = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(gid) = group {
            // best effort: only the owner's groups are allowed
            let _ = std::os::unix::fs::fchown(&f, None, Some(gid));
        }
        f.set_permissions(perm)?;
        fill(&mut f)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        if let Ok(d) = std::fs::File::open(dir) {
            d.sync_all()?;
        }
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(res?)
}

/// Writes `data` to `path` atomically (temporary file in the same
/// directory, fsync, rename, fsync of the directory). New files get mode
/// 0644 subject to the umask; an existing file keeps its permissions.
pub fn write_file_atomic(path: &std::path::Path, data: &[u8]) -> Result<()> {
    write_file_atomic_with(path, |f| f.write_all(data))
}

/// As `write_file_atomic`, with the contents written by `fill` (e.g.
/// through an encoder, without building them in memory first).
pub fn write_file_atomic_with(
    path: &std::path::Path,
    fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<()> {
    let (perm, group) = match std::fs::metadata(path) {
        Ok(m) => (m.permissions(), gid(&m)),
        Err(_) => {
            let dir = path
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or(std::path::Path::new("."));
            let probe = dir.join(temp_name("probe", 0));
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe)?;
            let p = f.metadata()?.permissions();
            drop(f);
            let _ = std::fs::remove_file(&probe);
            (p, None)
        }
    };
    replace_locked(path, fill, perm, group)
}

/// Writes `data` gzip-compressed (level 6) to `w`, streamed. The gzip
/// bytes differ from other gzip tools' output; the demo inside is the
/// same.
pub fn write_gzip<W: Write>(data: &[u8], w: W) -> std::io::Result<W> {
    let mut e = flate2::write::GzEncoder::new(w, flate2::Compression::new(6));
    e.write_all(data)?;
    e.finish()
}

/// The original demo, read sequentially (any reader).
pub fn reader<R: Read>(r: R, threads: usize) -> Result<Reader<Seq<R>>> {
    Reader::new(r, threads, &ReadOptions::default())
}
