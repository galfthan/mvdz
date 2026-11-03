# Changelog

## Unreleased

* Compression takes gzip input (`.mvd.gz`, every member) and decompresses
  it first, in the library, so the CLI, the C `mvdz_compress` and wasm all
  do; the output is identical to compressing the raw demo, whose SHA-256
  is the identity. Before, the gzip bytes were stored as they were, in a
  file with no frames. Corrupt gzip is an error. Library: `is_gzip`,
  `gunzip`, `write_gzip`, `write_file_atomic_with`.
* Every encoder refuses input that is not an MVD demo (`check_demo`):
  `Error::NotMvd`, C `MVDZ_E_NOTMVD` (-8). Empty input is refused too. The
  CLI's `--force` stores any input anyway; `mvdz::encode::compress` is the
  unchecked encoder.
* `Error` is `#[non_exhaustive]`: a `match` on it needs a `_` arm, and
  later variants are not breaking changes.
* C: `mvdz_writer_finish` and `mvdz_writer_abort` free a writer handle
  whose earlier call hit an internal error (they return `MVDZ_E_PANIC`).
  Before, such a handle could never be freed.
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
