// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-owned SMP layout policy, not detected cache geometry.

// Use Linux arm64's generic SMP layout policy; DMA alignment is separate.
// Reference: https://github.com/torvalds/linux/blob/master/arch/arm64/include/asm/cache.h
define_cache_alignment!(64);
