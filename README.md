# homepod-sink

Turns a HomePod (or other AirPlay 2 speaker) into a PipeWire audio output on
Linux. Creates a virtual PipeWire sink for every AirPlay 2 device on your
network and streams whatever audio is routed to one of them, in real time -
your whole system output, or just a single app like Spotify.

The AirPlay 2 protocol implementation (`src/airplay/`) started as a trimmed
vendor of [airplay2-rs](https://github.com/filedesless/airplay2-rs) — cut down
to just the HomeKit-paired, NTP-timed, single-device, live-streaming path this
project actually uses (no RAOP/AirPlay 1, no PTP, no Bluetooth, no TUI) — and
now lives in this repo as ordinary source, not an external dependency.

## How it works

One binary, `homepod-sink`:

- Runs continuous mDNS discovery for AirPlay 2 devices on the network.
- Creates a PipeWire virtual sink for each one it finds, named after the
  device, so they all show up in your system's audio output picker (e.g.
  Noctalia) alongside your usual outputs.
- Watches each sink for audio. As soon as something audible plays into one
  — because it's your default output, or because you moved a single app's
  stream to it (e.g. in pavucontrol) while everything else stays on your
  headphones — it opens an AirPlay connection to that device and streams.
- When you pause, it disconnects 3 seconds later, releasing the speaker so
  your phone or another computer can use it. It tells "paused" from "quiet
  passage" by the app's PipeWire stream state, not by listening for
  silence. An app that keeps its stream open while silent (e.g. a browser
  tab) is released after 10 seconds of silence instead. The first ~2
  seconds of audio after playback (re)starts are lost while it connects.
- One device streams at a time: if a second sink starts playing, it waits
  until the first one goes quiet.
- A sink's volume and mute (in your output picker, pavucontrol, `wpctl`,
  ...) are the HomePod's own volume, not software attenuation: the slider
  maps onto AirPlay's -30 dB to 0 dB range and is sent to the speaker
  whenever you move it. The HomePod has one volume for every sender, so
  when a connection starts the slider jumps to whatever level the speaker
  is already at (e.g. as you last set it from your phone) rather than
  overriding it. If the speaker won't report its volume, the slider's
  level is sent instead. Changes made on the speaker or another device
  while the PC is streaming aren't reflected back.

  App volume sliders (Spotify's own, or a per-app slider in your picker)
  only scale the audio below that level, so they can't make it louder
  than the speaker is set to. To raise the ceiling from the PC, change the
  AirPlay sink's own volume while it's playing: the easiest way is to
  pick it as your main output for a moment and use the normal volume
  control, or `wpctl set-volume <id> 60%` (id from `wpctl status`).

There's nothing to configure to select a device — routing audio to its
sink *is* picking the device.

PipeWire grants its own audio thread real-time scheduling via rtkit; the
AirPlay sender runs on its own dedicated thread with `SCHED_FIFO` real-time
scheduling instead (see [Tuning](#tuning)) rather than sharing rtkit's
elevation, so the two don't contend with each other.

A second binary, **`play`**, is a standalone diagnostic tool: it plays a
local audio file directly to a hardcoded HomePod IP, bypassing PipeWire and
device discovery entirely. Useful for checking the AirPlay connection itself
is healthy, independent of everything else.

## Installation

### Arch Linux

A `PKGBUILD` is included, building straight from this repo (`homepod-sink-git`):

```sh
makepkg -si
```

This builds and installs both binaries to `/usr/lib/homepod-sink/`
(deliberately not on `PATH`), plus a systemd user unit at
`/usr/lib/systemd/user/homepod-sink.service` and an env file template at
`/etc/homepod-sink/homepod-sink.env.example`.

**Switching from a manual/from-source install:** if you already have a unit
at `~/.config/systemd/user/homepod-sink.service` (e.g. from following the
from-source instructions below), it takes precedence over the package's
`/usr/lib/systemd/user/homepod-sink.service` — same filename, and
user-level units always win over system-level ones — so the package's unit
would silently never run. Disable and remove the old one first:

```sh
systemctl --user disable --now homepod-sink
rm ~/.config/systemd/user/homepod-sink.service
systemctl --user daemon-reload
```

Installing enables and starts the service for every logged-in user,
upgrading restarts it wherever it's running, and uninstalling (`sudo pacman
-R homepod-sink-git`) stops and disables it.

### From source

Requires:
- Rust (stable toolchain)
- PipeWire (with a running user session — this is what most modern Linux
  desktops use by default)

Everything else is an ordinary crates.io dependency — no sibling checkout or
path dependency needed.

```sh
cargo build --release
```

This produces `target/release/{homepod-sink,play}`.

## Usage

```sh
./target/release/homepod-sink
```

Every AirPlay 2 device on your network gets a PipeWire sink immediately.
Select one as your output, or route a single app to it — `homepod-sink`
connects when audio starts and disconnects once it stops.

```
$ ./target/release/homepod-sink
watching for AirPlay devices and audio routed to their sinks...
audio playing to Living Room - connecting (192.168.0.13)...
streaming to Living Room at 48000Hz stereo
```

### CLI reference

**`homepod-sink`**

| Flag | Default | Description |
|---|---|---|
| `--sample-rate` | `48000` | Sample rate for every virtual sink. Should match your PipeWire graph's clock rate (`pw-metadata -n settings \| grep clock.rate`) — a mismatch makes PipeWire insert its own rate converter, which has been observed to silently produce zero-valued (silent) samples on this setup. Resampled to 44.1kHz for AirPlay internally regardless. |

**`play`** (diagnostic only)

```sh
./target/release/play <path-to-audio-file> [volume 0.0-1.0, default 1.0]
```

Sends to a hardcoded HomePod IP/port at the top of `src/bin/play.rs` — edit
those constants for your device before building.

### Reconnecting

`homepod-sink` never gives up and exits on a lost connection. When the
active device drops the session (feedback keepalives fail three times in a
row), it checks whether the device still answers on its AirPlay port:

- **Still reachable:** another sender (e.g. your iPhone) took the speaker
  over. `homepod-sink` leaves it alone instead of stealing it back, even
  if the PC's audio carries on playing. To take it back, press pause and
  play again on the PC.
- **Unreachable** (lost power, dropped off Wi-Fi, new DHCP address): it
  retries with exponential backoff (2s up to 30s) for as long as audio
  keeps playing into the sink.

Every sink's PipeWire node stays alive throughout. Audio routed to a
disconnected one is dropped, not buffered, so nothing needs to be
restarted.

## Running as a systemd service

**If installed via the Arch package**, the unit is at
`/usr/lib/systemd/user/homepod-sink.service` and the package enables it for
every user logged in at install time. Anyone else enables it with:

```sh
systemctl --user enable --now homepod-sink
```

**If built from source**, the `systemd/` directory has everything needed to
run this as a persistent user service that starts with your session:

```sh
mkdir -p ~/.config/systemd/user ~/.config/homepod-sink
cp systemd/homepod-sink.service ~/.config/systemd/user/
cp systemd/homepod-sink.env.example ~/.config/homepod-sink/homepod-sink.env
systemctl --user daemon-reload
systemctl --user enable --now homepod-sink
```

The only environment variable `systemd/run.sh` reads (see
`systemd/homepod-sink.env.example`) is `HOMEPOD_SAMPLE_RATE` (default
`48000`), mapped to `homepod-sink --sample-rate`. There's no target device
to configure — pick one from your output picker as usual once the service
is running.

Check logs with:

```sh
journalctl --user -u homepod-sink -f
```

## Tuning

- **Sample rate**: `--sample-rate` should match your PipeWire graph's clock
  rate to avoid PipeWire's own (currently broken in this setup) rate
  conversion. Check with:

  ```sh
  pw-metadata -n settings | grep clock.rate
  ```

  `homepod-sink` resamples from that rate down to AirPlay's 44.1kHz
  internally regardless.
