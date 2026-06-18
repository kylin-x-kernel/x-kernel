// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tee_crypto::{
    hash::{Digest, Sha1, Sha224, Sha256, Sha384, Sha512, Sm3},
    md5::Md5,
};

#[path = "common.rs"]
mod common;

fn bench_digest<D: Digest>(c: &mut Criterion, name: &'static str) {
    let mut group = c.benchmark_group(format!("hash/{name}"));

    for &size in common::SIZES {
        let data = common::input(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &data, |b, data| {
            b.iter(|| {
                let mut digest = D::new();
                digest.update(black_box(data));
                black_box(digest.finalize());
            });
        });
    }

    group.finish();
}

fn hash_throughput(c: &mut Criterion) {
    bench_digest::<Md5>(c, "md5");
    bench_digest::<Sha1>(c, "sha1");
    bench_digest::<Sha224>(c, "sha224");
    bench_digest::<Sha256>(c, "sha256");
    bench_digest::<Sha384>(c, "sha384");
    bench_digest::<Sha512>(c, "sha512");
    bench_digest::<Sm3>(c, "sm3");
}

criterion_group!(benches, hash_throughput);
criterion_main!(benches);
