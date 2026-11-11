// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Initializes trap handling on the current CPU.
///
/// In detail, it initializes the GDT, IDT on x86_64 platforms and relevant
/// model-specific registers to configure the handler for `syscall` instruction,
/// then publishes the current CPU identity and usable capability snapshot.
/// Call during CPU bring-up before scheduling; execution must not migrate.
///
/// # Notes
/// Before calling this function, the per-CPU slot area must have been published
/// so per-CPU data structures are set up correctly (i.e., by calling
/// `khal::percpu::init_primary`, which initializes `kcpu_slot`).
pub fn init_trap() {
    crate::userspace_common::init_exception_table();
    super::gdt::init();
    super::idt::init();
    super::userspace::init_syscall();
    super::xstate::init();
    super::cpuinfo::init_cpu_info();
}
