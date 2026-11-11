# Changelog

## Unreleased

* META tag 15 `matchkey`: the raw text of the first `matchkey:` print
  (older mods' match start stamp, e.g. `8-2003-6-15:16-58-0`), joined
  over the prints a server splits the line into; also in `mvdz info` and
  the census `clock`. Readers that do not know tag 15 skip it.
* META tag 16 `players` replaces tag 7 (the roster snapshot, now
  unassigned): every svc_updateuserinfo and svc_setinfo in demo order,
  verbatim, with how many come before the first svc_playerinfo. Joins,
  reconnects, drops, spectators and setinfo changes are visible without
  decoding the demo; `Metadata::roster()` and `roster` in `mvdz info`
  rebuild the old snapshot from it. Capped at 65536 changes and 1 MiB of
  strings (`truncated`). `Metadata::roster` the field is now
  `Metadata::players`. Files from earlier drafts still decode, but their
  tag 7 roster is ignored (an empty roster).
* The census is no longer part of the file (META tag 9 is unassigned):
  the header holds only values taken from the demo as they are, not an
  analysis of it. Files are about 2 KB smaller. `mvdz census` still
  reports it, from a scan of the demo (an .mvdz is decoded first, so it
  is slower on .mvdz files); the census JSON is informative and not part
  of the format. `Metadata::census` and `census` in `mvdz info` are gone.
* The time sources are raw values only: FORMAT.md §3.1 no longer defines
  how to parse `epoch` or `matchdate`, a ranking of the sources, or a
  wall-clock base. It describes what each source means and how pauses
  shift demo time, and leaves interpretation to readers. The stored bytes
  do not change.
* Attachments are removed from the format, the library, the C API (the
  `mvdz_attachment*` and `mvdz_bundle_hash*` functions, `MVDZ_E_NOTFOUND`)
  and the CLI (`mvdz attach`), along with the bundle hash.
* The trailer no longer holds `adir_off` and `body_end`: it is 33 bytes,
  and earlier files do not read. Section kinds 0x04 and 0x05 are unassigned.
* The default segment size is now 256 KiB (`Config::default`, the CLI, and
  0 for the streaming writer, which was 1 MiB). `-s 0` keeps the whole demo
  in one segment. With the 65536-segment limit, the default covers demos
  up to about 16 GiB.
* Threads now work only across segments, in one-shot compress and
  decompress and in training the shared models; a segment is coded and
  decoded on one thread. The output bytes do not change.
* Breaking: the `threads` argument is gone from `Reader::new`,
  `Reader::open`, `decompress_stream`, `api::reader`, `mvdz_open_memory`,
  `mvdz_open_callbacks` and `mvdz_writer_open`.

## 1.0.0-rc.4

Segmented files at the normal level share one set of rANS models (`-s 256K`
7 % smaller, `-s 1M` 3 %), demos that record player inputs compress to
about 0.10 of raw (42 % smaller), and normal-level compression is about a
third faster.

**Format change.** rc.3 cannot decode files written by this version with
`-s` at the normal level (section `TABLES` is unknown to it and critical)
or files holding usercmd blocks; this version cannot decode rc.3 files
that hold usercmd blocks. Such files are refused, not decoded wrongly.
Other files are unchanged in both directions.

* **Shared models for segmented files** (format addition: section
  `TABLES`, kind 0x86, and back end 3; FORMAT.md §2.6, §8.4, §12 rule 11).
  `compress -s` at the normal level builds one model per rANS stream from
  that stream's symbols in every segment, stores the models once, before
  the first segment, and every segment codes those streams with them.
  Segments stay independently decodable. On the stratified sample
  (archive-weighted) `-s 256K` is 0.218 of raw instead of 0.235 and `-s 1M`
  0.213 instead of 0.219 (1 MiB segments now cost 0.9 % over one segment,
  not 3.8 %); full-size 4on4 demos shrink by 14 % at `-s 64K`, 9 % at
  `-s 256K` and 4.6 % at `-s 1M`. Whole files, the fast level and the
  streaming writer do not use TABLES. Readers only check the section when
  they open a file; the models are built once, under the reader's
  budget, when the first segment that uses them is decoded, so metadata
  and attachment tools never build them. At `-s 256K`, one thread, on
  set8: compression 6.1 to 8.0 MB/s (no model is built per segment),
  decoding 22.8 to 25.5 MB/s. The models are trained from per-context
  counts summed over the segments; compressing a 14.2 MB 4on4 demo with
  one thread peaks at 96 MB at `-s 256K` and 94 MB at `-s 1M` (87 MB
  without shared models, 133 MB for one segment). Attachment updates now
  read the file under the default limits.

* **usercmd blocks coded field by field** (format change: streams `uc`
  and `uc.a`; FORMAT.md §6.11, §7.6). mvdsv can record every player
  input as a hidden block (type 1, 23 bytes: `SV_DebugClientCommand`);
  in demos that do, these blocks are about 60 %
  of the raw demo, and rc.3 stored them as zstd. Their fields now go to
  their own stream with the same player's previous value as context, and
  the view angles, which are 16-bit values sent as floats, are coded as
  residuals against a linear prediction. On four 4on4 demos that record
  usercmds (36 MB raw each) the files shrink by 42 % at normal (6.25 to
  3.61 MB, 0.100 of raw) and 34 % at fast (7.27 to 4.80 MB); normal
  compression is about twice as fast, decoding about 25 % slower. A
  type-1, 23-byte hidden block is now always coded so: an rc.3 file
  holding one in `hid.misc` no longer decodes.

* **Faster normal-level compression** (encoder only, output
  byte-identical), by vikpe
  ([#29](https://github.com/galfthan/mvdz/pull/29)): the rANS encoder
  builds its models with dense arrays instead of a hash lookup per
  symbol. On set8 (BENCHMARKS.md), release download, interleaved with the
  build before: one thread 5.95 to 8.1 MB/s (+36 %), 4 files in parallel
  17.7 to 22.2 (+25 %), one file on 4 threads 10.9 to 11.7 (+7 %). On
  the four usercmd demos, one thread, 6.8 to 9.3 MB/s. A streaming writer
  with 1 MiB segments now costs about 0.15 s per MiB of demo at the
  normal level (0.08 s at fast).

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
