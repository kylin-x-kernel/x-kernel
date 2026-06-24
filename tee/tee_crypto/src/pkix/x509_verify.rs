// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X.509 TBSCertificate signature verification helpers (message + DER signature).
//!
//! Separate from [`crate::tee_ops`] which expects pre-hashed digests and raw signatures.

use p256::ecdsa::{DerSignature, VerifyingKey as P256VerifyingKey};
use p384::ecdsa::{DerSignature as P384DerSignature, VerifyingKey as P384VerifyingKey};
use rsa::{
    pkcs1v15::{Signature as RsaSignature, VerifyingKey as RsaVerifyingKey},
    signature::Verifier as RsaVerifierTrait,
};
use sha2::{Sha256, Sha384, Sha512};
use spki::SubjectPublicKeyInfoRef;

use crate::{
    pkix_path::{rsa_public_key_from_spki_ref, spki_subject_public_key_bytes},
    sm2::sm2_verify_message_sec1,
};

/// SM2 with SM3 certificate signature (`1.2.156.10197.1.501`).
pub const OID_SM2_SIGN_SM3: der::asn1::ObjectIdentifier =
    der::asn1::ObjectIdentifier::new_unwrap("1.2.156.10197.1.501");

type SigErr = ::signature::Error;

fn map_crypto_err<T>(r: Result<T, impl core::fmt::Debug>) -> Result<T, SigErr> {
    r.map_err(|_| SigErr::new())
}

/// Verify ECDSA P-256 + SHA-256 over TBSCertificate DER (`ecdsa-with-SHA256`).
pub fn verify_ecdsa_p256_sha256(
    issuer_spki: SubjectPublicKeyInfoRef<'_>,
    tbs_der: &[u8],
    signature_der: &[u8],
) -> Result<(), SigErr> {
    let sec1 = spki_subject_public_key_bytes(issuer_spki)?;
    let vk = map_crypto_err(P256VerifyingKey::from_sec1_bytes(sec1))?;
    let sig = map_crypto_err(DerSignature::try_from(signature_der))?;
    use p256::ecdsa::signature::Verifier;
    Verifier::verify(&vk, tbs_der, &sig).map_err(|_| SigErr::new())
}

/// Verify ECDSA P-384 + SHA-384 over TBSCertificate DER (`ecdsa-with-SHA384`).
pub fn verify_ecdsa_p384_sha384(
    issuer_spki: SubjectPublicKeyInfoRef<'_>,
    tbs_der: &[u8],
    signature_der: &[u8],
) -> Result<(), SigErr> {
    let sec1 = spki_subject_public_key_bytes(issuer_spki)?;
    let vk = map_crypto_err(P384VerifyingKey::from_sec1_bytes(sec1))?;
    let sig = map_crypto_err(P384DerSignature::try_from(signature_der))?;
    use p384::ecdsa::signature::Verifier;
    Verifier::verify(&vk, tbs_der, &sig).map_err(|_| SigErr::new())
}

/// Verify RSA PKCS#1 v1.5 + SHA-256 over TBSCertificate DER.
pub fn verify_rsa_pkcs1v15_sha256(
    issuer_spki: SubjectPublicKeyInfoRef<'_>,
    tbs_der: &[u8],
    signature_der: &[u8],
) -> Result<(), SigErr> {
    let pk = rsa_public_key_from_spki_ref(issuer_spki)?;
    let vk = RsaVerifyingKey::<Sha256>::new(pk);
    let sig = map_crypto_err(RsaSignature::try_from(signature_der))?;
    RsaVerifierTrait::verify(&vk, tbs_der, &sig).map_err(|_| SigErr::new())
}

/// Verify RSA PKCS#1 v1.5 + SHA-384 over TBSCertificate DER.
pub fn verify_rsa_pkcs1v15_sha384(
    issuer_spki: SubjectPublicKeyInfoRef<'_>,
    tbs_der: &[u8],
    signature_der: &[u8],
) -> Result<(), SigErr> {
    let pk = rsa_public_key_from_spki_ref(issuer_spki)?;
    let vk = RsaVerifyingKey::<Sha384>::new(pk);
    let sig = map_crypto_err(RsaSignature::try_from(signature_der))?;
    RsaVerifierTrait::verify(&vk, tbs_der, &sig).map_err(|_| SigErr::new())
}

/// Verify RSA PKCS#1 v1.5 + SHA-512 over TBSCertificate DER.
pub fn verify_rsa_pkcs1v15_sha512(
    issuer_spki: SubjectPublicKeyInfoRef<'_>,
    tbs_der: &[u8],
    signature_der: &[u8],
) -> Result<(), SigErr> {
    let pk = rsa_public_key_from_spki_ref(issuer_spki)?;
    let vk = RsaVerifyingKey::<Sha512>::new(pk);
    let sig = map_crypto_err(RsaSignature::try_from(signature_der))?;
    RsaVerifierTrait::verify(&vk, tbs_der, &sig).map_err(|_| SigErr::new())
}

/// Verify SM2-with-SM3 over TBSCertificate DER (message mode, default distid).
pub fn verify_sm2_sign_sm3(
    issuer_spki: SubjectPublicKeyInfoRef<'_>,
    tbs_der: &[u8],
    signature_der: &[u8],
) -> Result<(), SigErr> {
    let sec1 = spki_subject_public_key_bytes(issuer_spki)?;
    sm2_verify_message_sec1(sec1, tbs_der, signature_der).map_err(|_| SigErr::new())
}
