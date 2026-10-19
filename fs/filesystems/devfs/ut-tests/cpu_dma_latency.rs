// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_cpu_dma_latency_read_error() {
    let dev = CpuDmaLatency;
    let mut buf = [0u8; 4];
    assert!(dev.read_bytes(&mut buf).is_err());
}

#[def_test]
fn test_cpu_dma_latency_write_ok() {
    let dev = CpuDmaLatency;
    let data = [0u8; 4];
    let n = dev.write_bytes(&data).unwrap();
    assert_eq!(n, 4);
}
