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

unittest::path_mod_test!(tests, "../../ut-tests/null_zero_full.rs");
