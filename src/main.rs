use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use homepod_sink::airplay::audio::{AlacEncoder, LiveAudioDecoder, LiveFrameSender};
use homepod_sink::airplay::client::AirPlayClient;
use homepod_sink::airplay::core::device::{Device, DeviceId};
use homepod_sink::airplay::core::{AudioCodec, StreamConfig};
use homepod_sink::airplay::discovery::{Discovery, ServiceBrowser};
use homepod_sink::sink_manager::{SinkManager, StreamCommand, commands::derive_stream_command};

/// A PipeWire virtual sink per discovered AirPlay 2 device, with the one
/// currently selected as PipeWire's default output actually streaming to
/// it. Device selection happens entirely through an ordinary audio output
/// picker (e.g. Noctalia's) - there is no --ip/--name here.
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

async fn run(args: Args) -> Result<()> {
    let (sink_manager, mut sink_events) = SinkManager::spawn(args.sample_rate)
        .context("failed to start PipeWire sink manager")?;

    let browser = ServiceBrowser::new().context("failed to start mDNS browser")?;
    let mut browse_stream = browser.browse().await.context("failed to start discovery")?;

    // Mirrors the PW thread's own registry, kept here so StreamCommand
    // derivation (a plain DeviceId -> Device lookup) doesn't need to ask
    // the PW thread and wait on a reply.
    let mut known_devices: HashMap<DeviceId, Device> = HashMap::new();
    let mut current_device: Option<Device> = None;

    tracing::info!("watching for AirPlay devices and PipeWire's default output...");

    loop {
        tokio::select! {
            // A discovery event arrived - keep our own Device map current
            // (for StreamCommand derivation) and let the PW thread manage
            // the corresponding sink.
            maybe_event = tokio_stream::StreamExt::next(&mut browse_stream) => {
                let Some(event) = maybe_event else {
                    anyhow::bail!("discovery stream ended unexpectedly");
                };
                use homepod_sink::airplay::discovery::BrowseEvent;
                match &event {
                    BrowseEvent::Added(d) | BrowseEvent::Updated(d) => {
                        known_devices.insert(d.id.clone(), d.clone());
                    }
                    BrowseEvent::Removed(id) => {
                        known_devices.remove(id);
                    }
                }
                sink_manager.handle_browse_event(event);
            }

            // PipeWire's default sink changed - figure out whether that
            // means a different AirPlay connection should be active.
            maybe_sink_event = sink_events.recv() => {
                let Some(sink_event) = maybe_sink_event else {
                    anyhow::bail!("PipeWire sink manager event channel closed unexpectedly");
                };
                let currently_active = current_device.as_ref().map(|d| &d.id);
                tracing::debug!(
                    "sink event: {:?}, currently_active={:?}, known_devices={:?}",
                    sink_event,
                    currently_active,
                    known_devices.keys().collect::<Vec<_>>()
                );
                let command = derive_stream_command(currently_active, &sink_event, &known_devices);
                match command {
                    StreamCommand::NoOp => {}
                    StreamCommand::Disconnect => {
                        tracing::info!("no AirPlay device selected as default output, staying idle");
                        current_device = None;
                        sink_manager.set_active_target(None);
                    }
                    StreamCommand::ConnectTo(device) => {
                        tracing::info!(
                            "default output switched to {} - connecting",
                            device.name
                        );
                        current_device = Some(*device);
                    }
                }
            }
        }

        // Whenever a target is set (initial pick or a switch), (re)connect
        // to it. This runs the whole discover-less connect+stream+feedback
        // sequence and only returns once the connection is judged dead or
        // interrupted by a new target arriving - see stream_to_target.
        if let Some(device) = current_device.clone() {
            tracing::info!("connecting to {} ({})...", device.name, device.addresses.first().map(|a| a.to_string()).unwrap_or_default());

            let outcome = stream_to_target(
                &device,
                args.sample_rate,
                &sink_manager,
                &mut sink_events,
                &mut browse_stream,
                &mut known_devices,
            )
            .await;

            match outcome {
                StreamOutcome::SwitchedTo(new_device) => {
                    current_device = Some(new_device);
                    // Loop back around immediately to connect to it.
                    continue;
                }
                StreamOutcome::Disconnected => {
                    current_device = None;
                }
                StreamOutcome::ConnectionLost(err) => {
                    tracing::warn!("connection to {} lost: {:#}", device.name, err);
                    // Stay assigned to this device; the top-level loop's
                    // next iteration will retry it after a short backoff,
                    // unless a StreamCommand arrives first and preempts it.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}

enum StreamOutcome {
    /// A new StreamCommand::ConnectTo arrived while streaming - switch to it.
    SwitchedTo(Device),
    /// A StreamCommand::Disconnect arrived - stop, no target.
    Disconnected,
    /// The connection itself failed/died - same target should be retried.
    ConnectionLost(anyhow::Error),
}

/// Connect to `device` and stream to it until either the connection dies
/// (feedback failures) or a new StreamCommand arrives (default sink
/// changed again while this one was active). Keeps discovery and
/// sink-manager event handling running throughout via nested `select!`.
async fn stream_to_target(
    device: &Device,
    sample_rate: u32,
    sink_manager: &SinkManager,
    sink_events: &mut tokio::sync::mpsc::UnboundedReceiver<homepod_sink::sink_manager::SinkEvent>,
    browse_stream: &mut (impl tokio_stream::Stream<Item = homepod_sink::airplay::discovery::BrowseEvent> + Unpin),
    known_devices: &mut HashMap<DeviceId, Device>,
) -> StreamOutcome {
    let mut config = StreamConfig::realtime_ntp();
    if config.audio_format.codec == AudioCodec::Alac {
        match AlacEncoder::new(config.audio_format.clone()) {
            Ok(temp_encoder) => config.asc = Some(temp_encoder.magic_cookie()),
            Err(e) => return StreamOutcome::ConnectionLost(e.into()),
        }
    }

    let mut client = match AirPlayClient::with_config(config, None) {
        Ok(c) => c,
        Err(e) => return StreamOutcome::ConnectionLost(e.into()),
    };

    let mut device = device.clone();
    device.port = AIRPLAY_PORT;

    if let Err(e) = client.connect(&device).await {
        return StreamOutcome::ConnectionLost(e.into());
    }

    let (sender, decoder) = LiveAudioDecoder::create_pair(sample_rate, 2, 64);
    install_active_target(sink_manager, &device.id, sender);

    if let Err(e) = client.start_live_streaming_with_decoder(decoder).await {
        sink_manager.set_active_target(None);
        return StreamOutcome::ConnectionLost(e.into());
    }

    tracing::info!("streaming to {} at {}Hz stereo", device.name, sample_rate);

    match client.set_volume(1.0).await {
        Ok(()) => tracing::info!("set initial volume to 1.0"),
        Err(e) => tracing::warn!("failed to set initial volume: {}", e),
    }

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
                            break StreamOutcome::ConnectionLost(anyhow::anyhow!(
                                "feedback failed {} times in a row",
                                consecutive_failures
                            ));
                        }
                    }
                }
            }

            maybe_event = tokio_stream::StreamExt::next(browse_stream) => {
                let Some(event) = maybe_event else {
                    break StreamOutcome::ConnectionLost(anyhow::anyhow!("discovery stream ended unexpectedly"));
                };
                use homepod_sink::airplay::discovery::BrowseEvent;
                match &event {
                    BrowseEvent::Added(d) | BrowseEvent::Updated(d) => {
                        known_devices.insert(d.id.clone(), d.clone());
                    }
                    BrowseEvent::Removed(id) => {
                        known_devices.remove(id);
                    }
                }
                sink_manager.handle_browse_event(event);
            }

            maybe_sink_event = sink_events.recv() => {
                let Some(sink_event) = maybe_sink_event else {
                    break StreamOutcome::ConnectionLost(anyhow::anyhow!("PipeWire sink manager event channel closed unexpectedly"));
                };
                let command = derive_stream_command(Some(&device.id), &sink_event, known_devices);
                match command {
                    StreamCommand::NoOp => {}
                    StreamCommand::Disconnect => break StreamOutcome::Disconnected,
                    StreamCommand::ConnectTo(new_device) => break StreamOutcome::SwitchedTo(*new_device),
                }
            }
        }
    };

    // Clear the PCM target immediately so the (now-former) active sink
    // goes quiet right away, then gracefully tear down the RTSP session
    // (sends TEARDOWN) rather than just dropping the socket.
    sink_manager.set_active_target(None);
    let _ = client.disconnect().await;

    result
}

fn install_active_target(sink_manager: &SinkManager, id: &DeviceId, sender: LiveFrameSender) {
    sink_manager.set_active_target(Some((id.clone(), sender)));
}
