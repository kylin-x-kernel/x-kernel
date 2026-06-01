// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Initializes trap handling on the current CPU.
///
/// In detail, it initializes the GDT, IDT on x86_64 platforms and relevant
/// model-specific registers to configure the handler for `syscall` instruction.
///
/// # Notes
/// Before calling this function, the initialization function of the [`percpu`]
/// crate should have been invoked to ensure that the per-CPU data structures
/// are set up correctly (i.e., by calling `khal::percpu::init_primary`).
///
/// [`percpu`]: https://docs.rs/percpu/latest/percpu/index.html
pub fn init_trap() {
    crate::userspace_common::init_exception_table();
    super::gdt::init();
    super::idt::init();
    super::userspace::init_syscall();
}
