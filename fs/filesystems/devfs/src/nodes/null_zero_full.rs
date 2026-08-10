// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/null, /dev/zero, and /dev/full device nodes.

use alloc::sync::Arc;

use kerrno::KError;
use kvfs::{
    DeviceFileOps, DeviceId, DirMapping, MmapMapper, NodeFlags, SimpleFs, VfsFile, VfsResult,
};

use crate::{DeviceFile, add_device_entry};

/// /dev/null device - discards all writes and returns empty on reads.
struct Null;

impl Null {
    fn read_bytes(&self, _buf: &mut [u8]) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_bytes(&self, buf: &[u8]) -> VfsResult<usize> {
        Ok(buf.len())
    }
}

impl DeviceFileOps for Null {
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

/// /dev/zero device - returns zero-filled data on reads.
struct Zero;

impl Zero {
    fn read_bytes(&self, buf: &mut [u8]) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_bytes(&self, buf: &[u8]) -> VfsResult<usize> {
        Ok(buf.len())
    }
}

impl DeviceFileOps for Zero {
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

    fn mmap(&self, _file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        mapper.map_anonymous_shared()
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

/// /dev/full device - returns ENOSPC error on writes.
struct Full;

impl Full {
    fn read_bytes(&self, buf: &mut [u8]) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_bytes(&self, _buf: &[u8]) -> VfsResult<usize> {
        Err(KError::StorageFull)
    }
}

impl DeviceFileOps for Full {
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
        "null",
        DeviceFile::new_character(fs.clone(), DeviceId::new(1, 3), Arc::new(Null)),
    );
    add_device_entry(
        root,
        "zero",
        DeviceFile::new_character(fs.clone(), DeviceId::new(1, 5), Arc::new(Zero)),
    );
    add_device_entry(
        root,
        "full",
        DeviceFile::new_character(fs, DeviceId::new(1, 7), Arc::new(Full)),
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
        let n = dev.read_bytes(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[def_test]
    fn test_null_write_accepts_all() {
        let dev = Null;
        let data = [1u8; 128];
        let n = dev.write_bytes(&data).unwrap();
        assert_eq!(n, 128);
    }

    #[def_test]
    fn test_zero_read_fills_zeros() {
        let dev = Zero;
        let mut buf = [0xFFu8; 32];
        let n = dev.read_bytes(&mut buf).unwrap();
        assert_eq!(n, 32);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[def_test]
    fn test_zero_write_accepts_all() {
        let dev = Zero;
        let data = [42u8; 64];
        let n = dev.write_bytes(&data).unwrap();
        assert_eq!(n, 64);
    }

    #[def_test]
    fn test_full_read_fills_zeros() {
        let dev = Full;
        let mut buf = [0xFFu8; 16];
        let n = dev.read_bytes(&mut buf).unwrap();
        assert_eq!(n, 16);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[def_test]
    fn test_full_write_returns_error() {
        let dev = Full;
        let data = [1u8; 8];
        assert!(dev.write_bytes(&data).is_err());
    }

    #[def_test]
    fn test_null_flags() {
        let dev = Null;
        let flags = dev.flags();
        assert!(flags.contains(NodeFlags::NON_CACHEABLE));
    }

    #[def_test]
    fn test_zero_flags() {
        let dev = Zero;
        let flags = dev.flags();
        assert!(flags.contains(NodeFlags::NON_CACHEABLE));
    }
}
