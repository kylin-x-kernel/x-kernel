// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Message builder and decoding helpers for 9P packets.

use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use kerrno::{KError, KResult};

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

pub(crate) fn read_u8(buf: &[u8], offset: &mut usize) -> KResult<u8> {
    Ok(read_exact(buf, offset, 1)?[0])
}

pub(crate) fn read_u16(buf: &[u8], offset: &mut usize) -> KResult<u16> {
    let bytes = read_exact(buf, offset, 2)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

pub(crate) fn read_u32(buf: &[u8], offset: &mut usize) -> KResult<u32> {
    let bytes = read_exact(buf, offset, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

pub(crate) fn read_u64(buf: &[u8], offset: &mut usize) -> KResult<u64> {
    let bytes = read_exact(buf, offset, 8)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

pub(crate) fn read_str(buf: &[u8], offset: &mut usize) -> KResult<String> {
    let len = read_u16(buf, offset)? as usize;
    let value =
        core::str::from_utf8(read_exact(buf, offset, len)?).map_err(|_| KError::InvalidData)?;
    Ok(value.to_string())
}

fn read_exact<'a>(buf: &'a [u8], offset: &mut usize, len: usize) -> KResult<&'a [u8]> {
    let end = offset.checked_add(len).ok_or(KError::InvalidData)?;
    let bytes = buf.get(*offset..end).ok_or(KError::InvalidData)?;
    *offset = end;
    Ok(bytes)
}

pub(crate) fn read_qid(buf: &[u8], offset: &mut usize) -> KResult<Qid> {
    let type_ = read_u8(buf, offset)?;
    let version = read_u32(buf, offset)?;
    let path = read_u64(buf, offset)?;
    Ok(Qid {
        type_,
        version,
        path,
    })
}
