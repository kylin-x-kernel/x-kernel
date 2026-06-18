// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::{
    CryptoError,
    asymmetric::{Decryptor, Encryptor, Keypair, PublicKeyComponents, Signer, Verifier},
    rng::DeterministicRng,
    rsa::*,
    tee_ops::rsa::rsa_keygen,
};

#[test]
fn test_rsa_keygen_sign_verify_roundtrip() {
    let mut rng = DeterministicRng::seed_from_u64(42);
    let keypair = RsaKeypair::generate(&mut rng, 2048).expect("keygen");

    let msg = b"hello RSA";
    let sig = keypair.sign(msg, &mut rng).expect("sign");

    let pubkey = keypair.to_public_key();
    pubkey.verify(msg, &sig).expect("verify should succeed");
}

#[test]
fn test_rsa_encrypt_decrypt_roundtrip() {
    let mut rng = DeterministicRng::seed_from_u64(99);
    let keypair = RsaKeypair::generate(&mut rng, 2048).expect("keygen");
    let pubkey = keypair.to_public_key();

    let msg = b"secret message";
    let ct = pubkey.encrypt(msg, &mut rng).expect("encrypt");
    let pt = keypair.decrypt(&ct).expect("decrypt");
    assert_eq!(pt.expose_secret(), msg);
}

#[test]
fn test_rsa_verify_wrong_msg_fails() {
    let mut rng = DeterministicRng::seed_from_u64(7);
    let keypair = RsaKeypair::generate(&mut rng, 2048).expect("keygen");

    let msg = b"correct message";
    let sig = keypair.sign(msg, &mut rng).expect("sign");

    let pubkey = keypair.to_public_key();
    assert_eq!(
        pubkey.verify(b"wrong message", &sig).unwrap_err(),
        CryptoError::VerificationFailed
    );
}

#[test]
fn test_rsa_public_key_components() {
    let mut rng = DeterministicRng::seed_from_u64(123);
    let keypair = RsaKeypair::generate(&mut rng, 2048).expect("keygen");
    let comps = keypair.to_public_components().expect("components");
    match comps {
        PublicKeyComponents::Rsa(public) => {
            assert_eq!(public.n().len(), 256);
            assert!(!public.e().is_empty());
        }
        _ => panic!("expected RSA components"),
    }
}

mod ops_api {
    extern crate alloc;

    use tee_crypto::{
        CryptoError,
        hash::{DigestBytes, HashAlgorithm},
        rng::DeterministicRng,
        tee_ops::rsa::*,
    };

    fn sha256(data: &[u8]) -> DigestBytes {
        use digest::Digest;
        let mut h = sha2::Sha256::new();
        h.update(data);
        DigestBytes::new(h.finalize().to_vec(), HashAlgorithm::Sha256)
    }

    /// Strip leading zero bytes so that BoxedUint precision differences don't
    /// cause false assertion failures when comparing big-endian representations.
    fn strip_leading_zeros(bytes: &[u8]) -> &[u8] {
        let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
        &bytes[start..]
    }

    /// Assert two big-endian byte slices encode the same integer value,
    /// ignoring leading-zero padding from differing BoxedUint precisions.
    fn assert_be_eq(left: &[u8], right: &[u8]) {
        assert_eq!(strip_leading_zeros(left), strip_leading_zeros(right));
    }

    #[test]
    fn test_rsa_keygen_sign_verify_pkcs1v15_sha256() {
        let mut rng = DeterministicRng::seed_from_u64(42);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();
        let pub_key = key.to_public_key();

        let msg = b"hello RSA PKCS1v15 SHA-256";
        let digest = sha256(msg);
        let sig = rsa_sign(
            &key,
            RsaHashAlgo::Sha256,
            RsaSignPadding::Pkcs1v15,
            &digest,
            &mut rng,
            None,
        )
        .unwrap();
        rsa_verify(
            &pub_key,
            RsaHashAlgo::Sha256,
            RsaSignPadding::Pkcs1v15,
            &digest,
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn test_rsa_keygen_sign_verify_pss_sha256() {
        let mut rng = DeterministicRng::seed_from_u64(43);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();
        let pub_key = key.to_public_key();

        let msg = b"hello RSA PSS SHA-256";
        let digest = sha256(msg);
        let sig = rsa_sign(
            &key,
            RsaHashAlgo::Sha256,
            RsaSignPadding::Pss,
            &digest,
            &mut rng,
            None,
        )
        .unwrap();
        rsa_verify(
            &pub_key,
            RsaHashAlgo::Sha256,
            RsaSignPadding::Pss,
            &digest,
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn test_rsa_oaep_encrypt_decrypt_sha256() {
        let mut rng = DeterministicRng::seed_from_u64(44);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();
        let pub_key = key.to_public_key();

        let msg = b"OAEP secret";
        let ct = rsa_encrypt_oaep(&pub_key, RsaHashAlgo::Sha256, b"", msg, &mut rng).unwrap();
        let pt = rsa_decrypt_oaep(&key, RsaHashAlgo::Sha256, b"", &ct).unwrap();
        assert_eq!(pt.expose_secret(), msg);
    }

    #[test]
    fn test_rsa_pkcs1v15_encrypt_decrypt() {
        let mut rng = DeterministicRng::seed_from_u64(45);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();
        let pub_key = key.to_public_key();

        let msg = b"PKCS1v15 encrypt";
        let ct = rsa_encrypt_pkcs1v15(&pub_key, msg, &mut rng).unwrap();
        let pt = rsa_decrypt_pkcs1v15(&key, &ct).unwrap();
        assert_eq!(pt.expose_secret(), msg);
    }

    #[test]
    fn test_rsa_from_n_e_d_only_roundtrip() {
        let mut rng = DeterministicRng::seed_from_u64(45);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();
        let n = rsa_get_n(&key);
        let e = rsa_get_e(&key);
        let d = rsa_get_d(&key);

        let reconstructed = rsa_key_from_components(&n, &e, d.expose_secret(), &[], &[]).unwrap();
        let msg = b"PKCS1v15 encrypt with n/e/d only key";
        let ct = rsa_encrypt_pkcs1v15(&key.to_public_key(), msg, &mut rng).unwrap();
        let pt = rsa_decrypt_pkcs1v15(&reconstructed, &ct).unwrap();
        assert_eq!(pt.expose_secret(), msg);
    }

    #[test]
    fn test_rsa_from_components_roundtrip() {
        let mut rng = DeterministicRng::seed_from_u64(46);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();

        let n = rsa_get_n(&key);
        let e = rsa_get_e(&key);
        let d = rsa_get_d(&key);
        let primes = rsa_get_primes(&key);

        let reconstructed = rsa_key_from_components(
            &n,
            &e,
            d.expose_secret(),
            primes[0].expose_secret(),
            primes[1].expose_secret(),
        )
        .unwrap();

        let msg = b"reconstructed key test";
        let digest = sha256(msg);
        let sig = rsa_sign(
            &reconstructed,
            RsaHashAlgo::Sha256,
            RsaSignPadding::Pkcs1v15,
            &digest,
            &mut rng,
            None,
        )
        .unwrap();
        let pub_key = key.to_public_key();
        rsa_verify(
            &pub_key,
            RsaHashAlgo::Sha256,
            RsaSignPadding::Pkcs1v15,
            &digest,
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn test_rsa_nopad_n_e_d_only_roundtrip() {
        let mut rng = DeterministicRng::seed_from_u64(47);
        let key = rsa_keygen(&mut rng, 1024, 65537).unwrap();
        let pub_key = key.to_public_key();
        let n = rsa_get_n(&key);
        let e = rsa_get_e(&key);
        let d = rsa_get_d(&key);
        let key_ne_d = rsa_key_from_components(&n, &e, d.expose_secret(), &[], &[]).unwrap();

        let msg = b"nopad payload";
        let mut ct = vec![0u8; n.len()];
        let ct_len = rsa_nopad_encrypt(&pub_key, msg, &mut ct).unwrap();
        let mut pt = vec![0u8; n.len()];
        let pt_len = rsa_nopad_decrypt(&key_ne_d, &ct[..ct_len], &mut pt).unwrap();
        assert_eq!(&pt[..pt_len], msg);
    }

    #[test]
    fn test_rsa_wrong_signature_fails() {
        let mut rng = DeterministicRng::seed_from_u64(47);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();
        let pub_key = key.to_public_key();

        let digest1 = sha256(b"msg1");
        let digest2 = sha256(b"msg2");
        let sig = rsa_sign(
            &key,
            RsaHashAlgo::Sha256,
            RsaSignPadding::Pkcs1v15,
            &digest1,
            &mut rng,
            None,
        )
        .unwrap();
        assert_eq!(
            rsa_verify(
                &pub_key,
                RsaHashAlgo::Sha256,
                RsaSignPadding::Pkcs1v15,
                &digest2,
                &sig,
            )
            .unwrap_err(),
            CryptoError::VerificationFailed
        );
    }

    #[test]
    fn test_rsa_pkcs8_der_parsing() {
        let mut rng = DeterministicRng::seed_from_u64(48);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();

        // Encode key to PKCS#8 DER bytes
        let doc = key.to_pkcs8_der().unwrap();
        let der = doc.as_bytes();

        let parsed_key = rsa_private_key_from_pkcs8_der(der).unwrap();
        assert_be_eq(&rsa_get_n(&key), &rsa_get_n(&parsed_key));
        assert_be_eq(&rsa_get_e(&key), &rsa_get_e(&parsed_key));
        assert_be_eq(
            rsa_get_d(&key).expose_secret(),
            rsa_get_d(&parsed_key).expose_secret(),
        );

        // 2. Test extracting RsaKeyComponents directly
        let comps = rsa_key_components_from_pkcs8_der(der).unwrap();
        assert_be_eq(&comps.n, &rsa_get_n(&key));
        assert_be_eq(&comps.e, &rsa_get_e(&key));
        assert_be_eq(comps.d.expose_secret(), rsa_get_d(&key).expose_secret());
        assert_be_eq(
            comps.p.expose_secret(),
            rsa_get_primes(&key)[0].expose_secret(),
        );
        assert_be_eq(
            comps.q.expose_secret(),
            rsa_get_primes(&key)[1].expose_secret(),
        );
        assert_be_eq(comps.dp.expose_secret(), rsa_get_dp(&key).expose_secret());
        assert_be_eq(comps.dq.expose_secret(), rsa_get_dq(&key).expose_secret());
    }

    #[test]
    fn test_rsa_pkcs1_der_parsing() {
        let mut rng = DeterministicRng::seed_from_u64(49);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();

        // Encode key to PKCS#1 DER bytes
        let doc = key.to_pkcs1_der().unwrap();
        let der = doc.as_bytes();

        let parsed_key = rsa_private_key_from_pkcs1_der(der).unwrap();
        assert_be_eq(&rsa_get_n(&key), &rsa_get_n(&parsed_key));
        assert_be_eq(&rsa_get_e(&key), &rsa_get_e(&parsed_key));
        assert_be_eq(
            rsa_get_d(&key).expose_secret(),
            rsa_get_d(&parsed_key).expose_secret(),
        );

        // 2. Test extracting RsaKeyComponents directly
        let comps = rsa_key_components_from_pkcs1_der(der).unwrap();
        assert_be_eq(&comps.n, &rsa_get_n(&key));
        assert_be_eq(&comps.e, &rsa_get_e(&key));
        assert_be_eq(comps.d.expose_secret(), rsa_get_d(&key).expose_secret());
        assert_be_eq(
            comps.p.expose_secret(),
            rsa_get_primes(&key)[0].expose_secret(),
        );
        assert_be_eq(
            comps.q.expose_secret(),
            rsa_get_primes(&key)[1].expose_secret(),
        );
        assert_be_eq(comps.dp.expose_secret(), rsa_get_dp(&key).expose_secret());
        assert_be_eq(comps.dq.expose_secret(), rsa_get_dq(&key).expose_secret());
    }

    #[test]
    fn test_rsa_pkcs8_der_invalid_input() {
        assert_eq!(
            rsa_private_key_from_pkcs8_der(b"not valid der").err(),
            Some(CryptoError::Backend(
                tee_crypto::error::BackendError::RsaParseKey
            ))
        );
        assert_eq!(
            rsa_key_components_from_pkcs8_der(b"").err(),
            Some(CryptoError::Backend(
                tee_crypto::error::BackendError::RsaParseKey
            ))
        );
    }

    #[test]
    fn test_rsa_pkcs1_der_invalid_input() {
        assert_eq!(
            rsa_private_key_from_pkcs1_der(b"not valid der").err(),
            Some(CryptoError::Backend(
                tee_crypto::error::BackendError::RsaParseKey
            ))
        );
        assert_eq!(
            rsa_key_components_from_pkcs1_der(b"").err(),
            Some(CryptoError::Backend(
                tee_crypto::error::BackendError::RsaParseKey
            ))
        );
    }

    #[test]
    fn test_rsa_pkcs8_der_sign_verify_roundtrip() {
        let mut rng = DeterministicRng::seed_from_u64(50);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();
        let der = key.to_pkcs8_der().unwrap();

        let parsed = rsa_private_key_from_pkcs8_der(der.as_bytes()).unwrap();
        let pub_key = parsed.to_public_key();

        let msg = b"pkcs8 round-trip sign verify";
        let digest = sha256(msg);
        let sig = rsa_sign(
            &parsed,
            RsaHashAlgo::Sha256,
            RsaSignPadding::Pkcs1v15,
            &digest,
            &mut rng,
            None,
        )
        .unwrap();
        rsa_verify(
            &pub_key,
            RsaHashAlgo::Sha256,
            RsaSignPadding::Pkcs1v15,
            &digest,
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn test_rsa_pkcs1_der_encrypt_decrypt_roundtrip() {
        let mut rng = DeterministicRng::seed_from_u64(51);
        let key = rsa_keygen(&mut rng, 2048, 65537).unwrap();
        let der = key.to_pkcs1_der().unwrap();

        let parsed = rsa_private_key_from_pkcs1_der(der.as_bytes()).unwrap();
        let pub_key = parsed.to_public_key();

        let msg = b"pkcs1 round-trip encrypt decrypt";
        let ct = rsa_encrypt_oaep(&pub_key, RsaHashAlgo::Sha256, b"", msg, &mut rng).unwrap();
        let pt = rsa_decrypt_oaep(&parsed, RsaHashAlgo::Sha256, b"", &ct).unwrap();
        assert_eq!(pt.expose_secret(), msg);
    }
}

#[test]
fn test_rsa_keygen_small_sizes() {
    let mut rng = DeterministicRng::seed_from_u64(99);
    for bits in [256usize, 384, 512, 640, 768, 896] {
        rsa_keygen(&mut rng, bits, 65537).unwrap_or_else(|_| panic!("{bits}-bit RSA keygen"));
    }
}
