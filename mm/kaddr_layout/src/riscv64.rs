// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::LayoutConsts;

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
