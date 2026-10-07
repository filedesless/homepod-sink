//! Owns a PipeWire virtual sink per discovered AirPlay device, and reports
//! when each one starts or stops receiving audio - whether because it's
//! the default output or because a single app (e.g. Spotify) was routed
//! to it. Only the active device's captured PCM is forwarded to a live
//! AirPlay connection.

pub mod commands;
pub mod pw_thread;
pub mod registry;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::airplay::audio::LiveFrameSender;
use crate::airplay::core::device::DeviceId;
use crate::airplay::discovery::BrowseEvent;

pub use commands::{Arbiter, PwCommand, SinkEvent};

/// Handle for the async side to talk to the PipeWire mainloop thread.
pub struct SinkManager {
    cmd_tx: crossbeam_channel::Sender<PwCommand>,
    waker: pw_thread::PwWaker,
}

impl SinkManager {
    /// Spawns the dedicated PipeWire mainloop thread and returns a handle
    /// to it plus the channel on which `SinkEvent`s (sink activity
    /// changes) will arrive.
    pub fn spawn(sample_rate: u32) -> anyhow::Result<(Self, UnboundedReceiver<SinkEvent>)> {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<PwCommand>();
        let (waker_tx, waker_rx) = std::sync::mpsc::channel();
        let (sink_event_tx, sink_event_rx) = tokio::sync::mpsc::unbounded_channel();

        std::thread::Builder::new()
            .name("pw-mainloop".into())
            .spawn(move || {
                // ActiveTarget is Rc-based (single-threaded on purpose - see
                // its doc comment) and so is constructed here, on the PW
                // thread itself, rather than passed in from the spawning
                // thread (which would require it to be Send).
                if let Err(e) = pw_thread::run(cmd_rx, waker_tx, sink_event_tx, sample_rate)
                {
                    tracing::error!("PipeWire sink manager thread exited with error: {:#}", e);
                }
            })
            .map_err(|e| anyhow::anyhow!("failed to spawn pw-mainloop thread: {e}"))?;

        // The PW thread constructs its own waker (tied to the eventfd it
        // owns) and hands it back once its loop exists - block briefly for
        // that handoff. Not the tokio runtime's job to wait on a plain OS
        // thread, so this uses a std channel and a bounded wait.
        let waker = waker_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("timed out waiting for pw-mainloop thread to start"))?;

        Ok((Self { cmd_tx, waker }, sink_event_rx))
    }

    /// Feed a discovery event into the sink registry - creates, updates, or
    /// destroys a PipeWire sink as appropriate.
    pub fn handle_browse_event(&self, event: BrowseEvent) {
        let _ = self.cmd_tx.send(PwCommand::HandleBrowseEvent(Box::new(event)));
        self.waker.wake();
    }

    /// Install (or clear, with None) the live PCM forwarding target: which
    /// device's sink should have its captured audio sent through `sender`.
    pub fn set_active_target(&self, target: Option<(DeviceId, LiveFrameSender)>) {
        let _ = self.cmd_tx.send(PwCommand::SetActiveTarget(target));
        self.waker.wake();
    }

    /// Move a device's sink volume slider to `volume` (0.0-1.0).
    pub fn set_sink_volume(&self, id: DeviceId, volume: f32) {
        let _ = self.cmd_tx.send(PwCommand::SetSinkVolume { id, volume });
        self.waker.wake();
    }

    /// Stop the PipeWire mainloop thread.
    #[allow(dead_code)]
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(PwCommand::Shutdown);
        self.waker.wake();
    }
}
