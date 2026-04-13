// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Optional verification of signed TA ELF images when loading from the VFS.

#[cfg(feature = "tee_ta_sign")]
use kerrno::{KError, KResult};
#[cfg(feature = "tee_ta_sign")]
use kfs::CachedFile;
#[cfg(feature = "tee_ta_sign")]
use log::{error, info};

#[cfg(feature = "tee_ta_sign")]
use crate::ta_ctx::TeeTaCtx;

/// When `exec_path` names a TA (`*.ta` with UUID basename), read the full image and verify `.ta_signature`.
#[cfg(feature = "tee_ta_sign")]
pub fn verify_ta_elf_signature_if_applicable(exec_path: &str, cache: &CachedFile) -> KResult<()> {
    if !TeeTaCtx::is_ta(exec_path) {
        return Ok(());
    }
    let len = cache.location().len().map_err(|_| KError::InvalidData)?;
    let len = usize::try_from(len).map_err(|_| KError::InvalidData)?;
    let mut image = alloc::vec![0u8; len];
    let n = cache
        .read_at(&mut image[..], 0)
        .map_err(|_| KError::InvalidData)?;
    if n != len {
        return Err(KError::InvalidData);
    }
    info!(
        "verify_elf_signature_with_limits: image.len: {}",
        image.len()
    );
    tasign::verify_elf_signature(image.as_slice(), None)
        .map_err(|_| KError::InvalidExecutable)
        .inspect_err(|err| {
            error!("verify ta elf signature failed: {:?}", err);
        })
}
