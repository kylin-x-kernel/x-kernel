//! Unit tests for Seek operations.

#![cfg(unittest)]

use unittest::def_test;

use crate::{Cursor, Seek, SeekFrom};

#[def_test]
fn test_seek_from_variants() {
    // Test SeekFrom enum variants
    let start = SeekFrom::Start(100);
    let end = SeekFrom::End(-10);
    let current = SeekFrom::Current(5);

    // Verify they can be constructed and compared
    assert_eq!(start, SeekFrom::Start(100));
    assert_eq!(end, SeekFrom::End(-10));
    assert_eq!(current, SeekFrom::Current(5));
    assert_ne!(start, end);
}

#[def_test]
fn test_cursor_seek_boundary() {
    // Test seeking at boundaries
    let data = b"Hello, World!";
    let mut cursor = Cursor::new(data.as_slice());

    // Seek to start
    assert_eq!(cursor.seek(SeekFrom::Start(0)).unwrap(), 0);
    assert_eq!(cursor.position(), 0);

    // Seek to end
    assert_eq!(cursor.seek(SeekFrom::End(0)).unwrap(), 13);
    assert_eq!(cursor.position(), 13);

    // Seek beyond end (allowed)
    assert_eq!(cursor.seek(SeekFrom::Start(100)).unwrap(), 100);
    assert_eq!(cursor.position(), 100);

    // Seek with negative offset from end
    assert_eq!(cursor.seek(SeekFrom::End(-5)).unwrap(), 8);
    assert_eq!(cursor.position(), 8);
}

#[def_test]
fn test_cursor_seek_relative() {
    let data = b"0123456789";
    let mut cursor = Cursor::new(data.as_slice());

    // Start at position 5
    cursor.set_position(5);
    assert_eq!(cursor.position(), 5);

    // Seek forward
    cursor.seek_relative(3).unwrap();
    assert_eq!(cursor.position(), 8);

    // Seek backward
    cursor.seek_relative(-5).unwrap();
    assert_eq!(cursor.position(), 3);

    // Use stream_position
    assert_eq!(cursor.stream_position().unwrap(), 3);
}
