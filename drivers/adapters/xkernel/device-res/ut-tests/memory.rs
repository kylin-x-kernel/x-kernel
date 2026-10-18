// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::alloc::Layout;

use device_res::CpuBuffer;
use unittest::def_test;

use super::*;

#[def_test]
fn cpu_storage_uses_fallible_normal_allocator_for_small_and_bounded_buffers() {
    let provider = XKernelResourceProvider;
    for (size, align) in [(512, 4), (64 * 1024, 4), (513, 64)] {
        let mut buffer =
            CpuBuffer::alloc_with(&provider, Layout::from_size_align(size, align).unwrap())
                .unwrap();
        let bytes = buffer.as_mut_slice();
        assert_eq!(bytes.len(), size);
        assert!((bytes.as_ptr() as usize).is_multiple_of(align));
        assert!(bytes.iter().all(|&byte| byte == 0));
        bytes.fill(0x73);
        assert_eq!(bytes[size - 1], 0x73);
    }
}
