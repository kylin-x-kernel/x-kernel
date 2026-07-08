// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Execution state shared by all threads in a process runtime.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use ksync::Mutex;
use ktimer::ProcessTimerManager;
use memspace::{MmCpuResidencyRef, MmSpace};

use crate::Pid;

/// Process runtime execution state shared by all threads in a process.
pub(super) struct ProcessRuntimeState {
    address_space: Arc<Mutex<MmSpace>>,
    mm_cpu_residency: MmCpuResidencyRef,
    #[cfg(target_arch = "aarch64")]
    user_asid_context: Arc<memspace::Aarch64UserAsidContext>,
    heap_top: AtomicUsize,
    timer_manager: Arc<Mutex<ProcessTimerManager>>,
}

impl ProcessRuntimeState {
    /// Creates a new [`ProcessRuntimeState`].
    pub(super) fn new(
        owner_pid: Pid,
        address_space: Arc<Mutex<MmSpace>>,
        user_heap_base: usize,
    ) -> Self {
        let mm_cpu_residency = address_space.lock().cpu_residency().clone();
        #[cfg(target_arch = "aarch64")]
        let user_asid_context = address_space
            .lock()
            .user_asid_context()
            .expect("user process address space must carry an AArch64 ASID context")
            .clone();
        Self {
            address_space,
            mm_cpu_residency,
            #[cfg(target_arch = "aarch64")]
            user_asid_context,
            heap_top: AtomicUsize::new(user_heap_base),
            timer_manager: Arc::new(Mutex::new(ProcessTimerManager::new(owner_pid))),
        }
    }

    /// Returns the virtual address space.
    pub(super) fn address_space(&self) -> &Arc<Mutex<MmSpace>> {
        &self.address_space
    }

    /// Returns the mm-owned CPU residency state for this process address space.
    pub(super) fn mm_cpu_residency(&self) -> &MmCpuResidencyRef {
        &self.mm_cpu_residency
    }

    #[cfg(target_arch = "aarch64")]
    /// Returns the latest hardware page-table root for context switching.
    pub(super) fn page_table_hw_root(&self) -> karch::HwPageTableRoot {
        self.user_asid_context.prepare_switch_root()
    }

    /// Returns the top address of the user heap.
    pub(super) fn heap_top(&self) -> usize {
        self.heap_top.load(Ordering::Acquire)
    }

    /// Sets the top address of the user heap.
    pub(super) fn set_heap_top(&self, top: usize) {
        self.heap_top.store(top, Ordering::Release)
    }

    /// Returns the process-owned timer manager.
    pub(super) fn timer_manager(&self) -> &Arc<Mutex<ProcessTimerManager>> {
        &self.timer_manager
    }
}
