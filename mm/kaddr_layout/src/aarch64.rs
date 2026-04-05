// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::LayoutConsts;

// AArch64 kernel VA layout (48-bit TTBR1 half):
//
//   [0xffff000000000000, 0xffff800000000000)  linear map
//   [0xffff800000000000, 0xffff800020000000)  kernel image (512 MiB)
//   [0xffff800020000000, 0xffff800040000000)  iomap window (512 MiB)
//   [0xffff800040000000, 0xfffffffffffff000)  reserved
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
