// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{Ext4Error, Ext4Result};

pub(super) fn is_bitmap_bit_set(bitmap: &[u8], bit_index: u32) -> Ext4Result<bool> {
    let byte = usize::try_from(bit_index / 8).map_err(|_| Ext4Error::Overflow)?;
    let mask = 1u8 << (bit_index % 8);
    Ok(bitmap.get(byte).ok_or(Ext4Error::OutOfBounds)? & mask != 0)
}

pub(super) fn set_bitmap_bit(bitmap: &mut [u8], bit_index: u32) -> Ext4Result<()> {
    let byte = usize::try_from(bit_index / 8).map_err(|_| Ext4Error::Overflow)?;
    let mask = 1u8 << (bit_index % 8);
    *bitmap.get_mut(byte).ok_or(Ext4Error::OutOfBounds)? |= mask;
    Ok(())
}

pub(super) fn clear_bitmap_bit(bitmap: &mut [u8], bit_index: u32) -> Ext4Result<()> {
    let byte = usize::try_from(bit_index / 8).map_err(|_| Ext4Error::Overflow)?;
    let mask = 1u8 << (bit_index % 8);
    *bitmap.get_mut(byte).ok_or(Ext4Error::OutOfBounds)? &= !mask;
    Ok(())
}
