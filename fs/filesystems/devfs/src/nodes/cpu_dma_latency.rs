// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/cpu_dma_latency device node.

use alloc::sync::Arc;
use core::any::Any;

use kerrno::KError;
use kvfs::{DeviceFileOps, DeviceId, NodeFlags, NodeType, VfsResult};
use kvfs_simple::{DirMapping, SimpleFs};

use crate::DeviceFile;

/// /dev/cpu_dma_latency device - controls CPU DMA latency constraints.
struct CpuDmaLatency;

impl DeviceFileOps for CpuDmaLatency {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(KError::InvalidInput)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "cpu_dma_latency",
        DeviceFile::new(
            fs,
            NodeType::CharacterDevice,
            DeviceId::new(10, 1024),
            Arc::new(CpuDmaLatency),
        ),
    );
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_cpu_dma_latency_read_error() {
        let dev = CpuDmaLatency;
        let mut buf = [0u8; 4];
        assert!(dev.read_at(&mut buf, 0).is_err());
    }

    #[def_test]
    fn test_cpu_dma_latency_write_ok() {
        let dev = CpuDmaLatency;
        let data = [0u8; 4];
        let n = dev.write_at(&data, 0).unwrap();
        assert_eq!(n, 4);
    }
}
