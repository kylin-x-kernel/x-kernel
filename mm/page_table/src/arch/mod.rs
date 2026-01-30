// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::X64PageEntry as ArchPageEntry;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use self::aarch64::A64PageEntry as ArchPageEntry;

#[cfg(target_arch = "riscv64")]
pub mod riscv;
#[cfg(target_arch = "riscv64")]
pub use self::riscv::Rv64PageEntry as ArchPageEntry;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch64;
#[cfg(target_arch = "loongarch64")]
pub use self::loongarch64::La64PageEntry as ArchPageEntry;
