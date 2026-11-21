# mvdz-meta

What a QuakeWorld MVD demo contains, read from the demo itself: map and
server settings, the wall clock, pauses, the players at the start and
every later change, KTX statistics, and a census of the demo's messages.
It reads `.mvdz` (decoded first), `.mvd` and `.mvd.gz`.

None of this is stored in the `.mvdz` format, whose header holds only what
decoding needs (FORMAT.md §3). The output here is informative: it reports
what the demo says, but its layout may change between versions, and a
service that needs these facts should keep them in its own database.

```
mvdz-meta [--json] files-or-dirs...
```

A directory means the `.mvdz`, `.mvd` and `.mvd.gz` files directly in it.
A file that does not read, or is not a demo, is reported on standard error
and the exit status is 1; the other files are still shown.

## Text

```
game.mvdz
  duration   10:12.9 (31495 frames)
  demo       1460218 bytes, sha256 42d91b1c...
  map        dm2 "Claustrophobopolis"
  hostname   Splatterfest Team 2
  deathmatch 3
  teamplay   2
  timelimit  10
  matchkey   236-2004-7-23:23-13-22 (at 0:10.0)
  players at start:
      0  vent                 pink   userid 6742  spectator
      1  fistx                red    userid 6716
      ...
  player changes:
       0:10.0  slot 5   milamber: setinfo topcolor=13
       6:24.1  slot 2   slk joined as spectator (userid 6756)
       7:23.7  slot 6   *lotte* left
       ...
```

Names are shown with the high (coloured) bit dropped and the console's
bracket and digit glyphs as ASCII; any other control byte is shown as `_`,
so a demo cannot send escapes to the terminal. A player *joins* when a slot
gets a userinfo with a new userid, *leaves* when its userinfo is cleared,
and is a *spectator* when the userinfo has `*spectator` set (not empty and
not `0`).

## JSON

`--json` writes one object per demo, one per line. Byte strings are JSON
strings of Latin-1 code points (each byte becomes the code point of the
same value, so the bytes are recoverable).

| key | value |
|---|---|
| `file` | the path given |
| `sha256`, `size`, `frames`, `duration_ms` | the demo's identity and timing |
| `truncated` | the demo ends in bytes that do not form a whole frame |
| `map`, `levelname` | the first svc_modellist entry and the level name of the first svc_serverdata |
| `serverinfo` | the first `fullserverinfo` as `{key: value}` |
| `clock` | `{start_timestamp_ms, epoch, matchdate, matchkey, pauses, pauses_truncated}` (below) |
| `roster` | the players at the first svc_playerinfo: `[{slot, userid, name, team, spectator, userinfo: {key: value}}]` |
| `players` | `{truncated, start, changes}`: every userinfo change (below) |
| `demoinfo` | the KTX statistics JSON (below), or `null` |
| `census` | the census (below) |

## Player log

`players.changes` lists, in demo order, every svc_updateuserinfo as
`{t, slot, userid, userinfo}` (the whole userinfo string; empty when the
slot is freed) and every svc_setinfo as `{t, slot, key, value}`. `t` is
the demo time in ms. The first `start` changes come before the first
svc_playerinfo (all of them if there is none) and make up the roster; the
rest are joins, drops, reconnects, renames and setinfo during the game.
The log stops after 65536 changes or 1 MiB of strings, with `truncated`
true.

## Wall-clock time

The sources, each with the demo time it appeared at:

| key | source |
|---|---|
| `start_timestamp_ms` | `[demo_ms, unix_ms]`: the value of the first hidden block 0x000B (mvdhidden_demo_start_timestamp_ms: Unix milliseconds when the server opened the demo, mvdsv `sv_demo_misc.c` `SV_MVDEmbedStartTimestamp`) and the demo time it appeared at. The block body has two forms: current mvdsv writes a ULEB128, earlier builds a fixed 8-byte u64le. A body of exactly 8 bytes whose ULEB128 does not end at its 8th byte (a byte below 0x80 earlier) is the u64le; any other body is read as ULEB128 over at most its first 10 bytes, stopping after the first byte below 0x80 or at the end of the body |
| `epoch` | `[demo_ms, raw]`: the first value of the serverinfo key `epoch` (Unix seconds as decimal text, set by mvdsv when the demo starts, `sv_demo.c`), from the initial `fullserverinfo` or a later svc_serverinfo, verbatim (at most 128 bytes), and its demo time |
| `matchdate` | `[demo_ms, raw]`: the first svc_print whose text starts with `matchdate:` (KTX `match.c`, `matchdate: %Y-%m-%d %H:%M:%S %Z`, sent at match start): the text after the prefix up to the newline, trimmed, verbatim (at most 128 bytes), and the demo time of the print |
| `matchkey` | `[demo_ms, raw]`: the first svc_print whose text starts with `matchkey:` (older mods), the text after the prefix: svc_prints that directly follow in the same frame are joined (any other command, or the next frame, ends the line), up to 256 bytes, cut at the newline, trimmed of ASCII whitespace and cut to 128 bytes. Kept raw: it is not a date in any one format |

Demo time (the sum of frame times) stops while a server is paused: mvdsv
writes paused frames with time byte 0 and, for each, a hidden block of
type 0x000A (mvdhidden_paused_duration, `sv_demo.c`
`SV_MVDWritePausedTime`) whose one body byte is the real milliseconds that
frame took. Current mvdsv writes the framed form, `u32le 1 | u16le 0x000A
| u8 ms`; older builds wrote it without the block's length field (the
dem_multiple(0) payload is exactly the 3 bytes `u16le 0x000A | u8 ms`).
Both forms count. The scan adds every such block's ms to a *run* at the
current demo time (paused frames all share one demo time), so
`clock.pauses` lists one `[demo_ms, paused_ms]` per pause. At most 65536
runs are kept; `pauses_truncated` is true if more existed.

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
| 1 | `start_timestamp_ms` | 1 ms | `unix_ms` |
| 2 | `epoch` | 1 s | `1000 x epoch` (decimal digits, optionally surrounded by ASCII spaces; anything else: unusable) |
| 3 | `matchdate` | 1 s | `parse(raw) - demo_ms - P(demo_ms)` (below) |
| 4 | outside sources (a hub timestamp, a date in the file name) | as stated | for example extension keys a writer stored in the .mvdz (`mvdz info`); rank last |

The start timestamp and `epoch` are the server's clock when it opened the
demo file (mvdsv sets `epoch` and writes the 0x000B block while writing the
demo's first messages, before any frame time has passed), so they are
taken as the wall clock at demo time 0, whatever their `demo_ms` (the
block typically arrives at about 100 ms; `demo_ms` is kept for
reference). The `matchdate` print is sent later, at match start, so its
own demo time and the pauses before it are subtracted.

**Parsing `matchdate`** (so that tools agree): `raw` is
`YYYY-MM-DD HH:MM:SS` (24-hour), one space, and a zone, which is a numeric
offset `+HHMM`, `-HHMM`, `+HH:MM` or `-HH:MM`, or one of `UTC`, `GMT`,
`Z` (+0), `WET` (+0), `WEST` (+1), `CET` (+1), `CEST` (+2), `EET` (+2),
`EEST` (+3), `MSK` (+3). Anything else (other zone names, locale or
ctime-style text) is unparseable: the source is then unusable, and a
reader goes on to the next one.

A pause list that is truncated (`pauses_truncated`) makes `P(t)` exact
only up to the last run listed.

**KTX stats (demoinfo)** (`scan.rs:demoinfo_json`). KTX writes its match
statistics as JSON in hidden blocks of type 0x0003. They are reassembled
as demo tools commonly do: each block body is `u16le block
number | text` (a body shorter than 2 bytes: no JSON); for each block
number the *last* block wins (a demo can carry the demoinfo more than
once); then the texts of blocks 1, 2, ..., n in ascending order, then
block 0 (the last block), are concatenated.

## Census

A compact JSON object (about 4-8 KB) describing what the demo contains,
from a scan of every frame and message (`scan.rs:Scanner`), in the
`census` key of `--json`. Do not depend on its exact content.

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
demo_start_timestamp_ms   value of the first hidden 0x000B block (read as
                 below); only if there is one
clock            {[start_timestamp_ms: [demo_ms, unix_ms]], [epoch: [demo_ms, "raw"]],
                  [matchdate: [demo_ms, "raw"]], pause_runs, paused_ms} as above
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
sizer (FORMAT.md §6.2) and the protocol state it tracks (svc_serverdata updates it);
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
the `serverinfo` shown.
