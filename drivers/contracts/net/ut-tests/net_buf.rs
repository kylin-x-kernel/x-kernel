// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network buffer and pool guest unit tests.

use unittest::def_test;
extern crate alloc;
use alloc::{vec, vec::Vec};
use core::ptr::NonNull;

use super::*;

#[def_test]
fn test_netbuf_boundary_conditions() {
    // Test NetBufHandle creation with boundary values
    let test_data = vec![0u8; 1024];
    let owner_ptr = NonNull::new(test_data.as_ptr() as *mut u8).unwrap();
    let data_ptr = NonNull::new(test_data.as_ptr() as *mut u8).unwrap();

    // Test valid sizes
    let valid_sizes = [0, 1, 512, 1024, MIN_BUFFER_LEN];
    for &size in &valid_sizes {
        if size <= test_data.len() {
            let handle = NetBufHandle::new(owner_ptr, data_ptr, size);
            assert_eq!(handle.len(), size);
            assert_eq!(handle.is_empty(), size == 0);
            assert_eq!(handle.data().len(), size);
        }
    }

    // Test boundary behavior with zero-length buffer
    let zero_handle = NetBufHandle::new(owner_ptr, data_ptr, 0);
    assert!(zero_handle.is_empty());
    assert_eq!(zero_handle.len(), 0);
    assert!(zero_handle.data().is_empty());

    // Test data manipulation at boundaries
    let mut data = vec![0u8; 100];
    let owner_ptr = NonNull::new(data.as_mut_ptr()).unwrap();
    let data_ptr = NonNull::new(data.as_mut_ptr()).unwrap();
    let mut handle = NetBufHandle::new(owner_ptr, data_ptr, data.len());

    // Fill with test pattern
    let pattern = 0xAB;
    for byte in handle.data_mut().iter_mut() {
        *byte = pattern;
    }

    // Verify pattern is written correctly
    for &byte in handle.data().iter() {
        assert_eq!(byte, pattern);
    }
}

#[def_test]
fn test_netbuf_pool_edge_cases() {
    // Test pool allocation with boundary conditions
    let pool_sizes = [1, 10, 64];
    let buffer_sizes = [MIN_BUFFER_LEN, MIN_BUFFER_LEN + 100, 2048];

    for pool_size in pool_sizes {
        for buffer_size in buffer_sizes {
            // Test pool creation
            let pool_result = NetBufPool::new(pool_size, buffer_size);
            assert!(pool_result.is_ok(), "Pool creation should succeed");
            let pool = pool_result.unwrap();

            // Test allocation until exhaustion
            let mut allocated_buffers = Vec::new();

            // Allocate all available buffers
            for _ in 0..pool_size {
                match pool.alloc_buf() {
                    Some(buf) => {
                        // Verify buffer properties
                        assert_eq!(buf.capacity(), buffer_size);
                        allocated_buffers.push(buf);
                    }
                    None => {
                        // Expected when pool is exhausted
                        break;
                    }
                }
            }

            // Test buffer content manipulation
            for buf in allocated_buffers.iter_mut().take(3) {
                // Test payload manipulation - set a small payload first
                buf.set_payload_len(100).unwrap();
                let payload = buf.payload_mut();
                if !payload.is_empty() {
                    // Fill with test pattern
                    payload.fill(0x5A);

                    // Verify pattern
                    for &byte in buf.payload() {
                        assert_eq!(byte, 0x5A);
                    }
                }

                // Test payload length adjustment
                let original_payload_len = buf.payload_len();
                if original_payload_len > 10 {
                    buf.set_payload_len(original_payload_len - 5).unwrap();
                    assert_eq!(buf.payload_len(), original_payload_len - 5);
                }
            }

            // Drop all buffers (should return them to pool)
            drop(allocated_buffers);
        }
    }
}
