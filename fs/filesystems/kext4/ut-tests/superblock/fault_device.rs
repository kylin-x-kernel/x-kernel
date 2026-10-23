// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Persistent versus buffered fault models shared by component recovery tests.
use alloc::vec::Vec;

use block::{BlockDeviceOperations, Device, DeviceKind, DriverError, DriverResult};

use crate::sync::Mutex;
const DEVICE_BLOCK_SIZE: usize = 512;

pub(crate) struct WriteThroughFaultDevice {
    bytes: Mutex<Vec<u8>>,
    fail_write_at: Option<usize>,
    write_count: Mutex<usize>,
}

pub(crate) struct BufferedFaultDevice {
    committed: Mutex<Vec<u8>>,
    pending: Mutex<Vec<(usize, Vec<u8>)>>,
    fail_write_at: Option<usize>,
    fail_flush_at: Option<usize>,
    write_count: Mutex<usize>,
    flush_count: Mutex<usize>,
}

impl WriteThroughFaultDevice {
    pub(crate) fn fail_write_at(bytes: Vec<u8>, write: usize) -> Self {
        Self {
            bytes: Mutex::new(bytes),
            fail_write_at: Some(write),
            write_count: Mutex::new(0),
        }
    }

    pub(crate) fn committed_bytes(&self) -> Vec<u8> {
        self.bytes.lock().clone()
    }
}

impl BufferedFaultDevice {
    pub(crate) fn fail_write_at(bytes: Vec<u8>, write: usize) -> Self {
        Self::new(bytes, Some(write), None)
    }

    pub(crate) fn fail_flush_at(bytes: Vec<u8>, flush: usize) -> Self {
        Self::new(bytes, None, Some(flush))
    }

    pub(crate) fn new(
        bytes: Vec<u8>,
        fail_write_at: Option<usize>,
        fail_flush_at: Option<usize>,
    ) -> Self {
        Self {
            committed: Mutex::new(bytes),
            pending: Mutex::new(Vec::new()),
            fail_write_at,
            fail_flush_at,
            write_count: Mutex::new(0),
            flush_count: Mutex::new(0),
        }
    }

    pub(crate) fn committed_bytes(&self) -> Vec<u8> {
        self.committed.lock().clone()
    }
}

impl Device for WriteThroughFaultDevice {
    fn name(&self) -> &str {
        "kext4-write-through-faulty-test-image"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for WriteThroughFaultDevice {
    fn num_blocks(&self) -> u64 {
        (self.bytes.lock().len() / DEVICE_BLOCK_SIZE) as u64
    }

    fn block_size(&self) -> usize {
        DEVICE_BLOCK_SIZE
    }
}
impl WriteThroughFaultDevice {
    fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
        let start = block_start(block_id)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        output.copy_from_slice(
            self.bytes
                .lock()
                .get(start..end)
                .ok_or(DriverError::InvalidInput)?,
        );
        Ok(())
    }

    fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
        let mut write_count = self.write_count.lock();
        *write_count += 1;
        if self.fail_write_at == Some(*write_count) {
            return Err(DriverError::Io);
        }

        let start = block_start(block_id)?;
        let end = start
            .checked_add(input.len())
            .ok_or(DriverError::InvalidInput)?;
        self.bytes
            .lock()
            .get_mut(start..end)
            .ok_or(DriverError::InvalidInput)?
            .copy_from_slice(input);
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

impl block::BioOperations for WriteThroughFaultDevice {
    fn submit_bio(&self, mut bio: block::Bio) {
        let block_id = bio.sector() / (self.block_size() as u64 / 512);
        let result = match bio.operation() {
            block::BioOperation::Read => {
                bio.read_contiguous(|bytes| self.read_block(block_id, bytes))
            }
            block::BioOperation::Write => {
                bio.write_contiguous(|bytes| self.write_block(block_id, bytes))
            }
            block::BioOperation::Flush => self.flush(),
        };
        bio.end_io(result);
    }
}

impl Device for BufferedFaultDevice {
    fn name(&self) -> &str {
        "kext4-faulty-buffered-test-image"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for BufferedFaultDevice {
    fn num_blocks(&self) -> u64 {
        (self.committed.lock().len() / DEVICE_BLOCK_SIZE) as u64
    }

    fn block_size(&self) -> usize {
        DEVICE_BLOCK_SIZE
    }
}
impl BufferedFaultDevice {
    fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
        let start = block_start(block_id)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        output.copy_from_slice(
            self.committed
                .lock()
                .get(start..end)
                .ok_or(DriverError::InvalidInput)?,
        );

        for (pending_start, pending_bytes) in self.pending.lock().iter() {
            let pending_end = pending_start
                .checked_add(pending_bytes.len())
                .ok_or(DriverError::InvalidInput)?;
            let overlap_start = start.max(*pending_start);
            let overlap_end = end.min(pending_end);
            if overlap_start < overlap_end {
                let output_start = overlap_start - start;
                let pending_offset = overlap_start - pending_start;
                let len = overlap_end - overlap_start;
                output[output_start..output_start + len]
                    .copy_from_slice(&pending_bytes[pending_offset..pending_offset + len]);
            }
        }
        Ok(())
    }

    fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
        let mut write_count = self.write_count.lock();
        *write_count += 1;
        if self.fail_write_at == Some(*write_count) {
            return Err(DriverError::Io);
        }

        let start = block_start(block_id)?;
        let end = start
            .checked_add(input.len())
            .ok_or(DriverError::InvalidInput)?;
        if end > self.committed.lock().len() {
            return Err(DriverError::InvalidInput);
        }
        self.pending.lock().push((start, input.to_vec()));
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        let mut flush_count = self.flush_count.lock();
        *flush_count += 1;
        if self.fail_flush_at == Some(*flush_count) {
            self.pending.lock().clear();
            return Err(DriverError::Io);
        }

        let mut committed = self.committed.lock();
        let mut pending = self.pending.lock();
        for (start, bytes) in pending.drain(..) {
            let end = start
                .checked_add(bytes.len())
                .ok_or(DriverError::InvalidInput)?;
            committed
                .get_mut(start..end)
                .ok_or(DriverError::InvalidInput)?
                .copy_from_slice(&bytes);
        }
        Ok(())
    }
}

impl block::BioOperations for BufferedFaultDevice {
    fn submit_bio(&self, mut bio: block::Bio) {
        let block_id = bio.sector() / (self.block_size() as u64 / 512);
        let result = match bio.operation() {
            block::BioOperation::Read => {
                bio.read_contiguous(|bytes| self.read_block(block_id, bytes))
            }
            block::BioOperation::Write => {
                bio.write_contiguous(|bytes| self.write_block(block_id, bytes))
            }
            block::BioOperation::Flush => self.flush(),
        };
        bio.end_io(result);
    }
}

fn block_start(block_id: u64) -> Result<usize, DriverError> {
    usize::try_from(block_id)
        .map_err(|_| DriverError::InvalidInput)?
        .checked_mul(DEVICE_BLOCK_SIZE)
        .ok_or(DriverError::InvalidInput)
}
