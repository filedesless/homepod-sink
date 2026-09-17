# homepod-sink

Turns a HomePod (or other AirPlay 2 speaker) into a PipeWire audio output on
Linux. Captures system audio via a virtual PipeWire sink and streams it over
AirPlay 2 in real time.

Built on a local fork of [airplay2-rs](https://github.com/filedesless/airplay2-rs).

## How it works

Two binaries, piped together, running as separate processes on purpose (see
below):

```
capture  --(raw PCM over stdout)-->  homepod-sink  --(AirPlay 2 / RTP)-->  HomePod
```

- **`capture`** creates a virtual PipeWire sink (shows up in your system's
  audio output picker) and writes the raw interleaved 16-bit PCM it receives
  to stdout.
- **`homepod-sink`** reads that PCM from stdin, resamples/encodes it to ALAC,
  and streams it to an AirPlay 2 device over the network.

They're split into separate processes because PipeWire grants its own audio
thread real-time scheduling via rtkit — sharing a process with the AirPlay
sender's own real-time thread caused unpredictable preemption between the two.

A third binary, **`play`**, is a standalone diagnostic tool: it plays a local
audio file directly to a hardcoded HomePod IP, bypassing PipeWire and the
capture pipeline entirely. Useful for checking the AirPlay connection itself
is healthy, independent of everything else.

## Installation

Requires:
- Rust (stable toolchain)
- PipeWire (with a running user session — this is what most modern Linux
  desktops use by default)
- A checkout of [airplay2-rs](https://github.com/filedesless/airplay2-rs) as
  a sibling directory (`../airplay2-rs` relative to this repo, or adjust the
  `path = "..."` entries in `Cargo.toml`) — the AirPlay client crates are
  consumed as local path dependencies, not from crates.io.

```sh
git clone https://github.com/filedesless/airplay2-rs ../airplay2-rs
cargo build --release
```

This produces `target/release/{capture,homepod-sink,play}`.

## Usage

### Quick start (auto-discovery)

If there's exactly one AirPlay 2 device on your network, you don't need to
know its IP:

```sh
./target/release/capture &
./target/release/homepod-sink
```

`homepod-sink` will discover devices on the network for a few seconds and
connect automatically if exactly one is found. If it finds none, or more than
one, it prints what it found (name, model, IP) and exits — pass `--ip` (or
`--name`) to pick one explicitly:

```sh
$ ./target/release/homepod-sink
discovering AirPlay devices...
Error: found 2 devices, pass --ip (or a more specific --name) to select one:
  Living Room  (AudioAccessory1,1)  192.168.0.13
  Bedroom      (AudioAccessory1,1)  192.168.0.21

$ ./target/release/homepod-sink --ip 192.168.0.13
# or:
$ ./target/release/homepod-sink --name "living"
```

Once running, select the virtual sink (`HomePod` by default) as your system's
audio output.

### CLI reference

**`capture`**

| Flag | Default | Description |
|---|---|---|
| `--sink-name` | `HomePod` | Name of the virtual PipeWire sink, as shown in the output picker. |
| `--sample-rate` | `48000` | Sample rate for the virtual sink. Should match your PipeWire graph's clock rate (`pw-metadata -n settings \| grep clock.rate`) — a mismatch makes PipeWire insert its own rate converter, which has been observed to silently produce zero-valued (silent) samples on this setup. |

**`homepod-sink`**

| Flag | Default | Description |
|---|---|---|
| `--ip` | *(none — triggers discovery)* | IP address of the AirPlay device to connect to. |
| `--name` | *(none)* | Substring to match against discovered device names, used only when `--ip` is omitted and discovery finds more than one device. |
| `--port` | `7000` | AirPlay control port. |
| `--sample-rate` | `48000` | Sample rate of the incoming PCM from stdin — must match `capture`'s `--sample-rate`. Internally resampled to whatever the AirPlay stream needs (44.1kHz). |

**`play`** (diagnostic only)

```sh
./target/release/play <path-to-audio-file> [volume 0.0-1.0, default 1.0]
```

Sends to a hardcoded HomePod IP/port at the top of `src/bin/play.rs` — edit
those constants for your device before building.

## Running as a systemd service

The `systemd/` directory has everything needed to run this as a persistent
user service that starts with your session:

```sh
mkdir -p ~/.config/systemd/user ~/.config/homepod-sink
cp systemd/homepod-sink.service ~/.config/systemd/user/
cp systemd/homepod-sink.env.example ~/.config/homepod-sink/homepod-sink.env
# edit ~/.config/homepod-sink/homepod-sink.env: set HOMEPOD_IP (or HOMEPOD_NAME)
systemctl --user daemon-reload
systemctl --user enable --now homepod-sink
```

Environment variables read by `systemd/run.sh` (see
`systemd/homepod-sink.env.example`):

| Variable | Required | Default | Maps to |
|---|---|---|---|
| `HOMEPOD_IP` | one of `HOMEPOD_IP`/`HOMEPOD_NAME` | — | `homepod-sink --ip` |
| `HOMEPOD_NAME` | one of `HOMEPOD_IP`/`HOMEPOD_NAME` | — | `homepod-sink --name` |
| `HOMEPOD_SINK_NAME` | no | `HomePod` | `capture --sink-name` |
| `HOMEPOD_PORT` | no | `7000` | `homepod-sink --port` |
| `HOMEPOD_SAMPLE_RATE` | no | `48000` | both binaries' `--sample-rate` |

If neither `HOMEPOD_IP` nor `HOMEPOD_NAME` is set, `run.sh` leaves `--ip`
unset and relies on auto-discovery — only reliable if exactly one AirPlay
device is ever present on the network, since the service can't interactively
resolve ambiguity.

Check logs with:

```sh
journalctl --user -u homepod-sink -f
```

## Tuning

- **Real-time scheduling**: without elevated privileges, `homepod-sink` logs
  `Failed to set RT priority (need CAP_SYS_NICE or root)` and falls back to
  normal scheduling — it still works, but is more exposed to scheduling
  jitter under system load. To grant it:

  ```sh
  sudo setcap cap_sys_nice+ep target/release/homepod-sink
  ```

- **Sample rate**: `capture`'s `--sample-rate` should match your PipeWire
  graph's clock rate to avoid PipeWire's own (currently broken in this setup)
  rate conversion. Check with:

  ```sh
  pw-metadata -n settings | grep clock.rate
  ```

  `homepod-sink` resamples from that rate down to AirPlay's 44.1kHz
  internally regardless.
