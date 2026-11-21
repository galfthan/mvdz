//! What the encoder reads from the raw demo besides the transform: its
//! identity (SHA-256, size) and timing (frames, duration), and whether an
//! input starts like a demo at all. Nothing here reads game content;
//! that is the separate `mvdz-meta` tool.

use crate::proto::{Proto, frame_len, le32};
use crate::segment::frame_scan;
use sha2::{Digest, Sha256};

/// Identity and timing of a demo, fed whole frames in order (the last
/// feed may end in bytes that do not frame).
pub struct Ident {
    sha: Sha256,
    pub size: u64,
    pub frames: u64,
    pub duration_ms: u64,
}

impl Default for Ident {
    fn default() -> Self {
        Ident {
            sha: Sha256::new(),
            size: 0,
            frames: 0,
            duration_ms: 0,
        }
    }
}

impl Ident {
    pub fn of(b: &[u8]) -> Ident {
        let mut i = Ident::default();
        i.feed(b);
        i
    }

    pub fn feed(&mut self, b: &[u8]) {
        self.sha.update(b);
        self.size += b.len() as u64;
        let (frames, ms, _) = frame_scan(b);
        self.frames += frames;
        self.duration_ms += ms;
    }

    /// The SHA-256 of everything fed.
    pub fn sha256(&mut self) -> [u8; 32] {
        std::mem::take(&mut self.sha).finalize().into()
    }
}

/// Whether the messages of one whole frame parse, in a fresh protocol
/// state: every command is known and fits, and a hidden (`dem_multiple`
/// mask 0) payload is a sequence of whole `u32 len | u16 type | data`
/// blocks with len at least 1.
fn frame_parses(f: &[u8]) -> bool {
    let body = match f[1] & 7 {
        3 if le32(&f[2..]) == 0 => {
            let b = &f[10..];
            let mut i = 0;
            while i < b.len() {
                let rem = b.len() - i;
                if rem < 6 {
                    return false;
                }
                let bl = le32(&b[i..]) as u64;
                if bl < 1 || bl > (rem - 6) as u64 {
                    return false;
                }
                i += 6 + bl as usize;
            }
            return true;
        }
        3 => &f[10..],
        1 | 4 | 5 | 6 => &f[6..],
        _ => return true,
    };
    let mut proto = Proto::default();
    let mut i = 0;
    while i < body.len() {
        let cmd = body[i];
        let Some(sz) = proto.cmd_size(cmd, &body[i + 1..]).filter(|_| cmd != 0) else {
            return false;
        };
        if cmd == 11 {
            proto.observe_server_data(&body[i + 1..i + 1 + sz]);
        }
        i += 1 + sz;
    }
    true
}

/// Whether `b` starts like an MVD demo: `Some(true)` or `Some(false)`
/// once the start of `b` decides it, `None` while more bytes are needed.
/// A demo starts with a whole frame that carries messages (a non-empty
/// `dem_read`/`dem_all`/... payload, or hidden or multi-target data; not a
/// bare `dem_set`), and either its messages parse or, if that first frame
/// is damaged, the next 16 frames are whole too. A frame's length field
/// must fit the input, so other files almost never frame: text, gzip,
/// PNG, TIFF and random bytes fail on the first frame.
pub fn mvd_start(b: &[u8]) -> Option<bool> {
    let bad = |b: &[u8], p: usize| b.len() >= p + 2 && matches!(b[p + 1] & 7, 0 | 7);
    if b.len() < 2 {
        return None;
    }
    let head = match b[1] & 7 {
        0 | 2 | 7 => return Some(false),
        3 => 10,
        _ => 6,
    };
    let n = frame_len(b, 0)?;
    if n <= head {
        return Some(false);
    }
    if frame_parses(&b[..n]) {
        return Some(true);
    }
    let mut p = n;
    for _ in 0..16 {
        if bad(b, p) {
            return Some(false);
        }
        p += frame_len(b, p)?;
    }
    Some(true)
}

/// `mvd_start` of a whole input: a start that is not decided by the end of
/// the input is not a demo.
pub fn starts_like_mvd(b: &[u8]) -> bool {
    mvd_start(b) == Some(true)
}
