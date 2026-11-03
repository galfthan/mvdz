# mvdz benchmarks

Compression ratios, speeds and test campaigns of mvdz 1.0, measured on an
archive of QuakeWorld MVD demos. All figures are for the release build of
this repository unless stated. MB = 10^6 bytes of **raw demo**; speeds are
MB/s of raw demo, so a codec's speed does not depend on how well it
compresses.

## Machine

A cloud VM: 4 vCPUs of an Intel Xeon (Skylake, AVX2, no SHA extensions),
7 GB of memory, Linux. The machine ran other work during most of the
measurements (load average 1.5 to 9), so:

* every speed is the median of 3 or more runs, with [min-max];
* speeds are for an **idle core**: an independent re-run reproduced the
  library speeds within 3 % in CPU time, while its wall-clock speeds at
  load 8-9 were about 40 % lower;
* the comparisons that matter (trusted against full decode, portable
  against x86-64-v3 builds) were run interleaved on a pinned core.

The tables "Speeds" and "Identity check and trusted decoding" were taken
with a build for `target-cpu=x86-64-v3` (the repository's setting during
development). The default build is portable: "Default build" gives its
numbers (the ones the README quotes), and "Portable and x86-64-v3 builds"
compares the two.

## Samples

The archive: 50,951 gzip-compressed demos, 72.0 GB as gzip, about 112 GB
raw, from QuakeWorld servers between about 2000 and 2026, named by the
SHA-256 of the raw demo.

| sample | n | raw | how it was drawn |
|---|---|---|---|
| holdout | 200 | 462 MB | seeded shuffle of the whole archive (Python `random.Random(20261102)`), demos whose first frame is not svc_serverdata dropped |
| fresh | 100 | 252 MB | the same, independent seed (`0xF2E5B`), drawn by a reviewer |
| sample40 | 40 | 140 MB | the development sample: 27 archive demos and 13 recent hub games from a test corpus |
| stratified | 585 | 1.19 GB | stratified by era (7 classes, from `matchdate`, the date in the server info or the server version) x gz-size tercile (21 cells); half proportional, half equal allocation, at least 8 per cell; seed 20261102; weights cell size / picks, scaled to the archive |
| campaign | 110 | 239 MB | `shuf --random-source` seeded 20261102 |

The lists of every sample (the SHA-256 of each raw demo, which is the
hub's `demo_sha256`), with their seeds and, for the stratified sample, the
cells and weights, are in [`benchmarks/samples/`](benchmarks/samples).

The headline ratios use the stratified sample, weighted to the archive,
with 95 % stratified bootstrap intervals (2,000 resamples). The 300 =
holdout + fresh give 0.204 (normal) and 0.235 (fast), inside the same
sampling error (their own interval is about ±0.007).

## Ratios

Ratio = total compressed bytes / total raw bytes (byte-weighted: large
demos count for more).

| format | size / raw | size / archive gzip |
|---|---|---|
| mvdz normal | 0.211 [0.206-0.217] | 0.329 [0.320-0.338] |
| mvdz fast | 0.242 [0.237-0.249] | 0.378 [0.368-0.388] |
| mvdz normal, `-s 1M` | 0.219 | |
| mvdz fast, `-s 1M` | 0.247 | |
| archive gzip | 0.642 [0.635-0.648] | 1 |
| gzip -9 | 0.614 | |
| xz -9e (xz -9 on the 300: 0.468) | 0.473 [0.467-0.478] | |
| zstd -19 | 0.553 [0.547-0.559] | |
| zstd -3 --long | 0.612 | |

Projected onto the whole archive (72.0 GB as gzip): 23.7 [23.1-24.3] GB at
the normal level, 27.2 [26.5-27.9] GB at the fast level.

A typical demo compresses a little less well than the total: the per-demo
median is 0.231 (normal) and 0.258 (fast). Demos of a few KB can come out
larger than their gzip (7 of 585 here): the metadata is about 2-4 KB per
file. The normal level is not smaller than fast on every demo: rANS codes
each stream with static, per-context tables, while zstd also finds
repeated runs of values. On a demo that repeats long runs of identical
content, which constructed demos do, the fast level can be smaller: the
test vector `float-mvd1` (a constructed MVD1 demo with float coordinates)
is 114,304 bytes at normal and 81,017 at fast. It is not a cold start on a
small file: eight copies of the same demo in a row cost normal 2.2 bytes
per float coordinate, as one copy does, while zstd codes the copies almost
for free. On the real FTE float-coordinate vector (`float-fte`) normal is
the smaller (0.411 against 0.439 of raw). Segmenting costs about +2 to +4 % at `-s 1M` (+3.8 % normal, +1.8 %
fast; 64 KiB segments: about +29 % on sample40).

**By era** (year of the demo; byte-weighted, archive-weighted, 95 %
intervals; the 9 demos before 2007 are merged into the first group):

| era | n | normal | fast | median demo, normal (demos >= 60 s) | snapshots/s (median) | compressed B/s of game, normal (median) |
|---|---|---|---|---|---|---|
| up to 2014 | 95 | 0.232 [0.225-0.238] | 0.274 [0.265-0.280] | 0.237 | 26 | 467 |
| 2015-2022 | 124 | 0.241 [0.219-0.271] | 0.273 [0.249-0.304] | 0.241 | 27 | 682 |
| 2023-2026 | 215 | 0.183 [0.178-0.188] | 0.209 [0.203-0.215] | 0.189 | 62 | 669 |
| year unknown | 151 | 0.253 [0.248-0.258] | 0.292 [0.287-0.297] | 0.254 | 19 | 393 |
| all | 585 | 0.211 [0.206-0.217] | 0.242 [0.237-0.249] | 0.230 | - | 599 |

Snapshots/s counts the demo's time steps (blocks with a non-zero time
delta; one server frame can write several blocks), as a median over 40
demos per era. Current mvdsv records every server frame (`sv_demofps`
defaults to 77; about 62 a second in practice), while older demos mostly
hold 20-27. The compressed bytes per second of game stay about flat
(about 670 B/s from 2015 on), so the extra snapshots are nearly free: the
smaller time step makes each one easier to predict.

## Speeds

**Library, in memory** (`mvdz bench`; decode with every check, including
the SHA-256; compression including the self-check decode). "4t" is one
file at a time with 4 threads, "batch" 4 files in parallel.

| set | level | compress 1t | compress 4t | compress batch | decode 1t | decode 4t | decode batch |
|---|---|---|---|---|---|---|---|
| sample40 | normal | 6.12 [5.86-6.20] | 11.30 [11.28-11.57] | 20.68 [20.29-21.76] | 30.42 [30.22-32.76] | 46.63 [46.23-47.96] | 109.6 [100.6-116.4] |
| sample40 | fast | 2.24 [2.13-2.25] | 5.93 [5.83-6.32] | 7.00 [6.76-7.76] | 49.49 [47.68-51.80] | 68.10 [68.06-69.97] | 168.1 [144.8-181.7] |
| 300 | normal | | | | 32.50 [31.67-32.56] | 44.49 [43.95-45.69] | 109.2 [101.3-121.1] |
| 300 | fast | | | | 55.56 [54.28-56.39] | 67.74 [67.46-69.93] | 196.4 [183.9-197.7] |

**Command line**, one process per file, the decoded demo written to tmpfs
(`scripts/bench_cli.sh`), 300 demos. `mvdz decompress` writes a regular
file atomically (temporary file, fsync, rename); the others write through
a shell redirect. gzip decodes the archive's own files.

| format | size / raw (the 300) | decode, 1 process | decode, 4 processes |
|---|---|---|---|
| mvdz normal | 0.204 | 22.2 [21.6-24.9] | 51.6 [45.3-85.4] |
| mvdz fast | 0.235 | 37.5 [36.9-38.1] | 135.9 [128.6-142.8] |
| gzip -dc | 0.641 | 75.1 [71.9-75.9] | 275.7 [267.4-288.2] |
| xz -dc (xz -9) | 0.468 | 27.1 [27.1-27.4] | 96.9 [92.2-100.1] |
| zstd -dc (zstd -19) | 0.553 | 102.4 [100.9-105.2] | 335.1 [309.9-355.1] |

(The 4-process normal column is one noisy run; its median is the number to
quote.)

## Default build

The default (portable) release build, on sample40 (40 demos, 140 MB),
pinned core, load 1.5-5. These are the numbers the README quotes.

| level | compress, library, 1t | decode, library, 1t | decode, CLI, 1 process | decode, CLI, 4 processes |
|---|---|---|---|---|
| mvdz normal (0.1999 on these) | 6.02 [5.80-6.08] | 29.32 [27.72-29.56] | 23.2 [22.9-23.6] | 65.9 [62.0-67.6] |
| mvdz fast (0.2296) | 2.23 [1.87-2.35] | 46.91 [41.52-51.08] | 35.0 [33.8-35.2] | 116.0 [98.4-118.9] |
| gzip -6 (0.634) | | | 88.3 [81.0-89.6] | 305.8 [300.7-314.2] |
| xz -9 (0.472) | | | 28.2 [27.3-28.2] | 93.8 [93.0-97.0] |
| zstd -19 (0.567) | | | 118.1 [112.0-124.7] | 378.6 [349.9-398.8] |

On the same set the x86-64-v3 build gave 6.12 / 30.42 (normal) and 2.24 /
49.49 (fast) for library compress / decode (Speeds, above).

## Identity check and trusted decoding

A full decode recomputes the SHA-256 of the demo and compares it with the
identity stored in the file. On this CPU (no SHA extensions) the `sha2`
crate runs at 155-190 MB/s, which is **15-18 % of the decode time at the
normal level and 26-30 % at the fast level** (measured as the hash time
over the decode CPU time, and by A/B with the check disabled).

`--trusted` (library `ReadOptions::trusted`, C `MVDZ_TRUSTED`) skips the
SHA-256 and keeps every CRC-32C check. Library decode, one thread, 12
sample demos (43 MB), pinned core, load 2.8-3.8:

| level | full | trusted | gain |
|---|---|---|---|
| normal | 33.0 / 34.2 / 29.2 | 39.0 / 39.2 / 40.0 | about x1.2 |
| fast | 46.0 / 47.2 / 55.0 | 57.3 / 65.0 / 68.9 | x1.2-1.4 |

CRC-32C uses the SSE4.2 instruction (detected at run time) at about 3-6
GB/s: the two CRC passes are a few percent of a trusted decode.

## Portable and x86-64-v3 builds

The default build of 1.0 is portable (x86-64 baseline; CRC-32C still uses
SSE4.2 when the CPU has it, detected at run time). Building with
`RUSTFLAGS="-C target-cpu=x86-64-v3"` (or `native`) lets the compiler use
AVX2, BMI2 and LZCNT everywhere. Same machine, 8 sample demos (19.5 MB),
pinned core, runs interleaved:

| measure | portable | x86-64-v3 | difference |
|---|---|---|---|
| decode, normal, CPU time | 25.3-27.7 | 25.3-32.4 | v3 about 10-15 % faster |
| decode, fast, CPU time | 36.4-44.8 | 38.8-48.6 | v3 about 5-10 % faster |
| decode, normal, trusted, in process | 32.6-32.7 | 38.4-39.8 | v3 about 18 % faster |
| decode, fast, trusted, in process | 66.1-70.6 | 68.8-78.4 | within noise to 10 % |
| compress, normal, in process | 5.9 | 6.3-6.4 | v3 about 7 % faster |
| compress, fast, in process | 2.2 | 2.2-2.3 | none |
| SHA-256 (`sha2`) | 150-155 MB/s | 191 MB/s | v3 about 23 % faster |

The gain is spread over the whole decoder (rANS, walker, numeric
predictors, SHA-256), so it cannot be had by dispatching one function at
run time: a test that compiled the segment decoder entry point twice and
chose at run time recovered only a few percent, because most of the hot
code is called through functions that are not inlined into it. Builders
who know their CPUs can opt in with the flag above.

## Correctness campaigns

Every round trip below compares the decoded demo with the original by
SHA-256.

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
# ratios and library speeds over a set of demos
cargo build --release
target/release/mvdz bench --levels normal,fast -r 3 -j 4 demos/*.mvd
target/release/mvdz bench --levels normal,fast -r 5 -j 1 --skip-compress --trusted demos/*.mvd

# command-line decode speed against gzip, xz and zstd (one process per file)
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
