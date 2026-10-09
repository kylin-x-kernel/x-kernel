// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(target_arch = "aarch64")]
pub(super) mod aarch64;
#[cfg(target_arch = "x86_64")]
pub(super) mod x86_64;
