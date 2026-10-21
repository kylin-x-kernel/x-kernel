// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Serial read-side batch costs without deliberate updater pressure.

use alloc::{boxed::Box, vec::Vec};
use core::hint::black_box;

use kcpu_id_map::LogicalCpuId;

const OPERATIONS: usize = 1024;
const VALUE: u64 = 7;
const METRICS: [&str; 3] = ["baseline_batch", "empty_scope_batch", "object_read_batch"];

fn measure_batch(mut operation_fn: impl FnMut() -> u64) -> u64 {
    let mut sum = 0_u64;
    let begin = khal::time::monotonic_time_nanos();
    for _ in 0..OPERATIONS {
        sum += operation_fn();
    }
    let end = khal::time::monotonic_time_nanos();
    assert_eq!(sum, VALUE * OPERATIONS as u64);
    assert!(end >= begin);
    end - begin
}

pub(super) fn measure() -> Vec<[u64; 3]> {
    let value = crate::Rcu::new(Box::new(VALUE));
    let mut samples = Vec::with_capacity(super::SAMPLES);
    for index in 0..super::SAMPLES {
        let mut elapsed_ns = [0; 3];
        // Rotate order to avoid always assigning first-batch warmup to baseline.
        for offset in 0..3 {
            let metric = (index + offset) % 3;
            elapsed_ns[metric] = match metric {
                0 => measure_batch(|| black_box(VALUE)),
                1 => measure_batch(|| {
                    crate::with_read(|scope| {
                        black_box(scope);
                        black_box(VALUE)
                    })
                }),
                _ => measure_batch(|| {
                    crate::with_read(|scope| black_box(*black_box(&value).read(scope)))
                }),
            };
        }
        samples.push(elapsed_ns);
    }
    samples
}

pub(super) fn report(cpu: LogicalCpuId, round: usize, samples: &[[u64; 3]]) {
    assert_eq!(samples.len(), super::SAMPLES);
    for (metric_index, metric) in METRICS.iter().enumerate() {
        for (index, sample) in samples.iter().enumerate() {
            klogger::kprintln!(
                "RCU_READ_SAMPLE cpu={} round={round} index={index} metric={metric} \
                 operations={OPERATIONS} elapsed_ns={}",
                cpu.as_usize(),
                sample[metric_index]
            );
        }
        super::report(
            cpu,
            round,
            metric,
            samples.iter().map(|sample| sample[metric_index]).collect(),
        );
    }
}
