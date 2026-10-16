// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A kernel task must keep using a root independent of its creator's user MM.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::def_test;

#[def_test(serial)]
fn kernel_task_created_under_user_root_uses_permanent_kernel_root() {
    let user_mm = memspace::MmSpace::new_user_empty().unwrap();
    let user_root = user_mm.page_table_hw_root();
    let kernel_root: karch::HwPageTableRoot = memspace::kernel_page_table_root().into();
    #[cfg(any(
        target_arch = "riscv32",
        target_arch = "riscv64",
        target_arch = "x86_64"
    ))]
    let expected_saved_root = kernel_root;
    // Split-root architectures keep the kernel root active separately and
    // start a kernel task with the existing zero user-root default.
    #[cfg(any(target_arch = "aarch64", target_arch = "loongarch64"))]
    let expected_saved_root = karch::HwPageTableRoot::new(0);

    let observed_root = Arc::new(AtomicUsize::new(0));
    let worker_observed_root = observed_root.clone();

    // Keep the user MM alive and forbid scheduling while temporarily installing
    // its user root. The shared-root architectures also retain kernel mappings.
    // Restore hardware before making any assertions.
    let (task, active_root, saved_root) = {
        let _exclude = kspin::NoPreemptIrqSave::new();
        let previous_root = karch::read_user_page_table();
        // SAFETY: `user_mm` retains this root; `new_user_empty()` preserves
        // kernel mappings on shared-root architectures, while split-root
        // architectures retain their kernel root. The guard excludes switches.
        unsafe { karch::write_user_page_table(user_root) };
        karch::flush_tlb(None);

        let task = ktask::TaskInner::new_pidless_kthread(
            move || {
                worker_observed_root.store(
                    karch::read_kernel_page_table().as_usize(),
                    Ordering::Release,
                );
            },
            "kernel-root-probe".into(),
            16384,
        );
        let active_root = karch::read_user_page_table();
        #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
        let saved_root = task.ctx().satp;
        #[cfg(target_arch = "x86_64")]
        let saved_root = task.ctx().cr3;
        #[cfg(target_arch = "aarch64")]
        let saved_root = task.ctx().ttbr0_el1;
        #[cfg(target_arch = "loongarch64")]
        let saved_root = task.ctx().pgdl;

        // SAFETY: this restores the root active at guard entry; its owner has
        // remained alive throughout this interval with scheduling excluded.
        unsafe { karch::write_user_page_table(previous_root) };
        karch::flush_tlb(None);
        (task, active_root, saved_root)
    };

    unittest::ktest_println!(
        "kernel-root-probe: permanent={:#x} user={:#x} active={:#x} saved={:#x}",
        kernel_root.as_usize(),
        user_root.as_usize(),
        active_root.as_usize(),
        saved_root.as_usize(),
    );
    unittest::assert_ne!(user_root, kernel_root);
    unittest::assert_eq!(active_root, user_root);
    unittest::assert_ne!(saved_root, user_root);
    unittest::assert_eq!(saved_root, expected_saved_root);

    // Check the saved root before releasing the temporary user MM so an
    // unfixed kernel fails above without ever running on a freed page table.
    drop(user_mm);
    let task = ktask::spawn_task(task);
    unittest::assert_eq!(task.join(), 0);
    let running_root = observed_root.load(Ordering::Acquire);
    unittest::ktest_println!("kernel-root-probe: after MM release running={running_root:#x}");
    unittest::assert_eq!(running_root, kernel_root.as_usize());
}
