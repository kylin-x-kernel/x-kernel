// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lightweight architecture-specific low-level operations.
//!
//! This crate provides a uniform API across all supported architectures for
//! TLB flush, cache maintenance, CPU halt, interrupt management, thread pointer
//! access, and FP/SIMD enable operations.
//!
#![doc = include_str!("../README.md")]
#![no_std]

cfg_if::cfg_if! {
    if #[cfg(target_arch = "x86_64")] {
        mod x86_64;
        pub use self::x86_64::*;
    } else if #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))] {
        mod riscv;
        pub use self::riscv::*;
    } else if #[cfg(target_arch = "aarch64")] {
        mod aarch64;
        pub use self::aarch64::*;
    } else if #[cfg(target_arch = "loongarch64")] {
        mod loongarch64;
        pub use self::loongarch64::*;
    } else if #[cfg(target_arch = "arm")] {
        mod arm;
        pub use self::arm::*;
    }
}
