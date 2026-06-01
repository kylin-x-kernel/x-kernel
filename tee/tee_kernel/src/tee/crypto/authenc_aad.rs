// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buffered additional authenticated data for AEAD (OP-TEE semantics).
//!
//! mbedtls allows exactly one `cipher_update_ad` / `cipher_update_ad_ccm` per message.
//! The kernel accumulates `TEE_AEUpdateAAD` chunks and flushes once before payload
//! processing, matching OP-TEE's multi-call `crypto_authenc_update_aad` behavior.

use alloc::vec::Vec;

use mbedtls::cipher::raw::Cipher;
use tee_raw_sys::{
    TEE_ALG_AES_CCM, TEE_ALG_AES_GCM, TEE_ALG_SM4_CCM, TEE_ALG_SM4_GCM, TEE_ERROR_BAD_PARAMETERS,
    TEE_ERROR_BAD_STATE,
};

use crate::tee::TeeResult;

pub(crate) fn cipher_uses_authenc_aad_buffer(algo: u32) -> bool {
    matches!(
        algo,
        TEE_ALG_AES_GCM | TEE_ALG_SM4_GCM | TEE_ALG_AES_CCM | TEE_ALG_SM4_CCM
    )
}

fn cipher_uses_ccm(algo: u32) -> bool {
    matches!(algo, TEE_ALG_AES_CCM | TEE_ALG_SM4_CCM)
}

/// Accumulated AAD for one AE operation; flushed to mbedtls before the first payload byte.
pub(crate) struct TeeAuthencAadCtx {
    buffer: Vec<u8>,
    /// CCM: total AAD length passed to `TEE_AEInit` / `starts_ccm`.
    expected_len: Option<usize>,
    committed: bool,
    payload_started: bool,
    is_ccm: bool,
}

impl TeeAuthencAadCtx {
    pub(crate) fn new(algo: u32, aad_len: Option<usize>) -> Self {
        let is_ccm = cipher_uses_ccm(algo);
        Self {
            buffer: Vec::new(),
            expected_len: if is_ccm { aad_len } else { None },
            committed: false,
            payload_started: false,
            is_ccm,
        }
    }

    /// Append one `TEE_AEUpdateAAD` chunk (may be called multiple times before payload).
    pub(crate) fn append_aad(&mut self, data: &[u8]) -> TeeResult {
        if self.payload_started {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        if self.committed {
            return Err(TEE_ERROR_BAD_STATE);
        }
        if let Some(expected) = self.expected_len {
            let new_len = self
                .buffer
                .len()
                .checked_add(data.len())
                .ok_or(TEE_ERROR_BAD_PARAMETERS)?;
            if new_len > expected {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
        }
        self.buffer.extend_from_slice(data);
        Ok(())
    }

    /// Feed accumulated AAD to mbedtls (once). Called before the first payload update/final.
    pub(crate) fn flush_to_cipher(&mut self, cipher: &mut Cipher) -> TeeResult {
        if self.committed {
            return Ok(());
        }
        if self.is_ccm {
            let expected = self.expected_len.ok_or(TEE_ERROR_BAD_PARAMETERS)?;
            if self.buffer.len() != expected {
                return Err(TEE_ERROR_BAD_PARAMETERS);
            }
            cipher
                .update_ad_ccm(&self.buffer)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        } else if !self.buffer.is_empty() {
            cipher
                .update_ad(&self.buffer)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        }
        self.committed = true;
        Ok(())
    }

    /// Flush buffered AAD to mbedtls before the first payload byte (idempotent).
    pub(crate) fn enter_payload_phase(&mut self, cipher: &mut Cipher) -> TeeResult {
        if self.payload_started {
            return Ok(());
        }
        self.flush_to_cipher(cipher)?;
        self.payload_started = true;
        Ok(())
    }
}

impl Clone for TeeAuthencAadCtx {
    fn clone(&self) -> Self {
        Self {
            buffer: self.buffer.clone(),
            expected_len: self.expected_len,
            committed: self.committed,
            payload_started: self.payload_started,
            is_ccm: self.is_ccm,
        }
    }
}
