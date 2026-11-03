//! Factories that start workers on real hardware (or on simulators), so a UI never names driver
//! types.

use std::path::PathBuf;
use std::sync::Arc;

use silvercraft_epson::session::{Session, SessionConfig, V800};
use silvercraft_epson::{EpsonError, SimScanner};
use silvercraft_ptp::autocapture::AutoCaptureParams;

use crate::scanner::ScannerWorker;
use crate::tether::{FrameMeasure, TetherCamera, TetherWorker};

/// Starts a scanner worker writing previews to the work folder.
pub type ScannerFactory = Arc<dyn Fn(PathBuf, Arc<dyn Fn() + Send + Sync>) -> ScannerWorker + Send + Sync>;
/// Starts a tether worker.
pub type TetherFactory = Arc<dyn Fn(FrameMeasure, AutoCaptureParams, Arc<dyn Fn() + Send + Sync>) -> TetherWorker + Send + Sync>;

/// The first attached Epson V800/V850.
pub fn epson_scanner() -> ScannerFactory {
    Arc::new(|work_dir, wake| {
        ScannerWorker::spawn(
            || {
                let found = V800::discover()?;
                let first = found.first().ok_or_else(|| EpsonError::InvalidRequest("no Epson V800/V850 found (is it on and connected?)".into()))?;
                V800::open(first, SessionConfig::default())
            },
            work_dir,
            wake,
        )
    })
}

/// A simulated V800 (no hardware): for trying the workflow and for tests.
pub fn simulated_scanner(sim: SimScanner) -> ScannerFactory {
    Arc::new(move |work_dir, wake| {
        let opener = sim.opener();
        ScannerWorker::spawn(move || Ok(Session::with_opener(opener, SessionConfig::default())), work_dir, wake)
    })
}

/// The first attached Sony camera in PC-remote mode.
pub fn sony_camera() -> TetherFactory {
    Arc::new(|measure, params, wake| {
        TetherWorker::spawn(
            || {
                let found = silvercraft_ptp::sony::discover_sony_cameras().map_err(|e| e.to_string())?;
                let first = found.first().ok_or("no Sony camera found (USB mode \"PC Remote\", camera on?)")?;
                let (cam, _report) =
                    silvercraft_ptp::sony::connect_sony(first, silvercraft_ptp::sony::SonyOptions::default()).map_err(|e| e.to_string())?;
                Ok(Box::new(cam) as Box<dyn TetherCamera>)
            },
            measure,
            params,
            wake,
        )
    })
}
