use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use homepod_sink::airplay::audio::{AlacEncoder, LiveAudioDecoder, LiveFrameSender};
use homepod_sink::airplay::client::AirPlayClient;
use homepod_sink::airplay::core::device::{Device, DeviceId};
use homepod_sink::airplay::core::{AudioCodec, StreamConfig};
use homepod_sink::airplay::discovery::{Discovery, ServiceBrowser};
use homepod_sink::airplay::discovery::BrowseEvent;
use homepod_sink::sink_manager::{Arbiter, SinkEvent, SinkManager};

/// A PipeWire virtual sink per discovered AirPlay 2 device. Whichever sink
/// is receiving audio - as the default output, or because a single app was
/// routed to it - gets a live AirPlay connection. Device selection happens
/// entirely through ordinary PipeWire routing (an output picker,
/// pavucontrol, ...) - there is no --ip/--name here.
#[derive(Parser, Debug)]
struct Args {
    /// Sample rate for every virtual sink. Should match your PipeWire
    /// graph's clock rate (pw-metadata -n settings -> clock.rate) to avoid
    /// PipeWire inserting its own rate converter. Resampled to 44.1kHz for
    /// AirPlay internally regardless.
    #[arg(long, default_value_t = 48000)]
    sample_rate: u32,
}

/// AirPlay control port. Never varies in practice - not worth a flag.
const AIRPLAY_PORT: u16 = 7000;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run(args))?;
    // Keep the tokio runtime (and thus every background task, including
    // the PipeWire thread's command channel) alive for the process's
    // lifetime - this is a long-running daemon with no shutdown path that
    // needs a clean runtime teardown.
    std::mem::forget(runtime);
    Ok(())
}

/// Backoff bounds for reconnecting to a device that stopped responding.
const MIN_RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

type SinkEvents = tokio::sync::mpsc::UnboundedReceiver<SinkEvent>;

/// Everything the daemon reacts to, besides the AirPlay connection itself.
struct State<B> {
    sink_manager: SinkManager,
    sink_events: SinkEvents,
    browse_stream: B,
    /// Mirrors the PW thread's own registry, kept here so picking a target
    /// (a plain DeviceId -> Device lookup) doesn't need to ask the PW
    /// thread and wait on a reply.
    known_devices: HashMap<DeviceId, Device>,
    arbiter: Arbiter,
    /// Each device's PipeWire sink volume (0.0-1.0 slider position), which
    /// is applied as that device's AirPlay volume.
    volumes: HashMap<DeviceId, f32>,
}

impl<B: tokio_stream::Stream<Item = BrowseEvent> + Unpin> State<B> {
    /// Wait for the next discovery or sink-activity event and apply it.
    async fn next_event(&mut self) -> Result<()> {
        tokio::select! {
            maybe_event = tokio_stream::StreamExt::next(&mut self.browse_stream) => {
                let Some(event) = maybe_event else {
                    anyhow::bail!("discovery stream ended unexpectedly");
                };
                match &event {
                    BrowseEvent::Added(d) | BrowseEvent::Updated(d) => {
                        self.known_devices.insert(d.id.clone(), d.clone());
                    }
                    BrowseEvent::Removed(id) => {
                        self.known_devices.remove(id);
                        self.arbiter.device_removed(id);
                        self.volumes.remove(id);
                    }
                }
                self.sink_manager.handle_browse_event(event);
            }

            maybe_sink_event = self.sink_events.recv() => {
                let Some(sink_event) = maybe_sink_event else {
                    anyhow::bail!("PipeWire sink manager event channel closed unexpectedly");
                };
                tracing::debug!("sink event: {:?}", sink_event);
                if let SinkEvent::VolumeChanged { id, volume } = &sink_event {
                    self.volumes.insert(id.clone(), *volume);
                }
                self.arbiter.handle_sink_event(&sink_event);
            }
        }
        Ok(())
    }

    fn target(&self, current: Option<&DeviceId>) -> Option<DeviceId> {
        self.arbiter.target(current, &self.known_devices)
    }
}

async fn run(args: Args) -> Result<()> {
    let (sink_manager, sink_events) = SinkManager::spawn(args.sample_rate)
        .context("failed to start PipeWire sink manager")?;

    let browser = ServiceBrowser::new().context("failed to start mDNS browser")?;
    let browse_stream = browser.browse().await.context("failed to start discovery")?;

    let mut state = State {
        sink_manager,
        sink_events,
        browse_stream,
        known_devices: HashMap::new(),
        arbiter: Arbiter::default(),
        volumes: HashMap::new(),
    };
    let mut retry_delay = MIN_RETRY_DELAY;

    tracing::info!("watching for AirPlay devices and audio routed to their sinks...");

    loop {
        let Some(id) = state.target(None) else {
            state.next_event().await?;
            continue;
        };
        let device = state.known_devices[&id].clone();
        tracing::info!(
            "audio playing to {} - connecting ({})...",
            device.name,
            device.addresses.first().map(|a| a.to_string()).unwrap_or_default()
        );

        match stream_to_target(&device, args.sample_rate, &mut state, &mut retry_delay).await? {
            StreamOutcome::Retarget => {}
            StreamOutcome::ConnectionLost(err) => {
                tracing::warn!("connection to {} lost: {:#}", device.name, err);

                // A HomePod that still answers on its control port but
                // dropped our session was taken over by another sender
                // (e.g. an iPhone). Reconnecting would steal it straight
                // back, so leave it alone until playback here restarts.
                if is_reachable(&device).await {
                    tracing::info!(
                        "{} is still reachable, so another AirPlay sender took it over - \
                         not reconnecting until play is pressed again here",
                        device.name
                    );
                    state.arbiter.yield_device(&device.id);
                    retry_delay = MIN_RETRY_DELAY;
                    continue;
                }

                // Otherwise it's gone (power, Wi-Fi, new DHCP address):
                // retry with backoff, while still handling events so a
                // stop or switch takes effect immediately.
                tracing::info!("{} is unreachable, retrying in {:?}", device.name, retry_delay);
                let deadline = tokio::time::Instant::now() + retry_delay;
                retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
                while state.target(Some(&device.id)) == Some(device.id.clone()) {
                    match tokio::time::timeout_at(deadline, state.next_event()).await {
                        Ok(result) => result?,
                        Err(_elapsed) => break,
                    }
                }
            }
        }
    }
}

enum StreamOutcome {
    /// What should be playing changed (audio stopped, or the device was
    /// yielded/removed) - re-evaluate the target.
    Retarget,
    /// The connection itself failed/died.
    ConnectionLost(anyhow::Error),
}

/// Whether `device` accepts a TCP connection on its AirPlay control port.
/// A bare connect, no RTSP - it doesn't disturb whoever is playing.
async fn is_reachable(device: &Device) -> bool {
    for addr in &device.addresses {
        let connect = tokio::net::TcpStream::connect((*addr, AIRPLAY_PORT));
        if let Ok(Ok(_)) = tokio::time::timeout(Duration::from_secs(3), connect).await {
            return true;
        }
    }
    false
}

/// Connect to `device` and stream to it until either the connection dies
/// (feedback failures) or it stops being the target. Keeps discovery and
/// sink-activity event handling running throughout. Errors only if one of
/// those event sources itself ends.
async fn stream_to_target<B: tokio_stream::Stream<Item = BrowseEvent> + Unpin>(
    device: &Device,
    sample_rate: u32,
    state: &mut State<B>,
    retry_delay: &mut Duration,
) -> Result<StreamOutcome> {
    let mut config = StreamConfig::realtime_ntp();
    if config.audio_format.codec == AudioCodec::Alac {
        match AlacEncoder::new(config.audio_format.clone()) {
            Ok(temp_encoder) => config.asc = Some(temp_encoder.magic_cookie()),
            Err(e) => return Ok(StreamOutcome::ConnectionLost(e.into())),
        }
    }

    let mut client = match AirPlayClient::with_config(config, None) {
        Ok(c) => c,
        Err(e) => return Ok(StreamOutcome::ConnectionLost(e.into())),
    };

    let mut device = device.clone();
    device.port = AIRPLAY_PORT;

    if let Err(e) = client.connect(&device).await {
        return Ok(StreamOutcome::ConnectionLost(e.into()));
    }

    // Starting to stream always sends a volume, and the HomePod has one
    // volume shared by every sender. Prefer the level it's already at (e.g.
    // as last set from an iPhone) and move the sink's slider to match;
    // fall back to the slider's level if it won't say. Never the client's
    // default of 100%.
    let mut sent_volume = match client.get_volume().await {
        Ok(volume) => {
            tracing::info!("{} is at {:.0}% volume, matching the sink to it", device.name, volume * 100.0);
            state.volumes.insert(device.id.clone(), volume);
            state.sink_manager.set_sink_volume(device.id.clone(), volume);
            Some(volume)
        }
        Err(e) => {
            tracing::info!("couldn't read {}'s volume ({}), using the sink's", device.name, e);
            state.volumes.get(&device.id).copied()
        }
    };
    if let Some(volume) = sent_volume {
        if let Err(e) = client.set_initial_volume(volume) {
            return Ok(StreamOutcome::ConnectionLost(e.into()));
        }
    }

    let (sender, decoder) = LiveAudioDecoder::create_pair(sample_rate, 2, 64);
    install_active_target(&state.sink_manager, &device.id, sender);

    if let Err(e) = client.start_live_streaming_with_decoder(decoder).await {
        state.sink_manager.set_active_target(None);
        return Ok(StreamOutcome::ConnectionLost(e.into()));
    }

    tracing::info!("streaming to {} at {}Hz stereo", device.name, sample_rate);
    *retry_delay = MIN_RETRY_DELAY;

    const MAX_CONSECUTIVE_FAILURES: u32 = 3;
    let mut consecutive_failures = 0u32;
    let mut feedback_interval = tokio::time::interval(Duration::from_secs(2));
    feedback_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let result = loop {
        tokio::select! {
            _ = feedback_interval.tick() => {
                match client.send_feedback().await {
                    Ok(()) => consecutive_failures = 0,
                    Err(e) => {
                        consecutive_failures += 1;
                        tracing::warn!(
                            "feedback failed ({}/{}): {}",
                            consecutive_failures,
                            MAX_CONSECUTIVE_FAILURES,
                            e
                        );
                        if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                            break Ok(StreamOutcome::ConnectionLost(anyhow::anyhow!(
                                "feedback failed {} times in a row",
                                consecutive_failures
                            )));
                        }
                    }
                }
            }

            event_result = state.next_event() => {
                if let Err(e) = event_result {
                    break Err(e);
                }
                if state.target(Some(&device.id)) != Some(device.id.clone()) {
                    tracing::info!("audio to {} stopped - disconnecting", device.name);
                    break Ok(StreamOutcome::Retarget);
                }
                let volume = state.volumes.get(&device.id).copied();
                // Tolerance: a slider move we made ourselves (above) echoes
                // back through PipeWire with float rounding.
                let changed = |v: &f32| sent_volume.is_none_or(|sent| (v - sent).abs() > 0.005);
                if let Some(volume) = volume.filter(changed) {
                    sent_volume = Some(volume);
                    match client.set_volume(volume).await {
                        Ok(()) => tracing::info!("set {} volume to {:.0}%", device.name, volume * 100.0),
                        Err(e) => tracing::warn!("failed to set volume: {}", e),
                    }
                }
            }
        }
    };

    // Clear the PCM target immediately so the (now-former) active sink
    // goes quiet right away, then gracefully tear down the RTSP session
    // (sends TEARDOWN) rather than just dropping the socket - which also
    // frees the speaker for other senders.
    state.sink_manager.set_active_target(None);
    let _ = client.disconnect().await;

    result
}

fn install_active_target(sink_manager: &SinkManager, id: &DeviceId, sender: LiveFrameSender) {
    sink_manager.set_active_target(Some((id.clone(), sender)));
}
