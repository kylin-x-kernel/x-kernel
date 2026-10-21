// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::{
    CryptoError,
    rng::DeterministicRng,
    x448::{X448_KEY_SIZE_BYTES, x448_generate_keypair, x448_shared_secret},
};

#[test]
fn x448_shared_secret_symmetry() {
    let mut rng = DeterministicRng::seed_from_u64(0xA5A5);
    let (alice_priv, alice_pub) = x448_generate_keypair(&mut rng).expect("alice keygen");
    let (bob_priv, bob_pub) = x448_generate_keypair(&mut rng).expect("bob keygen");

    let secret_a = x448_shared_secret(&alice_priv, &bob_pub).expect("alice shared");
    let secret_b = x448_shared_secret(&bob_priv, &alice_pub).expect("bob shared");
    assert_eq!(secret_a, secret_b);
}

#[test]
fn x448_rejects_low_order_public_key() {
    let mut rng = DeterministicRng::seed_from_u64(0xB5B5);
    let (private, _) = x448_generate_keypair(&mut rng).expect("keygen");
    let low_order_public = [0u8; X448_KEY_SIZE_BYTES];
    assert_eq!(
        x448_shared_secret(&private, &low_order_public).unwrap_err(),
        CryptoError::InvalidKey
    );
}
