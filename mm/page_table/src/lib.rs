// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic page table abstractions and implementations for x-kernel.
//!
//! This crate provides a unified, multi-architecture page table framework.
//! Architecture-specific details (PTE format, TLB flush, address bit width)
//! are encapsulated in trait implementations, while the core mapping logic
//! is written once in [`PageTable64`].
//!
//! # Core types
//!
//! - [`PagingFlags`] — architecture-independent page permission and attribute flags.
//! - [`PageTableEntry`] — trait for architecture-specific PTE encoding/decoding.
//! - [`PagingMetaData`] — trait for paging metadata (levels, address bits, TLB flush).
//! - [`PagingHandler`] — trait for frame allocation and phys-to-virt translation.
//! - [`PageSize`] — supported page sizes (4K, 2M, 1G).
//! - [`PtError`] / [`PtResult`] — error type and `Result` alias.
//!
//! # Page table types
//!
//! - [`PageTable64`] — read-only page table (query only).
//! - [`PageTableMut`] — mutable page table access with deferred TLB flushes.
//!
//! # Architecture support
//!
//! | Architecture | PTE type | Levels | Type alias |
//! |-------------|----------|--------|------------|
//! | x86_64 | `X64PageEntry` | 4 | `X64PageTable` |
//! | AArch64 | `A64PageEntry` | 4 | `A64PageTable` |
//! | RISC-V Sv39 | `Rv64PageEntry` | 3 | `Sv39PageTable` |
//! | RISC-V Sv48 | `Rv64PageEntry` | 4 | `Sv48PageTable` |
//! | LoongArch64 | `La64PageEntry` | 4 | `LA64PageTable` |
//!
//! # Example
//!
//! ```ignore
//! use page_table::{PageTable64, PageTableMut, PagingFlags, PageSize, PtResult};
//! // Architecture-specific types are selected via cfg(target_arch).
//! // This example uses the x86_64 types for illustration.
//! use page_table::{X64PageTable, X64PageTableMut};
//!
//! fn example<H: PagingHandler>(pt: &mut X64PageTable<H>) -> PtResult {
//!     let mut m = pt.modify();
//!     m.map(vaddr, paddr, PageSize::Size4K, PagingFlags::READ | PagingFlags::WRITE)?;
//!     m.finish(); // or just drop `m`
//!     Ok(())
//! }
//! ```
//!
//! # Features
//!
//! - `smp` — enable SMP TLB shootdown via IPI.
//! - `copy-from` — enable `PageTableMut::copy_from` for fork support.
//! - `kerrno` — enable `From<PtError>` for `kerrno::KError`.

#![cfg_attr(not(test), no_std)]

#[macro_use]
mod macros;

mod arch;
mod defs;
mod table64;

pub use arch::*;
pub use defs::*;
pub use table64::*;
