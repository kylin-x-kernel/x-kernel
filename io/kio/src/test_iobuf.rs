//! Unit tests for IoBuf and IoBufMut.

#![allow(missing_docs)]

extern crate alloc;
use alloc::vec;

use unittest::{
    test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
};

use crate::{Cursor, IoBuf, IoBufMut};

test_fn! {
    using TestResult;

    fn test_iobuf_remaining() {
        // Test with slice
        let data = b"Hello";
        let cursor = Cursor::new(data.as_slice());
        assert_eq!(cursor.remaining(), 5);
        assert!(!cursor.is_empty());

        // Test with empty slice
        let empty: &[u8] = &[];
        let cursor = Cursor::new(empty);
        assert_eq!(cursor.remaining(), 0);
        assert!(cursor.is_empty());
    }
}

test_fn! {
    using TestResult;

    fn test_iobufmut_remaining() {
        let mut data = vec![0u8; 10];
        let mut cursor = Cursor::new(data.as_mut_slice());

        // Initial state
        assert_eq!(cursor.remaining_mut(), 10);
        assert!(!cursor.is_full());

        // After position change
        cursor.set_position(7);
        assert_eq!(cursor.remaining_mut(), 3);

        // At end
        cursor.set_position(10);
        assert_eq!(cursor.remaining_mut(), 0);
        assert!(cursor.is_full());
    }
}

tests_name!(TEST_IOBUF;
    test_iobuf_remaining,
    test_iobufmut_remaining,
);
