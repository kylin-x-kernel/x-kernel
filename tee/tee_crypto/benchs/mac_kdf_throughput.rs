// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tee_crypto::{
    hkdf::hkdf,
    mac::{Aes128Cmac, Aes256Cmac, Des3Cmac, HmacSha256, HmacSha512, HmacSm3, Mac, Sm4Cmac},
};

#[path = "common.rs"]
mod common;

fn bench_mac<M: Mac>(c: &mut Criterion, name: &'static str, key: &'static [u8]) {
    let mut group = c.benchmark_group(format!("mac/{name}"));

    for &size in common::SIZES {
        let data = common::input(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            b.iter(|| {
                let mut mac = M::new(black_box(key)).expect("mac init");
                mac.update(black_box(data));
                black_box(mac.finalize());
            });
        });
    }

    group.finish();
}

fn mac_throughput(c: &mut Criterion) {
    bench_mac::<HmacSha256>(c, "hmac-sha256", &[0x77; 32]);
    bench_mac::<HmacSha512>(c, "hmac-sha512", &[0x77; 32]);
    bench_mac::<HmacSm3>(c, "hmac-sm3", &[0x77; 32]);
    bench_mac::<Aes128Cmac>(c, "aes128-cmac", &[0x88; 16]);
    bench_mac::<Aes256Cmac>(c, "aes256-cmac", &[0x88; 32]);
    bench_mac::<Sm4Cmac>(c, "sm4-cmac", &[0x88; 16]);
    bench_mac::<Des3Cmac>(c, "3des-cmac", &[0x88; 24]);
}

fn hkdf_throughput(c: &mut Criterion) {
    let salt = [0x99; 32];
    let info = b"tee_crypto throughput hkdf";
    let mut group = c.benchmark_group("hkdf/hmac-sha256");

    for &size in common::SIZES {
        let ikm = common::input(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &ikm, |b, ikm| {
            b.iter(|| {
                let okm = hkdf::<HmacSha256>(black_box(&salt), black_box(ikm), black_box(info), 64)
                    .expect("hkdf");
                black_box(okm);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, mac_throughput, hkdf_throughput);
criterion_main!(benches);
