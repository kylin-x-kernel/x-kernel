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

/// Invokes `f` for each online logical CPU.
///
/// Unlike [`for_each_present_logical_cpu`], this skips CPUs that a hotplug
/// request took offline. Callbacks for work that must reach every usable CPU
/// (IPIs, TLB shootdowns) should use this traversal; firmware enumeration and
/// `/sys/devices/system/cpu/possible` must keep using the present traversal.
///
/// The callback receives the zero-based online CPU index, the logical CPU id,
/// and the total number of online logical CPUs for this traversal.
pub fn for_each_online_logical_cpu(mut f: impl FnMut(usize, LogicalCpuId, usize)) {
    imp::ensure_runtime_cpu_id_map();

    cpu_id::for_each_online_logical_cpu(|online_index, logical_cpu_id, online_count| {
        f(online_index, logical_cpu_id, online_count);
    });
}

/// Reports whether `logical_cpu_id` is present *and* online.
///
/// Returns `false` for a CPU that firmware never described, so callers do not
/// need a separate presence check before asking about online state.
pub fn is_cpu_online(logical_cpu_id: LogicalCpuId) -> bool {
    imp::ensure_runtime_cpu_id_map();

    cpu_id::is_online(logical_cpu_id)
}

/// Returns the number of online logical CPUs.
///
/// The boot CPU is always online, so this is never zero once the CPU id map
/// is initialized.
pub fn online_cpu_count() -> usize {
    imp::ensure_runtime_cpu_id_map();

    cpu_id::online_count()
}

/// Sets the online bit of a present logical CPU and returns its previous state.
///
/// Returns `None` when firmware never described the CPU; the caller decides
/// which errno that maps to. No policy is applied here: rejecting the boot CPU
/// or the last online CPU belongs to the hotplug state machine, which also owns
/// the hardware transition.
pub fn set_cpu_online(logical_cpu_id: LogicalCpuId, is_online: bool) -> Option<bool> {
    imp::ensure_runtime_cpu_id_map();

    cpu_id::set_online(logical_cpu_id, is_online)
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
