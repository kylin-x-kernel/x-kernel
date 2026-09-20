// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-owned SMP layout policy, not detected cache geometry.

// Conservatively cover Linux ARM's 32/64/128-byte configuration choices.
// Reference: https://github.com/torvalds/linux/blob/master/arch/arm/mm/Kconfig
define_cache_alignment!(128);
