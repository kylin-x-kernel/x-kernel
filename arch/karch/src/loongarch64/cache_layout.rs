// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-owned SMP layout policy, not detected cache geometry.

// Use Linux LoongArch's default L1_CACHE_SHIFT=6 layout policy.
// Reference: https://github.com/torvalds/linux/blob/master/arch/loongarch/Kconfig
define_cache_alignment!(64);
