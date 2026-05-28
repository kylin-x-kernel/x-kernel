// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device file operations trait and memory mapping callback.

use core::any::Any;

use kpoll::Pollable;
use memaddr::PhysAddrRange;

use crate::{NodeFlags, VfsError, VfsResult};

/// Callback trait for establishing memory mappings.
///
/// Passed to `FileLike::mmap` / `DeviceFileOps::mmap` so that devices and files
/// can request mapping establishment without depending on the memory subsystem.
/// Implemented by the mmap syscall layer (posix-mm).
pub trait MmapMapper {
    /// Map a physical address range (device memory, framebuffer, etc.)
    fn map_physical(&mut self, range: PhysAddrRange) -> VfsResult<()>;

    /// Request a file-backed mapping (regular file or cached file).
    fn map_file_backed(&mut self) -> VfsResult<()>;
}

/// Trait for device file backend operations.
///
/// Implementors provide low-level read/write/ioctl semantics that are adapted
/// to the VFS node interface by [`DeviceFile`] in `kvfs_simple`.
pub trait DeviceFileOps: Send + Sync {
    /// Reads data from the device at the specified offset.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize>;
    /// Writes data to the device at the specified offset.
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize>;
    /// Manipulates the underlying device parameters of special files.
    fn ioctl(&self, _cmd: u32, _arg: usize) -> VfsResult<usize> {
        Err(VfsError::NotATty)
    }

    /// Casts the device operations to a dynamic type.
    fn as_any(&self) -> &dyn Any;

    /// Casts the device operations to a [`Pollable`].
    fn as_pollable(&self) -> Option<&dyn Pollable> {
        None
    }

    /// Handle mmap for this device via the provided mapper.
    /// Default returns `ENODEV` (mmap not supported).
    fn mmap(&self, _mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        Err(VfsError::NoSuchDevice)
    }

    /// Returns the flags for the device node.
    fn flags(&self) -> NodeFlags {
        NodeFlags::empty()
    }
}
