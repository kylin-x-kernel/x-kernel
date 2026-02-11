//! Message builder and decoding helpers for 9P packets.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::protocol::Qid;

/// 9P message encoder with size prefix.
pub(crate) struct Message {
    buf: Vec<u8>,
}

impl Message {
    pub(crate) fn new(msg_type: u8, tag: u16) -> Self {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&[0, 0, 0, 0]);
        buf.push(msg_type);
        buf.extend_from_slice(&tag.to_le_bytes());
        Self { buf }
    }

    pub(crate) fn push_u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    pub(crate) fn push_u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn push_u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn push_u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn push_str(&mut self, value: &str) {
        let bytes = value.as_bytes();
        let len = bytes.len() as u16;
        self.push_u16(len);
        self.buf.extend_from_slice(bytes);
    }

    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        let size = self.buf.len() as u32;
        self.buf[0..4].copy_from_slice(&size.to_le_bytes());
        self.buf
    }
}

pub(crate) fn read_u8(buf: &[u8], offset: &mut usize) -> Result<u8, String> {
    if *offset + 1 > buf.len() {
        return Err(String::from("short buffer"));
    }
    let value = buf[*offset];
    *offset += 1;
    Ok(value)
}

pub(crate) fn read_u16(buf: &[u8], offset: &mut usize) -> Result<u16, String> {
    if *offset + 2 > buf.len() {
        return Err(String::from("short buffer"));
    }
    let value = u16::from_le_bytes([buf[*offset], buf[*offset + 1]]);
    *offset += 2;
    Ok(value)
}

pub(crate) fn read_u32(buf: &[u8], offset: &mut usize) -> Result<u32, String> {
    if *offset + 4 > buf.len() {
        return Err(String::from("short buffer"));
    }
    let value = u32::from_le_bytes([
        buf[*offset],
        buf[*offset + 1],
        buf[*offset + 2],
        buf[*offset + 3],
    ]);
    *offset += 4;
    Ok(value)
}

pub(crate) fn read_u64(buf: &[u8], offset: &mut usize) -> Result<u64, String> {
    if *offset + 8 > buf.len() {
        return Err(String::from("short buffer"));
    }
    let value = u64::from_le_bytes([
        buf[*offset],
        buf[*offset + 1],
        buf[*offset + 2],
        buf[*offset + 3],
        buf[*offset + 4],
        buf[*offset + 5],
        buf[*offset + 6],
        buf[*offset + 7],
    ]);
    *offset += 8;
    Ok(value)
}

pub(crate) fn read_str(buf: &[u8], offset: &mut usize) -> Result<String, String> {
    let len = read_u16(buf, offset)? as usize;
    if *offset + len > buf.len() {
        return Err(String::from("short buffer"));
    }
    let value = core::str::from_utf8(&buf[*offset..*offset + len])
        .map_err(|_| String::from("invalid utf8"))?;
    *offset += len;
    Ok(value.to_string())
}

pub(crate) fn read_qid(buf: &[u8], offset: &mut usize) -> Result<Qid, String> {
    let type_ = read_u8(buf, offset)?;
    let version = read_u32(buf, offset)?;
    let path = read_u64(buf, offset)?;
    Ok(Qid {
        type_,
        _version: version,
        _path: path,
    })
}

pub(crate) fn dump_hex(buf: &[u8]) -> String {
    let mut out = String::new();
    for (idx, byte) in buf.iter().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        out.push_str(&format!("{:02x}", byte));
    }
    out
}
