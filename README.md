# RustAmp

A headless Plex music player written in Rust. It runs as a daemon on the
machine connected to your speakers, plays audio locally, and is controlled
from any browser on the network — or from the Plex/Plexamp app on your
phone, which sees it as a normal playback target.

Not affiliated with or endorsed by Plex. This is an independent client
built against Plex's HTTP APIs.

## What works

- Claim-code login (the `plex.tv/claim` flow), token cached on disk
- Browse music libraries: artists → albums → tracks, with artwork
- Gapless-ish queue playback with one-track-ahead prefetch
- Web UI: play, pause, skip, previous, seek, volume, queue view, add
  to queue / play next, remove from queue
- Plex Companion: the player appears in the phone app and can be
  controlled from it; queue edits sync both directions
- Runs on Linux (x86-64 and ARM, including Raspberry Pi), Windows
  (x64 and ARM64), and macOS

## Requirements

- A Plex Media Server with a music library
- Rust (stable)
- **Linux:** ALSA development headers — `apt install libasound2-dev`
- **Windows:** the MSVC toolchain and Visual Studio Build Tools. Audio
  needs no extra dependency; cpal uses WASAPI, which ships with Windows.

## Build

The simplest approach is to build on the machine that will run it:

```
cargo build --release
```

The binary lands in `target/release/plexamp-rs` (`plexamp-rs.exe` on
Windows). This works on every supported platform — Linux x86-64, Linux ARM
(Raspberry Pi, 32- or 64-bit), Windows x64, Windows ARM64, macOS — and
avoids the glibc caveat described below.

### Cross-compiling

Building natively on a Raspberry Pi 2B works but is slow, especially with
link-time optimization enabled in the release profile. To build on a faster
machine instead, you need [`cross`](https://github.com/cross-rs/cross) and
Docker:

```
cargo install cross --git https://github.com/cross-rs/cross
```

Each target needs its image built once. The stock `cross` images don't
include the ALSA headers this needs at link time, which is what the
`Dockerfile.*` files add.

| Target | Hardware | Image build | Cross build |
|---|---|---|---|
| `armv7-unknown-linux-gnueabihf` | 32-bit Raspberry Pi OS (Pi 2/3/4/5, Zero 2) | `docker build -t rustamp-cross-armv7 -f Dockerfile.armv7 .` | `cross build --release --target armv7-unknown-linux-gnueabihf` |
| `aarch64-unknown-linux-gnu` | 64-bit Raspberry Pi OS (Pi 3/4/5, Zero 2) | `docker build -t rustamp-cross-aarch64 -f Dockerfile.aarch64 .` | `cross build --release --target aarch64-unknown-linux-gnu` |
| `x86_64-unknown-linux-gnu` | Linux PC | `docker build -t rustamp-cross-x86_64 -f Dockerfile.x86_64 .` | `cross build --release --target x86_64-unknown-linux-gnu` |
| `armv7-unknown-linux-musleabihf` | 32-bit Raspberry Pi OS, static — see below | `docker build -t rustamp-cross-armv7-musl -f Dockerfile.armv7-musl .` | `cross build --release --target armv7-unknown-linux-musleabihf` |

Binaries land in `target/<target>/release/plexamp-rs`.

Use `cross`, not `cargo`, for these. Running `cargo build --target
armv7-...` looks for an ARM linker on the host and fails with
`arm-linux-gnueabihf-gcc: program not found`.

**glibc caveat:** a cross-compiled binary links against the glibc version
in the cross image, so it needs a target system at least that new. These
images generally produce binaries that run on current Debian, Ubuntu, and
Raspberry Pi OS, but an older distro may fail with `GLIBC_2.xx not found`.
Building natively on the target avoids this — or use the musl target below,
which has no glibc dependency at all.

**Static musl build (32-bit Pi):** `armv7-unknown-linux-musleabihf` links
everything statically, so the binary carries no glibc dependency and runs on
old Raspberry Pi OS installs that the gnueabihf build fails on. Its image
takes longer to build than the others: no distro ships a musl armhf
`libasound`, so `Dockerfile.armv7-musl` compiles alsa-lib from source
against the image's musl toolchain. Only the PCM and mixer parts are built —
the sequencer, rawmidi, UCM, topology and Python bindings are disabled,
since the player uses none of them.

The image points alsa-lib's config directory at `/usr/share/alsa`, which is
where the Pi keeps `alsa.conf`, not at the build prefix inside the image.
Without that a static binary cannot find its configuration and opening the
default PCM fails at runtime, which looks like a broken audio device rather
than a build problem.

**CPU tuning:** `.cargo/config.toml` sets `target-cpu` per target —
`cortex-a7` for armv7 (Pi 2) and `cortex-a76` for aarch64 (Pi 5). That ties
each binary to that CPU class. If you want one aarch64 build that also runs
on a Pi 4 or Zero 2 W, remove the aarch64 section. Native builds ignore
these settings, since they're keyed to the cross target names.

## First run

```
./plexamp-rs
```

It will ask for three things:

1. **A claim code.** Open <https://plex.tv/claim>, sign in, copy the code
   (it starts with `claim-`). Codes expire after a few minutes.
2. **A player name** — what shows up in the Plex app.
3. **Your Plex server URL**, e.g. `http://192.168.1.50:32400`.

These are saved to `~/.config/rustamp/config.json`
(`%USERPROFILE%\.config\rustamp\config.json` on Windows), along with the
auth token, and it won't ask again. **That file contains a credential for
your Plex account — don't commit or share it.** Delete it to start over.

Then open `http://<host>:32500` in a browser, or pick the player in the
Plex/Plexamp app on your phone.

## Audio output

On Linux, playback goes to whatever ALSA's `default` device is. If you have
more than one output (HDMI, headphone jack, a DAC or HAT), set it
explicitly. Find the card number:

```
aplay -l
```

Then write `/etc/asound.conf`, substituting your card number:

```
pcm.!default {
  type plughw
  card 0
  device 0
}
ctl.!default {
  type hw
  card 0
}
```

Card numbers are not stable — they change when you enable or disable a
device in `/boot/config.txt`, so re-check `aplay -l` after any such change.
Verify with `speaker-test -c 2 -t wav` before blaming the player.

If a card has a hardware mixer, make sure its output is actually unmuted;
`amixer -c <n> contents` shows every control, and some (notably the
WM8731 on AudioInjector boards) ship with the DAC-to-output switch off:

```
amixer -c <n> cset numid=<n> on
```

Windows and macOS use the system default output device; there's nothing to
configure.

One Windows gotcha: over Remote Desktop, Windows replaces the default
output with a virtual device that redirects audio to whichever machine you
connected *from*. That device also advertises a narrow set of formats, so
playback may fail with "the requested stream configuration is not supported
by the device". Test at the console, or disconnect RDP so audio reverts to
real hardware.

## Running as a service (Linux)

`/etc/systemd/system/rustamp.service`:

```ini
[Unit]
Description=RustAmp
After=network-online.target sound.target

[Service]
ExecStart=/home/YOUR_USER/plexamp-rs
User=YOUR_USER
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```
sudo systemctl enable --now rustamp
```

Run it once in a terminal first to complete the claim flow — the service
has no way to prompt you. The user it runs as must be in the `audio` group
and must own the config file.

## Notes on the implementation

- **Audio output** is a small trait with two backends (`src/audio.rs`).
  Linux calls the ALSA C API directly; everything else uses `cpal`. This
  isn't arbitrary: on 32-bit ARM, cpal's stream setup segfaults inside
  `alsa::pcm::Status::get_htstamp`, which it calls unconditionally.
  Talking to ALSA directly avoids that path. Decoding uses `rodio`'s
  Symphonia decoders with its playback backend disabled.
- **Output format is whatever the device accepts.** WASAPI in shared mode
  generally only takes the device's own mix format, so the player opens
  the device, asks what it got, and resamples the decoded audio to match
  when it differs. ALSA's `plug` layer does this conversion itself, so
  Linux always gets the format it asks for.
- **Playback always runs from a server-side Plex play queue**, even when
  started from the web UI. Plex controllers stop a player whose timeline
  reports no `playQueueID` when they try to inspect its queue.
- **Prefetch stays exactly one track ahead**, so at most two tracks are
  held in memory. Track downloads retry once with freshly fetched
  metadata, because Plex part keys embed a version that goes stale when
  the server re-analyzes a file.
- **The ALSA buffer is capped at ~250 ms** and pending audio is discarded
  on pause, seek, and skip. Without that, controls lag by several seconds
  while the already-queued audio plays out.

## License

MIT
