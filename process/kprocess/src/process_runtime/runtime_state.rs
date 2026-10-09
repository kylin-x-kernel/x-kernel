// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Execution state shared by all threads in a process runtime.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kerrno::{KError, KResult};
use ksync::{Mutex, spin::SpinNoIrq};
use ktimer::ProcessTimerManager;
use memspace::{
    MmCpuResidencyRef, MmSpace,
    process_lifetime::{MmPin, MmUserHandle},
};

use crate::{AsThread, LiveAddressSpace, Pid};

/// Process runtime execution state shared by all threads in a process.
pub(super) struct ProcessRuntimeState {
    // Scheduler callbacks access the binding with IRQs disabled.
    address_space: SpinNoIrq<ProcessAddressSpace>,
    timers: ProcessTimers,
}

/// Ownership and activation metadata derived from the same address space.
struct ProcessAddressSpace {
    /// Keeps the address-space object pinned for runtime-lifetime observers.
    mm_pin: MmPin,
    mm_user: Option<MmUserHandle>,
    mm_id: u64,
    mm_cpu_residency: MmCpuResidencyRef,
    heap_top: Arc<AtomicUsize>,
    #[cfg(target_arch = "aarch64")]
    user_asid_context: Arc<memspace::Aarch64UserAsidContext>,
    #[cfg(not(target_arch = "aarch64"))]
    hardware_root: karch::HwPageTableRoot,
}

/// Process-owned timer state.
struct ProcessTimers {
    timer_manager: Arc<Mutex<ProcessTimerManager>>,
}

/// Immutable scheduling ownership for a thread that has left its process.
/// Contains no active MM user; mappings may be cleared while the root is pinned.
pub(crate) struct ExitAddressSpace {
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: owns the exit snapshot pin until Drop; inspected only \
                  by unittest observers"
    )]
    #[allow(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: owns the exit snapshot pin until Drop; \
                  inspected only by unittest observers"
    )]
    mm_pin: MmPin,
    mm_cpu_residency: MmCpuResidencyRef,
    #[cfg(target_arch = "aarch64")]
    user_asid_context: Arc<memspace::Aarch64UserAsidContext>,
}

impl ExitAddressSpace {
    pub(crate) fn set_mm_resident_cpu(&self, cpu_id: kcpu_id_map::LogicalCpuId) {
        self.mm_cpu_residency.set_cpu(cpu_id);
    }

    #[cfg(target_arch = "aarch64")]
    pub(crate) fn page_table_hw_root(&self) -> karch::HwPageTableRoot {
        // Prepare on the pinned CPU on every switch, including ASID rollover.
        self.user_asid_context.prepare_switch_root()
    }
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

impl ProcessAddressSpace {
    /// Builds matching ownership and metadata in sleepable context before publication.
    fn new(mm_user: MmUserHandle, user_heap_base: usize) -> Self {
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
        #[cfg(not(target_arch = "aarch64"))]
        let hardware_root = aspace.page_table_hw_root();
        drop(aspace);
        Self {
            mm_pin,
            mm_user: Some(mm_user),
            mm_id,
            mm_cpu_residency,
            heap_top: Arc::new(AtomicUsize::new(user_heap_base)),
            #[cfg(target_arch = "aarch64")]
            user_asid_context,
            #[cfg(not(target_arch = "aarch64"))]
            hardware_root,
        }
    }
}

impl ProcessRuntimeState {
    /// Creates a new [`ProcessRuntimeState`].
    pub(super) fn new(owner_pid: Pid, mm_user: MmUserHandle, user_heap_base: usize) -> Self {
        Self {
            address_space: SpinNoIrq::new(ProcessAddressSpace::new(mm_user, user_heap_base)),
            timers: ProcessTimers::new(owner_pid),
        }
    }

    /// Returns the pinned address-space object for stable observation.
    #[cfg(unittest)]
    pub(super) fn pinned_address_space(&self) -> Arc<Mutex<MmSpace>> {
        self.address_space.lock().mm_pin.address_space().clone()
    }

    /// Returns the bound address-space identity used by private futex keys.
    pub(super) fn mm_id(&self) -> u64 {
        self.address_space.lock().mm_id
    }

    /// Clones one coherent pin/residency/ASID tuple without MM locking or allocation.
    pub(super) fn capture_exit_address_space(&self) -> ExitAddressSpace {
        let binding = self.address_space.lock();
        ExitAddressSpace {
            mm_pin: binding.mm_pin.clone(),
            mm_cpu_residency: binding.mm_cpu_residency.clone(),
            #[cfg(target_arch = "aarch64")]
            user_asid_context: binding.user_asid_context.clone(),
        }
    }

    pub(super) fn clone_mm_user(&self) -> Option<MmUserHandle> {
        self.address_space
            .lock()
            .mm_user
            .as_ref()?
            .clone_user_unless_zero()
    }

    /// Captures the live MM and its matching heap marker in one critical section.
    pub(super) fn address_space(&self) -> Option<LiveAddressSpace> {
        let binding = self.address_space.lock();
        let mm_user = binding.mm_user.as_ref()?.clone_user_unless_zero()?;
        Some(LiveAddressSpace::new(mm_user, binding.heap_top.clone()))
    }

    /// Releases this runtime's address-space user and clears mappings for the last user.
    pub(super) fn exit_mm(&self) -> bool {
        // Final release may lock the MM and tear down mappings; keep it outside
        // the binding lock. The structural pin remains for scheduling tails.
        let mm_user = self.address_space.lock().mm_user.take();
        mm_user.is_some_and(|user| {
            // An uncontended MM mutex would otherwise hide a lock-scope regression.
            #[cfg(unittest)]
            assert!(ktask::is_sleepable_context());
            user.release_and_clear_if_last()
        })
    }

    /// Records CPU residency in the bound address space without taking its mutex.
    pub(super) fn set_mm_resident_cpu(&self, cpu_id: kcpu_id_map::LogicalCpuId) {
        self.address_space.lock().mm_cpu_residency.set_cpu(cpu_id);
    }

    /// Returns the latest hardware page-table root for context switching.
    #[cfg(target_arch = "aarch64")]
    pub(super) fn page_table_hw_root(&self) -> karch::HwPageTableRoot {
        self.address_space
            .lock()
            .user_asid_context
            .prepare_switch_root()
    }

    /// Returns the top address of the user heap.
    pub(super) fn heap_top(&self) -> usize {
        self.address_space.lock().heap_top.load(Ordering::Acquire)
    }

    /// Sets the top address of the user heap.
    pub(super) fn set_heap_top(&self, top: usize) {
        self.address_space
            .lock()
            .heap_top
            .store(top, Ordering::Release)
    }

    /// Returns the process-owned timer manager.
    pub(super) fn timer_manager(&self) -> &Arc<Mutex<ProcessTimerManager>> {
        self.timers.manager()
    }
}

/// Replaces the current single-threaded process's MM and activates its root.
///
/// Construction and displaced-owner release may sleep. No error is returned
/// after exchange. Unittest builds accept a request-local `fail_precheck` flag.
///
/// # Errors
/// Returns `NoSuchProcess` for a missing/exiting user thread or detached MM,
/// `InvalidInput` unless exactly the current thread is published, and
/// `WouldBlock` for the unittest precheck probe, before changing the binding.
///
/// # Panics
/// Panics if the scheduler has no current task.
///
/// # Safety
/// Call in sleepable task context without locks or old-user access windows.
/// No borrowed saved task context may span this call.
/// `new` must originate from `MmSpace::new_user_empty`, retain valid kernel
/// mappings, and remain user-empty. After success the caller must reset old
/// user registrations immediately, then complete loading/context reset or
/// take fatal exit; it must never resume the old user context. Kernel sleeping
/// during displaced-owner release must not dispatch user signals or cleanup
/// using old registrations against the new MM.
pub unsafe fn replace_current_address_space(
    new: MmSpace,
    #[cfg(unittest)] fail_precheck: bool,
) -> KResult<()> {
    debug_assert!(ktask::is_sleepable_context());
    let current = ktask::current();
    let thread = current.try_as_thread().ok_or(KError::NoSuchProcess)?;
    let process = thread.process();
    if thread.is_exiting() || process.is_exited() || process.is_group_exited() {
        return Err(KError::NoSuchProcess);
    }
    if process.thread_count() != 1 || !process.contains_published_tid(thread.tid()) {
        return Err(KError::InvalidInput);
    }
    let runtime = thread.process_runtime();
    let user = MmSpace::acquire_user(Arc::new(Mutex::new(new))).ok_or(KError::NoSuchProcess)?;
    let candidate = ProcessAddressSpace::new(user, kaddr_layout::USER_HEAP_BASE);
    let displaced = {
        let mut binding = runtime.runtime_state.address_space.lock();
        if binding.mm_user.is_none() {
            return Err(KError::NoSuchProcess);
        }
        #[cfg(unittest)]
        if fail_precheck {
            return Err(KError::WouldBlock);
        }
        let displaced = core::mem::replace(&mut *binding, candidate);
        binding
            .mm_cpu_residency
            .set_cpu(khal::percpu::this_cpu_id());
        #[cfg(target_arch = "aarch64")]
        let root = binding.user_asid_context.prepare_switch_root();
        #[cfg(not(target_arch = "aarch64"))]
        let root = binding.hardware_root;
        // SAFETY: the current task's coherent binding owns the validated new
        // root; the displaced owner retains the old root through activation.
        // IRQ/preemption exclusion prevents switching during saved/root update.
        // The caller also excludes outstanding saved-context borrows.
        unsafe { ktask::replace_current_user_page_table(root) };
        displaced
    };
    drop(displaced);
    Ok(())
}

impl Drop for ProcessRuntimeState {
    fn drop(&mut self) {
        self.exit_mm();
    }
}

#[unittest::mod_test]
mod tests {
    include!("tests_runtime_state.rs");
}
