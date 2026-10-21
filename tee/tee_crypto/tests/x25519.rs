// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::{
    CryptoError,
    rng::DeterministicRng,
    x25519::{X25519_KEY_SIZE_BYTES, x25519_generate_keypair, x25519_shared_secret},
};

#[test]
fn x25519_shared_secret_symmetry() {
    let mut rng = DeterministicRng::seed_from_u64(0xA5A5);
    let (alice_priv, alice_pub) = x25519_generate_keypair(&mut rng).expect("alice keygen");
    let (bob_priv, bob_pub) = x25519_generate_keypair(&mut rng).expect("bob keygen");

    let secret_a = x25519_shared_secret(&alice_priv, &bob_pub).expect("alice shared");
    let secret_b = x25519_shared_secret(&bob_priv, &alice_pub).expect("bob shared");
    assert_eq!(secret_a, secret_b);
}

#[test]
fn x25519_rejects_all_zero_shared_secret() {
    let mut private = [0u8; X25519_KEY_SIZE_BYTES];
    private[0] = 1;
    let low_order_public = [0u8; X25519_KEY_SIZE_BYTES];
    assert_eq!(
        x25519_shared_secret(&private, &low_order_public).unwrap_err(),
        CryptoError::VerificationFailed
    );
}

/// RFC 7748 §6.1 X25519 test vector.
///
/// These scalars are not reduced modulo the group order before multiplication.
/// A `from_bytes_mod_order` implementation still matches this vector on the
/// base point, so the low-order cases below are what lock in cofactor clearing.
#[test]
fn x25519_rfc7748_vector() {
    let alice_private = hex32("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    let alice_public = hex32("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
    let bob_private = hex32("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
    let bob_public = hex32("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
    let shared = hex32("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");

    assert_eq!(
        x25519_shared_secret(&alice_private, &X25519_BASEPOINT).expect("alice public"),
        alice_public
    );
    assert_eq!(
        x25519_shared_secret(&bob_private, &X25519_BASEPOINT).expect("bob public"),
        bob_public
    );
    assert_eq!(
        x25519_shared_secret(&alice_private, &bob_public).expect("alice shared"),
        shared
    );
    assert_eq!(
        x25519_shared_secret(&bob_private, &alice_public).expect("bob shared"),
        shared
    );
}

/// Low-order u-coordinates other than 0. Both encode to a non-zero input, and
/// both must still be rejected after cofactor clearing. `u = 0` is not enough:
/// it is the all-zero encoding either before or after reduction modulo the
/// group order.
#[test]
fn x25519_rejects_low_order_points() {
    let mut private = [0u8; X25519_KEY_SIZE_BYTES];
    private[0] = 9;
    // u = 1, and one order-8 point from the Curve25519 small-order set.
    let points = [
        {
            let mut point = [0u8; X25519_KEY_SIZE_BYTES];
            point[0] = 1;
            point
        },
        [
            0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f,
            0xc4, 0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16,
            0x5f, 0x49, 0xb8, 0x00,
        ],
    ];
    for point in points {
        assert_eq!(
            x25519_shared_secret(&private, &point).unwrap_err(),
            CryptoError::VerificationFailed
        );
    }
}

/// RFC 7748 base point u = 9.
const X25519_BASEPOINT: [u8; X25519_KEY_SIZE_BYTES] = [
    9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

fn hex32(text: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex byte");
    }
    out
}
