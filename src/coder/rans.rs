//! Tier F: two-pass static rANS with per-context frequency tables.
//!
//! * Symbols: bytes for structural streams (context = the walker's context,
//!   which fits in 16 bits, the table key), or the sign/bit-length bucket of
//!   a residual for numeric streams (context from [`num_ctx`]); numeric
//!   mantissa bits go raw into a separate bit stream (pcodec-style bins +
//!   offsets).
//! * Tables: the encoder counts symbols per context and gives a context its
//!   own table only where that beats the shared fallback table by more than
//!   the table's cost. Counts are sent log-quantised (`steps` per octave)
//!   with an adaptive binary coder and normalised to 2^SCALE identically on
//!   both sides (integer arithmetic only).
//! * Coder: 32-bit rANS, 16-bit renormalisation, interleaved states (after
//!   Giesen, "Interleaved entropy coders", 2014; written fresh).
//!   Single-symbol tables cost nothing and are skipped.

use super::rc::{Dec as RcDec, Enc as RcEnc, IntModel};
use crate::budget::{Budget, Charge, with_capacity};
use crate::error::{Error, Result};
use crate::numeric::NCtx;
use crate::streams::*;
use crate::util::{Rd, put_uv};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::{Arc, OnceLock};

pub const SCALE: u32 = 12;
const T: u32 = 1 << SCALE;
const L: u32 = 1 << 16;
const NSTATE: usize = 4;

/// 2^(k/4) in 16.16 fixed point, k = 0..3.
const POW4: [u64; 4] = [65536, 77936, 92682, 110218];

/// Reconstructed weight (16.16 fixed point) of a log-quantised count.
#[inline]
fn qval(q: u32, steps: u32) -> u64 {
    if q == 0 {
        return 0;
    }
    let e = q - 1;
    let (o, k) = (e / steps, e % steps);
    let frac = POW4[(k * 4 / steps) as usize];
    if o >= 40 { u64::MAX >> 8 } else { frac << o }
}

/// Log-quantised count: the q whose weight is nearest to c in ratio.
fn qlog(c: u32, steps: u32) -> u32 {
    if c == 0 {
        return 0;
    }
    let x = (c as u64) << 16;
    let o = 31 - c.leading_zeros();
    let mut e = o * steps;
    for k in 0..steps {
        if qval(1 + o * steps + k, steps) <= x {
            e = o * steps + k;
        }
    }
    let lo = qval(1 + e, steps);
    let hi = qval(2 + e, steps);
    // compare x/lo with hi/x  <=>  x*x vs lo*hi
    if (x as u128) * (x as u128) > (lo as u128) * (hi as u128) {
        2 + e
    } else {
        1 + e
    }
}

/// Normalises weights to a total of exactly T, every present symbol >= 1.
fn normalize(w: &[u64]) -> Vec<u32> {
    let tot: u128 = w.iter().map(|&x| x as u128).sum();
    let mut f = vec![0u32; w.len()];
    if tot == 0 {
        return f;
    }
    let mut sum: i64 = 0;
    for (i, &x) in w.iter().enumerate() {
        if x > 0 {
            f[i] = ((x as u128 * T as u128 / tot) as u32).max(1);
            sum += f[i] as i64;
        }
    }
    let mut diff = T as i64 - sum;
    while diff != 0 {
        // adjust the largest entry (first one on ties)
        let mut bi = 0;
        for i in 0..f.len() {
            if f[i] > f[bi] {
                bi = i;
            }
        }
        if diff > 0 {
            f[bi] += diff as u32;
            diff = 0;
        } else {
            let take = (-diff).min(f[bi] as i64 - 1).min(((f[bi] as i64) + 1) / 2);
            if take <= 0 {
                break;
            }
            f[bi] -= take as u32;
            diff += take;
        }
    }
    f
}

#[derive(Clone)]
struct Table {
    q: Vec<u32>,
    freq: Vec<u32>,
    cum: Vec<u32>,
    det: Option<u16>,
}

impl Table {
    fn from_q(q: Vec<u32>, steps: u32) -> Table {
        let w: Vec<u64> = q.iter().map(|&x| qval(x, steps)).collect();
        let freq = normalize(&w);
        let mut cum = vec![0u32; freq.len() + 1];
        for i in 0..freq.len() {
            cum[i + 1] = cum[i] + freq[i];
        }
        let present: Vec<usize> = (0..freq.len()).filter(|&i| freq[i] > 0).collect();
        let det = if present.len() == 1 {
            Some(present[0] as u16)
        } else {
            None
        };
        Table { q, freq, cum, det }
    }
}

fn entropy_bits(c: &[u32]) -> f64 {
    let n: u64 = c.iter().map(|&x| x as u64).sum();
    if n == 0 {
        return 0.0;
    }
    let ln = (n as f64).log2();
    c.iter()
        .filter(|&&x| x > 0)
        .map(|&x| x as f64 * (ln - (x as f64).log2()))
        .sum()
}

fn cross_bits(c: &[u32], p: &[u32]) -> f64 {
    let n: u64 = p.iter().map(|&x| x as u64).sum();
    let ln = (n.max(1) as f64).log2();
    let mut b = 0.0;
    for (i, &x) in c.iter().enumerate() {
        if x > 0 {
            if p[i] == 0 {
                return f64::INFINITY;
            }
            b += x as f64 * (ln - (p[i] as f64).log2());
        }
    }
    b
}

/// Hasher of the encoder's context maps (16-bit keys; one multiply). The
/// maps are only looked up, never iterated in hash order, so the output
/// does not depend on it.
#[derive(Default)]
struct CtxHash(u64);

impl Hasher for CtxHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.write_u32(x as u32);
        }
    }
    fn write_u32(&mut self, i: u32) {
        let h = (self.0 ^ i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        self.0 = h ^ (h >> 32);
    }
}

type CtxMap<V> = HashMap<u32, V, BuildHasherDefault<CtxHash>>;

struct Model {
    alpha: usize,
    steps: u32,
    ctx_of: CtxMap<usize>,
    tables: Vec<Table>,
}

pub static TABLE_BITS_PER_SYM: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(5);
const TABLE_BITS_BASE: f64 = 16.0;

/// The contexts with their own table and their counts, and the fallback
/// table's counts: everything of a model but the quantisation.
struct Sel {
    /// (context, counts), in context order
    own: Vec<(u32, Vec<u32>)>,
    rest: Vec<u32>,
}

fn select(syms: &[(u32, u16)], alpha: usize) -> Sel {
    let per_sym = TABLE_BITS_PER_SYM.load(std::sync::atomic::Ordering::Relaxed) as f64;
    let mut counts: CtxMap<Vec<u32>> = CtxMap::default();
    let mut global = vec![0u32; alpha];
    for &(c, s) in syms {
        counts.entry(c).or_insert_with(|| vec![0; alpha])[s as usize] += 1;
        global[s as usize] += 1;
    }
    let mut keys: Vec<u32> = counts.keys().copied().collect();
    keys.sort_by_key(|&k| (std::cmp::Reverse(counts[&k].iter().sum::<u32>()), k));
    let mut own: Vec<u32> = Vec::new();
    let mut fb = global.clone();
    for pass in 0..2 {
        own.clear();
        let mut rest = vec![0u32; alpha];
        for &k in &keys {
            let c = &counts[&k];
            let present = c.iter().filter(|&&x| x > 0).count() as f64;
            let own_cost = entropy_bits(c) + TABLE_BITS_BASE + per_sym * present;
            if own_cost < cross_bits(c, &fb) {
                own.push(k);
            } else {
                for i in 0..alpha {
                    rest[i] += c[i];
                }
            }
        }
        if pass == 0 {
            if rest.iter().all(|&x| x == 0) {
                break;
            }
            fb = rest;
        }
    }
    own.sort();
    let mut rest = vec![0u32; alpha];
    for &k in &keys {
        if own.binary_search(&k).is_err() {
            for i in 0..alpha {
                rest[i] += counts[&k][i];
            }
        }
    }
    let own = own
        .into_iter()
        .map(|k| {
            let c = counts.remove(&k).expect("counted context");
            (k, c)
        })
        .collect();
    Sel { own, rest }
}

fn model_of(sel: &Sel, alpha: usize, steps: u32) -> Model {
    let q = |c: &[u32]| -> Vec<u32> { c.iter().map(|&x| qlog(x, steps)).collect() };
    let mut tables = vec![Table::from_q(q(&sel.rest), steps)];
    let mut ctx_of = CtxMap::default();
    for (i, (k, c)) in sel.own.iter().enumerate() {
        tables.push(Table::from_q(q(c), steps));
        ctx_of.insert(*k, i + 1);
    }
    Model {
        alpha,
        steps,
        ctx_of,
        tables,
    }
}

// --------------------------------------------------------------- model I/O

const NQC: usize = 17 * 18;

fn write_table(e: &mut RcEnc, m: &mut IntModel, t: &[u32], fb: Option<&[u32]>) {
    let mut prev = 0u32;
    for (i, &q) in t.iter().enumerate() {
        let f = fb.map(|f| f[i].min(16)).unwrap_or(17) as usize;
        m.put(e, f * 17 + prev.min(16) as usize, q);
        prev = q;
    }
}

fn read_table(d: &mut RcDec, m: &mut IntModel, alpha: usize, fb: Option<&[u32]>) -> Vec<u32> {
    let mut t = vec![0u32; alpha];
    let mut prev = 0u32;
    for i in 0..alpha {
        let f = fb.map(|f| f[i].min(16)).unwrap_or(17) as usize;
        t[i] = m.get(d, f * 17 + prev.min(16) as usize).min(160);
        prev = t[i];
    }
    t
}

/// The serialized model header and the binary decisions it takes.
fn write_model(m: &Model) -> (Vec<u8>, u64) {
    let mut e = RcEnc::new();
    let mut im = IntModel::new(NQC + 8);
    im.put(&mut e, NQC, m.alpha as u32);
    im.put(&mut e, NQC + 1, m.steps);
    im.put(&mut e, NQC + 2, (m.tables.len() - 1) as u32);
    let fb = m.tables[0].q.clone();
    write_table(&mut e, &mut im, &fb, None);
    let mut own: Vec<(u32, usize)> = m.ctx_of.iter().map(|(&k, &v)| (k, v)).collect();
    own.sort();
    let mut prevk = 0u32;
    for (i, (k, ti)) in own.into_iter().enumerate() {
        im.put(&mut e, NQC + 3, if i == 0 { k } else { k - prevk });
        prevk = k;
        write_table(&mut e, &mut im, &m.tables[ti].q, Some(&fb));
    }
    let n = e.decisions;
    (e.finish(), n)
}

/// Tables with at most this many symbols are decoded by searching their
/// cumulative frequencies (2 bytes per symbol); wider ones use a slot LUT.
const SEARCH_MAX: usize = 64;

// Context-map entries: an offset into `pool` (search table), or a LUT
// offset with E_LUT, or a deterministic symbol with E_DET, or empty.
const E_DET: u32 = 1 << 31;
const E_LUT: u32 = 1 << 30;
const E_EMPTY: u32 = E_DET | E_LUT;

/// Format limit on the binary decisions a model header may take:
/// `DEC_PER_BYTE * header length + DEC_BASE` (FORMAT.md, rule M2).
pub const DEC_PER_BYTE: u64 = 128;
pub const DEC_BASE: u64 = 1024;

/// Decoder-side model. A search table at pool offset o is
/// `[0, c1, .., cn = T, pad..]` (16*chunks + 1 entries) followed by its
/// 16*chunks symbols, so one table is one or two cache lines.
pub struct DModel {
    map: Vec<u32>,
    fb: u32,
    chunks: usize,
    pool: Vec<u16>,
    luts: Vec<u32>,
    _charge: Option<Charge>,
}

/// Number of entries of `c[0..16*chunks]` that are <= slot.
#[inline(always)]
fn count_le(c: &[u16], chunks: usize, slot: u32) -> usize {
    debug_assert!(c.len() >= 16 * chunks);
    #[cfg(all(target_arch = "x86_64", not(feature = "scalar")))]
    {
        use std::arch::x86_64::*;
        // SAFETY: SSE2 is part of x86_64; the loads stay within
        // c[..16 * chunks]: every caller passes a search table of
        // 2 * 16 * chunks entries (read_model).
        unsafe {
            let sv = _mm_set1_epi16(slot as i16);
            let mut gt = 0u32;
            let p = c.as_ptr() as *const __m128i;
            for i in 0..chunks {
                let a = _mm_loadu_si128(p.add(2 * i));
                let b = _mm_loadu_si128(p.add(2 * i + 1));
                let m = (_mm_movemask_epi8(_mm_cmpgt_epi16(a, sv)) as u32)
                    | (_mm_movemask_epi8(_mm_cmpgt_epi16(b, sv)) as u32) << 16;
                gt += m.count_ones();
            }
            16 * chunks - (gt / 2) as usize
        }
    }
    #[cfg(any(not(target_arch = "x86_64"), feature = "scalar"))]
    {
        c[..16 * chunks]
            .iter()
            .filter(|&&x| x as u32 <= slot)
            .count()
    }
}

/// Reads a model header (FORMAT.md, "Model header"). `alpha` is the
/// alphabet the stream kind requires; `count` the stream's symbol count.
pub(crate) fn read_model(
    b: &[u8],
    alpha_req: usize,
    count: usize,
    budget: &Budget,
) -> Result<DModel> {
    let bad = Error::Corrupt("model");
    let max_dec = (b.len() as u64)
        .saturating_mul(DEC_PER_BYTE)
        .saturating_add(DEC_BASE);
    let mut d = RcDec::new(b);
    let mut im = IntModel::new(NQC + 8);
    let alpha = im.get(&mut d, NQC) as usize;
    let steps = im.get(&mut d, NQC + 1);
    let n = im.get(&mut d, NQC + 2) as usize;
    if alpha != alpha_req || !(1..=4).contains(&steps) || n > 65535 || n > count {
        return Err(bad);
    }
    // the log-quantised tables (u8: q <= 160) until the layout is known
    let _tc = budget.alloc(((n + 1) * alpha + 4 * (n + 1)) as u64)?;
    let mut qs: Vec<u8> = with_capacity((n + 1) * alpha)?;
    let mut keys: Vec<u32> = with_capacity(n)?;
    let fbq = read_table(&mut d, &mut im, alpha, None);
    qs.extend(fbq.iter().map(|&q| q as u8));
    let mut k = 0u32;
    for i in 0..n {
        if d.decisions > max_dec {
            return Err(bad);
        }
        let dk = im.get(&mut d, NQC + 3);
        if i > 0 && dk == 0 {
            return Err(bad);
        }
        k = k.checked_add(dk).ok_or(bad)?;
        if k > 0xffff {
            return Err(bad);
        }
        keys.push(k);
        let t = read_table(&mut d, &mut im, alpha, Some(&fbq));
        qs.extend(t.iter().map(|&q| q as u8));
        if d.overrun() {
            return Err(bad);
        }
    }
    // the header is consumed exactly: the decoder reads one byte per
    // byte the encoder wrote (5 at the start, then one per renormalisation)
    if d.pos() != b.len() || d.decisions > max_dec {
        return Err(bad);
    }
    budget.work(d.decisions)?;
    // a symbol is present (frequency >= 1) exactly when its q is non-zero
    // (`normalize`), so the layout is known before any table is built;
    // the tables are then built one at a time
    let present = |q: &[u8]| q.iter().filter(|&&x| x > 0).count();
    // one chunk count for every search table of the model: a fixed trip
    // count keeps the search loop free of mispredicted exits
    let chunks = qs
        .chunks(alpha)
        .map(present)
        .filter(|&n| n > 1 && n <= SEARCH_MAX)
        .max()
        .unwrap_or(1)
        .div_ceil(16);
    let (mut npool, mut nlut) = (0usize, 0usize);
    for q in qs.chunks(alpha) {
        let p = present(q);
        if p > SEARCH_MAX {
            nlut += T as usize;
        } else if p > 1 {
            npool += 2 * 16 * chunks + 1;
        }
    }
    let maxk = keys.last().map_or(0, |&k| k as usize + 1);
    let charge = budget.alloc((npool * 2 + nlut * 4 + maxk * 4) as u64)?;
    let mut dm = DModel {
        map: with_capacity(maxk)?,
        fb: E_EMPTY,
        chunks,
        pool: with_capacity(npool)?,
        luts: with_capacity(nlut)?,
        _charge: Some(charge),
    };
    let mut ents = with_capacity(n + 1)?;
    for q in qs.chunks(alpha) {
        let t = Table::from_q(q.iter().map(|&x| x as u32).collect(), steps);
        let present: Vec<usize> = (0..alpha).filter(|&s| t.freq[s] > 0).collect();
        let e = if present.is_empty() {
            E_EMPTY
        } else if let Some(d) = t.det {
            E_DET | d as u32
        } else if present.len() <= SEARCH_MAX {
            let off = dm.pool.len() as u32;
            dm.pool.push(0);
            for &s in &present {
                dm.pool.push(t.cum[s + 1] as u16);
            }
            for _ in present.len()..16 * chunks {
                dm.pool.push(0x7fff);
            }
            for &s in &present {
                dm.pool.push(s as u16);
            }
            for _ in present.len()..16 * chunks {
                dm.pool.push(0);
            }
            off
        } else {
            let off = dm.luts.len() as u32;
            dm.luts.resize(dm.luts.len() + T as usize, 0);
            let lut = &mut dm.luts[off as usize..];
            for &s in &present {
                for j in t.cum[s]..t.cum[s + 1] {
                    lut[j as usize] = s as u32 | t.freq[s] << 8 | (j - t.cum[s]) << 20;
                }
            }
            E_LUT | off
        };
        ents.push(e);
    }
    if dm.pool.len() >= E_LUT as usize || dm.luts.len() >= E_LUT as usize {
        return Err(Error::Limit("model size"));
    }
    dm.fb = ents[0];
    dm.map.resize(maxk, dm.fb);
    for (i, &k) in keys.iter().enumerate() {
        dm.map[k as usize] = ents[i + 1];
    }
    Ok(dm)
}

impl DModel {
    /// The model of an absent stream (no symbol can be decoded), shared.
    fn empty() -> Arc<DModel> {
        static E: OnceLock<Arc<DModel>> = OnceLock::new();
        E.get_or_init(|| {
            Arc::new(DModel {
                map: Vec::new(),
                fb: E_EMPTY,
                chunks: 1,
                pool: Vec::new(),
                luts: Vec::new(),
                _charge: None,
            })
        })
        .clone()
    }
    #[inline(always)]
    fn entry(&self, ctx: u32) -> u32 {
        let c = ctx as usize;
        if c < self.map.len() {
            self.map[c]
        } else {
            self.fb
        }
    }
    /// Decodes one symbol for map entry `e`; `None` for an empty table.
    #[inline(always)]
    fn decode(&self, d: &mut RDec, e: u32) -> Option<u32> {
        if e & E_DET != 0 {
            return if e & E_LUT != 0 {
                None
            } else {
                Some(e & 0xffff)
            };
        }
        let j = d.j;
        d.j = (j + 1) % NSTATE;
        let x = d.x[j];
        let slot = x & (T - 1);
        let (sym, mut x) = if e & E_LUT == 0 {
            let m = 16 * self.chunks;
            // SAFETY: every search entry was built pointing at a table of
            // 2*m + 1 pool entries (read_model), and entries are never
            // changed afterwards.
            let t = unsafe { self.pool.get_unchecked(e as usize..e as usize + 2 * m + 1) };
            let k = count_le(&t[1..], self.chunks, slot);
            let lo = t[k] as u32;
            let hi = t[k + 1] as u32;
            // wrapping: an initial state from a hostile file may overflow
            // (the result is then garbage that the stream's checks refuse)
            (
                t[m + 1 + k] as u32,
                (hi - lo).wrapping_mul(x >> SCALE).wrapping_add(slot - lo),
            )
        } else {
            let le = self.luts[(e & !E_LUT) as usize + slot as usize];
            (
                le & 0xff,
                ((le >> 8) & 0xfff)
                    .wrapping_mul(x >> SCALE)
                    .wrapping_add(le >> 20),
            )
        };
        if x < L {
            x = (x << 16) | d.word() as u32;
        }
        d.x[j] = x;
        Some(sym)
    }
}

// --------------------------------------------------------------- rANS core

struct REnc {
    x: [u32; NSTATE],
    words: Vec<u16>,
}

impl REnc {
    fn new() -> REnc {
        REnc {
            x: [L; NSTATE],
            words: Vec::new(),
        }
    }
    #[inline]
    fn put(&mut self, j: usize, freq: u32, cum: u32) {
        let mut x = self.x[j];
        let xmax = ((L >> SCALE) << 16) * freq;
        if x >= xmax {
            self.words.push(x as u16);
            x >>= 16;
        }
        self.x[j] = ((x / freq) << SCALE) + (x % freq) + cum;
    }
    fn finish(mut self) -> Vec<u8> {
        for j in (0..NSTATE).rev() {
            let x = self.x[j];
            self.words.push(x as u16);
            self.words.push((x >> 16) as u16);
        }
        self.words.reverse();
        let mut out = Vec::with_capacity(self.words.len() * 2);
        for w in self.words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }
}

struct RDec<'a> {
    x: [u32; NSTATE],
    b: &'a [u8],
    pos: usize,
    j: usize,
}

impl<'a> RDec<'a> {
    fn new(b: &'a [u8]) -> RDec<'a> {
        let mut d = RDec {
            x: [0; NSTATE],
            b,
            pos: 0,
            j: 0,
        };
        for j in 0..NSTATE {
            let hi = d.word() as u32;
            let lo = d.word() as u32;
            d.x[j] = hi << 16 | lo;
        }
        d
    }
    #[inline(always)]
    fn word(&mut self) -> u16 {
        let v = match self.b.get(self.pos..self.pos + 2) {
            Some(s) => u16::from_le_bytes([s[0], s[1]]),
            None => 0,
        };
        self.pos += 2;
        v
    }
    /// The decoder of an absent stream (no symbol can be decoded).
    fn finished_empty() -> RDec<'static> {
        RDec {
            x: [L; NSTATE],
            b: &[],
            pos: 0,
            j: 0,
        }
    }
    /// Every word read and every state back at its initial value.
    fn finished(&self) -> bool {
        self.pos == self.b.len() && self.x.iter().all(|&x| x == L)
    }
}

/// The table of each symbol's context.
fn table_ix(model: &Model, syms: &[(u32, u16)]) -> Vec<usize> {
    syms.iter()
        .map(|(c, _)| model.ctx_of.get(c).copied().unwrap_or(0))
        .collect()
}

fn rans_encode(model: &Model, syms: &[(u32, u16)]) -> Vec<u8> {
    rans_encode_ix(model, syms, &table_ix(model, syms))
}

fn rans_encode_ix(model: &Model, syms: &[(u32, u16)], tix: &[usize]) -> Vec<u8> {
    let mut coded = 0usize;
    let mut state_of = vec![0u8; syms.len()];
    for (i, &ti) in tix.iter().enumerate() {
        if model.tables[ti].det.is_none() {
            state_of[i] = (coded % NSTATE) as u8;
            coded += 1;
        }
    }
    let mut e = REnc::new();
    for i in (0..syms.len()).rev() {
        let t = &model.tables[tix[i]];
        if t.det.is_some() {
            continue;
        }
        let s = syms[i].1 as usize;
        e.put(state_of[i] as usize, t.freq[s], t.cum[s]);
    }
    e.finish()
}

/// `hdr_len uv | hdr | rans_len uv | rans | extra`, or None if the model
/// header breaks the decision limit (rule M2).
fn pack(model: &Model, syms: &[(u32, u16)], extra: &[u8]) -> Option<Vec<u8>> {
    let (hdr, decisions) = write_model(model);
    if decisions > (hdr.len() as u64) * DEC_PER_BYTE + DEC_BASE {
        return None;
    }
    let body = rans_encode(model, syms);
    let mut out = Vec::with_capacity(hdr.len() + body.len() + extra.len() + 8);
    put_uv(&mut out, hdr.len() as u64);
    out.extend_from_slice(&hdr);
    put_uv(&mut out, body.len() as u64);
    out.extend_from_slice(&body);
    out.extend_from_slice(extra);
    Some(out)
}

struct Unpacked<'a> {
    model: Arc<DModel>,
    rans: &'a [u8],
    extra: &'a [u8],
}

fn unpack<'a>(b: &'a [u8], alpha: usize, count: usize, budget: &Budget) -> Result<Unpacked<'a>> {
    let bad = Error::Corrupt("rans stream");
    let mut r = Rd::new(b);
    let hdr = r.bytes().ok_or(bad)?;
    let rans = r.bytes().ok_or(bad)?;
    Ok(Unpacked {
        model: Arc::new(read_model(hdr, alpha, count, budget)?),
        rans,
        extra: r.rest(),
    })
}

/// The smaller of the two quantisations, or None if neither model is
/// allowed (the caller then uses another back end).
fn best_pack(syms: &[(u32, u16)], alpha: usize, extra: &[u8]) -> Option<Vec<u8>> {
    let mut best: Option<Vec<u8>> = None;
    let sel = select(syms, alpha);
    for steps in [2u32, 4] {
        let m = model_of(&sel, alpha, steps);
        if let Some(p) = pack(&m, syms, extra) {
            if best.as_ref().is_none_or(|b| p.len() < b.len()) {
                best = Some(p);
            }
        }
    }
    best
}

// --------------------------------------------------------------- file tables

/// A model shared by the segments of a file (the TABLES section, back end
/// 3): built from one stream's symbols over every segment, its header
/// stored once per file.
pub struct Shared {
    model: Model,
    hdr: Vec<u8>,
}

impl Shared {
    /// The model header, as the TABLES section stores it.
    pub fn hdr(&self) -> &[u8] {
        &self.hdr
    }
}

/// The smaller of the two quantisations for `syms` (header plus coded
/// symbols), or None if no model is allowed (rules M1 and M2).
fn shared_from(syms: &[(u32, u16)], alpha: usize) -> Option<Shared> {
    let mut best: Option<(usize, Shared)> = None;
    let sel = select(syms, alpha);
    for steps in [2u32, 4] {
        let model = model_of(&sel, alpha, steps);
        if model.tables.len() > 65536 {
            continue;
        }
        let (hdr, decisions) = write_model(&model);
        if decisions > (hdr.len() as u64) * DEC_PER_BYTE + DEC_BASE {
            continue;
        }
        let cost = hdr.len() + rans_encode(&model, syms).len();
        if best.as_ref().is_none_or(|b| cost < b.0) {
            best = Some((cost, Shared { model, hdr }));
        }
    }
    best.map(|b| b.1)
}

/// The shared model of a structural stream from its segments' symbols and
/// contexts.
pub fn shared_bytes(segs: &[(&[u8], &[u32])]) -> Option<Shared> {
    let mut v = Vec::with_capacity(segs.iter().map(|s| s.0.len()).sum());
    for (syms, ctxs) in segs {
        v.extend(
            syms.iter()
                .zip(*ctxs)
                .map(|(&s, &c)| (byte_ctx(c), s as u16)),
        );
    }
    shared_from(&v, 256)
}

/// The shared model of numeric stream `sid` from its segments' values.
pub fn shared_num(sid: usize, segs: &[&[(NCtx, i32)]]) -> Option<Shared> {
    let mut v = Vec::with_capacity(segs.iter().map(|s| s.len()).sum());
    for vals in segs {
        v.extend(vals.iter().map(|(c, x)| (num_ctx(sid, c), bucket(*x).0)));
    }
    shared_from(&v, NALPHA)
}

/// `rans_len uv | rans | extra` with the shared model, or None if a symbol
/// has no frequency in it.
fn pack_shared(sh: &Shared, syms: &[(u32, u16)], extra: &[u8]) -> Option<Vec<u8>> {
    let m = &sh.model;
    let tix = table_ix(m, syms);
    for (&(_, s), &ti) in syms.iter().zip(&tix) {
        if m.tables[ti].freq[s as usize] == 0 {
            return None;
        }
    }
    let body = rans_encode_ix(m, syms, &tix);
    let mut out = Vec::with_capacity(body.len() + extra.len() + 4);
    put_uv(&mut out, body.len() as u64);
    out.extend_from_slice(&body);
    out.extend_from_slice(extra);
    Some(out)
}

/// A structural stream coded with the file's model (back end 3).
pub fn encode_bytes_shared(sh: &Shared, syms: &[u8], ctxs: &[u32]) -> Option<Vec<u8>> {
    pack_shared(sh, &byte_syms(syms, ctxs), &[])
}

/// A numeric stream coded with the file's model (back end 3).
pub fn encode_num_shared(sh: &Shared, sid: usize, vals: &[(NCtx, i32)]) -> Option<Vec<u8>> {
    let (syms, extra) = num_syms(sid, vals);
    pack_shared(sh, &syms, &extra)
}

/// The decoder models of a file's TABLES section, read once per file and
/// shared by every segment (FORMAT.md §2.6).
pub struct FileTables {
    m: Vec<Option<Arc<DModel>>>,
}

impl FileTables {
    /// Reads a TABLES section body: `n uv | n x { sid uv | len uv |
    /// model }`, ids strictly increasing, each model consumed exactly.
    pub fn parse(b: &[u8], budget: &Budget) -> Result<FileTables> {
        let e = Error::Corrupt("tables");
        let mut r = Rd::new(b);
        let n = r.uz().ok_or(e)?;
        if n == 0 || n > N_STREAMS {
            return Err(e);
        }
        let mut m: Vec<Option<Arc<DModel>>> = (0..N_STREAMS).map(|_| None).collect();
        let mut prev: Option<usize> = None;
        for _ in 0..n {
            let sid = r.uz().ok_or(e)?;
            if sid >= N_STREAMS || prev.is_some_and(|p| sid <= p) {
                return Err(e);
            }
            let alpha = if is_lazy(sid) {
                256
            } else if is_num(sid) {
                NALPHA
            } else {
                return Err(e);
            };
            let hdr = r.bytes().ok_or(e)?;
            m[sid] = Some(Arc::new(read_model(hdr, alpha, 65535, budget)?));
            prev = Some(sid);
        }
        if !r.done() {
            return Err(e);
        }
        Ok(FileTables { m })
    }

    /// The shared model of stream `sid`, if the section has one.
    pub fn get(&self, sid: usize) -> Option<&Arc<DModel>> {
        self.m.get(sid)?.as_ref()
    }
}

/// `rans_len uv | rans | extra` of a back end 3 body.
fn unpack_file<'a>(m: &Arc<DModel>, b: &'a [u8]) -> Result<Unpacked<'a>> {
    let mut r = Rd::new(b);
    let rans = r.bytes().ok_or(Error::Corrupt("rans stream"))?;
    Ok(Unpacked {
        model: m.clone(),
        rans,
        extra: r.rest(),
    })
}

// --------------------------------------------------------------- byte streams

#[inline(always)]
fn byte_ctx(c: u32) -> u32 {
    c & 0xffff
}

fn byte_syms(syms: &[u8], ctxs: &[u32]) -> Vec<(u32, u16)> {
    syms.iter()
        .zip(ctxs)
        .map(|(&s, &c)| (byte_ctx(c), s as u16))
        .collect()
}

pub fn encode_bytes(syms: &[u8], ctxs: &[u32]) -> Option<Vec<u8>> {
    best_pack(&byte_syms(syms, ctxs), 256, &[])
}

pub struct ByteDec<'a> {
    m: Arc<DModel>,
    d: RDec<'a>,
    n: usize,
    used: usize,
}

impl<'a> ByteDec<'a> {
    pub fn empty() -> Self {
        ByteDec {
            m: DModel::empty(),
            d: RDec::finished_empty(),
            n: 0,
            used: 0,
        }
    }
    pub fn new(b: &'a [u8], n: usize, budget: &Budget) -> Result<Self> {
        Self::from(unpack(b, 256, n, budget)?, n)
    }
    /// A back end 3 stream: the model is the file's (TABLES).
    pub fn new_file(m: &Arc<DModel>, b: &'a [u8], n: usize) -> Result<Self> {
        Self::from(unpack_file(m, b)?, n)
    }
    fn from(u: Unpacked<'a>, n: usize) -> Result<Self> {
        if !u.extra.is_empty() {
            return Err(Error::Corrupt("rans stream"));
        }
        Ok(ByteDec {
            m: u.model,
            d: RDec::new(u.rans),
            n,
            used: 0,
        })
    }
    #[inline(always)]
    pub fn get(&mut self, ctx: u32) -> Option<u8> {
        self.used += 1;
        let e = self.m.entry(byte_ctx(ctx));
        self.m.decode(&mut self.d, e).map(|v| v as u8)
    }
    /// Exactly the declared count read and the coded data consumed.
    pub fn done(&self) -> bool {
        self.used == self.n && self.d.finished()
    }
}

// --------------------------------------------------------------- numeric streams

pub const NALPHA: usize = 65;

/// Static-table context of a numeric value, per stream.
#[inline(always)]
pub fn num_ctx(sid: usize, c: &NCtx) -> u32 {
    let f = &c.f;
    let cl = |v: u8, m: u8| v.min(m) as u32;
    match sid {
        PI_OX..=50 => cl(f[0], 19) | cl(f[1], 19) << 5,
        PI_AP..=53 => cl(f[0], 23) | cl(f[1], 23) << 5,
        PE_OX..=56 => cl(f[0], 17) | cl(f[1], 17) << 5 | ((f[3] & 1) as u32) << 10,
        SND_O | TE_O => f[0] as u32 | (f[1] as u32) << 4,
        STL_VAL => f[0] as u32 | cl(f[1], 15) << 8,
        UC_A => cl(f[0], 23) | cl(f[1], 23) << 5 | cl(f[2], 15) << 10 | (f[3] as u32) << 14,
        _ => cl(f[0], 63) | (f[1] as u32) << 6,
    }
}

#[inline(always)]
fn bucket(v: i32) -> (u16, u32, u32) {
    let m = v.unsigned_abs();
    if m == 0 {
        return (0, 0, 0);
    }
    let n = 32 - m.leading_zeros();
    let b = 2 * n - 1 + (v < 0) as u32;
    (b as u16, n - 1, m & ((1u32 << (n - 1)) - 1))
}

#[derive(Default)]
pub struct BitW {
    acc: u64,
    n: u32,
    out: Vec<u8>,
}

impl BitW {
    #[inline]
    pub fn put(&mut self, v: u32, n: u32) {
        self.acc |= (v as u64) << self.n;
        self.n += n;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    pub fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

pub struct BitR<'a> {
    b: &'a [u8],
    pos: usize,
    acc: u64,
    n: u32,
    /// bits handed out so far
    used: u64,
}

impl<'a> BitR<'a> {
    pub fn new(b: &'a [u8]) -> BitR<'a> {
        BitR {
            b,
            pos: 0,
            acc: 0,
            n: 0,
            used: 0,
        }
    }
    #[inline(always)]
    fn refill(&mut self) {
        if self.pos + 8 <= self.b.len() {
            let w = u64::from_le_bytes(self.b[self.pos..self.pos + 8].try_into().unwrap());
            self.acc |= w << self.n;
            let take = (63 - self.n) / 8;
            self.pos += take as usize;
            self.n += take * 8;
        } else {
            while self.n <= 56 {
                let v = self.b.get(self.pos).copied().unwrap_or(0) as u64;
                self.acc |= v << self.n;
                self.pos += 1;
                self.n += 8;
            }
        }
    }
    /// `n` bits (n <= 31), least significant first; zeros past the end.
    #[inline(always)]
    pub fn get(&mut self, n: u32) -> u32 {
        if self.n < n {
            self.refill();
        }
        let v = (self.acc & ((1u64 << n) - 1)) as u32;
        self.acc >>= n;
        self.n -= n;
        self.used += n as u64;
        v
    }
    /// The bits read end within the last byte (at most 7 padding bits).
    pub fn finished(&self) -> bool {
        self.used.div_ceil(8) == self.b.len() as u64
    }
}

/// Bucket symbols and mantissa bits of numeric values.
fn num_syms(sid: usize, vals: &[(NCtx, i32)]) -> (Vec<(u32, u16)>, Vec<u8>) {
    let mut syms = Vec::with_capacity(vals.len());
    let mut bw = BitW::default();
    for (c, v) in vals {
        let (b, n, m) = bucket(*v);
        syms.push((num_ctx(sid, c), b));
        bw.put(m, n);
    }
    (syms, bw.finish())
}

pub fn encode_num(sid: usize, vals: &[(NCtx, i32)]) -> Option<Vec<u8>> {
    let (syms, extra) = num_syms(sid, vals);
    best_pack(&syms, NALPHA, &extra)
}

pub struct NumDec<'a> {
    sid: usize,
    m: Arc<DModel>,
    d: RDec<'a>,
    bits: BitR<'a>,
    n: usize,
    used: usize,
}

impl<'a> NumDec<'a> {
    pub fn empty() -> Self {
        NumDec {
            sid: 0,
            m: DModel::empty(),
            d: RDec::finished_empty(),
            bits: BitR::new(&[]),
            n: 0,
            used: 0,
        }
    }
    pub fn new(sid: usize, b: &'a [u8], n: usize, budget: &Budget) -> Result<Self> {
        Ok(Self::from(sid, unpack(b, NALPHA, n, budget)?, n))
    }
    /// A back end 3 stream: the model is the file's (TABLES).
    pub fn new_file(sid: usize, m: &Arc<DModel>, b: &'a [u8], n: usize) -> Result<Self> {
        Ok(Self::from(sid, unpack_file(m, b)?, n))
    }
    fn from(sid: usize, u: Unpacked<'a>, n: usize) -> Self {
        NumDec {
            sid,
            m: u.model,
            d: RDec::new(u.rans),
            bits: BitR::new(u.extra),
            n,
            used: 0,
        }
    }
    #[inline(always)]
    pub fn get(&mut self, c: NCtx) -> Option<i32> {
        self.used += 1;
        let e = self.m.entry(num_ctx(self.sid, &c));
        let b = self.m.decode(&mut self.d, e)?;
        if b > 64 {
            return None;
        }
        // branchless: b = 0 reads no bits and gives 0
        let n = (b + 1) / 2;
        let nb = n.saturating_sub(1);
        let lead = ((n > 0) as u32) << nb;
        let m = (lead | self.bits.get(nb)) as i32;
        Some(if b & 1 == 0 { m.wrapping_neg() } else { m })
    }
    /// Exactly the declared count read and every byte consumed.
    pub fn done(&self) -> bool {
        self.used == self.n && self.d.finished() && self.bits.finished()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_round_trip_and_limits() {
        // three contexts with different distributions: own tables
        let ctxs: Vec<u32> = (0..5000u32).map(|i| i % 3).collect();
        let syms: Vec<u8> = (0..5000u32)
            .map(|i| {
                [(i * 7 % 13) as u8, 100 + (i % 5) as u8, 200 + (i % 2) as u8][(i % 3) as usize]
            })
            .collect();
        let b = encode_bytes(&syms, &ctxs).unwrap();
        let budget = Budget::unlimited();
        let mut d = ByteDec::new(&b, syms.len(), &budget).unwrap();
        for (i, &s) in syms.iter().enumerate() {
            assert_eq!(d.get(ctxs[i]), Some(s));
        }
        assert!(d.done());
        // the alphabet of a numeric stream is refused for a byte stream
        assert!(NumDec::new(PI_OX, &b, syms.len(), &budget).is_err());
        // more tables than symbols (rule M1)
        assert!(ByteDec::new(&b, 1, &budget).is_err());
        // a body cut short is refused; a symbol short does not finish
        assert!(ByteDec::new(&b[..b.len() - 2], syms.len(), &budget).is_err());
        let mut d = ByteDec::new(&b, syms.len(), &budget).unwrap();
        for i in 0..syms.len() - 1 {
            d.get(ctxs[i]);
        }
        assert!(!d.done());
    }

    #[test]
    fn numeric_round_trip() {
        let vals: Vec<(NCtx, i32)> = (0..3000i32)
            .map(|i| {
                (
                    NCtx {
                        f: [(i % 5) as u8, 0, 0, 0, 0, 0],
                    },
                    (i * 37) % 2001 - 1000,
                )
            })
            .collect();
        let b = encode_num(PI_OX, &vals).unwrap();
        let budget = Budget::unlimited();
        let mut d = NumDec::new(PI_OX, &b, vals.len(), &budget).unwrap();
        for &(c, v) in &vals {
            assert_eq!(d.get(c), Some(v));
        }
        assert!(d.done());
    }
}
