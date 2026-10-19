// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/cpu_dma_latency device node.

use alloc::sync::Arc;

use kerrno::KError;
use kvfs::{DeviceFileOps, DeviceId, DirMapping, NodeFlags, SimpleFs, VfsFile, VfsResult};

use crate::{DeviceFile, add_device_entry};

/// /dev/cpu_dma_latency device - controls CPU DMA latency constraints.
struct CpuDmaLatency;

impl CpuDmaLatency {
    fn read_bytes(&self, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(KError::InvalidInput)
    }

    fn write_bytes(&self, buf: &[u8]) -> VfsResult<usize> {
        Ok(buf.len())
    }
}

impl DeviceFileOps for CpuDmaLatency {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.read_bytes(buf)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        self.write_bytes(buf)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    add_device_entry(
        root,
        "cpu_dma_latency",
        DeviceFile::new_character(fs, DeviceId::new(10, 1024), Arc::new(CpuDmaLatency)),
    );
}

unittest::path_mod_test!(tests, "../../ut-tests/cpu_dma_latency.rs");
