# homepod-sink

Turns a HomePod (or other AirPlay 2 speaker) into a PipeWire audio output on
Linux. Creates a virtual PipeWire sink for every AirPlay 2 device on your
network and streams system audio to whichever one you pick as your output,
in real time.

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
- Watches PipeWire's default-output setting. Whichever sink you select is
  the one it actually opens an AirPlay connection to and streams to —
  switching outputs in the picker tears down the old connection and opens a
  new one, the same as switching between two USB speakers.
- Every other sink stays present but idle: audio routed to it is silently
  discarded rather than sent anywhere, so picking a different output never
  errors or blocks.

There's nothing to configure to select a device — picking the output *is*
picking the device.

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

See
[Running as a systemd service](#running-as-a-systemd-service) below to
enable it. Uninstall with `sudo pacman -R homepod-sink-git`.

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
Open your system's audio output picker and select the one you want — that's
it. `homepod-sink` connects to it, and switching outputs later reconnects to
whichever one you pick next.

```
$ ./target/release/homepod-sink
watching for AirPlay devices and PipeWire's default output...
default output switched to Living Room - connecting
connecting to Living Room (192.168.0.13)...
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

`homepod-sink` never gives up and exits on a lost connection. If the active
device stops responding (feedback keepalives fail three times in a row —
e.g. it lost power, dropped off Wi-Fi, or got a new IP from DHCP), it
disconnects and retries discovery/connection to the same device with
exponential backoff (2s up to 30s) until it comes back or you pick a
different output. Every sink's PipeWire node stays alive throughout — audio
routed to the disconnected one is simply dropped, not buffered, so nothing
needs to be restarted.

## Running as a systemd service

**If installed via the Arch package**, the unit is already at
`/usr/lib/systemd/user/homepod-sink.service`:

```sh
systemctl --user daemon-reload
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
