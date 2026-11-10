# Proposal: media attachments (voice, commentary, transcripts)

**Status: archived.** Attachments were removed from the format before 1.0; the FORMAT.md section numbers below refer to 1.0.0-rc.4. This was proposed for version 2, open for discussion. Not part of
mvdz 1.0. Release candidates up to rc.4 carried attachments as a generic container (FORMAT.md
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
* They are personal data: voice in particular has to be protected, not
  just labelled, because a file is copied freely once it is shared (§4).

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
| `href` | an `https` URL | the media is not stored in the file but fetched from this URL (§3, External media). The attachment's stored bytes are then empty; `mime` and the other keys describe the media behind the URL |
| `href_sha256` | 64 lowercase hex digits | the SHA-256 of the bytes behind `href`; required for a fetchable file, so a player can verify what it downloaded |
| `href_size` | decimal | the length of the bytes behind `href`; required with `href_sha256` |
| `encryption` | `age` | the stored bytes are encrypted (§4); `mime` and every other key describe the plaintext. Absent: not encrypted. A reader that does not know the value treats the attachment as opaque bytes |

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
channel that contains it. Voice should be encrypted (§4) unless everyone
heard on it agreed to publish it. Voice can be far larger than the demo: about 29 MB of
Opus for 8 players over 20 minutes, against about 1.4 MB for the
compressed demo; tools that only need the demo should read the demo part
and skip the attachments (FORMAT.md §10: readers never load attachments to skip them).

**Commentary.** Like voice (Ogg Opus, stored), with `role=commentary`;
`player`, `slot` and `team` are absent; `speaker` names the caster and
`lang` the language. Several commentary tracks per demo are allowed (e.g.
pulled from several Twitch or YouTube VODs); they are told apart by
`speaker` and `lang`. `source` holds the VOD URL. Commentary is normally
not encrypted: it was broadcast.

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

**External media.** A track can be referenced instead of embedded: an
attachment with empty stored bytes and an `href`. A track is strictly one
or the other: an attachment with both stored bytes and `href` is
malformed, and readers ignore its `href`. This keeps one authoritative
copy per track.

* **A fetchable file** (a voice track on the hub, an Opus file on any
  web server) carries `href_sha256` and `href_size`. A player fetches the
  file (a web player can stream it with HTTP range requests), checks the
  size and the hash, and refuses a mismatch. It may be encrypted
  (`encryption`), so the host holds only ciphertext.
* **A stream link** (a YouTube or Twitch VOD) has no stable bytes to
  hash, so it carries no `href_sha256`; it can be opened but not
  verified, and is played through the platform's player, synced by the
  timing keys. `source` keeps its meaning (where a track came from, for
  credit); `href` is where its media is.

Why: voice is often about 20 times the size of the demo, and commentary
on a video platform cannot be embedded at all. A reference keeps the
`.mvdz` small, fetches media only when it is played, and lets the host
withdraw access later (which embedding cannot). The costs: URLs rot
(an archive that must last should embed, or host the media itself),
fetching tells the media host who is watching, a web player needs the
host to allow cross-origin requests, and offline use needs the media
downloaded first. The bundle hash (FORMAT.md §10) covers the reference,
and through `href_sha256` the media it names.

**Embedding a referenced track.** A tool that downloads the media of a
fetchable reference (an archive, a mirror) may replace the reference with
an embedded attachment through an ordinary attachment update: same name
and keys, the downloaded bytes stored, and `href`, `href_sha256` and
`href_size` removed. The stored bytes then have the SHA-256 the reference
named.

**Transcripts.** WebVTT (`text/vtt`), `role=transcript`, `ref` = the name
of the voice or commentary attachment it transcribes, `lang`; cue times are
on the media clock of that track and map to demo time through that track's
timing keys (the transcript may repeat them).
Speaker tags (`<v name>`) as in WebVTT. A transcript of an encrypted
track should be encrypted to the same recipients.

**Privacy.** An attachment without `encryption` is public: anyone holding
the file can read it. An encrypted one is safe to copy, mirror and
download with the file. Tools that redistribute files therefore need no
filtering rule for privacy; export by name or prefix (FORMAT.md §10)
remains for leaving large tracks out of a download.

**Known attachment types** (non-normative):

| role | MIME | content |
|---|---|---|
| `voice` | `audio/ogg; codecs=opus` | a player's or team channel's voice (above) |
| `commentary` | `audio/ogg; codecs=opus` | a caster's commentary (above) |
| `transcript` | `text/vtt` | a transcript of a voice or commentary track (above) |
| `analysis` | a vendor MIME type, e.g. `application/vnd.mvda.result+json` | an analysis result of the mvd-analyzer project, zstd-compressed (codec 1); its schema is owned by that project, not by mvdz; several versions may coexist under different names |

Producers of other data use their own vendor MIME types and schemas.


## 4. Encryption

Private media is protected by encrypting it, not by a label. A label
(the 1.0 drafts had `visibility`: `public`, `team` or `private`) only
takes effect where a server holds the file and serves filtered copies;
once the file is shared, anyone holding it can extract everything.
Encryption keeps holding on every copy and mirror.

**Format.** `encryption=age`: the attachment's bytes are an age v1 file
(<https://age-encryption.org/v1>). age is small, has maintained
libraries for Rust, Go and JavaScript, and a command-line tool. It takes
either of two kinds of recipient, never both in one file (age requires a
passphrase to be the only recipient):

* **X25519 keys**, any number: each player, a team key, a service's key.
  A team channel is encrypted once to all of them.
* **A passphrase** (scrypt): the simplest for a team that shares one
  password.

**Container.** The ciphertext is stored with codec 0 (it does not
compress; Opus is compressed already, and a writer that wants to
compress a transcript does so before encrypting). The ADIR `size` and
`sha256` are of the ciphertext, so the hash reveals nothing about the
plaintext, and the bundle hash (FORMAT.md §10) covers the encrypted
bytes. mvdz itself does no cryptography: it stores and extracts bytes;
the tools that make and play the tracks encrypt and decrypt.

**Keys stay in clear.** `name`, `mime`, `role`, `player`, `slot`, `team`,
`speaker`, `lang`, the timing keys and the size are not encrypted. This
is deliberate: they say no more than the demo itself (who played, in
which team, for how long), and they let tools list and sync tracks
without a key. The size shows roughly how long a track is.

**Changing access** is an ordinary attachment update (FORMAT.md §10):
an **embargo** is a track encrypted to an organiser's or a service's key
and replaced by its plaintext later; adding a recipient means
re-encrypting and replacing the attachment.

**Limits.**

* **Key distribution.** Players have no keys today. A service that keeps
  a key per account (and encrypts to the players of a match when the
  track is uploaded) can make this automatic; that service is then
  trusted with access, but copies of the file stay protected.
* **No revocation.** Whoever decrypted a track once keeps the plaintext.
  Replacing or re-encrypting the attachment protects only later copies.

**Reference use: a web viewer.** A site such as mvdanalyzer.com serves
the file unchanged; it holds only ciphertext. The page lists the
encrypted tracks from their clear keys; the viewer pastes a passphrase
or an age identity (`AGE-SECRET-KEY-1...`); the page decrypts in the
browser and plays the track in sync with the demo. The key and the
plaintext never reach the server; the page may remember the key in the
browser. Caveat: the page's code comes from the server, so a compromised
server could capture keys as they are typed. A viewer who does not trust
the site decrypts with the age tool instead.

Tools may still offer an export selection by `role` (keep these roles),
combined with the name filters so that every given condition must hold.

## 5. Validation

The draft writer refused malformed values of these keys: decimals as
defined in §2, `slot` 0-255, `encryption` a known value whose bytes are
stored with codec 0 and (for `age`) start with the age header line
`age-encryption.org/v1`, `href` an `https` URL on an attachment whose
stored bytes are empty (and `href_sha256` and `href_size` both present
or both absent), `lang`
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
5. Encryption: is age the right choice (rather than OpenPGP), and should
   passphrases be allowed (simple for a team, but a passphrase file
   cannot also be opened by a key)?
6. Should player names be matched as Latin-1 code points of the demo's
   bytes (as in FORMAT.md §3.2) or in the QuakeWorld character set?
7. One VOD over several demos: one copy per demo with its own map, or a
   shared attachment referenced from each demo?
8. Automatic sync from `svc_sound` events against the recorded audio: in
   scope for the format, or purely a tool?
9. Does `ref` need to be in the format at all (transcripts are its only
   use), and what should a reader do with a dangling `ref`?
10. age does not reveal who a file is encrypted to. Does a viewer need a
    hint of which key opens a track (a free-text `key_hint`, e.g. "team
    blue password"), and is a hint itself a leak?
11. External media: is `https` the only scheme worth allowing? Should a
    file be allowed to list several URLs for the same bytes (mirrors), all
    verified by the same `href_sha256`?

**Feedback**: open an issue on GitHub
(<https://github.com/galfthan/mvdz/issues>).
