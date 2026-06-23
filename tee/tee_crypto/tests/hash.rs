// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use tee_crypto::{hash::*, md5::Md5};

#[test]
fn test_hash_algorithm_metadata() {
    assert_eq!(HashAlgorithm::Sha256.name(), "SHA-256");
    assert_eq!(HashAlgorithm::Sha256.output_size(), Sha256::output_size());
    assert_eq!(HashAlgorithm::Sha512.spec().block_size, 128);
    assert_eq!(HashAlgorithm::Sm3.spec().output_size, Sm3::output_size());
}
extern crate alloc;

fn hex_encode(data: &[u8]) -> alloc::string::String {
    hex::encode(data)
}

#[test]
fn test_sha256_empty() {
    let h = Sha256::new();
    let result = h.finalize();
    assert_eq!(
        hex_encode(&result),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn test_sha256_abc() {
    let mut h = Sha256::new();
    h.update(b"abc");
    let result = h.finalize();
    assert_eq!(
        hex_encode(&result),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn test_sm3_abc() {
    let mut h = Sm3::new();
    h.update(b"abc");
    let result = h.finalize();
    assert_eq!(
        hex_encode(&result),
        "66c7f0f462eeedd9d1f2d46bdc10e4e24167c4875cf2f7a2297da02b8f4ba8e0"
    );
}

#[test]
fn test_output_sizes() {
    assert_eq!(Sha256::output_size(), 32);
    assert_eq!(Sha512::output_size(), 64);
    assert_eq!(Sm3::output_size(), 32);
    assert_eq!(Sha1::output_size(), 20);
}

#[test]
fn test_names() {
    assert_eq!(Sha256::name(), "SHA-256");
    assert_eq!(Sha512::name(), "SHA-512");
    assert_eq!(Sm3::name(), "SM3");
    assert_eq!(Sha1::name(), "SHA-1");
}

#[test]
fn test_sha512_empty() {
    let h = Sha512::new();
    let result = h.finalize();
    assert_eq!(result.len(), 64);
}

#[test]
fn test_sha1_empty() {
    let h = Sha1::new();
    let result = h.finalize();
    assert_eq!(result.len(), 20);
}

#[test]
fn test_incremental_update() {
    let mut h1 = Sha256::new();
    h1.update(b"hello");
    h1.update(b" world");

    let mut h2 = Sha256::new();
    h2.update(b"hello world");

    assert_eq!(h1.finalize(), h2.finalize());
}

#[test]
fn test_sha224_abc() {
    let mut h = Sha224::new();
    h.update(b"abc");
    let result = h.finalize();
    assert_eq!(
        hex_encode(&result),
        "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
    );
}

#[test]
fn test_sha384_abc() {
    let mut h = Sha384::new();
    h.update(b"abc");
    let result = h.finalize();
    assert_eq!(
        hex_encode(&result),
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
             8086072ba1e7cc2358baeca134c825a7"
    );
}

#[test]
fn test_md5_empty() {
    let h = Md5::new();
    let result = h.finalize();
    assert_eq!(hex_encode(&result), "d41d8cd98f00b204e9800998ecf8427e");
}

#[test]
fn test_md5_rfc1321_vectors() {
    let cases = [
        ("", "d41d8cd98f00b204e9800998ecf8427e"),
        ("a", "0cc175b9c0f1b6a831c399e269772661"),
        ("abc", "900150983cd24fb0d6963f7d28e17f72"),
        ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
        (
            "abcdefghijklmnopqrstuvwxyz",
            "c3fcd3d76192e4007dfb496cca67e13b",
        ),
        (
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
            "d174ab98d277d9f5a5611c2c9f419d9f",
        ),
        (
            "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
            "57edf4a22be3c955ac49da2e2107b67a",
        ),
    ];

    for (input, expected) in cases {
        let mut h = Md5::new();
        h.update(input.as_bytes());
        assert_eq!(hex_encode(&h.finalize()), expected);
    }
}

#[test]
fn test_md5_incremental() {
    let mut h1 = Md5::new();
    h1.update(b"hello");
    h1.update(b" world");

    let mut h2 = Md5::new();
    h2.update(b"hello world");

    assert_eq!(h1.finalize(), h2.finalize());
}

#[test]
fn test_md5_chunked_rfc1321_vector() {
    let mut h = Md5::new();
    for chunk in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789".chunks(7) {
        h.update(chunk);
    }
    assert_eq!(
        hex_encode(&h.finalize()),
        "d174ab98d277d9f5a5611c2c9f419d9f"
    );
}

#[test]
fn test_md5_million_a() {
    let mut h = Md5::new();
    let chunk = [b'a'; 1000];
    for _ in 0..1000 {
        h.update(&chunk);
    }
    assert_eq!(
        hex_encode(&h.finalize()),
        "7707d6ae4e027c70eea2a935c2296f21"
    );
}

#[test]
fn test_new_output_sizes() {
    assert_eq!(Sha224::output_size(), 28);
    assert_eq!(Sha384::output_size(), 48);
    assert_eq!(Md5::output_size(), 16);
}
