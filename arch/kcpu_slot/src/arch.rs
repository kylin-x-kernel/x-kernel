// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::arch::asm;

// RISC-V uses `gp` as the per-CPU base register, matching the existing
// `percpu 0.4` integration in this tree (see `percpu-0.4.0/src/imp.rs`).
// `tp` is not available here: the kernel's TLS implementation already uses it
// (`arch/khal/src/tls.rs`). On bare-metal kernels there is no active ABI global
// pointer discipline, so `gp` is free for this purpose; if that ever changes,
// the RISC-V branches below must be revisited together with the TLS mapping.
#[inline]
pub(crate) fn current_base() -> usize {
    let value: usize;
    // SAFETY: Reads the architecture's designated per-CPU base register.
    unsafe {
        cfg_select! {
            target_arch = "x86_64" => {
                asm!(
                    "mov {0}, qword ptr gs:[offset {SELF}]",
                    out(reg) value,
                    SELF = sym crate::layout::CPU_SLOT_SELF_PTR,
                    options(nostack, preserves_flags),
                );
            }
            target_arch = "aarch64" => {
                asm!("mrs {}, tpidr_el1", out(reg) value, options(nostack, preserves_flags));
            }
            any(target_arch = "riscv32", target_arch = "riscv64") => {
                asm!("mv {}, gp", out(reg) value, options(nostack, preserves_flags));
            }
            target_arch = "loongarch64" => {
                asm!("move {}, $r21", out(reg) value, options(nostack, preserves_flags));
            }
            not(test) => {
                compile_error!("kcpu_slot: unsupported architecture for per-CPU base register");
            }
            _ => {
                value = 0;
            }
        }
    }
    value
}

#[inline]
#[cfg(not(test))]
/// # Safety
/// `value` must be the unique, initialized per-CPU area for the current
/// CPU, and this must run during CPU setup before concurrent slot access.
pub(crate) unsafe fn set_current_base(value: usize) {
    // SAFETY: The caller has validated and exclusively owns the current CPU
    // area during early CPU setup; this writes only the architecture base and
    // its self-pointer slot.
    unsafe {
        cfg_select! {
            target_arch = "x86_64" => {
                let low = value as u32;
                let high = (value >> 32) as u32;
                asm!(
                    "wrmsr",
                    in("ecx") 0xC0000101u32,
                    in("eax") low,
                    in("edx") high,
                    options(nostack, preserves_flags),
                );
                asm!(
                    "mov qword ptr gs:[offset {SELF}], {base}",
                    SELF = sym crate::layout::CPU_SLOT_SELF_PTR,
                    base = in(reg) value,
                    options(nostack, preserves_flags),
                );
            }
            target_arch = "aarch64" => {
                asm!("msr tpidr_el1, {}", in(reg) value, options(nostack, preserves_flags));
            }
            any(target_arch = "riscv32", target_arch = "riscv64") => {
                asm!("mv gp, {}", in(reg) value, options(nostack, preserves_flags));
            }
            target_arch = "loongarch64" => {
                asm!("move $r21, {}", in(reg) value, options(nostack, preserves_flags));
            }
            _ => {
                compile_error!("kcpu_slot: unsupported architecture for per-CPU base register");
            }
        }
    }
}
