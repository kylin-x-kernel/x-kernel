// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tee_crypto::aead::{Aead, Aes128CcmAead, Aes128GcmAead, Aes256GcmAead, Sm4GcmAead};

#[path = "common.rs"]
mod common;

const AAD: &[u8] = b"tee_crypto throughput aad";

fn bench_aead<A: Aead>(
    c: &mut Criterion,
    name: &'static str,
    key: &'static [u8],
    nonce: &'static [u8],
) {
    let mut group = c.benchmark_group(format!("aead/{name}"));

    for &size in common::SIZES {
        let plaintext = common::input(size);
        let ciphertext = A::encrypt(key, nonce, AAD, &plaintext).expect("aead encrypt fixture");

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("encrypt", size),
            &plaintext,
            |b, plaintext| {
                b.iter(|| {
                    let ciphertext = A::encrypt(
                        black_box(key),
                        black_box(nonce),
                        black_box(AAD),
                        black_box(plaintext),
                    )
                    .expect("aead encrypt");
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
                    let plaintext = A::decrypt(
                        black_box(key),
                        black_box(nonce),
                        black_box(AAD),
                        black_box(ciphertext),
                    )
                    .expect("aead decrypt");
                    black_box(plaintext);
                });
            },
        );
    }

    group.finish();
}

fn aead_throughput(c: &mut Criterion) {
    bench_aead::<Aes128GcmAead>(c, "aes128-gcm", &[0x55; 16], &[0x66; 12]);
    bench_aead::<Aes256GcmAead>(c, "aes256-gcm", &[0x55; 32], &[0x66; 12]);
    bench_aead::<Sm4GcmAead>(c, "sm4-gcm", &[0x55; 16], &[0x66; 12]);
    bench_aead::<Aes128CcmAead>(c, "aes128-ccm", &[0x55; 16], &[0x66; 12]);
}

criterion_group!(benches, aead_throughput);
criterion_main!(benches);
