//! Pure logic for tracking which discovered AirPlay devices have a
//! PipeWire sink, and what to do about it as devices come and go.
//!
//! Deliberately has no PipeWire or tokio dependency so it can be unit
//! tested without a running PipeWire daemon or network.

use std::collections::HashMap;

use crate::airplay::core::device::{Device, DeviceId};
use crate::airplay::discovery::BrowseEvent;

/// What's known about a device that currently has a PipeWire sink.
#[derive(Debug, Clone)]
pub struct SinkEntry {
    /// Latest known device info (name/addresses/etc - kept fresh on
    /// BrowseEvent::Updated so reconnects always use a current IP).
    pub device: Device,
    /// PipeWire NODE_NAME this device's sink was created with. Kept stable
    /// for the sink's lifetime even if device.name changes later (renaming
    /// a live PipeWire node out from under Noctalia's picker would be more
    /// disruptive than a display name lagging by one rename).
    pub node_name: String,
}

/// What the PipeWire thread should do in response to a discovery event.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkAction {
    /// A new device was discovered - create a sink for it with this name.
    Create { device: Device, node_name: String },
    /// A known device's info changed (e.g. IP) - update the stored Device,
    /// no PipeWire object needs touching.
    UpdateDeviceInfo { id: DeviceId, device: Device },
    /// A known device disappeared - destroy its sink.
    Destroy { id: DeviceId },
    /// Nothing to do (e.g. Removed for a device we never had).
    NoOp,
}

/// Decide what sink action a BrowseEvent implies, given the current
/// registry state. Does not mutate `existing` - the caller applies the
/// resulting action (and updates its own bookkeeping) after actually
/// creating/destroying the PipeWire object.
pub fn plan_sink_action(
    existing: &HashMap<DeviceId, SinkEntry>,
    event: &BrowseEvent,
) -> SinkAction {
    match event {
        BrowseEvent::Added(device) => {
            if existing.contains_key(&device.id) {
                // Already have a sink (duplicate Added, or Added racing an
                // earlier Updated) - just refresh the stored info.
                SinkAction::UpdateDeviceInfo {
                    id: device.id.clone(),
                    device: device.clone(),
                }
            } else {
                let node_name = unique_node_name(existing, &device.name);
                SinkAction::Create {
                    device: device.clone(),
                    node_name,
                }
            }
        }
        BrowseEvent::Updated(device) => {
            if existing.contains_key(&device.id) {
                SinkAction::UpdateDeviceInfo {
                    id: device.id.clone(),
                    device: device.clone(),
                }
            } else {
                // Updated for a device we don't have a sink for yet -
                // treat like Added so it still gets one.
                let node_name = unique_node_name(existing, &device.name);
                SinkAction::Create {
                    device: device.clone(),
                    node_name,
                }
            }
        }
        BrowseEvent::Removed(id) => {
            if existing.contains_key(id) {
                SinkAction::Destroy { id: id.clone() }
            } else {
                SinkAction::NoOp
            }
        }
    }
}

/// Produce a PipeWire node name that doesn't collide with any currently
/// live sink's node_name. Two AirPlay devices can advertise the same
/// display name (e.g. two "Living Room" speakers on different floors) -
/// PipeWire node names should stay distinct so Noctalia's picker shows two
/// separate, individually selectable entries rather than one being hidden
/// or silently overwritten.
pub fn unique_node_name(existing: &HashMap<DeviceId, SinkEntry>, desired: &str) -> String {
    let taken: std::collections::HashSet<&str> =
        existing.values().map(|e| e.node_name.as_str()).collect();

    if !taken.contains(desired) {
        return desired.to_string();
    }

    let mut n = 2;
    loop {
        let candidate = format!("{desired} ({n})");
        if !taken.contains(candidate.as_str()) {
            return candidate;
        }
        n += 1;
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

    fn entry(device: Device, node_name: &str) -> SinkEntry {
        SinkEntry {
            device,
            node_name: node_name.to_string(),
        }
    }

    mod plan_sink_action {
        use super::*;

        #[test]
        fn added_for_unknown_device_creates() {
            let existing = HashMap::new();
            let device = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let action = plan_sink_action(&existing, &BrowseEvent::Added(device.clone()));
            assert_eq!(
                action,
                SinkAction::Create {
                    device,
                    node_name: "Living Room".to_string(),
                }
            );
        }

        #[test]
        fn added_for_known_device_updates_info_not_recreate() {
            let device = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let mut existing = HashMap::new();
            existing.insert(device.id.clone(), entry(device.clone(), "Living Room"));

            let action = plan_sink_action(&existing, &BrowseEvent::Added(device.clone()));
            assert_eq!(
                action,
                SinkAction::UpdateDeviceInfo {
                    id: device.id.clone(),
                    device,
                }
            );
        }

        #[test]
        fn updated_for_known_device_updates_info() {
            let device = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let mut existing = HashMap::new();
            existing.insert(device.id.clone(), entry(device.clone(), "Living Room"));

            let mut moved = device.clone();
            moved.addresses = vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 99))];

            let action = plan_sink_action(&existing, &BrowseEvent::Updated(moved.clone()));
            assert_eq!(
                action,
                SinkAction::UpdateDeviceInfo {
                    id: device.id.clone(),
                    device: moved,
                }
            );
        }

        #[test]
        fn updated_for_unknown_device_creates() {
            let existing = HashMap::new();
            let device = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let action = plan_sink_action(&existing, &BrowseEvent::Updated(device.clone()));
            assert_eq!(
                action,
                SinkAction::Create {
                    device,
                    node_name: "Living Room".to_string(),
                }
            );
        }

        #[test]
        fn removed_for_known_device_destroys() {
            let device = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let mut existing = HashMap::new();
            existing.insert(device.id.clone(), entry(device.clone(), "Living Room"));

            let action = plan_sink_action(&existing, &BrowseEvent::Removed(device.id.clone()));
            assert_eq!(action, SinkAction::Destroy { id: device.id });
        }

        #[test]
        fn removed_for_unknown_device_is_noop() {
            let existing = HashMap::new();
            let id = DeviceId([9, 9, 9, 9, 9, 9]);
            let action = plan_sink_action(&existing, &BrowseEvent::Removed(id));
            assert_eq!(action, SinkAction::NoOp);
        }

        #[test]
        fn create_for_duplicate_name_gets_unique_node_name() {
            let existing_device = make_device([1, 0, 0, 0, 0, 1], "Living Room");
            let mut existing = HashMap::new();
            existing.insert(existing_device.id.clone(), entry(existing_device, "Living Room"));

            let new_device = make_device([2, 0, 0, 0, 0, 2], "Living Room");
            let action = plan_sink_action(&existing, &BrowseEvent::Added(new_device.clone()));
            assert_eq!(
                action,
                SinkAction::Create {
                    device: new_device,
                    node_name: "Living Room (2)".to_string(),
                }
            );
        }
    }

    mod unique_node_name {
        use super::*;

        #[test]
        fn returns_desired_when_free() {
            let existing = HashMap::new();
            assert_eq!(unique_node_name(&existing, "Bedroom"), "Bedroom");
        }

        #[test]
        fn appends_suffix_on_collision() {
            let device = make_device([1, 0, 0, 0, 0, 1], "Bedroom");
            let mut existing = HashMap::new();
            existing.insert(device.id.clone(), entry(device, "Bedroom"));

            assert_eq!(unique_node_name(&existing, "Bedroom"), "Bedroom (2)");
        }

        #[test]
        fn skips_taken_suffixes() {
            let d1 = make_device([1, 0, 0, 0, 0, 1], "Bedroom");
            let d2 = make_device([2, 0, 0, 0, 0, 2], "Bedroom");
            let mut existing = HashMap::new();
            existing.insert(d1.id.clone(), entry(d1, "Bedroom"));
            existing.insert(d2.id.clone(), entry(d2, "Bedroom (2)"));

            assert_eq!(unique_node_name(&existing, "Bedroom"), "Bedroom (3)");
        }

        #[test]
        fn frees_name_when_original_removed() {
            let d2 = make_device([2, 0, 0, 0, 0, 2], "Bedroom");
            let mut existing = HashMap::new();
            // Only the "(2)" survivor remains - "Bedroom" itself is free again.
            existing.insert(d2.id.clone(), entry(d2, "Bedroom (2)"));

            assert_eq!(unique_node_name(&existing, "Bedroom"), "Bedroom");
        }
    }
}
