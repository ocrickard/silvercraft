//! PTP sessions. A [`Session`] always closes: `close()` or `Drop` sends
//! CloseSession (short timeout, errors ignored). Sessions were historically never closed; this drops
//! sessions, which is the likely cause of its "wedged session" recovery path
//!.

use std::io::Write;
use std::time::Duration;

use silvercraft_usb::Transport;

use crate::clock::Clock;
use crate::codec::{Response, op, rc};
use crate::connection::{ConnectionConfig, PtpConnection, TxnOutcome};
use crate::datasets::{DeviceInfo, ObjectInfo, parse_u32_list, usable_storage_ids};
use crate::{Event, PtpError};

/// Default session id (1).
pub const SESSION_ID: u32 = 1;
/// CloseSession timeout.
pub const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);
/// Settle time after a USB reset (0.75 s).
pub const RESET_SETTLE: Duration = Duration::from_millis(750);
/// Upper bound for small datasets collected in memory.
pub const DATASET_LIMIT: usize = 16 * 1024 * 1024;

/// An open PTP session.
#[derive(Debug)]
pub struct Session<T: Transport> {
    // `None` only after `close_into_transport` moved it out.
    conn: Option<PtpConnection<T>>,
    open: bool,
    session_id: u32,
}

impl<T: Transport> Session<T> {
    /// Drain the IN pipe, then OpenSession(1). On failure the transport is
    /// handed back so the caller can reset it.
    ///
    /// The response code is checked loosely: OK and SessionAlreadyOpen are
    /// success; anything else is logged and the session is treated as open,
    /// matching the established behaviour, which never checked it.
    pub fn open_or_return(transport: T, cfg: ConnectionConfig) -> Result<Self, (PtpError, T)> {
        let mut conn = PtpConnection::new(transport, cfg);
        conn.drain();
        conn.reset_txn();
        match conn.transaction(op::OPEN_SESSION, &[SESSION_ID], None, &mut std::io::sink()) {
            Ok(o) => {
                match o.response.code {
                    rc::OK => {}
                    rc::SESSION_ALREADY_OPEN => log::info!("OpenSession: session already open; reusing it"),
                    c => log::warn!("OpenSession answered 0x{c:04x}; continuing as before"),
                }
                Ok(Self { conn: Some(conn), open: true, session_id: SESSION_ID })
            }
            Err(e) => Err((e, conn.into_transport())),
        }
    }

    /// [`Session::open_or_return`] without the transport hand-back.
    pub fn open(transport: T, cfg: ConnectionConfig) -> Result<Self, PtpError> {
        Self::open_or_return(transport, cfg).map_err(|(e, _)| e)
    }

    /// Open with the original wedged-session recovery: if opening
    /// fails, USB-reset the device, wait 0.75 s, reconnect via `connect`
    /// (re-enumerate + claim) and try exactly once more.
    pub fn open_with_recovery<F, C>(mut connect: F, cfg: ConnectionConfig, clock: &C) -> Result<Self, PtpError>
    where
        F: FnMut() -> Result<T, PtpError>,
        C: Clock + ?Sized,
    {
        let t = connect()?;
        match Self::open_or_return(t, cfg.clone()) {
            Ok(s) => Ok(s),
            Err((e, mut t)) => {
                if e.is_fatal() {
                    return Err(e);
                }
                log::warn!("OpenSession failed ({e}); resetting the device and retrying once");
                if let Err(re) = t.reset() {
                    log::warn!("USB reset failed: {re}");
                }
                drop(t);
                clock.sleep(RESET_SETTLE);
                let t = connect()?;
                Self::open(t, cfg)
            }
        }
    }

    /// Whether CloseSession is still owed.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Session id.
    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// The underlying connection.
    pub fn connection(&mut self) -> Result<&mut PtpConnection<T>, PtpError> {
        self.conn.as_mut().ok_or(PtpError::NotOpen)
    }

    /// Run a transaction (see [`PtpConnection::transaction`]).
    pub fn transaction(&mut self, op: u16, params: &[u32], data_out: Option<&[u8]>, sink: &mut dyn Write) -> Result<TxnOutcome, PtpError> {
        if !self.open {
            return Err(PtpError::NotOpen);
        }
        self.connection()?.transaction(op, params, data_out, sink)
    }

    /// Run a transaction collecting the IN data phase (bounded) into memory.
    pub fn transaction_vec(&mut self, op: u16, params: &[u32], data_out: Option<&[u8]>) -> Result<(Response, Vec<u8>), PtpError> {
        self.transaction_vec_limited(op, params, data_out, DATASET_LIMIT)
    }

    /// [`Session::transaction_vec`] with an explicit size limit.
    pub fn transaction_vec_limited(
        &mut self,
        op: u16,
        params: &[u32],
        data_out: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Response, Vec<u8>), PtpError> {
        if !self.open {
            return Err(PtpError::NotOpen);
        }
        self.connection()?.transaction_vec(op, params, data_out, limit)
    }

    fn ok_vec(&mut self, op: u16, params: &[u32]) -> Result<Vec<u8>, PtpError> {
        let (r, data) = self.transaction_vec(op, params, None)?;
        if !r.is_ok() {
            return Err(PtpError::Response { op, code: r.code });
        }
        Ok(data)
    }

    /// GetDeviceInfo.
    pub fn device_info(&mut self) -> Result<DeviceInfo, PtpError> {
        DeviceInfo::parse(&self.ok_vec(op::GET_DEVICE_INFO, &[])?)
    }

    /// GetStorageIDs, filtered of 0 and 0xFFFFFFFF.
    pub fn storage_ids(&mut self) -> Result<Vec<u32>, PtpError> {
        Ok(usable_storage_ids(&self.ok_vec(op::GET_STORAGE_IDS, &[])?))
    }

    /// GetObjectHandles(storage, all formats, parent 0).
    pub fn object_handles(&mut self, storage: u32) -> Result<Vec<u32>, PtpError> {
        parse_u32_list(&self.ok_vec(op::GET_OBJECT_HANDLES, &[storage, 0, 0])?)
    }

    /// GetObjectInfo: the raw response plus the parsed dataset when the
    /// response is OK and the dataset parses.
    pub fn object_info(&mut self, handle: u32) -> Result<(Response, Option<ObjectInfo>), PtpError> {
        let (r, data) = self.transaction_vec(op::GET_OBJECT_INFO, &[handle], None)?;
        let info = if r.is_ok() { ObjectInfo::parse(&data).ok() } else { None };
        Ok((r, info))
    }

    /// GetObject streamed to `sink` (no size cap).
    pub fn get_object(&mut self, handle: u32, sink: &mut dyn Write) -> Result<TxnOutcome, PtpError> {
        self.transaction(op::GET_OBJECT, &[handle], None, sink)
    }

    /// Read one event from the interrupt endpoint.
    pub fn poll_event(&mut self, timeout: Duration) -> Result<Option<Event>, PtpError> {
        self.connection()?.poll_event(timeout)
    }

    fn send_close(&mut self) -> Result<(), PtpError> {
        if !self.open {
            return Ok(());
        }
        self.open = false;
        let conn = self.conn.as_mut().ok_or(PtpError::NotOpen)?;
        let saved = conn.config().timeout;
        conn.config_mut().timeout = CLOSE_TIMEOUT;
        let r = conn.transaction(op::CLOSE_SESSION, &[], None, &mut std::io::sink());
        conn.config_mut().timeout = saved;
        match r {
            Ok(o) if o.response.is_ok() => Ok(()),
            Ok(o) => Err(PtpError::Response { op: op::CLOSE_SESSION, code: o.response.code }),
            Err(e) => Err(e),
        }
    }

    /// Send CloseSession and return the transport (for a reset, say).
    pub fn close_into_transport(mut self) -> (Result<(), PtpError>, Option<T>) {
        let r = self.send_close();
        // `open` is now false, so Drop does nothing.
        (r, self.conn.take().map(PtpConnection::into_transport))
    }

    /// Send CloseSession. Errors are reported but the session is closed
    /// either way (Drop will not retry).
    pub fn close(mut self) -> Result<(), PtpError> {
        self.send_close()
    }
}

impl<T: Transport> Drop for Session<T> {
    fn drop(&mut self) {
        if self.open
            && let Err(e) = self.send_close()
        {
            log::debug!("CloseSession on drop failed: {e}");
        }
    }
}
