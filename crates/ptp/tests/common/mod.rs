//! Shared helpers for the scripted PTP session tests.
#![allow(dead_code)]

use silvercraft_ptp::codec::{encode_command, encode_data, encode_event, encode_response, rc};
use silvercraft_usb::Step;

/// Builds a byte-exact script, tracking transaction ids like the driver
/// (first transaction = 1).
#[derive(Debug, Default)]
pub struct Script {
    pub steps: Vec<Step>,
    pub txn: u32,
}

pub fn response(code: u16, txn: u32) -> Vec<u8> {
    encode_response(code, txn, &[]).unwrap()
}

pub fn data(op: u16, txn: u32, payload: &[u8]) -> Vec<u8> {
    encode_data(op, txn, payload)
}

pub fn event(code: u16, params: &[u32]) -> Vec<u8> {
    encode_event(code, 0, params).unwrap()
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    fn cmd(&mut self, op: u16, params: &[u32]) -> u32 {
        self.txn += 1;
        self.steps.push(Step::Out(encode_command(op, self.txn, params).unwrap()));
        self.txn
    }

    /// Command with an IN data phase; the device sends data + response in
    /// one transfer (so the response rides along as overshoot).
    pub fn data_in(&mut self, op: u16, params: &[u32], payload: Option<&[u8]>, code: u16) -> &mut Self {
        let txn = self.cmd(op, params);
        let mut reply = Vec::new();
        if let Some(p) = payload {
            reply.extend_from_slice(&data(op, txn, p));
        }
        reply.extend_from_slice(&response(code, txn));
        self.steps.push(Step::In(reply));
        self
    }

    /// Like `data_in` but the reply is split into the given transfer sizes
    /// (the last chunk takes the rest).
    pub fn data_in_split(&mut self, op: u16, params: &[u32], payload: &[u8], code: u16, splits: &[usize]) -> &mut Self {
        let txn = self.cmd(op, params);
        let mut reply = data(op, txn, payload);
        reply.extend_from_slice(&response(code, txn));
        let mut rest = reply.as_slice();
        for &s in splits {
            let s = s.min(rest.len());
            self.steps.push(Step::In(rest[..s].to_vec()));
            rest = &rest[s..];
        }
        if !rest.is_empty() {
            self.steps.push(Step::In(rest.to_vec()));
        }
        self
    }

    /// Command with an OUT data phase.
    pub fn data_out(&mut self, op: u16, params: &[u32], out: &[u8], code: u16) -> &mut Self {
        let txn = self.cmd(op, params);
        self.steps.push(Step::Out(encode_data(op, txn, out)));
        self.steps.push(Step::In(response(code, txn)));
        self
    }

    /// Command with no data phase.
    pub fn no_data(&mut self, op: u16, params: &[u32], code: u16) -> &mut Self {
        self.data_in(op, params, None, code)
    }

    pub fn open_session(&mut self) -> &mut Self {
        self.no_data(0x1002, &[1], rc::OK)
    }

    pub fn close_session(&mut self) -> &mut Self {
        self.no_data(0x1003, &[], rc::OK)
    }

    /// The four-step Sony handshake.
    pub fn handshake(&mut self, ext_info: &[u8]) -> &mut Self {
        self.data_in(0x9201, &[1, 0, 0], Some(&[0; 8]), 0xA101)
            .data_in(0x9201, &[2, 0, 0], Some(&[0; 8]), 0xA101)
            .data_in(0x9202, &[0xC8], Some(ext_info), rc::OK)
            .data_in(0x9201, &[3, 0, 0], Some(&[0; 8]), 0xA101)
    }

    /// The shutter sequence (S1 down, S2 down, S2 up, S1 up).
    pub fn shutter(&mut self) -> &mut Self {
        self.data_out(0x9207, &[0xD2C1], &[2, 0], rc::OK)
            .data_out(0x9207, &[0xD2C2], &[2, 0], rc::OK)
            .data_out(0x9207, &[0xD2C2], &[1, 0], rc::OK)
            .data_out(0x9207, &[0xD2C1], &[1, 0], rc::OK)
    }

    /// 0x9209 answering an ObjectInMemory value (structured libgphoto2 layout).
    pub fn object_in_memory(&mut self, value: u16) -> &mut Self {
        let blob = oim_blob(value);
        self.data_in(0x9209, &[0xC8], Some(&blob), rc::OK)
    }

    pub fn raw(&mut self, step: Step) -> &mut Self {
        self.steps.push(step);
        self
    }

    pub fn build(&self) -> Vec<Step> {
        self.steps.clone()
    }
}

/// A plausible 0x9202 payload: version 200, two props, two controls.
pub fn ext_info() -> Vec<u8> {
    let mut b = 200u16.to_le_bytes().to_vec();
    b.extend_from_slice(&2u32.to_le_bytes());
    b.extend_from_slice(&0xD215u16.to_le_bytes());
    b.extend_from_slice(&0x5007u16.to_le_bytes());
    b.extend_from_slice(&2u32.to_le_bytes());
    b.extend_from_slice(&0xD2C1u16.to_le_bytes());
    b.extend_from_slice(&0xD2C2u16.to_le_bytes());
    b
}

/// A 0x9209 blob with an F-number descriptor and ObjectInMemory.
pub fn oim_blob(value: u16) -> Vec<u8> {
    let mut b = 2u64.to_le_bytes().to_vec();
    // F-number: u16, range form.
    b.extend_from_slice(&0x5007u16.to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&[1, 1]);
    b.extend_from_slice(&280u16.to_le_bytes());
    b.extend_from_slice(&560u16.to_le_bytes());
    b.push(1);
    for v in [140u16, 2200, 10] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    // ObjectInMemory: u16, no form.
    b.extend_from_slice(&0xD215u16.to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&[0, 1]);
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&value.to_le_bytes());
    b.push(0);
    b
}

fn ptp_string(s: &str) -> Vec<u8> {
    silvercraft_ptp::codec::encode_ptp_string(s)
}

/// An ObjectInfo dataset.
pub fn object_info(format: u16, size: u32, name: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0x0001_0001u32.to_le_bytes());
    b.extend_from_slice(&format.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&size.to_le_bytes());
    b.extend_from_slice(&0u16.to_le_bytes());
    for v in [0u32, 0, 0, 8640, 5760, 14, 0] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&ptp_string(name));
    b.extend_from_slice(&ptp_string(""));
    b
}

/// A u32-counted u32 list.
pub fn u32_list(v: &[u32]) -> Vec<u8> {
    let mut b = (v.len() as u32).to_le_bytes().to_vec();
    for x in v {
        b.extend_from_slice(&x.to_le_bytes());
    }
    b
}

/// A fake ARW: TIFF magic then a deterministic pattern.
pub fn fake_arw(len: usize, seed: u8) -> Vec<u8> {
    let mut v: Vec<u8> = (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect();
    v[..4].copy_from_slice(b"II*\0");
    v
}
