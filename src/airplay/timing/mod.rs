//! NTP timing synchronization for AirPlay 2.
//!
//! This module provides:
//! - NTP-like timing server (responds to the receiver's timing requests)
//! - Clock offset calculation
//! - RTP timestamp correlation
//!
//! PTP (IEEE 1588) timing is not implemented — this sender always uses NTP
//! timing (`StreamConfig::realtime_ntp()`), which is what HomePods accept
//! for a single-device AirPlay 2 stream. PTP existed upstream only to back
//! multi-room/stereo-pair group streaming, which this project doesn't use
//! and has removed (see `Connection` — it no longer has PTP-master fields).

mod ntp;
mod clock;

pub use ntp::NtpTimingServer;
pub use clock::{Clock, TimestampPair, ClockOffset, NTP_EPOCH_OFFSET, unix_to_ntp, ntp_to_unix};
