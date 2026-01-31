//! Unit tests for Cursor.

#![cfg(unittest)]

extern crate alloc;
use alloc::vec;

use unittest::def_test;

use crate::Cursor;

#[def_test]
fn test_cursor_position_operations() {
    let data = vec![1u8, 2, 3, 4, 5];
    let mut cursor = Cursor::new(data);

    // Initial position
    assert_eq!(cursor.position(), 0);

    // Set position
    cursor.set_position(3);
    assert_eq!(cursor.position(), 3);

    // Set beyond length
    cursor.set_position(100);
    assert_eq!(cursor.position(), 100);
}

#[def_test]
fn test_cursor_split() {
    let data = b"Hello, World!";
    let mut cursor = Cursor::new(data.as_slice());

    // Split at start
    cursor.set_position(0);
    let (left, right) = cursor.split();
    assert_eq!(left.len(), 0);
    assert_eq!(right.len(), 13);

    // Split in middle
    cursor.set_position(7);
    let (left, right) = cursor.split();
    assert_eq!(left, b"Hello, ");
    assert_eq!(right, b"World!");

    // Split at end
    cursor.set_position(13);
    let (left, right) = cursor.split();
    assert_eq!(left.len(), 13);
    assert_eq!(right.len(), 0);

    // Split beyond end (should clamp to length)
    cursor.set_position(100);
    let (left, right) = cursor.split();
    assert_eq!(left.len(), 13);
    assert_eq!(right.len(), 0);
}

#[def_test]
fn test_cursor_accessors() {
    let data = vec![10u8, 20, 30];
    let mut cursor = Cursor::new(data);

    // get_ref
    assert_eq!(cursor.get_ref()[0], 10);

    // get_mut
    cursor.get_mut()[1] = 25;
    assert_eq!(cursor.get_ref()[1], 25);

    // into_inner
    let inner = cursor.into_inner();
    assert_eq!(inner, vec![10u8, 25, 30]);
}
