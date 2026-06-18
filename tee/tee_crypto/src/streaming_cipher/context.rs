// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Streaming cipher context state machine and backend dispatch.

use alloc::vec::Vec;

use super::{
    algo::{Direction, PaddingMode, StreamingCipherAlgo},
    mode::{aead, block, ctr},
    padding::{pkcs7_pad, pkcs7_unpad},
};
use crate::error::{CryptoError, Result};

/// Streaming cipher context that buffers partial blocks.
/// Also handles AEAD algorithms (GCM/CCM) — the update() method dispatches
/// to streaming CTR for GCM or buffers for CCM, matching mbedtls's behavior.
#[derive(Clone)]
pub struct StreamingCipherCtx {
    pub(super) algo: StreamingCipherAlgo,
    pub(super) key: Vec<u8>,
    pub(super) iv: Vec<u8>,
    pub(super) buffer: Vec<u8>,
    pub(super) direction: Direction,
    pub(super) padding: PaddingMode,
    pub(super) stream_offset: usize,
    // AEAD-specific fields (unused for plain cipher)
    pub(super) aad: Vec<u8>,
    /// For GCM: accumulated ciphertext (encrypt) or ciphertext input (decrypt) for GHASH.
    pub(super) ciphertext: Vec<u8>,
    /// Decrypted plaintext accumulator (decrypt only).
    pub(super) plaintext: Vec<u8>,
    /// How many bytes have already been returned to the caller via update().
    pub(super) returned_len: usize,
    pub(super) tag_len: usize,
}

impl StreamingCipherCtx {
    pub fn new(
        algo: StreamingCipherAlgo,
        key: &[u8],
        iv: &[u8],
        direction: Direction,
        padding: PaddingMode,
    ) -> Result<Self> {
        let iv = if algo.is_ecb() {
            Vec::new()
        } else {
            iv.to_vec()
        };
        Ok(Self {
            algo,
            key: key.to_vec(),
            iv,
            buffer: Vec::new(),
            direction,
            padding,
            stream_offset: 0,
            aad: Vec::new(),
            ciphertext: Vec::new(),
            plaintext: Vec::new(),
            returned_len: 0,
            tag_len: 16,
        })
    }

    /// Create a new AEAD context (GCM/CCM).
    pub fn new_aead(
        algo: StreamingCipherAlgo,
        key: &[u8],
        nonce: &[u8],
        direction: Direction,
        tag_len: usize,
    ) -> Result<Self> {
        Ok(Self {
            algo,
            key: key.to_vec(),
            iv: nonce.to_vec(),
            buffer: Vec::new(),
            direction,
            padding: PaddingMode::None,
            stream_offset: 0,
            aad: Vec::new(),
            ciphertext: Vec::new(),
            plaintext: Vec::new(),
            returned_len: 0,
            tag_len,
        })
    }

    /// Append AAD (Additional Authenticated Data) for AEAD operations.
    pub fn update_aad(&mut self, aad: &[u8]) {
        self.aad.extend_from_slice(aad);
    }

    /// True once any payload has been fed via `update()` — AAD must come before payload
    /// for GCM/CCM, so callers reject late AAD when this returns true.
    pub fn payload_started(&self) -> bool {
        !self.ciphertext.is_empty() || !self.plaintext.is_empty()
    }

    /// Return the maximum number of bytes `update()` can emit for `input_len`.
    pub fn max_update_output_len(&self, input_len: usize) -> usize {
        if self.algo.is_aead() {
            if self.algo.is_ccm() {
                return 0;
            }
            return input_len;
        }
        if self.algo.is_ctr() {
            return input_len;
        }

        let bs = self.algo.block_size();
        let buffered_len = self.buffer.len().saturating_add(input_len);
        let keep = if self.direction.is_decrypting()
            && matches!(self.padding, PaddingMode::Pkcs7)
            && buffered_len >= bs
        {
            bs
        } else {
            0
        };
        buffered_len.saturating_sub(keep) / bs * bs
    }

    /// Feed data. Returns output for complete blocks (CTR: all input).
    /// For AEAD GCM: encrypts/decrypts immediately via CTR mode.
    /// For AEAD CCM: buffers data, returns empty.
    pub fn update(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        if self.algo.is_aead() {
            return aead::update(self, data);
        }
        if self.algo.is_ctr() {
            return ctr::process(self, data);
        }
        self.buffer.extend_from_slice(data);
        let bs = self.algo.block_size();
        let keep = if self.direction.is_decrypting()
            && matches!(self.padding, PaddingMode::Pkcs7)
            && self.buffer.len() >= bs
        {
            bs
        } else {
            0
        };
        let complete = self.buffer.len().saturating_sub(keep) / bs * bs;
        if complete == 0 {
            return Ok(Vec::new());
        }
        let chunk: Vec<u8> = self.buffer.drain(..complete).collect();
        block::process(self, &chunk)
    }

    /// Finalize. Handles padding for the last block.
    pub fn r#final(&mut self) -> Result<Vec<u8>> {
        if self.algo.is_aead() {
            // AEAD final is handled via encrypt_final / decrypt_final
            return Ok(Vec::new());
        }
        if self.algo.is_ctr() {
            let data = core::mem::take(&mut self.buffer);
            return ctr::process(self, &data);
        }

        let bs = self.algo.block_size();
        if self.direction.is_encrypting() {
            let mut data = core::mem::take(&mut self.buffer);
            if matches!(self.padding, PaddingMode::Pkcs7) {
                pkcs7_pad(&mut data, bs)?;
            }
            block::process(self, &data)
        } else {
            let data = core::mem::take(&mut self.buffer);
            let output = block::process(self, &data)?;
            if matches!(self.padding, PaddingMode::Pkcs7) {
                pkcs7_unpad(&output, bs)
            } else {
                Ok(output)
            }
        }
    }

    /// AEAD encrypt-final: returns (ciphertext, tag).
    pub fn encrypt_final(mut self) -> Result<(Vec<u8>, Vec<u8>)> {
        if self.algo.is_gcm() {
            let tag = aead::compute_tag(&self)?;
            let ct = core::mem::take(&mut self.ciphertext);
            let pending = ct[self.returned_len..].to_vec();
            Ok((pending, tag.to_vec()))
        } else {
            // CCM: one-shot
            let tag_len = self.tag_len;
            let ct_with_tag = aead::one_shot(self)?;
            if ct_with_tag.len() < tag_len {
                return Err(CryptoError::InvalidLength);
            }
            let split = ct_with_tag.len() - tag_len;
            let tag = ct_with_tag[split..].to_vec();
            let ct = ct_with_tag[..split].to_vec();
            Ok((ct, tag))
        }
    }

    /// Process optional final plaintext and return the final ciphertext and tag.
    pub fn encrypt_final_with_input(mut self, input: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut output = Vec::new();
        if let Some(input) = input
            && !input.is_empty()
        {
            output.extend(self.update(input)?);
        }
        let (tail, tag) = self.encrypt_final()?;
        output.extend(tail);
        Ok((output, tag))
    }

    /// AEAD decrypt-final with explicit tag verification.
    pub fn decrypt_final(mut self, tag: &[u8]) -> Result<Vec<u8>> {
        if self.algo.is_gcm() {
            let expected_tag = aead::compute_tag(&self)?;
            use subtle::ConstantTimeEq;
            if expected_tag.ct_eq(tag).into() {
                let pt = core::mem::take(&mut self.plaintext);
                Ok(pt[self.returned_len..].to_vec())
            } else {
                Err(CryptoError::VerificationFailed)
            }
        } else {
            // CCM: append tag and one-shot decrypt
            self.buffer.extend_from_slice(tag);
            aead::one_shot(self)
        }
    }

    /// Process optional final ciphertext, verify the tag, and return final plaintext.
    pub fn decrypt_final_with_input(mut self, input: Option<&[u8]>, tag: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        if let Some(input) = input
            && !input.is_empty()
        {
            output.extend(self.update(input)?);
        }
        output.extend(self.decrypt_final(tag)?);
        Ok(output)
    }
}
