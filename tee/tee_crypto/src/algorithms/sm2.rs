// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SM2 implementation (DSA, PKE, KEP) using the `sm2` crate.
//!
//! Supports:
//! - SM2 DSA: digital signature (sign/verify with SM3)
//! - SM2 PKE: public key encryption/decryption (encrypt/decrypt with SM3)
//! - SM2 KEP: key exchange protocol (GB/T 32918.3 / GM/T 0003.3)

use alloc::vec::Vec;

use elliptic_curve::{
    Group,
    bigint::U256,
    point::AffineCoordinates,
    sec1::{FromSec1Point, ToSec1Point},
    subtle::CtOption,
};
use signature::hazmat::{PrehashSigner, PrehashVerifier};
use sm2::{AffinePoint, ProjectivePoint, Scalar};
use sm3::{Digest, Sm3};

use crate::{
    asymmetric::{
        Decryptor, Encryptor, Keypair, PublicKeyComponents, Signer, Sm2PublicPoint, Verifier,
    },
    bytes::PlaintextBytes,
    error::{CryptoError, Result},
    material::{
        CiphertextAlgorithm, CiphertextBytes, SignatureAlgorithm, SignatureBytes, SignatureEncoding,
    },
    rng::{CryptoRng, RngAdapter},
};

/// Default distinguishing identifier for SM2 DSA.
const DEFAULT_DISTID: &str = "1234567812345678";

/// Default distinguishing identifier for SM2 DSA (tasign / GmSSL compatible name).
pub const DEFAULT_DISTINGUISHING_ID: &str = DEFAULT_DISTID;

/// GM/T 0003 recommended curve parameters for ZA = SM3(ENTLA || ID || a || b || Gx || Gy || Px || Py).
const SM2_EQUATION_A: [u8; 32] = [
    0xff, 0xff, 0xff, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfc,
];
const SM2_EQUATION_B: [u8; 32] = [
    0x28, 0xe9, 0xfa, 0x9e, 0x9d, 0x9f, 0x5e, 0x34, 0x4d, 0x5a, 0x9e, 0x4b, 0xcf, 0x65, 0x09, 0xa7,
    0xf3, 0x97, 0x89, 0xf5, 0x15, 0xab, 0x8f, 0x92, 0xdd, 0xbc, 0xbd, 0x41, 0x4d, 0x94, 0x0e, 0x93,
];
const SM2_GENERATOR_X: [u8; 32] = [
    0x32, 0xc4, 0xae, 0x2c, 0x1f, 0x19, 0x81, 0x19, 0x5f, 0x99, 0x04, 0x46, 0x6a, 0x39, 0xc9, 0x94,
    0x8f, 0xe3, 0x0b, 0xbf, 0xf2, 0x66, 0x0b, 0xe1, 0x71, 0x5a, 0x45, 0x89, 0x33, 0x4c, 0x74, 0xc7,
];
const SM2_GENERATOR_Y: [u8; 32] = [
    0xbc, 0x37, 0x36, 0xa2, 0xf4, 0xf6, 0x77, 0x9c, 0x59, 0xbd, 0xce, 0xe3, 0x6b, 0x69, 0x21, 0x53,
    0xd0, 0xa9, 0x87, 0x7c, 0xc6, 0x2a, 0x47, 0x40, 0x02, 0xdf, 0x32, 0xe5, 0x21, 0x39, 0xf0, 0xa0,
];

/// SM2 Digital Signature Algorithm keypair.
///
/// Wraps `sm2::dsa::SigningKey` for signing and verification.
/// Uses SM3 as the hash function internally.
pub struct Sm2DsaKeypair {
    signing_key: sm2::dsa::SigningKey,
}

impl Sm2DsaKeypair {
    /// Access the inner signing key.
    pub fn as_inner(&self) -> &sm2::dsa::SigningKey {
        &self.signing_key
    }

    /// Get the verifying key.
    pub fn verifying_key(&self) -> &sm2::dsa::VerifyingKey {
        self.signing_key.verifying_key()
    }
}

impl Keypair for Sm2DsaKeypair {
    fn generate(rng: &mut dyn CryptoRng, _key_size_bits: usize) -> Result<Self> {
        use elliptic_curve::Generate;
        let secret_key =
            sm2::SecretKey::try_generate_from_rng(rng).map_err(|_| CryptoError::InternalError)?;
        let distid = DEFAULT_DISTID;
        let signing_key = sm2::dsa::SigningKey::new(distid, &secret_key)
            .map_err(|_| CryptoError::InternalError)?;
        Ok(Self { signing_key })
    }

    fn to_public_components(&self) -> Result<PublicKeyComponents> {
        let point = self.verifying_key().as_affine().to_sec1_point(false);
        let (x, y) = match point.coordinates() {
            elliptic_curve::sec1::Coordinates::Uncompressed { x, y } => (x, y),
            _ => return Err(CryptoError::InternalError),
        };
        Ok(PublicKeyComponents::Sm2(Sm2PublicPoint::from_be_bytes(
            x.to_vec(),
            y.to_vec(),
        )))
    }
}

impl Signer for Sm2DsaKeypair {
    fn sign(&self, msg: &[u8], _rng: &mut dyn CryptoRng) -> Result<SignatureBytes> {
        use sm2::dsa::signature::Signer as _;
        let sig: sm2::dsa::Signature = self
            .signing_key
            .try_sign(msg)
            .map_err(|_| CryptoError::InternalError)?;
        Ok(SignatureBytes::new(
            sig.to_bytes().to_vec(),
            SignatureAlgorithm::Sm2Dsa,
            SignatureEncoding::Raw,
        ))
    }
}

impl Verifier for Sm2DsaKeypair {
    fn verify(&self, msg: &[u8], signature: &SignatureBytes) -> Result<()> {
        if signature.algorithm() != SignatureAlgorithm::Sm2Dsa {
            return Err(CryptoError::InvalidInput);
        }
        if signature.encoding() != SignatureEncoding::Raw {
            return Err(CryptoError::InvalidInput);
        }
        use sm2::dsa::signature::Verifier;
        let signature = sm2::dsa::Signature::from_slice(signature.as_bytes())
            .map_err(|_| CryptoError::InvalidInput)?;
        self.verifying_key()
            .verify(msg, &signature)
            .map_err(|_| CryptoError::VerificationFailed)
    }
}

/// SM2 Public Key Encryption keypair.
///
/// Wraps `sm2::pke::DecryptingKey` and its associated `EncryptingKey`.
pub struct Sm2PkeKeypair {
    decrypting_key: sm2::pke::DecryptingKey,
}

impl Sm2PkeKeypair {
    /// Access the inner decrypting key.
    pub fn as_inner(&self) -> &sm2::pke::DecryptingKey {
        &self.decrypting_key
    }

    /// Get the encrypting key.
    pub fn encrypting_key(&self) -> &sm2::pke::EncryptingKey {
        self.decrypting_key.encrypting_key()
    }
}

impl Keypair for Sm2PkeKeypair {
    fn generate(rng: &mut dyn CryptoRng, _key_size_bits: usize) -> Result<Self> {
        use elliptic_curve::Generate;
        let secret_key =
            sm2::SecretKey::try_generate_from_rng(rng).map_err(|_| CryptoError::InternalError)?;
        let decrypting_key = sm2::pke::DecryptingKey::new(secret_key);
        Ok(Self { decrypting_key })
    }

    fn to_public_components(&self) -> Result<PublicKeyComponents> {
        let point = self.encrypting_key().as_affine().to_sec1_point(false);
        let (x, y) = match point.coordinates() {
            elliptic_curve::sec1::Coordinates::Uncompressed { x, y } => (x, y),
            _ => return Err(CryptoError::InternalError),
        };
        Ok(PublicKeyComponents::Sm2(Sm2PublicPoint::from_be_bytes(
            x.to_vec(),
            y.to_vec(),
        )))
    }
}

impl Encryptor for Sm2PkeKeypair {
    fn encrypt(&self, msg: &[u8], rng: &mut dyn CryptoRng) -> Result<CiphertextBytes> {
        let mut rng = RngAdapter::new(rng);
        let ciphertext = self
            .encrypting_key()
            .encrypt(&mut rng, msg)
            .map_err(|_| CryptoError::InternalError)?;
        Ok(CiphertextBytes::new(
            ciphertext,
            CiphertextAlgorithm::Sm2Pke,
        ))
    }
}

impl Decryptor for Sm2PkeKeypair {
    fn decrypt(&self, ciphertext: &CiphertextBytes) -> Result<PlaintextBytes> {
        if ciphertext.algorithm() != CiphertextAlgorithm::Sm2Pke {
            return Err(CryptoError::InvalidInput);
        }
        let plaintext = self
            .decrypting_key
            .decrypt(ciphertext.as_bytes())
            .map_err(|_| CryptoError::InternalError)?;
        Ok(PlaintextBytes::new(plaintext))
    }
}

/// Whether to compute the SM2 KEP confirmation tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfirmationMode {
    /// Skip confirmation tag computation.
    Skip,
    /// Compute both `conf_out` and `conf_peer`.
    Compute,
}

/// SM2 KEP peer public material passed into [`sm2_kep_derive`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sm2KepPeerKeys {
    pub static_x: [u8; 32],
    pub static_y: [u8; 32],
    pub ephemeral_x: [u8; 32],
    pub ephemeral_y: [u8; 32],
}

/// SM2 KEP derive parameters (aligned with OP-TEE `struct sm2_kep_parms`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sm2KepDeriveParams<'a> {
    /// `true` when the local party is the protocol initiator (GP `TEE_ATTR_SM2_KEP_USER` = 0).
    pub is_initiator: bool,
    pub initiator_id: &'a [u8],
    pub responder_id: &'a [u8],
    pub peer: Sm2KepPeerKeys,
    /// Optional confirmation from the peer.
    ///
    /// The initiator expects `Hash(0x02 || y || inner)` (the responder's
    /// `conf_out`). The responder expects `Hash(0x03 || y || inner)` (the
    /// initiator's `conf_out`). `inner` is
    /// `Hash(x || ZA || ZB || x1 || y1 || x2 || y2)`, matching GB/T 32918.3
    /// and OP-TEE `sm2_kep_compute_S`.
    pub conf_in: Option<&'a [u8]>,
    /// Whether to compute the local confirmation tag (`conf_out` / `conf_peer`).
    pub confirmation: ConfirmationMode,
}

/// Output of [`sm2_kep_derive`]: derived key material and optional confirmation tags.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sm2KepDeriveOutput {
    pub key: Vec<u8>,
    /// Local confirmation value written to `TEE_ATTR_SM2_KEP_CONFIRMATION_OUT`.
    ///
    /// OP-TEE semantics: initiator uses tag `0x03`, responder uses tag `0x02`.
    pub conf_out: Option<[u8; 32]>,
    /// Same bytes as [`Self::conf_out`].
    ///
    /// The peer supplies this value as its `conf_in`. Checking it locally fails,
    /// because each side expects the opposite tag.
    pub conf_peer: Option<[u8; 32]>,
}

/// `w` parameter from GB/T 32918.3 section 6.1 (`ceil(log2(n)/2) - 1`).
const SM2_KEP_W: u32 = 127;

/// SM2 Key Exchange Protocol keypair (static or ephemeral).
pub struct Sm2KepKeypair {
    secret_key: sm2::SecretKey,
}

impl Sm2KepKeypair {
    /// Build a keypair from a 32-byte private scalar (big-endian).
    pub fn from_secret_bytes(secret: &[u8; 32]) -> Result<Self> {
        let secret_key = sm2::SecretKey::from_slice(secret).map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self { secret_key })
    }

    /// Access raw 32-byte private scalar.
    pub fn secret_key_bytes(&self) -> [u8; 32] {
        self.secret_key.to_bytes().into()
    }

    /// Static/ephemeral public key affine coordinates (big-endian, 32 bytes each).
    pub fn public_key_bytes(&self) -> Result<([u8; 32], [u8; 32])> {
        public_key_bytes_from_secret(&self.secret_key)
    }
}

impl Keypair for Sm2KepKeypair {
    fn generate(rng: &mut dyn CryptoRng, _key_size_bits: usize) -> Result<Self> {
        use elliptic_curve::Generate;
        let secret_key =
            sm2::SecretKey::try_generate_from_rng(rng).map_err(|_| CryptoError::InternalError)?;
        Ok(Self { secret_key })
    }

    fn to_public_components(&self) -> Result<PublicKeyComponents> {
        let (x, y) = self.public_key_bytes()?;
        Ok(PublicKeyComponents::Sm2(Sm2PublicPoint::from_be_bytes(
            x.to_vec(),
            y.to_vec(),
        )))
    }
}

fn public_key_bytes_from_secret(secret_key: &sm2::SecretKey) -> Result<([u8; 32], [u8; 32])> {
    let point = secret_key.public_key().as_affine().to_sec1_point(false);
    match point.coordinates() {
        elliptic_curve::sec1::Coordinates::Uncompressed { x, y } => {
            let mut x_bytes = [0u8; 32];
            let mut y_bytes = [0u8; 32];
            x_bytes.copy_from_slice(x.as_slice());
            y_bytes.copy_from_slice(y.as_slice());
            Ok((x_bytes, y_bytes))
        }
        _ => Err(CryptoError::InternalError),
    }
}

fn affine_from_xy(x: &[u8], y: &[u8]) -> Result<AffinePoint> {
    let x_bytes: &sm2::FieldBytes = x.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &sm2::FieldBytes = y.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let point = sm2::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
    AffinePoint::from_sec1_point(&point)
        .into_option()
        .ok_or(CryptoError::InvalidKey)
}

fn scalar_from_secret_key(secret_key: &sm2::SecretKey) -> Result<Scalar> {
    let bytes = secret_key.to_bytes();
    Option::from(Scalar::from_bytes(&bytes)).ok_or(CryptoError::InvalidKey)
}

/// Z = SM3(ENTL || ID || a || b || Gx || Gy || Px || Py).
fn sm2_compute_user_z(user_id: &[u8], pub_x: &[u8; 32], pub_y: &[u8; 32]) -> Result<[u8; 32]> {
    let entla: u16 = (user_id.len() * 8)
        .try_into()
        .map_err(|_| CryptoError::InvalidInput)?;
    let mut hasher = Sm3::new();
    hasher.update(entla.to_be_bytes());
    hasher.update(user_id);
    hasher.update(SM2_EQUATION_A);
    hasher.update(SM2_EQUATION_B);
    hasher.update(SM2_GENERATOR_X);
    hasher.update(SM2_GENERATOR_Y);
    hasher.update(pub_x);
    hasher.update(pub_y);
    Ok(hasher.finalize().into())
}

/// GB/T 32918.3 KDF(Z, klen) with SM3 (`klen` in bytes).
fn sm2_kdf(z: &[u8], klen_bytes: usize) -> Result<Vec<u8>> {
    if klen_bytes == 0 {
        return Err(CryptoError::InvalidLength);
    }
    let klen_bits = klen_bytes
        .checked_mul(8)
        .ok_or(CryptoError::InvalidLength)?;
    let hash_bits = 256usize;
    let rounds = klen_bits.div_ceil(hash_bits);
    let mut out = Vec::with_capacity(klen_bytes);
    let mut ct: u32 = 1;
    for i in 0..rounds {
        let mut hasher = Sm3::new();
        hasher.update(z);
        hasher.update(ct.to_be_bytes());
        let ha = hasher.finalize();
        ct = ct.checked_add(1).ok_or(CryptoError::InvalidLength)?;

        if i + 1 == rounds && klen_bits % hash_bits != 0 {
            let take_bits = klen_bits % hash_bits;
            let take_bytes = take_bits.div_ceil(8);
            out.extend_from_slice(&ha[..take_bytes]);
        } else {
            out.extend_from_slice(&ha);
        }
    }
    out.truncate(klen_bytes);
    Ok(out)
}

fn sm2_kep_x_bar(x: &[u8; 32]) -> Result<Scalar> {
    let x_int = U256::from_be_slice(x);
    let mask = (U256::ONE << SM2_KEP_W) - U256::ONE;
    let x_bar_int = (U256::ONE << SM2_KEP_W) | (x_int & mask);
    Option::from(Scalar::from_uint(&x_bar_int)).ok_or(CryptoError::InvalidKey)
}

fn sm2_kep_shared_point(
    t: &Scalar,
    peer_static: &AffinePoint,
    peer_ephemeral: &AffinePoint,
    peer_ephemeral_x_bar: &Scalar,
) -> Result<AffinePoint> {
    let peer_eph = ProjectivePoint::from(*peer_ephemeral);
    let scaled_eph = peer_eph * peer_ephemeral_x_bar;
    let peer_stat = ProjectivePoint::from(*peer_static);
    let combined = peer_stat + scaled_eph;
    let shared = combined * t;
    let shared = CtOption::new(shared.to_affine(), !shared.is_identity())
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    Ok(shared)
}

fn affine_xy(point: &AffinePoint) -> ([u8; 32], [u8; 32]) {
    let mut x_bytes = [0u8; 32];
    let mut y_bytes = [0u8; 32];
    x_bytes.copy_from_slice(point.x().as_slice());
    y_bytes.copy_from_slice(point.y().as_slice());
    (x_bytes, y_bytes)
}

fn sm2_kep_inner_hash(
    shared_x: &[u8; 32],
    za: &[u8; 32],
    zb: &[u8; 32],
    initiator_ephemeral_x: &[u8; 32],
    initiator_ephemeral_y: &[u8; 32],
    responder_ephemeral_x: &[u8; 32],
    responder_ephemeral_y: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = Sm3::new();
    hasher.update(shared_x);
    hasher.update(za);
    hasher.update(zb);
    hasher.update(initiator_ephemeral_x);
    hasher.update(initiator_ephemeral_y);
    hasher.update(responder_ephemeral_x);
    hasher.update(responder_ephemeral_y);
    hasher.finalize().into()
}

fn sm2_kep_confirmation(tag: u8, shared_y: &[u8; 32], inner: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sm3::new();
    hasher.update([tag]);
    hasher.update(shared_y);
    hasher.update(inner);
    hasher.finalize().into()
}

/// Derive an SM2 KEP session key (GB/T 32918.3 section 6.1).
///
/// `my_static` is the local long-term keypair; `my_ephemeral` is the local ephemeral keypair.
/// `key_len` is the desired derived key size in bytes.
pub fn sm2_kep_derive(
    my_static: &Sm2KepKeypair,
    my_ephemeral: &Sm2KepKeypair,
    params: &Sm2KepDeriveParams<'_>,
    key_len: usize,
) -> Result<Sm2KepDeriveOutput> {
    if params.initiator_id.is_empty() || params.responder_id.is_empty() {
        return Err(CryptoError::InvalidInput);
    }

    let (my_stat_x, my_stat_y) = my_static.public_key_bytes()?;
    let (my_eph_x, my_eph_y) = my_ephemeral.public_key_bytes()?;

    let (init_stat_x, init_stat_y, resp_stat_x, resp_stat_y) = if params.is_initiator {
        (
            my_stat_x,
            my_stat_y,
            params.peer.static_x,
            params.peer.static_y,
        )
    } else {
        (
            params.peer.static_x,
            params.peer.static_y,
            my_stat_x,
            my_stat_y,
        )
    };

    let (init_eph_x, init_eph_y, resp_eph_x, resp_eph_y) = if params.is_initiator {
        (
            my_eph_x,
            my_eph_y,
            params.peer.ephemeral_x,
            params.peer.ephemeral_y,
        )
    } else {
        (
            params.peer.ephemeral_x,
            params.peer.ephemeral_y,
            my_eph_x,
            my_eph_y,
        )
    };

    let za = sm2_compute_user_z(params.initiator_id, &init_stat_x, &init_stat_y)?;
    let zb = sm2_compute_user_z(params.responder_id, &resp_stat_x, &resp_stat_y)?;

    let d = scalar_from_secret_key(&my_static.secret_key)?;
    let r = scalar_from_secret_key(&my_ephemeral.secret_key)?;
    let x_self_bar = sm2_kep_x_bar(&my_eph_x)?;
    let t = d + (x_self_bar * r);

    let peer_static = affine_from_xy(&params.peer.static_x, &params.peer.static_y)?;
    let peer_ephemeral = affine_from_xy(&params.peer.ephemeral_x, &params.peer.ephemeral_y)?;
    let peer_ephemeral_x_bar = sm2_kep_x_bar(&params.peer.ephemeral_x)?;

    let shared = sm2_kep_shared_point(&t, &peer_static, &peer_ephemeral, &peer_ephemeral_x_bar)?;
    let (shared_x, shared_y) = affine_xy(&shared);

    let kdf_input = {
        let mut buf = Vec::with_capacity(128);
        buf.extend_from_slice(&shared_x);
        buf.extend_from_slice(&shared_y);
        buf.extend_from_slice(&za);
        buf.extend_from_slice(&zb);
        buf
    };
    let key = sm2_kdf(&kdf_input, key_len)?;

    let inner = sm2_kep_inner_hash(
        &shared_x,
        &za,
        &zb,
        &init_eph_x,
        &init_eph_y,
        &resp_eph_x,
        &resp_eph_y,
    );

    // OP-TEE sm2-kep.c: conf_in expects tag 0x02 for initiator, 0x03 for responder;
    // conf_out uses the opposite tag (value sent to the peer).
    let conf_out_tag = if params.is_initiator { 0x03 } else { 0x02 };
    let conf_in_tag = if params.is_initiator { 0x02 } else { 0x03 };

    let conf_out = if params.confirmation == ConfirmationMode::Compute {
        Some(sm2_kep_confirmation(conf_out_tag, &shared_y, &inner))
    } else {
        None
    };
    let conf_peer = conf_out;

    if let Some(peer_conf) = params.conf_in {
        if peer_conf.len() != 32 {
            return Err(CryptoError::InvalidLength);
        }
        let expected = sm2_kep_confirmation(conf_in_tag, &shared_y, &inner);
        if peer_conf != expected {
            return Err(CryptoError::VerificationFailed);
        }
    }

    Ok(Sm2KepDeriveOutput {
        key,
        conf_out,
        conf_peer,
    })
}

/// SM2 PKE encrypt using raw public key (x, y) bytes.
pub fn sm2_pke_encrypt(
    public_x: &[u8],
    public_y: &[u8],
    input: &[u8],
    rng: &mut dyn CryptoRng,
) -> Result<CiphertextBytes> {
    let x_bytes: &sm2::FieldBytes = public_x
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &sm2::FieldBytes = public_y
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let point = sm2::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
    let affine = sm2::AffinePoint::from_sec1_point(&point)
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    let enc_key =
        sm2::pke::EncryptingKey::from_affine(affine).map_err(|_| CryptoError::InvalidKey)?;
    let mut rng = RngAdapter::new(rng);
    let ciphertext = enc_key
        .encrypt(&mut rng, input)
        .map_err(|_| CryptoError::InternalError)?;
    Ok(CiphertextBytes::new(
        ciphertext,
        CiphertextAlgorithm::Sm2Pke,
    ))
}

/// SM2 PKE decrypt using raw private key bytes.
pub fn sm2_pke_decrypt(secret_key: &[u8], ciphertext: &CiphertextBytes) -> Result<PlaintextBytes> {
    if ciphertext.algorithm() != CiphertextAlgorithm::Sm2Pke {
        return Err(CryptoError::InvalidInput);
    }
    let sk = sm2::SecretKey::from_slice(secret_key).map_err(|_| CryptoError::InvalidKey)?;
    let dk = sm2::pke::DecryptingKey::new(sk);
    let plaintext = dk
        .decrypt(ciphertext.as_bytes())
        .map_err(|_| CryptoError::InternalError)?;
    Ok(PlaintextBytes::new(plaintext))
}

/// SM2 DSA sign using raw private key bytes and a precomputed digest `e`.
///
/// OP-TEE/xtest pass `e = SM3(ZA || M)` (e.g. `SM3(ptx)` when `ptx = Z || M`),
/// not the raw message — use [`PrehashSigner::sign_prehash`], not `try_sign`.
pub fn sm2_dsa_sign(
    secret_key: &[u8],
    prehash: &[u8],
    _rng: &mut dyn CryptoRng,
) -> Result<SignatureBytes> {
    let sk = sm2::SecretKey::from_slice(secret_key).map_err(|_| CryptoError::InvalidKey)?;
    let distid = DEFAULT_DISTID;
    let signing_key =
        sm2::dsa::SigningKey::new(distid, &sk).map_err(|_| CryptoError::InternalError)?;
    let sig: sm2::dsa::Signature = signing_key
        .sign_prehash(prehash)
        .map_err(|_| CryptoError::InternalError)?;
    Ok(SignatureBytes::new(
        sig.to_bytes().to_vec(),
        SignatureAlgorithm::Sm2Dsa,
        SignatureEncoding::Raw,
    ))
}

/// SM2 DSA verify using raw public key (x, y) bytes and precomputed digest `e`.
///
/// Matches OP-TEE `sm2_verify_digest_raw`: `digest` is `e = SM3(ZA || M)`, signature
/// is raw 64-byte `r||s` or DER.
pub fn sm2_dsa_verify(
    public_x: &[u8],
    public_y: &[u8],
    prehash: &[u8],
    signature: &SignatureBytes,
) -> Result<()> {
    if signature.algorithm() != SignatureAlgorithm::Sm2Dsa {
        return Err(CryptoError::AlgorithmMismatch);
    }
    let x_bytes: &sm2::FieldBytes = public_x
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &sm2::FieldBytes = public_y
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let point = sm2::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
    let affine = sm2::AffinePoint::from_sec1_point(&point)
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    let pk = sm2::PublicKey::from_affine(affine).map_err(|_| CryptoError::InvalidKey)?;
    let distid = DEFAULT_DISTID;
    let verifying_key =
        sm2::dsa::VerifyingKey::new(distid, pk).map_err(|_| CryptoError::InternalError)?;
    let sig = match signature.encoding() {
        SignatureEncoding::Raw => sm2::dsa::Signature::from_slice(signature.as_bytes()),
        SignatureEncoding::Der => sm2::dsa::Signature::from_der(signature.as_bytes()),
    }
    .map_err(|_| CryptoError::InvalidInput)?;
    verifying_key
        .verify_prehash(prehash, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

/// Compute SM2 DSA sign/verify digest `e = SM3(ZA || M)` (OP-TEE `sm2_compute_sign_digest`).
///
/// Callers of [`sm2_dsa_sign`] / [`sm2_dsa_verify`] must pass this value, not the raw message.
pub fn sm2_compute_sign_digest(
    distid: Option<&str>,
    message: &[u8],
    public_key: &[u8; 64],
) -> Result<[u8; 32]> {
    let distid = distid.unwrap_or(DEFAULT_DISTID);
    let entla: u16 = (distid.len() * 8)
        .try_into()
        .map_err(|_| CryptoError::InvalidInput)?;
    let mut hasher = Sm3::new();
    hasher.update(entla.to_be_bytes());
    hasher.update(distid.as_bytes());
    hasher.update(SM2_EQUATION_A);
    hasher.update(SM2_EQUATION_B);
    hasher.update(SM2_GENERATOR_X);
    hasher.update(SM2_GENERATOR_Y);
    hasher.update(&public_key[..32]);
    hasher.update(&public_key[32..]);
    let z: [u8; 32] = hasher.finalize().into();

    let mut hasher = Sm3::new();
    hasher.update(z);
    hasher.update(message);
    Ok(hasher.finalize().into())
}

// ---------------------------------------------------------------------------
// Byte-level SM2 DSA (X.509 message mode / CMS) — additive API for tasign
// ---------------------------------------------------------------------------

fn verifying_key_from_sec1(public_key_sec1: &[u8]) -> Result<sm2::dsa::VerifyingKey> {
    sm2::dsa::VerifyingKey::from_sec1_bytes(DEFAULT_DISTINGUISHING_ID, public_key_sec1)
        .map_err(|_| CryptoError::InvalidKey)
}

/// Returns `Ok(())` when `public_key_sec1` is valid SM2 SEC1 encoding (rejects other curves).
pub fn sm2_validate_sec1_public_key(public_key_sec1: &[u8]) -> Result<()> {
    verifying_key_from_sec1(public_key_sec1).map(|_| ())
}

fn signing_key_from_secret(secret_key: &[u8]) -> Result<sm2::dsa::SigningKey> {
    let sk = sm2::SecretKey::from_slice(secret_key).map_err(|_| CryptoError::InvalidKey)?;
    sm2::dsa::SigningKey::new(DEFAULT_DISTID, &sk).map_err(|_| CryptoError::InternalError)
}

fn parse_sm2_signature_der(signature: &[u8]) -> Result<sm2::dsa::Signature> {
    sm2::dsa::Signature::from_der(signature).map_err(|_| CryptoError::InvalidInput)
}

/// Verify SM2 signature over a raw message (ZA‖M preprocessing), SEC1 uncompressed public key.
pub fn sm2_verify_message_sec1(public_key_sec1: &[u8], msg: &[u8], signature: &[u8]) -> Result<()> {
    use sm2::dsa::signature::Verifier;
    let verifying_key = verifying_key_from_sec1(public_key_sec1)?;
    let sig = parse_sm2_signature_der(signature)?;
    verifying_key
        .verify(msg, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

/// Sign SM2 message mode (ZA‖M); returns DER-encoded signature.
pub fn sm2_sign_message(
    secret_key: &[u8],
    msg: &[u8],
    rng: &mut dyn CryptoRng,
) -> Result<alloc::vec::Vec<u8>> {
    use sm2::dsa::signature::RandomizedSigner;
    let signing_key = signing_key_from_secret(secret_key)?;
    let mut adapter = RngAdapter::new(rng);
    let sig = signing_key
        .try_sign_with_rng(&mut adapter, msg)
        .map_err(|_| CryptoError::InternalError)?;
    Ok(sig.to_der().as_bytes().to_vec())
}

/// Sign SM2 digest mode (pre-hashed *e*); returns DER-encoded signature.
pub fn sm2_sign_digest(
    secret_key: &[u8],
    digest: &[u8; 32],
    rng: &mut dyn CryptoRng,
) -> Result<alloc::vec::Vec<u8>> {
    use sm2::dsa::signature::hazmat::RandomizedPrehashSigner;
    let signing_key = signing_key_from_secret(secret_key)?;
    let mut adapter = RngAdapter::new(rng);
    let sig = signing_key
        .sign_prehash_with_rng(&mut adapter, digest)
        .map_err(|_| CryptoError::InternalError)?;
    Ok(sig.to_der().as_bytes().to_vec())
}

/// Verify SM2 digest mode (pre-hashed *e*), SEC1 uncompressed public key.
pub fn sm2_verify_digest_sec1(
    public_key_sec1: &[u8],
    digest: &[u8; 32],
    signature: &[u8],
) -> Result<()> {
    use sm2::dsa::signature::hazmat::PrehashVerifier;
    let verifying_key = verifying_key_from_sec1(public_key_sec1)?;
    let sig = parse_sm2_signature_der(signature)?;
    verifying_key
        .verify_prehash(digest, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

/// Parse a 32-byte SM2 private scalar from PKCS#8 DER.
pub fn sm2_secret_scalar_from_pkcs8_der(der: &[u8]) -> Result<[u8; 32]> {
    use pkcs8::DecodePrivateKey;
    let sk = sm2::SecretKey::from_pkcs8_der(der).map_err(|_| CryptoError::InvalidKey)?;
    Ok(sk.to_bytes().into())
}
