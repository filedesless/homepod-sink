use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use airplay_audio::{AlacEncoder, LiveAudioDecoder, LivePcmFrame};
use airplay_client::AirPlayClient;
use airplay_core::device::DeviceId;
use airplay_core::{AudioCodec, StreamConfig};

/// Read raw interleaved i16 PCM from stdin (as produced by `capture`) and
/// stream it to a HomePod / AirPlay speaker.
#[derive(Parser, Debug)]
struct Args {
    /// IP address of the AirPlay device (see `debug_devices` in airplay2-rs to find it).
    #[arg(long)]
    ip: String,

    /// AirPlay control port.
    #[arg(long, default_value_t = 7000)]
    port: u16,

    /// Sample rate of the incoming PCM (must match `capture`'s --sample-rate).
    /// LiveAudioDecoder resamples internally to whatever the AirPlay
    /// StreamConfig's audio_format requires (44.1kHz).
    #[arg(long, default_value_t = 48000)]
    sample_rate: u32,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Create the frame channel and start reading stdin on a dedicated thread
    // *before* connecting to AirPlay, not after. AudioStreamer::start_live()
    // blocks for up to 5 seconds trying to fill its buffer to 50% before it
    // will start streaming (crates/airplay-audio/src/streamer.rs) - if stdin
    // reading only started after connect_airplay() returned, that wait
    // always hit its timeout against an empty channel ("buffer timeout at
    // 0.0%, starting anyway"), so the stream began in Streaming state with
    // nothing buffered. The HomePod would ACK the whole RTSP handshake and
    // receive correctly-formed encrypted RTP packets once real audio
    // eventually arrived, but never produced audible output - likely because
    // it had already committed to rendering (and discarding) an empty
    // stream. Running capture concurrently with connect lets real PCM start
    // filling the buffer during that wait instead of after it.
    let (sender, decoder) = LiveAudioDecoder::create_pair(args.sample_rate, 2, 64);

    let sample_rate = args.sample_rate;
    let stdin_thread = std::thread::Builder::new()
        .name("stdin-pcm-reader".into())
        .spawn(move || read_stdin_pcm(sender, sample_rate))
        .context("failed to spawn stdin reader thread")?;

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(connect_airplay(&args, decoder))?;
    // Keep the tokio runtime (and thus the connection's background tasks) alive
    // for the lifetime of the process.
    std::mem::forget(runtime);

    // stdin_thread runs for the lifetime of the process (loops until stdin
    // closes or the channel disconnects); propagate its result/panic.
    stdin_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stdin reader thread panicked"))?
}

async fn connect_airplay(args: &Args, decoder: LiveAudioDecoder) -> Result<()> {
    // StreamConfig::realtime_ntp() (= Default::default()) leaves `asc` as
    // None. The RTSP SETUP flow only auto-generates an ASC (Audio Specific
    // Config) for AAC codecs — for ALAC, a None `asc` means the HomePod
    // never receives the magic cookie describing the stream's sample
    // rate/bit depth/frame size, and it silently accepts the connection but
    // never produces audible output. Build the magic cookie explicitly,
    // matching airplay2-rs's own play_audio example (the config confirmed
    // to actually play audio on this HomePod). Note this config only sets
    // stream parameters — AirPlayClient::connect() always uses the AirPlay 2
    // (HomeKit pairing) protocol regardless of this config.
    let mut config = StreamConfig::realtime_ntp();
    if config.audio_format.codec == AudioCodec::Alac {
        let temp_encoder = AlacEncoder::new(config.audio_format.clone())?;
        config.asc = Some(temp_encoder.magic_cookie());
    }

    let mut client = AirPlayClient::with_config(config, None)
        .context("failed to create AirPlay client")?;

    let ip: std::net::IpAddr = args.ip.parse().context("invalid --ip address")?;
    let octets = match ip {
        std::net::IpAddr::V4(v4) => v4.octets(),
        _ => anyhow::bail!("only IPv4 addresses are supported for now"),
    };
    let device_id_str = format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:00:00",
        octets[0], octets[1], octets[2], octets[3]
    );
    let derived_id = DeviceId::from_mac_string(&device_id_str)
        .map_err(|e| anyhow::anyhow!("failed to build device id: {e:?}"))?;

    tracing::info!("discovering AirPlay devices...");
    let devices = client.discover(Duration::from_secs(5)).await?;
    let device = devices
        .into_iter()
        .find(|d| d.id == derived_id || d.addresses.iter().any(|a| *a == ip))
        .with_context(|| format!("device at {} not found during discovery", args.ip))?;

    tracing::info!("connecting to {} ({})...", device.name, args.ip);
    client.connect(&device).await?;

    tracing::info!("starting live stream at {}Hz stereo", args.sample_rate);
    client.start_live_streaming_with_decoder(decoder).await?;

    // The RAOP/AirPlay SET_PARAMETER volume command is never sent unless we
    // call this explicitly — without it, the HomePod may render at whatever
    // volume it defaults/remembers to (observed as no audible output despite
    // correct, non-silent audio data reaching it).
    match client.set_volume(1.0).await {
        Ok(()) => tracing::info!("Set initial volume to 1.0"),
        Err(e) => tracing::warn!("Failed to set initial volume: {}", e),
    }

    // Leak the client so the connection stays alive for the process lifetime.
    // This is a small standalone daemon with no shutdown path that needs
    // to drop it cleanly today.
    let client: &'static mut AirPlayClient = Box::leak(Box::new(client));

    // play_audio.rs (airplay2-rs's own reference example, confirmed to
    // produce audible output) sends periodic feedback/keepalive via
    // GET_PARAMETER/OPTIONS every ~2 seconds; homepod-sink never did. Some
    // receivers may treat a connection with no control-channel activity as
    // idle/inactive for rendering purposes even while still accepting RTP
    // packets on the data socket, which would explain correctly-encoded,
    // correctly-encrypted audio arriving with no audible output.
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if let Err(e) = client.send_feedback().await {
                tracing::warn!("Feedback failed: {}", e);
            }
        }
    });

    Ok(())
}

fn read_stdin_pcm(sender: airplay_audio::LiveFrameSender, sample_rate: u32) -> Result<()> {
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    // Read in smaller chunks than capture's 1024-sample PipeWire quantum
    // (256 samples/channel * 2 channels * 2 bytes/sample) so this loop wakes
    // up more often with less data per wake, reducing how long it can block
    // the RTP sender's downstream channel at a time.
    let mut buf = vec![0u8; 256 * 2 * 2];

    loop {
        lock.read_exact(&mut buf)
            .context("failed to read PCM from stdin (capture process exited?)")?;

        let mut samples = vec![0i16; buf.len() / 2];
        for (i, chunk) in buf.chunks_exact(2).enumerate() {
            samples[i] = i16::from_le_bytes([chunk[0], chunk[1]]);
        }

        sender.send(LivePcmFrame {
            samples,
            channels: 2,
            sample_rate,
        });
    }
}
