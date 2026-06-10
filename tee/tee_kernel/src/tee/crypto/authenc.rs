// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AEAD buffering (OP-TEE semantics).
//!
//! mbedtls multipart CCM/GCM only produce correct results when payload is fed in a
//! single `cipher_update` (after AAD). Incremental updates and `Cipher::clone` corrupt
//! shared state. We accumulate AAD/payload chunks and re-run AEAD from scratch on each
//! update/final, returning only bytes not yet delivered to the caller.
//!
//! GCM long nonces: OP-TEE accepts arbitrary nonce length (SP 800-38D GHASH derivation).
//! mbedtls `cipher_set_iv` rejects IV > 16 bytes before GCM mode can call `gcm_starts`; see
//! `cipher_gcm_set_nonce`.

use alloc::{vec, vec::Vec};

use mbedtls::cipher::raw::{Cipher, CipherId, CipherMode, Operation};
use mbedtls_sys_auto::{
    GCM_DECRYPT, GCM_ENCRYPT, MODE_GCM, cipher_context_t, gcm_context, gcm_starts,
};
use tee_raw_sys::{
    TEE_ALG_AES_CCM, TEE_ALG_AES_GCM, TEE_ALG_SM4_CCM, TEE_ALG_SM4_GCM, TEE_ERROR_BAD_PARAMETERS,
    TEE_ERROR_BAD_STATE, TEE_ERROR_MAC_INVALID, TEE_ERROR_SHORT_BUFFER, TEE_OperationMode,
};

use crate::tee::TeeResult;

const GCM_BLOCK_SIZE: usize = 16;
/// mbedtls `MBEDTLS_MAX_IV_LENGTH`; longer GCM nonces must use `gcm_starts` directly.
const MBEDTLS_MAX_IV_LENGTH: usize = 16;

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
struct TeeAuthencAadCtx {
    buffer: Vec<u8>,
    /// CCM: total AAD length passed to `TEE_AEInit` / `starts_ccm`.
    expected_len: Option<usize>,
    committed: bool,
    payload_started: bool,
    is_ccm: bool,
}

impl TeeAuthencAadCtx {
    fn new(algo: u32, aad_len: Option<usize>) -> Self {
        let is_ccm = cipher_uses_ccm(algo);
        Self {
            buffer: Vec::new(),
            expected_len: if is_ccm { aad_len } else { None },
            committed: false,
            payload_started: false,
            is_ccm,
        }
    }

    fn append_aad(&mut self, data: &[u8]) -> TeeResult {
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

    fn flush_to_cipher(&mut self, cipher: &mut Cipher) -> TeeResult {
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
        } else {
            cipher
                .update_ad(&self.buffer)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        }
        self.committed = true;
        Ok(())
    }

    fn aad_bytes(&self) -> &[u8] {
        &self.buffer
    }

    fn enter_payload_phase_buffered_only(&mut self) -> TeeResult {
        if self.payload_started {
            return Ok(());
        }
        self.payload_started = true;
        Ok(())
    }

    fn enter_payload_phase(&mut self, cipher: &mut Cipher) -> TeeResult {
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

/// CCM or GCM payload replay state.
enum TeeAuthencPayloadCtx {
    Ccm(TeeAuthencCcmCtx),
    Gcm(TeeAuthencGcmCtx),
}

impl TeeAuthencPayloadCtx {
    fn fresh_standby_cipher(&self, algo: u32, key: &[u8]) -> TeeResult<Cipher> {
        match self {
            Self::Ccm(ccm) => ccm.fresh_standby_cipher(algo, key),
            Self::Gcm(gcm) => gcm.fresh_standby_cipher(algo, key),
        }
    }

    fn update_payload(
        &mut self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> TeeResult<usize> {
        match self {
            Self::Ccm(ccm) => ccm.update_payload(algo, key, aad, input, output),
            Self::Gcm(gcm) => gcm.update_payload(algo, key, aad, input, output),
        }
    }

    fn enc_final(
        &mut self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        input: Option<&[u8]>,
        output: &mut [u8],
        tag: &mut [u8],
    ) -> TeeResult<usize> {
        match self {
            Self::Ccm(ccm) => ccm.enc_final(algo, key, aad, input, output, tag),
            Self::Gcm(gcm) => gcm.enc_final(algo, key, aad, input, output, tag),
        }
    }

    fn dec_final(
        &mut self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        input: Option<&[u8]>,
        output: &mut [u8],
        tag: &[u8],
    ) -> TeeResult<usize> {
        match self {
            Self::Ccm(ccm) => ccm.dec_final(algo, key, aad, input, output, tag),
            Self::Gcm(gcm) => gcm.dec_final(algo, key, aad, input, output, tag),
        }
    }
}

impl Clone for TeeAuthencPayloadCtx {
    fn clone(&self) -> Self {
        match self {
            Self::Ccm(ccm) => Self::Ccm(ccm.clone()),
            Self::Gcm(gcm) => Self::Gcm(gcm.clone()),
        }
    }
}

/// Buffered AAD and payload for one AEAD operation (CCM or GCM).
pub(crate) struct TeeAuthencCtx {
    aad: TeeAuthencAadCtx,
    payload: TeeAuthencPayloadCtx,
}

impl TeeAuthencCtx {
    pub(crate) fn new_ccm(
        algo: u32,
        nonce: &[u8],
        payload_len: usize,
        aad_len: usize,
        tag_len: usize,
        mode: TEE_OperationMode,
    ) -> Self {
        Self {
            aad: TeeAuthencAadCtx::new(algo, Some(aad_len)),
            payload: TeeAuthencPayloadCtx::Ccm(TeeAuthencCcmCtx::new(
                nonce,
                payload_len,
                aad_len,
                tag_len,
                mode,
            )),
        }
    }

    pub(crate) fn new_gcm(algo: u32, nonce: &[u8], mode: TEE_OperationMode) -> Self {
        Self {
            aad: TeeAuthencAadCtx::new(algo, None),
            payload: TeeAuthencPayloadCtx::Gcm(TeeAuthencGcmCtx::new(nonce, mode)),
        }
    }

    pub(crate) fn append_aad(&mut self, data: &[u8]) -> TeeResult {
        self.aad.append_aad(data)
    }

    pub(crate) fn aad_bytes(&self) -> &[u8] {
        self.aad.aad_bytes()
    }

    /// Enter payload phase: GCM replay feeds AAD itself; CCM flushes AAD to `cipher`.
    pub(crate) fn enter_payload_phase(&mut self, cipher: &mut Cipher) -> TeeResult {
        match &self.payload {
            TeeAuthencPayloadCtx::Gcm(_) => self.aad.enter_payload_phase_buffered_only(),
            TeeAuthencPayloadCtx::Ccm(_) => self.aad.enter_payload_phase(cipher),
        }
    }

    pub(crate) fn fresh_standby_cipher(&self, algo: u32, key: &[u8]) -> TeeResult<Cipher> {
        self.payload.fresh_standby_cipher(algo, key)
    }

    pub(crate) fn update_payload(
        &mut self,
        algo: u32,
        key: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> TeeResult<usize> {
        let aad = self.aad.aad_bytes();
        self.payload.update_payload(algo, key, aad, input, output)
    }

    pub(crate) fn enc_final(
        &mut self,
        algo: u32,
        key: &[u8],
        input: Option<&[u8]>,
        output: &mut [u8],
        tag: &mut [u8],
    ) -> TeeResult<usize> {
        let aad = self.aad.aad_bytes();
        self.payload.enc_final(algo, key, aad, input, output, tag)
    }

    pub(crate) fn dec_final(
        &mut self,
        algo: u32,
        key: &[u8],
        input: Option<&[u8]>,
        output: &mut [u8],
        tag: &[u8],
    ) -> TeeResult<usize> {
        let aad = self.aad.aad_bytes();
        self.payload.dec_final(algo, key, aad, input, output, tag)
    }
}

impl Clone for TeeAuthencCtx {
    fn clone(&self) -> Self {
        Self {
            aad: self.aad.clone(),
            payload: self.payload.clone(),
        }
    }
}

fn gcm_mode(op: Operation) -> TeeResult<i32> {
    match op {
        Operation::Encrypt => Ok(GCM_ENCRYPT),
        Operation::Decrypt => Ok(GCM_DECRYPT),
        Operation::None => Err(TEE_ERROR_BAD_PARAMETERS),
    }
}

unsafe fn cipher_gcm_context(cipher: &mut Cipher) -> TeeResult<*mut gcm_context> {
    let ctx = cipher as *mut Cipher as *mut cipher_context_t;
    let ctx = unsafe { &*ctx };
    if ctx.cipher_info.is_null() || unsafe { (*ctx.cipher_info).mode != MODE_GCM } {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    Ok(ctx.cipher_ctx as *mut gcm_context)
}

/// Prepare GCM after `set_key`: short nonces use `set_iv`; long nonces defer `gcm_starts` to
/// `cipher_gcm_update_ad` because mbedtls stores IV in a 16-byte `cipher_context_t::iv` buffer.
pub(crate) fn cipher_gcm_set_nonce(cipher: &mut Cipher, nonce: &[u8]) -> TeeResult {
    if nonce.is_empty() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if nonce.len() <= MBEDTLS_MAX_IV_LENGTH {
        cipher.set_iv(nonce).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    }
    cipher.reset().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    Ok(())
}

/// Feed AAD to GCM with OP-TEE semantics (including nonces longer than 16 bytes).
pub(crate) fn cipher_gcm_update_ad(
    cipher: &mut Cipher,
    op: Operation,
    nonce: &[u8],
    ad: &[u8],
) -> TeeResult {
    if nonce.is_empty() {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if nonce.len() <= MBEDTLS_MAX_IV_LENGTH {
        cipher.update_ad(ad).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        return Ok(());
    }
    let mode = gcm_mode(op)?;
    unsafe {
        let gcm = cipher_gcm_context(cipher)?;
        if gcm_starts(
            gcm,
            mode,
            nonce.as_ptr(),
            nonce.len(),
            ad.as_ptr(),
            ad.len(),
        ) != 0
        {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
    }
    Ok(())
}

pub(crate) fn cipher_uses_ccm_payload_buffer(algo: u32) -> bool {
    matches!(algo, TEE_ALG_AES_CCM | TEE_ALG_SM4_CCM)
}

pub(crate) fn cipher_uses_gcm_payload_buffer(algo: u32) -> bool {
    matches!(algo, TEE_ALG_AES_GCM | TEE_ALG_SM4_GCM)
}

pub(crate) fn cipher_uses_authenc_payload_buffer(algo: u32) -> bool {
    cipher_uses_ccm_payload_buffer(algo) || cipher_uses_gcm_payload_buffer(algo)
}

fn ccm_cipher_id(algo: u32) -> Option<CipherId> {
    match algo {
        TEE_ALG_AES_CCM => Some(CipherId::Aes),
        TEE_ALG_SM4_CCM => Some(CipherId::SM4),
        _ => None,
    }
}

fn gcm_cipher_id(algo: u32) -> Option<CipherId> {
    match algo {
        TEE_ALG_AES_GCM => Some(CipherId::Aes),
        TEE_ALG_SM4_GCM => Some(CipherId::SM4),
        _ => None,
    }
}

/// Buffered CCM payload for one AE operation.
pub(crate) struct TeeAuthencCcmCtx {
    nonce: Vec<u8>,
    payload_len: usize,
    aad_len: usize,
    tag_len: usize,
    encrypt: bool,
    buffer: Vec<u8>,
    emitted: usize,
}

impl TeeAuthencCcmCtx {
    pub(crate) fn new(
        nonce: &[u8],
        payload_len: usize,
        aad_len: usize,
        tag_len: usize,
        mode: TEE_OperationMode,
    ) -> Self {
        let encrypt = mode == TEE_OperationMode::TEE_MODE_ENCRYPT;
        Self {
            nonce: nonce.to_vec(),
            payload_len,
            aad_len,
            tag_len,
            encrypt,
            buffer: Vec::new(),
            emitted: 0,
        }
    }

    fn append(&mut self, data: &[u8]) -> TeeResult {
        let new_len = self
            .buffer
            .len()
            .checked_add(data.len())
            .ok_or(TEE_ERROR_BAD_PARAMETERS)?;
        if new_len > self.payload_len {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        self.buffer.extend_from_slice(data);
        Ok(())
    }

    /// Fresh mbedtls context after `starts_ccm` (no AAD/payload fed). Used for `TEE_CopyOperation`
    /// instead of `Cipher::clone`, which corrupts CCM multipart state.
    pub(crate) fn fresh_standby_cipher(&self, algo: u32, key: &[u8]) -> TeeResult<Cipher> {
        self.setup_cipher(algo, key)
    }

    fn setup_cipher(&self, algo: u32, key: &[u8]) -> TeeResult<Cipher> {
        let cipher_id = ccm_cipher_id(algo).ok_or(TEE_ERROR_BAD_PARAMETERS)?;
        let mut cipher = Cipher::setup(cipher_id, CipherMode::CCM, (key.len() * 8) as u32)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let op = if self.encrypt {
            Operation::Encrypt
        } else {
            Operation::Decrypt
        };
        cipher
            .set_key(op, key)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        cipher
            .set_iv(&self.nonce)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        cipher.reset().map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        cipher
            .starts_ccm(self.payload_len, self.aad_len, self.tag_len)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        Ok(cipher)
    }

    fn replay_process(
        &self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        scratch: &mut [u8],
    ) -> TeeResult<usize> {
        let mut cipher = self.setup_cipher(algo, key)?;
        cipher
            .update_ad_ccm(aad)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let n = cipher
            .update(&self.buffer, scratch)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        if n != self.buffer.len() {
            return Err(TEE_ERROR_BAD_STATE);
        }
        Ok(n)
    }

    pub(crate) fn update_payload(
        &mut self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> TeeResult<usize> {
        self.append(input)?;
        let mut scratch = vec![0u8; self.buffer.len() + GCM_BLOCK_SIZE];
        let n = self.replay_process(algo, key, aad, &mut scratch)?;
        let new_len = n - self.emitted;
        if output.len() < new_len {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        output[..new_len].copy_from_slice(&scratch[self.emitted..n]);
        self.emitted = n;
        Ok(new_len)
    }

    pub(crate) fn enc_final(
        &mut self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        input: Option<&[u8]>,
        output: &mut [u8],
        tag: &mut [u8],
    ) -> TeeResult<usize> {
        if let Some(data) = input {
            self.append(data)?;
        }
        if self.buffer.len() != self.payload_len {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        let mut scratch = vec![0u8; self.buffer.len() + GCM_BLOCK_SIZE];
        let mut cipher = self.setup_cipher(algo, key)?;
        cipher
            .update_ad_ccm(aad)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let n = cipher
            .update(&self.buffer, &mut scratch)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let tail_len = n - self.emitted;
        if output.len() < tail_len {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        if tail_len > 0 {
            output[..tail_len].copy_from_slice(&scratch[self.emitted..n]);
        }
        cipher
            .write_tag(tag)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        self.emitted = n;
        Ok(tail_len)
    }

    pub(crate) fn dec_final(
        &mut self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        input: Option<&[u8]>,
        output: &mut [u8],
        tag: &[u8],
    ) -> TeeResult<usize> {
        if let Some(data) = input {
            self.append(data)?;
        }
        if self.buffer.len() != self.payload_len {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        let mut scratch = vec![0u8; self.buffer.len() + GCM_BLOCK_SIZE];
        let mut cipher = self.setup_cipher(algo, key)?;
        cipher
            .update_ad_ccm(aad)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let n = cipher
            .update(&self.buffer, &mut scratch)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let tail_len = n - self.emitted;
        if output.len() < tail_len {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        if tail_len > 0 {
            output[..tail_len].copy_from_slice(&scratch[self.emitted..n]);
        }
        cipher.check_tag(tag).map_err(|_| TEE_ERROR_MAC_INVALID)?;
        self.emitted = n;
        Ok(tail_len)
    }
}

impl Clone for TeeAuthencCcmCtx {
    fn clone(&self) -> Self {
        Self {
            nonce: self.nonce.clone(),
            payload_len: self.payload_len,
            aad_len: self.aad_len,
            tag_len: self.tag_len,
            encrypt: self.encrypt,
            buffer: self.buffer.clone(),
            emitted: self.emitted,
        }
    }
}

/// Buffered GCM payload for one AE operation.
pub(crate) struct TeeAuthencGcmCtx {
    nonce: Vec<u8>,
    encrypt: bool,
    buffer: Vec<u8>,
    emitted: usize,
}

impl TeeAuthencGcmCtx {
    pub(crate) fn new(nonce: &[u8], mode: TEE_OperationMode) -> Self {
        let encrypt = mode == TEE_OperationMode::TEE_MODE_ENCRYPT;
        Self {
            nonce: nonce.to_vec(),
            encrypt,
            buffer: Vec::new(),
            emitted: 0,
        }
    }

    fn append(&mut self, data: &[u8]) -> TeeResult {
        self.buffer
            .len()
            .checked_add(data.len())
            .ok_or(TEE_ERROR_BAD_PARAMETERS)?;
        self.buffer.extend_from_slice(data);
        Ok(())
    }

    /// Fresh mbedtls GCM context (AAD/payload not fed). Used for `TEE_CopyOperation`.
    pub(crate) fn fresh_standby_cipher(&self, algo: u32, key: &[u8]) -> TeeResult<Cipher> {
        self.setup_cipher(algo, key)
    }

    fn setup_cipher(&self, algo: u32, key: &[u8]) -> TeeResult<Cipher> {
        let cipher_id = gcm_cipher_id(algo).ok_or(TEE_ERROR_BAD_PARAMETERS)?;
        let mut cipher = Cipher::setup(cipher_id, CipherMode::GCM, (key.len() * 8) as u32)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        let op = if self.encrypt {
            Operation::Encrypt
        } else {
            Operation::Decrypt
        };
        cipher
            .set_key(op, key)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        cipher_gcm_set_nonce(&mut cipher, &self.nonce)?;
        Ok(cipher)
    }

    fn gcm_operation(&self) -> Operation {
        if self.encrypt {
            Operation::Encrypt
        } else {
            Operation::Decrypt
        }
    }

    fn replay_process(
        &self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        scratch: &mut [u8],
    ) -> TeeResult<usize> {
        let mut cipher = self.setup_cipher(algo, key)?;
        cipher_gcm_update_ad(&mut cipher, self.gcm_operation(), &self.nonce, aad)?;
        if self.buffer.is_empty() {
            return Ok(0);
        }
        let n = cipher
            .update(&self.buffer, scratch)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        if n != self.buffer.len() {
            return Err(TEE_ERROR_BAD_STATE);
        }
        Ok(n)
    }

    pub(crate) fn update_payload(
        &mut self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        input: &[u8],
        output: &mut [u8],
    ) -> TeeResult<usize> {
        self.append(input)?;
        let mut scratch = vec![0u8; self.buffer.len() + GCM_BLOCK_SIZE];
        let n = self.replay_process(algo, key, aad, &mut scratch)?;
        let new_len = n - self.emitted;
        if output.len() < new_len {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        output[..new_len].copy_from_slice(&scratch[self.emitted..n]);
        self.emitted = n;
        Ok(new_len)
    }

    pub(crate) fn enc_final(
        &mut self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        input: Option<&[u8]>,
        output: &mut [u8],
        tag: &mut [u8],
    ) -> TeeResult<usize> {
        if let Some(data) = input {
            self.append(data)?;
        }
        let mut scratch = vec![0u8; self.buffer.len() + GCM_BLOCK_SIZE];
        let mut cipher = self.setup_cipher(algo, key)?;
        cipher_gcm_update_ad(&mut cipher, self.gcm_operation(), &self.nonce, aad)?;
        let n = if self.buffer.is_empty() {
            0
        } else {
            cipher
                .update(&self.buffer, &mut scratch)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?
        };
        let tail_len = n - self.emitted;
        if output.len() < tail_len {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        if tail_len > 0 {
            output[..tail_len].copy_from_slice(&scratch[self.emitted..n]);
        }
        cipher
            .write_tag(tag)
            .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
        self.emitted = n;
        Ok(tail_len)
    }

    pub(crate) fn dec_final(
        &mut self,
        algo: u32,
        key: &[u8],
        aad: &[u8],
        input: Option<&[u8]>,
        output: &mut [u8],
        tag: &[u8],
    ) -> TeeResult<usize> {
        if let Some(data) = input {
            self.append(data)?;
        }
        let mut scratch = vec![0u8; self.buffer.len() + GCM_BLOCK_SIZE];
        let mut cipher = self.setup_cipher(algo, key)?;
        cipher_gcm_update_ad(&mut cipher, self.gcm_operation(), &self.nonce, aad)?;
        let n = if self.buffer.is_empty() {
            0
        } else {
            cipher
                .update(&self.buffer, &mut scratch)
                .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?
        };
        let tail_len = n - self.emitted;
        if output.len() < tail_len {
            return Err(TEE_ERROR_SHORT_BUFFER);
        }
        if tail_len > 0 {
            output[..tail_len].copy_from_slice(&scratch[self.emitted..n]);
        }
        cipher.check_tag(tag).map_err(|_| TEE_ERROR_MAC_INVALID)?;
        self.emitted = n;
        Ok(tail_len)
    }
}

impl Clone for TeeAuthencGcmCtx {
    fn clone(&self) -> Self {
        Self {
            nonce: self.nonce.clone(),
            encrypt: self.encrypt,
            buffer: self.buffer.clone(),
            emitted: self.emitted,
        }
    }
}
