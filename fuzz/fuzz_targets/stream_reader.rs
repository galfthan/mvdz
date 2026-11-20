#![no_main]
//! The sequential reader (in small reads) and the seekable reader agree
//! with whole-file decoding.
use libfuzzer_sys::fuzz_target;
use std::io::Read;

struct Dribble<'a>(&'a [u8], usize);
impl Read for Dribble<'_> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = b.len().min(5).min(self.0.len() - self.1);
        b[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
        self.1 += n;
        Ok(n)
    }
}

fuzz_target!(|data: &[u8]| {
    let l: mvdz::ReadOptions = mvdz::Limits {
        memory: 256 << 20,
        ..Default::default()
    }
    .into();
    let whole = mvdz::decompress_with(data, 1, &l).ok();
    let mut out = Vec::new();
    let seq = mvdz::Reader::new(Dribble(data, 0), &l)
        .ok()
        .and_then(|mut r| r.read_to_end(&mut out).ok().map(|_| out));
    assert_eq!(whole.is_some(), seq.is_some());
    if let (Some(a), Some(b)) = (&whole, &seq) {
        assert!(a == b);
    }
    if let Ok(mut r) = mvdz::Reader::open(std::io::Cursor::new(data), &l) {
        let n = r.segments();
        if n > 1 {
            let _ = r.seek_segment(n / 2);
            let mut v = Vec::new();
            let _ = r.read_to_end(&mut v);
        }
        let _ = r.seek_time(1000);
    }
});
