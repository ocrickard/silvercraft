//! SilverCraft PTP (layer 1): the PIMA 15740 container codec, sessions that
//! always close, the Sony SDIO extension (handshake, properties, shutter,
//! capture, live view) and the auto-capture state machine.
//!
//! Statements marked **(EXT, unverified)** come from public
//! Sony/libgphoto2 knowledge rather than hardware-validated
//! behaviour, and must be checked during hardware bring-up.
//!
//! Live bring-up path:
//! [`sony::discover_sony_cameras`] → [`sony::connect_sony`] (claim, OpenSession
//! with reset-retry-once, SDIO handshake) → [`sony::SonyCamera::live_view_frame`]
//! → [`sony::SonyCamera::capture`] (streams the file to a `Write`) → drop or
//! [`sony::SonyCamera::close`] (CloseSession).

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod autocapture;
pub mod clock;
pub mod codec;
pub mod connection;
pub mod datasets;
pub mod error;
pub mod session;
pub mod sony;

pub use clock::{Clock, FakeClock, SystemClock};
pub use codec::{ContainerKind, Event, Header, Response};
pub use connection::{ConnectionConfig, PtpConnection, TxnOutcome};
pub use datasets::{DeviceInfo, ObjectInfo};
pub use error::PtpError;
pub use session::Session;
