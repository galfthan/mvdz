//! Shared helpers: LEB128 varints, checked integer conversions, CRC-32C,
//! a scoped parallel map.

use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------- varints

pub fn put_uv(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Byte cursor over a buffer. Every read is bounds-checked.
pub struct Rd<'a> {
    pub b: &'a [u8],
    pub i: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Rd<'a> {
        Rd { b, i: 0 }
    }
    /// Unsigned LEB128, at most 10 bytes. A value that does not fit in 64
    /// bits (bits set above bit 63 in the 10th byte) is an error.
    pub fn uv(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for k in 0..10 {
            let b = *self.b.get(self.i)?;
            self.i += 1;
            if k == 9 && b > 1 {
                return None;
            }
            v |= ((b & 0x7f) as u64) << (7 * k);
            if b < 0x80 {
                return Some(v);
            }
        }
        None
    }
    /// A `uv` that must fit in a `usize`.
    pub fn uz(&mut self) -> Option<usize> {
        usize::try_from(self.uv()?).ok()
    }
    /// A `uv` that must fit in a `u32`.
    pub fn u32v(&mut self) -> Option<u32> {
        u32::try_from(self.uv()?).ok()
    }
    pub fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.i)?;
        self.i += 1;
        Some(v)
    }
    pub fn u32le(&mut self) -> Option<u32> {
        let s = self.take(4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }
    /// A length-prefixed byte string (`uv` length, bytes).
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.uz()?;
        self.take(n)
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.b[self.i.min(self.b.len())..]
    }
    pub fn done(&self) -> bool {
        self.i == self.b.len()
    }
}

/// `u64` to `usize`, or `None` on a 32-bit target when it does not fit.
#[inline]
pub fn usz(v: u64) -> Option<usize> {
    usize::try_from(v).ok()
}

// ---------------------------------------------------------------- crc32

static CRC_TABLE: std::sync::OnceLock<[[u32; 256]; 8]> = std::sync::OnceLock::new();

/// CRC-32C (Castagnoli). Hardware `crc32` instruction when SSE4.2 is
/// present, slice-by-8 tables otherwise.
pub fn crc32(b: &[u8]) -> u32 {
    !crc32c_update(!0, b)
}

/// CRC-32C register update (no pre/post inversion): crc32(b) =
/// !crc32c_update(!0, b), and updates chain over consecutive chunks.
pub fn crc32c_update(c: u32, b: &[u8]) -> u32 {
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar")))]
    {
        if std::arch::is_x86_feature_detected!("sse4.2") {
            // SAFETY: the CPU supports SSE4.2 (checked above)
            return unsafe { crc32c_hw(c, b) };
        }
    }
    crc32c_sw(c, b)
}

#[cfg(all(target_arch = "x86_64", not(feature = "scalar")))]
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_hw(c0: u32, b: &[u8]) -> u32 {
    use std::arch::x86_64::*;
    let mut c = c0 as u64;
    let mut ch = b.chunks_exact(8);
    for w in &mut ch {
        c = _mm_crc32_u64(c, u64::from_le_bytes(w.try_into().unwrap()));
    }
    let mut c = c as u32;
    for &x in ch.remainder() {
        c = _mm_crc32_u8(c, x);
    }
    c
}

pub fn crc32c_sw(c0: u32, b: &[u8]) -> u32 {
    let t = CRC_TABLE.get_or_init(|| {
        let mut t = [[0u32; 256]; 8];
        for i in 0..256u32 {
            let mut c = i;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0x82F6_3B78 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            t[0][i as usize] = c;
        }
        for i in 0..256 {
            for k in 1..8 {
                t[k][i] = (t[k - 1][i] >> 8) ^ t[0][(t[k - 1][i] & 0xff) as usize];
            }
        }
        t
    });
    let mut c = c0;
    let mut ch = b.chunks_exact(8);
    for w in &mut ch {
        let lo = c ^ u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        let hi = u32::from_le_bytes([w[4], w[5], w[6], w[7]]);
        c = t[7][(lo & 0xff) as usize]
            ^ t[6][((lo >> 8) & 0xff) as usize]
            ^ t[5][((lo >> 16) & 0xff) as usize]
            ^ t[4][(lo >> 24) as usize]
            ^ t[3][(hi & 0xff) as usize]
            ^ t[2][((hi >> 8) & 0xff) as usize]
            ^ t[1][((hi >> 16) & 0xff) as usize]
            ^ t[0][(hi >> 24) as usize];
    }
    for &x in ch.remainder() {
        c = (c >> 8) ^ t[0][((c ^ x as u32) & 0xff) as usize];
    }
    c
}

/// Running CRC-32C over consecutive chunks.
#[derive(Clone, Copy)]
pub struct Crc(u32);

impl Default for Crc {
    fn default() -> Self {
        Crc(!0)
    }
}

impl Crc {
    pub fn update(&mut self, b: &[u8]) {
        self.0 = crc32c_update(self.0, b);
    }
    pub fn finish(&self) -> u32 {
        !self.0
    }
}

// ---------------------------------------------------------------- parallel helper

/// True where the standard library can spawn threads.
pub const HAS_THREADS: bool = !cfg!(target_family = "wasm");

/// Runs `f(i)` for i in 0..n on up to `threads` threads; results in order.
pub fn par_map<T: Send, F: Fn(usize) -> T + Sync>(threads: usize, n: usize, f: F) -> Vec<T> {
    if threads <= 1 || n <= 1 || !HAS_THREADS {
        return (0..n).map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut out: Vec<Option<T>> = (0..n).map(|_| None).collect();
    let slots = SyncPtr(out.as_mut_ptr());
    std::thread::scope(|s| {
        for _ in 0..threads.min(n) {
            s.spawn(|| {
                let slots = &slots;
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let v = f(i);
                    // SAFETY: each index is claimed by exactly one thread,
                    // and `out` outlives the scope.
                    unsafe { *slots.0.add(i) = Some(v) };
                }
            });
        }
    });
    out.into_iter()
        .map(|v| v.expect("every index is filled"))
        .collect()
}

struct SyncPtr<T>(*mut T);
unsafe impl<T> Sync for SyncPtr<T> {}
unsafe impl<T> Send for SyncPtr<T> {}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32c_check_value() {
        assert_eq!(super::crc32(b"123456789"), 0xE306_9283);
        assert_eq!(super::crc32c_sw(!0, b"123456789"), !0xE306_9283);
        let c = super::crc32c_update(super::crc32c_update(!0, b"1234"), b"56789");
        assert_eq!(!c, 0xE306_9283);
    }

    #[test]
    fn uv_limits() {
        let mut v = Vec::new();
        super::put_uv(&mut v, u64::MAX);
        assert_eq!(v.len(), 10);
        assert_eq!(super::Rd::new(&v).uv(), Some(u64::MAX));
        // a 10th byte with bits above bit 63
        let bad = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert_eq!(super::Rd::new(&bad).uv(), None);
        // 11 bytes
        let long = [0x80; 11];
        assert_eq!(super::Rd::new(&long).uv(), None);
    }

    #[test]
    fn par_map_order() {
        let v = super::par_map(3, 100, |i| i * 2);
        assert_eq!(v, (0..100).map(|i| i * 2).collect::<Vec<_>>());
    }
}
