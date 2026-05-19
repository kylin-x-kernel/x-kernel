// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "aarch64")]
pub use self::aarch64::*;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::*;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use self::riscv64::*;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch64;

#[cfg(target_arch = "aarch64")]
pub(crate) use kcpu_id_map::CPU_ID_MAP;
pub(crate) use kcpu_id_map::init_boot_cpu_id_map;
pub use kcpu_id_map::{
    KCpuMask, KCpuMaskExt, LogicalCpuId, LogicalCpuIdIter, RawCpuId, for_each_present_logical_cpu,
    logical_cpu_id, raw_cpu_id,
};

#[cfg(target_arch = "loongarch64")]
pub use self::loongarch64::*;

// Provide a no-op fallback so callers can unconditionally call
// `kernel_boot::arch::set_secondary_boot_stack_top()` without `cfg`.
#[cfg(not(target_arch = "aarch64"))]
/// No-op on architectures that do not use a boot-time secondary stack table.
pub fn set_secondary_boot_stack_top(_cpu_id: LogicalCpuId, _stack_top_paddr: usize) {}
