// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{CorruptKind, Ext4Error, Ext4Result};

pub(crate) fn bytes<const N: usize>(input: &[u8], offset: usize) -> Ext4Result<[u8; N]> {
    let end = offset.checked_add(N).ok_or(Ext4Error::Overflow)?;
    let source = input
        .get(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    let mut value = [0; N];
    value.copy_from_slice(source);
    Ok(value)
}

pub(crate) fn le_u16(input: &[u8], offset: usize) -> Ext4Result<u16> {
    Ok(u16::from_le_bytes(bytes(input, offset)?))
}

pub(crate) fn le_u32(input: &[u8], offset: usize) -> Ext4Result<u32> {
    Ok(u32::from_le_bytes(bytes(input, offset)?))
}
