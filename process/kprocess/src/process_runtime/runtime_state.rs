// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Execution state shared by all threads in a process runtime.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use ksync::Mutex;
use ktimer::ProcessTimerManager;
use memspace::{
    MmCpuResidencyRef, MmSpace,
    process_lifetime::{MmPin, MmUserHandle},
};

use crate::Pid;

/// Process runtime execution state shared by all threads in a process.
pub(super) struct ProcessRuntimeState {
    mm_pin: MmPin,
    mm_user: Mutex<Option<MmUserHandle>>,
    /// Immutable address-space identity; safe to read without the aspace lock.
    mm_id: u64,
    mm_cpu_residency: MmCpuResidencyRef,
    #[cfg(target_arch = "aarch64")]
    user_asid_context: Arc<memspace::Aarch64UserAsidContext>,
    heap_top: AtomicUsize,
    timers: ProcessTimers,
}

/// Process-owned timer state.
struct ProcessTimers {
    timer_manager: Arc<Mutex<ProcessTimerManager>>,
}

impl ProcessTimers {
    fn new(owner_pid: Pid) -> Self {
        Self {
            timer_manager: Arc::new(Mutex::new(ProcessTimerManager::new(owner_pid))),
        }
    }

    fn manager(&self) -> &Arc<Mutex<ProcessTimerManager>> {
        &self.timer_manager
    }
}

impl ProcessRuntimeState {
    /// Creates a new [`ProcessRuntimeState`].
    pub(super) fn new(owner_pid: Pid, mm_user: MmUserHandle, user_heap_base: usize) -> Self {
        let mm_pin = mm_user.pin();
        let address_space = mm_pin.address_space().clone();
        let aspace = address_space.lock();
        let mm_id = aspace.mm_id();
        let mm_cpu_residency = aspace.cpu_residency().clone();
        #[cfg(target_arch = "aarch64")]
        let user_asid_context = aspace
            .user_asid_context()
            .expect("user process address space must carry an AArch64 ASID context")
            .clone();
        drop(aspace);
        Self {
            mm_pin,
            mm_user: Mutex::new(Some(mm_user)),
            mm_id,
            mm_cpu_residency,
            #[cfg(target_arch = "aarch64")]
            user_asid_context,
            heap_top: AtomicUsize::new(user_heap_base),
            timers: ProcessTimers::new(owner_pid),
        }
    }

    /// Returns the pinned address-space object for stable observation.
    pub(super) fn pinned_address_space(&self) -> &Arc<Mutex<MmSpace>> {
        self.mm_pin.address_space()
    }

    /// Returns the immutable address-space identity used by private futex keys.
    pub(super) fn mm_id(&self) -> u64 {
        self.mm_id
    }

    pub(super) fn clone_mm_user(&self) -> Option<MmUserHandle> {
        self.mm_user.lock().as_ref()?.clone_user_unless_zero()
    }

    /// Releases this runtime's address-space user and clears mappings for the last user.
    pub(super) fn clear_exclusive_address_space(&self) -> bool {
        let mm_user = self.mm_user.lock().take();
        mm_user.is_some_and(MmUserHandle::release_and_clear_if_last)
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
        self.timers.manager()
    }
}

impl Drop for ProcessRuntimeState {
    fn drop(&mut self) {
        self.clear_exclusive_address_space();
    }
}
