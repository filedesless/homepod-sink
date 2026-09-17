//! PTP (IEEE 1588) timing is not implemented.
//!
//! This sender always uses `StreamConfig::realtime_ntp()`, which sets
//! `TimingProtocol::Ntp` — HomePods accept NTP timing for a single-device
//! AirPlay 2 stream. `Connection` (client/connection.rs) still has a
//! `TimingProtocol::Ptp` code path (for multi-room group streaming, which
//! this project doesn't use) that references the types below only to
//! type-check; none of it runs under `TimingProtocol::Ntp`, and
//! `Connection::ptp_master` is never actually constructed even in upstream
//! airplay2-rs. These are minimal stand-ins so that dead branch still
//! compiles, without vendoring the ~2500-line real PTP implementation
//! (Sync/Announce/BMCA/Delay_Req-Resp) that would back it.

use crate::airplay::core::error::Result;
use super::ClockOffset;
use std::net::IpAddr;

/// UDP port for PTP event messages (Sync, Delay_Req). Unused — no PTP
/// socket is ever opened — kept only because `Connection` references the
/// constant in its (dead, NTP-only-in-practice) PTP code path.
pub const PTP_EVENT_PORT: u16 = 319;

/// Stand-in for a PTP grandmaster. Never actually constructed (see module
/// docs) — `Connection::ptp_master` stays `None` for the lifetime of a
/// real connection.
pub struct PtpMaster;

impl PtpMaster {
    pub async fn stop(&mut self) {}
}

/// Unimplemented: PTP slave sync. Only reachable if a caller explicitly
/// selects `StreamConfig::airplay2_buffered()` / `TimingProtocol::Ptp`,
/// which this project never does.
pub async fn run_ptp_slave(
    _master_addr: IpAddr,
    _offset_tx: tokio::sync::watch::Sender<ClockOffset>,
) -> Result<()> {
    Err(crate::airplay::core::error::Error::Rtsp(
        crate::airplay::core::error::RtspError::SetupFailed(
            "PTP timing is not implemented in this build (NTP-only)".into(),
        ),
    ))
}

/// Unimplemented: BMCA yield flow (Mac-style gPTP negotiation). See
/// `run_ptp_slave`.
pub async fn run_bmca_yield_flow(
    _master_addr: IpAddr,
    _priority1: u8,
    _offset_tx: tokio::sync::watch::Sender<ClockOffset>,
    _clock_id_tx: tokio::sync::oneshot::Sender<[u8; 8]>,
) -> Result<()> {
    Err(crate::airplay::core::error::Error::Rtsp(
        crate::airplay::core::error::RtspError::SetupFailed(
            "PTP timing is not implemented in this build (NTP-only)".into(),
        ),
    ))
}

/// Unimplemented: PTP group-master flow (multi-room). See `run_ptp_slave`.
pub async fn run_ptp_group_master_flow(
    _peer_ips: Vec<IpAddr>,
    _priority1: u8,
    _clock_id_tx: tokio::sync::oneshot::Sender<[u8; 8]>,
) -> Result<()> {
    Err(crate::airplay::core::error::Error::Rtsp(
        crate::airplay::core::error::RtspError::SetupFailed(
            "PTP timing is not implemented in this build (NTP-only)".into(),
        ),
    ))
}
