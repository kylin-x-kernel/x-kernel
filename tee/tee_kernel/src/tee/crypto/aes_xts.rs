// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stateful AES-XTS (OP-TEE / LibTomCrypt semantics).
//!
//! mbedtls `cipher_update` for XTS recomputes the tweak from `ctx->iv` on every call, so
//! multi-part updates must advance tweak state in the kernel. This module uses two AES-ECB
//! contexts (data and tweak keys) without modifying rust-mbedtls.

use mbedtls::cipher::raw::{Cipher, CipherId, CipherMode, Operation};
use tee_raw_sys::TEE_ERROR_BAD_PARAMETERS;

use crate::tee::TeeResult;

const XTS_BLOCK_SIZE: usize = 16;

/// Running AES-XTS state carried across `cipher_update` / `cipher_final` syscalls.
pub(crate) struct AesXtsState {
    crypt_ecb: Cipher,
    tweak_ecb: Cipher,
    pub(crate) running_tweak: [u8; XTS_BLOCK_SIZE],
    decrypt: bool,
    /// Tweak applied to the most recently completed full block (before `gf128mul_x`).
    last_block_tweak: [u8; XTS_BLOCK_SIZE],
    /// Last full block output (ciphertext on encrypt, plaintext on decrypt).
    last_block: [u8; XTS_BLOCK_SIZE],
    /// Input to the last full block (`plaintext` on encrypt, `ciphertext` on decrypt).
    last_cipher_block: [u8; XTS_BLOCK_SIZE],
    /// Tweak for decrypt ciphertext stealing (`prev_tweak` in mbedtls).
    decrypt_steal_tweak: [u8; XTS_BLOCK_SIZE],
    has_last_block: bool,
    /// Input bytes already decrypted/encrypted before the current block.
    msg_offset: usize,
}

/// Per-syscall context: OP-TEE splits `cipher_update` + `cipher_final`; tweak advance for
/// ciphertext stealing must use the whole message length, not the current slice length.
pub(crate) struct AesXtsStream {
    /// Input bytes fed in prior syscalls (not including the current `input` slice).
    pub prior_bytes: usize,
    /// True on the final syscall (`cipher_final`, including its leading `update` of tail data).
    pub is_final: bool,
}

/// AES-XTS operation state carried in [`crate::tee::tee_svc_cryp2::TeeCipherCtx`].
///
/// Groups the kernel cipher state and syscall bookkeeping for incremental TA updates.
pub(crate) struct TeeCipherXtsCtx {
    pub state: AesXtsState,
    /// TA output base VA for patching the previous XTS block after a split `cipher_final`.
    pub user_base: Option<usize>,
    /// Total output bytes already written to the TA output buffer for this operation.
    pub emitted_bytes: usize,
    /// Replacement for the last full block when ciphertext stealing runs in a later syscall.
    pub patch_block: Option<[u8; XTS_BLOCK_SIZE]>,
    /// User VA of the last full block (for patch after a split `cipher_final`).
    pub patch_user_off: Option<usize>,
    /// Total input bytes fed across prior `cipher_update` / `cipher_final` syscalls.
    pub bytes_ingested: usize,
    /// Set while processing `cipher_final` (including its leading tail `update`).
    pub in_final_syscall: bool,
}

impl TeeCipherXtsCtx {
    pub(crate) fn new(state: AesXtsState) -> Self {
        Self {
            state,
            user_base: None,
            emitted_bytes: 0,
            patch_block: None,
            patch_user_off: None,
            bytes_ingested: 0,
            in_final_syscall: false,
        }
    }

    pub(crate) fn stream(&self) -> AesXtsStream {
        AesXtsStream {
            prior_bytes: self.bytes_ingested,
            is_final: self.in_final_syscall,
        }
    }

    pub(crate) fn final_stream(&self) -> AesXtsStream {
        AesXtsStream {
            prior_bytes: self.bytes_ingested,
            is_final: true,
        }
    }

    pub(crate) fn record_user_base_if_unset(&mut self, base: usize) {
        if self.user_base.is_none() {
            self.user_base = Some(base);
        }
    }

    pub(crate) fn after_update(&mut self, input_len: usize, output_len: usize) {
        self.bytes_ingested += input_len;
        self.emitted_bytes += output_len;
        self.patch_block = None;
        self.patch_user_off = None;
    }

    /// Record a ciphertext-stealing patch for a block written in an earlier syscall.
    pub(crate) fn record_patch_from_final(
        &mut self,
        patch: [u8; XTS_BLOCK_SIZE],
        output: &mut [u8],
    ) {
        if self.emitted_bytes < XTS_BLOCK_SIZE {
            return;
        }
        let patch_off = self.emitted_bytes - XTS_BLOCK_SIZE;
        if let Some(base) = self.user_base {
            self.patch_block = Some(patch);
            self.patch_user_off = Some(base + patch_off);
        } else if patch_off + XTS_BLOCK_SIZE <= output.len() {
            output[patch_off..patch_off + XTS_BLOCK_SIZE].copy_from_slice(&patch);
        }
    }

    /// If the patched block lies in this syscall's memref, update the kernel copy in place.
    pub(crate) fn merge_patch_into_slice(&self, dst_ptr: usize, dst: &mut [u8]) {
        let Some(patch) = self.patch_block.as_ref() else {
            return;
        };
        let Some(patch_va) = self.patch_user_off else {
            return;
        };
        if patch_va < dst_ptr || patch_va + patch.len() > dst_ptr + dst.len() {
            return;
        }
        let local_off = patch_va - dst_ptr;
        dst[local_off..local_off + patch.len()].copy_from_slice(patch);
    }

    /// Apply a deferred patch when the target block is outside the current memref (`patch_va` is a buffer offset).
    pub(crate) fn apply_patch_to_buffer(&mut self, out: &mut [u8]) {
        let Some(patch) = self.patch_block.take() else {
            return;
        };
        let Some(patch_va) = self.patch_user_off else {
            return;
        };
        if patch_va + patch.len() <= out.len() {
            out[patch_va..patch_va + patch.len()].copy_from_slice(&patch);
        }
    }
}

impl Clone for TeeCipherXtsCtx {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            user_base: self.user_base,
            emitted_bytes: self.emitted_bytes,
            patch_block: self.patch_block,
            patch_user_off: self.patch_user_off,
            bytes_ingested: self.bytes_ingested,
            in_final_syscall: self.in_final_syscall,
        }
    }
}

impl Clone for AesXtsState {
    fn clone(&self) -> Self {
        Self {
            crypt_ecb: self.crypt_ecb.clone(),
            tweak_ecb: self.tweak_ecb.clone(),
            running_tweak: self.running_tweak,
            decrypt: self.decrypt,
            last_block_tweak: self.last_block_tweak,
            last_block: self.last_block,
            last_cipher_block: self.last_cipher_block,
            decrypt_steal_tweak: self.decrypt_steal_tweak,
            has_last_block: self.has_last_block,
            msg_offset: self.msg_offset,
        }
    }
}

pub(crate) fn cipher_uses_aes_xts_kernel(algo: u32) -> bool {
    algo == tee_raw_sys::TEE_ALG_AES_XTS
}

fn split_xts_keys(key: &[u8]) -> Result<(&[u8], &[u8]), u32> {
    if key.len() < 32 || !key.len().is_multiple_of(2) {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    let half = key.len() / 2;
    Ok((&key[..half], &key[half..]))
}

fn ecb_crypt_block(cipher: &mut Cipher, decrypt: bool, block: &[u8], out: &mut [u8]) -> TeeResult {
    let n = if decrypt {
        cipher.decrypt(block, out)
    } else {
        cipher.encrypt(block, out)
    };
    n.map(|_| ()).map_err(|_| TEE_ERROR_BAD_PARAMETERS)
}

/// GF(2^128) multiply by x (mbedtls `mbedtls_gf128mul_x_ble`).
fn message_total_len(stream: &AesXtsStream, current_input_len: usize) -> Option<usize> {
    if stream.is_final {
        Some(stream.prior_bytes + current_input_len)
    } else {
        None
    }
}

/// mbedtls: `leftover && blocks == 0` on decrypt — advance tweak only before the last full
/// block of the entire message when `length % 16 != 0`.
fn decrypt_pre_advance_before_block(
    xts: &AesXtsState,
    stream: &AesXtsStream,
    current_input_len: usize,
) -> bool {
    if !xts.decrypt {
        return false;
    }
    let Some(total) = message_total_len(stream, current_input_len) else {
        return false;
    };
    let mo = total % XTS_BLOCK_SIZE;
    mo != 0 && xts.msg_offset + XTS_BLOCK_SIZE == total - mo
}

fn decrypt_pre_advance_for_stealing(xts: &mut AesXtsState) {
    xts.decrypt_steal_tweak = xts.running_tweak;
    let tweak = xts.running_tweak;
    gf128mul_x(&mut xts.running_tweak, &tweak);
}

fn gf128mul_x(r: &mut [u8; XTS_BLOCK_SIZE], x: &[u8; XTS_BLOCK_SIZE]) {
    let a = u64::from_le_bytes(x[0..8].try_into().unwrap());
    let b = u64::from_le_bytes(x[8..16].try_into().unwrap());
    let ra = (a << 1) ^ (0x0087u64 >> (8 - ((b >> 63) as u32) * 8));
    let rb = (a >> 63) | (b << 1);
    r[0..8].copy_from_slice(&ra.to_le_bytes());
    r[8..16].copy_from_slice(&rb.to_le_bytes());
}

pub(crate) fn aes_xts_init(
    key: &[u8],
    iv: Option<&[u8]>,
    decrypt: bool,
) -> Result<AesXtsState, u32> {
    let (key1, key2) = split_xts_keys(key)?;
    let key_bits = (key1.len() * 8) as u32;

    let mut crypt_ecb = Cipher::setup(CipherId::Aes, CipherMode::ECB, key_bits)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    let crypt_op = if decrypt {
        Operation::Decrypt
    } else {
        Operation::Encrypt
    };
    crypt_ecb
        .set_key(crypt_op, key1)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    let mut tweak_ecb = Cipher::setup(CipherId::Aes, CipherMode::ECB, key_bits)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    tweak_ecb
        .set_key(Operation::Encrypt, key2)
        .map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;

    let mut data_unit = [0u8; XTS_BLOCK_SIZE];
    if let Some(iv) = iv {
        if iv.len() != XTS_BLOCK_SIZE {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        data_unit.copy_from_slice(iv);
    }

    let mut running_tweak = [0u8; XTS_BLOCK_SIZE];
    ecb_crypt_block(&mut tweak_ecb, false, &data_unit, &mut running_tweak)?;

    Ok(AesXtsState {
        crypt_ecb,
        tweak_ecb,
        running_tweak,
        decrypt,
        last_block_tweak: running_tweak,
        last_block: [0u8; XTS_BLOCK_SIZE],
        last_cipher_block: [0u8; XTS_BLOCK_SIZE],
        decrypt_steal_tweak: running_tweak,
        has_last_block: false,
        msg_offset: 0,
    })
}

/// Ciphertext stealing tail (mbedtls `mbedtls_aes_crypt_xts` leftover path).
fn xts_ciphertext_stealing(
    xts: &mut AesXtsState,
    input_tail: &[u8],
    output: &mut [u8],
    written: usize,
    decrypt_t: Option<[u8; XTS_BLOCK_SIZE]>,
) -> Result<([u8; XTS_BLOCK_SIZE], usize), u32> {
    let leftover = input_tail.len();
    if leftover == 0 || leftover >= XTS_BLOCK_SIZE || !xts.has_last_block {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if output.len() < written + leftover {
        return Err(tee_raw_sys::TEE_ERROR_SHORT_BUFFER);
    }

    let t = if xts.decrypt {
        decrypt_t.unwrap_or(xts.decrypt_steal_tweak)
    } else {
        xts.running_tweak
    };

    let prev_out = &xts.last_block;

    let mut tmp = [0u8; XTS_BLOCK_SIZE];
    for i in 0..leftover {
        output[written + i] = prev_out[i];
        tmp[i] = input_tail[i] ^ t[i];
    }
    for i in leftover..XTS_BLOCK_SIZE {
        tmp[i] = prev_out[i] ^ t[i];
    }

    let mut block_out = [0u8; XTS_BLOCK_SIZE];
    ecb_crypt_block(&mut xts.crypt_ecb, xts.decrypt, &tmp, &mut block_out)?;

    let mut patch = [0u8; XTS_BLOCK_SIZE];
    for i in 0..XTS_BLOCK_SIZE {
        patch[i] = block_out[i] ^ t[i];
    }

    xts.last_block = patch;
    Ok((patch, written + leftover))
}

fn crypt_one_full_block(xts: &mut AesXtsState, input: &[u8], output: &mut [u8]) -> TeeResult {
    xts.last_cipher_block
        .copy_from_slice(&input[..XTS_BLOCK_SIZE]);
    let mut tmp = [0u8; XTS_BLOCK_SIZE];
    for i in 0..XTS_BLOCK_SIZE {
        tmp[i] = input[i] ^ xts.running_tweak[i];
    }
    xts.last_block_tweak = xts.running_tweak;
    ecb_crypt_block(&mut xts.crypt_ecb, xts.decrypt, &tmp, output)?;
    for (out, tweak) in output
        .iter_mut()
        .zip(xts.running_tweak.iter())
        .take(XTS_BLOCK_SIZE)
    {
        *out ^= tweak;
    }
    xts.last_block.copy_from_slice(&output[..XTS_BLOCK_SIZE]);
    xts.has_last_block = true;
    xts.msg_offset += XTS_BLOCK_SIZE;
    let tweak = xts.running_tweak;
    gf128mul_x(&mut xts.running_tweak, &tweak);
    Ok(())
}

/// Encrypt/decrypt `input` into `output`, updating `running_tweak` (may include ciphertext stealing).
pub(crate) fn aes_xts_crypt(
    xts: &mut AesXtsState,
    input: &[u8],
    output: &mut [u8],
) -> Result<usize, u32> {
    if input.len() < XTS_BLOCK_SIZE {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }
    if input.len() > (1 << 20) * XTS_BLOCK_SIZE {
        return Err(TEE_ERROR_BAD_PARAMETERS);
    }

    let blocks = input.len() / XTS_BLOCK_SIZE;
    let leftover = input.len() % XTS_BLOCK_SIZE;

    if leftover == 0 {
        for block in 0..blocks {
            let in_off = block * XTS_BLOCK_SIZE;
            let out_off = block * XTS_BLOCK_SIZE;
            crypt_one_full_block(
                xts,
                &input[in_off..in_off + XTS_BLOCK_SIZE],
                &mut output[out_off..out_off + XTS_BLOCK_SIZE],
            )?;
        }
        return Ok(blocks * XTS_BLOCK_SIZE);
    }

    let mut pending = [0u8; XTS_BLOCK_SIZE];
    let mut pending_len = 0usize;
    let stream = AesXtsStream {
        prior_bytes: 0,
        is_final: true,
    };
    let written =
        aes_xts_update_buffered(xts, &mut pending, &mut pending_len, input, output, &stream)?;

    let decrypt_t = if xts.decrypt && blocks > 0 {
        Some(xts.decrypt_steal_tweak)
    } else {
        None
    };
    let (patch, n) =
        xts_ciphertext_stealing(xts, &pending[..pending_len], output, written, decrypt_t)?;
    pending_len = 0;
    if written >= XTS_BLOCK_SIZE {
        output[written - XTS_BLOCK_SIZE..written].copy_from_slice(&patch);
    }
    Ok(n)
}

/// Flush a deferred decrypt full block (see `decrypt_defer_last_block_in_update`).
fn decrypt_flush_deferred_block(
    xts: &mut AesXtsState,
    pending: &[u8],
    stream: &AesXtsStream,
    current_input_len: usize,
    output: &mut [u8],
) -> TeeResult {
    if decrypt_pre_advance_before_block(xts, stream, current_input_len) {
        decrypt_pre_advance_for_stealing(xts);
    }
    crypt_one_full_block(xts, &pending[..XTS_BLOCK_SIZE], output)
}

/// On decrypt, hold the last full block of a non-final `cipher_update` until `cipher_final`.
fn decrypt_defer_last_block_in_update(
    xts: &AesXtsState,
    stream: &AesXtsStream,
    tail_after: usize,
    input_len: usize,
    in_pos: usize,
) -> bool {
    xts.decrypt && !stream.is_final && tail_after == 0 && in_pos + XTS_BLOCK_SIZE == input_len
}

/// Feed data through pending buffer; only full blocks are emitted.
pub(crate) fn aes_xts_update_buffered(
    xts: &mut AesXtsState,
    pending: &mut [u8],
    pending_len: &mut usize,
    input: &[u8],
    output: &mut [u8],
    stream: &AesXtsStream,
) -> Result<usize, u32> {
    let mut written = 0usize;
    let mut in_pos = 0usize;

    if xts.decrypt && *pending_len == XTS_BLOCK_SIZE && in_pos < input.len() {
        if output.len() < written + XTS_BLOCK_SIZE {
            return Err(tee_raw_sys::TEE_ERROR_SHORT_BUFFER);
        }
        decrypt_flush_deferred_block(
            xts,
            &pending[..XTS_BLOCK_SIZE],
            stream,
            input.len(),
            &mut output[written..],
        )?;
        written += XTS_BLOCK_SIZE;
        *pending_len = 0;
    }

    while *pending_len > 0 && *pending_len < XTS_BLOCK_SIZE && in_pos < input.len() {
        let need = XTS_BLOCK_SIZE - *pending_len;
        let take = core::cmp::min(need, input.len() - in_pos);
        pending[*pending_len..*pending_len + take].copy_from_slice(&input[in_pos..in_pos + take]);
        *pending_len += take;
        in_pos += take;

        if *pending_len == XTS_BLOCK_SIZE {
            if decrypt_pre_advance_before_block(xts, stream, input.len()) {
                decrypt_pre_advance_for_stealing(xts);
            }
            if output.len() < written + XTS_BLOCK_SIZE {
                return Err(tee_raw_sys::TEE_ERROR_SHORT_BUFFER);
            }
            crypt_one_full_block(xts, &pending[..XTS_BLOCK_SIZE], &mut output[written..])?;
            written += XTS_BLOCK_SIZE;
            *pending_len = 0;
        }
    }

    while in_pos + XTS_BLOCK_SIZE <= input.len() {
        let tail_after = input.len() - in_pos - XTS_BLOCK_SIZE;
        if decrypt_defer_last_block_in_update(xts, stream, tail_after, input.len(), in_pos) {
            pending[..XTS_BLOCK_SIZE].copy_from_slice(&input[in_pos..in_pos + XTS_BLOCK_SIZE]);
            *pending_len = XTS_BLOCK_SIZE;
            in_pos += XTS_BLOCK_SIZE;
            break;
        }
        if decrypt_pre_advance_before_block(xts, stream, input.len()) {
            decrypt_pre_advance_for_stealing(xts);
        }
        if output.len() < written + XTS_BLOCK_SIZE {
            return Err(tee_raw_sys::TEE_ERROR_SHORT_BUFFER);
        }
        crypt_one_full_block(
            xts,
            &input[in_pos..in_pos + XTS_BLOCK_SIZE],
            &mut output[written..],
        )?;
        written += XTS_BLOCK_SIZE;
        in_pos += XTS_BLOCK_SIZE;
    }

    let rem = input.len() - in_pos;
    if rem > 0 {
        if *pending_len + rem > XTS_BLOCK_SIZE {
            return Err(TEE_ERROR_BAD_PARAMETERS);
        }
        pending[*pending_len..*pending_len + rem].copy_from_slice(&input[in_pos..]);
        *pending_len += rem;
    }

    Ok(written)
}

/// Flush pending bytes plus optional final chunk (handles ciphertext stealing).
///
/// Returns `(bytes written into `output`, optional 16-byte patch for the previous full block)`.
/// The patch must be applied at the user output offset `emitted_total - 16` when the previous
/// block was written in an earlier syscall (libutee `CipherDoFinal` split).
pub(crate) fn aes_xts_final_buffered(
    xts: &mut AesXtsState,
    pending: &mut [u8],
    pending_len: &mut usize,
    input: &[u8],
    output: &mut [u8],
    stream: &AesXtsStream,
) -> Result<(usize, Option<[u8; XTS_BLOCK_SIZE]>), u32> {
    let written = aes_xts_update_buffered(xts, pending, pending_len, input, output, stream)?;

    if *pending_len == XTS_BLOCK_SIZE {
        if output.len() < written + XTS_BLOCK_SIZE {
            return Err(tee_raw_sys::TEE_ERROR_SHORT_BUFFER);
        }
        if decrypt_pre_advance_before_block(xts, stream, input.len()) {
            decrypt_pre_advance_for_stealing(xts);
        }
        crypt_one_full_block(xts, &pending[..XTS_BLOCK_SIZE], &mut output[written..])?;
        *pending_len = 0;
        return Ok((written + XTS_BLOCK_SIZE, None));
    }

    if *pending_len == 0 {
        return Ok((written, None));
    }

    let leftover = *pending_len;
    let tail = &pending[..leftover];
    let decrypt_t = if xts.decrypt {
        Some(xts.decrypt_steal_tweak)
    } else {
        None
    };
    let (patch, n) = xts_ciphertext_stealing(xts, tail, output, written, decrypt_t)?;
    *pending_len = 0;
    Ok((n, Some(patch)))
}
