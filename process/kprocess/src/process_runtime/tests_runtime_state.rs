// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use khal::paging::{MappingFlags, PageSize};
use ksync::Mutex;
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use memspace::{MmSpace, VmRuntimeRef};
use unittest::{assert, assert_eq, def_test};

use super::{ProcessAddressSpace, ProcessRuntimeState};
use crate::AsThread;

#[def_test]
fn exit_snapshot_alone_pins_cleared_mm_without_resurrection() {
    let mm = mapped_mm(0x4000);
    let weak = Arc::downgrade(&mm);
    let state = ProcessRuntimeState::new(8_144, MmSpace::acquire_user(mm).unwrap(), 0);
    // Capture must not need the MM mutex, even when called under IRQ exclusion.
    let snapshot = {
        let owner = weak.upgrade().unwrap();
        let _mm_guard = owner.lock();
        let _irq_guard = kspin::NoPreemptIrqSave::new();
        state.capture_exit_address_space()
    };
    {
        let binding = state.address_space.lock();
        assert!(Arc::ptr_eq(
            snapshot._mm_pin.address_space(),
            binding.mm_pin.address_space(),
        ));
        assert!(Arc::ptr_eq(
            &snapshot.mm_cpu_residency,
            &binding.mm_cpu_residency,
        ));
        #[cfg(target_arch = "aarch64")]
        assert!(Arc::ptr_eq(
            &snapshot.user_asid_context,
            &binding.user_asid_context,
        ));
    }
    assert!(state.exit_mm());
    assert!(!state.exit_mm());
    assert!(state.address_space().is_none());
    drop(state);
    {
        let owner = weak
            .upgrade()
            .expect("snapshot is the only structural owner");
        assert_eq!(owner.lock().vmas().count(), 0);
        assert!(MmSpace::acquire_user(owner).is_none());
    }
    assert!(!snapshot._mm_pin.is_user_alive());
    assert!(snapshot._mm_pin.try_upgrade_user().is_none());
    assert!(ktask::is_sleepable_context());
    drop(snapshot);
    assert!(weak.upgrade().is_none());
}

#[def_test(serial)]
fn exit_thread_snapshot_stays_on_old_binding_until_task_drop() {
    let old_mm = mapped_mm(0x4000);
    let old_weak = Arc::downgrade(&old_mm);
    let old_residency = old_mm.lock().cpu_residency().clone();
    #[cfg(target_arch = "aarch64")]
    let old_asid = old_mm.lock().user_asid_context().unwrap().clone();
    let (process, task) = crate::tests::process_with_address_space(8_145, old_mm);
    let task = ktask::prepare_task(task);
    crate::publication::process_publication()
        .publish_task(&task)
        .unwrap();
    let thread = task.as_thread();
    let runtime = thread.process_runtime();
    let cpu = khal::percpu::this_cpu_id();
    // Exercise live residency before the real exit boundary installs a snapshot.
    thread.set_process_mm_resident_cpu(cpu);
    assert!(old_residency.snapshot().get(cpu.as_usize()));
    old_residency.clear_cpu(cpu);
    assert_eq!(process.thread_count(), 1);
    assert!(process.exit_thread(&task, 0));
    assert!(thread.has_exit_address_space());
    assert!(!thread.is_exiting());
    assert_eq!(process.thread_count(), 0);
    assert!(crate::lookup::task(thread.tid()).is_err());

    // These tasks never run. Simulate only the private binding mutation, twice;
    // this does not test a hardware handoff or a resumable fatal-exit schedule.
    for start in [0x8000, 0xc000] {
        let new_mm = mapped_mm(start);
        let new_residency = new_mm.lock().cpu_residency().clone();
        let mut replacement = ProcessAddressSpace::new(MmSpace::acquire_user(new_mm).unwrap(), 0);
        core::mem::swap(
            &mut *runtime.runtime_state.address_space.lock(),
            &mut replacement,
        );
        drop(replacement);
        assert!(process.exit_thread(&task, 0));
        thread.set_process_mm_resident_cpu(cpu);
        assert!(old_residency.snapshot().get(cpu.as_usize()));
        assert!(!new_residency.snapshot().get(cpu.as_usize()));
        // ASID preparation changes current-CPU allocator state even without
        // installing TTBR0. This never-run target must only inspect identity.
        assert!(thread.exit_address_space_matches(|snapshot| {
            let matches = Arc::ptr_eq(&snapshot.mm_cpu_residency, &old_residency);
            #[cfg(target_arch = "aarch64")]
            let matches = matches && Arc::ptr_eq(&snapshot.user_asid_context, &old_asid);
            matches
        }));
        old_residency.clear_cpu(cpu);
        let owner = old_weak
            .upgrade()
            .expect("thread snapshot must retain old MM");
        assert_eq!(owner.lock().vmas().count(), 0);
        assert!(MmSpace::acquire_user(owner).is_none());
    }
    crate::process_exit::finalize_process_exit(&process);
    crate::wait_reap::assert_reap_zombie_process(&process);
    drop(runtime);
    assert!(old_weak.upgrade().is_some());
    assert!(ktask::is_sleepable_context());
    drop(task);
    assert!(old_weak.upgrade().is_none());
}

fn mapped_mm(start: usize) -> Arc<Mutex<MmSpace>> {
    let start = VirtAddr::from_usize(start);
    let mut mm = MmSpace::new_empty_user(start, PAGE_SIZE_4K * 4).unwrap();
    mm.map(
        start,
        PAGE_SIZE_4K,
        MappingFlags::READ | MappingFlags::WRITE,
        false,
        VmRuntimeRef::new_anon_private(start, PageSize::Size4K),
    )
    .unwrap();
    Arc::new(Mutex::new(mm))
}

#[def_test]
fn captured_mm_and_heap_stay_paired_across_binding_replacement() {
    let old_mm = mapped_mm(0x4000);
    let new_mm = mapped_mm(0x8000);
    let old_id = old_mm.lock().mm_id();
    let new_id = new_mm.lock().mm_id();
    let state = ProcessRuntimeState::new(
        8_142,
        MmSpace::acquire_user(old_mm.clone()).unwrap(),
        0x5000,
    );
    let mut replacement = ProcessRuntimeState::new(
        8_143,
        MmSpace::acquire_user(new_mm.clone()).unwrap(),
        0x9000,
    );
    let old_view = state.address_space().unwrap();
    assert_eq!(old_view.lock().mm_id(), old_id);
    assert_eq!(old_view.heap_top(), 0x5000);
    state.set_heap_top(0x6000);
    assert_eq!(old_view.heap_top(), 0x6000);

    // These test-owned runtimes are never attached to tasks or hardware roots.
    // Model only binding replacement; do not drop displaced ownership under the lock.
    core::mem::swap(
        &mut *state.address_space.lock(),
        replacement.address_space.get_mut(),
    );
    let new_view = state.address_space().unwrap();
    assert_eq!(new_view.lock().mm_id(), new_id);
    assert_eq!(new_view.heap_top(), 0x9000);
    state.set_heap_top(0xa000);
    assert_eq!(new_view.heap_top(), 0xa000);
    assert_eq!(old_view.lock().mm_id(), old_id);
    assert_eq!(old_view.heap_top(), 0x6000);
    replacement.set_heap_top(0x7000);
    assert_eq!(old_view.heap_top(), 0x7000);
    assert_eq!(new_view.heap_top(), 0xa000);

    assert!(!replacement.exit_mm());
    assert!(replacement.address_space().is_none());
    drop(replacement);
    assert_eq!(old_view.lock().vmas().count(), 1);
    assert_eq!(old_view.heap_top(), 0x7000);
    drop(old_view);
    assert_eq!(old_mm.lock().vmas().count(), 0);
    assert_eq!(new_view.lock().vmas().count(), 1);

    assert!(!state.exit_mm());
    drop(state);
    assert_eq!(new_view.heap_top(), 0xa000);
    drop(new_view);
    assert_eq!(new_mm.lock().vmas().count(), 0);
}
