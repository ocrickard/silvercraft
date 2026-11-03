//! SilverCraft Epson Perfection V800/V850 driver (layer 1).
//!
//! The hardware-qualified *production* paths only: the origin bed prescan,
//! the streamed visible scan, and the registered visible + infrared dual pass,
//! with wide-scan tiling. Legacy tiled scan paths, standalone/diagnostic IR
//! paths and native probes lock the firmware and are deliberately absent.
//!
//! ```text
//! V800::discover() → V800::open(&found, cfg) → Session
//!   session.status() / session.recover()
//!   session.prescan(..)       origin (0,0) scan: calibrates the carriage → Preview
//!   plan_frames(&preview, ..) per-frame regions (pure)
//!   session.scan_visible(..)  → Scan      (RGB16)
//!   session.scan_dual_ir(..)  → DualScan  (RGB16 + registered IR16)
//! ```
//!
//! # Hardware safety
//! The firmware hard-hangs (power cycle) on several host mistakes. The driver makes each one
//! impossible to express or refuses it before any byte reaches the device:
//!
//! | Wedge | Prevention |
//! |---|---|
//! | Area beyond 8×10 in | [`Region::new`] refuses it; an invalid `Region` cannot exist |
//! | Targeted FS G on an uncalibrated carriage | sessions refuse ([`EpsonError::NotCalibrated`]) or auto-run the origin prescan; calibration is keyed by device *enumeration* (lost on power cycle/replug) and invalidated on any FS G failure or disconnect; never assumed |
//! | IR FS G not armed | IR needs an `IrArmed` token minted only from a `CompleteRaster` proof (type-level) |
//! | Host timeout during the silent cold start | FS G reads use ≥30 s and ignore cancel until the reply |
//! | Mid-command failure without CAN | RAII guard: drain + CAN on every error path, including panics |
//! | Undrained chunk trailer / remainder | every chunk is read as `n + 1` bytes |
//! | CAN back-pressured by a queued chunk | drain before CAN |
//! | Abort flag still set during cleanup | cleanup I/O ignores the cancel token |
//! | Hard kill mid-transfer | cooperative [`CancelToken`] only |
//! | Resending a timed-out write's prefix | counted writes resume from the remainder |
//! | Repeated partial native tails | escalated to [`EpsonError::PowerCycleRequired`] |
//! | FS W width > 4096 px (NAK) | auto-tiling; widths above 4096 never sent |
//!
//! Windows (usbscan.sys) is not supported: [`V800::open`] returns [`EpsonError::Unsupported`].
//! The Windows de-framing heuristics (§3.9) are not ported; they damage the clean libusb/nusb
//! stream.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod cancel;
pub mod clock;
pub mod codec;
pub mod error;
pub mod handle;
pub mod io;
pub mod opener;
pub mod plan;
pub mod raster;
pub mod recovery;
pub mod session;
pub mod sim;

pub use cancel::CancelToken;
pub use clock::{Clock, SystemClock, VirtualClock};
pub use codec::{DeviceArea, Dpi, PassKind, Region, Status, area_pixels};
pub use error::{EpsonError, POWER_CYCLE_MESSAGE};
pub use opener::{CloneOpener, FoundScanner, HandleOpener, NusbOpener};
pub use plan::{FrameBox, FramePlan, FramePlanOptions, PixelRect, exposed_box_in_capture, plan_frames, plan_frames_px};
pub use raster::{GrayImage, RgbImage, Seam};
pub use recovery::{PostflightGate, RecoveryClass, RecoveryOptions, RecoveryReport, RecoveryStep};
pub use session::{
    Calibration, DualScan, Pass, PassMeta, Phase, PrescanOptions, Preview, Registration, Scan, ScanRequest, Session, SessionConfig, StatusReport,
    V800, plan_tiles,
};
pub use sim::{SimConfig, SimFault, SimScanner};
