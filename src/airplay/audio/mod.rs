//! Audio encoding and RTP streaming for AirPlay 2.
//!
//! This module provides:
//! - Audio decoding from files (via symphonia) — used by the `play` diagnostic binary
//! - Live audio streaming from an external source (the PipeWire capture pipeline)
//! - ALAC encoding for realtime streaming
//! - RTP packet formatting and transmission
//! - Audio buffer management

pub mod cipher;
mod decoder;
mod encoder;
pub mod eq;
mod live_decoder;
mod resampler;
mod rtp;
mod buffer;
pub mod spatial;
mod streamer;
mod traits;

pub use decoder::{AudioDecoder, DecodedFrame};
pub use encoder::{AlacEncoder, AudioEncoder, EncodedPacket, create_encoder};
pub use eq::{EqConfig, EqParams, Equalizer};
pub use live_decoder::{LiveAudioDecoder, LiveFrameSender, LivePcmFrame};
pub use rtp::{RtpPacket, RtpSender, RtpReceiver, RtpHeader, RetransmitRequest, build_retransmit_response};
pub use buffer::{AudioBuffer, AudioFrame};
pub use spatial::{SpatialMixer, SpatialParams, SpatialMode, SpatialSnapshot, Position, SpeakerConfig, SpeakerParams};
pub use streamer::AudioStreamer;
pub use traits::{AudioSource, EncoderTrait};
