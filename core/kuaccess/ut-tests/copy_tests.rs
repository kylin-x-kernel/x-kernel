// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Demand faults must reach a sleepable backing provider from user copies.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use khal::paging::{PageSize, PageTableMut};
use ksync::Mutex;
use memaddr::{PAGE_SIZE_4K, VirtAddrRange};
use memspace::{
    FaultContext, FileMappingInfo, ForkCloneTarget, InvalidateHandle, MmSpace, VmArea,
    VmBackingInfo, VmBackingKind, VmRuntimeOps, VmRuntimeRef,
    backend::{FaultCompletion, FaultCompletionResult, PrivateBackend},
};
use unittest::def_test;
use vmobj::{FileObjectId, VmObjectId};

use super::*;

enum FaultBehavior {
    Normal,
    StagedWrite,
    Stalled,
    CowConflict,
    AdmissionWait,
    AdmissionReprotect(VirtAddr),
    NoProgress,
    /// Resolves immediately, so trap-handler resolution succeeds in place.
    Immediate,
}

struct SleepableBacking {
    pages: PrivateBackend,
    faults: AtomicUsize,
    behavior: FaultBehavior,
}

impl SleepableBacking {
    fn install(
        self: &Arc<Self>,
        mm: &mut MmSpace,
        start: VirtAddr,
        len: usize,
        flags: MappingFlags,
    ) {
        let vma = VmArea::new(
            start,
            len,
            flags,
            flags,
            self.backing_info(),
            0,
            Some(FileMappingInfo {
                offset: 0,
                inode: 1,
                path: None,
            }),
        );
        mm.map_runtime_vma(vma, false, VmRuntimeRef::new_file_shared(self.clone()))
            .unwrap();
    }
}

impl VmRuntimeOps for SleepableBacking {
    fn backing_info(&self) -> VmBackingInfo {
        VmBackingInfo::new(
            VmBackingKind::FileShared {
                object: VmObjectId::File(FileObjectId::from_raw(1)),
            },
            PageSize::Size4K,
        )
    }

    fn map(&self, range: VirtAddrRange, flags: MappingFlags, table: &mut PageTableMut) -> KResult {
        VmRuntimeOps::map(&self.pages, range, flags, table)
    }

    fn unmap(&self, range: VirtAddrRange, table: &mut PageTableMut) -> KResult {
        VmRuntimeOps::unmap(&self.pages, range, table)
    }

    fn on_protect(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        table: &mut PageTableMut,
    ) -> KResult<MappingFlags> {
        VmRuntimeOps::on_protect(&self.pages, range, flags, table)
    }

    fn handle_fault(
        &self,
        ctx: FaultContext,
        flags: MappingFlags,
        table: &mut PageTableMut,
    ) -> FaultCompletionResult {
        // Exercise the same context requirement as IRQ-driven block I/O, without
        // depending on disk timing. A remembered wake permits a deterministic park.
        if !matches!(self.behavior, FaultBehavior::Immediate) {
            let mut wait = ktask::future::PreparedTaskWait::prepare()?;
            if khal::context::in_exception_context() {
                return Err(KError::BadState);
            }
            wait.waker().wake_by_ref();
            wait.park();
        }
        let attempt = self.faults.fetch_add(1, Ordering::Relaxed) + 1;
        match self.behavior {
            FaultBehavior::StagedWrite
                if ctx.access_flags().contains(MappingFlags::WRITE)
                    && matches!(
                        table.query(ctx.address()),
                        Err(khal::paging::PagingError::NotMapped)
                    ) =>
            {
                // Model shared-file materialization separately from write permission.
                VmRuntimeOps::handle_fault(
                    &self.pages,
                    FaultContext::new(ctx.address(), MappingFlags::READ),
                    flags - MappingFlags::WRITE,
                    table,
                )
            }
            FaultBehavior::Stalled => Ok(FaultCompletion::from_populate((1, None))),
            FaultBehavior::CowConflict => Ok(FaultCompletion::cow_conflict_retry()),
            FaultBehavior::AdmissionWait if attempt <= MAX_USER_PAGE_FAULT_ATTEMPTS * 2 => {
                Err(KError::WouldBlock)
            }
            FaultBehavior::AdmissionReprotect(_) => Err(KError::WouldBlock),
            FaultBehavior::NoProgress => Ok(FaultCompletion::from_populate((0, None))),
            _ => VmRuntimeOps::handle_fault(&self.pages, ctx, flags, table),
        }
    }

    fn wait_for_fault(&self) -> KResult<()> {
        let curr = current();
        let space = curr.as_thread().process().address_space()?;
        // Taking this lock proves that the retry released MM serialization.
        let mut mm = space.lock();
        if let FaultBehavior::AdmissionReprotect(start) = self.behavior {
            mm.protect(start, PAGE_SIZE_4K, MappingFlags::USER | MappingFlags::READ)?;
        }
        drop(mm);
        ktask::yield_now();
        Ok(())
    }

    fn relocate_for_mremap(
        &self,
        source_range: VirtAddrRange,
        start: VirtAddr,
        new_len: usize,
        id: u64,
        space: &Arc<Mutex<MmSpace>>,
        invalidate: Option<InvalidateHandle>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        VmRuntimeOps::relocate_for_mremap(
            &self.pages,
            source_range,
            start,
            new_len,
            id,
            space,
            invalidate,
        )
    }

    fn clone_for_fork(
        &self,
        range: VirtAddrRange,
        flags: MappingFlags,
        old: &mut PageTableMut,
        new: &mut PageTableMut,
        target: ForkCloneTarget<'_>,
    ) -> KResult<Arc<dyn VmRuntimeOps>> {
        VmRuntimeOps::clone_for_fork(&self.pages, range, flags, old, new, target)
    }
}

#[def_test(user)]
fn user_copies_resolve_sleepable_backing_and_preserve_permissions() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::Normal,
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K * 3, flags);

    // The first copy crosses two initially absent pages without touching them
    // from userspace first. The third page is first faulted by copy-to-user.
    let address = start.as_usize() + PAGE_SIZE_4K - 8;
    let mut output = [MaybeUninit::<u8>::uninit(); 16];
    osvm::read_vm_bytes(address as *const u8, &mut output).unwrap();
    assert_eq!(backing.faults.load(Ordering::Relaxed), 2);
    for byte in output {
        // SAFETY: successful read_vm_bytes initialized every output byte.
        assert_eq!(unsafe { byte.assume_init() }, 0);
    }
    let write_address = start.as_usize() + PAGE_SIZE_4K * 3 - 8;
    osvm::write_vm_bytes(write_address as *mut u8, &[0x5a; 8]).unwrap();
    assert_eq!(backing.faults.load(Ordering::Relaxed), 3);
    let mut readback = [MaybeUninit::<u8>::uninit(); 8];
    osvm::read_vm_bytes(write_address as *const u8, &mut readback).unwrap();
    for byte in readback {
        // SAFETY: successful read_vm_bytes initialized every output byte.
        assert_eq!(unsafe { byte.assume_init() }, 0x5a);
    }

    space
        .lock()
        .protect(
            start,
            PAGE_SIZE_4K * 3,
            MappingFlags::USER | MappingFlags::READ,
        )
        .unwrap();
    assert_eq!(
        osvm::write_vm_bytes(write_address as *mut u8, &[1]),
        Err(MemError::NoAccess)
    );
    space.lock().unmap(start, PAGE_SIZE_4K * 3).unwrap();
    assert_eq!(
        osvm::read_vm_bytes(address as *const u8, &mut output),
        Err(MemError::NoAccess)
    );
}

#[def_test(user)]
fn user_copy_handles_staged_write_faults_across_pages() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::StagedWrite,
    });
    // Total faults exceed one page's budget, so completed pages must reset it.
    let page_count = MAX_USER_PAGE_FAULT_ATTEMPTS / 2 + 1;
    let mapping_len = PAGE_SIZE_4K * page_count;
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, mapping_len, flags);
    let address = start.as_usize() + PAGE_SIZE_4K - 8;
    let input = alloc::vec![0x5a; mapping_len - PAGE_SIZE_4K + 8];
    let result = osvm::write_vm_bytes(address as *mut u8, &input);
    let mut output = alloc::vec![MaybeUninit::<u8>::uninit(); input.len()];
    let read_result = osvm::read_vm_bytes(address as *const u8, &mut output);
    space.lock().unmap(start, mapping_len).unwrap();
    assert_eq!(
        result,
        Ok(()),
        "copy must survive materialization then write-permission faults"
    );
    assert_eq!(read_result, Ok(()));
    assert_eq!(backing.faults.load(Ordering::Relaxed), page_count * 2);
    for byte in output {
        // SAFETY: successful read_vm_bytes initialized every output byte.
        assert_eq!(unsafe { byte.assume_init() }, 0x5a);
    }
}

#[def_test(user)]
fn user_copy_bounds_faults_without_progress() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    for behavior in [
        FaultBehavior::Stalled,
        FaultBehavior::CowConflict,
        FaultBehavior::NoProgress,
    ] {
        let expected_attempts = if matches!(behavior, FaultBehavior::NoProgress) {
            1
        } else {
            MAX_USER_PAGE_FAULT_ATTEMPTS
        };
        let backing = Arc::new(SleepableBacking {
            pages: PrivateBackend::new(start, PageSize::Size4K),
            faults: AtomicUsize::new(0),
            behavior,
        });
        backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
        let result = osvm::write_vm_bytes(start.as_usize() as *mut u8, &[1]);
        space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
        assert_eq!(result, Err(MemError::NoAccess));
        assert_eq!(backing.faults.load(Ordering::Relaxed), expected_attempts);
    }
}

#[def_test(user)]
fn user_copy_admission_wait_does_not_consume_materialization_budget() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::AdmissionWait,
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
    let write = osvm::write_vm_bytes(start.as_usize() as *mut u8, &[0x5a]);
    let mut output = [MaybeUninit::<u8>::uninit(); 1];
    let read = osvm::read_vm_bytes(start.as_usize() as *const u8, &mut output);
    space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
    assert_eq!(write, Ok(()));
    assert_eq!(read, Ok(()));
    assert_eq!(
        backing.faults.load(Ordering::Relaxed),
        MAX_USER_PAGE_FAULT_ATTEMPTS * 2 + 1
    );
    // SAFETY: the successful read initialized the single output byte.
    assert_eq!(unsafe { output[0].assume_init() }, 0x5a);

    backing.faults.store(0, Ordering::Relaxed);
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
    assert_eq!(atomic_cmpxchg_u32(start.as_usize(), 0, 1), Ok((true, 0)));
    assert_eq!(
        backing.faults.load(Ordering::Relaxed),
        MAX_USER_PAGE_FAULT_ATTEMPTS * 2 + 1
    );
    space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
}

#[def_test(user)]
fn user_copy_rechecks_permissions_after_waiting_without_the_mm_lock() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::AdmissionReprotect(start),
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
    let result = osvm::write_vm_bytes(start.as_usize() as *mut u8, &[1]);
    let prefault = Vm::new().fault_in_writeable(start.as_usize(), 1);
    let copied = Vm::new().write_mem_nofault(start.as_usize(), &[1]);
    space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
    assert_eq!(result, Err(MemError::NoAccess));
    assert_eq!(prefault, Err(MemError::NoAccess));
    assert_eq!(copied, Ok(0));
    assert_eq!(backing.faults.load(Ordering::Relaxed), 1);
}

#[def_test(user)]
fn user_nofault_copy_defers_backing_admission_until_prefault() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::AdmissionWait,
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
    let mut vm = Vm::new();
    let copied = access_user_memory(|| {
        assert_eq!(
            atomic_cmpxchg_u32_nofault(start.as_usize(), 0, 1),
            Err(MemError::NoAccess)
        );
        let copied = vm.write_mem_nofault(start.as_usize(), &[0x5a]);
        assert!(curr.as_thread().is_accessing_user_memory());
        copied
    });
    assert_eq!(copied, Ok(0));
    assert!(!curr.as_thread().is_accessing_user_memory());
    assert_eq!(backing.faults.load(Ordering::Relaxed), 0);
    assert_eq!(vm.fault_in_writeable(start.as_usize(), 1), Ok(()));
    assert_eq!(
        backing.faults.load(Ordering::Relaxed),
        MAX_USER_PAGE_FAULT_ATTEMPTS * 2 + 1
    );
    assert_eq!(vm.write_mem_nofault(start.as_usize(), &[0x5a]), Ok(1));
    space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
}

#[def_test(user)]
fn user_nofault_copy_preserves_prefix_before_an_unmapped_page() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::Normal,
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
    let mut vm = Vm::new();
    vm.fault_in_writeable(start.as_usize(), 1).unwrap();
    let address = start.as_usize() + PAGE_SIZE_4K - 8;
    // Align the source to enter the unrolled bulk path with eight writable bytes.
    let input = [0x5a; 128 + 7];
    let alignment = input.as_ptr().align_offset(8);
    assert_eq!(
        vm.write_mem_nofault(address, &input[alignment..alignment + 128]),
        Ok(8)
    );
    assert_eq!(backing.faults.load(Ordering::Relaxed), 1);
    assert_eq!(
        vm.fault_in_writeable(address + 8, 1),
        Err(MemError::NoAccess)
    );
    let mut output = [MaybeUninit::uninit(); 8];
    vm.read_mem(address, &mut output).unwrap();
    space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
    for byte in output {
        // SAFETY: successful read_mem initialized every output byte.
        assert_eq!(unsafe { byte.assume_init() }, 0x5a);
    }
}

#[def_test(user)]
fn user_nofault_copy_balances_pagefault_depth_on_all_exits() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::Normal,
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
    let mut vm = Vm::new();
    vm.fault_in_writeable(start.as_usize(), 1).unwrap();

    assert_eq!(vm.write_mem_nofault(start.as_usize(), &[0x5a]), Ok(1));
    assert!(!curr.as_thread().pagefault_disabled());

    assert_eq!(vm.write_mem_nofault(start.as_usize(), &[]), Ok(0));
    assert!(!curr.as_thread().pagefault_disabled());

    assert_eq!(
        vm.write_mem_nofault(USER_SPACE_BASE - 1, &[0x5a]),
        Err(MemError::NoAccess)
    );
    assert!(!curr.as_thread().pagefault_disabled());

    space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
}

#[def_test(user)]
fn user_nofault_copy_fault_exit_balances_pagefault_depth() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::AdmissionWait,
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
    let mut vm = Vm::new();

    assert_eq!(vm.write_mem_nofault(start.as_usize(), &[0x5a]), Ok(0));
    assert!(!curr.as_thread().pagefault_disabled());

    space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
}

#[def_test(user)]
fn user_nofault_copy_nests_pagefault_depth() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::Normal,
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
    let mut vm = Vm::new();
    vm.fault_in_writeable(start.as_usize(), 1).unwrap();

    let thread = curr.as_thread();
    let outer = thread.pagefault_guard();
    assert!(thread.pagefault_disabled());
    assert_eq!(vm.write_mem_nofault(start.as_usize(), &[0x5a]), Ok(1));
    assert!(thread.pagefault_disabled());
    drop(outer);
    assert!(!thread.pagefault_disabled());

    space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
}

#[def_test(user)]
fn user_pagefault_guard_suppresses_trap_resolution_in_access_window() {
    let curr = current();
    let space = curr.as_thread().process().address_space().unwrap();
    let start = VirtAddr::from_usize(kaddr_layout::USER_HEAP_BASE + 0x1000_0000);
    let backing = Arc::new(SleepableBacking {
        pages: PrivateBackend::new(start, PageSize::Size4K),
        faults: AtomicUsize::new(0),
        behavior: FaultBehavior::Immediate,
    });
    let flags = MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE;
    backing.install(&mut space.lock(), start, PAGE_SIZE_4K, flags);
    let thread = curr.as_thread();

    // A nofault atomic suppresses trap-handler resolution even inside an access
    // window: the fault returns through the fixup instead of being resolved.
    let suppressed = access_user_memory(|| atomic_load_u32_nofault(start.as_usize()));
    assert_eq!(suppressed, Err(MemError::NoAccess));
    assert_eq!(backing.faults.load(Ordering::Relaxed), 0);
    assert!(!thread.pagefault_disabled());

    // The faultable variant keeps resolving the same page inside the window.
    let resolved = access_user_memory(|| atomic_load_u32(start.as_usize()));
    assert_eq!(resolved, Ok(0));
    assert_eq!(backing.faults.load(Ordering::Relaxed), 1);
    assert!(!thread.pagefault_disabled());

    space.lock().unmap(start, PAGE_SIZE_4K).unwrap();
}
