// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::LayoutConsts;

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
