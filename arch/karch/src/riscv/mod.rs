// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V low-level architecture operations.

mod cpu;
mod irq;
mod mmu;
mod tls;
mod tlb;
mod trap;

pub use cpu::{await_interrupts, stop_cpu};
#[allow(deprecated)]
pub use irq::{
    disable_irq, disable_local_irq, enable_irq, enable_local_irq, irq_enabled, local_irq_enabled,
    restore_irq, save_irq_and_disable,
};
pub use mmu::{
    read_kernel_page_table, read_user_page_table, write_kernel_page_table, write_user_page_table,
};
pub use tls::{read_thread_pointer, write_thread_pointer};
pub use tlb::flush_tlb;
pub use trap::write_trap_vector_base;
