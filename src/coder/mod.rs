//! Back ends. Every stream records its own back end id.
//!
//! | id | name  | structural streams       | numeric streams                    | blob streams |
//! |----|-------|--------------------------|------------------------------------|--------------|
//! | 0  | store | bytes                    | zigzag LEB128                      | bytes        |
//! | 1  | zstd  | zstd of the bytes        | zstd of the LEB128 bytes           | zstd         |
//! | 2  | rans  | static rANS, ctx tables  | static rANS bucket + mantissa bits | (not allowed)|
//! | 3  | rans-file | rANS, the file's tables | rANS, the file's tables       | (not allowed)|

pub mod rans;
pub mod rc;

use crate::budget::Budget;
use crate::error::{Error, Result};
use crate::numeric::NCtx;
use rans::{DModel, FileTables};
use std::borrow::Cow;
use std::sync::Arc;

/// The file model of a back end 3 stream; a stream naming back end 3
/// without one is corrupt.
pub fn file_model(ft: Option<&FileTables>, sid: usize) -> Result<&Arc<DModel>> {
    ft.and_then(|t| t.get(sid))
        .ok_or(Error::Corrupt("no file table for the stream"))
}

pub const BE_STORE: u8 = 0;
pub const BE_ZSTD: u8 = 1;
pub const BE_RANS: u8 = 2;
/// rANS with the model from the file's TABLES section.
pub const BE_FILE: u8 = 3;
pub const BE_NAMES: [&str; 4] = ["store", "zstd", "rans", "rans-file"];

#[inline]
pub fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}
#[inline]
pub fn unzigzag(u: u32) -> i32 {
    ((u >> 1) as i32) ^ -((u & 1) as i32)
}

pub fn put_leb(out: &mut Vec<u8>, mut u: u32) {
    while u >= 0x80 {
        out.push(u as u8 | 0x80);
        u >>= 7;
    }
    out.push(u as u8);
}

fn leb_vals(vals: &[(NCtx, i32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vals.len() * 2);
    for &(_, v) in vals {
        put_leb(&mut out, zigzag(v));
    }
    out
}

/// Encodes a structural stream with back end `be` (None: not allowed).
pub fn encode_lazy(be: u8, syms: &[u8], ctxs: &[u32], zlevel: i32) -> Option<Vec<u8>> {
    match be {
        BE_STORE => Some(syms.to_vec()),
        BE_ZSTD => Some(crate::zst::compress(syms, zlevel)),
        BE_RANS => rans::encode_bytes(syms, ctxs),
        _ => None,
    }
}

pub fn encode_num(be: u8, sid: usize, vals: &[(NCtx, i32)], zlevel: i32) -> Option<Vec<u8>> {
    match be {
        BE_STORE => Some(leb_vals(vals)),
        BE_ZSTD => Some(crate::zst::compress(&leb_vals(vals), zlevel)),
        BE_RANS => rans::encode_num(sid, vals),
        _ => None,
    }
}

pub fn encode_blob(be: u8, data: &[u8], zlevel: i32) -> Option<Vec<u8>> {
    match be {
        BE_STORE => Some(data.to_vec()),
        BE_ZSTD => Some(crate::zst::compress(data, zlevel)),
        _ => None,
    }
}

/// Bytes of scratch memory the decoder of a stream needs beyond its body
/// (rANS models are charged when they are read).
pub fn scratch(be: u8, numeric: bool, count: usize) -> u64 {
    match (be, numeric) {
        (BE_ZSTD, false) => count as u64,
        (BE_ZSTD, true) => 5 * count as u64,
        _ => 0,
    }
}

pub fn decode_blob(be: u8, body: &[u8], n: usize) -> Result<Cow<'_, [u8]>> {
    match be {
        BE_STORE if body.len() == n => Ok(Cow::Borrowed(body)),
        BE_STORE => Err(Error::Corrupt("stored stream length")),
        BE_ZSTD => Ok(Cow::Owned(crate::zst::decompress(body, n)?)),
        _ => Err(Error::Corrupt("back end")),
    }
}

/// A plain byte cursor that reports exhaustion.
pub struct Cursor<'a> {
    pub b: Cow<'a, [u8]>,
    pub i: usize,
}

impl Cursor<'_> {
    #[inline(always)]
    pub fn get(&mut self) -> Option<u8> {
        let v = self.b.get(self.i).copied();
        self.i += 1;
        v
    }
}

/// Lazy decoder of one structural stream.
pub enum LazyDec<'a> {
    Bytes(Cursor<'a>),
    Rans(Box<rans::ByteDec<'a>>),
    Empty,
}

impl<'a> LazyDec<'a> {
    pub fn new(
        be: u8,
        sid: usize,
        body: &'a [u8],
        n: usize,
        ft: Option<&FileTables>,
        budget: &Budget,
    ) -> Result<LazyDec<'a>> {
        Ok(match be {
            BE_STORE | BE_ZSTD => LazyDec::Bytes(Cursor {
                b: decode_blob(be, body, n)?,
                i: 0,
            }),
            BE_RANS => LazyDec::Rans(Box::new(rans::ByteDec::new(body, n, budget)?)),
            BE_FILE => LazyDec::Rans(Box::new(rans::ByteDec::new_file(
                file_model(ft, sid)?,
                body,
                n,
            )?)),
            _ => return Err(Error::Corrupt("back end")),
        })
    }

    /// Next symbol; `None` once the stream is exhausted.
    #[inline(always)]
    pub fn get(&mut self, ctx: u32) -> Option<u8> {
        match self {
            LazyDec::Bytes(c) => c.get(),
            LazyDec::Rans(d) => d.get(ctx),
            LazyDec::Empty => None,
        }
    }

    pub fn done(&self) -> bool {
        match self {
            LazyDec::Bytes(c) => c.i == c.b.len(),
            LazyDec::Rans(d) => d.done(),
            LazyDec::Empty => true,
        }
    }
}

/// Lazy decoder of one numeric stream.
pub enum NumDec<'a> {
    Leb(Cursor<'a>, usize),
    Rans(Box<rans::NumDec<'a>>),
    Empty,
}

impl<'a> NumDec<'a> {
    pub fn new(
        be: u8,
        sid: usize,
        body: &'a [u8],
        n: usize,
        ft: Option<&FileTables>,
        budget: &Budget,
    ) -> Result<NumDec<'a>> {
        Ok(match be {
            BE_STORE => NumDec::Leb(
                Cursor {
                    b: Cow::Borrowed(body),
                    i: 0,
                },
                n,
            ),
            // the LEB128 length is not stored; it is at most 5 bytes a value
            BE_ZSTD => NumDec::Leb(
                Cursor {
                    b: Cow::Owned(crate::zst::decompress_upto(body, 5 * n)?),
                    i: 0,
                },
                n,
            ),
            BE_RANS => NumDec::Rans(Box::new(rans::NumDec::new(sid, body, n, budget)?)),
            BE_FILE => NumDec::Rans(Box::new(rans::NumDec::new_file(
                sid,
                file_model(ft, sid)?,
                body,
                n,
            )?)),
            _ => return Err(Error::Corrupt("back end")),
        })
    }

    #[inline(always)]
    pub fn get(&mut self, c: NCtx) -> Option<i32> {
        match self {
            NumDec::Leb(cur, left) => {
                if *left == 0 {
                    return None;
                }
                *left -= 1;
                let mut u = 0u32;
                for k in 0..5 {
                    let b = cur.get()?;
                    // a fifth byte carries the top 4 bits and ends the value
                    if k == 4 && b > 0x0f {
                        return None;
                    }
                    u |= ((b & 0x7f) as u32) << (7 * k);
                    if b < 0x80 {
                        return Some(unzigzag(u));
                    }
                }
                None
            }
            NumDec::Rans(d) => d.get(c),
            NumDec::Empty => None,
        }
    }

    pub fn done(&self) -> bool {
        match self {
            NumDec::Leb(c, left) => *left == 0 && c.i == c.b.len(),
            NumDec::Rans(d) => d.done(),
            NumDec::Empty => true,
        }
    }
}
