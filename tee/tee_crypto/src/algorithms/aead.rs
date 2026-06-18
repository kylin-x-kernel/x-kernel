// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AEAD cipher abstraction — AES-128/192/256-GCM, SM4-GCM, AES-CCM.

use alloc::vec::Vec;

use cipher::{InnerIvInit, KeyInit, StreamCipherCore, consts::U16};
use ghash::{GHash, universal_hash::UniversalHash};

use crate::error::{CryptoError, Result};

/// Trait for AEAD (Authenticated Encryption with Associated Data) ciphers.
pub trait Aead {
    /// Encrypt plaintext. Output = ciphertext || tag.
    fn encrypt(key: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>>;

    /// Decrypt ciphertext (which includes the tag).
    fn decrypt(key: &[u8], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>>;

    /// Required key size in bytes.
    fn key_size() -> usize;

    /// Nonce size in bytes (default: 12 for GCM).
    fn nonce_size() -> usize {
        12
    }

    /// Tag size in bytes (default: 16 for GCM).
    fn tag_size() -> usize {
        16
    }
}

/// Generic GCM encrypt for any block cipher with 16-byte blocks.
fn gcm_encrypt<C>(
    key: &[u8],
    nonce: &[u8],
    aad: &[u8],
    plaintext: &[u8],
    key_len: usize,
) -> Result<Vec<u8>>
where
    C: cipher::KeyInit + cipher::BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
{
    if key.len() != key_len {
        return Err(CryptoError::InvalidKey);
    }
    if nonce.is_empty() || nonce.len() > 16 {
        return Err(CryptoError::InvalidLength);
    }

    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let mut ghash_key = ghash::Key::default();
    cipher.encrypt_block(&mut ghash_key);
    let ghash = GHash::new(&ghash_key);
    let j0 = compute_gcm_j0(&ghash, nonce);

    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let mut ctr = ctr::CtrCore::<C, ctr::flavors::Ctr32BE>::inner_iv_init(cipher, &j0);
    let mut tag_mask = cipher::Array::<u8, U16>::default();
    ctr.write_keystream_block(&mut tag_mask);

    let mut ciphertext = plaintext.to_vec();
    ctr.apply_keystream_partial((&mut ciphertext[..]).into());

    let full_tag = compute_gcm_tag(&ghash, aad, &ciphertext, tag_mask);
    let mut output = ciphertext;
    output.extend_from_slice(&full_tag);
    Ok(output)
}

/// Generic GCM decrypt for any block cipher with 16-byte blocks.
fn gcm_decrypt<C>(
    key: &[u8],
    nonce: &[u8],
    aad: &[u8],
    ciphertext: &[u8],
    key_len: usize,
) -> Result<Vec<u8>>
where
    C: cipher::KeyInit + cipher::BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
{
    if key.len() != key_len {
        return Err(CryptoError::InvalidKey);
    }
    if nonce.is_empty() || nonce.len() > 16 {
        return Err(CryptoError::InvalidLength);
    }
    if ciphertext.len() < 16 {
        return Err(CryptoError::InvalidLength);
    }

    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let mut ghash_key = ghash::Key::default();
    cipher.encrypt_block(&mut ghash_key);
    let ghash = GHash::new(&ghash_key);
    let j0 = compute_gcm_j0(&ghash, nonce);

    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let mut ctr = ctr::CtrCore::<C, ctr::flavors::Ctr32BE>::inner_iv_init(cipher, &j0);
    let mut tag_mask = cipher::Array::<u8, U16>::default();
    ctr.write_keystream_block(&mut tag_mask);

    let (ct, tag) = ciphertext.split_at(ciphertext.len() - 16);
    let expected_tag = compute_gcm_tag(&ghash, aad, ct, tag_mask);

    use subtle::ConstantTimeEq;
    if expected_tag.ct_eq(tag).into() {
        let mut plaintext = ct.to_vec();
        ctr.apply_keystream_partial((&mut plaintext[..]).into());
        Ok(plaintext)
    } else {
        Err(CryptoError::VerificationFailed)
    }
}

macro_rules! impl_gcm_aead {
    ($wrapper:ident, $cipher:ty, $key_len:expr) => {
        impl Aead for $wrapper {
            fn encrypt(key: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
                gcm_encrypt::<$cipher>(key, nonce, aad, plaintext, $key_len)
            }

            fn decrypt(key: &[u8], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
                gcm_decrypt::<$cipher>(key, nonce, aad, ciphertext, $key_len)
            }

            fn key_size() -> usize {
                $key_len
            }
        }
    };
}

/// AES-128-GCM AEAD cipher.
pub struct Aes128GcmAead;

/// AES-192-GCM AEAD cipher.
pub struct Aes192GcmAead;

/// AES-256-GCM AEAD cipher.
pub struct Aes256GcmAead;

/// SM4-GCM AEAD cipher.
pub struct Sm4GcmAead;

impl_gcm_aead!(Aes128GcmAead, aes::Aes128, 16);
impl_gcm_aead!(Aes192GcmAead, aes::Aes192, 24);
impl_gcm_aead!(Aes256GcmAead, aes::Aes256, 32);
impl_gcm_aead!(Sm4GcmAead, sm4::Sm4, 16);

/// AES-128-CCM AEAD cipher.
pub struct Aes128CcmAead;

/// AES-256-CCM AEAD cipher.
pub struct Aes256CcmAead;

macro_rules! impl_ccm_aead {
    ($wrapper:ident, $cipher:ty, $key_len:expr) => {
        impl Aead for $wrapper {
            fn encrypt(key: &[u8], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
                ccm_encrypt::<$cipher>(key, nonce, aad, plaintext, 16)
            }

            fn decrypt(key: &[u8], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
                ccm_decrypt::<$cipher>(key, nonce, aad, ciphertext, 16)
            }

            fn key_size() -> usize {
                $key_len
            }
        }
    };
}

impl_ccm_aead!(Aes128CcmAead, aes::Aes128, 16);
impl_ccm_aead!(Aes256CcmAead, aes::Aes256, 32);

/// CCM encrypt with variable tag length (RFC 3610).
pub fn ccm_encrypt<
    C: cipher::KeyInit + cipher::BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
>(
    key: &[u8],
    nonce: &[u8],
    aad: &[u8],
    plaintext: &[u8],
    tag_len: usize,
) -> Result<Vec<u8>> {
    ccm_dispatch::<C>(key, nonce, aad, plaintext, tag_len, true)
}

/// CCM decrypt with variable tag length (RFC 3610).
pub fn ccm_decrypt<
    C: cipher::KeyInit + cipher::BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
>(
    key: &[u8],
    nonce: &[u8],
    aad: &[u8],
    ciphertext: &[u8],
    tag_len: usize,
) -> Result<Vec<u8>> {
    ccm_dispatch::<C>(key, nonce, aad, ciphertext, tag_len, false)
}

/// Dispatch CCM operation over all valid (tag_len, nonce_len) combinations.
///
/// The `ccm` crate requires const-generic tag and nonce sizes, so we dispatch
/// at runtime via a two-level match over all 7×7 = 49 valid combinations.
macro_rules! ccm_dispatch_nonce {
    (
        $key:expr, $nonce:expr, $aad:expr, $msg:expr, $encrypt:expr, $C:ty, $T:ty, $nonce_len:expr
    ) => {
        match $nonce_len {
            7 => ccm_inner::<$C, $T, ccm::consts::U7>($key, $nonce, $aad, $msg, $encrypt),
            8 => ccm_inner::<$C, $T, ccm::consts::U8>($key, $nonce, $aad, $msg, $encrypt),
            9 => ccm_inner::<$C, $T, ccm::consts::U9>($key, $nonce, $aad, $msg, $encrypt),
            10 => ccm_inner::<$C, $T, ccm::consts::U10>($key, $nonce, $aad, $msg, $encrypt),
            11 => ccm_inner::<$C, $T, ccm::consts::U11>($key, $nonce, $aad, $msg, $encrypt),
            12 => ccm_inner::<$C, $T, ccm::consts::U12>($key, $nonce, $aad, $msg, $encrypt),
            13 => ccm_inner::<$C, $T, ccm::consts::U13>($key, $nonce, $aad, $msg, $encrypt),
            _ => Err(CryptoError::InvalidLength),
        }
    };
}

macro_rules! ccm_dispatch {
    ($key:expr, $nonce:expr, $aad:expr, $msg:expr, $tag_len:expr, $encrypt:expr, $C:ty) => {{
        let nonce_len = $nonce.len();
        match $tag_len {
            4 => ccm_dispatch_nonce!(
                $key,
                $nonce,
                $aad,
                $msg,
                $encrypt,
                $C,
                ccm::consts::U4,
                nonce_len
            ),
            6 => ccm_dispatch_nonce!(
                $key,
                $nonce,
                $aad,
                $msg,
                $encrypt,
                $C,
                ccm::consts::U6,
                nonce_len
            ),
            8 => ccm_dispatch_nonce!(
                $key,
                $nonce,
                $aad,
                $msg,
                $encrypt,
                $C,
                ccm::consts::U8,
                nonce_len
            ),
            10 => ccm_dispatch_nonce!(
                $key,
                $nonce,
                $aad,
                $msg,
                $encrypt,
                $C,
                ccm::consts::U10,
                nonce_len
            ),
            12 => ccm_dispatch_nonce!(
                $key,
                $nonce,
                $aad,
                $msg,
                $encrypt,
                $C,
                ccm::consts::U12,
                nonce_len
            ),
            14 => ccm_dispatch_nonce!(
                $key,
                $nonce,
                $aad,
                $msg,
                $encrypt,
                $C,
                ccm::consts::U14,
                nonce_len
            ),
            16 => ccm_dispatch_nonce!(
                $key,
                $nonce,
                $aad,
                $msg,
                $encrypt,
                $C,
                ccm::consts::U16,
                nonce_len
            ),
            _ => Err(CryptoError::InvalidLength),
        }
    }};
}

/// Typed CCM encrypt/decrypt using the `ccm` crate.
fn ccm_inner<C, T, N>(
    key: &[u8],
    nonce: &[u8],
    aad: &[u8],
    msg: &[u8],
    encrypt: bool,
) -> Result<Vec<u8>>
where
    C: cipher::KeyInit + cipher::BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
    T: ccm::TagSize + aead::array::ArraySize,
    N: ccm::NonceSize + aead::array::ArraySize,
{
    type CcmType<C, T, N> = ccm::Ccm<C, T, N>;
    let ccm = CcmType::<C, T, N>::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let nonce: &cipher::Array<u8, N> =
        &cipher::Array::try_from(nonce).map_err(|_| CryptoError::InvalidLength)?;
    if encrypt {
        use aead::Aead;
        ccm.encrypt(nonce, aead::Payload { msg, aad })
            .map_err(|_| CryptoError::InternalError)
    } else {
        use aead::Aead;
        ccm.decrypt(nonce, aead::Payload { msg, aad })
            .map_err(|_| CryptoError::VerificationFailed)
    }
}

fn ccm_dispatch<
    C: cipher::KeyInit + cipher::BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
>(
    key: &[u8],
    nonce: &[u8],
    aad: &[u8],
    msg: &[u8],
    tag_len: usize,
    encrypt: bool,
) -> Result<Vec<u8>> {
    ccm_dispatch!(key, nonce, aad, msg, tag_len, encrypt, C)
}

/// Advance CTR by discarding keystream blocks to reach the desired byte offset.
pub(crate) fn advance_ctr<C>(
    ctr: &mut ctr::CtrCore<C, ctr::flavors::Ctr32BE>,
    byte_offset: usize,
) -> usize
where
    C: cipher::BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
{
    let full_blocks = byte_offset / 16;
    let mut dummy = cipher::Array::<u8, U16>::default();
    for _ in 0..full_blocks {
        ctr.write_keystream_block(&mut dummy);
    }
    byte_offset % 16
}

/// Build a GCM CTR stream cipher from key and nonce, optionally advancing past
/// `byte_offset` bytes of already-produced keystream.
///
/// Returns the CTR instance ready for encryption/decryption.
pub(crate) fn gcm_ctr_from_key<C>(
    key: &[u8],
    nonce: &[u8],
    byte_offset: usize,
) -> Result<ctr::CtrCore<C, ctr::flavors::Ctr32BE>>
where
    C: cipher::KeyInit + cipher::BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
{
    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let mut ghash_key = ghash::Key::default();
    cipher.encrypt_block(&mut ghash_key);
    let ghash = GHash::new(&ghash_key);
    let j0 = compute_gcm_j0(&ghash, nonce);

    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let mut ctr = ctr::CtrCore::<C, ctr::flavors::Ctr32BE>::inner_iv_init(cipher, &j0);
    ctr.write_keystream_block(&mut cipher::Array::<u8, U16>::default());
    if byte_offset > 0 {
        let _partial_offset = advance_ctr(&mut ctr, byte_offset);
    }
    Ok(ctr)
}

/// Compute GCM J0 (pre-counter block) per NIST SP 800-38D Section 8.2.
///
/// For 96-bit (12-byte) nonce: `J0 = nonce || 0^31 || 1`
/// For other lengths: `J0 = GHASH_H(nonce || 0^pad || 0^64 || [len(nonce)*8]^64)`
pub(crate) fn compute_gcm_j0(ghash: &GHash, nonce: &[u8]) -> cipher::Array<u8, U16> {
    if nonce.len() == 12 {
        // Fast path: 96-bit nonce
        let mut j0: cipher::Array<u8, U16> = Default::default();
        j0[..12].copy_from_slice(nonce);
        j0[15] = 1;
        j0
    } else {
        // General case: GHASH-based J0 for non-standard nonce sizes
        let mut ghash = ghash.clone();
        ghash.update_padded(nonce);
        let nonce_bits = (nonce.len() as u64) * 8;
        let mut len_block = ghash::Block::default();
        len_block[8..].copy_from_slice(&nonce_bits.to_be_bytes());
        ghash.update(&[len_block]);
        ghash.finalize()
    }
}

/// Compute GCM authentication tag.
pub(crate) fn compute_gcm_tag(
    ghash: &GHash,
    aad: &[u8],
    ciphertext: &[u8],
    tag_mask: cipher::Array<u8, U16>,
) -> [u8; 16] {
    let mut ghash = ghash.clone();
    ghash.update_padded(aad);
    ghash.update_padded(ciphertext);

    let aad_bits = (aad.len() as u64) * 8;
    let ct_bits = (ciphertext.len() as u64) * 8;
    let mut len_block = ghash::Block::default();
    len_block[..8].copy_from_slice(&aad_bits.to_be_bytes());
    len_block[8..].copy_from_slice(&ct_bits.to_be_bytes());
    ghash.update(&[len_block]);

    let mut tag = ghash.finalize();
    for (a, b) in tag.as_mut_slice().iter_mut().zip(tag_mask.as_slice()) {
        *a ^= *b;
    }

    let mut result = [0u8; 16];
    result.copy_from_slice(&tag);
    result
}
