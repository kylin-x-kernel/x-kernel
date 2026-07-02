// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared 32-byte Curve25519 key material helpers (Ed25519 today, X25519 later).

use core::{mem::size_of, ptr::write_volatile};

use tee_raw_sys::{TEE_ERROR_BAD_PARAMETERS, TEE_ERROR_OVERFLOW};

use super::{TeeResult, libutee::utee_defines::tee_u32_to_big_endian};

/// Fixed width for Curve25519-family key octet strings (Ed25519 / X25519).
pub const KEY_SIZE_BYTES_25519: usize = 32;

/// `tee_cryp_obj` attribute ops table index for 32-byte curve25519 keys.
pub const ATTR_OPS_INDEX_25519: u32 = 3;

fn secure_zero(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a valid mutable reference; volatile stores inhibit
        // the compiler from eliding the wipe.
        unsafe {
            write_volatile(byte, 0);
        }
    }
}

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

pub(crate) fn key32_update_from_binary(
    key: &mut [u8; KEY_SIZE_BYTES_25519],
    data: &[u8],
    offs: &mut usize,
) -> TeeResult {
    let mut len: u32 = 0;
    u32_from_binary(&mut len, data, offs)?;
    if *offs + len as usize > data.len() || len as usize > KEY_SIZE_BYTES_25519 {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    key[..len as usize].copy_from_slice(&data[*offs..*offs + len as usize]);
    if (len as usize) < KEY_SIZE_BYTES_25519 {
        secure_zero(&mut key[len as usize..]);
    }
    *offs += len as usize;
    Ok(())
}

pub(crate) fn key32_to_binary(
    key: &[u8; KEY_SIZE_BYTES_25519],
    data: &mut [u8],
    offs: &mut usize,
) -> TeeResult {
    u32_to_binary(KEY_SIZE_BYTES_25519 as u32, data, offs)?;
    let next_offs = offs
        .checked_add(KEY_SIZE_BYTES_25519)
        .ok_or(TEE_ERROR_OVERFLOW)?;
    if !data.is_empty() && next_offs <= data.len() {
        data[*offs..next_offs].copy_from_slice(key);
    }
    *offs = next_offs;
    Ok(())
}

pub(crate) fn key32_clear(key: &mut [u8; KEY_SIZE_BYTES_25519]) {
    secure_zero(key);
}
