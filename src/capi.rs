//! C ABI (declared in `include/mvdz.h`).
//!
//! * Every entry point catches panics (none should happen) and reports
//!   them as `MVDZ_E_PANIC`; no unwinding crosses the boundary.
//! * Handles are opaque ids looked up in a registry, not pointers to
//!   memory: a closed, finished or unknown handle gives `MVDZ_E_INVALID`
//!   instead of undefined behaviour, and closing twice is harmless. Calls
//!   on one handle are serialised by a lock, so a handle may be used from
//!   several threads. A call on a handle from inside one of that handle's
//!   own callbacks (same thread, lock held) fails with `MVDZ_E_INVALID`
//!   instead of deadlocking. A handle whose call panicked fails every
//!   later call with `MVDZ_E_PANIC`.
//! * Buffers returned to C carry their length and capacity in a hidden
//!   header, so `mvdz_free` does not depend on the length the caller
//!   passes; the result is handed over without a copy. Empty results are
//!   returned as NULL with length 0.

use crate::api::{self, Config};
use crate::budget::{Limits, ReadOptions};
use crate::error::Error;
use crate::segment::Level;
use crate::stream::{Reader, Seekable, Seq};
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_int, c_longlong, c_void};
use std::io::{Read, Seek, SeekFrom, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

pub const MVDZ_OK: c_int = 0;
pub const MVDZ_E_FORMAT: c_int = -1;
pub const MVDZ_E_CORRUPT: c_int = -2;
pub const MVDZ_E_INVALID: c_int = -3;
pub const MVDZ_E_IO: c_int = -4;
pub const MVDZ_E_NOTFOUND: c_int = -5;
pub const MVDZ_E_PANIC: c_int = -6;
pub const MVDZ_E_LIMIT: c_int = -7;
pub const MVDZ_E_NOTMVD: c_int = -8;

/// Decode flag: trusted input, skip the SHA-256 identity check (the
/// CRC-32C checks still run).
pub const MVDZ_TRUSTED: u32 = 1;

fn code(e: &Error) -> c_int {
    match e {
        Error::Format(_) => MVDZ_E_FORMAT,
        Error::Corrupt(_) => MVDZ_E_CORRUPT,
        Error::Limit(_) => MVDZ_E_LIMIT,
        Error::Invalid(_) => MVDZ_E_INVALID,
        Error::NotFound => MVDZ_E_NOTFOUND,
        Error::Io(_) => MVDZ_E_IO,
        Error::Internal(_) => MVDZ_E_PANIC,
        Error::NotMvd(_) => MVDZ_E_NOTMVD,
    }
}

/// Runs `f`, turning a panic into `err`.
fn guard<T>(err: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(err)
}

unsafe fn slice<'a>(p: *const c_void, n: usize) -> Option<&'a [u8]> {
    if n == 0 {
        return Some(&[]);
    }
    if p.is_null() {
        return None;
    }
    // SAFETY: the caller passes n readable bytes at p
    Some(unsafe { std::slice::from_raw_parts(p as *const u8, n) })
}

unsafe fn set<T>(p: *mut T, v: T) {
    if !p.is_null() {
        // SAFETY: a non-NULL out-pointer points at a T
        unsafe { *p = v };
    }
}

const HDR: usize = 16;

/// Hands a buffer to C: NULL for an empty one, else a pointer just past a
/// hidden header (length, capacity) in the same allocation, freed by
/// mvdz_free. The bytes are moved within the buffer (a realloc that can
/// grow in place, then a memmove), not copied to a second buffer;
/// allocation failure is MVDZ_E_LIMIT, not an abort.
unsafe fn give(mut v: Vec<u8>, out: *mut *mut u8, out_n: *mut usize) -> c_int {
    if out.is_null() || out_n.is_null() {
        return MVDZ_E_INVALID;
    }
    let n = v.len();
    let p = if n == 0 {
        std::ptr::null_mut()
    } else {
        if v.try_reserve_exact(HDR).is_err() {
            return MVDZ_E_LIMIT;
        }
        v.resize(n + HDR, 0);
        v.copy_within(0..n, HDR);
        let cap = v.capacity();
        v[..8].copy_from_slice(&(n as u64).to_le_bytes());
        v[8..HDR].copy_from_slice(&(cap as u64).to_le_bytes());
        let mut v = std::mem::ManuallyDrop::new(v);
        // SAFETY: HDR < len of the allocation
        unsafe { v.as_mut_ptr().add(HDR) }
    };
    unsafe {
        *out_n = n;
        *out = p;
    }
    MVDZ_OK
}

/// Frees a buffer returned by the library. `n` is ignored (kept for the
/// ABI); NULL is a no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_free(p: *mut c_void, _n: usize) {
    if p.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        // SAFETY: p came from `give`: the header (length, capacity of the
        // Vec it was) precedes it
        let base = (p as *mut u8).sub(HDR);
        let h = std::slice::from_raw_parts(base, HDR);
        let n = u64::from_le_bytes(h[..8].try_into().unwrap()) as usize;
        let cap = u64::from_le_bytes(h[8..].try_into().unwrap()) as usize;
        drop(Vec::from_raw_parts(base, HDR + n, cap));
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn mvdz_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

#[unsafe(no_mangle)]
pub extern "C" fn mvdz_strerror(code: c_int) -> *const c_char {
    let s: &'static [u8] = match code {
        MVDZ_OK => b"ok\0",
        MVDZ_E_FORMAT => b"not an MVDZ file, or an unsupported version\0",
        MVDZ_E_CORRUPT => b"corrupt or truncated file\0",
        MVDZ_E_INVALID => b"invalid argument or handle\0",
        MVDZ_E_IO => b"i/o error\0",
        MVDZ_E_NOTFOUND => b"not found\0",
        MVDZ_E_PANIC => b"internal error\0",
        MVDZ_E_LIMIT => b"memory or work limit exceeded\0",
        MVDZ_E_NOTMVD => b"input is not an MVD demo (or is corrupt gzip)\0",
        _ => b"unknown error\0",
    };
    s.as_ptr() as *const c_char
}

fn limits(max_memory: u64) -> Limits {
    let mut l = Limits::default();
    if max_memory != 0 {
        l.memory = max_memory;
    }
    l
}

/// The decode options of a C call; None (MVDZ_E_INVALID) for a flag bit
/// this version does not define, so future flags are safe to add.
fn read_options(max_memory: u64, flags: u32) -> Option<ReadOptions> {
    (flags & !MVDZ_TRUSTED == 0).then(|| ReadOptions {
        limits: limits(max_memory),
        trusted: flags & MVDZ_TRUSTED != 0,
    })
}

/// 1 if the buffer starts like an MVDZ file this library reads (5 bytes).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_is_mvdz(buf: *const c_void, n: usize) -> c_int {
    guard(0, || match unsafe { slice(buf, n) } {
        Some(b) => api::is_mvdz(b) as c_int,
        None => 0,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_decompress(
    buf: *const c_void,
    n: usize,
    threads: c_int,
    max_memory: u64,
    flags: u32,
    out: *mut *mut u8,
    out_n: *mut usize,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        let Some(b) = (unsafe { slice(buf, n) }) else {
            return MVDZ_E_INVALID;
        };
        let Some(o) = read_options(max_memory, flags) else {
            return MVDZ_E_INVALID;
        };
        match api::decompress_with(b, threads.max(1) as usize, &o) {
            Ok(v) => unsafe { give(v, out, out_n) },
            Err(e) => code(&e),
        }
    })
}

fn level(l: c_int) -> Option<Level> {
    match l {
        0 => Some(Level::Normal),
        1 => Some(Level::Fast),
        _ => None,
    }
}

/// `level`: 0 normal, 1 fast. `segment`: raw bytes per segment, 0 = one.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_compress(
    buf: *const c_void,
    n: usize,
    lvl: c_int,
    segment: usize,
    threads: c_int,
    out: *mut *mut u8,
    out_n: *mut usize,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        let Some(b) = (unsafe { slice(buf, n) }) else {
            return MVDZ_E_INVALID;
        };
        let Some(level) = level(lvl) else {
            return MVDZ_E_INVALID;
        };
        let cfg = Config {
            level,
            segment,
            threads: threads.max(1) as usize,
            ..Default::default()
        };
        match api::compress(b, &cfg) {
            Ok(v) => unsafe { give(v, out, out_n) },
            Err(e) => code(&e),
        }
    })
}

// ---------------------------------------------------------------- callbacks

/// read: bytes read into buf (0 at end of file, negative on error).
pub type ReadFn =
    Option<unsafe extern "C" fn(user: *mut c_void, buf: *mut c_void, n: usize) -> c_longlong>;
/// seek: new absolute position, negative on error; whence 0 SET, 1 CUR, 2 END.
pub type SeekFn =
    Option<unsafe extern "C" fn(user: *mut c_void, off: c_longlong, whence: c_int) -> c_longlong>;
/// write: bytes consumed (> 0), negative on error.
pub type WriteFn =
    Option<unsafe extern "C" fn(user: *mut c_void, buf: *const c_void, n: usize) -> c_longlong>;

struct Cb {
    rd: ReadFn,
    sk: SeekFn,
    user: *mut c_void,
}

// SAFETY: the callbacks are only called from the thread that is inside an
// mvdz_* call on this handle, one call at a time (the handle lock); the
// header documents that the user pointer must allow that.
unsafe impl Send for Cb {}

impl Read for Cb {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let f = self
            .rd
            .ok_or_else(|| std::io::Error::other("no read callback"))?;
        let r = unsafe { f(self.user, b.as_mut_ptr() as *mut c_void, b.len()) };
        if r < 0 || r as u64 > b.len() as u64 {
            return Err(std::io::Error::other("read callback failed"));
        }
        Ok(r as usize)
    }
}

impl Seek for Cb {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        let f = self
            .sk
            .ok_or_else(|| std::io::Error::other("no seek callback"))?;
        let (off, wh) = match p {
            SeekFrom::Start(o) => (
                i64::try_from(o).map_err(|_| std::io::Error::other("seek offset"))?,
                0,
            ),
            SeekFrom::Current(o) => (o, 1),
            SeekFrom::End(o) => (o, 2),
        };
        let r = unsafe { f(self.user, off, wh) };
        if r < 0 {
            return Err(std::io::Error::other("seek callback failed"));
        }
        Ok(r as u64)
    }
}

enum RInner {
    Seq(Reader<Seq<Cb>>),
    Seek(Reader<Seekable<Cb>>),
    Mem(Reader<Seekable<std::io::Cursor<Vec<u8>>>>),
}

struct RHandle {
    r: RInner,
    raw_size: i64,
}

struct WHandle(Option<api::Writer<WCb>>);

enum Handle {
    R(RHandle),
    W(WHandle),
}

/// Opaque handle types (ids, never dereferenced).
pub type MvdzReader = c_void;
pub type MvdzWriter = c_void;

/// A live handle: its lock and the thread that holds it (0 = none).
struct Slot {
    h: Mutex<Handle>,
    owner: AtomicU64,
}

type Registry = Mutex<(usize, HashMap<usize, Arc<Slot>>)>;

/// A small id per thread (never 0).
fn thread_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local!(static ID: u64 = NEXT.fetch_add(1, Ordering::Relaxed));
    ID.with(|x| *x)
}

/// The lock of a handle, with its owner recorded.
struct Held<'a> {
    g: MutexGuard<'a, Handle>,
    owner: &'a AtomicU64,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        // cleared before the guard field unlocks the mutex
        self.owner.store(0, Ordering::SeqCst);
    }
}

/// Locks a handle. A call from inside one of the handle's own callbacks
/// (this thread holds the lock) is MVDZ_E_INVALID instead of a deadlock; a
/// handle whose call panicked is MVDZ_E_PANIC from then on.
fn lock(s: &Slot) -> Result<Held<'_>, c_int> {
    let me = thread_id();
    if s.owner.load(Ordering::SeqCst) == me {
        return Err(MVDZ_E_INVALID);
    }
    let g = s.h.lock().map_err(|_| MVDZ_E_PANIC)?;
    s.owner.store(me, Ordering::SeqCst);
    Ok(Held { g, owner: &s.owner })
}

fn registry() -> &'static Registry {
    static R: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new((0, HashMap::new())))
}

fn register(h: Handle) -> *mut c_void {
    let mut g = registry().lock().unwrap_or_else(|e| e.into_inner());
    g.0 += 1;
    let id = g.0;
    g.1.insert(
        id,
        Arc::new(Slot {
            h: Mutex::new(h),
            owner: AtomicU64::new(0),
        }),
    );
    id as *mut c_void
}

fn lookup(p: *const c_void) -> Option<Arc<Slot>> {
    let g = registry().lock().unwrap_or_else(|e| e.into_inner());
    g.1.get(&(p as usize)).cloned()
}

fn unregister(p: *const c_void) -> Option<Arc<Slot>> {
    let mut g = registry().lock().unwrap_or_else(|e| e.into_inner());
    g.1.remove(&(p as usize))
}

/// Runs `f` on a live reader handle; an error code otherwise.
fn with_reader<T: From<c_int>>(p: *const c_void, f: impl FnOnce(&mut RHandle) -> T) -> T {
    let Some(h) = lookup(p) else {
        return T::from(MVDZ_E_INVALID);
    };
    let mut g = match lock(&h) {
        Ok(g) => g,
        Err(e) => return T::from(e),
    };
    match &mut *g.g {
        Handle::R(r) => f(r),
        Handle::W(_) => T::from(MVDZ_E_INVALID),
    }
}

fn open_result(r: Result<RHandle, Error>, err: *mut c_int) -> *mut MvdzReader {
    match r {
        Ok(h) => {
            unsafe { set(err, MVDZ_OK) };
            register(Handle::R(h))
        }
        Err(e) => {
            unsafe { set(err, code(&e)) };
            std::ptr::null_mut()
        }
    }
}

/// Opens a reader over a memory buffer (copied).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_open_memory(
    buf: *const c_void,
    n: usize,
    threads: c_int,
    max_memory: u64,
    flags: u32,
    err: *mut c_int,
) -> *mut MvdzReader {
    guard(std::ptr::null_mut(), || {
        let Some(b) = (unsafe { slice(buf, n) }) else {
            unsafe { set(err, MVDZ_E_INVALID) };
            return std::ptr::null_mut();
        };
        let Some(o) = read_options(max_memory, flags) else {
            unsafe { set(err, MVDZ_E_INVALID) };
            return std::ptr::null_mut();
        };
        let r = Reader::open(
            std::io::Cursor::new(b.to_vec()),
            threads.max(1) as usize,
            &o,
        )
        .map(|r| RHandle {
            raw_size: r.metadata().map_or(-1, |m| m.size as i64),
            r: RInner::Mem(r),
        });
        open_result(r, err)
    })
}

/// Opens a reader over callbacks. With `seek` NULL the file is read
/// sequentially (seeking then fails with MVDZ_E_INVALID).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_open_callbacks(
    read: ReadFn,
    seek: SeekFn,
    user: *mut c_void,
    threads: c_int,
    max_memory: u64,
    flags: u32,
    err: *mut c_int,
) -> *mut MvdzReader {
    guard(std::ptr::null_mut(), || {
        let (true, Some(l)) = (read.is_some(), read_options(max_memory, flags)) else {
            unsafe { set(err, MVDZ_E_INVALID) };
            return std::ptr::null_mut();
        };
        let cb = Cb {
            rd: read,
            sk: seek,
            user,
        };
        let th = threads.max(1) as usize;
        let r = if seek.is_some() {
            Reader::open(cb, th, &l).map(|r| RHandle {
                raw_size: r.metadata().map_or(-1, |m| m.size as i64),
                r: RInner::Seek(r),
            })
        } else {
            Reader::new(cb, th, &l).map(|r| RHandle {
                raw_size: -1,
                r: RInner::Seq(r),
            })
        };
        open_result(r, err)
    })
}

/// Reads up to n demo bytes: count read, 0 at the end, negative error
/// code. After an error every later call returns the same error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_read(r: *mut MvdzReader, out: *mut c_void, n: usize) -> c_longlong {
    guard(MVDZ_E_PANIC as c_longlong, || {
        if out.is_null() && n > 0 {
            return MVDZ_E_INVALID as c_longlong;
        }
        with_reader(r, |h| {
            let b: &mut [u8] = if n == 0 {
                &mut []
            } else {
                unsafe { std::slice::from_raw_parts_mut(out as *mut u8, n) }
            };
            let res = match &mut h.r {
                RInner::Seq(x) => x.read_demo(b),
                RInner::Seek(x) => x.read_demo(b),
                RInner::Mem(x) => x.read_demo(b),
            };
            if let RInner::Seq(x) = &h.r {
                if h.raw_size < 0 {
                    h.raw_size = x.metadata().map_or(-1, |m| m.size as i64);
                }
            }
            match res {
                Ok(k) => k as c_longlong,
                Err(e) => code(&e) as c_longlong,
            }
        })
    })
}

/// Size of the original demo in bytes, or -1 if not known (yet).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_raw_size(r: *const MvdzReader) -> c_longlong {
    guard(MVDZ_E_PANIC as c_longlong, || {
        with_reader(r, |h| h.raw_size)
    })
}

/// Number of segments (seek points); negative if the reader cannot seek.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_segment_count(r: *const MvdzReader) -> c_longlong {
    guard(MVDZ_E_PANIC as c_longlong, || {
        with_reader(r, |h| match &h.r {
            RInner::Seek(x) => x.segments() as c_longlong,
            RInner::Mem(x) => x.segments() as c_longlong,
            RInner::Seq(_) => MVDZ_E_INVALID as c_longlong,
        })
    })
}

/// Seeks to the segment holding demo time `ms`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_seek_time(
    r: *mut MvdzReader,
    ms: u64,
    seg_start_ms: *mut u64,
    raw_offset: *mut u64,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        with_reader(r, |h| {
            let res = match &mut h.r {
                RInner::Seek(x) => x.seek_time(ms),
                RInner::Mem(x) => x.seek_time(ms),
                RInner::Seq(_) => return MVDZ_E_INVALID,
            };
            match res {
                Ok((_, t, o)) => {
                    unsafe {
                        set(seg_start_ms, t);
                        set(raw_offset, o);
                    }
                    MVDZ_OK
                }
                Err(e) => code(&e),
            }
        })
    })
}

/// Seeks to segment `i` (0 <= i < segment count).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_seek_segment(
    r: *mut MvdzReader,
    i: usize,
    raw_offset: *mut u64,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        with_reader(r, |h| {
            let res = match &mut h.r {
                RInner::Seek(x) => x.seek_segment(i),
                RInner::Mem(x) => x.seek_segment(i),
                RInner::Seq(_) => return MVDZ_E_INVALID,
            };
            match res {
                Ok(o) => {
                    unsafe { set(raw_offset, o) };
                    MVDZ_OK
                }
                Err(e) => code(&e),
            }
        })
    })
}

/// Closes a reader. Closing an unknown or closed handle does nothing, and
/// so does closing a handle from inside its own callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_close(r: *mut MvdzReader) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let Some(h) = lookup(r) else { return };
        if h.owner.load(Ordering::SeqCst) == thread_id() {
            return;
        }
        let reader = match h.h.lock() {
            Ok(g) => matches!(*g, Handle::R(_)),
            // a panicked handle can still be closed
            Err(p) => matches!(*p.into_inner(), Handle::R(_)),
        };
        if reader {
            drop(unregister(r));
        }
    }));
}

// ---------------------------------------------------------------- metadata, attachments

fn give_json(
    fi: api::FileInfo,
    max_memory: u64,
    json: *mut *mut c_char,
    json_n: *mut usize,
) -> c_int {
    let max = usize::try_from(limits(max_memory).memory).unwrap_or(usize::MAX);
    let mut s = match fi.to_json_with(max) {
        Ok(s) => s.into_bytes(),
        Err(e) => return code(&e),
    };
    let n = s.len();
    s.push(0);
    let rc = unsafe { give(s, json as *mut *mut u8, json_n) };
    if rc == MVDZ_OK {
        // the NUL is not counted
        unsafe { *json_n = n };
    }
    rc
}

/// Metadata, index and attachment directory as a NUL-terminated JSON
/// string (free with mvdz_free). Reads only section headers and the
/// metadata, index and directory sections; the reading and the JSON are
/// held within `max_memory`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_info_json(
    read: ReadFn,
    seek: SeekFn,
    user: *mut c_void,
    max_memory: u64,
    json: *mut *mut c_char,
    json_n: *mut usize,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        if read.is_none() || seek.is_none() {
            return MVDZ_E_INVALID;
        }
        match api::read_metadata_with(
            Cb {
                rd: read,
                sk: seek,
                user,
            },
            &limits(max_memory),
        ) {
            Ok(fi) => give_json(fi, max_memory, json, json_n),
            Err(e) => code(&e),
        }
    })
}

/// Same as mvdz_info_json for a file in memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_info_json_memory(
    buf: *const c_void,
    n: usize,
    max_memory: u64,
    json: *mut *mut c_char,
    json_n: *mut usize,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        let Some(b) = (unsafe { slice(buf, n) }) else {
            return MVDZ_E_INVALID;
        };
        match api::read_metadata_with(std::io::Cursor::new(b), &limits(max_memory)) {
            Ok(fi) => give_json(fi, max_memory, json, json_n),
            Err(e) => code(&e),
        }
    })
}

/// The bundle hash (FORMAT.md §10.2) into `out[32]`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_bundle_hash(
    read: ReadFn,
    seek: SeekFn,
    user: *mut c_void,
    max_memory: u64,
    out: *mut u8,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        if read.is_none() || seek.is_none() || out.is_null() {
            return MVDZ_E_INVALID;
        }
        match api::read_metadata_with(
            Cb {
                rd: read,
                sk: seek,
                user,
            },
            &limits(max_memory),
        ) {
            Ok(fi) => {
                // SAFETY: out points at 32 writable bytes
                unsafe { std::ptr::copy_nonoverlapping(fi.bundle_hash().as_ptr(), out, 32) };
                MVDZ_OK
            }
            Err(e) => code(&e),
        }
    })
}

/// Same as mvdz_bundle_hash for a file in memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_bundle_hash_memory(
    buf: *const c_void,
    n: usize,
    max_memory: u64,
    out: *mut u8,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        let Some(b) = (unsafe { slice(buf, n) }) else {
            return MVDZ_E_INVALID;
        };
        if out.is_null() {
            return MVDZ_E_INVALID;
        }
        match api::read_metadata_with(std::io::Cursor::new(b), &limits(max_memory)) {
            Ok(fi) => {
                // SAFETY: out points at 32 writable bytes
                unsafe { std::ptr::copy_nonoverlapping(fi.bundle_hash().as_ptr(), out, 32) };
                MVDZ_OK
            }
            Err(e) => code(&e),
        }
    })
}

/// Reads one attachment of a file (via callbacks, seeking straight to it).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_attachment_read(
    read: ReadFn,
    seek: SeekFn,
    user: *mut c_void,
    name: *const c_char,
    max_memory: u64,
    out: *mut *mut u8,
    out_n: *mut usize,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        if read.is_none() || seek.is_none() || name.is_null() {
            return MVDZ_E_INVALID;
        }
        let Ok(name) = unsafe { CStr::from_ptr(name) }.to_str() else {
            return MVDZ_E_NOTFOUND;
        };
        match api::read_attachment_with(
            Cb {
                rd: read,
                sk: seek,
                user,
            },
            name,
            &limits(max_memory),
        ) {
            Ok(v) => unsafe { give(v, out, out_n) },
            Err(e) => code(&e),
        }
    })
}

/// An attachment to add (see mvdz.h).
#[repr(C)]
pub struct MvdzAttachment {
    pub name: *const c_char,
    pub mime: *const c_char,
    pub keys: *const *const c_char,
    pub values: *const *const c_char,
    pub nkv: usize,
    pub data: *const c_void,
    pub size: usize,
}

/// A UTF-8 C string (None: NULL or not UTF-8).
unsafe fn cstr(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(p) }.to_str().ok().map(String::from)
    }
}

/// Removes the attachments named in `remove` and adds `add` in the file at
/// `path` (see api::update_attachments).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_attachments_update(
    path: *const c_char,
    remove: *const *const c_char,
    nremove: usize,
    add: *const MvdzAttachment,
    nadd: usize,
) -> c_int {
    guard(MVDZ_E_PANIC, || unsafe {
        let Some(path) = cstr(path) else {
            return MVDZ_E_INVALID;
        };
        if (nremove > 0 && remove.is_null()) || (nadd > 0 && add.is_null()) {
            return MVDZ_E_INVALID;
        }
        let mut rm = Vec::new();
        for i in 0..nremove {
            match cstr(*remove.add(i)) {
                Some(s) => rm.push(s),
                None => return MVDZ_E_INVALID,
            }
        }
        let mut atts = Vec::new();
        for i in 0..nadd {
            let a = &*add.add(i);
            let (Some(name), Some(data)) = (cstr(a.name), slice(a.data, a.size)) else {
                return MVDZ_E_INVALID;
            };
            if a.nkv > 0 && (a.keys.is_null() || a.values.is_null()) {
                return MVDZ_E_INVALID;
            }
            let mut kv = Vec::new();
            for k in 0..a.nkv {
                match (cstr(*a.keys.add(k)), cstr(*a.values.add(k))) {
                    (Some(x), Some(y)) => kv.push((x, y)),
                    _ => return MVDZ_E_INVALID,
                }
            }
            let mime = if a.mime.is_null() {
                String::new()
            } else {
                match cstr(a.mime) {
                    Some(m) => m,
                    None => return MVDZ_E_INVALID,
                }
            };
            atts.push(api::Attachment {
                name,
                mime,
                kv,
                data: data.to_vec(),
            });
        }
        let rmr: Vec<&str> = rm.iter().map(|s| s.as_str()).collect();
        match api::update_attachments(std::path::Path::new(&path), &rmr, &atts) {
            Ok(()) => MVDZ_OK,
            Err(e) => code(&e),
        }
    })
}

/// An export selection by name (see mvdz.h).
#[repr(C)]
pub struct MvdzSelection {
    pub keep: *const *const c_char,
    pub nkeep: usize,
    pub keep_prefix: *const *const c_char,
    pub nkeep_prefix: usize,
    pub drop: *const *const c_char,
    pub ndrop: usize,
    pub drop_prefix: *const *const c_char,
    pub ndrop_prefix: usize,
}

/// Writes a copy of the file read through `read`/`seek` to `write`,
/// keeping the attachments `sel` selects (NULL: all). `bundle` (32 bytes,
/// may be NULL) receives the output's bundle hash.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_attachments_export(
    read: ReadFn,
    seek: SeekFn,
    user: *mut c_void,
    write: WriteFn,
    wuser: *mut c_void,
    sel: *const MvdzSelection,
    bundle: *mut u8,
) -> c_int {
    guard(MVDZ_E_PANIC, || unsafe {
        if read.is_none() || seek.is_none() || write.is_none() {
            return MVDZ_E_INVALID;
        }
        let list = |p: *const *const c_char, n: usize| -> Option<Vec<String>> {
            if n > 0 && p.is_null() {
                return None;
            }
            (0..n).map(|i| cstr(*p.add(i))).collect()
        };
        let sel = match sel.as_ref() {
            None => api::Selection::default(),
            Some(s) => {
                let (Some(keep), Some(keep_prefix), Some(drop), Some(drop_prefix)) = (
                    list(s.keep, s.nkeep),
                    list(s.keep_prefix, s.nkeep_prefix),
                    list(s.drop, s.ndrop),
                    list(s.drop_prefix, s.ndrop_prefix),
                ) else {
                    return MVDZ_E_INVALID;
                };
                api::Selection {
                    keep,
                    keep_prefix,
                    drop,
                    drop_prefix,
                }
            }
        };
        match api::export_attachments(
            Cb {
                rd: read,
                sk: seek,
                user,
            },
            WCb {
                wr: write,
                user: wuser,
            },
            |e| sel.matches(e),
        ) {
            Ok(h) => {
                if !bundle.is_null() {
                    // SAFETY: bundle points at 32 writable bytes
                    std::ptr::copy_nonoverlapping(h.as_ptr(), bundle, 32);
                }
                MVDZ_OK
            }
            Err(e) => code(&e),
        }
    })
}

// ---------------------------------------------------------------- streaming writer

pub struct WCb {
    wr: WriteFn,
    user: *mut c_void,
}

// SAFETY: as for Cb.
unsafe impl Send for WCb {}

impl Write for WCb {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if b.is_empty() {
            return Ok(0);
        }
        let f = self
            .wr
            .ok_or_else(|| std::io::Error::other("no write callback"))?;
        let r = unsafe { f(self.user, b.as_ptr() as *const c_void, b.len()) };
        if r <= 0 || r as u64 > b.len() as u64 {
            return Err(std::io::Error::other("write callback failed"));
        }
        Ok(r as usize)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Opens a streaming compressor writing through `write`. `level`: 0
/// normal, 1 fast. `segment`: raw bytes per segment (0 = 1 MiB).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_writer_open(
    write: WriteFn,
    user: *mut c_void,
    lvl: c_int,
    segment: usize,
    threads: c_int,
    err: *mut c_int,
) -> *mut MvdzWriter {
    guard(std::ptr::null_mut(), || {
        let Some(level) = level(lvl) else {
            unsafe { set(err, MVDZ_E_INVALID) };
            return std::ptr::null_mut();
        };
        if write.is_none() {
            unsafe { set(err, MVDZ_E_INVALID) };
            return std::ptr::null_mut();
        }
        let cfg = Config {
            level,
            segment,
            threads: threads.max(1) as usize,
            ..Default::default()
        };
        match api::Writer::new(WCb { wr: write, user }, &cfg) {
            Ok(w) => {
                unsafe { set(err, MVDZ_OK) };
                register(Handle::W(WHandle(Some(w))))
            }
            Err(e) => {
                unsafe { set(err, code(&e)) };
                std::ptr::null_mut()
            }
        }
    })
}

/// Appends demo bytes. After an error every later call (and finish)
/// returns the same error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_writer_write(
    w: *mut MvdzWriter,
    data: *const c_void,
    n: usize,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        let Some(d) = (unsafe { slice(data, n) }) else {
            return MVDZ_E_INVALID;
        };
        let Some(h) = lookup(w) else {
            return MVDZ_E_INVALID;
        };
        let mut g = match lock(&h) {
            Ok(g) => g,
            Err(e) => return e,
        };
        match &mut *g.g {
            Handle::W(WHandle(Some(wr))) => match wr.write_demo(d) {
                Ok(()) => MVDZ_OK,
                Err(e) => code(&e),
            },
            _ => MVDZ_E_INVALID,
        }
    })
}

/// Sets an extension metadata key (UTF-8, NUL-terminated) to `n` bytes
/// at `value`, replacing an earlier value of the key. A refused key leaves
/// the writer as it was.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_writer_set_ext(
    w: *mut MvdzWriter,
    key: *const c_char,
    value: *const c_void,
    n: usize,
) -> c_int {
    guard(MVDZ_E_PANIC, || {
        let (Some(k), Some(v)) = (unsafe { cstr(key) }, unsafe { slice(value, n) }) else {
            return MVDZ_E_INVALID;
        };
        let Some(h) = lookup(w) else {
            return MVDZ_E_INVALID;
        };
        let mut g = match lock(&h) {
            Ok(g) => g,
            Err(e) => return e,
        };
        match &mut *g.g {
            Handle::W(WHandle(Some(wr))) => match wr.set_ext(&k, v) {
                Ok(()) => MVDZ_OK,
                Err(e) => code(&e),
            },
            _ => MVDZ_E_INVALID,
        }
    })
}

/// Takes the writer out of its handle and unregisters the handle. A handle
/// whose earlier call panicked is unregistered too, its writer dropped
/// unfinished, and gives MVDZ_E_PANIC: finish and abort always free a
/// writer handle, except from inside its own callback.
fn take_writer(w: *const c_void) -> Result<api::Writer<WCb>, c_int> {
    let h = lookup(w).ok_or(MVDZ_E_INVALID)?;
    if h.owner.load(Ordering::SeqCst) == thread_id() {
        return Err(MVDZ_E_INVALID);
    }
    let mut g = match h.h.lock() {
        Ok(g) => g,
        Err(p) => {
            if !matches!(*p.into_inner(), Handle::W(_)) {
                return Err(MVDZ_E_INVALID);
            }
            drop(unregister(w));
            return Err(MVDZ_E_PANIC);
        }
    };
    let Handle::W(wh) = &mut *g else {
        return Err(MVDZ_E_INVALID);
    };
    let wr = wh.0.take();
    drop(g);
    drop(unregister(w));
    wr.ok_or(MVDZ_E_INVALID)
}

/// Ends the demo (writes metadata, index, trailer) and frees the handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_writer_finish(w: *mut MvdzWriter) -> c_int {
    guard(MVDZ_E_PANIC, || match take_writer(w) {
        Ok(wr) => match wr.finish() {
            Ok(_) => MVDZ_OK,
            Err(e) => code(&e),
        },
        Err(c) => c,
    })
}

/// Frees the handle without finishing the file: what was written is an
/// incomplete file (no trailer) whose complete segments still decode.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mvdz_writer_abort(w: *mut MvdzWriter) -> c_int {
    guard(MVDZ_E_PANIC, || match take_writer(w) {
        Ok(wr) => {
            let _ = wr.abort();
            MVDZ_OK
        }
        Err(c) => c,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn sink(_: *mut c_void, _: *const c_void, n: usize) -> c_longlong {
        n as c_longlong
    }

    fn open() -> *mut MvdzWriter {
        let mut err = 0;
        let w = unsafe { mvdz_writer_open(Some(sink), std::ptr::null_mut(), 0, 0, 1, &mut err) };
        assert_eq!(err, MVDZ_OK);
        w
    }

    /// Poisons the handle's lock, as a call that panicked inside it does.
    fn poison(w: *const c_void) {
        let h = lookup(w).unwrap();
        let _ = std::thread::spawn(move || {
            let _g = h.h.lock().unwrap();
            panic!("poison the handle");
        })
        .join();
    }

    #[test]
    fn a_panicked_writer_handle_is_freed_by_finish_and_abort() {
        for finish in [true, false] {
            let w = open();
            poison(w);
            let f = |w| unsafe {
                if finish {
                    mvdz_writer_finish(w)
                } else {
                    mvdz_writer_abort(w)
                }
            };
            assert_eq!(f(w), MVDZ_E_PANIC);
            assert!(lookup(w).is_none(), "the handle is freed");
            assert_eq!(f(w), MVDZ_E_INVALID);
        }
    }

    #[test]
    fn finish_and_abort_free_a_healthy_writer_handle() {
        let w = open();
        assert_eq!(unsafe { mvdz_writer_abort(w) }, MVDZ_OK);
        assert!(lookup(w).is_none());
        assert_eq!(unsafe { mvdz_writer_abort(w) }, MVDZ_E_INVALID);
    }
}
