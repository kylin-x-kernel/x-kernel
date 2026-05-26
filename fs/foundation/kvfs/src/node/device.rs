// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device file operations trait and memory mapping types.

use alloc::sync::Arc;
use core::any::Any;

use kpoll::Pollable;
use memaddr::PhysAddrRange;

use crate::{NodeFlags, VfsError, VfsResult};

/// Memory mapping behavior for device files.
pub enum DeviceMmap {
    /// The device is not mappable.
    None,
    /// Maps to a physical address range.
    Physical(PhysAddrRange),
    /// The device is read-only and will be mapped as CoW.
    ReadOnly,
    /// Maps to a cached file backend.
    ///
    /// The payload is type-erased so that this crate does not depend on the
    /// page-cache implementation. Producers wrap a `CachedFile` in
    /// `Arc::new(..)` and consumers downcast it back.
    Cache(Arc<dyn Any + Send + Sync>),
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

    /// Returns the memory mapping behavior of the device.
    fn mmap(&self) -> DeviceMmap {
        DeviceMmap::None
    }

    /// Returns the flags for the device node.
    fn flags(&self) -> NodeFlags {
        NodeFlags::empty()
    }
}
