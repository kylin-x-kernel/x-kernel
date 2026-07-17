// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 low-level architecture operations.

mod asid;
mod cache;
mod cpu;
mod fp;
mod irq;
mod mmu;
pub mod pmr;
mod tlb;
mod tls;
mod trap;

pub use asid::{
    USER_ASID_BITS, encode_user_page_table_root, user_asid_from_ttbr, user_page_table_root_paddr,
};
pub use cache::{
    clean_dcache_line_to_poc, clean_dcache_range_to_poc, flush_icache_all, flush_icache_all_local,
    flush_icache_range, flush_icache_remote,
};
pub use cpu::{await_interrupts, stop_cpu};
pub use fp::enable_fp;
#[allow(deprecated)]
pub use irq::{
    disable_irq, disable_local_irq, enable_irq, enable_local_irq, irq_enabled, local_irq_enabled,
    prepare_enter_user_irq, restore_irq, save_irq_and_disable,
};
pub use mmu::{
    HwPageTableRoot, read_kernel_page_table, read_user_page_table, write_kernel_page_table,
    write_user_page_table,
};
pub use tlb::{flush_tlb, flush_tlb_asid, flush_tlb_va_asid};
pub use tls::{read_thread_pointer, write_thread_pointer};
pub use trap::write_trap_vector_base;
