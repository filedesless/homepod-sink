//! Message types crossing the PipeWire-thread / async-runtime boundary,
//! plus the pure logic for deciding which AirPlay connection (if any)
//! should be active given which sinks are currently receiving audio.

use std::collections::{HashMap, HashSet};

use crate::airplay::core::device::{Device, DeviceId};
use crate::airplay::discovery::BrowseEvent;

/// Sent from the async side into the PipeWire mainloop thread.
pub enum PwCommand {
    /// A discovery BrowseEvent arrived - create/update/destroy a sink.
    /// Boxed: BrowseEvent embeds a full Device (640+ bytes), which would
    /// otherwise size every PwCommand to the largest variant even though
    /// this one is sent relatively rarely (once per discovery event).
    HandleBrowseEvent(Box<BrowseEvent>),
    /// Install (or clear) the live PCM target: which DeviceId's sink
    /// should have its captured audio forwarded to the AirPlay sender,
    /// and the channel to forward it through.
    SetActiveTarget(Option<(DeviceId, crate::airplay::audio::LiveFrameSender)>),
    /// Move a device's sink volume slider (0.0-1.0, unmuting if above 0),
    /// e.g. to match the volume the device itself reported.
    SetSinkVolume { id: DeviceId, volume: f32 },
    /// Stop the PipeWire mainloop and exit its thread.
    Shutdown,
}

/// Sent from the PipeWire mainloop thread out to the async side.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkEvent {
    /// A device's sink started playing (`playing: true`) - a stream
    /// feeding it is running and audio is coming through - or stopped
    /// (`false`): its streams were paused, or it has been silent for a
    /// while. Any app routed to the sink counts - it doesn't need to be
    /// the default output.
    ActivityChanged { id: DeviceId, playing: bool },
    /// A stream feeding a device's sink went from stopped to running:
    /// someone pressed play on this machine.
    PlaybackStarted { id: DeviceId },
    /// A device's sink volume changed in PipeWire, as a 0.0-1.0 slider
    /// position (what pavucontrol/wpctl show), 0.0 when muted.
    VolumeChanged { id: DeviceId, volume: f32 },
}

/// Decides which device (if any) should have a live AirPlay connection.
///
/// A device is wanted while audio is playing into its sink, unless another
/// AirPlay sender (e.g. an iPhone) took the speaker over from us - then we
/// leave it alone until play is pressed again here, rather than stealing
/// it straight back while this machine's audio carries on regardless.
///
/// Pure and separately testable so the decision doesn't need a running
/// tokio select! loop or real channels to verify.
#[derive(Debug, Default)]
pub struct Arbiter {
    /// Devices whose sink is currently playing, in the order they started.
    playing: Vec<DeviceId>,
    /// Devices another sender took over mid-playback. Cleared when play
    /// is pressed again here, or the device's sink stops playing.
    yielded: HashSet<DeviceId>,
}

impl Arbiter {
    pub fn handle_sink_event(&mut self, event: &SinkEvent) {
        match event {
            SinkEvent::ActivityChanged { id, playing } => {
                self.playing.retain(|p| p != id);
                if *playing {
                    self.playing.push(id.clone());
                } else {
                    self.yielded.remove(id);
                }
            }
            SinkEvent::PlaybackStarted { id } => {
                self.yielded.remove(id);
            }
            SinkEvent::VolumeChanged { .. } => {}
        }
    }

    pub fn device_removed(&mut self, id: &DeviceId) {
        self.playing.retain(|p| p != id);
        self.yielded.remove(id);
    }

    /// Another sender took `id` over: don't reconnect to it until play is
    /// pressed again here, or its sink stops and starts playing again.
    pub fn yield_device(&mut self, id: &DeviceId) {
        self.yielded.insert(id.clone());
    }

    /// The device that should be streamed to. Sticks with `current` while
    /// it's still wanted, so a second sink starting to play doesn't yank
    /// the connection away; otherwise picks the earliest-started playing
    /// device we know how to reach.
    pub fn target(
        &self,
        current: Option<&DeviceId>,
        known_devices: &HashMap<DeviceId, Device>,
    ) -> Option<DeviceId> {
        let wanted =
            |id: &DeviceId| !self.yielded.contains(id) && known_devices.contains_key(id);
        if let Some(current) = current {
            if self.playing.contains(current) && wanted(current) {
                return Some(current.clone());
            }
        }
        self.playing.iter().find(|id| wanted(id)).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::airplay::core::{Features, Version};
    use std::net::{IpAddr, Ipv4Addr};

    fn make_device(mac: [u8; 6], name: &str) -> Device {
        Device {
            id: DeviceId(mac),
            name: name.to_string(),
            model: "TestModel".to_string(),
            manufacturer: None,
            serial_number: None,
            addresses: vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, mac[5]))],
            port: 7000,
            features: Features::default(),
            required_sender_features: None,
            public_key: None,
            source_version: Version::default(),
            firmware_version: None,
            os_version: None,
            protocol_version: None,
            requires_password: false,
            status_flags: 0,
            access_control: None,
            pairing_identity: None,
            system_pairing_identity: None,
            bluetooth_address: None,
            homekit_home_id: None,
            group_id: None,
            is_group_leader: false,
            group_public_name: None,
            group_contains_discoverable_leader: false,
            home_group_id: None,
            household_id: None,
            parent_group_id: None,
            parent_group_contains_discoverable_leader: false,
            tight_sync_id: None,
            raop_port: None,
            raop_encryption_types: None,
            raop_codecs: None,
            raop_transport: None,
            raop_metadata_types: None,
            raop_digest_auth: false,
            vodka_version: None,
        }
    }

    fn known(devices: &[&Device]) -> HashMap<DeviceId, Device> {
        devices.iter().map(|d| (d.id.clone(), (*d).clone())).collect()
    }

    fn activity(d: &Device, playing: bool) -> SinkEvent {
        SinkEvent::ActivityChanged { id: d.id.clone(), playing }
    }

    #[test]
    fn idle_has_no_target() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let arbiter = Arbiter::default();
        assert_eq!(arbiter.target(None, &known(&[&living_room])), None);
    }

    #[test]
    fn playing_sink_becomes_target() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let mut arbiter = Arbiter::default();
        arbiter.handle_sink_event(&activity(&living_room, true));
        assert_eq!(
            arbiter.target(None, &known(&[&living_room])),
            Some(living_room.id.clone())
        );
    }

    #[test]
    fn going_quiet_clears_target() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let known = known(&[&living_room]);
        let mut arbiter = Arbiter::default();
        arbiter.handle_sink_event(&activity(&living_room, true));
        arbiter.handle_sink_event(&activity(&living_room, false));
        assert_eq!(arbiter.target(Some(&living_room.id), &known), None);
    }

    #[test]
    fn unknown_device_is_never_a_target() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let mut arbiter = Arbiter::default();
        arbiter.handle_sink_event(&activity(&living_room, true));
        assert_eq!(arbiter.target(None, &HashMap::new()), None);
    }

    #[test]
    fn yielded_device_stays_untargeted_until_playback_restarts() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let known = known(&[&living_room]);
        let mut arbiter = Arbiter::default();
        arbiter.handle_sink_event(&activity(&living_room, true));
        arbiter.yield_device(&living_room.id);
        assert_eq!(arbiter.target(Some(&living_room.id), &known), None);

        arbiter.handle_sink_event(&activity(&living_room, false));
        arbiter.handle_sink_event(&activity(&living_room, true));
        assert_eq!(arbiter.target(None, &known), Some(living_room.id.clone()));
    }

    #[test]
    fn pressing_play_reclaims_yielded_device_while_still_playing() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let known = known(&[&living_room]);
        let mut arbiter = Arbiter::default();
        arbiter.handle_sink_event(&activity(&living_room, true));
        arbiter.yield_device(&living_room.id);
        assert_eq!(arbiter.target(None, &known), None);

        arbiter.handle_sink_event(&SinkEvent::PlaybackStarted { id: living_room.id.clone() });
        assert_eq!(arbiter.target(None, &known), Some(living_room.id.clone()));
    }

    #[test]
    fn volume_changes_dont_affect_target() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let known = known(&[&living_room]);
        let mut arbiter = Arbiter::default();
        arbiter.handle_sink_event(&SinkEvent::VolumeChanged { id: living_room.id.clone(), volume: 0.5 });
        assert_eq!(arbiter.target(None, &known), None);
    }

    #[test]
    fn sticks_with_current_while_another_starts() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let bedroom = make_device([2, 0, 0, 0, 0, 2], "Bedroom");
        let known = known(&[&living_room, &bedroom]);
        let mut arbiter = Arbiter::default();
        arbiter.handle_sink_event(&activity(&bedroom, true));
        arbiter.handle_sink_event(&activity(&living_room, true));
        assert_eq!(
            arbiter.target(Some(&living_room.id), &known),
            Some(living_room.id.clone())
        );
        assert_eq!(arbiter.target(None, &known), Some(bedroom.id.clone()));
    }

    #[test]
    fn falls_back_to_other_playing_device() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let bedroom = make_device([2, 0, 0, 0, 0, 2], "Bedroom");
        let known = known(&[&living_room, &bedroom]);
        let mut arbiter = Arbiter::default();
        arbiter.handle_sink_event(&activity(&living_room, true));
        arbiter.handle_sink_event(&activity(&bedroom, true));
        arbiter.handle_sink_event(&activity(&living_room, false));
        assert_eq!(
            arbiter.target(Some(&living_room.id), &known),
            Some(bedroom.id.clone())
        );
    }

    #[test]
    fn removed_device_is_forgotten() {
        let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
        let known = known(&[&living_room]);
        let mut arbiter = Arbiter::default();
        arbiter.handle_sink_event(&activity(&living_room, true));
        arbiter.device_removed(&living_room.id);
        assert_eq!(arbiter.target(None, &known), None);
    }
}
