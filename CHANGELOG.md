# Changelog

## Unreleased

* **Faster normal-level compression** (encoder only; output byte for byte
  the same). The encoder's context maps hash their 16-bit keys with one
  multiply instead of SipHash, and the contexts that get their own table
  are selected once per stream for both quantisations (`steps` 2 and 4)
  instead of once each. On set8 (8 demos, 18.9 MB raw, one thread,
  median of 5 interleaved runs) normal compression goes from 6.15 to
  7.79 MB/s (about 21 % less time); the fast level and decoding are
  unchanged.

## 1.0.0-rc.3

The normal level codes five structural contexts as the spec meant them
(0.15 % smaller), the fast level compresses about 6 times faster (zstd
level 6; files about 5 % larger), and the release downloads use mimalloc
(up to 40 % faster than with musl's allocator).

**Format change.** Files written by rc.1 and rc.2 at the normal level do
not decode with this version, and files written by it do not decode with
rc.1 or rc.2. Such a file is refused by the integrity checks, not decoded
wrongly: an rc.2 file is reported as `corrupt: structure` (the walk reads
different symbols and the stream checks fail; any wrong output would also
fail the segment CRC-32C). Fast-level and untransformed files (no rans
streams) read unchanged. The version byte stays 1.

* Five structural contexts kept information above bit 15, which the rans
  back end drops (it keys tables by `ctx & 0xFFFF`, FORMAT.md §8.3). Every
  structural context now fits in 16 bits, as coded:
  * CMD: after a command of class 15 ("other"), the context names that
    command, `0x8000 | prev_cmd | (tyi << 8) | (min(idx, 7) << 11)`;
    before, every such command shared one context. Other classes keep the
    old low 16 bits.
  * ST_IDX: `st_prev_idx | (prev_cmd << 8)` (was `<< 16`, lost).
  * PI_FLAGS: the main symbol's context is `0x8000 | (prev & 0x7fff)`, the
    previous flags word (bit 15 aside); before, only 7 of its bits reached
    the table.
  * PI_FRAME: `ps.frame | (flags & 0x300)`, DF_DEAD and DF_GIB at bits 8-9
    (were at 16-17, lost).
  * PI_NUM: `pi_prev_num`; the `prev_cmd << 16` term was always lost and
    carries no information here, so it is dropped (same tables).

  On a 200-demo holdout (462 MB raw) the normal level is 148,128 bytes
  (0.154 %) smaller: cmd -2.9 %, pi.flags -1.5 %, pi.frame -1.5 %, st.idx
  -2.2 %. Decode speed is unchanged; the fast level is byte-identical.
  The encoder and decoder debug-assert that every structural context fits
  in 16 bits. FORMAT.md §6.5, §6.6, §6.7 and §6.12 give the new contexts
  and their bit layouts; the Go conformance decoder and the test vectors
  follow.

* zstd levels (encoder only: files of every level read with existing
  decoders). The fast level compresses its streams, untransformed segments
  and META at zstd level 6 instead of 19, so it is fast both ways: on the
  200-demo holdout, one thread, compression runs at 15.0 MB/s instead of
  2.2, for a file 5.3 % larger (ratio 0.2535 instead of 0.2406); decoding
  is as fast. The normal level uses 17 instead of 19 for its zstd streams,
  untransformed segments and META: compression 6.2 MB/s instead of 5.5,
  0.05 % larger. Attachments are compressed at level 1 first and stored
  when that does not save more than 1/32; otherwise level 19 and the 1/16
  rule as before. Already-compressed media no longer costs a level-19 pass
  (about 0.2 s/MB). Data whose only redundancy is beyond level 1's window
  (512 KiB) is now stored where level 19 would have compressed it. The
  levels are `segment.rs:ZSTD_NORMAL`, `ZSTD_FAST`, `ZSTD_ATTACH_PROBE` and
  `ZSTD_ATTACH` (FORMAT.md §12, rule 10); `Metadata::encode_for` takes the
  level. The test vectors that hold zstd data at those levels are
  regenerated.
* The release downloads use mimalloc as the allocator (new cargo feature
  `mimalloc`, off by default; the release workflow turns it on). musl's
  own allocator made the static builds 15-40 % slower than a glibc build,
  and multi-threaded decoding barely faster than one thread; with mimalloc
  the baseline download decodes the normal level at 27 MB/s on one thread
  (was 23) and 34 MB/s on four (was 24).
* BENCHMARKS.md is re-measured for this version: the ratios on the
  stratified sample (fast level now 0.255 of raw, 28.6 GB for the
  archive), and every speed on 8 demos, 5 interleaved rounds per build,
  for the release downloads and the glibc builds.
* Test: `truncated_demo_round_trips_and_is_flagged` expects a one-byte cut
  to be refused as a non-demo (since rc.2); it failed when run with
  `MVDZ_TEST_DATA`.

## 1.0.0-rc.2

No change to the file format: rc.1 files read unchanged, and rc.2 writes
the same bytes as rc.1 for the same raw demo. The main changes: `.mvd.gz`
is compressed directly, input that is not a demo is refused, and
`compress` and `decompress` name their outputs after their inputs, like
zstd (an existing output now needs `-f` to be overwritten).

* Compression takes gzip input (`.mvd.gz`, every member) and decompresses
  it first, in the library, so the CLI, the C `mvdz_compress` and wasm all
  do; the output is identical to compressing the raw demo, whose SHA-256
  is the identity. Before, the gzip bytes were stored as they were, in a
  file with no frames. Corrupt gzip is an error. Library: `is_gzip`,
  `gunzip`, `write_gzip`, `write_file_atomic_with`.
* Every encoder refuses input that is not an MVD demo (`check_demo`):
  `Error::NotMvd`, C `MVDZ_E_NOTMVD` (-8). Empty input is refused too. The
  CLI's `--allow-non-demo` stores any input anyway; `mvdz::encode::compress` is the
  unchecked encoder.
* `Error` is `#[non_exhaustive]`: a `match` on it needs a `_` arm, and
  later variants are not breaking changes.
* C: `mvdz_writer_finish` and `mvdz_writer_abort` free a writer handle
  whose earlier call hit an internal error (they return `MVDZ_E_PANIC`).
  Before, such a handle could never be freed.
* `mvdz compress` and `mvdz decompress` work like zstd: `mvdz compress
  *.mvd.gz` writes `game.mvdz` beside each `game.mvd.gz` (or `game.mvd`),
  `mvdz decompress *.mvdz` writes `game.mvd` (`game.mvd.gz` with `--gzip`),
  several files at once with `-j`. Inputs are kept (`--rm` deletes them),
  `-o` names the output, and the two-name form `in out` still works.
  **Changed:** an existing output is no longer overwritten without `-f`.
* `mvdz decompress --gzip` (or an output named `*.gz`, unless `--no-gzip`)
  writes the demo gzip-compressed.
* `mvdz census` reports `demoinfo` for raw demos (it was always false for
  `.mvd`), and reads `.mvd.gz`.
* The command-line tool exits quietly when standard output is closed (a
  pipe into `head`) instead of panicking.
* C interface: `mvdz_writer_set_ext` sets extension metadata on a
  streaming writer (Rust: `Writer::set_ext`). `mvdz_reader` and
  `mvdz_writer` are distinct incomplete struct types instead of `void`
  (same ABI; passing one for the other no longer compiles silently). The
  header documents the static strings, the cost of `mvdz_writer_write`,
  that finish and abort always free the handle, and threading.
* Documentation: what reads `.mvdz` today, when to segment (`-s 1M`), a
  FAQ, exact labels for the gzip and xz rows of the ratio table, the
  benchmark sample lists (`benchmarks/samples/`), and when the fast level
  can be smaller than normal.
* FORMAT.md: the META ordering rule is in §5.3, not §5.1 (#8).

## 1.0.0-rc.1

First public release: a release candidate, for comments. The file format
(version 1) may still change before 1.0; files written by a release
candidate are not guaranteed to be readable by 1.0.

* The MVDZ file format, version 1 ([FORMAT.md](FORMAT.md)): a section
  container with metadata (identity SHA-256, server info, roster, census,
  wall-clock sources and pauses), a seek index, independently decodable
  segments, attachments and a trailer; the structural transform of MVD
  demos with static rANS (normal level) or zstd (fast level) per stream.
* Integrity: CRC-32C per segment and over the whole demo; the SHA-256
  identity verified by every full decode unless the caller trusts the
  input. A bundle hash over the identity and the attachments.
* Readers: whole-file, sequential (pipes, sockets, VFS callbacks) and
  random access with seeking by demo time; all apply the same validation
  rules, bound memory and work by the input, and take a caller budget.
* Writers: one-shot and streaming (record straight to `.mvdz`); every
  output is checked by decoding it.
* Attachments: add, replace, remove (crash-safe, concurrent updaters
  serialised on POSIX), list and extract without decoding, export a copy
  with a selection by name.
* Interfaces: the `mvdz` command-line tool, the Rust library, a C ABI
  (`include/mvdz.h`, static and shared library), WebAssembly builds.
* Test vectors, a conformance decoder written from the specification in Go
  (`interop/specdec`), fuzz targets.
