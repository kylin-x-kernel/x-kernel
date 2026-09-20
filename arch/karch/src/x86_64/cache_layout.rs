// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-owned SMP layout policy, not detected cache geometry.

// Use Linux's X86_64 default, not the distinct legacy x86-32 CPU policies.
// Reference: https://github.com/torvalds/linux/blob/master/arch/x86/Kconfig.cpu
define_cache_alignment!(64);
