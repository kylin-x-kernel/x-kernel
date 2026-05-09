// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{LayoutConsts, UserLayoutConsts};

// x86_64 kernel VA layout:
//
//   [0xffff800000000000, 0xffff808000000000)  linear map (PML4[256], 512 GiB)
//   [0xffffff0000000000, 0xffffff0020000000)  iomap window (512 MiB, reserved slot)
//   [0xffffff8000000000, 0xffffffc000000000)  kernel image (PML4[511], 512 GiB)
//
// The gaps between these ranges stay reserved and match the existing boot
// assumptions about the linear-map and kernel-image slots.
pub const LAYOUT: LayoutConsts = LayoutConsts {
    pg_va_bits: 48,
    kernel_aspace_base: 0xffff_8000_0000_0000,
    kernel_aspace_size: 0x0000_7fff_ffff_f000,
    linear_map_vaddr: 0xffff_8000_0000_0000,
    linear_map_vsize: 0x0000_0080_0000_0000,
    page_offset: 0xffff_8000_0000_0000,
    iomap_vaddr: 0xffff_ff00_0000_0000,
    iomap_vsize: 0x0000_0000_2000_0000,
    kimage_vaddr: 0xffff_ff80_0000_0000,
    kimage_vsize: 0x0000_0080_0000_0000,
};

// x86_64 user VA layout (low canonical half):
//
//   0x0000_0000_0000_0000 ┄┄┐ (null guard)
//   0x0000_0000_0000_1000 ┄┄┘
//   0x0000_0000_0400_0000    ← USER_INTERP_BASE   (ELF interpreter)
//   0x0000_0000_4000_0000 ┄┐ ← USER_HEAP_BASE     (brk starts here)
//                            │   initial 64 KiB, can grow to 512 MiB
//   0x0000_0000_6000_1000 ┄┄┘ ← SIGNAL_TRAMPOLINE
//                            ·
//                            ·   (gap)
//                            ·
//   0x0000_7FFF_0000_0000 ┄┐ ← USER_STACK_TOP
//          ↑ 0x8_0000       │   USER_STACK_SIZE = 512 KiB
//   0x0000_7FFF_FFFF_F000 ┄┄┘ ← USER_SPACE_BASE + USER_SPACE_SIZE
pub const USER_LAYOUT: UserLayoutConsts = UserLayoutConsts {
    user_space_base: 0x1000,
    user_space_size: 0x7fff_ffff_f000,
    user_interp_base: 0x400_0000,
    user_heap_base: 0x4000_0000,
    user_heap_size: 0x1_0000,
    user_heap_size_max: 0x2000_0000,
    signal_trampoline: 0x6000_1000,
    user_stack_top: 0x7fff_0000_0000,
    user_stack_size: 0x8_0000,
};
