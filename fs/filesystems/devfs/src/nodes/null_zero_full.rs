// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/null, /dev/zero, and /dev/full device nodes.

use alloc::sync::Arc;
use core::any::Any;

use kerrno::KError;
use kvfs::{DeviceFileOps, DeviceId, MmapMapper, NodeFlags, NodeType, VfsResult};
use kvfs_simple::{DirMapping, SimpleFs};

use crate::DeviceFile;

/// /dev/null device - discards all writes and returns empty on reads.
struct Null;

impl DeviceFileOps for Null {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

/// /dev/zero device - returns zero-filled data on reads.
struct Zero;

impl DeviceFileOps for Zero {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn mmap(&self, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        mapper.map_anonymous_shared()
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

/// /dev/full device - returns ENOSPC error on writes.
struct Full;

impl DeviceFileOps for Full {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(KError::StorageFull)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "null",
        DeviceFile::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 3),
            Arc::new(Null),
        ),
    );
    root.add(
        "zero",
        DeviceFile::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 5),
            Arc::new(Zero),
        ),
    );
    root.add(
        "full",
        DeviceFile::new(
            fs,
            NodeType::CharacterDevice,
            DeviceId::new(1, 7),
            Arc::new(Full),
        ),
    );
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_null_read_returns_zero() {
        let dev = Null;
        let mut buf = [0xFFu8; 64];
        let n = dev.read_at(&mut buf, 0).unwrap();
        assert_eq!(n, 0);
    }

    #[def_test]
    fn test_null_write_accepts_all() {
        let dev = Null;
        let data = [1u8; 128];
        let n = dev.write_at(&data, 0).unwrap();
        assert_eq!(n, 128);
    }

    #[def_test]
    fn test_zero_read_fills_zeros() {
        let dev = Zero;
        let mut buf = [0xFFu8; 32];
        let n = dev.read_at(&mut buf, 0).unwrap();
        assert_eq!(n, 32);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[def_test]
    fn test_zero_write_accepts_all() {
        let dev = Zero;
        let data = [42u8; 64];
        let n = dev.write_at(&data, 0).unwrap();
        assert_eq!(n, 64);
    }

    #[def_test]
    fn test_full_read_fills_zeros() {
        let dev = Full;
        let mut buf = [0xFFu8; 16];
        let n = dev.read_at(&mut buf, 0).unwrap();
        assert_eq!(n, 16);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[def_test]
    fn test_full_write_returns_error() {
        let dev = Full;
        let data = [1u8; 8];
        assert!(dev.write_at(&data, 0).is_err());
    }

    #[def_test]
    fn test_null_flags() {
        let dev = Null;
        let flags = dev.flags();
        assert!(flags.contains(NodeFlags::NON_CACHEABLE));
        assert!(flags.contains(NodeFlags::STREAM));
    }

    #[def_test]
    fn test_zero_flags() {
        let dev = Zero;
        let flags = dev.flags();
        assert!(flags.contains(NodeFlags::NON_CACHEABLE));
        assert!(flags.contains(NodeFlags::STREAM));
    }
}
