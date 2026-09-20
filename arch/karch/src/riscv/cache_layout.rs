// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-owned SMP layout policy, not detected cache geometry.

// Use Linux's generic RISC-V layout policy for both XLEN variants.
// Reference: https://github.com/torvalds/linux/blob/master/arch/riscv/include/asm/cache.h
define_cache_alignment!(64);
