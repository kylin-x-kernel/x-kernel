// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU residency state owned by one address space.

use alloc::sync::Arc;

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use kspin::SpinNoPreempt;

/// CPUs that may still hold valid TLB state for one address space.
///
/// This mask is owned by `MmSpace` rather than individual tasks. Non-AArch64
/// user-page-table shootdown uses it as a conservative target set: extra CPUs
/// are acceptable, but missing a CPU that may still retain stale translations
/// is not.
pub struct MmCpuResidency {
    active_cpu_mask: SpinNoPreempt<KCpuMask>,
}

impl MmCpuResidency {
    /// Creates an empty residency set.
    pub fn new() -> Self {
        Self {
            active_cpu_mask: SpinNoPreempt::new(KCpuMask::new()),
        }
    }

    /// Returns a snapshot of CPUs that may still retain this mm's TLB state.
    pub fn snapshot(&self) -> KCpuMask {
        *self.active_cpu_mask.lock()
    }

    /// Marks `cpu_id` as potentially retaining this mm's TLB state.
    pub fn set_cpu(&self, cpu_id: LogicalCpuId) {
        self.active_cpu_mask.lock().set_logical(cpu_id, true);
    }

    /// Clears `cpu_id` from this mm's residency set.
    pub fn clear_cpu(&self, cpu_id: LogicalCpuId) {
        self.active_cpu_mask.lock().set_logical(cpu_id, false);
    }

    /// Resets residency to exactly one CPU.
    pub fn reset_to_cpu(&self, cpu_id: LogicalCpuId) {
        *self.active_cpu_mask.lock() = KCpuMask::one_shot_logical(cpu_id);
    }
}

impl Default for MmCpuResidency {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared handle to one address space's CPU residency state.
pub type MmCpuResidencyRef = Arc<MmCpuResidency>;

unittest::path_mod_test!(tests_cpu_residency, "../ut-tests/cpu_residency.rs");
