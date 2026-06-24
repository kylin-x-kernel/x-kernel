// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! tee_crypto PKIX facade: inlined [`pkix_path`] + SM2 / GmSSL extensions.

use spki::{AlgorithmIdentifierRef, SubjectPublicKeyInfoRef};

mod x509_verify;

pub use x509_cert::Certificate;
pub use x509_verify::{
    OID_SM2_SIGN_SM3, verify_ecdsa_p256_sha256, verify_ecdsa_p384_sha384,
    verify_rsa_pkcs1v15_sha256, verify_rsa_pkcs1v15_sha384, verify_rsa_pkcs1v15_sha512,
    verify_sm2_sign_sm3,
};

pub use crate::pkix_path::{
    DefaultVerifier as UpstreamDefaultVerifier, DerError, DnAttrRule, EcdsaP256Verifier,
    EcdsaP384Verifier, Error, PolicyTreeNode, Profile, Result, RsaPkcs1v15Sha256Verifier,
    RsaPkcs1v15Sha384Verifier, RsaPkcs1v15Sha512Verifier, SignatureVerifier, TrustAnchor,
    ValidatedPath, ValidationPolicy, cert_is_ca, names_match, validate_path,
    validate_path_with_profile,
};

/// Recommended verifier for tasign / tee_crypto: upstream RSA/ECDSA + SM2-with-SM3.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DefaultVerifier;

impl SignatureVerifier for DefaultVerifier {
    fn verify_signature(
        &self,
        algorithm: AlgorithmIdentifierRef<'_>,
        issuer_spki: SubjectPublicKeyInfoRef<'_>,
        message: &[u8],
        signature: &[u8],
    ) -> core::result::Result<(), signature::Error> {
        if algorithm.oid == OID_SM2_SIGN_SM3 {
            return verify_sm2_sign_sm3(issuer_spki, message, signature);
        }
        UpstreamDefaultVerifier.verify_signature(algorithm, issuer_spki, message, signature)
    }
}
