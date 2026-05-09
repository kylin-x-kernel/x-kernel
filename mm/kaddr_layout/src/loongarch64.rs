// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{LayoutConsts, UserLayoutConsts};

// LoongArch64 kernel VA layout (48-bit canonical upper-half).
//
// LoongArch uses x86-like canonical addressing for 48-bit VAs, so the
// paged kernel address space must live in the sign-extended upper half
// starting at 0xffff_8000_0000_0000. A linear map rooted at
// 0xffff_0000_... is non-canonical and traps on access.
//
//   [0xffff800000000000, 0xffff808000000000)              linear map
//   [0xffffff0000200000, 0xffffff0020200000)              iomap window
//   [0xffffff8000200000, 0xffffff8020200000)              kernel image
//   [rest of kernel aspace]                               reserved
pub const LAYOUT: LayoutConsts = LayoutConsts {
    pg_va_bits: 48,
    kernel_aspace_base: 0xffff_8000_0000_0000,
    kernel_aspace_size: 0x0000_7fff_ffff_f000,
    linear_map_vaddr: 0xffff_8000_0000_0000,
    linear_map_vsize: 0x0000_0080_0000_0000,
    page_offset: 0xffff_8000_0000_0000,
    iomap_vaddr: 0xffff_ff00_0020_0000,
    iomap_vsize: 0x0000_0000_2000_0000,
    kimage_vaddr: 0xffff_ff80_0020_0000,
    kimage_vsize: 0x0000_0000_2000_0000,
};

// LoongArch64 user VA layout (low canonical half):
//
//   0x0000_0000_0000_1000 ┄┐ ← USER_SPACE_BASE
//   0x0000_0000_0400_0000   ← USER_INTERP_BASE
//   0x0000_0000_4000_0000 ┄┐ ← USER_HEAP_BASE
//                            │   grows up to USER_HEAP_SIZE_MAX (512 MiB)
//   0x0000_0000_6000_1000 ┄┄┘ ← SIGNAL_TRAMPOLINE
//                            ·
//   0x0000_0000_4_0000_0000┄┐ ← USER_STACK_TOP
//          ↑ 0x8_0000       │   USER_STACK_SIZE = 512 KiB
//   0x0000_003F_FFFF_F000 ┄┄┘ ← USER_SPACE_BASE + USER_SPACE_SIZE

pub const USER_LAYOUT: UserLayoutConsts = UserLayoutConsts {
    user_space_base: 0x1000,
    user_space_size: 0x3f_ffff_f000,
    user_interp_base: 0x400_0000,
    user_heap_base: 0x4000_0000,
    user_heap_size: 0x1_0000,
    user_heap_size_max: 0x2000_0000,
    signal_trampoline: 0x6000_1000,
    user_stack_top: 0x4_0000_0000,
    user_stack_size: 0x8_0000,
};
