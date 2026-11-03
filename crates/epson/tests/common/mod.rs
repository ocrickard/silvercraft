//! Shared helpers for the scripted wire tests.
#![allow(dead_code)]

use std::sync::Arc;

use silvercraft_epson::codec::{self, LutChannel};
use silvercraft_epson::{CloneOpener, Phase, Session, SessionConfig, VirtualClock};
use silvercraft_usb::{ScriptedTransport, Step};

pub const BUSY: [u8; 14] = [0x02, 0x92, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
pub const IDLE: [u8; 4] = [0x02, 0x12, 0x00, 0x00];

pub fn ready(unit: u32, full: u32, rem: u32) -> Vec<u8> {
    let mut v = vec![0x02, 0x12];
    v.extend(unit.to_le_bytes());
    v.extend(full.to_le_bytes());
    v.extend(rem.to_le_bytes());
    v
}

/// A byte-exact script builder.
#[derive(Default)]
pub struct Wire {
    pub steps: Vec<Step>,
}

impl Wire {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn out(&mut self, b: &[u8]) -> &mut Self {
        self.steps.push(Step::Out(b.to_vec()));
        self
    }
    pub fn inn(&mut self, b: &[u8]) -> &mut Self {
        self.steps.push(Step::In(b.to_vec()));
        self
    }
    pub fn ack(&mut self, b: &[u8]) -> &mut Self {
        self.out(b).inn(&[0x06])
    }
    /// The preflight status probe.
    pub fn status_probe(&mut self) -> &mut Self {
        self.out(&[0x1B, 0x46]).inn(&IDLE)
    }
    /// One preview priming cycle (§3.2).
    pub fn priming(&mut self) -> &mut Self {
        self.out(&[0x1B, 0x40]).inn(&[0x06]);
        self.out(&[0x1C, 0x46]).inn(&[0u8; 16]);
        self.out(&[0x1C, 0x49]).inn(&[0u8; 80]);
        let mut esc_i = vec![0x02, 0x12, 0x6A, 0x00];
        esc_i.extend([0u8; 106]);
        self.out(&[0x1B, 0x49]).inn(&esc_i)
    }
    pub fn priming3(&mut self) -> &mut Self {
        self.priming().priming().priming()
    }
    /// Scan-handle setup through ESC Z (§3.3), with the exact LUT and parameter bytes.
    pub fn setup(&mut self, luts: &[LutChannel], params: &[u8; 64], esc_f: &[u8]) -> &mut Self {
        for d in [0x08, 0x10, 0x08] {
            self.ack(&[0x1B, 0x44]).ack(&[d]);
        }
        self.out(&[0x1C, 0x46]).inn(&[0u8; 16]);
        self.out(&[0x1B, 0x66]).inn(esc_f);
        self.ack(&[0x1B, 0x40]);
        self.out(&[0x1B, 0x46]).inn(&IDLE);
        for l in luts {
            self.ack(&[0x1B, 0x7A]).ack(&codec::identity_lut(*l));
        }
        self.ack(&[0x1C, 0x57]).ack(params);
        self.ack(&[0x1B, 0x5A]).ack(&[0x03])
    }
    pub fn fs_g(&mut self, reply: &[u8]) -> &mut Self {
        self.out(&[0x1C, 0x47]).inn(reply)
    }
    /// Raster chunks (each already carrying its trailer); ACK between, not after the last.
    pub fn chunks(&mut self, chunks: &[Vec<u8>]) -> &mut Self {
        for (i, c) in chunks.iter().enumerate() {
            self.inn(c);
            if i + 1 != chunks.len() {
                self.out(&[0x06]);
            }
        }
        self
    }
    pub fn postflight(&mut self) -> &mut Self {
        self.out(&[0x1B, 0x46]).inn(&IDLE)
    }
    pub fn transport(&self) -> ScriptedTransport {
        ScriptedTransport::new(self.steps.clone())
    }
}

pub fn session(t: &ScriptedTransport, cfg: SessionConfig) -> Session<CloneOpener<ScriptedTransport>> {
    Session::with_transport_and_clock(t.clone(), Arc::new(VirtualClock::new()), cfg)
}

pub fn noop() -> impl FnMut(Phase) {
    |_| {}
}

/// the original `_tok` normalisation.
pub fn tok(w: &[u8]) -> String {
    match w.len() {
        257 => format!("lut:{:02x}", w[0]),
        64 => "param64".into(),
        32 => "hs32".into(),
        _ => w.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "),
    }
}

pub fn toks(ws: &[Vec<u8>]) -> Vec<String> {
    ws.iter().map(|w| tok(w)).collect()
}

/// 8x3 BIL raster with samples 1..=72 (u16le) — T:5539 fixture.
pub fn bil_8x3() -> Vec<u8> {
    (1u16..=72).flat_map(|s| s.to_le_bytes()).collect()
}

pub fn with_trailer(mut b: Vec<u8>, t: u8) -> Vec<u8> {
    b.push(t);
    b
}
