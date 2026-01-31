//! Unit tests for IoBuf and IoBufMut.

#![cfg(unittest)]

extern crate alloc;
use alloc::vec;

use unittest::def_test;

use crate::{Cursor, IoBuf, IoBufMut};

#[def_test]
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

#[def_test]
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
