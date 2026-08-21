// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch64 low-level architecture operations.

mod cache;
mod cpu;
mod fp;
mod irq;
mod mmu;
mod tlb;
mod tls;
mod trap;

pub use cache::{
    dma_read_barrier, flush_icache_all, flush_icache_all_local, flush_icache_range,
    flush_icache_remote,
};
pub use cpu::{await_interrupts, stop_cpu};
pub use fp::{enable_fp, enable_lsx};
#[allow(deprecated)]
pub use irq::{
    disable_irq, disable_local_irq, enable_irq, enable_local_irq, irq_enabled, local_irq_enabled,
    restore_irq, save_irq_and_disable,
};
pub use mmu::{
    HwPageTableRoot, encode_page_table_root, init_mmu, read_kernel_page_table,
    read_user_page_table, write_kernel_page_table, write_pwc, write_user_page_table,
};
pub use tlb::flush_tlb;
pub use tls::{read_thread_pointer, write_thread_pointer};
pub use trap::{init_trap_state, write_trap_vector_base};
