// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{any::Any, task::Context};

use kerrno::{KError, KResult};
use tipc_handle::HandleWaitState;

use crate::{Handle, HandleEventMask, HandleKind};

const MEMREF_PAGE_SIZE: usize = 4096;
/// Read access permission for a memref mapping.
pub const MMAP_FLAG_PROT_READ: u32 = 0x1;
/// Write access permission for a memref mapping.
pub const MMAP_FLAG_PROT_WRITE: u32 = 0x2;
/// Execute access permission for a memref mapping.
pub const MMAP_FLAG_PROT_EXEC: u32 = 0x4;
/// Memory tagging permission for a memref mapping.
pub const MMAP_FLAG_PROT_MTE: u32 = 0x8;
/// Mask of all supported memref protection bits.
pub const MMAP_FLAG_PROT_MASK: u32 =
    MMAP_FLAG_PROT_READ | MMAP_FLAG_PROT_WRITE | MMAP_FLAG_PROT_EXEC | MMAP_FLAG_PROT_MTE;

/// A transferable reference to a caller-owned memory range.
///
/// This first-stage X-Kernel implementation keeps the Trusty-visible handle
/// lifetime and access metadata. Binding to a concrete VMM object and mmaping
/// the handle into another address space will be layered on top of this object.
pub struct MemRef {
    addr: usize,
    size: usize,
    mmap_prot: u32,
    handle: HandleWaitState,
}

impl MemRef {
    /// Creates a transferable memref handle.
    pub fn create(addr: usize, size: usize, mmap_prot: u32) -> KResult<Arc<Self>> {
        if size == 0 {
            return Err(KError::InvalidInput);
        }
        validate_mmap_prot(mmap_prot)?;
        addr.checked_add(size).ok_or(KError::OutOfRange)?;
        if !is_memref_page_aligned(addr) || !is_memref_page_aligned(size) {
            return Err(KError::InvalidInput);
        }
        Ok(Arc::new(Self {
            addr,
            size,
            mmap_prot,
            handle: HandleWaitState::new(),
        }))
    }

    /// Returns the userspace start address supplied at creation time.
    pub fn addr(&self) -> usize {
        self.addr
    }

    /// Returns the byte length of the referenced memory range.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Returns the allowed mmap protection flags.
    pub fn mmap_prot(&self) -> u32 {
        self.mmap_prot
    }

    /// Validates that an `mmap` request stays within this memref.
    pub fn validate_mmap(&self, offset: usize, size: usize, mmap_prot: u32) -> KResult {
        if size == 0 {
            return Err(KError::InvalidInput);
        }
        if !is_memref_page_aligned(offset) || !is_memref_page_aligned(size) {
            return Err(KError::InvalidInput);
        }
        if offset > self.size || size > self.size - offset {
            return Err(KError::PermissionDenied);
        }
        validate_mmap_prot(mmap_prot)?;
        if !is_mmap_accessible(self.mmap_prot, mmap_prot) {
            return Err(KError::PermissionDenied);
        }
        Ok(())
    }
}

fn is_memref_page_aligned(value: usize) -> bool {
    value & (MEMREF_PAGE_SIZE - 1) == 0
}

fn is_mmap_accessible(access_prot: u32, requested_prot: u32) -> bool {
    let requested = requested_prot & MMAP_FLAG_PROT_MASK;
    requested != 0 && access_prot & requested == requested
}

fn validate_mmap_prot(mmap_prot: u32) -> KResult {
    if mmap_prot & !MMAP_FLAG_PROT_MASK != 0 {
        return Err(KError::InvalidInput);
    }
    if mmap_prot & MMAP_FLAG_PROT_MASK == 0 {
        return Err(KError::InvalidInput);
    }
    if mmap_prot & MMAP_FLAG_PROT_EXEC != 0 {
        return Err(KError::InvalidInput);
    }
    if mmap_prot & MMAP_FLAG_PROT_WRITE != 0 && mmap_prot & MMAP_FLAG_PROT_READ == 0 {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

impl Handle for MemRef {
    fn kind(&self) -> HandleKind {
        HandleKind::MemRef
    }

    fn poll(&self, _finalize: bool) -> HandleEventMask {
        HandleEventMask::empty()
    }

    fn register(&self, cx: &mut Context<'_>, _event_mask: HandleEventMask) {
        self.handle.register(cx);
    }

    fn close(&self) {
        self.handle.notify();
    }

    fn set_cookie(&self, cookie: usize) {
        self.handle.set_cookie(cookie);
    }

    fn cookie(&self) -> usize {
        self.handle.cookie()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
