// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub const SIZES: &[usize] = &[1024, 16 * 1024, 64 * 1024];

pub fn input(size: usize) -> Vec<u8> {
    (0..size)
        .map(|offset| offset.wrapping_mul(31).wrapping_add(17) as u8)
        .collect()
}
