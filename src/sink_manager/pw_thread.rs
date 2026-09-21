//! The PipeWire mainloop thread: owns one virtual sink per discovered
//! AirPlay device, watches PipeWire's `default.audio.sink` metadata, and
//! forwards captured PCM for whichever sink is currently active.
//!
//! Everything PipeWire-owned (streams, listeners, timers, the registry)
//! lives entirely on this one dedicated OS thread and never crosses to
//! another thread - `pw::loop_::Loop`'s event/timer/io sources all borrow
//! the loop and are not `Send`. Commands arrive from the async side over a
//! plain `crossbeam_channel`; a self-pipe (eventfd) wakes this thread's
//! `add_io` source to drain it, since PipeWire's own `EventSource` can't be
//! constructed on one thread and signaled from another (its `signal()`
//! borrows the loop with a non-'static lifetime).

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

use crate::airplay::audio::{LiveFrameSender, LivePcmFrame};
use crate::airplay::core::device::DeviceId;

use super::commands::{PwCommand, SinkEvent};
use super::registry::{
    self, SinkAction, SinkEntry,
};

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

// Both fields exist only to be kept alive (never read again after
// construction): the stream's Drop closes its PipeWire resources, and the
// listener's Drop unregisters it. Dropping either early would tear the
// sink down while it might still be needed.
//
// The per-sink driver timer is NOT stored here - see `timers` in `run()`
// for why, and for the bug this split fixes (a leaked timer closure was
// keeping every "destroyed" sink's stream alive forever).
#[allow(dead_code)]
struct LiveSink {
    stream: pw::stream::StreamRc,
    listener: pw::stream::StreamListener<()>,
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
    // DeviceId -> Device map used for StreamCommand derivation - this one
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

    let registry = core.get_registry_rc()?;

    // Registry listener: look for the "default" metadata object (there can
    // be other Metadata globals; only this one carries
    // default.audio.sink/source) and bind + listen to it once found.
    //
    // Rc<RefCell<_>>, not Arc<RwLock<_>>: every closure here runs on this
    // one PW thread only (PipeWire's local listeners are single-threaded
    // callbacks, never invoked concurrently), and Metadata/MetadataListener
    // are themselves !Send/!Sync (raw-pointer-backed), so Arc would be
    // actively misleading about the actual thread-safety story.
    let entries_for_metadata = std::rc::Rc::new(std::cell::RefCell::new(HashMap::<DeviceId, SinkEntry>::new()));
    let sink_event_tx_for_metadata = sink_event_tx.clone();
    let registry_for_bind = registry.clone();
    let metadata_slot: std::rc::Rc<std::cell::RefCell<Option<pw::metadata::Metadata>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let metadata_listener_slot: std::rc::Rc<std::cell::RefCell<Option<pw::metadata::MetadataListener>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let metadata_slot_for_cb = std::rc::Rc::clone(&metadata_slot);
    let metadata_listener_slot_for_cb = std::rc::Rc::clone(&metadata_listener_slot);
    let entries_for_metadata_cb = std::rc::Rc::clone(&entries_for_metadata);

    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != pw::types::ObjectType::Metadata {
                return;
            }
            let is_default_metadata = global
                .props
                .and_then(|p| p.get("metadata.name"))
                .map(|name| name == "default")
                .unwrap_or(false);
            if !is_default_metadata {
                return;
            }
            if metadata_slot_for_cb.borrow().is_some() {
                // Already bound (shouldn't normally fire twice, but guard anyway).
                return;
            }

            let bound: Result<pw::metadata::Metadata, _> = registry_for_bind.bind(global);
            let bound = match bound {
                Ok(m) => m,
                Err(e) => {
                    warn!("failed to bind default metadata object: {}", e);
                    return;
                }
            };

            let entries_for_prop = std::rc::Rc::clone(&entries_for_metadata_cb);
            let tx = sink_event_tx_for_metadata.clone();
            let listener = bound
                .add_listener_local()
                .property(move |_subject, key, _type, value| {
                    if key != Some("default.audio.sink") {
                        return 0;
                    }
                    let resolved = match value {
                        None => None,
                        Some(v) => match registry::parse_default_sink_node_name(v) {
                            Some(node_name) => {
                                let entries = entries_for_prop.borrow();
                                let id = registry::resolve_node_name_to_device_id(
                                    &entries, &node_name,
                                );
                                if id.is_none() {
                                    debug!(
                                        "default.audio.sink is \"{}\", not one of ours",
                                        node_name
                                    );
                                }
                                id
                            }
                            None => {
                                warn!("malformed default.audio.sink metadata value: {:?}", v);
                                None
                            }
                        },
                    };
                    if let Err(e) = tx.send(SinkEvent::DefaultSinkChanged(resolved)) {
                        warn!("failed to send SinkEvent: {}", e);
                    }
                    0
                })
                .register();

            *metadata_slot_for_cb.borrow_mut() = Some(bound);
            *metadata_listener_slot_for_cb.borrow_mut() = Some(listener);
            info!("bound PipeWire default metadata, watching default.audio.sink");
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
                            sample_rate,
                            &event,
                        );
                        *entries_for_metadata.borrow_mut() = entries.borrow().clone();
                    }
                    PwCommand::SetActiveTarget(target) => {
                        *active_target.write().unwrap() = target;
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
    let sinks_for_timer = std::rc::Rc::clone(&sinks);
    let _driver_timer = mainloop.loop_().add_timer(move |_expirations| {
        for live_sink in sinks_for_timer.borrow().values() {
            if let Err(e) = live_sink.stream.trigger_process() {
                debug!("trigger_process failed (sink likely mid-teardown): {}", e);
            }
        }
    });
    _driver_timer
        .update_timer(Some(period), Some(period))
        .into_result()?;

    info!("PipeWire sink manager thread running");
    mainloop.run();

    // metadata_slot/metadata_listener_slot and every LiveSink's stream drop
    // here, after mainloop.run() returns (on Shutdown), which is the
    // correct point to release PipeWire resources.
    Ok(())
}

fn handle_browse_event(
    core: &pw::core::CoreRc,
    sinks: &mut HashMap<DeviceId, LiveSink>,
    entries: &mut HashMap<DeviceId, SinkEntry>,
    active_target: &ActiveTarget,
    sample_rate: u32,
    event: &crate::airplay::discovery::BrowseEvent,
) {
    let action = registry::plan_sink_action(entries, event);
    match action {
        SinkAction::Create { device, node_name } => {
            match create_sink(core, &device.id, &node_name, sample_rate, active_target) {
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

fn create_sink(
    core: &pw::core::CoreRc,
    device_id: &DeviceId,
    node_name: &str,
    sample_rate: u32,
    active_target: &ActiveTarget,
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

    let listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_stream, _data, old, new| {
            debug!("sink stream state: {:?} -> {:?}", old, new);
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
                    // every other sink's timer and the metadata/registry
                    // listeners - it must never stall on AirPlay-side
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

    Ok(LiveSink { stream, listener })
}
