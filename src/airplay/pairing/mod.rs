//! HomeKit authentication for AirPlay 2.
//!
//! This module implements:
//! - HomeKit pair-setup (SRP-6a based) — use [`PairSetup`]
//! - HomeKit pair-verify (Curve25519 + Ed25519) — use [`PairVerify`]
//! - Transient pairing (no persistent storage) — use [`PairingSession`]
//!
//! HomePods use [`PairSetup::new_transient()`] with HKP=4 (HomeKit Transient).

mod controller;
mod pair_setup;
mod pair_verify;
mod session;
mod traits;

pub use controller::ControllerIdentity;
pub use pair_setup::{PairSetup, TransientPairSetup};
pub use pair_verify::PairVerify;
pub use session::{PairingSession, PairingStep};
pub use traits::{PairingHandler, Transport};
