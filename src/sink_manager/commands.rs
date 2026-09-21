//! Message types crossing the PipeWire-thread / async-runtime boundary,
//! plus the pure logic for turning a "default sink changed" event into a
//! decision about which AirPlay connection should be active.

use std::collections::HashMap;

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
    /// Stop the PipeWire mainloop and exit its thread.
    Shutdown,
}

/// Sent from the PipeWire mainloop thread out to the async side.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkEvent {
    /// PipeWire's `default.audio.sink` metadata changed. `None` means it
    /// now points at a sink we don't own (or was cleared) - no device
    /// should be active. `Some(id)` names one of our own device sinks.
    DefaultSinkChanged(Option<DeviceId>),
}

/// What the AirPlay side should do in response to a `SinkEvent`.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamCommand {
    /// Connect (or switch) to this device. Boxed for the same reason as
    /// PwCommand::HandleBrowseEvent - Device is large relative to the
    /// other (data-free) variants.
    ConnectTo(Box<Device>),
    /// No device should be active - disconnect if currently connected.
    Disconnect,
    /// Nothing changed that the AirPlay side needs to react to.
    NoOp,
}

/// Turn a default-sink-changed event into a StreamCommand, given the
/// currently-active device (if any) and a lookup of known devices by id.
///
/// Pure and separately testable so the switching decision doesn't need a
/// running tokio select! loop or real channels to verify.
pub fn derive_stream_command(
    currently_active: Option<&DeviceId>,
    event: &SinkEvent,
    known_devices: &HashMap<DeviceId, Device>,
) -> StreamCommand {
    let SinkEvent::DefaultSinkChanged(new_id) = event;

    match new_id {
        None => {
            if currently_active.is_some() {
                StreamCommand::Disconnect
            } else {
                StreamCommand::NoOp
            }
        }
        Some(id) => {
            if currently_active == Some(id) {
                return StreamCommand::NoOp;
            }
            match known_devices.get(id) {
                Some(device) => StreamCommand::ConnectTo(Box::new(device.clone())),
                // Default sink points at a device id we don't (or no
                // longer) know about - e.g. it vanished from discovery in
                // the same moment it stopped being default. Treat like no
                // active device rather than connecting to stale info.
                None => {
                    if currently_active.is_some() {
                        StreamCommand::Disconnect
                    } else {
                        StreamCommand::NoOp
                    }
                }
            }
        }
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

    mod derive_stream_command {
        use super::*;

        #[test]
        fn none_to_none_is_noop() {
            let known = HashMap::new();
            let cmd = derive_stream_command(None, &SinkEvent::DefaultSinkChanged(None), &known);
            assert_eq!(cmd, StreamCommand::NoOp);
        }

        #[test]
        fn some_to_none_disconnects() {
            let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let known = HashMap::new();
            let cmd = derive_stream_command(
                Some(&living_room.id),
                &SinkEvent::DefaultSinkChanged(None),
                &known,
            );
            assert_eq!(cmd, StreamCommand::Disconnect);
        }

        #[test]
        fn none_to_known_device_connects() {
            let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let mut known = HashMap::new();
            known.insert(living_room.id.clone(), living_room.clone());

            let cmd = derive_stream_command(
                None,
                &SinkEvent::DefaultSinkChanged(Some(living_room.id.clone())),
                &known,
            );
            assert_eq!(cmd, StreamCommand::ConnectTo(Box::new(living_room)));
        }

        #[test]
        fn switching_to_a_different_known_device_connects() {
            let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let bedroom = make_device([2, 0, 0, 0, 0, 2], "Bedroom");
            let mut known = HashMap::new();
            known.insert(living_room.id.clone(), living_room.clone());
            known.insert(bedroom.id.clone(), bedroom.clone());

            let cmd = derive_stream_command(
                Some(&living_room.id),
                &SinkEvent::DefaultSinkChanged(Some(bedroom.id.clone())),
                &known,
            );
            assert_eq!(cmd, StreamCommand::ConnectTo(Box::new(bedroom)));
        }

        #[test]
        fn same_device_again_is_noop() {
            let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let mut known = HashMap::new();
            known.insert(living_room.id.clone(), living_room.clone());

            let cmd = derive_stream_command(
                Some(&living_room.id),
                &SinkEvent::DefaultSinkChanged(Some(living_room.id.clone())),
                &known,
            );
            assert_eq!(cmd, StreamCommand::NoOp);
        }

        #[test]
        fn unknown_device_id_with_no_active_is_noop() {
            let known = HashMap::new();
            let unknown_id = DeviceId([9, 9, 9, 9, 9, 9]);
            let cmd = derive_stream_command(
                None,
                &SinkEvent::DefaultSinkChanged(Some(unknown_id)),
                &known,
            );
            assert_eq!(cmd, StreamCommand::NoOp);
        }

        #[test]
        fn unknown_device_id_while_active_disconnects() {
            let living_room = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let known = HashMap::new(); // living_room not registered here
            let unknown_id = DeviceId([9, 9, 9, 9, 9, 9]);

            let cmd = derive_stream_command(
                Some(&living_room.id),
                &SinkEvent::DefaultSinkChanged(Some(unknown_id)),
                &known,
            );
            assert_eq!(cmd, StreamCommand::Disconnect);
        }
    }
}
