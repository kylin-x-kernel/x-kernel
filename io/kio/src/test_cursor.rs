// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

#[def_test]
fn test_cursor_read_basic() {
    use crate::Read;
    let data = b"Hello, World!";
    let mut cursor = Cursor::new(data.as_slice());

    let mut buf = [0u8; 5];
    let n = cursor.read(&mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf, b"Hello");
    assert_eq!(cursor.position(), 5);

    let n = cursor.read(&mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf, b", Wor");
    assert_eq!(cursor.position(), 10);

    let n = cursor.read(&mut buf).unwrap();
    assert_eq!(n, 3);
    assert_eq!(&buf[..3], b"ld!");
    assert_eq!(cursor.position(), 13);

    let n = cursor.read(&mut buf).unwrap();
    assert_eq!(n, 0);
}

#[def_test]
fn test_cursor_read_exact_ok() {
    use crate::Read;
    let data = b"ABCDEFGHIJ";
    let mut cursor = Cursor::new(data.as_slice());

    let mut buf = [0u8; 5];
    cursor.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ABCDE");
    assert_eq!(cursor.position(), 5);

    cursor.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"FGHIJ");
    assert_eq!(cursor.position(), 10);
}

#[def_test]
fn test_cursor_read_exact_eof() {
    use crate::Read;
    let data = b"AB";
    let mut cursor = Cursor::new(data.as_slice());

    let mut buf = [0u8; 5];
    assert!(cursor.read_exact(&mut buf).is_err());
}

#[def_test]
fn test_cursor_read_to_end() {
    use crate::Read;
    let data = b"Hello, World!";
    let mut cursor = Cursor::new(data.as_slice());
    cursor.set_position(7);

    let mut buf = vec![];
    let n = cursor.read_to_end(&mut buf).unwrap();
    assert_eq!(n, 6);
    assert_eq!(&buf, b"World!");
}

#[def_test]
fn test_cursor_read_to_string() {
    use crate::Read;
    let data = b"Hello!";
    let mut cursor = Cursor::new(data.as_slice());

    let mut s = alloc::string::String::new();
    let n = cursor.read_to_string(&mut s).unwrap();
    assert_eq!(n, 6);
    assert_eq!(s, "Hello!");
}

#[def_test]
fn test_cursor_write_slice() {
    use crate::Write;
    let mut buf = [0u8; 10];
    let mut cursor = Cursor::new(buf.as_mut_slice());

    let n = cursor.write(b"Hello").unwrap();
    assert_eq!(n, 5);
    assert_eq!(cursor.position(), 5);

    let n = cursor.write(b"World!!!!X").unwrap();
    assert_eq!(n, 5);
    assert_eq!(cursor.position(), 10);

    drop(cursor);
    assert_eq!(&buf, b"HelloWorld");
}

#[def_test]
fn test_cursor_write_vec() {
    use crate::Write;
    let mut v = vec![];
    let mut cursor = Cursor::new(&mut v);

    cursor.write_all(b"Hello ").unwrap();
    cursor.write_all(b"World").unwrap();
    assert_eq!(cursor.position(), 11);

    drop(cursor);
    assert_eq!(&v, b"Hello World");
}

#[def_test]
fn test_cursor_write_vec_overwrite() {
    use crate::Write;
    let mut v = vec![0u8; 5];
    let mut cursor = Cursor::new(&mut v);

    cursor.write_all(b"AB").unwrap();
    assert_eq!(cursor.position(), 2);

    drop(cursor);
    assert_eq!(&v[..2], b"AB");
    assert_eq!(v[2], 0);
}

#[def_test]
fn test_cursor_write_owned_vec() {
    use crate::Write;
    let mut cursor = Cursor::new(vec![]);

    cursor.write_all(b"test data").unwrap();
    assert_eq!(cursor.position(), 9);

    let inner = cursor.into_inner();
    assert_eq!(&inner, b"test data");
}

#[def_test]
fn test_cursor_write_fixed_array() {
    use crate::Write;
    let mut cursor = Cursor::new([0u8; 8]);

    let n = cursor.write(b"ABCD").unwrap();
    assert_eq!(n, 4);
    assert_eq!(cursor.position(), 4);

    let n = cursor.write(b"EFGH").unwrap();
    assert_eq!(n, 4);

    let inner = cursor.into_inner();
    assert_eq!(&inner, b"ABCDEFGH");
}

#[def_test]
fn test_cursor_write_all_overflow() {
    use crate::Write;
    let mut buf = [0u8; 3];
    let mut cursor = Cursor::new(buf.as_mut_slice());

    assert!(cursor.write_all(b"ABCDE").is_err());
}

#[def_test]
fn test_cursor_seek_operations() {
    use crate::Seek;
    let data = b"0123456789";
    let mut cursor = Cursor::new(data.as_slice());

    assert_eq!(cursor.seek(crate::SeekFrom::Start(5)).unwrap(), 5);
    assert_eq!(cursor.seek(crate::SeekFrom::Current(3)).unwrap(), 8);
    assert_eq!(cursor.seek(crate::SeekFrom::Current(-2)).unwrap(), 6);
    assert_eq!(cursor.seek(crate::SeekFrom::End(0)).unwrap(), 10);
    assert_eq!(cursor.seek(crate::SeekFrom::End(-3)).unwrap(), 7);
    assert_eq!(cursor.stream_len().unwrap(), 10);
    assert_eq!(cursor.stream_position().unwrap(), 7);

    cursor.rewind().unwrap();
    assert_eq!(cursor.position(), 0);
}

#[def_test]
fn test_cursor_bufread() {
    use crate::BufRead;
    let data = b"Hello\nWorld\n";
    let mut cursor = Cursor::new(data.as_slice());

    let buf = cursor.fill_buf().unwrap();
    assert_eq!(buf, b"Hello\nWorld\n");

    cursor.consume(6);
    let buf = cursor.fill_buf().unwrap();
    assert_eq!(buf, b"World\n");

    cursor.consume(6);
    let buf = cursor.fill_buf().unwrap();
    assert_eq!(buf.len(), 0);
}

#[def_test]
fn test_cursor_clone() {
    let data = vec![1u8, 2, 3];
    let mut original = Cursor::new(data);
    original.set_position(2);

    let cloned = original.clone();
    assert_eq!(cloned.position(), 2);
    assert_eq!(cloned.get_ref(), original.get_ref());
}

#[def_test]
fn test_cursor_split_mut() {
    let mut data = vec![1u8, 2, 3, 4, 5];
    let mut cursor = Cursor::new(data.as_mut_slice());
    cursor.set_position(3);

    let (left, right) = cursor.split_mut();
    assert_eq!(left, &[1, 2, 3]);
    assert_eq!(right, &[4, 5]);

    left[0] = 10;
    assert_eq!(left[0], 10);
}

#[def_test]
fn test_cursor_iobuf() {
    use crate::{IoBuf, IoBufMut};
    let data = b"Hello";
    let cursor = Cursor::new(data.as_slice());
    assert_eq!(cursor.remaining(), 5);

    let mut mdata = [0u8; 10];
    let mut cursor = Cursor::new(mdata.as_mut_slice());
    assert_eq!(cursor.remaining_mut(), 10);
    cursor.set_position(7);
    assert_eq!(cursor.remaining_mut(), 3);
}

#[def_test]
fn test_cursor_write_at_gap() {
    use crate::Write;
    let mut v = vec![];
    let mut cursor = Cursor::new(&mut v);
    cursor.set_position(5);
    cursor.write_all(b"AB").unwrap();

    drop(cursor);
    assert_eq!(v.len(), 7);
    assert_eq!(&v[..5], &[0, 0, 0, 0, 0]);
    assert_eq!(&v[5..], b"AB");
}
