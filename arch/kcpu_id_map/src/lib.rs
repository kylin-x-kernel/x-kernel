// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]

mod arch;
mod cpu_id;

pub use arch::init_boot_cpu_id_map;
pub use cpu_id::{KCpuMask, KCpuMaskExt, LogicalCpuId, LogicalCpuIdIter, RawCpuId};

use self::arch::imp;

pub fn raw_cpu_id(logical_cpu_id: LogicalCpuId) -> Option<RawCpuId> {
    imp::ensure_runtime_cpu_id_map();

    cpu_id::logical_to_raw(logical_cpu_id)
}

pub fn logical_cpu_id(raw_cpu_id: RawCpuId) -> Option<LogicalCpuId> {
    imp::ensure_runtime_cpu_id_map();

    let normalized_raw_cpu_id = imp::normalize_raw_id(raw_cpu_id);
    cpu_id::raw_to_logical(normalized_raw_cpu_id)
}

/// Invokes `f` for each present logical CPU.
///
/// The callback receives the zero-based present CPU index, the logical CPU id,
/// and the total number of present logical CPUs for this traversal.
pub fn for_each_present_logical_cpu(mut f: impl FnMut(usize, LogicalCpuId, usize)) {
    imp::ensure_runtime_cpu_id_map();

    cpu_id::for_each_present_logical_cpu(|present_index, logical_cpu_id, present_count| {
        f(present_index, logical_cpu_id, present_count);
    });
}

/// Returns the number of present CPUs discovered at runtime from the device
/// tree (AArch64/RISC-V/LoongArch) or ACPI MADT (x86_64), clamped to the
/// compile-time `NR_CPUS` cap.
///
/// This is the runtime counterpart to the compile-time `NR_CPUS` maximum: it
/// reflects how many CPUs the platform actually described, rather than the
/// static array bound. Before the CPU id map is initialized it returns 1, since
/// the boot CPU is always present.
pub fn nr_cpus() -> usize {
    imp::ensure_runtime_cpu_id_map();

    cpu_id::present_count().max(1)
}
