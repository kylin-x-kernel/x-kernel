// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime state shared by all threads in a process.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kfs::FsContext;
use kprocess::Pid;
use ksync::Mutex;
use ktimer::ProcessTimerManager;
use memspace::{MmCpuResidencyRef, MmSpace};

/// Process runtime state shared by all threads in a process.
pub struct ProcessRuntimeState {
    address_space: Arc<Mutex<MmSpace>>,
    mm_cpu_residency: MmCpuResidencyRef,
    #[cfg(target_arch = "aarch64")]
    user_asid_context: Arc<memspace::Aarch64UserAsidContext>,
    fs_context: Arc<Mutex<FsContext>>,
    heap_top: AtomicUsize,
    timer_manager: Arc<Mutex<ProcessTimerManager>>,
}

impl ProcessRuntimeState {
    /// Creates a new [`ProcessRuntimeState`].
    pub fn new(
        owner_pid: Pid,
        address_space: Arc<Mutex<MmSpace>>,
        fs_context: Arc<Mutex<FsContext>>,
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
            fs_context,
            heap_top: AtomicUsize::new(user_heap_base),
            timer_manager: Arc::new(Mutex::new(ProcessTimerManager::new(owner_pid))),
        }
    }

    /// Returns the virtual address space.
    pub fn address_space(&self) -> &Arc<Mutex<MmSpace>> {
        &self.address_space
    }

    /// Returns the mm-owned CPU residency state for this process address space.
    pub fn mm_cpu_residency(&self) -> &MmCpuResidencyRef {
        &self.mm_cpu_residency
    }

    #[cfg(target_arch = "aarch64")]
    /// Returns the latest hardware page-table root for context switching.
    pub fn page_table_hw_root(&self) -> karch::HwPageTableRoot {
        self.user_asid_context.prepare_switch_root()
    }

    /// Returns the process-owned filesystem context.
    pub fn fs_context(&self) -> &Arc<Mutex<FsContext>> {
        &self.fs_context
    }

    /// Returns the top address of the user heap.
    pub fn heap_top(&self) -> usize {
        self.heap_top.load(Ordering::Acquire)
    }

    /// Sets the top address of the user heap.
    pub fn set_heap_top(&self, top: usize) {
        self.heap_top.store(top, Ordering::Release)
    }

    /// Returns the process-owned timer manager.
    pub fn timer_manager(&self) -> &Arc<Mutex<ProcessTimerManager>> {
        &self.timer_manager
    }
}
