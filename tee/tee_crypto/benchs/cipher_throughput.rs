// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tee_crypto::cipher::{
    aes128_cbc_decrypt, aes128_cbc_encrypt, aes128_ctr, aes256_cbc_decrypt, aes256_cbc_encrypt,
    aes256_ctr, des_cbc_decrypt, des_cbc_encrypt, des3_cbc_decrypt, des3_cbc_encrypt,
    sm4_cbc_decrypt, sm4_cbc_encrypt, sm4_ctr,
};

#[path = "common.rs"]
mod common;

fn bench_cbc_pair(
    c: &mut Criterion,
    name: &'static str,
    key: &'static [u8],
    iv: &'static [u8],
    encrypt: fn(&[u8], &[u8], &[u8]) -> tee_crypto::Result<Vec<u8>>,
    decrypt: fn(&[u8], &[u8], &[u8]) -> tee_crypto::Result<Vec<u8>>,
) {
    let mut group = c.benchmark_group(format!("cipher/cbc/{name}"));

    for &size in common::SIZES {
        let plaintext = common::input(size);
        let ciphertext = encrypt(key, iv, &plaintext).expect("cbc encrypt fixture");

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("encrypt", size),
            &plaintext,
            |b, plaintext| {
                b.iter(|| {
                    let ciphertext = encrypt(black_box(key), black_box(iv), black_box(plaintext))
                        .expect("cbc encrypt");
                    black_box(ciphertext);
                });
            },
        );

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("decrypt", size),
            &ciphertext,
            |b, ciphertext| {
                b.iter(|| {
                    let plaintext = decrypt(black_box(key), black_box(iv), black_box(ciphertext))
                        .expect("cbc decrypt");
                    black_box(plaintext);
                });
            },
        );
    }

    group.finish();
}

fn bench_ctr(
    c: &mut Criterion,
    name: &'static str,
    key: &'static [u8],
    iv: &'static [u8],
    apply: fn(&[u8], &[u8], &mut [u8]) -> tee_crypto::Result<()>,
) {
    let mut group = c.benchmark_group(format!("cipher/ctr/{name}"));

    for &size in common::SIZES {
        let data = common::input(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            b.iter_batched(
                || data.clone(),
                |mut data| {
                    apply(black_box(key), black_box(iv), black_box(&mut data)).expect("ctr apply");
                    black_box(data);
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn cipher_throughput(c: &mut Criterion) {
    bench_cbc_pair(
        c,
        "aes128",
        &[0x11; 16],
        &[0x22; 16],
        aes128_cbc_encrypt,
        aes128_cbc_decrypt,
    );
    bench_cbc_pair(
        c,
        "aes256",
        &[0x11; 32],
        &[0x22; 16],
        aes256_cbc_encrypt,
        aes256_cbc_decrypt,
    );
    bench_cbc_pair(
        c,
        "sm4",
        &[0x11; 16],
        &[0x22; 16],
        sm4_cbc_encrypt,
        sm4_cbc_decrypt,
    );
    bench_cbc_pair(
        c,
        "des",
        &[0x11; 8],
        &[0x22; 8],
        des_cbc_encrypt,
        des_cbc_decrypt,
    );
    bench_cbc_pair(
        c,
        "3des",
        &[0x11; 24],
        &[0x22; 8],
        des3_cbc_encrypt,
        des3_cbc_decrypt,
    );

    bench_ctr(c, "aes128", &[0x33; 16], &[0x44; 16], aes128_ctr);
    bench_ctr(c, "aes256", &[0x33; 32], &[0x44; 16], aes256_ctr);
    bench_ctr(c, "sm4", &[0x33; 16], &[0x44; 16], sm4_ctr);
}

criterion_group!(benches, cipher_throughput);
criterion_main!(benches);
