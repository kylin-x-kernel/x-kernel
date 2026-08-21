// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::cmp;

use tee_crypto::{
    bignum::TeeBigNum,
    hash::{Digest, Sm3},
    hkdf,
    mac::{HmacSm3, Mac},
    material::{SignatureAlgorithm, SignatureBytes, SignatureEncoding},
    rng::DeterministicRng,
    sm2,
    tee_ops::ecc::{EccCurve, ecc_public_from_private},
};

use crate::cbor_cert_op::DiceResult;

const DICE_PRIVATE_KEY_BUFFER_SIZE: usize = 32;
const DICE_SIGNATURE_BUFFER_SIZE: usize = 64;
const DICE_PUBLIC_KEY_BUFFER_SIZE: usize = 64;
const DICE_HASH_SIZE: usize = 32;

const SM2_CURVE_ORDER: [u8; 32] = [
    0xFF, 0xFF, 0xFF, 0xFE, // FFFFFFFE
    0xFF, 0xFF, 0xFF, 0xFF, // FFFFFFFF
    0xFF, 0xFF, 0xFF, 0xFF, // FFFFFFFF
    0xFF, 0xFF, 0xFF, 0xFF, // FFFFFFFF
    0x72, 0x03, 0xDF, 0x6B, // 7203DF6B
    0x21, 0xC6, 0x05, 0x2B, // 21C6052B
    0x53, 0xBB, 0xF4, 0x09, // 53BBF409
    0x39, 0xD5, 0x41, 0x23, // 39D54123
];

pub fn hmac_rust(key: &[u8], input: &[u8; 64], output: &mut [u8], out_len: usize) -> i32 {
    let mut mac = match HmacSm3::new(key) {
        Ok(m) => m,
        Err(_) => return -1,
    };
    mac.update(input);
    let result = mac.finalize();
    let copy_len = cmp::min(out_len, result.len());
    if output.len() >= copy_len {
        output[..copy_len].copy_from_slice(&result[..copy_len]);
        0
    } else {
        -1
    }
}

pub fn hmac3_rust(
    key: &[u8],
    in1: &[u8; 64],
    in2: u8,
    in3: Option<&[u8]>,
    in3_len: usize,
    out: &mut [u8; 64],
) -> i32 {
    let mut combined_data = Vec::with_capacity(64 + 1 + in3_len);
    combined_data.extend_from_slice(in1);
    combined_data.push(in2);

    if let Some(data) = in3
        && in3_len > 0
    {
        let actual_len = cmp::min(in3_len, data.len());
        combined_data.extend_from_slice(&data[..actual_len]);
    }

    let mut mac = match HmacSm3::new(key) {
        Ok(m) => m,
        Err(_) => return -1,
    };
    mac.update(&combined_data);
    let result = mac.finalize();
    let copy_len = cmp::min(out.len(), result.len());
    out[..copy_len].copy_from_slice(&result[..copy_len]);
    0
}
fn derive_private_key_rust(seed: &[u8], private_key_len: usize) -> Result<[u8; 32], i32> {
    let mut v = [1u8; 64];
    let mut k = [0u8; 64];

    if private_key_len > 64 {
        return Err(-1);
    }

    let mut k_new = [0u8; 64];
    if hmac3_rust(&k, &v, 0x00, Some(seed), seed.len(), &mut k_new) != 0 {
        return Err(-1);
    }
    k = k_new;

    let mut temp_v = [0u8; 64];
    if hmac_rust(&k, &v, &mut temp_v, 64) != 0 {
        return Err(-1);
    }
    v.copy_from_slice(&temp_v);

    let mut k_new2 = [0u8; 64];
    if hmac3_rust(&k, &v, 0x01, Some(seed), seed.len(), &mut k_new2) != 0 {
        return Err(-1);
    }
    k = k_new2;

    let n = TeeBigNum::from_bytes(&SM2_CURVE_ORDER).map_err(|_| -1)?;

    loop {
        let mut k_new3 = [0u8; 64];
        if hmac_rust(&k, &v, &mut temp_v, 64) != 0 {
            return Err(-1);
        }
        v.copy_from_slice(&temp_v);

        if hmac_rust(&k, &v, &mut temp_v, 64) != 0 {
            return Err(-1);
        }
        v.copy_from_slice(&temp_v);

        let candidate = TeeBigNum::from_bytes(&v[..private_key_len]).map_err(|_| -1)?;

        if hmac3_rust(&k, &v, 0x00, None, 0, &mut k_new3) != 0 {
            return Err(-1);
        }
        k = k_new3;

        if candidate.compare(&n) == core::cmp::Ordering::Less && !candidate.is_zero() {
            let mut result = [0u8; 32];
            let bytes = candidate.to_bytes().map_err(|_| -1)?;
            if bytes.len() > 32 {
                continue;
            }
            let offset = 32 - bytes.len();
            result[offset..].copy_from_slice(&bytes);
            return Ok(result);
        }
    }
}
pub fn sm2_keypair_from_seed(
    public_key: &mut [u8; 64],
    private_key: &mut [u8; 32],
    seed: &[u8; 32],
) -> Result<(), i32> {
    let private_bytes = derive_private_key_rust(seed, 32)?;
    private_key.copy_from_slice(&private_bytes);

    let (pub_x, pub_y) = ecc_public_from_private(EccCurve::Sm2, &private_bytes).map_err(|_| -1)?;

    if pub_x.len() != 32 || pub_y.len() != 32 {
        return Err(-1);
    }
    public_key[..32].copy_from_slice(&pub_x);
    public_key[32..].copy_from_slice(&pub_y);

    Ok(())
}
pub fn sm2_sign(message: &[u8], private_key: &[u8; 32]) -> Result<[u8; 64], i32> {
    let (pub_x, pub_y) = ecc_public_from_private(EccCurve::Sm2, private_key).map_err(|_| -101)?;

    let mut public_key = [0u8; 64];
    public_key[..pub_x.len()].copy_from_slice(&pub_x);
    public_key[pub_x.len()..pub_x.len() + pub_y.len()].copy_from_slice(&pub_y);

    let prehash = sm2::sm2_compute_sign_digest(None, message, &public_key).map_err(|_| -102)?;

    let mut rng = DeterministicRng::seed_from_bytes(&[0u8; 32]);
    let sig = sm2::sm2_dsa_sign(private_key, &prehash, &mut rng).map_err(|_| -103)?;

    let sig_bytes = sig.as_bytes();
    if sig_bytes.len() != 64 {
        return Err(-104);
    }
    let mut result = [0u8; 64];
    result.copy_from_slice(sig_bytes);
    Ok(result)
}
pub fn sm2_verify(message: &[u8], signature: &[u8; 64], public_key: &[u8; 64]) -> Result<(), i32> {
    let prehash = sm2::sm2_compute_sign_digest(None, message, public_key).map_err(|_| -1)?;

    let sig = SignatureBytes::new(
        signature.to_vec(),
        SignatureAlgorithm::Sm2Dsa,
        SignatureEncoding::Raw,
    );

    sm2::sm2_dsa_verify(&public_key[..32], &public_key[32..], &prehash, &sig).map_err(|_| -1)?;

    Ok(())
}
pub fn dice_keypair_from_seed(seed: &[u8; 32]) -> Result<([u8; 64], [u8; 32]), DiceResult> {
    let mut public_key = [0u8; 64];
    let mut private_key = [0u8; 32];

    sm2_keypair_from_seed(&mut public_key, &mut private_key, seed)
        .map_err(|_| DiceResult::PlatformError(-1))?;

    Ok((public_key, private_key))
}
pub fn dice_sign(
    _context: &mut u8,
    message: &[u8],
    private_key: &[u8; DICE_PRIVATE_KEY_BUFFER_SIZE],
) -> Result<[u8; 64], i32> {
    let sig = sm2_sign(message, private_key)?;
    Ok(sig)
}
pub fn dice_verify(
    message: &[u8],
    signature: &[u8; DICE_SIGNATURE_BUFFER_SIZE],
    public_key: &[u8; DICE_PUBLIC_KEY_BUFFER_SIZE],
) -> Result<(), i32> {
    sm2_verify(message, signature, public_key)
}
pub fn dice_hash(
    _context: &mut u8,
    input: &[u8],
    output: &mut [u8; DICE_HASH_SIZE],
) -> Result<(), i32> {
    let mut hasher = Sm3::new();
    hasher.update(input);
    let result = hasher.finalize();
    let bytes = result.as_bytes();
    let copy_len = cmp::min(output.len(), bytes.len());
    output[..copy_len].copy_from_slice(&bytes[..copy_len]);
    Ok(())
}
pub fn dice_kdf(
    ikm: &[u8],
    salt: Option<&[u8]>,
    info: Option<&[u8]>,
    output: &mut [u8],
) -> Result<(), i32> {
    let salt = salt.unwrap_or(&[]);
    let info = info.unwrap_or(&[]);
    let result = hkdf::hkdf::<HmacSm3>(salt, ikm, info, output.len()).map_err(|_| -1)?;
    output.copy_from_slice(&result);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sm2_sign_verify_roundtrip() {
        let seed = [0x42u8; 32];
        let mut public_key = [0u8; 64];
        let mut private_key = [0u8; 32];

        sm2_keypair_from_seed(&mut public_key, &mut private_key, &seed)
            .expect("keypair generation failed");

        let message = b"hello sm2 sign verify test";
        let signature = sm2_sign(message, &private_key).expect("sign failed");
        assert_eq!(signature.len(), 64);

        sm2_verify(message, &signature, &public_key).expect("verify failed");
    }

    #[test]
    fn test_dice_hash() {
        let input = b"test hash data";
        let mut output = [0u8; 32];
        dice_hash(&mut 0, input, &mut output).expect("hash failed");
        assert!(output.iter().any(|&b| b != 0));
    }

    #[test]
    fn test_dice_kdf() {
        let ikm = [0x01u8; 32];
        let salt = [0x02u8; 16];
        let info = b"kdf info";
        let mut output = [0u8; 64];
        dice_kdf(&ikm, Some(&salt), Some(info), &mut output).expect("kdf failed");
        assert!(output.iter().any(|&b| b != 0));
    }
}
