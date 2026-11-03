//! SilverCraft USB transport (layer 0).
//!
//! A deliberately small abstraction over bulk and interrupt USB I/O:
//!
//! - [`Transport`]: the trait protocol layers (PTP, the Epson driver) talk to.
//! - [`device`]: the real implementation over the pure-Rust `nusb` crate, plus
//!   discovery by vendor id / product id / interface class.
//! - [`mock`]: a scripted fake (expected writes, canned replies, asserted
//!   exchanged bytes) for tests.
//! - [`record`]: a wrapper that logs traffic to a text file, and a parser that
//!   turns such a log back into a [`mock`] script for fixtures.
//!
//! Everything in this crate treats the device as hostile: no reply can make
//! it panic.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod device;
pub mod error;
pub mod mock;
pub mod record;
pub mod transport;

pub use device::{
    DeviceFilter, EndpointDesc, EndpointKind, FoundDevice, InterfaceDesc, NusbTransport, OpenOptions, PipeSelection, discover, select_pipes,
};
pub use error::UsbError;
pub use mock::{MockEvent, ScriptedTransport, Step};
pub use record::{RecordingTransport, parse_recording};
pub use transport::{Pipe, Transport, round_up_to_packet};
