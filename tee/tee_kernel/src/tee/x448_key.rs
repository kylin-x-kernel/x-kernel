// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared 56-byte Curve448 key material helpers (X448).

use core::mem::size_of;

use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_OVERFLOW};
use zeroize::Zeroize;

use super::{TeeResult, libutee::utee_defines::tee_u32_to_big_endian};

/// Fixed width for Curve448 / X448 key octet strings.
pub(super) const KEY_SIZE_BYTES_448: usize = 56;

/// `tee_cryp_obj` attribute ops table index for 56-byte Curve448 keys.
pub(super) const ATTR_OPS_INDEX_448: u32 = 4;

fn secure_zero(bytes: &mut [u8]) {
    // Volatile stores inside `Zeroize` keep the wipe from being deleted as a dead store.
    bytes.zeroize();
}

/// Serialize a u32 into `data`, advancing `*offs` by 4.
///
/// When `data` is empty or too short, skips the write but still advances `*offs`
/// so callers can probe the required size (see `tee_obj_attr_to_binary` with
/// `data = &mut []`). Overflow checks happen at the top level, not here.
fn u32_to_binary(v: u32, data: &mut [u8], offs: &mut usize) -> TeeResult {
    let next_offs = offs
        .checked_add(size_of::<u32>())
        .ok_or(TEE_ERROR_OVERFLOW)?;

    if data.len() >= next_offs {
        let field = tee_u32_to_big_endian(v);
        data[*offs..*offs + size_of::<u32>()].copy_from_slice(&field.to_ne_bytes());
    }
    *offs = next_offs;
    Ok(())
}

fn u32_from_binary(v: &mut u32, data: &[u8], offs: &mut usize) -> TeeResult {
    if data.len() < *offs + size_of::<u32>() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let field_bytes = &data[*offs..*offs + size_of::<u32>()];
    let field = u32::from_be_bytes(
        field_bytes
            .try_into()
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
    );
    *v = field;
    *offs += size_of::<u32>();
    Ok(())
}

pub(crate) fn key56_update_from_binary(
    key: &mut [u8; KEY_SIZE_BYTES_448],
    data: &[u8],
    offs: &mut usize,
) -> TeeResult {
    let mut len: u32 = 0;
    u32_from_binary(&mut len, data, offs)?;
    if *offs + len as usize > data.len() || len as usize > KEY_SIZE_BYTES_448 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    key[..len as usize].copy_from_slice(&data[*offs..*offs + len as usize]);
    if (len as usize) < KEY_SIZE_BYTES_448 {
        secure_zero(&mut key[len as usize..]);
    }
    *offs += len as usize;
    Ok(())
}

pub(crate) fn key56_to_binary(
    key: &[u8; KEY_SIZE_BYTES_448],
    data: &mut [u8],
    offs: &mut usize,
) -> TeeResult {
    u32_to_binary(KEY_SIZE_BYTES_448 as u32, data, offs)?;
    let next_offs = offs
        .checked_add(KEY_SIZE_BYTES_448)
        .ok_or(TEE_ERROR_OVERFLOW)?;
    if !data.is_empty() && next_offs <= data.len() {
        data[*offs..next_offs].copy_from_slice(key);
    }
    *offs = next_offs;
    Ok(())
}

pub(crate) fn key56_clear(key: &mut [u8; KEY_SIZE_BYTES_448]) {
    secure_zero(key);
}
