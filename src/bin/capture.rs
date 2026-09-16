use std::io::Write;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;

/// Capture desktop audio via a virtual PipeWire sink and write raw
/// interleaved i16 PCM to stdout. Meant to be piped into `sink`.
///
/// This runs in its own process, separate from the AirPlay sender, because
/// PipeWire's own audio thread gets real-time scheduling priority via
/// rtkit — sharing a process with the sender's own (non-rtkit) real-time
/// thread causes it to be preempted unpredictably.
#[derive(Parser, Debug)]
struct Args {
    /// Name shown in the audio output picker.
    #[arg(long, default_value = "HomePod")]
    sink_name: String,

    /// Sample rate for the virtual sink. Defaults to 48000 to match this
    /// system's PipeWire graph clock rate (pw-metadata -n settings ->
    /// clock.rate) and avoid PipeWire having to insert a rate converter,
    /// which was silently producing zero-valued samples in this pipeline.
    /// homepod-sink resamples to 44.1kHz for AirPlay on its end.
    #[arg(long, default_value_t = 48000)]
    sample_rate: u32,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    let args = Args::parse();

    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::NODE_NAME => args.sink_name.clone(),
        *pw::keys::NODE_DESCRIPTION => format!("{} (AirPlay)", args.sink_name),
        *pw::keys::NODE_VIRTUAL => "true",
        *pw::keys::NODE_ALWAYS_PROCESS => "true",
    };

    let stream = pw::stream::StreamRc::new(core, &args.sink_name, props.to_owned())?;

    let stdout = std::io::stdout();

    let _listener = stream
        .add_local_listener_with_user_data(stdout)
        .state_changed(|_stream, _data, old, new| {
            tracing::info!("PipeWire stream state: {:?} -> {:?}", old, new);
        })
        .process(move |stream, stdout| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.get_mut(0) else {
                return;
            };
            let valid_len = data.chunk().size() as usize;
            let Some(slice) = data.data() else {
                return;
            };
            let valid = &slice[..valid_len.min(slice.len())];
            let mut lock = stdout.lock();
            let _ = lock.write_all(valid);
            let _ = lock.flush();
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(args.sample_rate);
    audio_info.set_channels(2);

    let obj = pw::spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS
            | pw::stream::StreamFlags::DRIVER,
        &mut params,
    )?;

    stream.set_active(true)?;

    // As a DRIVER stream, nothing else will pull cycles through our node —
    // we have to trigger them ourselves on a steady clock, matching the
    // graph's actual clock quantum (default 1024 samples @ the declared rate).
    let driver_stream = stream.clone();
    let period = Duration::from_secs_f64(1024.0 / args.sample_rate as f64);
    let timer = mainloop.loop_().add_timer(move |_expirations| {
        if let Err(e) = driver_stream.trigger_process() {
            tracing::warn!("trigger_process failed: {}", e);
        }
    });
    timer
        .update_timer(Some(period), Some(period))
        .into_result()?;

    tracing::info!(
        "virtual sink \"{}\" is live — select it as your audio output",
        args.sink_name
    );

    mainloop.run();

    Ok(())
}
