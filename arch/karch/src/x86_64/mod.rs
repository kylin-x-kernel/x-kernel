// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 low-level architecture operations.

mod cache;
mod cpu;
mod hypercall;
mod irq;
mod mmu;
mod rng;
mod tlb;
mod tls;

pub use cache::{
    dma_read_barrier, flush_icache_all, flush_icache_all_local, flush_icache_range,
    flush_icache_remote,
};
pub use cpu::{await_interrupts, stop_cpu};
pub use hypercall::hypercall;
#[allow(deprecated)]
pub use irq::{
    disable_irq, disable_local_irq, enable_irq, enable_local_irq, irq_enabled, local_irq_enabled,
    restore_irq, save_irq_and_disable,
};
pub use mmu::{
    HwPageTableRoot, read_kernel_page_table, read_user_page_table, write_kernel_page_table,
    write_user_page_table,
};
pub use rng::{cpu_rng_available, init_cpu_rng, read_cpu_random};
pub use tlb::flush_tlb;
pub use tls::{read_thread_pointer, write_thread_pointer};
