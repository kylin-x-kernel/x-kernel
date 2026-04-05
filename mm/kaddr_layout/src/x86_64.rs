// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::LayoutConsts;

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
