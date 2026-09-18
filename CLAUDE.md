# CLAUDE.md

Context for working on RustAmp. Read this before changing anything.

## What this is

A headless Plex music player: a Rust daemon that plays audio on the machine
it runs on, serves a web UI for control, and implements enough of the Plex
Companion protocol to appear as a playback target in the Plex/Plexamp phone
apps. Targets Raspberry Pi (32- and 64-bit), Linux x86-64, and Windows
(x64/ARM64).

Everything currently works: claim-code auth, library browsing, queue
playback with prefetch, transport controls, queue editing, and two-way sync
with the phone app.

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
   re-analyzes a file.

7. **Output format is negotiated, not assumed.** `audio::open` returns the
   format it actually got; the player resamples when it differs. WASAPI
   shared mode usually only accepts the device's mix format.

## Conventions

- Comments explain *why*, not what. The non-obvious constraints above are
  worth a comment at each site; routine code isn't.
- Errors that a user can act on go to stderr with enough detail to act on
  (which device, which URL — redact tokens with `redact()`). Avoid
  per-track or per-request logging; it was removed deliberately once things
  worked.
- The web UI is deliberately dependency-free: no framework, no build step,
  no external assets. Keep it that way.
- The player thread owns all playback state and is driven by `PlayerCmd`
  over a channel; web handlers never touch it directly. Status flows back
  through `SharedStatus`.

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

## Roadmap

Roughly in priority order. Items 1 and 2 are the ones a user would notice
missing first.

1. **Report playback to Plex.** Nothing is currently recorded: no play
   counts, no last-played, no "continue listening", no Last.fm scrobbling.
   Needs periodic `POST /:/timeline` to the server with
   `ratingKey`/`key`/`state`/`time`/`duration`, plus a `playing` ping on
   start and `stopped` at the end. Small change, biggest user-visible win.

2. **Search.** `GET /library/sections/{key}/search?query=` or the global
   `/hubs/search`. The UI needs a search field in the header and a results
   view grouping artists/albums/tracks.

3. **Gapless playback.** Currently the device is opened and closed per
   track. Keep one device open across tracks of the same format and feed
   the next decoder straight into it.

4. **Shuffle and repeat.** The timeline hardcodes both to `0`. Needs
   player-side support plus honoring `setParameters?shuffle=/repeat=` from
   the phone.

5. **Transcode fallback.** We direct-play only and fail on anything
   Symphonia can't decode. Fall back to
   `/music/:/transcode/universal/start.mp3` when direct play fails.

6. **Stream instead of buffering whole tracks.** Currently each track is
   downloaded fully into memory before playing, which is why the first
   track has a noticeable delay.

7. **Loudness leveling.** Plex analyzes tracks for this and Plexamp uses
   it; we ignore it. Read `gainRef`/`loudnessAnalysisVersion` from the
   track metadata and apply gain.

8. **Playlists**, browsing by genre/year, recently added, most played.

9. **Persist state across restarts** so the player resumes where it left
   off.

10. **GDM discovery** (UDP broadcast on 32410-32414) so the player is
    findable on the LAN without going through plex.tv.

11. **Companion pubsub websocket.** We're poll-only, which is why
    `pubsub-player` is deliberately absent from `PROTOCOL_CAPABILITIES` —
    advertising it without implementing it makes controllers relay
    commands we'd drop.

12. **Hardware volume** via the ALSA mixer instead of software gain.

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
