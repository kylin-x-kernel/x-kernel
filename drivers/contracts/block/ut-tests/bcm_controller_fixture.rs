// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU controller fixture matching the legacy SDK methods used by the adapter.
//! It exercises the real adapter's word views and sector loop, not MMIO or DMA.

use alloc::{vec, vec::Vec};

pub(crate) const BLOCK_SIZE: usize = 512;
const BLOCKS: usize = 8;

#[derive(Clone, Copy)]
pub(crate) enum SdhciError {
    Io,
    AlreadyExists,
    Again,
    BadState,
    InvalidParam,
    NoMemory,
    ResourceBusy,
    Unsupported,
}

pub(crate) struct EmmcCtl {
    pub(crate) storage: Vec<u32>,
    pub(crate) reads: usize,
    pub(crate) writes: usize,
    pub(crate) last_read_address: usize,
    pub(crate) last_write_address: usize,
    pub(crate) read_failure: Option<(u32, SdhciError)>,
    pub(crate) write_failure: Option<(u32, SdhciError)>,
}

impl EmmcCtl {
    pub(crate) fn new() -> Self {
        Self {
            storage: vec![0; BLOCKS * BLOCK_SIZE / 4],
            reads: 0,
            writes: 0,
            last_read_address: 0,
            last_write_address: 0,
            read_failure: None,
            write_failure: None,
        }
    }

    pub(crate) fn init(&mut self) -> i32 {
        0
    }

    pub(crate) fn get_block_num(&self) -> u64 {
        BLOCKS as u64
    }

    pub(crate) fn get_block_size(&self) -> usize {
        BLOCK_SIZE
    }

    pub(crate) fn read_block(
        &mut self,
        block: u32,
        count: usize,
        words: &mut [u32],
    ) -> Result<(), SdhciError> {
        assert_eq!(count, 1);
        assert_eq!(words.len(), BLOCK_SIZE / 4);
        self.reads += 1;
        self.last_read_address = words.as_ptr() as usize;
        if let Some((failed_block, error)) = self.read_failure
            && block == failed_block
        {
            words.fill(0xeeeeeeee);
            return Err(error);
        }
        let start = block as usize * BLOCK_SIZE / 4;
        words.copy_from_slice(&self.storage[start..start + BLOCK_SIZE / 4]);
        Ok(())
    }

    pub(crate) fn write_block(
        &mut self,
        block: u32,
        count: usize,
        words: &[u32],
    ) -> Result<(), SdhciError> {
        assert_eq!(count, 1);
        assert_eq!(words.len(), BLOCK_SIZE / 4);
        self.writes += 1;
        self.last_write_address = words.as_ptr() as usize;
        if let Some((failed_block, error)) = self.write_failure
            && block == failed_block
        {
            return Err(error);
        }
        let start = block as usize * BLOCK_SIZE / 4;
        self.storage[start..start + BLOCK_SIZE / 4].copy_from_slice(words);
        Ok(())
    }
}
