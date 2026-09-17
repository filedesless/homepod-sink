//! Vendored AirPlay 2 sender implementation.
//!
//! This is the subset of the (unofficial, reverse-engineered) AirPlay 2
//! protocol that this project's two binaries actually need: mDNS discovery,
//! HomeKit transient pairing, RTSP session setup, NTP timing, ChaCha20 RTP
//! audio encryption, and ALAC encoding — enough to open a live audio stream
//! to a HomePod (`main.rs`) or play a local file to one for diagnostics
//! (`bin/play.rs`).
//!
//! Originally vendored from a local fork of airplay2-rs
//! (github.com/filedesless/airplay2-rs), trimmed of code unreachable from
//! that usage: AirPlay 1/RAOP, PTP timing (multi-room only), AAC encoding
//! (buffered-mode only), FairPlay DRM, Bluetooth capture, and the TUI.
//! Equalizer and spatial-audio support are still present but unused (see
//! the doc comment on their re-exports in `client::mod`) — cutting them
//! required editing deeply inside `AudioStreamer`'s and `Connection`'s
//! working hot-path logic, which wasn't worth the risk for a project that
//! doesn't use either feature.

pub mod core;
pub mod discovery;
pub mod crypto;
pub mod pairing;
pub mod rtsp;
pub mod timing;
pub mod audio;
pub mod client;
