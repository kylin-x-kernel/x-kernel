// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Returns the link-time VMA offset of a hidden CPU-slot template symbol.
#[doc(hidden)]
#[macro_export]
macro_rules! __cpu_slot_symbol_offset {
    ($symbol:path) => {{
        let value: usize;
        cfg_select! {
            target_arch = "x86_64" => {
                // SAFETY: This only materializes the link-time offset; it does
                // not create a reference to the VMA-zero template symbol.
                unsafe {
                    core::arch::asm!(
                        "mov {0:e}, offset {VAR}",
                        out(reg) value,
                        VAR = sym $symbol,
                    );
                }
            }
            target_arch = "aarch64" => {
                // SAFETY: The assembler materializes the template's link-time offset.
                unsafe {
                    core::arch::asm!(
                        "movz {0}, #:abs_g0_nc:{VAR}",
                        out(reg) value,
                        VAR = sym $symbol,
                    );
                }
            }
            any(target_arch = "riscv32", target_arch = "riscv64") => {
                // SAFETY: The assembler materializes the template's link-time offset.
                unsafe {
                    core::arch::asm!(
                        "lui {0}, %hi({VAR})",
                        "addi {0}, {0}, %lo({VAR})",
                        out(reg) value,
                        VAR = sym $symbol,
                    );
                }
            }
            target_arch = "loongarch64" => {
                // SAFETY: The assembler materializes the template's link-time offset.
                unsafe {
                    core::arch::asm!(
                        "lu12i.w {0}, %abs_hi20({VAR})",
                        "ori {0}, {0}, %abs_lo12({VAR})",
                        out(reg) value,
                        VAR = sym $symbol,
                    );
                }
            }
            _ => {
                value = 0;
            }
        }
        value
    }};
}

/// Declares a typed immutable/mutable per-CPU slot template.
#[macro_export]
macro_rules! cpu_slot {
    ($(#[$meta:meta])* $vis:vis static $name:ident : $ty:ty = $value:expr $(;)?) => {
        #[allow(non_snake_case, dead_code)]
        mod $name {
            #[used]
            #[unsafe(link_section = ".cpu_slot.template")]
            pub static TEMPLATE: $ty = $value;

            #[inline]
            pub fn offset() -> usize {
                $crate::__cpu_slot_symbol_offset!(TEMPLATE)
            }
        }
        $(#[$meta])* #[allow(dead_code)] $vis static $name: $crate::CpuSlot<$ty> =
            // SAFETY: `$name::offset` is this module's own offset function for a
            // `.cpu_slot.template` symbol, so it satisfies `from_offset_fn`'s
            // precondition by construction.
            unsafe { $crate::CpuSlot::from_offset_fn($name::offset) };
    };
}

/// Declares an interior-mutable per-CPU slot template.
#[macro_export]
macro_rules! cpu_slot_cell {
    ($(#[$meta:meta])* $vis:vis static $name:ident : $ty:ty = $value:expr $(;)?) => {
        #[allow(non_snake_case, dead_code)]
        mod $name {
            #[used]
            #[unsafe(link_section = ".cpu_slot.template")]
            pub static TEMPLATE: $ty = $value;

            #[inline]
            pub fn offset() -> usize {
                $crate::__cpu_slot_symbol_offset!(TEMPLATE)
            }
        }
        $(#[$meta])* #[allow(dead_code)] $vis static $name: $crate::CpuSlotCell<$ty> =
            // SAFETY: `$name::offset` is this module's own offset function for a
            // `.cpu_slot.template` symbol, so it satisfies `from_offset_fn`'s
            // precondition by construction.
            unsafe { $crate::CpuSlotCell::from_offset_fn($name::offset) };
    };
}
