# CLAUDE.md

Context for working on RustAmp. Read this before changing anything.

## What this is

A headless Plex music player: a Rust daemon that plays audio on the machine
it runs on, serves a web UI for control, and implements enough of the Plex
Companion protocol to appear as a playback target in the Plex/Plexamp phone
apps. Targets Raspberry Pi (32- and 64-bit), Linux x86-64, Windows
(x64/ARM64) and macOS.

Everything currently works: claim-code auth, library browsing (by artist,
album or song title), search, queue playback with prefetch, transport
controls, queue editing, shuffle and repeat, playback reporting to the
server, transcode fallback for codecs Symphonia can't decode, and two-way
sync with the phone app.

## Layout

- `src/main.rs` — everything except audio output: config, Plex API,
  player thread, web server, and the front end as one `INDEX_HTML` string
  constant.
- `src/audio.rs` — output backends behind the `AudioOut` trait.

`main.rs` is large and the embedded HTML/CSS/JS makes it larger. Splitting
it into modules (`plex`, `player`, `web`, `companion`) and moving the front
end into `static/` served via `include_str!` would be welcome, but do it as
its own change, not mixed into a feature.

## Decisions that must not be undone

Each of these cost real debugging. They look arbitrary; they aren't.

1. **Linux uses the ALSA C API directly, not cpal.** On 32-bit ARM, cpal's
   stream setup segfaults inside `alsa::pcm::Status::get_htstamp`, which it
   calls unconditionally. Verified on a Pi 2B across several cpal/alsa
   versions and on both native and cross builds. Non-Linux platforms use
   cpal, which is fine there.

2. **Playback always runs from a server-side Plex play queue**, even when
   started from the web UI (`create_play_queue`). Plex controllers inspect
   a player's queue when you open the browse view, and *stop the player* if
   its timeline reports no `playQueueID`. Do not "simplify" GUI playback to
   a local-only list.

3. **`refreshPlayQueue` fetches without `own=1`.** That flag claims queue
   ownership and can retire the queue we're playing, producing 404s on
   subsequent edits.

4. **The ALSA buffer is capped at ~250 ms, and pending audio is discarded
   on pause/seek/skip.** Without this, controls lag ~5 seconds while queued
   audio drains. `pause_now` rewinds the decoder by however much was
   discarded so resuming doesn't skip.

5. **Prefetch stays exactly one track ahead** — at most two tracks in
   memory. The prefetcher exits when a newer queue serial appears.

6. **Track downloads retry once with freshly fetched metadata on 404.**
   Plex part keys embed a version that goes stale when the server
   re-analyzes a file. A *truncated* body retries the same URL instead:
   the key was fine, the connection died.

7. **Output format is negotiated, not assumed.** `audio::open` returns the
   format it actually got; the player resamples when it differs. WASAPI
   shared mode usually only accepts the device's mix format.

8. **Every play queue mutation is followed by a read-back with `window=`.**
   PMS answers with a 21-item window and *ignores* `window` on the
   POST/PUT/DELETE that change a queue — only a plain GET honours it.
   Without the read-back a long album silently stops after 21 tracks.
   Note PMS caps what a client may see regardless: album-sized queues come
   back complete, a 290-track artist queue returns 200, a 2700-track one
   returns 700–900. Verified against PMS 1.43.4.

9. **The transcode request needs two things that look removable.** It needs
   the header `X-Plex-Client-Profile-Name: Generic`, because
   `X-Plex-Client-Profile-Extra` only *adds* targets to a base profile and
   PMS ships none for a product it has never heard of — without it you get
   400 "unable to find a matching profile". And it needs a `decision` call
   before `start.mp3`, sharing the same session and identical parameters,
   or PMS denies access to "a session lacking decision". Skipping the
   decision fails *intermittently* — the first transcode after a restart
   usually works and later ones 400, and a session can be killed mid-stream
   — which makes the omission look harmless. Sessions also stay open until
   explicitly stopped.

10. **Shuffle mode and Shuffle Play are different features.** Shuffle mode
    is player-side: the queue keeps its order and the player picks the next
    track at random, playing every track once before repeating any. The
    Shuffle buttons on albums/artists and "Shuffle everything" instead ask
    PMS to build a queue that is already shuffled, so the phone's "up next"
    shows the real order. Do not try to implement shuffle mode by
    reordering the queue: PMS has no way to reorder one in place
    (`/playQueues/{id}/shuffle` is a music-provider endpoint that 404s on a
    local server), and a local reorder would desync the phone's view.

11. **Plex's repeat vocabulary is 0 off, 1 *this track*, 2 *the queue*.**
    Not the 1-is-all ordering that gets quoted around. Checked against
    Plexamp; getting it backwards silently swaps the two modes.

12. **Flat listings sort with `sort=title`, not `sort=titleSort`.** The
    latter is what Plex's own UIs use, but it comes back in an order that
    is not alphabetical by anything we display, which looks broken in an
    A–Z list.

## Conventions

- Comments explain *why*, not what. The non-obvious constraints above are
  worth a comment at each site; routine code isn't.
- Errors that a user can act on go to stderr with enough detail to act on
  (which device, which URL — redact tokens with `redact()`). Avoid
  per-track or per-request logging; it was removed deliberately once things
  worked. Log failures with `{e:#}` so anyhow prints the source chain —
  "error decoding response body" alone doesn't say which of two very
  different causes it was.
- The web UI is deliberately dependency-free: no framework, no build step,
  no external assets. Keep it that way.
- The player thread owns all playback state and is driven by `PlayerCmd`
  over a channel; web handlers never touch it directly. Status flows back
  through `SharedStatus`.
- The player thread never touches the network. Downloads happen in async
  handlers and the prefetcher, which hand finished bytes over. Keep it that
  way: a blocking network read in the audio loop would freeze the transport
  controls along with playback.

## Testing

There are no automated tests, and adding meaningful ones would require
faking a Plex server — worth doing if you're touching the Plex API layer
much. Manual checks that catch most regressions:

- Play an album from the web UI; confirm it advances between tracks with
  no gap beyond the expected one.
- Pause/skip/seek and confirm they react within a second.
- Start playback from the web UI, then open the phone app's browse view —
  the player must keep playing (regression test for decision 2).
- Add and remove queue items from both the web UI and the phone; both
  views should agree.
- Play an album with more than 21 tracks and confirm it doesn't stop at 21
  (regression test for decision 8).
- Play a track in a codec Symphonia can't decode (WMA, Opus) and confirm it
  plays; then check `/transcode/sessions` returns to 0 afterwards.
- Turn shuffle mode on and confirm the queue order in the phone's "up next"
  does *not* change while playback jumps around it. Then use an album's
  Shuffle button and confirm the phone's queue order *does* change.
- Set repeat to "this track" and confirm it replays without re-downloading.
- Search, and browse by album and by song title, on a library big enough to
  be slow (thousands of tracks).

## Roadmap

Roughly in priority order.

1. **Loudness leveling.** Plex analyzes tracks for this and Plexamp uses
   it; we ignore it. Read `gainRef`/`loudnessAnalysisVersion` from the
   track metadata and apply gain.

2. **Playlists**, browsing by genre/year, recently added, most played.
   (Browsing by album name and song title, and shuffling a whole artist or
   the whole library, are done.)

3. **Persist state across restarts** so the player resumes where it left
   off.

4. **GDM discovery** (UDP broadcast on 32410-32414) so the player is
   findable on the LAN without going through plex.tv.

5. **Companion pubsub websocket.** We're poll-only, which is why
   `pubsub-player` is deliberately absent from `PROTOCOL_CAPABILITIES` —
   advertising it without implementing it makes controllers relay commands
   we'd drop.

6. **Hardware volume** via the ALSA mixer instead of software gain.

### Considered and deliberately deferred

- **Gapless playback.** The device is still opened and closed per track,
  so there is a real seam between them. Judged inaudible in practice on
  this setup; revisit if a continuous album (live set, DJ mix, opera)
  exposes it. Doing it properly means one device held open across tracks of
  the same format, and reworking decision 4's rewind, which currently
  assumes the device buffer belongs to a single track.

- **Streaming instead of buffering whole tracks.** Measured before
  deciding: Symphonia needs 31 KB of an MP3, 131 KB of a FLAC and 623 KB of
  an M4A to produce the first second of audio, with zero seeks — so
  streaming would work. But at ~25 MB/s on the LAN the full-download wait
  is only 0.16 s (mp3), 0.37 s (aac) and 1.23 s (flac), and only on the
  first track, since prefetch covers the rest. Not worth a blocking network
  read in the player thread plus a new starvation path, and it would cost
  the pre-playback truncation retry in decision 6. Revisit if the daemon
  ever runs somewhere with much slower networking.

Not planned: video, photos, Tidal, speaker groups.

## Gotchas when testing

- ALSA card numbers shift when devices are enabled or disabled in
  `/boot/config.txt`. Re-check `aplay -l` before assuming the code is
  wrong.
- Over Remote Desktop, Windows redirects audio to the connecting machine
  and exposes a virtual device with a narrow format list.
- Claim codes from plex.tv/claim expire in a few minutes.
- The config file at `~/.config/rustamp/config.json` holds a live Plex
  auth token. Never commit it, never paste it in logs or issues.
- A library can hold an unmatched local album (`guid = local://…`) that
  shares a title with a properly matched one, so the same album name
  appears twice with different artwork and track counts. Plex's own clients
  show both. It's a library metadata problem, not a listing bug — don't
  "fix" it by deduplicating on title, which would hide real tracks.
- Transcode sessions do not close themselves. If transcoding starts
  failing with 400, check `/transcode/sessions` for one left open.
