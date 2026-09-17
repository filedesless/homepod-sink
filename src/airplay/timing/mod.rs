//! NTP timing synchronization for AirPlay 2.
//!
//! This module provides:
//! - NTP-like timing server (responds to the receiver's timing requests)
//! - Clock offset calculation
//! - RTP timestamp correlation
//!
//! PTP (IEEE 1588) timing is not implemented — this sender always uses NTP
//! timing (`StreamConfig::realtime_ntp()`), which is what HomePods accept
//! for a single-device AirPlay 2 stream.

mod ntp;
mod clock;
mod ptp_stub;

pub use ntp::NtpTimingServer;
pub use clock::{Clock, TimestampPair, ClockOffset, NTP_EPOCH_OFFSET, unix_to_ntp, ntp_to_unix};
pub use ptp_stub::{PtpMaster, PTP_EVENT_PORT, run_ptp_slave, run_bmca_yield_flow, run_ptp_group_master_flow};
