//! The PipeWire mainloop thread: owns one virtual sink per discovered
//! AirPlay device, reports when each one starts or stops receiving audible
//! audio, and forwards captured PCM for whichever sink is currently active.
//!
//! Everything PipeWire-owned (streams, listeners, timers)
//! lives entirely on this one dedicated OS thread and never crosses to
//! another thread - `pw::loop_::Loop`'s event/timer/io sources all borrow
//! the loop and are not `Send`. Commands arrive from the async side over a
//! plain `crossbeam_channel`; a self-pipe (eventfd) wakes this thread's
//! `add_io` source to drain it, since PipeWire's own `EventSource` can't be
//! constructed on one thread and signaled from another (its `signal()`
//! borrows the loop with a non-'static lifetime).

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::airplay::audio::{LiveFrameSender, LivePcmFrame};
use crate::airplay::core::device::DeviceId;

use super::commands::{PwCommand, SinkEvent};
use super::registry::{self, SinkAction, SinkEntry};

/// Shared between every sink's process() callback: which device (if any)
/// is currently active, and the channel to forward its PCM through.
///
/// Arc<RwLock<_>>, deliberately not Rc<RefCell<_>>: pipewire-rs's own docs
/// on `process()` say it is "normally called from the mainloop but can
/// also be called directly from the realtime data thread" when
/// StreamFlags::RT_PROCESS is set (which every sink here uses, matching
/// capture.rs) - so despite every *other* callback in this module running
/// safely single-threaded on the PW mainloop thread, this one specific
/// value cannot assume that, and needs real synchronization.
pub type ActiveTarget = Arc<RwLock<Option<(DeviceId, LiveFrameSender)>>>;

/// A sample louder than this (out of i16::MAX) counts as audio actually
/// playing. Not zero, so dither or a filter chain's noise floor on an
/// otherwise-paused stream doesn't keep a sink "playing" forever.
const AUDIBLE_THRESHOLD: i16 = 4;

/// How long a sink must stay below AUDIBLE_THRESHOLD before it counts as
/// stopped even though a stream feeding it is still running - for apps
/// that keep a stream open while silent (e.g. a browser tab). Long enough
/// to ride out quiet passages and gaps between tracks.
const IDLE_AFTER: Duration = Duration::from_secs(10);

/// How long after the last stream feeding a sink stops running (e.g.
/// Spotify was paused) the sink counts as stopped, which releases the
/// AirPlay connection. Short, since pausing is unambiguous; just enough
/// that a quick pause/play doesn't cost a reconnect.
const RELEASE_AFTER: Duration = Duration::from_secs(3);

/// What's connected to what in the PipeWire graph, and which nodes are
/// running - enough to tell whether any stream feeding a sink is playing
/// (as opposed to paused/corked, which stops it running).
#[derive(Default)]
struct Graph {
    /// Link id -> (output node id, input node id).
    links: HashMap<u32, (u32, u32)>,
    /// Node id -> whether that node is currently running.
    running: HashMap<u32, bool>,
}

impl Graph {
    fn has_running_input(&self, node_id: u32) -> bool {
        self.links
            .values()
            .any(|(output, input)| *input == node_id && self.running.get(output) == Some(&true))
    }
}

use spa::sys::{
    SPA_PROP_channelVolumes, SPA_PROP_mute, SPA_PROP_softMute, SPA_PROP_softVolumes,
};

/// A minimal self-pipe used purely to wake the PW thread's `add_io` source
/// from another thread. `libc::eventfd` gives us a single fd that supports
/// both write-to-signal and read-to-drain, cheaper than a real pipe.
struct EventFd(std::os::unix::io::RawFd);

impl EventFd {
    fn new() -> std::io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(fd))
    }

    /// Drain the eventfd's counter so it stops being readable until the
    /// next `PwWaker::wake()`.
    fn drain(&self) {
        let mut buf: u64 = 0;
        unsafe {
            let _ = libc::read(
                self.0,
                &mut buf as *mut u64 as *mut libc::c_void,
                std::mem::size_of::<u64>(),
            );
        }
    }
}

impl std::os::unix::io::AsRawFd for EventFd {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.0
    }
}

impl Drop for EventFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Cheap, Send+Sync handle the async side uses to wake the PW thread after
/// pushing a command onto the crossbeam channel. Wraps only the writable
/// side of the eventfd - `EventFd` itself stays on the PW thread as the
/// add_io source's owned IO object.
#[derive(Clone)]
pub struct PwWaker {
    fd: std::os::unix::io::RawFd,
}

impl PwWaker {
    pub fn wake(&self) {
        let one: u64 = 1;
        unsafe {
            let _ = libc::write(
                self.fd,
                &one as *const u64 as *const libc::c_void,
                std::mem::size_of::<u64>(),
            );
        }
    }
}

// SAFETY: PwWaker only ever calls write(2) on a raw fd, which is thread-safe.
unsafe impl Send for PwWaker {}
unsafe impl Sync for PwWaker {}

// The listener exists only to be kept alive (never read again after
// construction): its Drop unregisters it, and the stream's Drop closes its
// PipeWire resources. Dropping either early would tear the sink down while
// it might still be needed.
//
// The per-sink driver timer is NOT stored here - see `timers` in `run()`
// for why, and for the bug this split fixes (a leaked timer closure was
// keeping every "destroyed" sink's stream alive forever).
struct LiveSink {
    stream: pw::stream::StreamRc,
    #[allow(dead_code)]
    listener: pw::stream::StreamListener<()>,
    /// Written by process(): milliseconds since `run()`'s `clock_start`
    /// at which this sink last received an audible sample, 0 if never.
    last_audible_ms: Arc<AtomicU64>,
    /// The rest is only touched by the driver timer, on the PW mainloop
    /// thread. Whether a stream feeding this sink was running last tick.
    input_running: std::cell::Cell<bool>,
    /// Milliseconds since `clock_start` at which a stream feeding this
    /// sink was last seen running, 0 if never.
    last_running_ms: std::cell::Cell<u64>,
    /// Whether the last ActivityChanged sent for this sink said playing.
    reported_playing: std::cell::Cell<bool>,
}

/// Runs on its own dedicated OS thread for the process's lifetime (or
/// until PwCommand::Shutdown). Blocking; the caller should spawn it with
/// `std::thread::Builder::new().name("pw-mainloop")`.
///
/// `waker_tx` is used exactly once, at startup, to hand the constructed
/// `PwWaker` back to the caller so it can be paired with `cmd_tx` and
/// handed out to the rest of the process.
pub fn run(
    cmd_rx: crossbeam_channel::Receiver<PwCommand>,
    waker_tx: std::sync::mpsc::Sender<PwWaker>,
    sink_event_tx: UnboundedSender<SinkEvent>,
    sample_rate: u32,
) -> anyhow::Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let active_target: ActiveTarget = Arc::new(RwLock::new(None));
    let clock_start = Instant::now();

    let eventfd = EventFd::new()?;
    let waker = PwWaker {
        fd: eventfd.as_raw_fd(),
    };
    // If the receiving end is gone the caller already stopped caring about
    // us; nothing to do but proceed running (Shutdown will never arrive,
    // but that just means this thread runs until process exit like today's
    // capture.rs does).
    let _ = waker_tx.send(waker);

    // Registry of live sinks, kept in a local so it never needs to leave
    // this thread. Mirrors (but is not identical to) the async side's
    // DeviceId -> Device map used for picking a target - this one
    // additionally tracks each sink's node_name and live PipeWire objects.
    // Rc<RefCell<_>> (not Mutex) because everything here runs
    // single-threaded on this one PW thread - the Rc lets both the add_io
    // closure (creates/destroys sinks) and the shared driver timer's
    // closure below (iterates them each tick) hold their own reference to
    // the same map; RefCell gives the interior mutability add_io's Fn (not
    // FnMut) bound requires even without any real concurrency.
    let sinks: std::rc::Rc<std::cell::RefCell<HashMap<DeviceId, LiveSink>>> =
        std::rc::Rc::new(std::cell::RefCell::new(HashMap::new()));
    let entries: std::cell::RefCell<HashMap<DeviceId, SinkEntry>> =
        std::cell::RefCell::new(HashMap::new());

    // Track links and node run states, so the driver timer can tell when
    // a stream feeding one of our sinks starts or stops playing. Every
    // node is bound (there are only a few dozen on a desktop) since its
    // run state only arrives through a bound proxy's info events.
    let graph = std::rc::Rc::new(std::cell::RefCell::new(Graph::default()));
    let bound_nodes: std::rc::Rc<
        std::cell::RefCell<HashMap<u32, (pw::node::Node, pw::node::NodeListener)>>,
    > = std::rc::Rc::new(std::cell::RefCell::new(HashMap::new()));
    let registry = core.get_registry_rc()?;
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let registry = registry.clone();
            let graph = std::rc::Rc::clone(&graph);
            let bound_nodes = std::rc::Rc::clone(&bound_nodes);
            move |global| match global.type_ {
                pw::types::ObjectType::Link => {
                    let node_prop = |key| {
                        global.props.and_then(|p| p.get(key)).and_then(|v| v.parse::<u32>().ok())
                    };
                    if let (Some(output), Some(input)) =
                        (node_prop("link.output.node"), node_prop("link.input.node"))
                    {
                        graph.borrow_mut().links.insert(global.id, (output, input));
                    }
                }
                pw::types::ObjectType::Node => {
                    let node: pw::node::Node = match registry.bind(global) {
                        Ok(node) => node,
                        Err(e) => {
                            debug!("failed to bind node {}: {}", global.id, e);
                            return;
                        }
                    };
                    let id = global.id;
                    let graph = std::rc::Rc::clone(&graph);
                    let listener = node
                        .add_listener_local()
                        .info(move |info| {
                            let running = matches!(info.state(), pw::node::NodeState::Running);
                            graph.borrow_mut().running.insert(id, running);
                        })
                        .register();
                    bound_nodes.borrow_mut().insert(id, (node, listener));
                }
                _ => {}
            }
        })
        .global_remove({
            let graph = std::rc::Rc::clone(&graph);
            let bound_nodes = std::rc::Rc::clone(&bound_nodes);
            move |id| {
                let mut graph = graph.borrow_mut();
                graph.links.remove(&id);
                graph.running.remove(&id);
                bound_nodes.borrow_mut().remove(&id);
            }
        })
        .register();

    // Wake the mainloop whenever the async side pushes a command.
    //
    // add_io requires its callback to be 'static, so we can't capture the
    // `&Loop` borrowed from `mainloop` (tied to this stack frame) directly -
    // instead capture an owned, cloned MainLoopRc and re-derive `&Loop`
    // from it inside the callback each time. MainLoopRc::loop_() always
    // returns the same underlying loop either way.
    let mainloop_for_io = mainloop.clone();
    let _io_source = mainloop.loop_().add_io(eventfd, spa::support::system::IoFlags::IN, {
        let core = core.clone();
        let sinks = std::rc::Rc::clone(&sinks);
        let sink_event_tx = sink_event_tx.clone();
        move |eventfd: &mut EventFd| {
            eventfd.drain();
            while let Ok(cmd) = cmd_rx.try_recv() {
                match cmd {
                    PwCommand::HandleBrowseEvent(event) => {
                        handle_browse_event(
                            &core,
                            &mut sinks.borrow_mut(),
                            &mut entries.borrow_mut(),
                            &active_target,
                            clock_start,
                            sample_rate,
                            &sink_event_tx,
                            &event,
                        );
                    }
                    PwCommand::SetActiveTarget(target) => {
                        *active_target.write().unwrap() = target;
                    }
                    PwCommand::SetSinkVolume { id, volume } => {
                        if let Some(live_sink) = sinks.borrow().get(&id) {
                            set_sink_volume(&live_sink.stream, volume);
                        }
                    }
                    PwCommand::Shutdown => {
                        mainloop_for_io.quit();
                    }
                }
            }
        }
    });

    // Every sink is a DRIVER stream (nothing else pulls cycles through a
    // virtual sink node), so something has to trigger_process() each of
    // them on a steady clock, same as capture.rs does for its one static
    // sink. Rather than one TimerSource per sink - which doesn't fit this
    // module's threading model, see the long comment in create_sink() -
    // this single shared timer, with the same lifetime as `_io_source`
    // above, ticks once per period and drives whatever's currently in
    // `sinks`. A destroyed sink just stops being in that iteration from
    // the next tick onward; nothing needs to be armed/disarmed per sink.
    let period = Duration::from_secs_f64(1024.0 / sample_rate as f64);
    //
    // The same tick also turns each sink's input streams' run state and
    // last-audible timestamp into SinkEvents - done here rather than in
    // process() because process() may run on the realtime data thread and
    // must not touch the (allocating) tokio channel.
    //
    // A sink is playing while a stream feeding it is running (or stopped
    // less than RELEASE_AFTER ago) AND it has heard audio within
    // IDLE_AFTER. A stream going from stopped to running is a deliberate
    // play press, reported separately as PlaybackStarted.
    let sinks_for_timer = std::rc::Rc::clone(&sinks);
    let graph_for_timer = std::rc::Rc::clone(&graph);
    let _driver_timer = mainloop.loop_().add_timer(move |_expirations| {
        let now_ms = clock_start.elapsed().as_millis() as u64;
        let within = |ms: u64, window: Duration| {
            ms != 0 && now_ms.saturating_sub(ms) < window.as_millis() as u64
        };
        let graph = graph_for_timer.borrow();
        let send = |event: SinkEvent| {
            if let Err(e) = sink_event_tx.send(event) {
                warn!("failed to send SinkEvent: {}", e);
            }
        };
        for (id, live_sink) in sinks_for_timer.borrow().iter() {
            if let Err(e) = live_sink.stream.trigger_process() {
                debug!("trigger_process failed (sink likely mid-teardown): {}", e);
            }

            let input_running = graph.has_running_input(live_sink.stream.node_id());
            if input_running {
                if !live_sink.input_running.get() {
                    send(SinkEvent::PlaybackStarted { id: id.clone() });
                }
                live_sink.last_running_ms.set(now_ms.max(1));
            }
            live_sink.input_running.set(input_running);

            let playing = within(live_sink.last_running_ms.get(), RELEASE_AFTER)
                && within(live_sink.last_audible_ms.load(Ordering::Relaxed), IDLE_AFTER);
            if playing != live_sink.reported_playing.get() {
                live_sink.reported_playing.set(playing);
                send(SinkEvent::ActivityChanged { id: id.clone(), playing });
            }
        }
    });
    _driver_timer
        .update_timer(Some(period), Some(period))
        .into_result()?;

    info!("PipeWire sink manager thread running");
    mainloop.run();

    // Every LiveSink's stream drops here, after mainloop.run() returns (on Shutdown), which is the
    // correct point to release PipeWire resources.
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_browse_event(
    core: &pw::core::CoreRc,
    sinks: &mut HashMap<DeviceId, LiveSink>,
    entries: &mut HashMap<DeviceId, SinkEntry>,
    active_target: &ActiveTarget,
    clock_start: Instant,
    sample_rate: u32,
    sink_event_tx: &UnboundedSender<SinkEvent>,
    event: &crate::airplay::discovery::BrowseEvent,
) {
    let action = registry::plan_sink_action(entries, event);
    match action {
        SinkAction::Create { device, node_name } => {
            match create_sink(
                core,
                &device.id,
                &node_name,
                clock_start,
                sample_rate,
                active_target,
                sink_event_tx,
            ) {
                Ok(live_sink) => {
                    info!(
                        "created sink \"{}\" for device {} ({})",
                        node_name,
                        device.name,
                        device.id.to_mac_string()
                    );
                    sinks.insert(device.id.clone(), live_sink);
                    entries.insert(
                        device.id.clone(),
                        SinkEntry {
                            device,
                            node_name,
                        },
                    );
                }
                Err(e) => {
                    warn!("failed to create sink for {}: {}", device.name, e);
                }
            }
        }
        SinkAction::UpdateDeviceInfo { id, device } => {
            if let Some(entry) = entries.get_mut(&id) {
                entry.device = device;
            }
        }
        SinkAction::Destroy { id } => {
            // Dropping the LiveSink here drops its StreamRc/StreamListener,
            // which tears down the underlying PipeWire node. This is safe
            // to do without any per-sink timer bookkeeping now that driving
            // sinks is handled by ONE shared timer (see `driver_timer` in
            // `run()`) that iterates whatever's currently in `sinks` on
            // every tick - a destroyed sink just stops appearing in that
            // iteration from the next tick onward, nothing to disarm here.
            if sinks.remove(&id).is_some() {
                info!("destroyed sink for device {}", id.to_mac_string());
            }
            entries.remove(&id);
            // If the device that just vanished was the active PCM target,
            // clear it so its (now-gone) sink's stream callback - which
            // will fire its own teardown independently - doesn't matter
            // either way, and so nothing keeps forwarding into a sender
            // whose connection is about to be torn down by the async side
            // once it observes the resulting SinkEvent.
            let mut guard = active_target.write().unwrap();
            if guard.as_ref().map(|(active_id, _)| active_id) == Some(&id) {
                *guard = None;
            }
        }
        SinkAction::NoOp => {}
    }
}

/// Move a sink's volume slider, as if the user had: its control_info
/// callback then reports the change like any other.
fn set_sink_volume(stream: &pw::stream::StreamRc, volume: f32) {
    // Slider position -> linear channel volume, the inverse of the cbrt in
    // create_sink's control_info. Every sink is stereo.
    let channel_volume = volume.clamp(0.0, 1.0).powi(3);
    // Each set must be followed directly by re-pinning the soft control:
    // unlike a change from outside (wpctl, a picker), one made by the
    // stream itself turns PipeWire's software volume back on, and the
    // re-pin in control_info doesn't undo it - measured on a test sink, a
    // self-set 0.125 cut a 16000-peak tone to 2000 until this was added.
    let result = stream
        .set_control(SPA_PROP_channelVolumes, &[channel_volume; 2])
        .and_then(|()| stream.set_control(SPA_PROP_softVolumes, &[1.0; 2]));
    if let Err(e) = result {
        warn!("failed to set sink volume: {}", e);
    }
    if volume > 0.0 {
        let result = stream
            .set_control(SPA_PROP_mute, &[0.0])
            .and_then(|()| stream.set_control(SPA_PROP_softMute, &[0.0]));
        if let Err(e) = result {
            warn!("failed to unmute sink: {}", e);
        }
    }
}

fn create_sink(
    core: &pw::core::CoreRc,
    device_id: &DeviceId,
    node_name: &str,
    clock_start: Instant,
    sample_rate: u32,
    active_target: &ActiveTarget,
    sink_event_tx: &UnboundedSender<SinkEvent>,
) -> anyhow::Result<LiveSink> {
    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::NODE_NAME => node_name,
        *pw::keys::NODE_DESCRIPTION => format!("{node_name} (AirPlay)"),
        *pw::keys::NODE_VIRTUAL => "true",
        *pw::keys::NODE_ALWAYS_PROCESS => "true",
    };

    let stream = pw::stream::StreamRc::new(core.clone(), node_name, props.to_owned())?;

    let my_id = device_id.clone();
    let active_target = Arc::clone(active_target);
    let last_audible_ms = Arc::new(AtomicU64::new(0));
    let last_audible_for_process = Arc::clone(&last_audible_ms);
    let sink_event_tx = sink_event_tx.clone();

    // Last-seen PipeWire volume state, to turn control_info updates into
    // one VolumeChanged event per actual change.
    let volume_id = device_id.clone();
    let mut channel_volume = 1.0_f32;
    let mut muted = false;
    let mut reported_volume: Option<f32> = None;

    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_stream, _data, old, new| {
            debug!("sink stream state: {:?} -> {:?}", old, new);
        })
        // The sink's volume and mute become the AirPlay device's own volume
        // (see main.rs) - so PipeWire must not ALSO apply them to the PCM,
        // or the audio would be attenuated twice. PipeWire's audioconvert
        // scales by channelVolumes until the node sets softVolumes itself
        // (as ALSA sinks with a hardware mixer do), so pin softVolumes and
        // softMute to unity/unmuted whenever volume or mute change.
        .control_info(move |stream, _data, id, control| {
            // SAFETY: PipeWire passes a valid control for the duration of
            // this callback, whose `values` holds `n_values` floats.
            let values = unsafe {
                let control = &*control;
                if control.values.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(control.values, control.n_values as usize)
                }
            };
            debug!("sink {} control {} = {:?}", volume_id.to_mac_string(), id, values);
            match id {
                _ if id == SPA_PROP_channelVolumes && !values.is_empty() => {
                    if let Err(e) = stream.set_control(SPA_PROP_softVolumes, &vec![1.0; values.len()]) {
                        warn!("failed to bypass software volume: {}", e);
                    }
                    channel_volume = values.iter().copied().fold(0.0, f32::max);
                }
                _ if id == SPA_PROP_mute && !values.is_empty() => {
                    if let Err(e) = stream.set_control(SPA_PROP_softMute, &[0.0]) {
                        warn!("failed to bypass software mute: {}", e);
                    }
                    muted = values[0] > 0.5;
                }
                _ => return,
            }

            // channelVolumes are linear amplitude; the slider position
            // users see (and AirPlay's dB scale is laid over) is its cube
            // root.
            let volume = if muted { 0.0 } else { channel_volume.cbrt().min(1.0) };
            if reported_volume != Some(volume) {
                reported_volume = Some(volume);
                let event = SinkEvent::VolumeChanged { id: volume_id.clone(), volume };
                if let Err(e) = sink_event_tx.send(event) {
                    warn!("failed to send SinkEvent: {}", e);
                }
            }
        })
        .process(move |stream, _data| {
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

            let audible = valid.chunks_exact(2).any(|chunk| {
                i16::from_le_bytes([chunk[0], chunk[1]]).saturating_abs() > AUDIBLE_THRESHOLD
            });
            if audible {
                // max(1): 0 is reserved for "never audible".
                let now_ms = (clock_start.elapsed().as_millis() as u64).max(1);
                last_audible_for_process.store(now_ms, Ordering::Relaxed);
            }

            // Only forward PCM if this sink is the currently-active
            // target; otherwise the buffer is still dequeued/recycled
            // above (keeping the sink alive and functional as a normal,
            // silently-idle audio output) but nothing is sent anywhere.
            let guard = active_target.read().unwrap();
            if let Some((active_id, sender)) = guard.as_ref() {
                if active_id == &my_id && !valid.is_empty() {
                    let mut samples = vec![0i16; valid.len() / 2];
                    for (i, chunk) in valid.chunks_exact(2).enumerate() {
                        samples[i] = i16::from_le_bytes([chunk[0], chunk[1]]);
                    }
                    // try_send, never blocking send: this callback runs on
                    // the PW mainloop thread, which also has to service
                    // every other sink's timer and the async side's
                    // commands - it must never stall on AirPlay-side
                    // backpressure the way main.rs's dedicated stdin
                    // thread is allowed to.
                    sender.try_send(LivePcmFrame {
                        samples,
                        channels: 2,
                        sample_rate,
                    });
                }
            }
        })
        .register()?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::S16LE);
    audio_info.set_rate(sample_rate);
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

    // As a DRIVER stream, nothing else pulls cycles through our node - it
    // needs to be triggered on a steady clock, same as capture.rs. That's
    // handled by ONE shared timer for every sink (see `driver_timer` in
    // `run()`) rather than a per-sink timer here: a per-sink TimerSource
    // borrows the mainloop's `Loop` with a lifetime that can't be named
    // consistently with both `run()`'s outer `sinks` map (which must
    // outlive any single event-loop callback) and the 'static bound on the
    // add_io callback that drives sink creation/destruction. A previous
    // version of this function worked around that by `mem::forget`-leaking
    // the timer, which turned out to be an actual bug, not a harmless
    // leak: the leaked closure held a `StreamRc` clone that kept the
    // stream's refcount above zero forever, so destroying a sink never
    // actually dropped its PipeWire node - confirmed live via repeated
    // mDNS churn leaving duplicate zombie sinks in `pactl` indefinitely.

    Ok(LiveSink {
        stream,
        listener,
        last_audible_ms,
        input_running: std::cell::Cell::new(false),
        last_running_ms: std::cell::Cell::new(0),
        reported_playing: std::cell::Cell::new(false),
    })
}
