// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Page-fault dispatch outcomes.

use khal::trap::PageFaultFlags;
use memaddr::VirtAddr;

/// External page-fault request before VMA lookup.
///
/// This is the stable boundary type for trap and user-runtime callers. It
/// carries only CPU/user-visible fault facts; VMA-derived
/// backing offsets are attached when it is converted into [`FaultContext`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaultInput {
    address: VirtAddr,
    access_flags: PageFaultFlags,
}

impl FaultInput {
    /// Creates a fault request from the trap-reported address and access mode.
    pub const fn new(address: VirtAddr, access_flags: PageFaultFlags) -> Self {
        Self {
            address,
            access_flags,
        }
    }

    /// Returns the faulting virtual address.
    pub const fn address(self) -> VirtAddr {
        self.address
    }

    /// Returns the access mode that triggered the fault.
    pub const fn access_flags(self) -> PageFaultFlags {
        self.access_flags
    }

    /// Converts the external request into a VMA-local context.
    pub const fn into_context(self) -> FaultContext {
        FaultContext::new(self.address, self.access_flags)
    }
}

/// Internal page-fault request resolved against a single VMA instance.
///
/// `FaultContext` starts from [`FaultInput`] and is enriched with VMA-derived
/// backing coordinates before it is passed to runtime/backing handlers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaultContext {
    address: VirtAddr,
    access_flags: PageFaultFlags,
    page_index: Option<u64>,
    file_offset: Option<u64>,
    page_data_offset: Option<usize>,
}

impl FaultContext {
    /// Creates a fault context from the trap-reported address and access mode.
    pub const fn new(address: VirtAddr, access_flags: PageFaultFlags) -> Self {
        Self {
            address,
            access_flags,
            page_index: None,
            file_offset: None,
            page_data_offset: None,
        }
    }

    /// Attaches VMA-derived backing indices to the fault context.
    pub const fn with_backing(
        mut self,
        page_index: Option<u64>,
        file_offset: Option<u64>,
        page_data_offset: Option<usize>,
    ) -> Self {
        self.page_index = page_index;
        self.file_offset = file_offset;
        self.page_data_offset = page_data_offset;
        self
    }

    /// Returns the faulting virtual address.
    pub const fn address(self) -> VirtAddr {
        self.address
    }

    /// Returns the access mode that triggered the fault.
    pub const fn access_flags(self) -> PageFaultFlags {
        self.access_flags
    }

    /// Returns the VMA-derived backing page index, if available.
    pub const fn page_index(self) -> Option<u64> {
        self.page_index
    }

    /// Returns the VMA-derived backing byte offset, if available.
    pub const fn file_offset(self) -> Option<u64> {
        self.file_offset
    }

    /// Returns the byte offset inside the faulting page where this VMA's data
    /// begins, if the VMA metadata provided it.
    pub const fn page_data_offset(self) -> Option<usize> {
        self.page_data_offset
    }

    /// Returns the backing-object byte offset corresponding to the start of
    /// the faulting page anchored at `page_base`.
    ///
    /// This keeps page-relative file offset calculation attached to the VMA
    /// fault context rather than re-deriving it inside file-backed runtime
    /// materialization code.
    pub fn page_file_offset(self, page_base: VirtAddr) -> Option<u64> {
        let delta = self.address.as_usize().saturating_sub(page_base.as_usize()) as u64;
        self.file_offset
            .and_then(|offset| offset.checked_sub(delta))
    }
}

/// Result of resolving a page fault against a VMA/backing object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultOutcome {
    /// Fault was resolved and the mapping is now present.
    Resolved,
    /// Fault should be retried because the observed state changed.
    Retry,
    /// Address was outside the address space or did not belong to any VMA.
    Unmapped,
    /// VMA existed, but requested access was not permitted.
    AccessDenied,
    /// The backing object rejected the access, such as file-backed fault past EOF.
    BusError,
    /// The backing object or page table path ran out of memory.
    OutOfMemory,
    /// COW observed a conflicting PTE/object state and should retry.
    CowConflictRetry,
    /// Backing object handled the request but no page was materialized.
    NoProgress,
    /// Backing object failed while trying to resolve the fault.
    Failed,
}

impl FaultOutcome {
    /// Returns `true` if the fault was successfully resolved.
    pub const fn is_resolved(self) -> bool {
        matches!(self, Self::Resolved)
    }

    /// Returns `true` if the caller may retry the fault without reporting it
    /// to user space.
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Retry | Self::CowConflictRetry)
    }

    /// Returns `true` if the fault should be surfaced as a Linux SIGBUS-class
    /// condition rather than a generic address/protection fault.
    pub const fn is_bus_error(self) -> bool {
        matches!(self, Self::BusError)
    }
}

/// Compatibility alias for older call sites.
pub type PageFaultOutcome = FaultOutcome;

#[cfg(unittest)]
mod tests {
    use khal::trap::PageFaultFlags;
    use memaddr::VirtAddr;
    use unittest::def_test;

    use super::{FaultInput, FaultOutcome};

    #[def_test]
    fn fault_input_preserves_trap_address_and_access() {
        let input = FaultInput::new(VirtAddr::from_usize(0x4000), PageFaultFlags::WRITE);

        assert_eq!(input.address(), VirtAddr::from_usize(0x4000));
        assert_eq!(input.access_flags(), PageFaultFlags::WRITE);
        assert_eq!(input.into_context().address(), VirtAddr::from_usize(0x4000));
    }

    #[def_test]
    fn fault_outcome_classifies_success_retry_and_bus() {
        assert!(FaultOutcome::Resolved.is_resolved());
        assert!(!FaultOutcome::Unmapped.is_resolved());
        assert!(FaultOutcome::Retry.is_retryable());
        assert!(FaultOutcome::CowConflictRetry.is_retryable());
        assert!(!FaultOutcome::AccessDenied.is_retryable());
        assert!(FaultOutcome::BusError.is_bus_error());
    }
}
