// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Floating-point/SIMD operations for LoongArch64.

/// Enables floating-point instructions by setting `EUEN.FPE`.
///
/// - `EUEN`: <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#extended-component-unit-enable>
#[inline]
pub fn enable_fp() {
    loongArch64::register::euen::set_fpe(true);
}

/// Enables LSX extension by setting `EUEN.LSX`.
///
/// - `EUEN`: <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#extended-component-unit-enable>
#[inline]
pub fn enable_lsx() {
    loongArch64::register::euen::set_sxe(true);
}
