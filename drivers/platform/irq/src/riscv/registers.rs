// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PLIC register geometry and bounds-checked MMIO access.
//!
//! This module owns the register layout used by the driver. It never creates a
//! reference to the whole register block: a platform may map a shorter window,
//! so every access is checked against the mapped length before the volatile
//! read or write.

use core::{num::NonZeroU32, ptr::NonNull};

/// Maximum source supported by the PLIC priority-array layout.
pub(super) const MAX_SOURCE: u32 = 1023;
/// Maximum context-local register block supported by this layout.
pub(super) const MAX_CONTEXT: usize = 15872;

const PRIORITY_BYTES: usize = 0x1000;
const ENABLE_BASE: usize = 0x2000;
const ENABLE_CONTEXT_STRIDE: usize = 0x80;
const CONTEXT_BASE: usize = 0x20_0000;
const CONTEXT_STRIDE: usize = 0x1000;
const CONTEXT_CLAIM_BYTES: usize = 8;

/// Rejection at the register-window construction boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlicRegisterError {
    /// The mapped base is not aligned for the required 32-bit MMIO accesses.
    UnalignedBase,
    /// The mapped length is not a nonempty whole number of 32-bit registers.
    InvalidWindow,
}

impl core::fmt::Display for PlicRegisterError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnalignedBase => formatter.write_str("PLIC MMIO base is not 4-byte aligned"),
            Self::InvalidWindow => formatter.write_str("PLIC MMIO length is not register-aligned"),
        }
    }
}

/// Bounds-checked access to one mapped PLIC register window.
pub(super) struct PlicRegisters {
    base: NonNull<u8>,
    size: usize,
}

// SAFETY: the pointer and length describe one immutable platform MMIO mapping.
// All accesses are volatile, bounds-checked and serialized by the controller
// lock; no Rust memory aliasing is introduced.
unsafe impl Send for PlicRegisters {}
// SAFETY: see the `Send` implementation.
unsafe impl Sync for PlicRegisters {}

impl PlicRegisters {
    /// Creates a register view over an already-mapped PLIC window.
    ///
    /// # Safety
    /// `base` must point to at least `size` readable and writable bytes of the
    /// live PLIC MMIO mapping, and that mapping must remain valid for the
    /// lifetime of this value.
    ///
    /// # Errors
    /// Rejects a base that is not aligned for 32-bit MMIO access, or a length
    /// that is zero or not a whole number of 32-bit registers.
    pub(super) unsafe fn new(base: NonNull<u8>, size: usize) -> Result<Self, PlicRegisterError> {
        let register_size = core::mem::size_of::<u32>();
        if size < register_size || !size.is_multiple_of(register_size) {
            return Err(PlicRegisterError::InvalidWindow);
        }
        if !(base.as_ptr() as usize).is_multiple_of(core::mem::align_of::<u32>()) {
            return Err(PlicRegisterError::UnalignedBase);
        }
        Ok(Self { base, size })
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: the register offset is checked against \
                  the mapped window before the volatile access"
    )]
    fn register(&self, offset: usize) -> *mut u32 {
        let end = offset
            .checked_add(core::mem::size_of::<u32>())
            .expect("PLIC register offset overflow");
        assert!(
            end <= self.size,
            "PLIC register access outside mapped window: offset={offset:#x}, size={:#x}",
            self.size
        );
        self.base.as_ptr().wrapping_add(offset) as *mut u32
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: callers pass a validated PLIC source, \
                  context index and register offset"
    )]
    fn offset(offset: Option<usize>) -> usize {
        offset.expect("validated PLIC register layout")
    }

    fn read32(&self, offset: usize) -> u32 {
        let register = self.register(offset) as *const u32;
        // SAFETY: `register` was checked against the mapping promised to `new`.
        unsafe { register.read_volatile() }
    }

    fn write32(&self, offset: usize, value: u32) {
        let register = self.register(offset);
        // SAFETY: `register` was checked against the mapping promised to `new`.
        unsafe { register.write_volatile(value) };
    }

    fn priority_offset(source: NonZeroU32) -> Option<usize> {
        let source = usize::try_from(source.get()).ok()?;
        if source == 0 || source > MAX_SOURCE as usize {
            return None;
        }
        source.checked_mul(core::mem::size_of::<u32>())
    }

    fn enable_offset(source: NonZeroU32, context: usize) -> Option<usize> {
        if context >= MAX_CONTEXT {
            return None;
        }
        let source = usize::try_from(source.get()).ok()?;
        if source == 0 || source > MAX_SOURCE as usize {
            return None;
        }
        let group = source / u32::BITS as usize;
        ENABLE_BASE
            .checked_add(context.checked_mul(ENABLE_CONTEXT_STRIDE)?)?
            .checked_add(group.checked_mul(core::mem::size_of::<u32>())?)
    }

    fn local_offset(context: usize, offset: usize) -> Option<usize> {
        if context >= MAX_CONTEXT {
            return None;
        }
        CONTEXT_BASE
            .checked_add(context.checked_mul(CONTEXT_STRIDE)?)?
            .checked_add(offset)
    }

    /// Returns the exclusive end of one context's enable block.
    pub(super) fn enable_context_end(context: usize) -> Option<usize> {
        ENABLE_BASE
            .checked_add(context.checked_mul(ENABLE_CONTEXT_STRIDE)?)?
            .checked_add(ENABLE_CONTEXT_STRIDE)
    }

    /// Returns the minimum MMIO length needed by one context.
    pub(super) fn required_aperture(context: usize) -> Option<usize> {
        let enable_end = Self::enable_context_end(context)?;
        let local_end = Self::local_offset(context, CONTEXT_CLAIM_BYTES)?;
        Some(PRIORITY_BYTES.max(enable_end).max(local_end))
    }

    /// Validates a context against the layout and the mapped window length.
    #[cfg(unittest)]
    pub(super) fn validate_context(context: usize, size: usize) -> Option<usize> {
        let required = Self::required_aperture(context)?;
        (size >= required).then_some(required)
    }

    pub(super) fn read_priority(&self, source: NonZeroU32) -> u32 {
        let offset = Self::offset(Self::priority_offset(source));
        self.read32(offset)
    }

    pub(super) fn write_priority(&mut self, source: NonZeroU32, priority: u32) {
        let offset = Self::offset(Self::priority_offset(source));
        self.write32(offset, priority);
    }

    pub(super) fn is_enabled(&self, source: NonZeroU32, context: usize) -> bool {
        let offset = Self::offset(Self::enable_offset(source, context));
        let bit = 1u32 << (source.get() % u32::BITS);
        self.read32(offset) & bit != 0
    }

    pub(super) fn set_enabled(&mut self, source: NonZeroU32, context: usize, enabled: bool) {
        let offset = Self::offset(Self::enable_offset(source, context));
        let bit = 1u32 << (source.get() % u32::BITS);
        let value = self.read32(offset);
        let next = if enabled { value | bit } else { value & !bit };
        self.write32(offset, next);
    }

    pub(super) fn init_by_context(&mut self, context: usize) {
        let offset = Self::offset(Self::local_offset(context, 0));
        self.write32(offset, 0);
    }

    pub(super) fn claim(&mut self, context: usize) -> Option<NonZeroU32> {
        let offset = Self::offset(Self::local_offset(context, core::mem::size_of::<u32>()));
        NonZeroU32::new(self.read32(offset))
    }

    pub(super) fn complete(&mut self, context: usize, source: NonZeroU32) {
        let offset = Self::offset(Self::local_offset(context, core::mem::size_of::<u32>()));
        self.write32(offset, source.get());
    }
}
