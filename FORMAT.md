# MVDZ file format, version 1

**Status: release candidate (1.0.0-rc.1), for comments.** The format may
still change before 1.0: files written by a release candidate are not
guaranteed to be readable by 1.0. The version byte stays 1.

This document specifies the MVDZ format precisely enough to write an
independent decoder (and, with §12 and Appendix A, an encoder whose files
the reference accepts). The reference implementation is the Rust crate in
this repository; sections cite the source functions they describe
(`file:function`) so a porter can cross-check. Test vectors are in
`tests/vectors/` (see `MANIFEST.tsv`): every valid vector must decode to the
recorded SHA-256 and every hostile vector must be refused; a decoder should
reproduce all of them. The manifest records the reference's error for each
hostile vector; another decoder must refuse the file, and should report
the same category (§9, "Errors"), but its message text is its own.

An MVDZ file holds one QuakeWorld MVD demo, losslessly: a section container
(metadata, a seek index, attachments, a trailer) whose segments carry the
demo, each either transformed (a structural walk of the demo's frames and
`svc_*` commands into streams, numeric fields coded as residuals against
predictors, every stream entropy-coded) or stored as zstd or raw bytes.

**Identity, integrity, bundle.** Three different hashes, for three jobs:

* *Identity*: a demo is identified by the SHA-256 of the original `.mvd`
  bytes (the hub's `demo_sha256`), never by a hash of the `.mvdz` file: the
  `.mvdz` bytes change when attachments are added, with the segment size or
  the level, and between encoder versions. The metadata stores this hash
  (§3, tag 1); a full decode recomputes and checks it, by default.
* *Integrity*: a CRC-32C of every segment's decoded bytes (§5.1) and of the
  whole demo (§4) detects damage (bit rot, truncation, a bad copy), also
  for a decode that starts in the middle (§4, seeking). A reader that
  trusts its input (a cache that verified the SHA-256 once, at ingest) may
  skip the SHA-256 and rely on the CRCs alone (the reference's *trusted*
  option); it then detects damage but not a deliberately forged file
  (§14).
* *Bundle*: a SHA-256 over the identity and the attachments' names, MIME
  types, SHA-256s and keys (§10.2), the same however the file is
  compressed: a download ETag and a basis for signatures.

**Versions.** The byte after the magic is the format version, here 1. A
reader refuses any other version. Compatible additions do not change it:
they use new section kinds (§2.2), metadata tags (§3), attachment keys
(§10.1) or segment codings and back ends that old readers refuse cleanly.
A new non-critical section that must survive attachment edits belongs in
the demo part: sections in the attachment part are dropped when the
attachment set changes (§10, Updating).
Nothing was released before version 1.

---------------------------------------------------------------------------

## 1. Conventions

* **`uv`**: unsigned LEB128: 7 bits per byte, least significant group
  first, high bit = continuation; at most 10 bytes. The value must fit in
  64 bits: a 10th byte other than 0x00 or 0x01 is an error, as is an 11th
  byte or running off the end. Non-minimal encodings are accepted. Every
  field states its range; a value outside it is an error (§9). In
  particular a value used as a length or count must fit the reader's
  address space, or the file is refused with a limit error.
* **`u8`, `u16le`, `u32le`, `u64le`**: unsigned fixed-width integers,
  little-endian.
* **CRC-32C**: Castagnoli, reflected polynomial 0x82F63B78, initial value
  0xFFFFFFFF, final XOR 0xFFFFFFFF. Check value: CRC-32C("123456789") =
  0xE3069283.
* **SHA-256**: FIPS 180-4.
* **zstd data** of N bytes: one or more zstd frames (RFC 8878 §3.1.1)
  that together decompress to exactly N bytes, and nothing else. Skippable
  frames (§3.1.2) are allowed and skipped; any other magic number (in
  particular the legacy formats before RFC 8878) is an error. Every frame:
  the reserved bit of the frame header descriptor is 0; the dictionary id,
  if present, is 0; its window size (Frame_Content_Size for a
  single-segment frame) is at most `max(N, 2^20)`; a content size, if
  present, is at most N; no block is of the reserved type or larger than
  `min(window size, 128 KiB)`; a frame that carries a content size or a
  content checksum must match it. Empty data (no frame) is an error, even
  for N = 0. (The window rule lets a decoder that buffers a frame's window,
  as streaming decoders do, stay within about 2N + 1 MiB; one-shot zstd
  compression with a known size, the usual way to write these bodies,
  always satisfies it.)
* **Integer arithmetic.** Unless stated, values are as wide as their type
  and arithmetic wraps modulo 2^bits where it says "wrapping"; elsewhere it
  cannot overflow for valid input (§13 lists the places where it could
  for invalid input, and what the reference does). `i16(x)` reinterprets
  the low 16 bits as signed; `x as u16` keeps the low 16 bits; a signed
  right shift is arithmetic; integer division truncates toward zero, and
  every division in this format has non-negative operands. "Rust
  semantics" below means: `as` casts truncate (`u32 as u16`), sign-extend
  (`i16 as i32`) or reinterpret (`u32 as i32`).
* **Precedence** of the expressions quoted below: `* / %` > `+ -` >
  `<< >>` > `&` > `^` > `|`. E.g. `a | b << 8` is `a | (b << 8)`; most
  formulas are written with explicit parentheses.
* `popcount(x)` = number of set bits. `min`, `max`, `clamp(x, lo, hi)` as
  usual.
* **q(v)** for a 32-bit signed v (`numeric.rs:q`): 0 if v = 0, else
  `2 * bitlen(|v|) - 1 + (v < 0 ? 1 : 0)` with `bitlen(m) = 32 - clz32(m)`
  and |i32::MIN| = 2^31, so q is in 0..=64. `qb(v: u8) = q(v as i8 as i32)`
  (`walk.rs:qb`).
* **zigzag(v)** = `((v << 1) ^ (v >> 31)) as u32`;
  `unzigzag(u) = ((u >> 1) as i32) ^ -((u & 1) as i32)` (`coder/mod.rs`).
* "Symbol" = one byte of a structural stream; "value" = one i32 of a
  numeric stream.

---------------------------------------------------------------------------

## 2. Container (`format.rs`)

```
file    := prefix section*
prefix  := "MVDZ" | 0x01 | flags u8 | reserved u8 u8 | meta_off u64le      (16 bytes)
section := kind u8 | len u64le | body[len]
```

### 2.1 Prefix

* Bytes 0-3: `MVDZ`. Byte 4: the version, 1.
* Byte 5, `flags`: bit 0 = the file was written by the streaming encoder
  (§11; its metadata is near the end). Other bits are 0 when written;
  readers ignore them.
* Bytes 6-7: 0 when written; readers ignore them.
* `meta_off`: file offset of the META section, or 0 (then the trailer's
  `meta_off` is used). A cataloguing reader can read 16 bytes, then the META
  section: two reads. If non-zero it must equal the trailer's `meta_off`.
* A file shorter than 16 bytes is refused: "not an MVDZ file" if it is
  shorter than 5 bytes or its magic does not match, "unsupported MVDZ
  version" if byte 4 is not 1, else "truncated prefix".

### 2.2 Sections

Sections follow each other from offset 16 to the end of the file. A
section whose length runs past the end of the file, or a final fragment
shorter than a section header (9 bytes), ends the section list: the file
is then incomplete (§2.4).

**Kinds.** Bit 7 (0x80) of `kind` marks a *critical* section: a reader
that does not know a critical kind refuses the file ("unknown critical
section", a format error). A reader skips every non-critical kind it does
not know, without reading its body.

| kind | name | critical | content | at most |
|---|---|---|---|---|
| 0x01 | META | no | metadata (§3) | one |
| 0x82 | SEGMENT | yes | one segment of the demo (§5) | any |
| 0x03 | INDEX | no | whole-demo size, CRC and duration; the segments (§4) | one |
| 0x04 | ATTACH | no | one attachment's stored bytes (§10) | any |
| 0x05 | ADIR | no | the attachment directory (§10) | one |
| 0x7F | TRAILER | no | offsets of INDEX, META, ADIR; end of the demo part (§2.3) | one, last |

**Order.** A file has two parts:

* the *demo part*: META, the SEGMENT sections in demo order, then INDEX
  (META may come before, between or after segments; §5.3 says when it must
  come first);
* the *attachment part*, from the first ATTACH or ADIR section on: ATTACH
  sections, then the ADIR.

Then the TRAILER. Unknown non-critical sections may appear anywhere before
the trailer (those in the attachment part do not survive a change of the
attachments, §10). Every one of these is an error: a second META, INDEX or ADIR;
a SEGMENT after the INDEX; a META, SEGMENT or INDEX in the attachment part;
an ATTACH after the ADIR; ATTACH sections without an ADIR; a TRAILER that
is not the last 49 bytes of the file; any byte after the trailer.

### 2.3 Trailer

```
0x7F | len = 40 (u64le) | index_off u64le | meta_off u64le | adir_off u64le | body_end u64le | "MVDZTRL1"
```

The last 49 bytes of a complete file. Offsets are file offsets of section
headers. The trailer must agree with the sections (§9): `index_off` is the
INDEX section, `meta_off` the META section, `adir_off` the ADIR section or
0 if there is none, and `body_end` the offset of the first ATTACH or ADIR
section, or the trailer's own offset if there is none. A section of kind
0x7F whose length is not 40, or whose last 8 bytes are not `MVDZTRL1`, is
an error.

### 2.4 Complete and incomplete files

A *complete* file ends with its trailer and has exactly one META and one
INDEX. A file whose sections end without a trailer (a recording that was
interrupted, a cut download) is *incomplete*: every reader refuses it as a
whole ("incomplete file (no trailer)"), but a sequential reader first
delivers the bytes of every complete segment before that point (§11.2), so
a recorder's crash loses only the tail.

### 2.5 Layouts written by the reference encoder

A reader must not depend on these beyond the rules above.

* One shot (`encode.rs:compress`): `prefix (meta_off = 16) | META |
  SEGMENT* | INDEX | TRAILER`.
* Streaming (`stream.rs:StreamEncoder`, §11): `prefix (flags = 1,
  meta_off = 0) | SEGMENT* | META | SEGMENT* | INDEX | TRAILER`. The
  segments that share KTX demoinfo (§5.3) and all segments after the first
  of them follow the META.
* Attachments (`api.rs:update_attachments_bytes`): the demo part, copied
  byte for byte, then `ATTACH* | ADIR | TRAILER`.

---------------------------------------------------------------------------

## 3. Metadata (META)

Raw facts about the demo, readable without decoding it. No interpretation
(no mode classification, team assignment or identity folding).

```
META body := version u8 (= 1) | codec u8 | raw_len uv | crc u32le | payload
  codec 0: payload = the raw_len bytes of `fields` (the payload must end the section)
  codec 1: payload = zstd data (§1) of raw_len bytes of `fields`
fields := { tag uv | len uv | value[len] }*                  (exactly raw_len bytes)
```

* The META section body is at most 2^24 + 64 bytes.
* `version` other than 1: format error. `codec` other than 0 or 1: format
  error. `raw_len` <= 2^24 (16 MiB), and the **ratio limit** `raw_len <=
  1024 x L + 4096` with L the length of the META section body (as for
  segments, §5.1), else an error ("metadata size"). `crc` is the CRC-32C
  of the raw `fields` bytes; a mismatch is an error.
* Tags 1 to 14 are defined below; each may appear at most once (a
  duplicate is an error). Other tags are skipped (and may repeat).
* Tags 1 (sha256), 2 (size) and 8 (timing) are required.
* A defined tag's value must have exactly the grammar given (consumed
  exactly; strings checked as stated: tags 9 and 10 UTF-8 where the table
  says so, tags 12 and 13 at most 128 bytes of `raw`), else the file is
  refused.

| tag | name | value |
|---|---|---|
| 1 | sha256 | 32 bytes: SHA-256 of the original demo (its identity) |
| 2 | size | `uv`: length of the original demo |
| 3 | demoinfo | the KTX demoinfo store: one or more blocks `u32le n | n bytes` filling the value exactly (an empty value is an error). Present exactly when a segment shares it (§5.3); the segments' shares are consecutive and cover it exactly |
| 4 | serverinfo | the first `fullserverinfo` string, verbatim (the quoted part of the stufftext `fullserverinfo "\k\v..."`); empty if none |
| 5 | levelname | the level name of the first svc_serverdata, verbatim |
| 6 | map | the first svc_modellist entry (e.g. `maps/dm4.bsp`), verbatim |
| 7 | roster | `count uv (<= 256) | count x { slot uv (< 256), userid uv (< 2^32), len uv, userinfo[len] }`, slots strictly ascending: each slot's svc_updateuserinfo as of the first svc_playerinfo of the demo (or the end of the demo if there is none) |
| 8 | timing | `duration_ms uv | frames uv`: the demo's total frame time and frame count |
| 9 | census | UTF-8 text (invalid UTF-8 is an error): a JSON object describing the demo (§3.3); informative |
| 10 | ext | `count uv | count x { klen uv, key[klen] (UTF-8), vlen uv, value[vlen] }`, keys unique (bytewise): extension key/values given at compress time (e.g. `source`, `hub_game_id`, `captured`); not part of the demo bytes |
| 11 | start_timestamp | `demo_ms uv | unix_ms uv`: the value of the first hidden block 0x000B (mvdhidden_demo_start_timestamp_ms: Unix milliseconds when the server opened the demo, mvdsv `sv_demo_misc.c` `SV_MVDEmbedStartTimestamp`) and the demo time it appeared at. The block body has two forms: current mvdsv writes a ULEB128, earlier builds a fixed 8-byte u64le. A body of exactly 8 bytes whose ULEB128 does not end at its 8th byte (a byte below 0x80 earlier) is the u64le; any other body is read as ULEB128 over at most its first 10 bytes, stopping after the first byte below 0x80 or at the end of the body |
| 12 | epoch | `demo_ms uv | raw[rest]`: the first value of the serverinfo key `epoch` (Unix seconds as decimal text, set by mvdsv when the demo starts, `sv_demo.c`), from the initial `fullserverinfo` or a later svc_serverinfo, verbatim (at most 128 bytes), and its demo time |
| 13 | matchdate | `demo_ms uv | raw[rest]`: the first svc_print whose text starts with `matchdate:` (KTX `match.c`, `matchdate: %Y-%m-%d %H:%M:%S %Z`, sent at match start): the text after the prefix up to the newline, trimmed, verbatim (at most 128 bytes), and the demo time of the print |
| 14 | pauses | `truncated u8 (0 or 1) | count uv (1..65536) | count x { demo_ms uv, paused_ms uv }`, demo_ms strictly increasing: the paused periods (§3.1) |

**Checked by a full decode** (§5.5): sha256, size and timing must match the
decoded demo; a mismatch is an error ("identity mismatch", "demo timing").
A reader that does not decode the demo (a catalogue reading only the
metadata) gets the CRC check but not these; it must not treat the identity
as authenticated.

Tags 11-14 are present only when the demo has the corresponding data.

**Encoder caps** (`format.rs:Metadata::from_scan`, `encode`). The reference
leaves out a value that is too large, so the META stays small: tags 4-6
over 64 KiB are written empty; roster entries whose userinfo is over 4 KiB
are left out; demoinfo blocks are shared (tag 3) only while the store stays
within 4 MiB (later blocks stay in their segments); extension keys and
values over 1 MiB in total, or a duplicate key, make compression fail
before anything is written. META is zstd-compressed only when that is
smaller and keeps within the ratio limit.

**Demoinfo JSON** (`meta.rs:demoinfo_json`). Readers reassemble the KTX
JSON from tag 3 (as demo tools commonly do): each block body is `u16le block
number | text` (a body shorter than 2 bytes: no JSON); for each block
number the *last* block wins (a demo can carry the demoinfo more than
once); then the texts of blocks 1, 2, ..., n in ascending order, then
block 0 (the last block), are concatenated. The demo stream does not carry
these blocks a second time (§5.3); re-emitting them in their original order
and chunking is exact because tag 3 keeps every block, in file order, with
its length.

### 3.1 Wall-clock time

Demo time (§4) stops while a server is paused: mvdsv writes paused
frames with time byte 0 and, for each, a hidden block of type 0x000A
(mvdhidden_paused_duration, `sv_demo.c` `SV_MVDWritePausedTime`) whose one
body byte is the real milliseconds that frame took. Current mvdsv writes
the framed form, `u32le 1 | u16le 0x000A | u8 ms`; older builds wrote it
without the block's length field (the dem_multiple(0) payload is exactly
the 3 bytes `u16le 0x000A | u8 ms`). Both forms count. The encoder adds every such block's ms to a
*run* at the current demo time (paused frames all share one demo time), so
tag 14 lists one (demo_ms, paused_ms) per pause. At most 65536 runs are
kept; `truncated` = 1 if more existed.

With `P(t)` = the sum of `paused_ms` over the runs with `demo_ms < t`, the
wall-clock time (Unix ms) at demo time t is

```
wall(t) = base + t + P(t)
```

and a pause at demo time p covers the wall-clock interval
`[wall(p), wall(p) + paused_ms)`. Inversely, a wall-clock time w maps to the
largest t with `wall(t) <= w` (inside a pause, to the pause's demo time).
`base` comes from the first available source, in this order:

| order | source | precision | base |
|---|---|---|---|
| 1 | tag 11 start_timestamp | 1 ms | `unix_ms` |
| 2 | tag 12 epoch | 1 s | `1000 x epoch` (decimal digits, optionally surrounded by ASCII spaces; anything else: unusable) |
| 3 | tag 13 matchdate | 1 s | `parse(raw) - demo_ms - P(demo_ms)` (below) |
| 4 | outside sources (a hub timestamp, a date in the file name) | as stated | stored by the encoder's caller as extension keys (tag 10) with their `source`; rank last |

Tags 11 and 12 are the server's clock when it opened the demo file
(mvdsv sets `epoch` and writes the 0x000B block while writing the
demo's first messages, before any frame time has passed), so they are
taken as the wall clock at demo time 0, whatever their `demo_ms` (the
block typically arrives at about 100 ms; `demo_ms` is kept for
reference). The `matchdate` print is sent later, at match start, so its
own demo time and the pauses before it are subtracted.

**Parsing `matchdate`** (so that conforming readers agree): `raw` is
`YYYY-MM-DD HH:MM:SS` (24-hour), one space, and a zone, which is a numeric
offset `+HHMM`, `-HHMM`, `+HH:MM` or `-HH:MM`, or one of `UTC`, `GMT`,
`Z` (+0), `WET` (+0), `WEST` (+1), `CET` (+1), `CEST` (+2), `EET` (+2),
`EEST` (+3), `MSK` (+3). Anything else (other zone names, locale or
ctime-style text) is unparseable: the source is then unusable, and a
reader goes on to the next one.

A pause list that is `truncated` makes `P(t)` exact only up to the last run
listed. The census repeats tags 11-13 and the pause count and total
(`clock`), so `mvdz census` shows which demos carry which source.

### 3.2 Rendering

The reference renders metadata as JSON (`api.rs:FileInfo::to_json`, CLI
`mvdz info`, C `mvdz_info_json`) with a serializer: byte strings (map,
levelname, serverinfo, userinfo, demoinfo, extension values) as JSON
strings of Latin-1 code points (each byte becomes the code point of the
same value, so a reader recovers the bytes); the census is embedded only
if it parses as a JSON object, else as `null`. Nothing a file contains can
add keys to the rendering. Escaping can make the rendering up to six
times the metadata; the C interface counts it against `max_memory`.

One object per file, keys in this order:

| key | value |
|---|---|
| `version` | the format version, 1 |
| `streamed` | prefix flag bit 0 (§2.1) |
| `file_size` | bytes of the `.mvdz` file |
| `bundle_sha256` | the bundle hash (§10.2), hex |
| `sha256`, `size` | tags 1 and 2 (hex; integer) |
| `duration_ms`, `frames` | tag 8 |
| `map`, `levelname`, `serverinfo` | tags 6, 5, 4 (Latin-1 strings) |
| `roster` | tag 7: `[{slot, userid, userinfo}]` |
| `demoinfo` | the reassembled demoinfo JSON (§3), as a Latin-1 string, or `null` |
| `ext` | tag 10: `{key: Latin-1 string}` |
| `census` | tag 9 (an object) or `null` |
| `clock` | `{start_timestamp_ms: [demo_ms, unix_ms] or null, epoch: [demo_ms, raw] or null, matchdate: [demo_ms, raw] or null, pauses: [[demo_ms, paused_ms]...], pauses_truncated}` (tags 11-14) |
| `segments` | the INDEX: `[{t_start_ms, raw_offset, raw_len}]` |
| `attachments` | the ADIR: `[{name, mime, size, stored, sha256, kv: {key: value}}]` |

Integers are JSON numbers (a reader that keeps only 53-bit doubles may lose
precision above 2^53).

### 3.3 Census (informative)

A compact JSON object (about 4-8 KB, 1-3 KB after zstd) describing what the
demo contains, from a scan of every frame and message (`meta.rs:Scanner`).
Readers must not depend on its exact content; this section describes what
the reference writes, so other encoders can match it.

Strings are bytes: printable ASCII (0x20-0x7E) as is, with `"` and `\`
escaped, every other byte as `\u00XX`. Field order is fixed; lists are
capped. Census strings (list entries, info keys and values, the serverdata
gamedir and levelname) are cut to 1024 bytes.

```
census_version   1
size, frames, duration_ms
truncated        true if the demo ends in bytes that do not form a whole frame
tail_bytes       how many
max_message      largest frame payload (bytes)
serverdata       {count, protocol, fte_pext, fte_pext2, mvd_pext ("0x%08x"),
                  gamedir, levelname} of the first svc_serverdata (only count
                  if there is none)
svc              {svc_name: [count, bytes]} per svc_* command (bytes incl. the command byte)
dem              {dem_name: [count, bytes]} per demo frame type (bytes incl. headers)
hidden           {"0xTTTT": [count, bytes]} per hidden block type (dem_multiple, mask 0)
demo_start_timestamp_ms   value of the first hidden 0x000B block (as tag 11 reads
                 it); only if there is one
clock            {[start_timestamp_ms: [demo_ms, unix_ms]], [epoch: [demo_ms, "raw"]],
                  [matchdate: [demo_ms, "raw"]], pause_runs, paused_ms} as in §3.1
raw_path         {escapes, bytes[, first, first_offset]}: unparseable data
                 (unknown or misfitting svc, malformed hidden blocks): count,
                 bytes, where it first happened ("svc N" or "hidden") and its
                 byte offset (first and first_offset only if there is one)
stufftext        {messages, commands: {key: {count, example}}, overflow}
serverinfo_keys  [names] from fullserverinfo and svc_serverinfo (cap 64)
userinfo_keys    [names] from svc_updateuserinfo (cap 64)
client_values    {"*client"|"*version"|"*z_ext": [distinct values]} (cap 16 each)
updates          {setinfo: svc_setinfo count, serverinfo: svc_serverinfo count,
                  fullserverinfo: fullserverinfo stufftext count}
modellist, soundlist          [entries] verbatim, in order (cap 256 each)
modellist_dropped, soundlist_dropped
print            {"0".."3"|"other": count} svc_print by level (only levels seen)
centerprint      count
```

**Names.** `svc_name` for command c < 56 is entry c of: svc_bad, svc_nop,
svc_disconnect, svc_updatestat, svc_version, svc_setview, svc_sound,
svc_time, svc_print, svc_stufftext, svc_setangle, svc_serverdata,
svc_lightstyle, svc_updatename, svc_updatefrags, svc_clientdata,
svc_stopsound, svc_updatecolors, svc_particle, svc_damage, svc_spawnstatic,
svc_spawnbinary, svc_spawnbaseline, svc_temp_entity, svc_setpause,
svc_signonnum, svc_centerprint, svc_killedmonster, svc_foundsecret,
svc_spawnstaticsound, svc_intermission, svc_finale, svc_cdtrack,
svc_sellscreen, svc_smallkick, svc_bigkick, svc_updateping,
svc_updateentertime, svc_updatestatlong, svc_muzzleflash,
svc_updateuserinfo, svc_download, svc_playerinfo, svc_nails,
svc_chokecount, svc_modellist, svc_soundlist, svc_packetentities,
svc_deltapacketentities, svc_maxspeed, svc_entgravity, svc_setinfo,
svc_serverinfo, svc_updatepl, svc_nails2, svc_qizmomsg; for c >= 56 it is
`svc_c` (decimal). `dem_name` for frame type t = `type & 7` is entry t of:
dem_cmd, dem_read, dem_set, dem_multiple, dem_single, dem_stats, dem_all,
dem_7. Only names with a non-zero count are listed, in number order.

**Commands and frames.** A frame's payload is parsed with the command
sizer (§6.2) and the protocol state it tracks (svc_serverdata updates it);
the first command that is unknown, 0, or does not fit ends the payload's
parse (an escape: counted in raw_path). Hidden payloads (dem_multiple with
mask 0) are parsed as blocks `[u32le len][u16le type][len bytes]`; a block
that does not fit, or whose length is 0, ends the parse (an escape).

**stufftext keys.** svc_stufftext strings (up to their NUL) are appended
per destination: dem_all; dem_single, dem_stats and dem_read by their
target (`type >> 3`); dem_multiple by its mask. The text is cut into lines
at `\n`, so a command split over several messages is reassembled; a line is
cut to 65536 bytes (the rest up to its newline is dropped). Each line is
split at `;` outside double quotes, as the console does, and each command
trimmed of ASCII whitespace; empty commands are skipped. A command's key
is its first whitespace-separated token; for a `//` directive (first token
starting with `//`) the second token is added (`//ktx took`) when it is 1
to 32 bytes of `a-z`, `0-9`, `_` starting with a letter, so dates, numbers
and quoted strings never become part of a key. Keys are cut to 64 bytes.
Each key has a count and one example (its first command, cut to 80 bytes).
At most 64 keys; further new keys only increment `overflow`. A
`fullserverinfo` command also feeds `serverinfo_keys`, and the first one is
the META serverinfo (tag 4).

**Roster cut point.** The roster is the slot table (slot -> (userid,
userinfo string up to its NUL)) of svc_updateuserinfo commands, copied when
the first svc_playerinfo command is met (or at the end of the demo); slots
with an empty userinfo are left out.

---------------------------------------------------------------------------

## 4. INDEX

```
INDEX body := size uv | crc u32le | duration_ms uv
              nseg uv | nseg x { off uv, raw_len uv, t_start uv }      (consumed exactly)
```

**Demo time.** Demo time 0 is the start of the demo file: the time
before its first frame. The demo time of a frame is the sum of the time
bytes (first byte of each frame) of all frames up to and including it, in
milliseconds; this is the time the index, the metadata `duration_ms` and
seeking use.

* `size`, `crc`: length and CRC-32C of the whole original demo.
* `duration_ms`: total demo time (sum of all frame time bytes).
* One entry per SEGMENT section, in file order (= demo order): `off` is the
  file offset of its section; `raw_len` and `t_start` must equal the
  segment header's. Segment i starts at raw offset `sum(raw_len[0..i])`;
  the sum of all `raw_len` must equal `size` (and the META size).
* **Seeking** (`format.rs:Index::seg_at_time`). `t_start` is the demo time
  *before* the segment's first frame (the sum of the time bytes of all
  earlier frames); its first frame is at `t_start` plus its own time byte.
  To decode every frame at demo time `ms` or later, start at the last
  segment whose `t_start < ms` (or the first segment). Decoding resumes at
  that segment's first byte, a frame boundary. (A player still needs the
  game state at that point; the serverdata, lists and baselines are at the
  start of the demo.)

---------------------------------------------------------------------------

## 5. Segments

### 5.1 SEGMENT body

```
SEGMENT body := raw_off uv | raw_len uv | t_start uv | crc u32le | coding u8 | coded
```

* `raw_off`: offset of the segment's first byte in the demo; it must equal
  the sum of `raw_len` of all earlier segments (so no segment can be
  dropped, repeated or reordered unnoticed). `raw_len`: bytes it decodes to,
  at most 0xFFFF0000. `t_start`: as in §4. `crc`: CRC-32C of its decoded
  bytes.
* **Ratio limit.** `raw_len <= 1024 x L + 4096`, where L is the length of
  the SEGMENT section body. This bounds every decoder's memory and work by
  the bytes actually present; the encoder never exceeds it (§12).
* `coding`: 0 transform (§5.2), 1 zstd: `coded` is zstd data (§1) of
  `raw_len` bytes, 2 stored: `coded` is the `raw_len` bytes. Any other
  value: format error ("unknown segment coding").
* Every segment but the last ends at a frame boundary (§5.5 checks it).
* Segments are independent: every walker and predictor state is fresh per
  segment; the protocol state at the start is in the transform header.

### 5.2 Transform body (coding 0) (`segment.rs:parse_header`)

```
flags uv | dinfo_off uv | dinfo_len uv | fte uv | fte2 uv | mvd1 uv | tail uv
nstreams uv | nstreams x { id uv, backend uv, count uv, len uv }
stream bodies, concatenated in table order (body k is len_k bytes)
```

* `flags`: bit 0 (`SF_DINFO`) = KTX demoinfo blocks come from the
  metadata's store (§5.3). Other bits: error ("segment flags"; they would
  change the decoding). With bit 0 clear, `dinfo_off` and `dinfo_len` must
  be 0; with bit 0 set, `dinfo_len` must be at least 1 (a flagged segment
  carries at least one block). These rules on the first three fields are
  structural (§9 rule 6): every reader applies them when it meets the
  segment header. The rest of the transform header is checked when the
  segment is decoded.
* `fte`, `fte2`, `mvd1` (each < 2^32): protocol extension state at the
  segment start (§6.1). `tail` (<= raw_len): length of the unframeable
  bytes at the end of the segment.
* `nstreams` <= 384. Per entry: `id` must be a structural, numeric or blob
  id (§5.4), each at most once; `backend` 0 store, 1 zstd, 2 rans for
  structural and numeric streams, 0 or 1 for blobs, else an error; `count`
  (symbols, values or bytes) <= raw_len + 16. The bodies must end exactly
  at the end of the section.
* A structural or numeric stream absent from the table has no symbols:
  reading a symbol or value from it is an error. An absent blob stream is
  an empty byte array: reading 0 bytes from it succeeds (an empty `tail`,
  a command whose body is empty), reading more is an error.

### 5.3 Demoinfo blocks

With flag `SF_DINFO`, hidden blocks of type 3 (KTX demoinfo) are not
stored in the segment. The walk (§6.11) reads them from the segment's
*share* of the metadata's demoinfo store, `META.demoinfo[dinfo_off ..
dinfo_off + dinfo_len]`: a sequence of `u32le n | n bytes` whose `n` must
equal the block's length field; the segment must consume its share exactly.
The shares of the flagged segments, in file order, are consecutive and
cover the store exactly (the first starts at 0). A flagged segment must come
after the META section in the file (a sequential reader needs the store
first). The demoinfo text is thus stored once per file.

### 5.4 Stream ids (`streams.rs`)

| id | name | kind | content |
|---|---|---|---|
| 0 | time | structural | frame time delta byte |
| 1 | type | structural | frame type byte |
| 2 | mask | structural | dem_multiple mask (4 bytes) |
| 3 | cmd | structural | svc command byte, 255 = end of payload, 0 = escape |
| 4 | esclen | structural | escape length (LEB128 bytes) |
| 5 | hid.tag | structural | hidden-block tag (0 escape, 1 block, 2 end) |
| 6 | hid.len | structural | hidden-block length (4 bytes) |
| 7 | hid.type | structural | hidden-block type (2 bytes) |
| 8 | hid.dmg | structural | hidden damage block (4 × 2 bytes) |
| 9 | pi.num | structural | playerinfo player number |
| 10 | pi.flags | structural | playerinfo flags (1 or 3 symbols) |
| 11 | pi.frame | structural | playerinfo frame (residual) |
| 12 | pi.model | structural | playerinfo model |
| 13 | pi.skin | structural | playerinfo skin |
| 14 | pi.eff | structural | playerinfo effects |
| 15 | pi.wframe | structural | playerinfo weapon frame (residual) |
| 16 | pe.hdr | structural | delta packet-entities "from" byte (residual) |
| 17 | pe.ent | structural | entity-number gap |
| 18 | pe.bits | structural | U_* high bits of the entity word, XOR previous |
| 19 | pe.more | structural | U_MOREBITS byte, FTE extension bytes |
| 20 | pe.model | structural | modelindex (1 or 2 bytes) |
| 21 | pe.frame | structural | frame (residual) |
| 22 | pe.misc | structural | colormap, skin, effects, FTE trans/scale/fatness/colourmod |
| 23 | pe.a | structural | entity angles (residual or raw) |
| 24 | st.idx | structural | svc_updatestat index |
| 25 | st.val | structural | svc_updatestat value (residual) |
| 26 | stl.idx | structural | svc_updatestatlong index |
| 27 | snd.ch | structural | sound channel word (2 bytes) |
| 28 | snd.misc | structural | sound volume / attenuation |
| 29 | snd.num | structural | sound number |
| 30 | te.type | structural | temp-entity type |
| 31 | te.misc | structural | temp-entity entity (2 bytes) or count (1 byte) |
| 32 | pr.level | structural | svc_print level |
| 33 | frags | structural | svc_updatefrags (3 bytes) |
| 48,49,50 | pi.ox, pi.oy, pi.oz | numeric | player origin, 16-bit coords |
| 51,52,53 | pi.ap, pi.ay, pi.ar | numeric | player angles (16-bit) |
| 54,55,56 | pe.ox, pe.oy, pe.oz | numeric | entity origin, 16-bit coords |
| 57 | snd.o | numeric | sound origin (16-bit coords) |
| 58 | te.o | numeric | temp-entity origins (16-bit coords) |
| 59 | stl.val | numeric | svc_updatestatlong value (32-bit) |
| 60,61,62 | pi.fx, pi.fy, pi.fz | numeric | player origin, 32-bit float coords (bit patterns) |
| 63,64,65 | pe.fx, pe.fy, pe.fz | numeric | entity origin, 32-bit float coords |
| 80 | raw | blob | escaped payload tails |
| 81 | tail | blob | bytes after the last parsable frame |
| 82 | set | blob | dem_set bodies (8 bytes each) |
| 83 | text | blob | string bodies of svc 8 (after level), 9, 26 |
| 84 | ping | blob | bodies of svc 36, 37, 53 |
| 85 | nails | blob | bodies of svc 43, 54 |
| 86 | hid.misc | blob | hidden block bodies other than damage |
| 87 | snd.f | blob | sound origins with float coords (12 bytes) |
| 88 | te.f | blob | temp-entity origins with float coords |
| 79 | dinfo | (not stored) | the segment's demoinfo share (§5.3); never in the stream table |
| 128+c | gen.c | blob | body of every other svc command c (incl. 11 serverdata) |

Kinds (`streams.rs:is_lazy/is_num/is_blob`): structural = `id < 34`; numeric =
`48 <= id < 66`; blob = `80 <= id <= 88` or `128 <= id < 384`.

* **Structural** streams are decoded one symbol at a time by the
  walker (§6), each symbol with a side context `ctx: u32` chosen by the
  walker. Back ends: store, zstd, rans.
* **Blob** streams are decoded entirely up front (byte arrays of exactly
  `count` bytes) and consumed sequentially by the walker. Back ends: store,
  zstd (rans: error).
* **Numeric** streams hold i32 residuals, decoded after (or alongside) the
  walk by the numeric groups (§7), each value with a feature vector `NCtx`.
  Back ends: store, zstd, rans.

---

### 5.5 Decoding a segment, and the whole-demo checks (`decode.rs`)

Per segment:

1. Parse the header (§5.1) and, for a transform segment, the transform
   header and stream table (§5.2).
2. Coding 1/2: decompress or copy. Coding 0:
   1. Decode every **blob** stream to exactly `count` bytes (§8).
   2. Construct the structural and numeric decoders (a bad model header,
      a store length or zstd size mismatch, ... is an error).
   3. **Walk** (§6), producing the structural bytes and the record lists.
      Afterwards every structural stream must be complete (§8: exactly
      `count` symbols read and its coded data consumed), and every blob
      stream, and the demoinfo share, consumed exactly.
   4. **Wave 1** numeric streams, then **wave 2** (§7.7); afterwards every
      numeric stream must be complete.
3. `crc32c(output) == crc`.

The reference runs segments, and within a segment the blob decoding and
wave 1, in parallel; the bits do not depend on this.

Then, for the whole demo (a reader that decodes every segment from the
first; a reader that starts in the middle checks only per segment):

* the segments' `t_start` values are the running sums of their frames'
  time bytes (each segment's frames are found by `frame_len`, §6.2, from its
  first byte); every segment but the last consists of whole frames;
* `crc32c(demo) == INDEX.crc`, its length == `INDEX.size`;
* the sum of the frame times == `INDEX.duration_ms` == META `duration_ms`,
  the frame count == META `frames`;
* `sha256(demo) == META.sha256`, its length == META `size`.

The decoded demo is returned only if all of these pass. A sequential
reader delivers each segment's bytes as soon as that segment passes its
own checks, and reports the end of the demo only after the whole-demo
checks; a failure after some bytes were delivered is an error, and those
bytes are a prefix of the demo.

---------------------------------------------------------------------------

## 6. The structural transform (`walk.rs`, `proto.rs`)

The decoder rebuilds the segment (length `raw_len`) into an output buffer
`dst`. The walker writes all structural bytes; it *reserves* the bytes of
numeric fields (records their offsets in record lists) without writing
them; the numeric groups (§7) fill those bytes in afterwards. The structure
never depends on a numeric value.

Notation for the walker primitives (decode direction; `p` = output cursor):

* `b8(sid, ctx)`: decode one symbol `v` from structural stream `sid` with context
  `ctx`, write `v` at `dst[p]`, `p += 1`, return `v`.
* `r8(sid, ctx, pred)`: decode symbol `s`; `v = (s + pred) mod 256`; write
  and return `v`.
* `bn(sid, ctx, n)`: for `k = 0..n-1`: `byte_k = b8(sid, ctx | (k << 14))`;
  returns the little-endian value. (Byte index in context bits 14-15.)
* `blob(sid, n)`: copy the next `n` bytes of blob stream `sid` to `dst[p..]`,
  `p += n`. Error if the blob has fewer than `n` bytes left or the output
  would overflow.
* `reserve(n)`: `off = p; p += n`; error if `p > raw_len`; returns `off`.
* `generic(sid, c)`: `n = cmd_size(c, remaining bytes of blob sid)` (§6.2);
  `None` → error; else `blob(sid, n)`.

`wr` of a byte beyond `raw_len` is an error (sticky; the walk stops at the
next check). If a structural stream is exhausted (or absent), the read returns 255
and flags an error (`segment.rs:DecIO::byte`) — 255 ends every loop, and
the segment then fails.

### 6.1 Protocol state (`proto.rs:Proto`)

```
FTE_FLOAT = 0x8000, FTE_TRANS = 0x8, FTE_COLOURMOD = 0x80000, MVD1_FLOAT = 1
float_coords = (fte & FTE_FLOAT) != 0 || (mvd1 & MVD1_FLOAT) != 0
msg_wide     = (fte & FTE_FLOAT) != 0
coord  = float_coords ? 4 : 2    // entity/player origins, sound/TE coords
wcoord = msg_wide ? 4 : 2        // generic coords (svc 30 only)
angle  = msg_wide ? 2 : 1
```

`observe_server_data(body)` (after an svc 11 body has been written):

```
i = 0
while i + 4 <= len(body):
    v = le32(body[i..]); i += 4
    if v == 28: break                      // PROTOCOL_QW
    if i + 4 > len(body): break
    f = le32(body[i..]); i += 4
    if v == 0x58455446 ('FTEX'): fte = f
    if v == 0x32455446 ('FTE2'): fte2 = f
    if v == 0x3144564D ('MVD1'): mvd1 = f
re-derive float_coords / msg_wide
```

### 6.2 Command sizer (`proto.rs:Proto::cmd_size`)

`cmd_size(cmd, b)` = number of body bytes (excluding the command byte) of
`cmd` whose body starts at `b[0]`, or `None` if `cmd` is unknown or the body
does not fit in `b`. Primitives: `skip(n)` (fails if fewer than n bytes
left), `u8`, `u16`, `u32` (LE, fail if short), `str` (scan to and including
the next NUL; fail if none).

| cmd | body |
|---|---|
| 1, 27, 28, 34, 35 | empty |
| 2, 9, 26, 31 | str |
| 3 | skip 2 |
| 6 | `ch = u16`; if `ch & 0x8000` skip 1; if `ch & 0x4000` skip 1; skip `1 + 3*coord` |
| 8 | skip 1; str |
| 10 | skip `1 + 3*angle` |
| 11 | loop { `v = u32`; if v == 28 break; skip 4 }; skip 4; str; skip 4; str; skip 40 |
| 12 | skip 1; str |
| 14 | skip 3 |
| 19 | skip `2 + 3*coord` |
| 20 | skip `4 + 3*(coord + angle)` |
| 21, 66 | `w = u16`; delta(w) |
| 22 | skip `2 + 4 + 3*(coord + angle)` |
| 23 | `t = u8`: t ∈ {5,6,9}: skip `2 + 6*coord`; t ∈ {0,1,3,4,7,8,10,11,13}: skip `3*coord`; t ∈ {2,12}: skip `1 + 3*coord`; else None |
| 24, 32, 44 | skip 1 |
| 29 | skip `3*coord + 3` |
| 30 | skip `3*wcoord + 3*angle` |
| 36 | skip 3 |
| 37, 38 | skip 5 |
| 39, 53 | skip 2 |
| 40 | skip 5; str |
| 41 | `sz = u16 as i16`; skip 1; if sz > 0 skip sz |
| 42 | skip 1; `f = u16`; skip 1; for i in 0..3: if `f & (1<<i)` skip coord; for i in 0..3: if `f & (8<<i)` skip 2; then skip 1 for each of DF_MODEL (1<<11), DF_SKIN (1<<7), DF_EFFECTS (1<<6), DF_WEAPONFRAME (1<<10) that is set |
| 43 | `n = u8`; skip `6*n` |
| 54 | `n = u8`; skip `7*n` |
| 45, 46 | skip 1; loop { s = str; break if it was empty (just the NUL) }; skip 1 |
| 47 | loop { `w = u16`; if w == 0 break; delta(w) } |
| 48 | skip 1; then as 47 |
| 49, 50 | skip 4 |
| 51 | skip 1; str; str |
| 52 | str; str |
| other (incl. 0, 4, 5, 7, 13, 15–18, 25, 33, 55–65, 67–255) | None |

`delta(w)` (`proto.rs:delta_size`), U_* bits: U_ANGLE1=1<<0, U_ANGLE3=1<<1,
U_MODEL=1<<2, U_COLORMAP=1<<3, U_SKIN=1<<4, U_EFFECTS=1<<5, U_EVENMORE=1<<7,
U_ORIGIN1=1<<9, U_ORIGIN2=1<<10, U_ORIGIN3=1<<11, U_ANGLE2=1<<12,
U_FRAME=1<<13, U_MOREBITS=1<<15; X_SCALE=1<<0, X_TRANS=1<<1, X_FATNESS=1<<2,
X_MODELDBL=1<<3, X_ENTITYDBL=1<<5, X_ENTITYDBL2=1<<6, X_YETMORE=1<<7,
X_COLOURMOD=1<<10.

```
bits = w & ~511; more = 0
if bits & U_MOREBITS: bits |= u8
if bits & U_EVENMORE and fte != 0: more = u8; if more & X_YETMORE: more |= u8 << 8
if bits & U_MODEL: skip 1  elif more & X_MODELDBL: skip 2
for m in [U_FRAME, U_COLORMAP, U_SKIN, U_EFFECTS]: if bits & m: skip 1
for (o, a) in [(U_ORIGIN1,U_ANGLE1), (U_ORIGIN2,U_ANGLE2), (U_ORIGIN3,U_ANGLE3)]:
    if bits & o: skip coord
    if bits & a: skip angle
if more & X_TRANS and fte & FTE_TRANS: skip 1
if more & X_SCALE: skip 1
if more & X_FATNESS: skip 1
if more & X_COLOURMOD and fte & FTE_COLOURMOD: skip 3
```

`frame_len` (`proto.rs:frame_len`) finds frames: the encoder uses it for
the walk and the cut points, the decoder for the timing checks of §5.5.
At offset p, with at least 2 bytes left: byte p+1 & 7 = 2 → 10 bytes; 3 → `10 + le32
at offset 6`; 1,4,5,6 → `6 + le32 at offset 2`; anything else (0, 7) or a
truncated frame → not a frame (the rest of the segment becomes the tail).

### 6.3 Walker state (`walk.rs:Walker::new`)

All zero/default unless noted, fresh per segment:

| state | type | init | lifetime |
|---|---|---|---|
| `t` | u32 | 0 | demo time (ms), += every time byte (wrapping) |
| `target` | u8 | 0 | `type >> 3` of the current frame |
| `last_type`, `last_tb` | u8 | 0 | previous frame's type / time byte |
| `prev_cmd` | u8 | 0 | reset to 0 at each payload start |
| `first_by_ty[8]` | u8 | 0 | per frame type (`type & 7`), see §6.5 |
| `pi_prev_num` | u8 | 255 | reset to 255 at each payload start |
| `st_prev_idx` | u8 | 255 | reset to 255 at each payload start |
| `st_prev_val` | u8 | 0 | reset to 0 at each payload start |
| `last_hdr` | u8 | 0 | segment |
| `prev_te` | u8 | 0 | segment |
| `pe_prev_d` | u8 | 0 | reset to 0 at each packet-entities command |
| `pl[256]` | {flags u16, frame u8, wf u8} | 0 | per player number |
| `ent[2048]` | {a[3] u8, ares[3] u8, frame u8, model u8, bits u8, last_pkt u32} | 0 | per entity |
| `st[32][256]` | u8 | 0 | stat values per (target & 31, index) |
| `pl_off[256][3]`, `ent_off[2048][3]` | u32 | NONE (0xFFFFFFFF) | offset of the last sent origin coord per axis |
| `prev_pkt`, `cur_pkt` | list of u16 | empty | entity numbers (9-bit) of the previous / current packet |
| `pkt_ptr` | usize | 0 | |
| `pkt_no` | u32 | 1 | packet counter |
| `proto` | Proto | from segment header | |

Record lists (`walk.rs:Recs`), appended in walk order and consumed by §7:

* `PiRec {off u32, t u32, flags u16, num u8, fl u8}` (fl bit 0 = float_coords at the time)
* `PeRec {off u32, t u32, num u16, bits u16, model u8, ubits u8}`; `bits`
  holds PE_O1=1, PE_A1=2, PE_O2=4, PE_A2=8, PE_O3=16, PE_A3=32,
  PE_INPREV=64, PE_FLOAT=128, PE_WIDE=256
* `SndRec {off u32, refs[3] u32, kind u8, num u8}`
* `TeRec {off u32, refs[3] u32, ty u8}`
* `StlRec {off u32, key u16}`

The reference sizes these lists exactly from the stream table
(`Recs::fixed`): pi ≤ count(pi.num), pe ≤ count(pe.bits), snd ≤
count(snd.num), te ≤ count(te.type), stl ≤ count(stl.idx); pushing beyond
capacity is an error. In a valid file these are always exact, so a port may
use growable lists (but see §13).

### 6.4 Segment walk and frames (`Walker::run`, `Walker::frame`)

```
stop = max(raw_len - tail, 0)                // saturating
while no error and p < stop: frame()
if p != stop: error
blob(TAIL, tail)
```

`frame()`:

```
tb = b8(TIME, last_type | (last_tb << 8));            last_tb = tb; t += tb
ty = b8(TYPE, last_type | (min(tb, 32) << 8));        last_type = ty; target = ty >> 3
switch ty & 7:
  2:          blob(SET, 8)                            // dem_set
  3:          mask = bn(MASK, 0, 4)                   // dem_multiple
              length_prefixed(ty, hidden = (mask == 0))
  1, 4, 5, 6: length_prefixed(ty, hidden = false)
  0, 7:       error
```

Note the contexts use `last_type` *before* it is updated, and TIME's context
uses the previous frame's time byte.

`length_prefixed(ty, hidden)` (decode): `at = reserve(4)` (return on error);
then `hidden ? hidden_payload() : payload(ty)`; then write `n = p - at - 4`
as u32le at `dst[at..at+4]` (only if in bounds).

### 6.5 Payload command loop (`Walker::payload`)

```
prev_cmd = 0; pi_prev_num = 255; st_prev_idx = 255; st_prev_val = 0
idx = 0
tyi = ty & 7
first_prev = cmd_class(first_by_ty[tyi])        // captured once, at payload start
first = 255
loop:
    if error: return
    ctx = cmd_class(prev_cmd) | (min(idx, 15) << 4) | (first_prev << 8) | (tyi << 12) | (prev_cmd << 16)
    idx += 1
    if idx == 2: first = prev_cmd
    first_by_ty[tyi] = (idx >= 2) ? first : 255
    cmd = (symbol from CMD with ctx)
    if cmd == 255: return                          // end of payload
    if cmd == 0:   escape(); return                // rest of the payload is raw
    write cmd at dst[p]; p += 1
    command(cmd)
    prev_cmd = cmd
```

Note `cmd` is decoded and written separately (the CMD symbol is not written
for 255/0). The effect of the `first_by_ty` rule: after a payload with at
least one command, `first_by_ty[tyi]` holds its first command; after an
empty payload (immediate 255 or 0) it holds 255. `cmd_class(255) = 15`;
initially `first_by_ty = 0` → class 0.

`cmd_class(c)` (`walk.rs:cmd_class`): 0→0, 42→1, 48→2, 47→3, 6→4, 23→5, 3→6,
38→7, 8→8, 51→9, 14→10, 36→11, 9→12, 43→13, 54→13, 19→14, anything else→15.

`escape()` (`Walker::escape`, decode):

```
n = 0
for k in 0..5:
    b = symbol from ESCLEN with ctx k
    n |= (b & 0x7f) << (7*k)                     // u32, high bits of k=4 lost
    if b < 0x80: break
    if k == 4: error; return
blob(RAW, n)
```

(The encoder escapes when a command byte is 0, unknown to the sizer, or its
body does not fit before the payload end; the escaped part runs to the end
of the payload.)

### 6.6 Commands (`Walker::command`)

Each row lists, in decode order, every symbol `(stream, context)`; `r8`
symbols are residuals as defined in §6.

| cmd | handling |
|---|---|
| 42 playerinfo | §6.7 |
| 47, 48 packet entities | §6.8 (48 = delta) |
| 3 updatestat | `t = target & 31`; `i = b8(ST_IDX, st_prev_idx | (prev_cmd << 16))`; `prev = st[t][i]`; `v = r8(ST_VAL, i | (st_prev_val << 8), prev)`; `st_prev_idx = i`; `st_prev_val = v - prev` (mod 256); `st[t][i] = v` |
| 38 updatestatlong | `t = target & 31`; `i = b8(STL_IDX, prev_cmd)`; `off = reserve(4)`; push `StlRec{off, key = (t << 8) | i}` |
| 6 sound | §6.9 |
| 23 temp entity | §6.10 |
| 8 print | `b8(PR_LEVEL, 0)`; `generic(TEXT, 9)` (string) |
| 9, 26 | `generic(TEXT, 9)` |
| 14 updatefrags | `bn(FRAGS, 0, 3)` |
| 36, 37, 53 | `generic(PING, cmd)` |
| 43, 54 | `generic(NAILS, cmd)` |
| 11 serverdata | `start = p`; `generic(128 + 11, 11)`; if no error: `proto.observe_server_data(dst[start..p])` |
| any other | `generic(128 + cmd, cmd)` |

`prev_cmd` in these contexts is the *previous* command of the payload (0 for
the first); it is updated after `command()` returns.

### 6.7 playerinfo (svc 42) (`Walker::playerinfo`, `Walker::pi_flags`)

```
num = b8(PI_NUM, pi_prev_num | (prev_cmd << 16)); pi_prev_num = num
ps = pl[num]
flags = pi_flags(ps.flags)
frame = r8(PI_FRAME, ps.frame | ((flags & 0x300) << 8), ps.frame)
cw = proto.coord
off = reserve(popcount(flags & 7) * cw + popcount(flags & 0x38) * 2)
push PiRec{off, t, flags, num, fl = float_coords ? 1 : 0}
o = off
for i in 0..3: if flags & (1 << i): pl_off[num][i] = o; o += cw
if flags & DF_MODEL   (1<<11): b8(PI_MODEL, 0)
if flags & DF_SKIN    (1<<7):  b8(PI_SKIN, 0)
if flags & DF_EFFECTS (1<<6):  b8(PI_EFF, 0)
wf = ps.wf
if flags & DF_WEAPONFRAME (1<<10): wf = r8(PI_WFRAME, ps.wf, ps.wf)
pl[num] = {flags, frame, wf}
```

The reserved block holds the origin coords (axes with flag bits 0-2, `cw`
bytes each) followed by the angles (flag bits 3-5, always 2 bytes each);
§7 fills it.

`pi_flags(prev)` — the 16-bit flags are coded as `x = flags XOR prev`:

```
cx = (prev & 0x3f) | (((prev >> 10) & 1) * 64) | (prev << 16)
a  = symbol(PI_FLAGS, cx)
x  = (a & 0x3f) | (((a >> 6) & 1) << 10)
if a & 0x80:                                      // a rare bit changed
    b1 = symbol(PI_FLAGS, 0x100 | ((prev >> 6) & 0xff))
    b2 = symbol(PI_FLAGS, 0x200)
    x |= ((b1 & 0xf) << 6) | ((b1 >> 4) << 11) | ((b2 & 1) << 15)
flags = x ^ prev
write flags as 2 bytes LE (low byte first)
return flags
```

(Symbol a: bits 0-5 = x bits 0-5, bit 6 = x bit 10, bit 7 = escape. b1: low
nibble = x bits 6-9, high nibble = x bits 11-14. b2 = x bit 15; the encoder
only emits 0/1, the decoder uses bit 0.)

### 6.8 Packet entities (svc 47, 48) (`Walker::packet_entities`, `Walker::entity_delta`)

```
if cmd == 48: h = r8(PE_HDR, 0, last_hdr); last_hdr = h
pe_prev_d = 0
prev_ent = -1 (i32)
pkt_ptr = 0; cur_pkt = []
pkt_no += 1 (wrapping)
loop:
    if error: break
    pe = predict_ent(prev_ent)
    ectx = min(pe - prev_ent, 255) | (min(pe_prev_d, 15) << 8)
    d = symbol(PE_ENT, ectx)
    if d == 255:
        d = symbol(PE_ENT, 1 << 12) | (symbol(PE_ENT, 2 << 12) << 8)
    if d == 0: write 0x00, 0x00; break                 // end of packet
    ent = (prev_ent + d) & 511
    es = ent[ent]
    b = symbol(PE_BITS, es.bits | (es.model << 7)) XOR es.bits
    w = (b << 9) | ent                                 // u16
    if w == 0: error; break
    write w as 2 bytes LE
    pe_prev_d = clamp(ent - prev_ent, 0, 255)
    prev_ent = ent
    cur_pkt.append(ent)
    ent[ent].bits = w >> 9                             // 7 bits, stored at the 9-bit index
    entity_delta(w)
swap(prev_pkt, cur_pkt)                                // also after an error break
```

`predict_ent(prev)`: advance `pkt_ptr` while `pkt_ptr < len(prev_pkt)` and
`prev_pkt[pkt_ptr] <= prev`; return `prev_pkt[pkt_ptr]` if `pkt_ptr <
len(prev_pkt)`, else `prev + 1`. (`pkt_ptr` only moves forward within a
packet.) The encoder codes `d = ((ent - prev_ent - 1) & 511) + 1` (1..=512),
directly if `d < 255`, else as 255 followed by two bytes.

`entity_delta(w)`:

```
bits = w & ~511; more = 0
num = w & 511
m0 = ent[num].model
if bits & U_MOREBITS: bits |= b8(PE_MORE, (bits >> 9) | (m0 << 8))
if bits & U_EVENMORE and proto.fte != 0:
    more = b8(PE_MORE, 0x8000)
    if more & X_YETMORE: more |= b8(PE_MORE, 0x8001) << 8
if more & X_ENTITYDBL:  num += 512
if more & X_ENTITYDBL2: num += 1024
in_prev = (ent[num].last_pkt == pkt_no - 1)            // wrapping
ent[num].last_pkt = pkt_no
es = copy of ent[num]
if bits & U_MODEL:          es.model = b8(PE_MODEL, es.model)
elif more & X_MODELDBL:     bn(PE_MODEL, 0x100, 2)     // model not updated
model = es.model
if bits & U_FRAME:    es.frame = r8(PE_FRAME, model, es.frame)
if bits & U_COLORMAP: b8(PE_MISC, model)
if bits & U_SKIN:     b8(PE_MISC, model | (1 << 8))
if bits & U_EFFECTS:  b8(PE_MISC, model | (2 << 8))
rb = 0
for i in 0..3 (pairs (U_ORIGIN1,U_ANGLE1), (U_ORIGIN2,U_ANGLE2), (U_ORIGIN3,U_ANGLE3)):
    if bits & origin_i: rb |= 1 << (2*i)
    if bits & angle_i:  rb |= 2 << (2*i)
if in_prev: rb |= PE_INPREV
if proto.float_coords: rb |= PE_FLOAT
if proto.msg_wide:     rb |= PE_WIDE
rec = PeRec{off = p, t, num, bits = rb, model = es.model, ubits = (w >> 9) as u8}
for i in 0..3:
    if bits & origin_i: ent_off[num][i] = reserve(proto.coord)
    if bits & angle_i:
        if proto.msg_wide:
            bn(PE_A, 3 | (model << 6), 2)
        else:
            qa = min(qb(es.ares[i]), 15)
            v = r8(PE_A, i | (qa << 2) | (model << 6), es.a[i])
            es.ares[i] = v - es.a[i] (mod 256); es.a[i] = v
push rec
if more & X_TRANS and proto.fte & FTE_TRANS:          b8(PE_MISC, 0x3000)
if more & X_SCALE:                                    b8(PE_MISC, 0x3001)
if more & X_FATNESS:                                  b8(PE_MISC, 0x3002)
if more & X_COLOURMOD and proto.fte & FTE_COLOURMOD:  bn(PE_MISC, 0x3003, 3)
ent[num] = es
```

Subtleties:

* `m0` (PE_MORE context) and the PE_BITS context use the entity at the
  9-bit index `w & 511`, whereas the model/frame/angle state uses the
  extended `num`. `ent[w & 511].bits` is written before `entity_delta`; if
  `num != w & 511` the final `ent[num] = es` writes back `ent[num]`'s own
  (old) `bits`.
* `es` is copied *after* `last_pkt` is set, so the final write-back keeps
  the new `last_pkt`.
* `prev_pkt`/`cur_pkt` hold 9-bit numbers.
* Angles are interleaved with the reserved origin coords in wire order
  (origin1, angle1, origin2, angle2, origin3, angle3), which is why §7
  recomputes origin offsets from `rec.off` and `rec.bits` (§7.5).

### 6.9 sound (svc 6) (`Walker::sound`)

```
lo = b8(SND_CH, prev_cmd)
ch = lo | (b8(SND_CH, 0x100 | lo) << 8)
if ch & 0x8000: b8(SND_MISC, 0)       // volume
if ch & 0x4000: b8(SND_MISC, 1)       // attenuation
ent = (ch >> 3) & 1023
num = b8(SND_NUM, (ch & 7) | (min(ent, 33) << 3))
if proto.coord != 2: blob(SNDF, 12); return
if 1 <= ent <= 32: refs = pl_off[ent - 1]; kind = 1
else:              refs = ent_off[ent]; kind = 2     // ent <= 1023 < 2048 always
off = reserve(6)
push SndRec{off, refs, kind, num}
```

`refs` are copied at this moment (the latest origin offsets of that
player/entity in this segment, or NONE).

### 6.10 temp entity (svc 23) (`Walker::temp_entity`)

```
t = b8(TE_TYPE, prev_te | ((prev_cmd == 23 ? 1 : 0) << 8)); prev_te = t
c = proto.coord
if c != 2:
    if t in {5,6,9}: bn(TE_MISC, t, 2); blob(TEF, 6*c)
    elif t in {2,12}: b8(TE_MISC, t | 0x100); blob(TEF, 3*c)
    else: blob(TEF, 3*c)
    return
if t in {5,6,9}:                                       // beams
    e = bn(TE_MISC, t, 2)
    refs = (1 <= e <= 32) ? pl_off[e - 1] : [NONE]*3
    off = reserve(12); push TeRec{off, refs, ty = t}
elif t in {2,12}:
    b8(TE_MISC, t | 0x100)
    off = reserve(6); push TeRec{off, [NONE]*3, t}
else:
    off = reserve(6); push TeRec{off, [NONE]*3, t}
```

(For an unknown type the encoder escapes the payload, because the sizer
fails, so the last branch only sees types known to §6.2.)

### 6.11 Hidden payloads (dem_multiple with mask 0) (`Walker::hidden`)

The payload is a sequence of blocks `[u32 len][u16 type][len bytes]`:

```
loop:
    if error: return
    tag = symbol(HIDTAG, 0)
    if tag == 0: escape(); return
    if tag == 2: return
    if tag != 1: error; return
    bl = bn(HIDLEN, 0, 4)
    ty = bn(HIDTYPE, 0, 2)
    if ty == 7 and bl == 8:
        for k in 0..4: bn(HIDDMG, k, 2)             // contexts k | (byte << 14)
    elif ty == 3 and (flags & SF_DINFO):         // KTX demoinfo, from the share (§5.3)
        n = next 4 bytes of the share as u32le; error if missing or n != bl
        copy the next bl bytes of the share
    else:
        blob(HIDMISC, bl)
```

(The encoder emits tag 0 when fewer than 6 bytes remain or the block length
is 0 or overruns the payload.)

### 6.12 Context summary for structural streams

| stream | context(s) |
|---|---|
| TIME | `last_type | (last_tb << 8)` |
| TYPE | `last_type | (min(tb,32) << 8)` |
| MASK | `k << 14`, k = 0..3 |
| CMD | §6.5 |
| ESCLEN | `k` (0..4) |
| HIDTAG | 0 |
| HIDLEN, HIDTYPE | `k << 14` |
| HIDDMG | `j | (k << 14)`, j = 0..3 block field, k = byte |
| PI_NUM | `pi_prev_num | (prev_cmd << 16)` |
| PI_FLAGS | `cx` (§6.7), `0x100 | ((prev >> 6) & 0xff)`, `0x200` |
| PI_FRAME | `ps.frame | ((flags & 0x300) << 8)` |
| PI_MODEL, PI_SKIN, PI_EFF | 0 |
| PI_WFRAME | `ps.wf` |
| PE_HDR | 0 |
| PE_ENT | `ectx`, `1 << 12`, `2 << 12` |
| PE_BITS | `es.bits | (es.model << 7)` (9-bit index) |
| PE_MORE | `(bits >> 9) | (m0 << 8)`, `0x8000`, `0x8001` |
| PE_MODEL | `es.model`; `0x100 | (k << 14)` |
| PE_FRAME | `model` |
| PE_MISC | `model`, `model | 0x100`, `model | 0x200`, `0x3000`, `0x3001`, `0x3002`, `0x3003 | (k << 14)` |
| PE_A | `i | (qa << 2) | (model << 6)`; wide: `3 | (model << 6) | (k << 14)` |
| ST_IDX | `st_prev_idx | (prev_cmd << 16)` |
| ST_VAL | `i | (st_prev_val << 8)` |
| STL_IDX | `prev_cmd` |
| SND_CH | `prev_cmd`; `0x100 | lo` |
| SND_MISC | 0, 1 |
| SND_NUM | `(ch & 7) | (min(ent,33) << 3)` |
| TE_TYPE | `prev_te | ((prev_cmd == 23) << 8)` |
| TE_MISC | `t | (k << 14)`; `t | 0x100` |
| PR_LEVEL | 0 |
| FRAGS | `k << 14` |

The rans back end uses `ctx & 0xFFFF` (§8.3); store and zstd ignore
contexts.

---

---------------------------------------------------------------------------

## 7. Numeric groups (`numeric.rs`)

After the walk, each numeric stream is decoded by iterating its record list
in walk order. For each value: compute a prediction `pred` and a feature
vector `NCtx f[6]` (u8 each) from stream-local state, decode a residual `r`
from the stream with `f`, write `pred + r` at the recorded offset, update
state. Each numeric stream has its **own** decoder and its own state:
the `Track` arrays and the `tm` arrays below are separate per stream (per
group of §7.7 and per axis), each starting from zero, even where two
streams compute the same values (48+a and 60+a; the angle streams). So the
streams can be decoded independently and in any order, subject to the wave
rule (§7.7).

### 7.1 Value write rules (`numeric.rs:x16`, `x32`)

* 16-bit field: `v = (pred + (r as u16)) mod 2^16`, written u16le at `off`;
  the residual kept as state is `r' = (r as u16) as i16 as i32`.
* 32-bit field: `v = (pred + (r as u32)) mod 2^32`, written u32le; state
  residual `r' = r`.
* Writes and reads (`Buf::rd16/rd32/wr16/wr32`) outside `[0, raw_len)` are
  ignored / read as 0 (cannot happen in a well-formed walk).

The encoder residual is `(v - pred) as i16 as i32` (16-bit) or `(v - pred)
as i32` (32-bit), i.e. always in i16 range for 16-bit fields.

### 7.2 Prediction helpers

```
times(tm = (t_last, t_prev), t) -> (dt_now, dt_prev):
    dt_now  = (t - t_last) as i32        // u32 wrapping, then reinterpret
    dt_prev = (t_last - t_prev) as i32
    tm = (t, t_last)

dd(now, prev) = clamp(now - prev, -3, 3) + 3            // 0..6

pred_dt(last u16, prev u16, dt_now, dt_prev) -> u16:
    d = (last - prev) as i16 as i32
    if dt_now == dt_prev or dt_prev <= 0 or dt_now < 0:
        return last + (d as u16)                          // mod 2^16
    return last + (div_round(d * min(dt_now, 32767), min(dt_prev, 32767)) as u16)

div_round(a i32, b i32 > 0) -> i32:                       // halves away from zero
    n = |a| + b / 2                                       // as u32
    q = n / b                                             // exact floor
    return a < 0 ? -q : q
```

(The reference computes `n / b` via a reciprocal table for `b < 256`; it is
exact for all `n < 2^31`, so plain integer division is equivalent;
`numeric.rs:div_round`, test `div_round_matches_reference`.)

`Track` state per (player or entity, axis): `last, prev, prev2: u16; res:
i32; flast, fprev: u32; fres: i32; t_last: u32`, all zero initially.

### 7.3 Player origins: streams 48-50 (int) and 60-62 (float) (`pi_origin_step`)

For axis `a` in 0..3, iterate all `PiRec r` in order. Per player number:
`tm[num] = (0,0)` initially, `(dt_now, dt_prev) = times(tm[r.num], r.t)` (done
for every record of that player, whatever its flags). `s = Track[r.num][a]`.
`sent = r.flags & (1 << a)`.

```
if r.fl & 1:                                   // float coords → stream 60 + a
    if sent:
        off  = r.off + 4 * popcount(r.flags & ((1 << a) - 1))
        pred = flast + (flast - fprev)          // u32 wrapping
        f    = [q(fres), a, 0, 0, 0, 0]
        v, r' = x32(off, pred, f)
        fprev = flast; flast = v; fres = r'
    else:
        fprev = flast; fres = 0
    // int state untouched
else:                                           // 16-bit coords → stream 48 + a
    if sent:
        pred = pred_dt(last, prev, dt_now, dt_prev)
        off  = r.off + 2 * popcount(r.flags & ((1 << a) - 1))
        vel  = (last - prev) as i16 as i32
        f    = [q(res), q(vel), clamp(dt_now, 0, 63), r.flags & 7, dd(dt_now, dt_prev), 0]
        v, r' = x16(off, pred, f); res = r'
    else:
        res = 0; v = last
    prev = last; last = v
    // float state untouched
```

### 7.4 Player angles: streams 51-53 (`pi_angle_step`)

For axis `a`, all `PiRec` in order, `times` as above (separate `tm` array per
group but identical values), `s = Track[r.num][a]`:

```
if r.flags & (8 << a):
    pred  = pred_dt(last, prev, dt_now, dt_prev)
    cw    = (r.fl & 1) ? 4 : 2
    off   = r.off + cw * popcount(r.flags & 7) + 2 * popcount((r.flags >> 3) & ((1 << a) - 1))
    dprev = (prev - prev2) as i16 as i32
    f     = [q(res), q(dprev), clamp(dt_now, 0, 63), (r.flags >> 3) & 7, dd(dt_now, dt_prev), 0]
    v, r' = x16(off, pred, f); res = r'
else:
    res = 0; v = last
prev2 = prev; prev = last; last = v
```

Angles are always 16-bit (the stream is the same for int and float coords).

### 7.5 Entity origins: streams 54-56 (int) and 63-65 (float) (`pe_origin_step`)

For axis `a`, all `PeRec r` in order; `s = Track[r.num & 2047][a]`:

```
dt_now = (r.t - t_last) as i32; t_last = r.t           // every record
if r.bits & (1 << (2*a)):                               // origin a sent
    off = origin_offs(r)[a]
    if r.bits & PE_FLOAT:                               // stream 63 + a
        pred = flast + (flast - fprev)
        f = [q(fres), a, 0, 0, 0, 0]
        v, r' = x32(off, pred, f)
        fprev = flast; flast = v; fres = r'
        // int state untouched
    else:                                               // stream 54 + a
        vel  = (last - prev) as i16 as i32
        pred = last + (vel as u16)
        ip   = (r.bits & PE_INPREV) ? 1 : 0
        f = [q(res), q(vel), r.model, ip | ((r.ubits & 7) << 1), clamp(dt_now, 0, 63), 0]
        v, r' = x16(off, pred, f); res = r'
        prev = last; last = v
        // float state untouched
else:
    prev = last; res = 0; fprev = flast; fres = 0
```

`origin_offs(r)` (`walk.rs:PeRec::origin_offs`): `cw = PE_FLOAT ? 4 : 2`,
`aw = PE_WIDE ? 2 : 1`, `pos = r.off`; for i in 0..3: if `bits & (1 << 2i)`:
`o[i] = pos; pos += cw`; if `bits & (2 << 2i)`: `pos += aw`.

(`ubits & 7` are the U_ORIGIN1..3 bits of the entity word.)

### 7.6 Stat longs (59), sounds (57), temp entities (58)

**stl.val (59)** (`numeric.rs:stl`): tables `last[8192] u32`, `lres[8192]
i32`, zero. For each `StlRec r`: `k = r.key & 0x1fff`; `f = [r.key & 0xff,
q(lres[k]), 0, 0, 0, 0]`; `v, r' = x32(r.off, last[k], f)`; `last[k] = v;
lres[k] = r'`.

**snd.o (57)** (`numeric.rs:snd`): for each `SndRec r`, for `i` in 0..3:
`pred = (r.refs[i] == NONE) ? 0 : rd16(r.refs[i])` (the **decoded output**
at that offset); `f = [i | (r.kind << 2), r.num, 0, 0, 0, 0]`; `x16(r.off +
2i, pred, f)`.

**te.o (58)** (`numeric.rs:te`): `last[3] u16 = 0` across the segment. For
each `TeRec r`:

```
if r.ty in {5,6,9}:
    for i in 0..3:  start[i] = x16(r.off + 2i, pred = refs-or-0 as for sounds, f = [i | (1<<2), r.ty, 0,0,0,0]).v
    for i in 0..3:  last[i]  = x16(r.off + 6 + 2i, pred = start[i],           f = [i | (2<<2), r.ty, 0,0,0,0]).v
else:
    for i in 0..3:  last[i]  = x16(r.off + 2i, pred = last[i], f = [i, r.ty, 0,0,0,0]).v
```

Value order within a stream is record order, then `i` (then start before end
for beams).

### 7.7 Waves (`numeric.rs:WAVE1/WAVE2`, `segment.rs:decode`)

* **Wave 1** (independent of each other; may run concurrently with the walk,
  following its record lists): player origins (48-50, 60-62), player angles
  (51-53), entity origins (54-56, 63-65), stl.val (59).
* **Wave 2** (after the walk and all of wave 1): snd.o (57) and te.o (58).
  They read, through `refs`, coordinates written by wave 1 (`pl_off` /
  `ent_off` point only at player/entity origin fields).

The reference groups streams for scheduling (`Group::streams`: e.g.
`PiO(a)` = {48+a, 60+a}, `PeO(a)` = {54+a, 63+a}, fused variants); grouping
has no effect on the bits.

### 7.8 Feature vectors per stream (summary)

| stream | f[0] | f[1] | f[2] | f[3] | f[4] | f[5] |
|---|---|---|---|---|---|---|
| 48-50 pi.o (int) | q(res) | q(vel) | clamp(dt_now,0,63) | flags & 7 | dd | 0 |
| 51-53 pi.a | q(res) | q(dprev) | clamp(dt_now,0,63) | (flags>>3) & 7 | dd | 0 |
| 54-56 pe.o (int) | q(res) | q(vel) | model | inprev \| (ubits&7)<<1 | clamp(dt_now,0,63) | 0 |
| 57 snd.o | i \| kind<<2 | sound num | 0 | 0 | 0 | 0 |
| 58 te.o | i \| sel<<2 (sel 0 point, 1 beam start, 2 beam end) | type | 0 | 0 | 0 | 0 |
| 59 stl.val | stat index (key & 0xff) | q(lres) | 0 | 0 | 0 | 0 |
| 60-62, 63-65 float | q(fres) | axis | 0 | 0 | 0 | 0 |

---

---------------------------------------------------------------------------

## 8. Back ends (`coder/`)

| id | name | structural | numeric | blob |
|---|---|---|---|---|
| 0 | store | raw bytes | zigzag LEB128 | raw bytes |
| 1 | zstd | zstd of the bytes | zstd of the LEB128 bytes | zstd |
| 2 | rans | static rANS, per-context tables | rANS bucket + raw mantissa bits | — (error) |

Other ids → error. `count` below is the stream's `count` from the table.

### 8.1 store (0)

* structural/blob: the body length must equal `count`; the body is the bytes.
* numeric: the body is `count` values, each `zigzag(v)` as LEB128 of a u32:
  up to 5 bytes, `u |= (b & 0x7f) << (7k)`; a 5th byte above 0x0F (a
  continuation bit or bits beyond 32), or running out of bytes → error;
  `v = unzigzag(u)`. After the last value the body must be exactly consumed.

### 8.2 zstd (1)

* structural/blob: the body is zstd data (§1) of exactly `count` bytes.
* numeric: the body is zstd data of at most `5 x count` bytes (more is an
  error), decoded as in §8.1 (exact consumption required).

### 8.3 rans (2) (`coder/rans.rs`)

#### Body

```
hdr_len uv | hdr (hdr_len bytes, range coded model) | rans_len uv | rans (rans_len bytes) | extra (rest of body)
```

`extra` holds the raw mantissa bits of numeric streams; for a structural
stream it must be empty.

#### Range coder (model headers) (`coder/rc.rs:Dec`)

LZMA-style binary decoder, 12-bit probabilities `p = P(bit = 1)` in 1..4095:

```
init: code = 0; range = 0xFFFFFFFF; pos = 0
      repeat 5: code = (code << 8) | next()        // u32: the first byte is shifted out
next(): byte at pos, or 0 past the end; pos += 1 always
bit(p):
    bound = (range >> 12) * p
    if code < bound: bit = 1; range = bound
    else:            bit = 0; code -= bound; range -= bound
    while range < 2^24: range <<= 8; code = (code << 8) | next()
    return bit
overrun() = pos > len + 5
```

Note bit **1** takes the lower sub-interval.

#### Adaptive probability `P` and `IntModel` (`rc.rs:P`, `rc.rs:IntModel`)

`P` is a u16, initial 32768. `p12 = clamp(P >> 4, 1, 4095)`. Update with
shift `sh`: bit 1: `P += (65535 - P) >> sh`; bit 0: `P -= P >> sh`.

`IntModel(nctx)` has `nctx * 64` P's. `get(ctx)` (Elias-gamma-like):

```
base = ctx * 64
n = 0
while n < 32:
    b = bit(P[base + n].p12); update P[base + n] with shift 4
    if b == 0: break
    n += 1
v1 = 1 (u64)
for i = n-1 down to 0:
    k = base + 33 + min(i, 30)
    b = bit(P[k].p12); update P[k] with shift 5
    v1 = (v1 << 1) | b
return (v1 - 1) as u32
```

(If 32 ones are read, no terminating zero is read. Slot 32 is unused.)

#### Model header (`rans.rs:read_model`)

A fresh range decoder over `hdr`; one `IntModel` with `NQC + 8 = 314`
contexts, `NQC = 17*18 = 306`:

```
alpha = get(306); steps = get(307); n = get(308)
require alpha == 256 (structural stream) or 65 (numeric stream)
require 1 <= steps <= 4, n <= 65535, n <= count            (rule M1)
fbq = read_table(fb = none)
k = 0
for i in 0..n:
    dk = get(309)
    if i > 0 and dk == 0: error
    k += dk (checked u32 add); require k <= 0xFFFF
    keys[i] = k
    table_i = read_table(fb = fbq)
    if overrun(): error
if overrun(): error
if pos != hdr_len: error                                     (consumed exactly)
if decisions > 128 * hdr_len + 1024: error                  (rule M2)

read_table(fb):
    prev = 0
    for s in 0..alpha:
        f = (fb is none) ? 17 : min(fb[s], 16)
        t[s] = min(get(f * 17 + min(prev, 16)), 160)
        prev = t[s]                                   // the clamped value
```

`decisions` counts every `bit()` call of the range decoder while reading
the header. **The header is consumed exactly**: after the last table the
decoder's `pos` (5 initial bytes plus one per renormalisation) equals
`hdr_len`. The encoder writes exactly that many bytes (Appendix A: one byte
per `shift_low`, 5 of them in `finish`), so a header padded with extra
bytes, which would buy more decisions under M2, is refused. Rules M1 and M2 bound the work and memory of a model by the
bytes it occupies: an adaptive binary coder can code a long run of
predictable decisions in almost no space (about 0.0004 bits each), so
without them a few kilobytes of header could describe millions of tables.
A decoder may check M2 after each table (the reference does) and must
check it at the end. Valid models from the reference encoder use at most
about 81 decisions per header byte; the encoder never writes a model that
breaks M1 or M2 (it codes the stream with zstd or store instead, §12).

Each table is a vector of log-quantised counts `q[s]` (0 = absent). Table 0
is the **fallback**; tables 1..n are attached to contexts `keys[i]`
(strictly increasing).

#### From q to frequencies (`rans.rs:qval`, `normalize`, `Table::from_q`)

```
POW4 = [65536, 77936, 92682, 110218]
qval(q, steps) -> u64:
    if q == 0: return 0
    e = q - 1; o = e / steps; k = e % steps
    frac = POW4[(k * 4) / steps]
    return (o >= 40) ? (2^64 - 1) >> 8 : frac << o

normalize(w[0..alpha]) -> f[0..alpha]   (T = 4096):
    tot = Σ w (u128)
    if tot == 0: return all zeros
    sum = 0
    for i: if w[i] > 0: f[i] = max(floor(w[i] * 4096 / tot), 1) (u128 arithmetic); sum += f[i]
    diff = 4096 - sum
    while diff != 0:
        bi = index of the largest f (first one on ties; scan from 0 with strict >)
        if diff > 0: f[bi] += diff; diff = 0
        else:
            take = min(-diff, f[bi] - 1, (f[bi] + 1) / 2)
            if take <= 0: break
            f[bi] -= take; diff += take
    return f
```

`freq = normalize(qval(q[s]))`, `cum[0] = 0`, `cum[s+1] = cum[s] + freq[s]`.
A table is **empty** if no `freq > 0`, **deterministic** if exactly one
symbol has `freq > 0` (then `freq = 4096`).

#### Symbol decode (`rans.rs:DModel::decode`, `RDec`)

State: `X[4]` u32, a word cursor over `rans`, and `j` (next state index,
starts at 0).

```
init: for j in 0..4: hi = word(); lo = word(); X[j] = (hi << 16) | lo
word(): u16le at pos (0 if fewer than 2 bytes remain); pos += 2

decode(ctx):
    tab = keys contains ctx ? its table : fallback
    if tab empty: error
    if tab deterministic: return its symbol            // no state, no j advance
    jj = j; j = (j + 1) % 4
    x = X[jj]; slot = x & 4095
    s = the symbol with cum[s] <= slot < cum[s+1]
    x = freq[s] * (x >> 12) + slot - cum[s]
    if x < 2^16: x = (x << 16) | word()
    X[jj] = x
    return s
```

The 4 states are used round-robin over **coded** symbols only (symbols from
deterministic tables are skipped). The decoder-side table layout (search
tables, slot LUTs, context map) in the reference is an implementation
detail. (Encoder: states start at 2^16; `finish` emits each state as two
words so the decoder reads `X[0]` first, high word first; Appendix A.)

**End of the stream.** After the last symbol, every state must be back at
2^16 and every word of `rans` read (the read position equals `rans_len`;
an empty `rans` body therefore never completes, as its states read as 0).
For a numeric stream, the mantissa bits read must end within the last byte
of `extra`: `ceil(bits_read / 8) == len(extra)` (at most 7 padding bits).

#### Contexts and symbols

* **Structural streams** (`ByteDec`): `ctx = walker_ctx & 0xFFFF`; the
  symbol is the byte.
* **Numeric streams** (`NumDec`): `ctx = num_ctx(sid, f)`; the symbol is the
  bucket `b`; a decoded `b > 64` → error. Then

  ```
  n  = (b + 1) / 2                 // bit length of |v|
  nb = max(n - 1, 0)
  m  = ((n > 0 ? 1 : 0) << nb) | bits.get(nb)      // u32
  v  = (b is even) ? -m (wrapping) : m        // b = 0 → 0
  ```

  `m` is a u32, reinterpreted as i32 before the sign is applied, and the
  negation wraps: buckets 63 and 64 (n = 32) give `m` up to 2^32 - 1, so
  bucket 63 yields `m as i32` (negative for m >= 2^31) and bucket 64 yields
  `(m as i32).wrapping_neg()` (m = 2^31 gives i32::MIN).

  `bits` reads `extra` as an LSB-first bit stream (bit k of the stream =
  bit `k % 8` of byte `k / 8`), zeros past the end; `get(0) = 0`.
  (Encoder bucket: `b = 2n - 1 + (v<0)`, mantissa = `|v|` without its
  leading 1, `n - 1` bits.)

`num_ctx(sid, f)` (`rans.rs:num_ctx`), with `cl(v, m) =
min(v, m)`:

| sid | ctx |
|---|---|
| 48-50 | `cl(f0,19) | (cl(f1,19) << 5)` |
| 51-53 | `cl(f0,23) | (cl(f1,23) << 5)` |
| 54-56 | `cl(f0,17) | (cl(f1,17) << 5) | ((f3 & 1) << 10)` |
| 57, 58 | `f0 | (f1 << 4)` |
| 59 | `f0 | (cl(f1,15) << 8)` |
| others (60-65) | `cl(f0,63) | (f1 << 6)` |

Count rule: the stream is complete iff exactly `count` symbols were
requested and the end-of-stream checks above pass.

---------------------------------------------------------------------------

## 9. Validation rules and limits

Every reader applies the same rules: whole-file decoding, sequential
reading, seekable (random-access) reading, metadata-only reading and the C
interface accept and refuse the same files (`format.rs:Layout`; the
reference tests this on every vector and on mutated files). A reader that
does not decode the demo (metadata only) applies every rule of this
section except the content checks of §5.5; a random-access reader applies
the structural rules when it opens the file, reading every section header
and every segment header (one small read each).

**Structural rules** (refuse the file if any fails):

1. Prefix: magic and version (§2.1); at least 16 bytes.
2. Section order and multiplicity (§2.2); unknown critical kind.
3. A complete file has a trailer, a META and an INDEX (§2.4); the trailer's
   offsets agree with the sections and the prefix `meta_off` (§2.3, §2.1).
4. META: body at most 2^24 + 64 bytes; version, codec, `raw_len` <= 2^24
   and the ratio limit, zstd data (§1), CRC, tag grammar, duplicate tags,
   required tags (§3).
5. INDEX: grammar, consumed exactly, at most 2^24 bytes; one entry per
   SEGMENT section with equal offset, `raw_len` and `t_start`; sizes sum to
   `size`; `size` equals META `size` (§4).
6. SEGMENT headers: `raw_off` chain, `raw_len` <= 0xFFFF0000, the ratio
   limit, a known coding; for a transform segment the first three fields
   of the transform header (`flags`, `dinfo_off`, `dinfo_len`, §5.2);
   demoinfo shares non-empty, consecutive, inside the store and covering
   it, each flagged segment after the META (§5).
7. ADIR: grammar, consumed exactly, at most 2^24 bytes, names non-empty
   and unique, keys unique within an entry, names, MIME types, keys and
   values UTF-8; one entry per ATTACH section in the same order, with
   equal offset and `stored` length (§10).

**Content rules** (a full decode): every check of §5.5 (including the
rest of each transform header, §5.2) and §8.

**Errors.** A refusal falls in one of three categories, which every
decoder should report alike (the reference's `Error` kinds, the C codes
`MVDZ_E_*`; the messages in `MANIFEST.tsv` are the reference's):

* *format* (`MVDZ_E_FORMAT`): not an MVDZ file, an unsupported version of
  the file or of the META, an unknown META codec, an unknown critical
  section, an unknown segment coding, an unknown attachment codec (when
  that attachment is read);
* *limit* (`MVDZ_E_LIMIT`): the file is valid but needs more than the
  reader's budget, or a size that does not fit the reader's address space;
* *corrupt* (`MVDZ_E_CORRUPT`): every other rule of this document.

When a file breaks several rules, which one is reported depends on the
order of the checks. The reference checks sections in file order, each
section's own rules (header, then body) when it is met, then the
cross-checks of the trailer (rule 3, the INDEX and ADIR agreement), then,
when decoding, the content rules segment by segment and the whole-demo
checks; the hostile vectors each break one rule only.

**Limits.** All of them are rules of the format, stated in format terms:

| limit | value | rule |
|---|---|---|
| segment raw length | 0xFFFF0000 | §5.1 |
| segment ratio | `raw_len <= 1024 x section length + 4096` | §5.1 |
| attachment ratio (zstd codec) | `size <= 1024 x stored + 4096` | §10 |
| stream count, stream table | `nstreams <= 384`, `count <= raw_len + 16` | §5.2 |
| numeric zstd body | at most `5 x count` bytes | §8.2 |
| model tables (M1) | `n <= min(65535, count)` | §8.3 |
| model decisions (M2) | `<= 128 x header length + 1024` | §8.3 |
| META fields | 2^24 bytes | §3 |
| INDEX, ADIR sections | 2^24 bytes | §4, §10 |
| roster | 256 entries | §3 |
| META ratio | `raw_len <= 1024 x L + 4096` (L: META body length) | §3 |
| rANS model header | consumed exactly | §8.3 |
| zstd frames | window <= `max(N, 2^20)`, no legacy frames | §1 |

With these, every size a decoder allocates is bounded by the bytes
present: memory and work are linear in the input (at most about 1024
output bytes and 300 model decisions per input byte). On top of this a
decoder may apply its own budget (the reference's `Limits`: memory 1 GiB
on 64-bit targets by default, and work), and refuse a file that needs more
with a limit error (not a format error). The reference charges to that
budget everything it holds: section bodies while it parses them, the
metadata, index and directory it keeps, a fixed amount per SEGMENT and
ATTACH section it records (so a long stream of empty sections cannot grow
a sequential reader without bound), model tables and output. Duplicate
checks (META extension keys, ADIR names and keys) must not be quadratic: a
directory of 2^24 bytes can hold millions of keys. A decoder must never
abort the process on a declared size.

**What is not checked.** Census content; roster, serverinfo, levelname
and map against the demo; attachment content (checked only when an
attachment is read: length, decompression, SHA-256). A reader that does
not decode the demo does not verify the identity (§14).

---------------------------------------------------------------------------

## 10. Attachments

Files stored beside the demo, not part of its bytes (an analysis result,
a screenshot, notes, a recording). mvdz never interprets their content:
an attachment is a name, a MIME type, free-form key/values (§10.1) and
bytes.

```
ATTACH body := the stored bytes of one attachment
ADIR body   := count uv | count x entry                       (consumed exactly)
entry := nlen uv | name[nlen]   UTF-8, non-empty, unique in the file
         mlen uv | mime[mlen]   UTF-8, a MIME type (may be empty)
         codec u8               0 store, 1 zstd
         size uv                original length
         stored uv              ATTACH body length (must equal the section's)
         sha256 [32]            of the original bytes
         off u64le              file offset of the ATTACH section
         nkv uv | nkv x { klen uv, key[klen], vlen uv, value[vlen] }   keys and values UTF-8, keys unique
```

Uniqueness is bytewise. The entries are in the order of the ATTACH
sections. (A reader may refuse `count > len / 46` and `nkv > len / 2`, with
`len` the ADIR body length, before allocating: no valid directory exceeds
them, as an entry takes at least 46 bytes and a key/value pair 2.) A reader lists
attachments from the ADIR and reads one by seeking to its `off`, without
touching the demo or the other attachments. Reading one checks: codec 0,
`size == stored`; codec 1, `size <= 1024 x stored + 4096` and zstd data of
`size` bytes; then the SHA-256. An unknown codec makes that attachment
unreadable (a format error when it is read), not the file. Readers do not
read attachment bodies to skip them.

**Writing.** The reference stores data as zstd (level 19) when that saves
at least 1/16 of it and keeps within the ratio limit, else stored; so
already-compressed media (Opus, PNG) is stored as is.

**Updating.** Adding, replacing or removing attachments rewrites only the
part after `body_end`: the new file is `file[0 .. body_end]`, one ATTACH
section per attachment, the ADIR (if any attachment remains) and a trailer
with the same `index_off`, `meta_off`, `body_end` and the new `adir_off`.
The demo part and its offsets are unchanged, unknown sections in it
included; removing every attachment restores the original file byte for
byte. The attachment part is rebuilt from the directory: unknown
non-critical sections in it are dropped (they may depend on the
attachment set, as a signature over the bundle would), and the directory
is re-encoded (minimal varints). An update that changes nothing (no
attachment added, none of the names to remove present) leaves the file as
it is. An updater must not write a file
that readers refuse: the reference refuses an update (and leaves the file
alone) whose directory would exceed 2^24 bytes (checked before any
attachment is packed, with every new entry's sizes counted at their
largest encoding, so an update within about 20 bytes per new attachment
of the limit may be refused), or whose new attachments use an unprefixed
key (§10.1). The
reference (`api.rs:update_attachments`) takes an advisory lock on the file
(so concurrent updaters, threads or processes, serialise; on POSIX
systems), writes the new file to a fresh temporary file in the same
directory (created exclusively, so it never follows or reuses an existing
path, and owner-only until it has the original's permissions and group,
which it gets before any data is written), syncs it, renames it over the
original and syncs the directory; a symlink is followed to the file it
names. Readers that have the old file open keep a consistent view
(POSIX).

**Exporting** is the same operation as updating with only removals,
written to a new file or stream instead of in place: `file[0 ..
body_end]` byte for byte, the kept ATTACH sections exactly as stored (no
recoding), a new ADIR with the kept entries in their original order and
new offsets, and a trailer as above (unknown sections in the attachment
part are dropped, as in an update, unless every attachment is kept); the
source is not changed. Keeping
every attachment copies the file as it is, byte for byte; keeping none
gives the file without attachments, byte for byte. A typical export is a demo download without
large attachments. The reference selects by name: an attachment is kept
if its name is one of the `keep` names or starts with one of the
`keep_prefix` prefixes (every attachment, if neither is given), and is
none of the `drop` names and starts with none of the `drop_prefix`
prefixes (`api.rs:Selection`, `export_attachments`, C
`mvdz_attachments_export`, CLI `mvdz attach export [--keep name]...
[--keep-prefix p]... [--drop name]... [--drop-prefix p]...`). Each
returns or prints the bundle hash of the output (§10.2).

### 10.1 Keys

Each entry carries free-form key/value pairs (UTF-8). Version 1 defines
no key. **Keys without a prefix are reserved** for future versions of
this specification; everyone else writes keys that start with `x-` or
with a vendor prefix, a name followed by a dot (`mvda.schema`,
`ezquake.offset`). Writers refuse an unprefixed key (the reference does);
readers accept and preserve every key, prefixed or not, exactly as
stored, and ignore keys they do not know. Uniqueness of keys within an
entry is bytewise. A future version can then define keys of its own
(see `proposals/media-attachments.md` for media keys proposed for
version 2) without colliding with keys already in files.

### 10.2 Bundle hash (`api.rs:bundle_hash`)

A SHA-256 that names the demo together with its attachments, the same
whatever the level, the segmenting, the attachments' codecs and the order
of the sections, so two files that carry the same demo and the same
attachments have the same bundle hash. It is meant as a download ETag and
as the message a future signature would sign. It is computed, not stored:

```
bundle_sha256 = SHA-256( "MVDZBND1"                         8 ASCII bytes
                         | sha256 [32]                      META tag 1
                         | count uv                         attachments in the ADIR (0 without one)
                         | count x attachment )             in ascending bytewise order of name
attachment := nlen uv | name | mlen uv | mime | sha256 [32]
              | nkv uv | nkv x { klen uv | key | vlen uv | value }   in ascending bytewise order of key
```

Strings are the bytes stored in the ADIR (UTF-8), `uv` as in §1 (minimal
encoding). It covers what the file *claims*: the META identity and the
directory's SHA-256s. Verifying it means a full decode (the identity) and
reading every attachment (their SHA-256s); a signature over it is only as
good as those checks (§14). The demo's own metadata (server info, census,
extension keys) is not covered separately: META is part of the demo part,
which no operation of this format rewrites. The extension keys (tag 10)
are fixed when the demo is compressed; changing them means rewriting META
and with it the offsets of the demo part (the prefix, the INDEX and the
trailer), and v1 has no tool for that. Data about a demo that changes
over time (ratings, tags, links) belongs outside the file.

---------------------------------------------------------------------------

## 11. Streaming

### 11.1 Writing (`stream.rs:StreamEncoder`, API `Writer`, C `mvdz_writer_*`)

The streaming encoder compresses a demo that is still being recorded:

* The configuration (extension metadata) is checked when the writer is
  created, before anything is written. A write that would start a
  segment beyond the reference's limit of 65536 segments per file fails.
* It cuts a segment whenever at least the segment size (default 1 MiB) of
  whole frames has arrived, codes it, decodes it again to check it (falling
  back to zstd or stored bytes on a mismatch), and writes it at once. Bytes
  that do not form a whole frame wait for more data; at the end they are
  the last segment.
* Every segment is self-contained (its models are in its streams), so the
  file can be read while it grows.
* At the end it writes META (now that the identity, census and demoinfo
  are known), the segments held back (the first segment that shares
  demoinfo and every segment after it: they are coded when they fill and
  kept in memory as coded bytes), INDEX and TRAILER. KTX writes the
  demoinfo at the end of a match, so this is normally only the last
  segment; a demo with demoinfo early holds most of its coded bytes until
  the end.
* The first error of the sink poisons the writer: every later call returns
  it and nothing more is written; `finish` then fails. `abort` stops
  without a trailer. Either way the output is a prefix of a valid stream:
  an incomplete file whose complete segments decode sequentially.

A reader finds the metadata of a streamed file through the trailer
(`prefix.meta_off = 0`).

### 11.2 Reading sequentially (`stream.rs:Reader::new`)

A sequential reader processes sections in file order, applying §9 as they
arrive: it keeps META, INDEX and ADIR (bounded sizes), decodes each
SEGMENT and delivers its bytes once the segment's own checks pass, skips
ATTACH and unknown sections without buffering them, and at the TRAILER
runs the cross-checks of §9 and the whole-demo checks of §5.5 before
reporting the end. A stream that ends without its trailer yields its
complete segments, then an error. Errors are sticky.

---------------------------------------------------------------------------

## 12. Encoder rules

These are what the reference encoder does; a decoder does not depend on
them, but the test vectors are reproduced byte for byte only by an encoder
that follows them (the vectors also depend on libzstd 1.5.7 at level 19).

1. **Cut points** (`segment.rs:cut_points`): with segment size S = 0 the
   whole demo is one segment. Otherwise walk the frames from the start
   (`frame_len`, §6.2) and cut after a frame whenever at least S bytes have
   accumulated since the last cut and bytes remain; once a frame does not
   parse, the rest of the demo is the last segment. The streaming encoder
   cuts the same way on the data it has.
2. **Protocol state**: segment k's `fte`, `fte2`, `mvd1` are the walker's
   state at the end of segment k-1 (initially 0).
3. **Demoinfo sharing**: a segment shares its KTX demoinfo blocks (flag
   `SF_DINFO`, only if it has any) if the store stays within 4 MiB with
   them; otherwise they stay in `hid.misc`.
4. **Back ends** (`segment.rs:pack_best`): normal level, structural and
   numeric streams: rans; if the model breaks M1 or M2, the smaller of
   store and zstd. Blobs, and every stream at the fast level: the smaller
   of store and zstd (store on a tie). Streams are written in id order,
   empty streams omitted. rans tries `steps` 2 and 4 and keeps the smaller
   (2 on a tie).
5. **Segment coding**: transform, unless the walk cannot represent the
   segment, its section would break the ratio limit, or decoding it does
   not give the input (one-shot: the whole file is decoded and compared;
   streaming: each segment). Otherwise zstd if that is smaller than the
   raw bytes and within the ratio limit, else stored.
6. **Model building** (`rans.rs:build_model`): counts per context (the low
   16 bits); a context gets its own table where its entropy plus a table
   cost (16 + 5 bits per present symbol) beats coding it with the shared
   fallback table; two passes (the fallback is rebuilt from the contexts
   that did not get a table); counts are log-quantised (`qlog`) to the
   nearest `qval`. Appendix A gives the coders.
7. **Metadata**: §3 (caps); META is zstd-compressed when that is smaller
   and keeps within the ratio limit. Tag 11 reads the 0x000B block as §3
   says (both forms).
8. **Writer limits**: a writer never writes a file a reader refuses. The
   reference checks the extension metadata before any work, writes at most
   65536 segments (so the INDEX stays far below 2^24 bytes; asking for a
   smaller segment size than that allows is an error, before any work in
   the one-shot encoder), and refuses an attachment update whose directory
   would exceed 2^24 bytes.
9. **Determinism**: the output depends only on the input, the level, the
   segment size and the extension metadata: not on the thread count.

---------------------------------------------------------------------------

## 13. Notes for implementers

1. **Determinism of the walker vs numeric groups.** The reference runs wave
   1 concurrently with the walk (pipelined) and splits groups differently
   by thread count; the bits do not depend on this. A straightforward port
   can: walk, then each wave-1 stream, then snd.o and te.o.
2. **Record list capacity.** The reference sizes record lists from stream
   counts (`Recs::fixed`) and fails if the walk pushes more records than
   that (more PeRecs than `count(pe.bits)`, ...). Every record consumes a
   symbol from the named stream (one pi.num per PiRec, one pe.bits per
   PeRec, one snd.num per SndRec, one te.type per TeRec, one stl.idx per
   StlRec), and over-reading a stream fails the stream's completeness check,
   so a port with growable lists rejects the same files.
3. **An exhausted structural stream returns 255** and marks failure; the
   walk keeps going until a loop sees the 255. A port may stop at once: the
   result is an error either way. The rans decoder does not stop at
   `count` by itself; the completeness check after the walk enforces it.
4. **Arithmetic that could overflow for invalid input** (wraps in the
   reference): `dd(now, prev)`'s `now - prev` and `pred_dt`'s `d * dt`
   (bounded by the clamps for valid data). Use wrapping i32 arithmetic in
   other languages (in Go, compute `dd` in int32 and let it wrap before the
   clamp).
5. **Integer widths are checked**: a `uv` above 2^64 is an error (§1); `fte`,
   `fte2`, `mvd1` above 2^32 are errors; sizes that do not fit the reader's
   address space are limit errors. LEB128 values of store/zstd numeric
   streams are at most 32 bits (§8.1). `esclen` (§6.5) keeps v2's rule: at
   most 5 bytes, bits above 32 of the 5th byte are lost.
6. **rans `normalize` break path.** The `take <= 0 -> break` exit would
   leave a table not summing to 4096; it is unreachable (a sum over 4096
   implies the largest entry is over 1, because at most 256 entries can be
   1).
7. **The rans alphabet is checked** against the stream kind (256 structural,
   65 numeric); a decoded bucket over 64 is an error as well.
8. **PE context subtlety:** the PE_BITS context and `ent[...].bits` use the
   9-bit index `w & 511`; PE_MORE uses `ent[w & 511].model`; everything
   after the FTE extension bytes uses the extended number `num`
   (+512/+1024). `w = (b << 9) | ent` is truncated to 16 bits: the top bit
   of the PE_BITS symbol is discarded. `pe_prev_d` uses the 9-bit `ent` and
   clamps a negative difference to 0.
9. **`pl_off` / `ent_off` may point at 4-byte float coords** if the protocol
   switched to float coords earlier in the same segment (svc 11 mid-segment)
   and back; sounds and TEs then read the low 16 bits of that field as their
   prediction. Deterministic, just unusual. `observe_server_data` takes
   effect in the middle of a payload: it changes `coord` for later commands
   of the same payload.
10. **Sound `kind` 0 is unreachable** (`ent <= 1023 < 2048`), so the
    feature `kind` is 1 or 2.
11. **`count <= raw_len + 16`** applies to numeric counts too (values, not
    bytes); a valid stream always satisfies it.
12. **HIDDMG contexts** are `k | (byte << 14)` with `k` the field index
    0..3.
13. **TE `refs` use `pl_off[e-1]` only for `1 <= e <= 32`**; beams from
    other entities predict their start from 0.
14. **The `TEXT` blob is shared** by svc 8 (after the level byte), 9 and 26;
    all are sized as a NUL-terminated string (`cmd_size(9, ...)`). The
    `PING` and `NAILS` blobs are shared by several commands but sized with
    each command's own rule.
15. **The rANS lane rule**: 4 states, used round-robin over *coded* symbols
    only; a symbol from a single-symbol table does not advance the lane.
    The final-state check (§8.3) needs the states initialised from the
    first 16 bytes even when no symbol is coded.
16. **The TIME context uses the unclamped `last_tb`**; TYPE uses
    `min(tb, 32)`.
17. **`t` is segment-relative**: every walker and predictor state starts
    fresh in each segment, `t` at 0 (not at `t_start`). This changes
    predictions: `dt_prev` of a player's second record in a segment is
    measured from 0.
18. **Per-track timing differs**: in pe.o, `t_last` is per (entity, axis)
    and advances on every record, sent or not; in pi.o/pi.a, `tm` is per
    player and advances on every record.
19. **zstd data**: libzstd accepts several concatenated frames and skips
    skippable frames; so must other decoders (§1). The reference's zstd
    frames carry the content size but no checksum.
20. **The META CRC covers the raw fields**, not the zstd frame: decompress
    first, then check.
21. **Shares and order**: a decoder that keeps only the META (not the
    segments) can still verify the share layout from the segment headers.
    The share of a flagged segment is never empty, so tag 3 is present
    exactly when some segment is flagged.

---------------------------------------------------------------------------

## 14. Security considerations

**Forged identity.** The SHA-256 in META (tag 1) is a claim until a full
decode has recomputed it. Anyone can build a valid file whose META names
another demo's hash (with a correct CRC: CRCs are not authentication), so
a reader that only reads metadata (a catalogue, `mvdz info`, a hub
listing) must not treat the hash as proof of the content, and a cache
keyed by it must verify it once, by a full decode, before trusting it.
The *trusted* decoding option (reference `ReadOptions::trusted`, C
`MVDZ_TRUSTED`, CLI `--trusted`) skips that recomputation and is for
exactly such a cache, never for files from elsewhere. The bundle hash
(§10.2) inherits the same caveat for the attachments' SHA-256s. A
signature scheme must sign the bundle hash only after verifying it.

**Hostile input.** Every size a file declares is bounded by its bytes
(§9, Limits) and a decoder applies its own budget on top, so a small file
cannot make a conforming decoder allocate or work without bound; a
decoder must fail with an error, never abort, hang or touch memory out of
bounds. Duplicate checks and directory parsing must be close to linear;
zstd bodies must be checked against the window rule (§1) before a
streaming zstd decoder buffers them; transient structures (model tables,
parsed directories) count against the budget. The reference is fuzzed on
all its readers and its C interface. The C interface takes no pointers
from files; handles are ids.

**Commands in demos.** A demo replays what the server sent: `svc_stufftext`
(console commands such as `bind`, `alias`, `cmd`, `exec`), print and
centerprint text with control bytes, model and sound names, download
offers. mvdz carries them losslessly and does not filter them: lossless
means unchanged. A client that plays a demo from an untrusted source must
apply the same command filtering it applies to demos and servers (ezQuake
and FTE restrict stufftext from demos); tools that display metadata
(server info, names, the census) must escape it (the reference renders it
as JSON, and the CLI escapes what it prints as text).

**Attachments.** Attachments are untrusted content of any type: the MIME
type is a claim, not a check. Readers must not use names as file paths
(a name may contain `/`, `..` or control characters; the reference never
does), must cap the size they accept (an attachment may be large; its
`size` is known from the directory before it is read), and should open
media in a sandbox or with hardened decoders. Attachments are not
access-controlled: anyone holding a file can list and read all of them,
so check a file's attachments (`mvdz attach ls`) before sharing it, and
export a copy without the ones that must not travel (§10, Exporting).

**Bit rot.** CRC-32C detects every burst error of up to 32 bits and all
but about 1 in 2^32 random corruptions per segment, and a second CRC
covers the whole demo; a full decode also checks sizes, timing and,
unless trusted, the SHA-256. Corruption is therefore detected, not
repaired: there is no redundancy to correct it, and the segment it hits
is lost (other segments still decode by seeking). The CRCs protect the
decoded demo, not the attachments (their SHA-256 does) nor the metadata
beyond the META CRC. Neither CRC nor SHA-256 detects a file that was
replaced as a whole by another valid file; only a hash or signature kept
elsewhere does. Keep a second copy of an archive.

---------------------------------------------------------------------------

## Appendix A. Encoders

**Range encoder** (`coder/rc.rs:Enc`), the inverse of §8.3's decoder
(LZMA style): `low: u64 = 0, range: u32 = 0xFFFFFFFF, cache: u8 = 0,
cache_size = 1`. `bit(b, p)`: `bound = (range >> 12) * p`; b = 1: `range =
bound`; b = 0: `low += bound; range -= bound`; then while `range < 2^24`:
`range <<= 8; shift_low()`. `shift_low()`: if `(low as u32) < 0xFF000000`
or `low >> 32 != 0`: emit `cache + carry` (carry = `low >> 32`), then
`cache_size - 1` bytes of `0xFF + carry`, `cache = (low >> 24) as u8`,
`cache_size = 0`; then `cache_size += 1; low = (low & 0x00FFFFFF) << 8`.
`finish`: `shift_low()` 5 times. The first byte written is always 0.

**IntModel.put(ctx, v)**: `v1 = v + 1` (u64), `n = bitlen(v1) - 1`; code
`n` ones at `P[base + i]` (i = 0..n-1, shift 4), then, if `n < 32`, a zero
at `P[base + n]`; then the bits of `v1` below its leading one, from bit
`n-1` down to 0, at `P[base + 33 + min(i, 30)]` (shift 5).

**Model header**: `put(306, alpha)`, `put(307, steps)`, `put(308, n)`, the
fallback table, then per own table in ascending key order `put(309, key -
previous key)` (the first: the key itself) and the table (`read_table`'s
contexts).

**rANS encoder**: states `X[0..4] = 2^16`. Assign each symbol whose table
is not single-symbol the state `(index among such symbols) mod 4`. Encode
the symbols in reverse order, skipping single-symbol ones: with `freq`,
`cum` of its table, `x = X[j]`; if `x >= ((2^16 >> 12) << 16) * freq`:
emit the word `x & 0xFFFF` and `x >>= 16`; `X[j] = (x / freq) << 12 + x %
freq + cum`. Finally, for j = 3 down to 0, emit `X[j] & 0xFFFF` then
`X[j] >> 16`; reverse the emitted word list and write it as u16le words.
With no coded symbol the body is the 16 bytes of four states 2^16.

**Mantissa bits**: for a value v != 0, `n = bitlen(|v|)`, bucket `2n - 1 +
(v < 0)`, and the `n - 1` bits of `|v|` below its leading one, appended
LSB-first to the bit stream; the last byte is padded with zero bits.
