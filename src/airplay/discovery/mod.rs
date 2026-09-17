//! mDNS/Bonjour service discovery for AirPlay 2 devices.
//!
//! This module provides:
//! - Async device discovery via mDNS
//! - TXT record parsing for device capabilities

mod browser;
mod parser;
mod traits;

pub use browser::ServiceBrowser;
pub use parser::TxtRecordParser;
pub use traits::{BrowseEvent, Discovery};

/// AirPlay 2 service type for mDNS discovery.
pub const AIRPLAY_SERVICE_TYPE: &str = "_airplay._tcp.local.";

/// RAOP (Remote Audio Output Protocol) service type for legacy AirPlay discovery.
pub const RAOP_SERVICE_TYPE: &str = "_raop._tcp.local.";
