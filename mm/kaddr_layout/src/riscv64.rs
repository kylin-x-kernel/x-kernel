// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{LayoutConsts, UserLayoutConsts};

// RISC-V Sv39 kernel VA layout:
//
//   [0xffffffc000000000, 0xffffffe000000000)  linear map
//   [0xffffffe000000000, 0xffffffe020000000)  kernel image (512 MiB)
//   [0xffffffe020000000, 0xffffffe040000000)  iomap window (512 MiB)
//   [0xffffffe040000000, 0xfffffffffffff000)  reserved
//
// This makes the high-half partition explicit instead of mixing the linked
// kernel image into the linear map.
pub const LAYOUT: LayoutConsts = LayoutConsts {
    pg_va_bits: 39,
    kernel_aspace_base: 0xffff_ffc0_0000_0000,
    kernel_aspace_size: 0x0000_003f_ffff_f000,
    linear_map_vaddr: 0xffff_ffc0_0000_0000,
    linear_map_vsize: 0x0000_0020_0000_0000,
    page_offset: 0xffff_ffc0_0000_0000,
    iomap_vaddr: 0xffff_ffe0_2000_0000,
    iomap_vsize: 0x0000_0000_2000_0000,
    kimage_vaddr: 0xffff_ffe0_0000_0000,
    kimage_vsize: 0x0000_0000_2000_0000,
};

// RISC-V Sv39 user VA layout (low half, 38-bit):
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
