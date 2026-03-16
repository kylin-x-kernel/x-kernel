// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// This module defines constants related to virtual address space layout, page sizes, and provides
/// utilities for calculating page offsets and limits.
/// The constants are defined based on the architecture's virtual address space and page table
/// structure.
/// The module also calculates the virtual address ranges for kernel image and modules based on the
/// defined constants.
/// The page size is set to 4KB (1 << 12), and the virtual address space is defined as 48 bits,
/// which is common for 64-bit architectures with canonical form. The module also includes utilities for
/// calculating page offsets and limits based on the virtual address bits.
/// The constants defined in this module are used throughout the kernel for memory management,
/// paging, and virtual address calculations.

use crate::size_consts::*;

const MODULES_VADDR: usize = _page_end(PG_VA_BITS);

const VSIZE_P: usize = 0x10;

const MODULES_VSIZE: usize = (1usize << PG_VA_BITS) / VSIZE_P * 0x8;

pub const KIMAGE_VSIZE: usize = (1usize << PG_VA_BITS) / VSIZE_P;

pub const KIMAGE_VADDR: usize = MODULES_VADDR + MODULES_VSIZE;

pub const KLINER_OFFSET: usize = KIMAGE_VADDR + KIMAGE_VSIZE;

