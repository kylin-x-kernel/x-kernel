// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tee_crypto::xts::{Aes128Xts, Aes256Xts, Sm4Xts, XtsCipher};

#[path = "common.rs"]
mod common;

fn bench_xts<X: XtsCipher>(c: &mut Criterion, name: &'static str, key: &'static [u8]) {
    let tweak = [0xaau8; 16];
    let mut group = c.benchmark_group(format!("xts/{name}"));

    for &size in common::SIZES {
        let plaintext = common::input(size);
        let mut ciphertext = plaintext.clone();
        X::encrypt(key, &tweak, &mut ciphertext).expect("xts encrypt fixture");

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("encrypt", size),
            &plaintext,
            |b, plaintext| {
                b.iter_batched(
                    || plaintext.clone(),
                    |mut data| {
                        X::encrypt(black_box(key), black_box(&tweak), black_box(&mut data))
                            .expect("xts encrypt");
                        black_box(data);
                    },
                    BatchSize::SmallInput,
                );
            },
        );

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("decrypt", size),
            &ciphertext,
            |b, ciphertext| {
                b.iter_batched(
                    || ciphertext.clone(),
                    |mut data| {
                        X::decrypt(black_box(key), black_box(&tweak), black_box(&mut data))
                            .expect("xts decrypt");
                        black_box(data);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn xts_throughput(c: &mut Criterion) {
    bench_xts::<Aes128Xts>(c, "aes128", &[0xbb; 32]);
    bench_xts::<Aes256Xts>(c, "aes256", &[0xbb; 64]);
    bench_xts::<Sm4Xts>(c, "sm4", &[0xbb; 32]);
}

criterion_group!(benches, xts_throughput);
criterion_main!(benches);
