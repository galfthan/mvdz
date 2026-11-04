# mvdz benchmarks

Compression ratios, speeds and test campaigns of mvdz 1.0 (1.0.0-rc.3),
measured on an archive of QuakeWorld MVD demos. MB = 10^6 bytes of **raw
demo**; speeds are MB/s of raw demo, so a codec's speed does not depend on
how well it compresses.

## Machine

A virtual machine (KVM) on a shared host: 4 vCPUs of an Intel Xeon
(Skylake; AVX2 and AVX-512, no SHA extensions), 7 GB of memory, Linux.
Other tenants share the host's cores, so speeds vary by a few percent from
run to run and are given as the median over rounds, with [min-max]:

* the speed tables use **set8**, 8 demos (18.9 MB raw; Samples), so that
  every build can be measured several times in a few minutes;
* 5 rounds, in each of which every build (and, for the command line, every
  codec) runs once, in an order rotated per round, so a busy spell on the
  host spreads over all of them;
* each run starts only when the machine is idle (under 20 % of the 4 cores
  busy over 5 s; the background services alone use 10-13 %).

The speeds quoted by the README are those of the **release downloads**
(Builds, below).

## Samples

The archive: 50,951 gzip-compressed demos, 72.0 GB as gzip, about 112 GB
raw, from QuakeWorld servers between about 2000 and 2026, named by the
SHA-256 of the raw demo.

| sample | n | raw | how it was drawn |
|---|---|---|---|
| holdout | 200 | 462 MB | seeded shuffle of the whole archive (Python `random.Random(20261102)`), demos whose first frame is not svc_serverdata dropped |
| fresh | 100 | 252 MB | the same, independent seed (`0xF2E5B`), drawn by a reviewer |
| sample40 | 40 | 140 MB | the development sample: 27 archive demos and 13 recent hub games from a test corpus |
| set8 | 8 | 18.9 MB | 8 demos of sample40 spread over its sizes (0.65 to 6.2 MB): the speed set |
| stratified | 585 | 1.19 GB | stratified by era (7 classes, from `matchdate`, the date in the server info or the server version) x gz-size tercile (21 cells); half proportional, half equal allocation, at least 8 per cell; seed 20261102; weights cell size / picks, scaled to the archive |
| campaign | 110 | 239 MB | `shuf --random-source` seeded 20261102 |

The lists of every sample (the SHA-256 of each raw demo, which is the
hub's `demo_sha256`), with their seeds and, for the stratified sample, the
cells and weights, are in [`benchmarks/samples/`](benchmarks/samples).

The headline ratios use the stratified sample, weighted to the archive,
with 95 % stratified bootstrap intervals (2,000 resamples). The 300 =
holdout + fresh give 0.204 (normal) and 0.248 (fast), inside the same
sampling error (their own interval is about ±0.007).

## Ratios

Ratio = total compressed bytes / total raw bytes (byte-weighted: large
demos count for more).

| format | size / raw | size / archive gzip |
|---|---|---|
| mvdz normal | 0.211 [0.206-0.216] | 0.328 [0.320-0.338] |
| mvdz fast | 0.255 [0.250-0.261] | 0.397 [0.388-0.407] |
| mvdz normal, `-s 1M` | 0.213 | |
| mvdz fast, `-s 1M` | 0.259 | |
| archive gzip | 0.642 [0.635-0.648] | 1 |
| gzip -9 | 0.614 | |
| xz -9e (xz -9 on the 300: 0.468) | 0.473 [0.467-0.478] | |
| zstd -19 | 0.553 [0.547-0.559] | |
| zstd -3 --long | 0.612 | |

Projected onto the whole archive (72.0 GB as gzip): 23.7 [23.0-24.3] GB at
the normal level, 28.6 [28.0-29.3] GB at the fast level.

The fast level uses zstd level 6 (normal: 17), which keeps compression
fast: at level 19, as in rc.2, the fast level was 0.242 [0.237-0.249] of
raw but compressed at about 2.2 MB/s, slower than the normal level. Four
demos of this sample carry a text prefix before the demo; compress refuses
them unless asked (`--allow-non-demo`, used here, as the earlier figures
were made before that check existed).

A typical demo compresses a little less well than the total: the per-demo
median is 0.231 (normal) and 0.269 (fast). Demos of a few KB can come out
larger than their gzip (6 of 585 here): the metadata is about 2-4 KB per
file. The normal level is not smaller than fast on every demo: rANS codes
each stream with static, per-context tables, while zstd also finds
repeated runs of values. On a demo that repeats long runs of identical
content, which constructed demos do, the fast level can be smaller: the
test vector `float-mvd1` (a constructed MVD1 demo with float coordinates)
is 114,248 bytes at normal and 84,017 at fast. It is not a cold start on a
small file: eight copies of the same demo in a row cost normal 2.2 bytes
per float coordinate, as one copy does, while zstd codes the copies almost
for free. On the real FTE float-coordinate vector (`float-fte`) normal is
the smaller (0.411 against 0.450 of raw). Segmenting costs about +1 to +2 %
at `-s 1M` (+0.9 % normal, +1.6 % fast). At the normal level the segments
of a one-shot file share one model per rANS stream, stored once (the
TABLES section, FORMAT.md §2.6); with a model in every segment, as rc.3
wrote them and as the streaming writer still does, `-s 1M` cost +3.8 %
(0.219). On set8 plus three full-size 4on4 demos (58.9 MB raw) shared
models make `-s 1M` files 3.9 % smaller (0.201 to 0.193 of raw; the
4on4s 4.6 %) and 64 KiB files 13.9 % smaller (0.249 to 0.214), for a
TABLES section of 12 KB (set8) to 32 KB (the 4on4s) per file at 1 MiB.
At `-s 1M`, one thread (median of 3 interleaved rounds, idle machine):
library decode 23.9 to 26.6 MB/s and compression 5.3 to 5.0 MB/s on those
11 demos; a 14 MB 4on4 compresses in 2.75 s (2.78 s before) with a peak
of 167 MB (118 MB before; one segment: 207 MB).

**By era** (year of the demo; byte-weighted, archive-weighted, 95 %
intervals; the 9 demos before 2007 are merged into the first group):

| era | n | normal | fast | median demo, normal (demos >= 60 s) | snapshots/s (median) | compressed B/s of game, normal (median) |
|---|---|---|---|---|---|---|
| up to 2014 | 95 | 0.232 [0.225-0.238] | 0.286 [0.277-0.292] | 0.237 | 26 | 467 |
| 2015-2022 | 124 | 0.241 [0.219-0.271] | 0.285 [0.261-0.316] | 0.241 | 27 | 682 |
| 2023-2026 | 215 | 0.183 [0.178-0.188] | 0.222 [0.216-0.228] | 0.189 | 62 | 669 |
| year unknown | 151 | 0.252 [0.248-0.257] | 0.305 [0.300-0.310] | 0.254 | 19 | 393 |
| all | 585 | 0.211 [0.206-0.216] | 0.255 [0.250-0.261] | 0.230 | - | 599 |

Snapshots/s counts the demo's time steps (blocks with a non-zero time
delta; one server frame can write several blocks), as a median over 40
demos per era. Current mvdsv records every server frame (`sv_demofps`
defaults to 77; about 62 a second in practice), while older demos mostly
hold 20-27. The compressed bytes per second of game stay about flat
(about 670 B/s from 2015 on), so the extra snapshots are nearly free: the
smaller time step makes each one easier to predict. (The last three
columns are of the normal level as measured for rc.2; rc.3 changes its
size by 0.15 %.)

**Demos with usercmd blocks.** mvdsv can record every player input as a
hidden block (type 1, 23 bytes, `SV_DebugClientCommand`; about 77 a
second per player). Such demos are rare in the archive so far and are not
in the sample above. On four 4on4 demos that record them (schloss, e1m2,
dm3, dm2 of 2026-10-02; 36.1 MB raw each, the blocks 60 % of it), the
blocks are coded field by field (FORMAT.md §6.11, flag `SF_UCMD`):

| | size per demo | / raw | / gzip |
|---|---|---|---|
| raw | 36.08 MB | 1 | 2.50 |
| gzip (the hub's `.mvd.gz`) | 14.46 MB | 0.401 | 1 |
| normal, rc.3 (blocks as zstd) | 6.25 MB | 0.173 | 0.433 |
| normal | 3.61 MB | 0.100 | 0.250 |
| fast, rc.3 | 7.27 MB | 0.202 | 0.503 |
| fast | 4.80 MB | 0.133 | 0.332 |

Library speed on these four, one thread (MB/s of raw demo, median of 3
interleaved rounds, rc.3 release build against this version, both with
mimalloc): normal compress 2.8 to 6.8, decode 42.6 to 32.4; fast compress
15.1 to 15.5, decode 56.6 to 53.2. Normal compression gets faster because
zstd 17 on the block bytes was its slow part; decoding gets slower because
each block is now about 15 coded symbols.

## Speeds

The release download (`mvdz-<version>-x86_64-unknown-linux-musl`, a
static musl build with mimalloc; Builds), set8 (normal level 0.212 of raw
on these, fast 0.255).

**Library, in memory** (`mvdz bench`; decode with every check, including
the SHA-256; compression including the self-check decode). "4t" is one
file at a time with 4 threads, "batch" 4 files in parallel, one thread
each.

| level | compress 1t | compress 4t | compress batch | decode 1t | decode 4t | decode batch |
|---|---|---|---|---|---|---|
| normal | 5.8 [5.3-6.0] | 10.8 [10.2-10.9] | 17.2 [15.4-18.2] | 26.8 [25.5-27.5] | 33.5 [29.8-35.8] | 71 [63-83] |
| fast | 12.8 [11.1-13.3] | 17.8 [16.8-18.5] | 39.2 [28.8-40.6] | 44.2 [39.6-45.3] | 47.8 [44.9-51.7] | 110 [96-121] |

**Command line**, `mvdz decompress`, one process per file, the decoded
demo written to tmpfs. `mvdz decompress` writes a regular file atomically
(temporary file, fsync, rename); the others write through a shell
redirect.

| format | size / raw (set8) | decode, release download | decode, -v3 download |
|---|---|---|---|
| mvdz normal | 0.212 | 21.8 [20.1-24.8] | 24.8 [23.8-27.7] |
| mvdz fast | 0.255 | 34.3 [29.4-35.0] | 36.7 [35.0-38.8] |
| gzip -6 (`gzip -dc`) | 0.611 | 78.9 [76.6-86.4] | |
| xz -9 (`xz -dc`) | 0.473 | 27.0 [26.2-28.4] | |
| zstd -19 (`zstd -dc`) | 0.550 | 104 [71-117] | |

## Builds

The release downloads are static musl builds (they run on any Linux),
built with mimalloc as the allocator (the cargo feature `mimalloc`): musl's
own allocator is slow, above all when several threads allocate, and costs
these builds 15-40 % against a glibc build; with mimalloc they come close.
The `-v3` download is built for `target-cpu=x86-64-v3` (AVX2, BMI2,
LZCNT; any x86-64 CPU from about 2015 on). `cargo build --release` gives a
glibc build with the system allocator, portable unless `RUSTFLAGS="-C
target-cpu=native"` is set. Speeds on set8 (library, or CLI as marked):

| measure | musl | musl -v3 | **release** (musl, mimalloc) | **release -v3** | glibc (`cargo build`) | glibc, native |
|---|---|---|---|---|---|---|
| compress, normal, 1t | 4.5 | 4.7 | 5.8 | 6.0 | 6.1 | 6.1 |
| compress, normal, batch | 11.7 | 12.8 | 17.2 | 17.6 | 18.0 | 18.8 |
| decode, normal, 1t | 22.9 | 25.5 | 26.8 | 31.8 | 27.2 | 30.6 |
| decode, normal, 4t | 23.7 | 25.5 | 33.5 | 39.4 | 38.0 | 40.3 |
| decode, normal, batch | 56 | 64 | 71 | 82 | 84 | 87 |
| decode, normal, trusted, 1t | 27.4 | 27.6 | 34.2 | 39.1 | 34.9 | 36.0 |
| compress, fast, 1t | 11.5 | 12.1 | 12.8 | 13.8 | 15.4 | 15.7 |
| compress, fast, batch | 30.8 | 32.5 | 39.2 | 36.4 | 42.6 | 43.9 |
| decode, fast, 1t | 38.8 | 42.4 | 44.2 | 51.3 | 50.1 | 53.6 |
| decode, fast, 4t | 42.0 | 49.0 | 47.8 | 55.9 | 57.3 | 61.4 |
| decode, fast, batch | 94 | 107 | 110 | 125 | 134 | 143 |
| decode, fast, trusted, 1t | 56.9 | 59.7 | 67.4 | 71.8 | 76.5 | 69.3 |
| CLI decode, normal | 18.1 | 20.8 | 23.1 | 25.5 | 23.3 | 23.7 |
| CLI decode, fast | 33.8 | 34.9 | 35.0 | 40.7 | 36.7 | 36.6 |

(Medians of 5 rounds; the ranges, about ±5-10 %, are in the raw results.)

* mimalloc brings the musl build within about 5 % of glibc at one thread
  at the normal level, and restores multi-threaded decoding (4 threads:
  23.7 to 33.5 MB/s; glibc 38.0). At the fast level, mostly zstd, it stays
  10-20 % behind glibc, probably in musl's C library itself (`memcpy`)
  rather than the allocator.
* x86-64-v3 is about 10-20 % faster at decoding (a little less for
  compression). The gain is spread over the whole decoder (rANS, walker,
  numeric predictors, SHA-256), so it cannot be had by dispatching one
  function at run time: a test that compiled the segment decoder entry
  point twice and chose at run time recovered only a few percent, because
  most of the hot code is called through functions that are not inlined
  into it.
* `native` on this AVX-512 CPU gains about as much over the portable
  glibc build as x86-64-v3 does over the portable musl one.

## Identity check and trusted decoding

A full decode recomputes the SHA-256 of the demo and compares it with the
identity stored in the file. On this CPU (no SHA extensions) the `sha2`
crate runs at about 150-190 MB/s (rc.2 measurement), which is about 20 %
of the decode time at the normal level and 30 % at the fast level.

`--trusted` (library `ReadOptions::trusted`, C `MVDZ_TRUSTED`) skips the
SHA-256 and keeps every CRC-32C check. Library decode, one thread, set8:

| level | release: full | trusted | gain | release -v3: full | trusted | gain |
|---|---|---|---|---|---|---|
| normal | 26.8 | 34.2 | x1.28 | 31.8 | 39.1 | x1.23 |
| fast | 44.2 | 67.4 | x1.52 | 51.3 | 71.8 | x1.40 |

CRC-32C uses the SSE4.2 instruction (detected at run time) at about 3-6
GB/s: the two CRC passes are a few percent of a trusted decode.

## Correctness campaigns

Every round trip below compares the decoded demo with the original by
SHA-256. The campaigns up to the pure-Rust row ran on rc.2; the format of
rc.3 differs in the contexts of five structural streams (FORMAT.md
§6.5-6.7), checked by the last two rows, the test vectors, the Go
conformance decoder and fuzzing.

| campaign | demos | configurations | round trips | failures |
|---|---|---|---|---|
| development sweep (`examples/sweep.rs`): normal and fast x whole and segmented x one-shot and streamed, decoded by the whole-file and the sequential reader | 340 (sample40, holdout, fresh) | 8 | 5,440 | 0 |
| correctness review, library: whole and 64 KiB segments, streamed in irregular chunks, decoded with 1 and 3 threads, sequential and random-access readers, seeks by segment and by time | 1,320+ archive demos | many | 1,320+ files | 0 |
| correctness review, CLI: file to file and through pipes | 1,961 archive demos | 2 of 4 per demo | 1,961 | 0 |
| correctness review, structure-aware random demos | 30,000 generated | random level and segment size, verified and unverified | 90,000 | 0 |
| correctness review, every prefix and 5,716 mutations of a real demo | 25,717 inputs | 3 | 77,151 | 0 |
| benchmark review | 585 (stratified) | normal, fast, both at `-s 1M` | 2,340 | 0 |
| post-review | 110 random | normal, fast, both at `-s 256K`, each decoded fully and trusted | 880 | 0 |
| pure-Rust zstd decoder (as on wasm) | 40 | 2 | 80 | 0 |
| rc.3 (contexts, zstd levels): holdout | 200 | normal, fast | 400 | 0 |
| rc.3: stratified, compression self-check | 585 | normal, fast, both at `-s 1M` | 2,340 | 0 |
| shared tables: stratified, compression self-check | 585 | normal `-s 1M` | 585 | 0 |
| shared tables: set8 and three 4on4 demos, decoded by the library and the Go conformance decoder | 11 | normal at `-s 64K` and `-s 1M` | 44 | 0 |

No segment of a real demo fell back to raw storage in any campaign.

## Fuzzing and Miri

cargo-fuzz with AddressSanitizer, `-rss_limit_mb=1536`:

* development: decode, stream_reader, read_metadata, attachments,
  stream_writer, 15 minutes each, plus a second round with inputs of at
  most 8 KB;
* robustness review: the same targets and a new C-interface target, 35-40
  minutes each;
* post-review: all 8 targets (also capi, compress_rt and the
  structure-aware gen_rt), 4 minutes each, and the attachment and C
  targets again after the 1.0 restructure.

No crash, leak, out-of-memory or timeout in any run. Miri (`cargo +nightly
miri test --lib`) passes the round-trip, rANS, numeric, CRC and threading
tests (the pipelined decoder's raw-pointer writer, `par_map`).

## Reproducing

The demos used here are public QuakeWorld demos; any collection of `.mvd`
files works the same way. A demo hub that serves demos by their SHA-256
(as the QuakeWorld hub's CDN does) makes a sample reproducible from a list
of hashes: [`benchmarks/samples/`](benchmarks/samples) holds the lists
used here.

```
# ratios and library speeds over a set of demos (as the release builds:
# --features mimalloc, and --target x86_64-unknown-linux-musl)
cargo build --release
target/release/mvdz bench --levels normal,fast -r 1 -j 4 demos/*.mvd
target/release/mvdz bench --levels normal,fast -r 1 -j 1 --skip-compress --trusted demos/*.mvd

# command-line decode speed against gzip, xz and zstd (one process per
# file); for the speed tables each command ran once per round, 5 rounds
ls demos/*.mvd > list.txt
scripts/bench_cli.sh /tmp/bench list.txt

# round trips: both levels, whole and segmented, one-shot and streamed
cargo run --release --example sweep -- -s 1M demos/*.mvd

# the tests, with real demos where they help (see README, Tests)
MVDZ_TEST_DATA=/path/to/testdata cargo test --release
```

For the confidence intervals, draw the sample at random from the whole
collection (stratify by era and size if the collection is skewed), weight
each demo by how many it stands for, and bootstrap within the strata.
