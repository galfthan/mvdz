# Proposal: media attachments (voice, commentary, transcripts)

**Status: proposed for version 2, open for discussion.** Not part of
mvdz 1.0. Version 1 carries attachments as a generic container (FORMAT.md
§10): a name, a MIME type, free-form key/values with prefixed keys, the
stored bytes, crash-safe add and remove, listing and extraction without
decoding, export of a copy with some attachments, and the bundle hash.
This document collects what a version 2 could add on top for media that
belongs to a demo: player voice, caster commentary, transcripts, and the
timing that lines them up with the demo. It was drafted during the 1.0
reviews and taken out of the format so that it can be debated first.

Because 1.0 reserves every unprefixed key for future versions of the
specification (FORMAT.md §10.1), the keys below can be defined by a
version 2 without colliding with keys already written by tools
(`x-...`, `vendor....`). Until then a tool that wants to experiment
writes them with its own prefix (e.g. `x-role`).

## 1. Why

* Voice and commentary make a demo far more watchable and are hard to
  find again later; storing them beside the demo keeps them together.
* They are large: about 29 MB of Opus for 8 players over 20 minutes,
  against about 1.4 MB for the compressed demo (voice is often about 20
  times the demo). Version 1's export (by name and name prefix) already
  lets a download leave them out.
* They are personal data: voice in particular needs a privacy default.

## 2. Keys

All keys are optional; other keys are allowed and preserved as is. Under
this proposal these keys would be unprefixed, defined by version 2.
Values are UTF-8 strings. A *decimal* is ASCII digits without leading
zeros (`0` itself allowed), preceded by `-` only where the table says
signed (`-0` is not allowed), in the range of an i64 (signed) or of the
stated range. Names taken from the demo (`player`, `team`) are the demo's
bytes as Latin-1 code points (each byte becomes the code point of the
same value, then UTF-8), as in FORMAT.md §3.2, so a reader can match them
to the roster and the demo exactly. Writers should check these values
(§5); a reader ignores a reserved key whose value is malformed, as if it
were absent.

| key | value | meaning |
|---|---|---|
| `role` | open vocabulary, non-empty | what the attachment is: `voice`, `commentary`, `transcript`, `analysis`, `screenshot`, `sidecar`, or another value |
| `time_offset_ms` | signed decimal | the demo time (ms, §3) at which the attachment's own time 0 falls: demo time = attachment time + `time_offset_ms` |
| `time_map` | JSON (§3) | a piecewise-linear map from media time to demo time; wins over `time_offset_ms` |
| `wallclock_start_ms` | decimal | the Unix ms at which the media starts (e.g. a VOD's reported start), for a coarse sync through FORMAT.md §3.1; used only without `time_map` and `time_offset_ms` |
| `player` | string | the player's name as written in the demo (Latin-1 code points) |
| `slot` | decimal 0-255 | the player's slot (svc_playerinfo number) |
| `team` | string | the team name as written in the demo (Latin-1 code points) |
| `speaker` | string | a display name for a track that does not belong to a player slot (a caster, a coach) |
| `lang` | BCP 47 language tag (subtags of 1-8 ASCII letters or digits joined by `-`, at most 35 bytes) | the spoken or written language (`en`, `sv`, `pt-BR`) |
| `ref` | an attachment name of the same file | the attachment this one belongs to (a transcript names the voice or commentary track it transcribes). Writers refuse a new attachment whose `ref` names nothing; removing or not exporting the attachment it names leaves the `ref` dangling, which readers treat as absent |
| `source` | string | provenance: where the attachment came from; for a voice or commentary track the URL of the original recording (e.g. a VOD link), so the speaker can be credited and linked |
| `created` | RFC 3339 timestamp | when the attachment was made |
| `visibility` | `public`, `team` or `private` | who may see it (§3). **Advisory**: mvdz has no encryption or access control, and anyone holding the file can list and extract every attachment. The label takes effect only where a server holds the full file and serves filtered copies (FORMAT.md §10, Exporting). Typical uses: an embargo (the uploader sets `public` later with an ordinary update) and a statement of the uploader's intent for tools and mirrors |

`created` is `YYYY-MM-DDTHH:MM:SS[.frac]` and `Z` or `±HH:MM`.

## 3. Conventions

Readers skip attachment types they do not know. Name attachments with a
prefix per kind or producer (`voice/...`, `commentary/...`,
`transcript/...`), so readers can filter by prefix.

**Demo time.** Demo time 0 is the start of the demo file: the time before
its first frame. The demo time of a frame is the sum of the time bytes
(first byte of each frame) of all frames up to and including it; this is
the time the index, the metadata `duration_ms` and seeking use.
`time_offset_ms` is the demo time at which the recording starts (its
sample 0); it may be negative when the recording started before the demo
(or, for commentary, when it was recorded against a delayed stream and
aligned afterwards).

**Media time** is the attachment's own clock, in ms from its origin: for
Ogg Opus, the PCM sample position (granule position minus pre-skip, RFC
7845 §4) divided by 48, so media time 0 is the first sample that is
played after the pre-skip, and a stream cut from a live source whose
first page starts at a later granule position starts at that later media
time; for WebVTT, the cue timestamps; for other media, the format's own
presentation time.

**Voice.** One Ogg Opus stream per recording: MIME `audio/ogg;
codecs=opus`, `role=voice`, stored (codec 0: Opus gains nothing from
zstd), `time_offset_ms` as above. The speaker is identified by `player`
and `slot` (and `team`) for a recording of one player; a recording of a
team channel (several players mixed) carries `team` only, without
`player` and `slot`. A file may hold one track per player, one per team
channel, or both; players should not mix a player's own track with a team
channel that contains it. `visibility` defaults to `private` (absent means
private) for voice. Voice can be far larger than the demo: about 29 MB of
Opus for 8 players over 20 minutes, against about 1.4 MB for the
compressed demo; tools that only need the demo should read the demo part
and skip the attachments (FORMAT.md §10: readers never load attachments to skip them).

**Commentary.** Like voice (Ogg Opus, stored), with `role=commentary`;
`player`, `slot` and `team` are absent; `speaker` names the caster and
`lang` the language. Several commentary tracks per demo are allowed (e.g.
pulled from several Twitch or YouTube VODs); they are told apart by
`speaker` and `lang`. `source` holds the VOD URL. `visibility` defaults to
`public` for commentary.

**Timing of a track.** Three ways, in this order of precedence:

1. `time_map`: a stream's timeline drifts from the demo's (map breaks,
   replays, ads, pauses), so one offset is not enough. The value is JSON
   (writers write it without whitespace; readers accept any valid JSON):
   a non-empty array of *runs*, each an array of at least two anchors
   `[media_ms, demo_ms]` (integers of magnitude below 2^53). Taken over all
   runs in order, both coordinates strictly increasing: within a run, and
   from the last anchor of a run to the first of the next (runs do not
   overlap on either clock), e.g.
   `[[[0,5000],[600000,605000]],[[900000,605001],[1500000,1205000]]]`.
   A run covers the closed intervals [first, last anchor] on both clocks.
   Within a run, media time m between anchors (m0, d0) and (m1, d1) maps to
   `d0 + floor((m - m0) x (d1 - d0) / (m1 - m0))`; a player that seeks by
   demo time uses the inverse, demo time d in [d0, d1] to media time
   `m0 + ceil((d - d0) x (m1 - m0) / (d1 - d0))` (the first media time
   that maps to d or later). Media time outside every run (before the
   first anchor, after the last, or between runs: a break, an ad, a
   replay), and demo time outside every run, is not played. A map that
   breaks any of these rules is ignored as a whole (the track falls back
   to `time_offset_ms`, then `wallclock_start_ms`, else it is unsynced).
   It is a key rather than a companion attachment because it describes
   this one track, is small (a few anchors per break), and must travel
   with the track when the track is replaced or copied; a separate
   attachment would need its own lifecycle and a lookup.
2. `time_offset_ms` (the simple case): demo time = media time +
   `time_offset_ms`, possibly negative (a recording started before the
   demo, or commentary aligned against a delayed stream).
3. `wallclock_start_ms`: media time 0 is at that Unix ms; convert with the
   demo's wall-clock time (FORMAT.md §3.1). Coarse (VOD start times are approximate,
   and the demo side may have only 1 s precision).

Writers of a `time_map` should also write `wallclock_start_ms` when they
know it (a VOD's reported start), for readers without the map. A VOD that
spans several demos (a whole evening of matches) is attached to each demo
it covers, each copy with its own `time_map` (or its own cut of the media);
the same `source` ties them together. Tools can derive anchors by matching
the demo's `svc_sound` events (their demo times are exact) against the
recorded audio; this is a suggestion, not part of the format.

**Transcripts.** WebVTT (`text/vtt`), `role=transcript`, `ref` = the name
of the voice or commentary attachment it transcribes, `lang`; cue times are
on the media clock of that track and map to demo time through that track's
timing keys (the transcript may repeat them).
Speaker tags (`<v name>`) as in WebVTT. `visibility` defaults to that of
the track it transcribes.

**Privacy.** Tools that redistribute files (downloads, exports, mirrors)
must drop every attachment that is not `public`. When `visibility` is
absent (or malformed), only these are public: `role=commentary` (it was
broadcast); `analysis`, `screenshot` or `sidecar` whose MIME type is not
`audio/*` (compared without case); a `transcript` whose `ref` names an
attachment of the same file that is public by these rules (a chain of
refs is followed at most 8 deep). Everything else is private: voice, any
audio without a public role, an absent or unknown `role`, a transcript of
a private or missing track. The draft implemented this rule as `is_public` (§4). `team` means visible to the players
of that team in this demo (the `team` key, else the team of the player in
`slot`); a tool that does not know who is asking treats it as private.

**Known attachment types** (non-normative):

| role | MIME | content |
|---|---|---|
| `voice` | `audio/ogg; codecs=opus` | a player's or team channel's voice (above) |
| `commentary` | `audio/ogg; codecs=opus` | a caster's commentary (above) |
| `transcript` | `text/vtt` | a transcript of a voice or commentary track (above) |
| `analysis` | a vendor MIME type, e.g. `application/vnd.mvda.result+json` | an analysis result of the mvd-analyzer project, zstd-compressed (codec 1); its schema is owned by that project, not by mvdz; several versions may coexist under different names |

Producers of other data use their own vendor MIME types and schemas.


## 4. Visibility is advisory

mvdz has no encryption or access control: anyone holding a file can list
and extract every attachment in it, whatever its `visibility`. The label
only takes effect where a server holds the full file and serves filtered
copies (FORMAT.md §10, Exporting); anything inside a file that is
distributed must be treated as public. Typical uses: an **embargo** (the
uploader stores the track `private` and switches it to `public` later
with an ordinary attachment update), and a statement of the uploader's
intent for tools and mirrors.

The draft implementation (removed before 1.0) had `is_public(entry,
directory)` implementing the privacy rule of §3, and an export selection
`--public` (keep what `is_public` allows) and `--role r` (keep these
roles), combined with the name filters so that every given condition
must hold.

## 5. Validation

The draft writer refused malformed values of these keys: decimals as
defined in §2, `slot` 0-255, `visibility` one of the three values, `lang`
as subtags of 1-8 ASCII letters or digits joined by `-` (at most 35
bytes), `created` as `YYYY-MM-DDTHH:MM:SS[.frac]` with `Z` or `±HH:MM`,
`time_map` as in §3, and a new attachment whose `ref` names no attachment
of the resulting file. Readers would ignore a malformed value, as if the
key were absent.

## 6. Open questions

1. Should these keys be part of the format (version 2) or a separate
   companion specification that tools opt into?
2. Is `role` the right axis, or should the MIME type plus a few keys
   suffice? Is an open vocabulary for `role` acceptable?
3. Timing: is `time_offset_ms` plus `time_map` enough? Should the inverse
   mapping (demo time to media time, needed by players that seek by demo
   time) be normative as written, and how should an invalid map degrade?
4. Media time origin for Ogg Opus (granule position minus pre-skip) and
   for streams cut from a live source: is the rule in §3 what players
   (ezQuake, FTE, web players) can implement?
5. Privacy default: private unless the role is commentary (or a public
   transcript, or a non-audio analysis): too strict, too loose? What
   should `team` mean when a tool does not know who is asking?
6. Should player names be matched as Latin-1 code points of the demo's
   bytes (as in FORMAT.md §3.2) or in the QuakeWorld character set?
7. One VOD over several demos: one copy per demo with its own map, or a
   shared attachment referenced from each demo?
8. Automatic sync from `svc_sound` events against the recorded audio: in
   scope for the format, or purely a tool?
9. Does `ref` need to be in the format at all (transcripts are its only
   use), and what should a reader do with a dangling `ref`?
10. Is visibility worth carrying at all, given that it is advisory?

**Feedback**: open an issue on GitHub
(<https://github.com/galfthan/mvdz/issues>).
