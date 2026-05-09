// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{LayoutConsts, UserLayoutConsts};

// Generic fallback layout used only for architectures that do not yet have a
// dedicated address-space description in this crate.
pub const LAYOUT: LayoutConsts = LayoutConsts {
    pg_va_bits: 48,
    kernel_aspace_base: 0xffff_0000_0000_0000,
    kernel_aspace_size: 0x0000_ffff_ffff_f000,
    linear_map_vaddr: 0xffff_0000_0000_0000,
    linear_map_vsize: 0x0000_8000_0000_0000,
    page_offset: 0xffff_0000_0000_0000,
    iomap_vaddr: 0xffff_8000_2000_0000,
    iomap_vsize: 0x0000_0000_2000_0000,
    kimage_vaddr: 0xffff_8000_0000_0000,
    kimage_vsize: 0x0000_0000_2000_0000,
};

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
