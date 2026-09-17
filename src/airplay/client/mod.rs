//! High-level AirPlay 2 sender client.
//!
//! This module provides:
//! - Simple API for discovering and connecting to AirPlay receivers
//! - Audio streaming from files or raw PCM
//! - Volume and playback control

mod client;
mod connection;
mod group;
mod playback;
mod builder;
mod events;
mod stats;

pub use client::AirPlayClient;
pub use connection::{Connection, StreamingParams};
pub use group::{DeviceGroup, GroupMember};
pub use playback::{PlaybackState, PlaybackInfo};
pub use builder::ClientBuilder;
pub use events::{ClientEvent, EventHandler, NoOpHandler, CallbackHandler};
pub use stats::{StreamStats, StatsSnapshot, DeviceStatsSnapshot};

// Re-export commonly used types
pub use crate::airplay::core::{Device, DeviceId, AudioFormat, StreamConfig, Error, Result};
pub use crate::airplay::discovery::Discovery;

// Live streaming types for the PipeWire capture pipeline.
pub use crate::airplay::audio::{LiveAudioDecoder, LiveFrameSender, LivePcmFrame};

// Equalizer and spatial-audio types: kept vendored (unused by this
// project's single-device live-streaming path, but interleaved into
// Connection/AudioStreamer's struct fields and hot-path match arms deeply
// enough that surgically removing them risked introducing a bug in code
// proven to work against a real HomePod) — see also the PTP stub in
// crate::airplay::timing for the same tradeoff.
pub use crate::airplay::audio::{EqConfig, EqParams};
pub use crate::airplay::audio::eq::MAX_GAIN_DB;
pub use crate::airplay::audio::{SpatialMixer, SpatialParams, SpatialMode, SpatialSnapshot, Position, SpeakerConfig, SpeakerParams};

// Timing types needed by Connection.
pub use crate::airplay::timing::ClockOffset;
