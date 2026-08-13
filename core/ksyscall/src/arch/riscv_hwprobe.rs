// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V `riscv_hwprobe` system-call ABI adapter.
//!
//! This module is the syscall-facing half of Linux `riscv_hwprobe`
//! (`arch/riscv/kernel/sys_hwprobe.c`). It owns only ABI plumbing: user-memory
//! copy-in/copy-out, cpuset parsing, and dispatch between the value-query and
//! `RISCV_HWPROBE_WHICH_CPUS` modes. Per-key values, cross-CPU aggregation and
//! per-CPU matching are delegated to [`kcpu`], the architecture capability
//! owner.

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use kerrno::{KError, KResult};
use osvm::VirtPtr;
use posix_types::{UserPtr, UserRead, UserWrite};

/// Selects the `WHICH_CPUS` query mode. Matches Linux `RISCV_HWPROBE_WHICH_CPUS`.
const RISCV_HWPROBE_WHICH_CPUS: u32 = 1 << 0;

/// A Linux `struct riscv_hwprobe { __s64 key; __u64 value; }` pair.
///
/// Internal ABI carrier for [`sys_riscv_hwprobe`]. Kept `pub(crate)` rather
/// than fully private because the `pub(crate)` syscall names it in its
/// signature, and `pub(crate)` keeps it out of the crate's external API: the
/// `pub use arch::*` re-export cannot widen it beyond the crate boundary.
#[repr(C)]
#[derive(Clone, Copy, Debug, UserRead, UserWrite)]
pub(crate) struct RiscvHwprobePair {
    key: i64,
    value: u64,
}

/// Queries RISC-V hardware properties using the Linux `riscv_hwprobe` ABI.
///
/// Without [`RISCV_HWPROBE_WHICH_CPUS`], each pair is filled with the value
/// aggregated across the requested CPU set (or all online CPUs when none is
/// supplied). With `RISCV_HWPROBE_WHICH_CPUS`, the pairs are treated as inputs
/// describing the requested values, and the CPU set is rewritten in place to
/// contain only the matching CPUs.
pub fn sys_riscv_hwprobe(
    pairs: UserPtr<RiscvHwprobePair>,
    pair_count: usize,
    cpusetsize: usize,
    cpus: UserPtr<u8>,
    flags: u32,
) -> KResult<isize> {
    if flags & !RISCV_HWPROBE_WHICH_CPUS != 0 {
        return Err(KError::InvalidInput);
    }

    if flags & RISCV_HWPROBE_WHICH_CPUS != 0 {
        hwprobe_get_cpus(pairs, pair_count, cpusetsize, cpus)?;
    } else {
        hwprobe_get_values(pairs, pair_count, cpusetsize, cpus)?;
    }
    Ok(0)
}

/// Fills each pair with the value aggregated across `cpus` (`flags == 0`).
///
/// Mirrors Linux `hwprobe_get_values`: `cpusetsize == 0 && cpus == NULL`
/// selects all online CPUs; any other CPU set is copied in (clamped to the
/// kernel cpumask size), intersected with the online CPUs, and rejected with
/// `EINVAL` when it contains no online CPU.
fn hwprobe_get_values(
    pairs: UserPtr<RiscvHwprobePair>,
    pair_count: usize,
    cpusetsize: usize,
    cpus: UserPtr<u8>,
) -> KResult<()> {
    let cpu_mask = load_values_cpuset(cpusetsize, cpus)?;

    // Stream pairs one at a time, matching Linux's per-pair get_user/put_user.
    // This avoids trusting the user-supplied `pair_count` with a bulk
    // allocation: an oversized count simply walks into unmapped memory and
    // returns `EFAULT`.
    for index in 0..pair_count {
        let slot = pair_slot(pairs, index);
        let mut pair = slot.read_vm()?;
        pair.value = 0;
        fill_hwprobe_pair(&mut pair, &cpu_mask);
        slot.write_vm(pair)?;
    }

    Ok(())
}

/// Rewrites the CPU set to keep only CPUs matching every pair
/// (`flags == RISCV_HWPROBE_WHICH_CPUS`).
///
/// Mirrors Linux `hwprobe_get_cpus`: an empty input CPU set means "all online
/// CPUs"; each known pair filters the set against the per-CPU value; an unknown
/// key marks that single pair as `key = -1, value = 0` and clears the whole
/// output set. Known pairs are passed through to userspace untouched.
fn hwprobe_get_cpus(
    pairs: UserPtr<RiscvHwprobePair>,
    pair_count: usize,
    cpusetsize: usize,
    cpus: UserPtr<u8>,
) -> KResult<()> {
    if cpusetsize == 0 || cpus.is_null() {
        return Err(KError::InvalidInput);
    }

    let online = all_present_cpu_mask();
    let mut cpu_mask = load_user_cpuset(cpusetsize, cpus)?;
    if cpu_mask.is_empty() {
        cpu_mask = online;
    } else {
        cpu_mask &= online;
    }

    let mut clear_all = false;
    for index in 0..pair_count {
        let slot = pair_slot(pairs, index);
        let mut pair = slot.read_vm()?;

        if !kcpu::hwprobe_key_is_known(pair.key) {
            // Unknown keys don't fail the call: report this pair back as
            // unrecognized, and clear the entire output set, matching Linux.
            clear_all = true;
            pair.key = kcpu::RISCV_HWPROBE_UNKNOWN_KEY;
            pair.value = 0;
            slot.write_vm(pair)?;
            continue;
        }

        if clear_all {
            continue;
        }

        filter_cpus_for_pair(&mut cpu_mask, pair.key, pair.value);
    }

    if clear_all {
        cpu_mask = ktask::KCpuMask::new();
    }

    write_user_cpuset(cpusetsize, cpus, &cpu_mask)
}

/// Loads the CPU mask for the value-query path.
fn load_values_cpuset(cpusetsize: usize, cpus: UserPtr<u8>) -> KResult<ktask::KCpuMask> {
    if cpusetsize == 0 && cpus.is_null() {
        return Ok(all_present_cpu_mask());
    }

    let online = all_present_cpu_mask();
    let mut mask = load_user_cpuset(cpusetsize, cpus)?;
    mask &= online;
    if mask.is_empty() {
        return Err(KError::InvalidInput);
    }
    Ok(mask)
}

/// Copies a user-supplied CPU set into a kernel mask, clamped to the kernel
/// cpumask size as Linux does. Bits at or beyond `NR_CPUS` are dropped by the
/// subsequent intersection with the online mask.
fn load_user_cpuset(cpusetsize: usize, cpus: UserPtr<u8>) -> KResult<ktask::KCpuMask> {
    let len = cpusetsize.min(cpumask_bytes());
    let bytes = cpus.load_vm_vec(len)?;

    // Only the bits actually present in the loaded bytes are meaningful; bound
    // the scan by `len * 8` (then NR_CPUS) so a short or zero-length cpuset
    // can never index out of bounds. An empty result is handled by the caller.
    let bit_limit = (len * 8).min(kbuild_config::NR_CPUS);
    let mut mask = ktask::KCpuMask::new();
    for cpu in 0..bit_limit {
        if bytes[cpu / 8] & (1 << (cpu % 8)) != 0 {
            mask.set(cpu, true);
        }
    }
    Ok(mask)
}

/// Writes a kernel CPU mask back to user memory, clamped to the kernel cpumask
/// size and the caller's `cpusetsize`.
fn write_user_cpuset(cpusetsize: usize, cpus: UserPtr<u8>, mask: &ktask::KCpuMask) -> KResult<()> {
    let len = cpusetsize.min(cpumask_bytes());
    let mut bytes = alloc::vec![0u8; len];

    // Bits beyond the caller's (clamped) buffer have nowhere to go; stop at
    // `len * 8` to avoid writing past the end of `bytes`.
    let bit_limit = (len * 8).min(kbuild_config::NR_CPUS);
    for cpu in 0..bit_limit {
        if mask.get(cpu) {
            bytes[cpu / 8] |= 1 << (cpu % 8);
        }
    }

    cpus.write_vm_slice(&bytes)?;
    Ok(())
}

/// Removes CPUs from `cpu_mask` whose per-CPU value for `key` does not match
/// the requested value, using the architecture owner's pure match semantics.
fn filter_cpus_for_pair(cpu_mask: &mut ktask::KCpuMask, key: i64, requested: u64) {
    for cpu in 0..kbuild_config::NR_CPUS {
        if !cpu_mask.get(cpu) {
            continue;
        }
        let matches =
            kcpu::hwprobe_cpu_matches(key, LogicalCpuId::new(cpu), requested).unwrap_or(false);
        if !matches {
            cpu_mask.set(cpu, false);
        }
    }
}

fn fill_hwprobe_pair(pair: &mut RiscvHwprobePair, cpu_mask: &ktask::KCpuMask) {
    match kcpu::hwprobe_aggregate_value(pair.key, cpu_mask) {
        Some(value) => pair.value = value,
        None => {
            // Matches Linux `hwprobe_get_values`: it calls `hwprobe_one_pair`,
            // whose `default` arm sets `pair->key = -1; pair->value = 0;`, then
            // writes both fields back with `put_user`. So the value-query path
            // (flags == 0) reports unknown keys as `key = -1, value = 0`, just
            // like the WHICH_CPUS path. The `key = -1` rewrite is NOT specific
            // to WHICH_CPUS.
            pair.key = kcpu::RISCV_HWPROBE_UNKNOWN_KEY;
            pair.value = 0;
        }
    }
}

/// Returns the `index`-th pair slot of a contiguous user array.
fn pair_slot(pairs: UserPtr<RiscvHwprobePair>, index: usize) -> UserPtr<RiscvHwprobePair> {
    let offset = index * core::mem::size_of::<RiscvHwprobePair>();
    UserPtr::from(pairs.as_ptr() as usize + offset)
}

/// Kernel CPU-mask size in bytes, matching Linux `cpumask_size()`.
const fn cpumask_bytes() -> usize {
    let bits_per_long = core::mem::size_of::<u64>() * 8;
    kbuild_config::NR_CPUS.div_ceil(bits_per_long) * core::mem::size_of::<u64>()
}

fn all_present_cpu_mask() -> ktask::KCpuMask {
    let mut mask = ktask::KCpuMask::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu_id, _| {
        mask.set_logical(cpu_id, true);
    });
    mask
}
