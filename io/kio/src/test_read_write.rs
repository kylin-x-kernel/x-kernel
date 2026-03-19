// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for Read, Write, BufReader, BufWriter, and helper utilities.

#![cfg(unittest)]

extern crate alloc;
use alloc::{boxed::Box, collections::VecDeque, string::String, vec, vec::Vec};

use unittest::def_test;

use crate::{
    BufRead, BufReader, BufWriter, Cursor, Error, LineWriter, Read, Seek, SeekFrom, Take, Write,
};

// ============ Read trait helpers ============

#[def_test]
fn test_read_exact_basic() {
    let data = b"ABCDEFGHIJ";
    let mut reader = data.as_slice();
    let mut buf = [0u8; 5];
    reader.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ABCDE");
}

#[def_test]
fn test_read_to_end() {
    let data = b"Hello, World!";
    let mut reader = data.as_slice();
    let mut buf = Vec::new();
    let n = reader.read_to_end(&mut buf).unwrap();
    assert_eq!(n, 13);
    assert_eq!(&buf, data.as_slice());
}

#[def_test]
fn test_read_to_string() {
    let data = b"Hello";
    let mut reader = data.as_slice();
    let mut s = String::new();
    let n = reader.read_to_string(&mut s).unwrap();
    assert_eq!(n, 5);
    assert_eq!(s, "Hello");
}

// ============ Take adapter ============

#[def_test]
fn test_take_limits_read() {
    let data = b"Hello, World!";
    let mut take = data.as_slice().take(5);

    let mut buf = [0u8; 10];
    let n = take.read(&mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf[..5], b"Hello");
    assert_eq!(take.limit(), 0);
}

#[def_test]
fn test_take_read_to_end() {
    let data = b"Hello, World!";
    let mut take = data.as_slice().take(5);

    let mut buf = Vec::new();
    let n = take.read_to_end(&mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf, b"Hello");
}

#[def_test]
fn test_take_set_limit() {
    let data = b"ABCDE";
    let mut take = data.as_slice().take(3);
    assert_eq!(take.limit(), 3);

    take.set_limit(2);
    assert_eq!(take.limit(), 2);

    let mut buf = [0u8; 5];
    let n = take.read(&mut buf).unwrap();
    assert_eq!(n, 2);
}

#[def_test]
fn test_take_accessors_and_position() {
    let mut inner = Cursor::new(b"abcdef".as_slice());
    let mut take = Take::new(&mut inner, 4);

    assert_eq!(take.limit(), 4);
    assert_eq!(take.position(), 0);
    assert_eq!(take.get_ref().position(), 0);
    take.get_mut().set_position(1);
    assert_eq!(take.get_ref().position(), 1);
}

#[def_test]
fn test_take_fill_buf_and_consume() {
    let data = b"abcdef";
    let mut take = data.as_slice().take(4);

    assert_eq!(take.fill_buf().unwrap(), b"abcd");
    take.consume(2);
    assert_eq!(take.position(), 2);
    assert_eq!(take.fill_buf().unwrap(), b"cd");
    take.consume(10);
    assert_eq!(take.limit(), 0);
    assert_eq!(take.fill_buf().unwrap(), b"");
}

#[def_test]
fn test_take_seek_and_remaining() {
    use crate::IoBuf;

    let cursor = Cursor::new(b"abcdef".as_slice());
    let mut take = cursor.take(4);

    assert_eq!(take.remaining(), 4);
    assert_eq!(take.seek(SeekFrom::Start(2)).unwrap(), 2);
    assert_eq!(take.position(), 2);
    assert_eq!(take.remaining(), 2);
    take.seek_relative(-1).unwrap();
    assert_eq!(take.position(), 1);
    assert_eq!(take.stream_len().unwrap(), 4);
    assert_eq!(take.stream_position().unwrap(), 1);
}

#[def_test]
fn test_take_into_inner() {
    let cursor = Cursor::new(b"abcdef".as_slice());
    let take = cursor.take(3);
    let inner = take.into_inner();
    assert_eq!(inner.position(), 0);
}

// ============ Chain adapter ============

#[def_test]
fn test_chain_read() {
    let a = b"Hello, ";
    let b = b"World!";
    let mut chain = a.as_slice().chain(b.as_slice());

    let mut buf = Vec::new();
    let n = chain.read_to_end(&mut buf).unwrap();
    assert_eq!(n, 13);
    assert_eq!(&buf, b"Hello, World!");
}

#[def_test]
fn test_chain_partial_read() {
    let a = b"AB";
    let b = b"CD";
    let mut chain = a.as_slice().chain(b.as_slice());

    let mut buf = [0u8; 3];
    let n = chain.read(&mut buf).unwrap();
    assert_eq!(n, 2);
    assert_eq!(&buf[..2], b"AB");

    let n = chain.read(&mut buf).unwrap();
    assert_eq!(n, 2);
    assert_eq!(&buf[..2], b"CD");

    let n = chain.read(&mut buf).unwrap();
    assert_eq!(n, 0);
}

#[def_test]
fn test_chain_accessors() {
    let mut chain = Cursor::new(b"ab".as_slice()).chain(Cursor::new(b"cd".as_slice()));
    assert_eq!(chain.get_ref().0.position(), 0);
    chain.get_mut().0.set_position(1);
    assert_eq!(chain.get_ref().0.position(), 1);

    let (first, second) = chain.into_inner();
    assert_eq!(first.position(), 1);
    assert_eq!(second.position(), 0);
}

#[def_test]
fn test_chain_fill_buf_and_consume() {
    let first = Cursor::new(b"ab".as_slice());
    let second = Cursor::new(b"cd".as_slice());
    let mut chain = first.chain(second);

    assert_eq!(chain.fill_buf().unwrap(), b"ab");
    chain.consume(2);
    assert_eq!(chain.fill_buf().unwrap(), b"cd");
}

#[def_test]
fn test_chain_read_until() {
    let first = Cursor::new(b"ab".as_slice());
    let second = Cursor::new(b"cd\nef".as_slice());
    let mut chain = first.chain(second);
    let mut out = Vec::new();

    let n = chain.read_until(b'\n', &mut out).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&out, b"abcd\n");
}

#[def_test]
fn test_chain_remaining() {
    use crate::IoBuf;

    let first = Cursor::new(b"abc".as_slice());
    let second = Cursor::new(b"de".as_slice());
    let chain = first.chain(second);
    assert_eq!(chain.remaining(), 5);
}

// ============ Write trait helpers ============

#[def_test]
fn test_write_to_vec() {
    let mut buf = Vec::new();
    buf.write_all(b"Hello").unwrap();
    buf.write_all(b" World").unwrap();
    assert_eq!(&buf, b"Hello World");
}

#[def_test]
fn test_write_fmt() {
    use core::fmt::Write as FmtWrite;
    let mut s = String::new();
    core::write!(&mut s, "num={}", 42).unwrap();
    assert_eq!(s, "num=42");
}

#[def_test]
fn test_kio_write_fmt_default_impl() {
    let mut buf = Vec::new();
    buf.write_fmt(format_args!("hello {}", 7)).unwrap();
    assert_eq!(&buf, b"hello 7");
}

#[def_test]
fn test_write_impls_for_slice_and_vecdeque() {
    let mut storage = [0u8; 4];
    let mut slice = storage.as_mut_slice();
    assert_eq!(slice.write(b"ab").unwrap(), 2);
    assert_eq!(slice.write_all(b"cde"), Err(crate::Error::WriteZero));
    assert_eq!(&storage[..], b"abcd");

    let mut deque = VecDeque::new();
    deque.write_all(b"xyz").unwrap();
    assert_eq!(deque.len(), 3);
}

#[def_test]
fn test_write_by_ref() {
    let mut buf = Vec::new();
    let writer = buf.by_ref();
    writer.write_all(b"abc").unwrap();
    assert_eq!(&buf, b"abc");
}

// ============ BufReader ============

#[def_test]
fn test_bufreader_basic() {
    let data = b"Hello, World! This is a test of buffered reading.";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(8, cursor);

    let mut buf = [0u8; 5];
    let n = reader.read(&mut buf).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&buf, b"Hello");
}

#[def_test]
fn test_bufreader_fill_buf() {
    let data = b"ABCDEFGHIJKLMNOP";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(8, cursor);

    let buf = reader.fill_buf().unwrap();
    assert_eq!(buf.len(), 8);
    assert_eq!(&buf[..8], b"ABCDEFGH");

    reader.consume(3);
    let buf = reader.fill_buf().unwrap();
    assert_eq!(&buf[..5], b"DEFGH");
}

#[def_test]
fn test_bufreader_read_to_end() {
    let data = b"Hello, World!";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(4, cursor);

    let mut result = Vec::new();
    let n = reader.read_to_end(&mut result).unwrap();
    assert_eq!(n, 13);
    assert_eq!(&result, data.as_slice());
}

#[def_test]
fn test_bufreader_lines() {
    let data = b"line1\nline2\nline3";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(32, cursor);

    let mut line = String::new();
    let n = reader.read_line(&mut line).unwrap();
    assert!(n > 0);
    assert_eq!(line, "line1\n");

    line.clear();
    let n = reader.read_line(&mut line).unwrap();
    assert!(n > 0);
    assert_eq!(line, "line2\n");

    line.clear();
    let n = reader.read_line(&mut line).unwrap();
    assert!(n > 0);
    assert_eq!(line, "line3");
}

#[def_test]
fn test_bufreader_seek() {
    let data = b"0123456789";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(4, cursor);

    let mut buf = [0u8; 3];
    reader.read(&mut buf).unwrap();
    assert_eq!(&buf, b"012");

    reader.seek(SeekFrom::Start(7)).unwrap();
    reader.read(&mut buf).unwrap();
    assert_eq!(&buf, b"789");
}

#[def_test]
fn test_bufreader_capacity() {
    let data = b"test";
    let cursor = Cursor::new(data.as_slice());
    let reader = BufReader::with_capacity(16, cursor);
    assert_eq!(reader.capacity(), 16);
}

#[def_test]
fn test_bufreader_peek_and_buffer() {
    let data = b"abcdefgh";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(4, cursor);

    assert_eq!(reader.buffer(), b"");
    assert_eq!(reader.peek(3).unwrap(), b"abc");
    assert_eq!(reader.buffer(), b"abcd");

    reader.consume(2);
    assert_eq!(reader.peek(2).unwrap(), b"cd");
}

#[def_test]
fn test_bufreader_peek_eof_short() {
    let data = b"ab";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(8, cursor);

    assert_eq!(reader.peek(4).unwrap(), b"ab");
}

#[def_test]
fn test_bufreader_read_to_string_append_path() {
    let data = b"world";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(4, cursor);
    let mut s = String::from("hello ");

    let n = reader.read_to_string(&mut s).unwrap();
    assert_eq!(n, 5);
    assert_eq!(s, "hello world");
}

#[def_test]
fn test_bufreader_getters_and_into_inner() {
    let cursor = Cursor::new(b"xyz".as_slice());
    let mut reader = BufReader::new(cursor);

    assert_eq!(reader.get_ref().position(), 0);
    reader.get_mut().set_position(1);
    assert_eq!(reader.get_ref().position(), 1);

    let inner = reader.into_inner();
    assert_eq!(inner.position(), 1);
}

#[def_test]
fn test_bufreader_seek_relative_in_buffer() {
    let data = b"0123456789";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(8, cursor);

    assert_eq!(reader.peek(6).unwrap(), b"012345");
    reader.seek_relative(3).unwrap();
    assert_eq!(reader.fill_buf().unwrap(), b"34567");
    reader.seek_relative(-2).unwrap();
    assert_eq!(reader.fill_buf().unwrap(), b"1234567");
}

#[def_test]
fn test_bufreader_remaining() {
    use crate::IoBuf;

    let data = b"0123456789";
    let cursor = Cursor::new(data.as_slice());
    let mut reader = BufReader::with_capacity(4, cursor);

    assert_eq!(reader.remaining(), 10);
    let _ = reader.peek(4).unwrap();
    assert_eq!(reader.remaining(), 10);
    reader.consume(3);
    assert_eq!(reader.remaining(), 7);
}

// ============ BufWriter ============

#[def_test]
fn test_bufwriter_basic() {
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(8, inner);

    writer.write_all(b"Hello").unwrap();
    writer.flush().unwrap();

    let inner = writer.into_inner().unwrap();
    assert_eq!(inner.into_inner(), b"Hello");
}

#[def_test]
fn test_bufwriter_auto_flush() {
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(4, inner);

    writer.write_all(b"AB").unwrap();
    writer.write_all(b"CDEF").unwrap();
    writer.flush().unwrap();

    let inner = writer.into_inner().unwrap();
    assert_eq!(inner.into_inner(), b"ABCDEF");
}

#[def_test]
fn test_bufwriter_large_write() {
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(4, inner);

    writer.write_all(b"ABCDEFGHIJ").unwrap();
    writer.flush().unwrap();

    let inner = writer.into_inner().unwrap();
    assert_eq!(inner.into_inner(), b"ABCDEFGHIJ");
}

#[def_test]
fn test_bufwriter_capacity() {
    let inner = Cursor::new(vec![]);
    let writer = BufWriter::with_capacity(32, inner);
    assert_eq!(writer.capacity(), 32);
}

// ============ LineWriter ============

#[def_test]
fn test_linewriter_flushes_completed_line() {
    let inner = Cursor::new(vec![]);
    let mut writer = LineWriter::with_capacity(8, inner);

    writer.write_all(b"hello\n").unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"hello\n");

    writer.write_all(b"world").unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"hello\n");

    writer.flush().unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"hello\nworld");
}

#[def_test]
fn test_linewriter_write_and_into_inner() {
    let inner = Cursor::new(vec![]);
    let mut writer = LineWriter::new(inner);

    assert_eq!(writer.write(b"ab").unwrap(), 2);
    assert_eq!(writer.get_ref().get_ref(), b"");

    writer.write_all(b"cd\n").unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"abcd\n");

    let inner = writer.into_inner().unwrap();
    assert_eq!(inner.into_inner(), b"abcd\n");
}

#[def_test]
fn test_linewriter_write_fmt_and_get_mut() {
    let inner = Cursor::new(vec![]);
    let mut writer = LineWriter::with_capacity(4, inner);

    writer.write_fmt(format_args!("x={}\n", 3)).unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"x=3\n");

    writer.get_mut().write_all(b"tail").unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"x=3\ntail");
}

// ============ Empty / Repeat / Sink ============

#[def_test]
fn test_empty_read() {
    let mut empty = crate::empty();
    let mut buf = [0u8; 10];
    let n = empty.read(&mut buf).unwrap();
    assert_eq!(n, 0);
}

#[def_test]
fn test_empty_full_trait_surface() {
    use crate::{IoBuf, IoBufMut};

    let mut empty = crate::empty();
    let mut data = [0u8; 2];
    assert_eq!(
        empty.read_exact(&mut data),
        Err(crate::Error::UnexpectedEof)
    );
    assert_eq!(empty.read_exact(&mut []), Ok(()));

    let mut bytes = Vec::new();
    let mut line = String::new();
    assert_eq!(empty.read_to_end(&mut bytes).unwrap(), 0);
    assert_eq!(empty.read_to_string(&mut line).unwrap(), 0);
    assert_eq!(empty.fill_buf().unwrap(), b"");
    assert!(!empty.has_data_left().unwrap());
    assert_eq!(empty.skip_until(b'\n').unwrap(), 0);
    assert_eq!(empty.read_until(b'\n', &mut bytes).unwrap(), 0);
    assert_eq!(empty.read_line(&mut line).unwrap(), 0);
    assert_eq!(empty.seek(SeekFrom::Start(10)).unwrap(), 0);
    assert_eq!(empty.stream_len().unwrap(), 0);
    assert_eq!(empty.stream_position().unwrap(), 0);
    assert_eq!(empty.write(b"abc").unwrap(), 3);
    empty.write_all(b"abc").unwrap();
    empty.write_fmt(format_args!("x{}", 1)).unwrap();
    empty.flush().unwrap();
    assert_eq!(empty.remaining(), 0);
    assert_eq!(empty.remaining_mut(), usize::MAX);

    let mut empty_ref = &empty;
    assert_eq!(empty_ref.write(b"zz").unwrap(), 2);
    empty_ref.write_all(b"zz").unwrap();
    empty_ref.write_fmt(format_args!("ok")).unwrap();
    empty_ref.flush().unwrap();
}

#[def_test]
fn test_repeat_read() {
    let mut repeat = crate::repeat(0xAB);
    let mut buf = [0u8; 5];
    let n = repeat.read(&mut buf).unwrap();
    assert_eq!(n, 5);
    assert!(buf.iter().all(|&b| b == 0xAB));
}

#[def_test]
fn test_sink_write() {
    let mut sink = crate::sink();
    let n = sink.write(b"Hello, World!").unwrap();
    assert_eq!(n, 13);
    sink.flush().unwrap();
}

#[def_test]
fn test_sink_full_trait_surface() {
    use crate::IoBufMut;

    let mut sink = crate::sink();
    sink.write_all(b"abc").unwrap();
    sink.write_fmt(format_args!("x{}", 1)).unwrap();
    sink.flush().unwrap();
    assert_eq!(sink.remaining_mut(), usize::MAX);

    let mut sink_ref = &sink;
    assert_eq!(sink_ref.write(b"yy").unwrap(), 2);
    sink_ref.write_all(b"yy").unwrap();
    sink_ref.write_fmt(format_args!("ok")).unwrap();
    sink_ref.flush().unwrap();
}

// ============ copy ============

#[def_test]
fn test_copy_basic() {
    let src = b"Hello, World!";
    let mut reader = src.as_slice();
    let mut writer = Cursor::new(vec![]);

    let n = crate::copy(&mut reader, &mut writer).unwrap();
    assert_eq!(n, 13);
    assert_eq!(writer.into_inner(), b"Hello, World!");
}

#[def_test]
fn test_copy_empty() {
    let src: &[u8] = &[];
    let mut reader = src;
    let mut writer = Cursor::new(vec![]);

    let n = crate::copy(&mut reader, &mut writer).unwrap();
    assert_eq!(n, 0);
}

#[def_test]
fn test_read_impls_for_mut_ref_and_box() {
    let mut slice = b"hello".as_slice();
    let by_ref = &mut slice;
    let mut buf = [0u8; 2];
    by_ref.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"he");

    let mut boxed_reader = Box::new(Cursor::new(b"xyz".as_slice()));
    let mut out = Vec::new();
    boxed_reader.read_to_end(&mut out).unwrap();
    assert_eq!(&out, b"xyz");
}

#[def_test]
fn test_seek_impls_for_mut_ref_and_box() {
    let mut cursor = Cursor::new(b"abcdef".as_slice());
    let by_ref = &mut cursor;
    assert_eq!(by_ref.seek(SeekFrom::Start(2)).unwrap(), 2);
    by_ref.seek_relative(2).unwrap();
    assert_eq!(by_ref.stream_position().unwrap(), 4);
    by_ref.rewind().unwrap();
    assert_eq!(by_ref.stream_len().unwrap(), 6);

    let mut boxed = Box::new(Cursor::new(b"xyz".as_slice()));
    assert_eq!(boxed.seek(SeekFrom::End(-1)).unwrap(), 2);
    assert_eq!(boxed.stream_position().unwrap(), 2);
}

#[def_test]
fn test_repeat_full_trait_surface() {
    use crate::IoBuf;

    let mut repeat = crate::repeat(0xCD);
    let mut buf = [0u8; 4];
    assert_eq!(repeat.read(&mut buf).unwrap(), 4);
    assert_eq!(&buf, &[0xCD; 4]);
    repeat.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, &[0xCD; 4]);

    let mut out = Vec::new();
    let mut s = String::new();
    assert_eq!(repeat.read_to_end(&mut out), Err(crate::Error::NoMemory));
    assert_eq!(repeat.read_to_string(&mut s), Err(crate::Error::NoMemory));
    assert_eq!(repeat.remaining(), usize::MAX);
}

#[def_test]
fn test_copy_specialized_paths() {
    let mut deq = VecDeque::from(vec![b'a', b'b', b'c']);
    let mut out = Vec::new();
    assert_eq!(crate::copy(&mut deq, &mut out).unwrap(), 3);
    assert_eq!(&out, b"abc");

    let cursor = Cursor::new(b"hello".as_slice());
    let mut reader = BufReader::with_capacity(8, cursor);
    let mut out2 = Vec::new();
    assert_eq!(crate::copy(&mut reader, &mut out2).unwrap(), 5);
    assert_eq!(&out2, b"hello");
}

#[def_test]
fn test_iobuf_ext_specializations() {
    use crate::{IoBufExt, IoBufMutExt};

    let mut src = b"abcd".as_slice();
    let mut dst = Vec::new();
    assert_eq!(src.write_to(&mut dst).unwrap(), 4);
    assert_eq!(&dst, b"abcd");

    let mut src2 = b"xyz".as_slice();
    let mut dst2 = Vec::with_capacity(16);
    let n = dst2.read_from(&mut src2).unwrap();
    assert!(n > 0);
    assert!(dst2.starts_with(b"x"));

    let cursor = Cursor::new(b"12".as_slice());
    let mut reader = BufReader::with_capacity(8, cursor);
    let mut dst3 = Vec::new();
    assert_eq!(reader.write_to(&mut dst3).unwrap(), 2);
    assert_eq!(&dst3, b"12");

    let mut writer = BufWriter::with_capacity(8, Cursor::new(vec![]));
    let mut src3 = b"mn".as_slice();
    assert_eq!(writer.read_from(&mut src3).unwrap(), 2);
    writer.flush().unwrap();
    let inner = writer.into_inner().unwrap();
    assert_eq!(inner.into_inner(), b"mn");
}

#[def_test]
fn test_into_inner_error_accessors() {
    #[derive(Debug)]
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> crate::Result<usize> {
            Err(crate::Error::InvalidInput)
        }

        fn flush(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }

    let mut writer = BufWriter::with_capacity(8, FailingWriter);
    writer.write_all(b"abc").unwrap();

    let err = match writer.into_inner() {
        Err(err) => err,
        Ok(_) => panic!("expected into_inner to fail"),
    };
    assert_eq!(*err.error(), crate::Error::InvalidInput);

    let (error, _writer) = err.into_parts();
    assert_eq!(error, crate::Error::InvalidInput);
}

// ============ VecDeque Read / BufRead impls ============

#[def_test]
fn test_vecdeque_read_basic() {
    let mut deq: VecDeque<u8> = VecDeque::from(vec![1, 2, 3, 4, 5]);
    let mut buf = [0u8; 3];
    let n = deq.read(&mut buf).unwrap();
    assert_eq!(n, 3);
    assert_eq!(&buf, &[1, 2, 3]);
    assert_eq!(deq.len(), 2);
}

#[def_test]
fn test_vecdeque_read_exact() {
    let mut deq: VecDeque<u8> = VecDeque::from(vec![10, 20, 30, 40]);
    let mut buf = [0u8; 3];
    deq.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, &[10, 20, 30]);
    assert_eq!(deq.len(), 1);

    let mut big = [0u8; 5];
    assert_eq!(deq.read_exact(&mut big), Err(crate::Error::UnexpectedEof));
}

#[def_test]
fn test_vecdeque_read_to_end_and_string() {
    let mut deq: VecDeque<u8> = VecDeque::from(b"hello".to_vec());
    let mut out = Vec::new();
    let n = deq.read_to_end(&mut out).unwrap();
    assert_eq!(n, 5);
    assert_eq!(&out, b"hello");
    assert!(deq.is_empty());

    let mut deq2: VecDeque<u8> = VecDeque::from(b"world".to_vec());
    let mut s = String::new();
    let n = deq2.read_to_string(&mut s).unwrap();
    assert_eq!(n, 5);
    assert_eq!(s, "world");
}

#[def_test]
fn test_vecdeque_bufread() {
    let mut deq: VecDeque<u8> = VecDeque::from(vec![b'a', b'b', b'c']);
    let front = deq.fill_buf().unwrap();
    assert!(!front.is_empty());
    let first = front[0];
    assert_eq!(first, b'a');
    deq.consume(1);
    assert_eq!(deq.len(), 2);
}

// ============ &[u8] single-byte read path ============

#[def_test]
fn test_slice_read_single_byte() {
    let mut data: &[u8] = &[42];
    let mut buf = [0u8; 1];
    let n = data.read(&mut buf).unwrap();
    assert_eq!(n, 1);
    assert_eq!(buf[0], 42);
}

#[def_test]
fn test_slice_read_exact_single_byte() {
    let mut data: &[u8] = &[99, 100];
    let mut buf = [0u8; 1];
    data.read_exact(&mut buf).unwrap();
    assert_eq!(buf[0], 99);
    assert_eq!(data.len(), 1);
}

#[def_test]
fn test_slice_read_exact_insufficient() {
    let mut data: &[u8] = &[1, 2];
    let mut buf = [0u8; 5];
    assert_eq!(data.read_exact(&mut buf), Err(crate::Error::UnexpectedEof));
    assert!(data.is_empty());
}

#[def_test]
fn test_slice_bufread_fill_and_consume() {
    let mut data: &[u8] = &[1, 2, 3, 4];
    let buf = BufRead::fill_buf(&mut data).unwrap();
    assert_eq!(buf, &[1, 2, 3, 4]);
    BufRead::consume(&mut data, 2);
    assert_eq!(data, &[3, 4]);
}

// ============ BufRead for Box<B> ============

#[def_test]
fn test_box_bufread_impls() {
    let inner = Cursor::new(b"line1\nline2".as_slice());
    let mut boxed: Box<BufReader<Cursor<&[u8]>>> = Box::new(BufReader::new(inner));

    let buf = boxed.fill_buf().unwrap();
    assert!(buf.starts_with(b"line1"));

    assert!(boxed.has_data_left().unwrap());

    let mut out = Vec::new();
    let n = boxed.read_until(b'\n', &mut out).unwrap();
    assert!(n > 0);
    assert_eq!(&out, b"line1\n");

    let mut line = String::new();
    let n2 = boxed.read_line(&mut line).unwrap();
    assert!(n2 > 0);
    assert_eq!(line, "line2");
}

// ============ Box<W> Write impls ============

#[def_test]
fn test_box_write_impls() {
    let inner = Cursor::new(vec![]);
    let mut boxed: Box<Cursor<Vec<u8>>> = Box::new(inner);
    boxed.write(b"hi").unwrap();
    boxed.write_all(b" there").unwrap();
    boxed.write_fmt(format_args!("!")).unwrap();
    boxed.flush().unwrap();
    assert_eq!(boxed.into_inner(), b"hi there!");
}

// ============ Vec<u8> Write impl ============

#[def_test]
fn test_vec_write_individual() {
    let mut v = Vec::new();
    assert_eq!(v.write(b"abc").unwrap(), 3);
    v.write_all(b"def").unwrap();
    v.flush().unwrap();
    assert_eq!(&v, b"abcdef");
}

// ============ copy: Vec<u8> writer + BufWriter writer paths ============

#[def_test]
fn test_copy_into_vec() {
    let mut src: &[u8] = b"some data to copy";
    let mut dst = Vec::new();
    let n = crate::copy(&mut src, &mut dst).unwrap();
    assert_eq!(n, 17);
    assert_eq!(&dst, b"some data to copy");
}

#[def_test]
fn test_copy_via_bufwriter() {
    let mut src: &[u8] = b"buffered copy";
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(crate::DEFAULT_BUF_SIZE, inner);
    let n = crate::copy(&mut src, &mut writer).unwrap();
    assert_eq!(n, 13);
    writer.flush().unwrap();
    assert_eq!(writer.into_inner().unwrap().into_inner(), b"buffered copy");
}

#[def_test]
fn test_copy_via_bufwriter_small_cap() {
    let mut src: &[u8] = b"small buf";
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(2, inner);
    let n = crate::copy(&mut src, &mut writer).unwrap();
    assert_eq!(n, 9);
    writer.flush().unwrap();
    assert_eq!(writer.into_inner().unwrap().into_inner(), b"small buf");
}

#[def_test]
fn test_copy_bufreader_to_bufwriter() {
    let cursor = Cursor::new(b"reader to writer".as_slice());
    let mut reader = BufReader::with_capacity(crate::DEFAULT_BUF_SIZE, cursor);
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(crate::DEFAULT_BUF_SIZE, inner);

    let n = crate::copy(&mut reader, &mut writer).unwrap();
    assert_eq!(n, 16);
    writer.flush().unwrap();
    assert_eq!(
        writer.into_inner().unwrap().into_inner(),
        b"reader to writer"
    );
}

// ============ IntoInnerError: Display, From, into_inner, into_error ============

#[def_test]
fn test_into_inner_error_display_and_from() {
    use alloc::string::ToString;

    #[derive(Debug)]
    struct BadWriter;
    impl Write for BadWriter {
        fn write(&mut self, _: &[u8]) -> crate::Result<usize> {
            Err(crate::Error::BrokenPipe)
        }
        fn flush(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }

    let mut w = BufWriter::with_capacity(8, BadWriter);
    w.write_all(b"abc").unwrap();
    let err = w.into_inner().unwrap_err();

    let display = err.to_string();
    assert!(!display.is_empty());

    let _inner_writer = err.into_inner();
}

#[def_test]
fn test_into_inner_error_into_error_and_from() {
    #[derive(Debug)]
    struct BadWriter2;
    impl Write for BadWriter2 {
        fn write(&mut self, _: &[u8]) -> crate::Result<usize> {
            Err(crate::Error::StorageFull)
        }
        fn flush(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }

    let mut w = BufWriter::with_capacity(8, BadWriter2);
    w.write_all(b"xyz").unwrap();
    let err = w.into_inner().unwrap_err();
    let io_err: crate::Error = err.into();
    assert_eq!(io_err, crate::Error::StorageFull);
}

// ============ Repeat: read_buf, read_exact, debug ============

#[def_test]
fn test_repeat_read_exact() {
    let mut r = crate::repeat(0x55);
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).unwrap();
    assert!(buf.iter().all(|&b| b == 0x55));
}

#[def_test]
fn test_repeat_debug() {
    use alloc::format;
    let r = crate::repeat(0xAA);
    let dbg = format!("{:?}", r);
    assert!(dbg.contains("Repeat"));
}

// ============ Seek impls for Box<S>: full coverage ============

#[def_test]
fn test_box_seek_full() {
    let mut boxed = Box::new(Cursor::new(b"abcdef".as_slice()));
    assert_eq!(boxed.seek(SeekFrom::Start(3)).unwrap(), 3);
    assert_eq!(boxed.stream_position().unwrap(), 3);
    boxed.seek_relative(-1).unwrap();
    assert_eq!(boxed.stream_position().unwrap(), 2);
    assert_eq!(boxed.stream_len().unwrap(), 6);
    boxed.rewind().unwrap();
    assert_eq!(boxed.stream_position().unwrap(), 0);
}

// ============ BufRead for &mut B: skip_until, has_data_left ============

#[def_test]
fn test_mut_ref_bufread_impls() {
    let inner = Cursor::new(b"abcXdef".as_slice());
    let mut reader = BufReader::new(inner);
    let by_ref: &mut BufReader<Cursor<&[u8]>> = &mut reader;

    assert!(by_ref.has_data_left().unwrap());

    let n = by_ref.skip_until(b'X').unwrap();
    assert_eq!(n, 4);

    let buf = by_ref.fill_buf().unwrap();
    assert_eq!(buf, b"def");
    by_ref.consume(1);

    let mut out = Vec::new();
    by_ref.read_until(b'f', &mut out).unwrap();
    assert_eq!(&out, b"ef");

    let mut line = String::new();
    let n = by_ref.read_line(&mut line).unwrap();
    assert_eq!(n, 0);
}

// ============ Box<R> Read: more methods ============

#[def_test]
fn test_box_read_more_methods() {
    let mut boxed: Box<&[u8]> = Box::new(b"hello world".as_slice());
    let mut buf = [0u8; 5];
    boxed.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello");

    let mut out = Vec::new();
    boxed.read_to_end(&mut out).unwrap();
    assert_eq!(&out, b" world");

    let mut boxed2: Box<&[u8]> = Box::new(b"test".as_slice());
    let mut s = String::new();
    boxed2.read_to_string(&mut s).unwrap();
    assert_eq!(s, "test");
}

// ============ &mut W Write: write_fmt ============

#[def_test]
fn test_mut_ref_write_fmt() {
    let mut v = Vec::new();
    let w: &mut Vec<u8> = &mut v;
    w.write(b"a").unwrap();
    w.write_all(b"bc").unwrap();
    w.write_fmt(format_args!("d")).unwrap();
    w.flush().unwrap();
    assert_eq!(&v, b"abcd");
}

// ============ IoBufMutExt: BorrowedCursor specialization ============

#[def_test]
fn test_iobufmut_ext_borrowed_cursor() {
    use core::{io::BorrowedBuf, mem::MaybeUninit};

    use crate::IoBufMutExt;

    let mut backing = [MaybeUninit::uninit(); 16];
    let mut bbuf: BorrowedBuf<'_> = (&mut backing[..]).into();
    let mut cursor = bbuf.unfilled();
    let mut src: &[u8] = b"hello";
    let n = cursor.read_from(&mut src).unwrap();
    assert_eq!(n, 5);
}

// ============ stack_buffer_copy (through copy with non-specialized types) ============

#[def_test]
fn test_stack_buffer_copy_path() {
    let cursor = Cursor::new(b"non-special reader".as_slice());
    let mut reader = BufReader::with_capacity(2, cursor);
    let mut dst = Cursor::new(vec![]);
    let n = crate::copy(&mut reader, &mut dst).unwrap();
    assert_eq!(n, 18);
    assert_eq!(dst.into_inner(), b"non-special reader");
}

// ============ BufWriter: into_parts, Debug, Seek, write_cold ============

#[def_test]
fn test_bufwriter_into_parts() {
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(32, inner);
    writer.write_all(b"buffered").unwrap();

    let (inner, buf_result) = writer.into_parts();
    let buf = buf_result.unwrap();
    assert_eq!(&buf, b"buffered");
    assert_eq!(inner.into_inner(), b"");
}

#[def_test]
fn test_bufwriter_debug() {
    use alloc::format;
    let inner = Cursor::new(vec![]);
    let writer = BufWriter::with_capacity(16, inner);
    let dbg = format!("{:?}", writer);
    assert!(dbg.contains("BufWriter"));
}

#[def_test]
fn test_bufwriter_seek() {
    let inner = Cursor::new(vec![0u8; 20]);
    let mut writer = BufWriter::with_capacity(8, inner);
    writer.write_all(b"AB").unwrap();
    let pos = writer.seek(SeekFrom::Start(10)).unwrap();
    assert_eq!(pos, 10);
}

#[def_test]
fn test_bufwriter_write_cold_large() {
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(4, inner);
    writer.write_all(b"AB").unwrap();
    writer.write_all(b"CDEFGHIJKLMN").unwrap();
    writer.flush().unwrap();
    assert_eq!(writer.into_inner().unwrap().into_inner(), b"ABCDEFGHIJKLMN");
}

#[def_test]
fn test_bufwriter_write_exact_capacity() {
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(4, inner);
    let n = writer.write(b"ABCD").unwrap();
    assert_eq!(n, 4);
    writer.flush().unwrap();
    assert_eq!(writer.into_inner().unwrap().into_inner(), b"ABCD");
}

#[def_test]
fn test_bufwriter_iobufmut() {
    use crate::IoBufMut;
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(8, inner);
    let rm = writer.remaining_mut();
    assert!(rm > 0);
    writer.write_all(b"abc").unwrap();
    let rm2 = writer.remaining_mut();
    assert!(rm2 < rm);
}

#[def_test]
fn test_bufwriter_get_ref_get_mut() {
    let inner = Cursor::new(vec![]);
    let mut writer = BufWriter::with_capacity(8, inner);
    writer.write_all(b"test").unwrap();
    assert_eq!(writer.buffer(), b"test");
    let _ = writer.get_ref();
    writer.get_mut().write_all(b"direct").unwrap();
    writer.flush().unwrap();
}

// ============ LineWriter: more branch coverage ============

#[def_test]
fn test_linewriter_no_newline_write() {
    let inner = Cursor::new(vec![]);
    let mut writer = LineWriter::with_capacity(16, inner);
    writer.write(b"no newline here").unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"");
    writer.flush().unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"no newline here");
}

#[def_test]
fn test_linewriter_multiple_lines() {
    let inner = Cursor::new(vec![]);
    let mut writer = LineWriter::with_capacity(32, inner);
    writer.write_all(b"line1\nline2\n").unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"line1\nline2\n");

    writer.write_all(b"partial").unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"line1\nline2\n");
    writer.flush().unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"line1\nline2\npartial");
}

#[def_test]
fn test_linewriter_write_all_with_tail() {
    let inner = Cursor::new(vec![]);
    let mut writer = LineWriter::with_capacity(32, inner);
    writer.write_all(b"first\nsecond").unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"first\n");
    writer.flush().unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"first\nsecond");
}

#[def_test]
fn test_linewriter_write_all_buffered_then_newline() {
    let inner = Cursor::new(vec![]);
    let mut writer = LineWriter::with_capacity(32, inner);
    writer.write_all(b"buf").unwrap();
    writer.write_all(b"fered\n").unwrap();
    assert_eq!(writer.get_ref().get_ref(), b"buffered\n");
}

// ============ BufReader: more branch coverage ============

#[def_test]
fn test_bufreader_has_data_left() {
    let cursor = Cursor::new(b"abc".as_slice());
    let mut reader = BufReader::new(cursor);
    assert!(reader.has_data_left().unwrap());

    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).unwrap();
    assert!(!reader.has_data_left().unwrap());
}

#[def_test]
fn test_bufreader_skip_until() {
    let cursor = Cursor::new(b"abcXdefYghi".as_slice());
    let mut reader = BufReader::new(cursor);
    let n = reader.skip_until(b'X').unwrap();
    assert_eq!(n, 4);

    let buf = reader.fill_buf().unwrap();
    assert_eq!(buf[0], b'd');
}

#[def_test]
fn test_bufreader_read_until() {
    let cursor = Cursor::new(b"hello;world;end".as_slice());
    let mut reader = BufReader::new(cursor);
    let mut out = Vec::new();
    let n = reader.read_until(b';', &mut out).unwrap();
    assert_eq!(n, 6);
    assert_eq!(&out, b"hello;");
}

#[def_test]
fn test_bufreader_empty_read() {
    let cursor = Cursor::new(b"".as_slice());
    let mut reader = BufReader::new(cursor);
    assert!(!reader.has_data_left().unwrap());
    let mut buf = Vec::new();
    assert_eq!(reader.read_to_end(&mut buf).unwrap(), 0);
}

#[def_test]
fn test_bufreader_new_default_capacity() {
    let cursor = Cursor::new(b"test".as_slice());
    let reader = BufReader::new(cursor);
    assert!(reader.capacity() >= 1);
}

// ============ read/mod.rs: default_read_to_end via custom reader ============

#[def_test]
fn test_default_read_to_end_path() {
    struct ChunkedReader {
        data: &'static [u8],
        pos: usize,
    }
    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> crate::Result<usize> {
            let remaining = &self.data[self.pos..];
            let n = buf.len().min(remaining.len()).min(3);
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    let mut reader = ChunkedReader {
        data: b"hello world this is a longer test string",
        pos: 0,
    };
    let mut out = Vec::new();
    let n = reader.read_to_end(&mut out).unwrap();
    assert_eq!(n, 40);
    assert_eq!(&out, b"hello world this is a longer test string");
}

#[def_test]
fn test_default_read_to_string_path() {
    struct SmallReader {
        data: &'static [u8],
        pos: usize,
    }
    impl Read for SmallReader {
        fn read(&mut self, buf: &mut [u8]) -> crate::Result<usize> {
            let remaining = &self.data[self.pos..];
            let n = buf.len().min(remaining.len()).min(5);
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    let mut reader = SmallReader {
        data: b"hello world",
        pos: 0,
    };
    let mut s = String::new();
    let n = reader.read_to_string(&mut s).unwrap();
    assert_eq!(n, 11);
    assert_eq!(s, "hello world");
}

// ============ BufRead: split, lines, read_line ============

#[def_test]
fn test_split_iterator() {
    let data = b"aXbXcX";
    let reader = BufReader::new(data.as_slice());
    let parts: Vec<Vec<u8>> = reader.split(b'X').map(|r| r.unwrap()).collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0], b"a");
    assert_eq!(parts[1], b"b");
    assert_eq!(parts[2], b"c");
}

#[def_test]
fn test_split_no_trailing_delim() {
    let data = b"aXbXc";
    let reader = BufReader::new(data.as_slice());
    let parts: Vec<Vec<u8>> = reader.split(b'X').map(|r| r.unwrap()).collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[2], b"c");
}

#[def_test]
fn test_split_empty() {
    let data = b"";
    let reader = BufReader::new(data.as_slice());
    let parts: Vec<Vec<u8>> = reader.split(b'X').map(|r| r.unwrap()).collect();
    assert_eq!(parts.len(), 0);
}

#[def_test]
fn test_lines_iterator() {
    let data = b"line1\nline2\nline3\n";
    let reader = BufReader::new(data.as_slice());
    let lines: Vec<String> = reader.lines().map(|r| r.unwrap()).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "line1");
    assert_eq!(lines[1], "line2");
    assert_eq!(lines[2], "line3");
}

#[def_test]
fn test_lines_crlf() {
    let data = b"hello\r\nworld\r\n";
    let reader = BufReader::new(data.as_slice());
    let lines: Vec<String> = reader.lines().map(|r| r.unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "hello");
    assert_eq!(lines[1], "world");
}

#[def_test]
fn test_lines_no_trailing_newline() {
    let data = b"first\nsecond";
    let reader = BufReader::new(data.as_slice());
    let lines: Vec<String> = reader.lines().map(|r| r.unwrap()).collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1], "second");
}

#[def_test]
fn test_read_line_basic() {
    let data = b"hello\nworld\n";
    let mut reader = BufReader::new(data.as_slice());
    let mut line = String::new();
    let n = reader.read_line(&mut line).unwrap();
    assert_eq!(n, 6);
    assert_eq!(line, "hello\n");

    line.clear();
    let n = reader.read_line(&mut line).unwrap();
    assert_eq!(n, 6);
    assert_eq!(line, "world\n");
}

#[def_test]
fn test_read_to_string_standalone() {
    let data = b"standalone test";
    let n = crate::read_to_string(data.as_slice()).unwrap();
    assert_eq!(n, "standalone test");
}

// ============ Write: write_fmt error path, write_all interrupted ============

#[def_test]
fn test_write_all_write_zero() {
    struct ZeroWriter;
    impl Write for ZeroWriter {
        fn write(&mut self, _buf: &[u8]) -> crate::Result<usize> {
            Ok(0)
        }
        fn flush(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }
    let mut w = ZeroWriter;
    let err = w.write_all(b"data").unwrap_err();
    assert_eq!(err, Error::WriteZero);
}

#[def_test]
fn test_write_all_interrupted_retry() {
    struct InterruptOnceWriter {
        interrupted: bool,
        buf: Vec<u8>,
    }
    impl Write for InterruptOnceWriter {
        fn write(&mut self, data: &[u8]) -> crate::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(Error::Interrupted);
            }
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }
    let mut w = InterruptOnceWriter {
        interrupted: false,
        buf: Vec::new(),
    };
    w.write_all(b"hello").unwrap();
    assert_eq!(&w.buf, b"hello");
}

#[def_test]
fn test_write_fmt_with_failing_writer() {
    struct FailWriter;
    impl Write for FailWriter {
        fn write(&mut self, _buf: &[u8]) -> crate::Result<usize> {
            Err(Error::BrokenPipe)
        }
        fn flush(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }
    let mut w = FailWriter;
    let err = w.write_fmt(format_args!("test {}", 42)).unwrap_err();
    assert_eq!(err, Error::BrokenPipe);
}

// ============ Seek: default trait methods ============

#[def_test]
fn test_seek_stream_len() {
    let mut cursor = Cursor::new(vec![0u8; 100]);
    cursor.seek(SeekFrom::Start(50)).unwrap();
    let len = cursor.stream_len().unwrap();
    assert_eq!(len, 100);
    assert_eq!(cursor.stream_position().unwrap(), 50);
}

#[def_test]
fn test_seek_rewind() {
    let mut cursor = Cursor::new(vec![0u8; 50]);
    cursor.seek(SeekFrom::Start(25)).unwrap();
    assert_eq!(cursor.stream_position().unwrap(), 25);
    cursor.rewind().unwrap();
    assert_eq!(cursor.stream_position().unwrap(), 0);
}

#[def_test]
fn test_seek_relative() {
    let mut cursor = Cursor::new(vec![0u8; 100]);
    cursor.seek(SeekFrom::Start(10)).unwrap();
    cursor.seek_relative(20).unwrap();
    assert_eq!(cursor.stream_position().unwrap(), 30);
    cursor.seek_relative(-5).unwrap();
    assert_eq!(cursor.stream_position().unwrap(), 25);
}

#[def_test]
fn test_seek_stream_len_at_end() {
    let mut cursor = Cursor::new(vec![0u8; 100]);
    cursor.seek(SeekFrom::End(0)).unwrap();
    let len = cursor.stream_len().unwrap();
    assert_eq!(len, 100);
}

// ============ read_to_string with invalid UTF-8 ============

#[def_test]
fn test_read_to_string_invalid_utf8() {
    let data: &[u8] = &[0xff, 0xfe, 0x80];
    let err = crate::read_to_string(data).unwrap_err();
    assert_eq!(err, Error::IllegalBytes);
}

// ============ LineWriter: more shim branches ============

#[def_test]
fn test_linewriter_write_large_no_newline() {
    let mut out = Vec::new();
    let mut lw = LineWriter::new(&mut out);
    let big = vec![b'a'; 2048];
    let n = lw.write(&big).unwrap();
    assert!(n > 0);
}

#[def_test]
fn test_linewriter_flush() {
    let mut out = Vec::new();
    {
        let mut lw = LineWriter::new(&mut out);
        lw.write_all(b"buffered").unwrap();
        lw.flush().unwrap();
    }
    assert_eq!(&out, b"buffered");
}

// ============ Read: by_ref ============

#[def_test]
fn test_read_by_ref() {
    let data = b"abcdef";
    let mut reader = data.as_slice();
    let mut buf = [0u8; 3];
    reader.by_ref().read(&mut buf).unwrap();
    assert_eq!(&buf, b"abc");
    reader.by_ref().read(&mut buf).unwrap();
    assert_eq!(&buf, b"def");
}

// ============ Write: by_ref ============

#[def_test]
fn test_write_by_ref_trait() {
    let mut out = Vec::new();
    out.by_ref().write_all(b"hello").unwrap();
    out.by_ref().write_all(b" world").unwrap();
    assert_eq!(&out, b"hello world");
}

// ============ read_exact Interrupted retry ============

#[def_test]
fn test_read_exact_interrupted_retry() {
    struct InterruptReader {
        data: &'static [u8],
        pos: usize,
        interrupted: bool,
    }
    impl Read for InterruptReader {
        fn read(&mut self, buf: &mut [u8]) -> crate::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(Error::Interrupted);
            }
            let n = buf.len().min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
    let mut reader = InterruptReader {
        data: b"abcdef",
        pos: 0,
        interrupted: false,
    };
    let mut buf = [0u8; 6];
    reader.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"abcdef");
}

// ============ read_exact UnexpectedEof ============

#[def_test]
fn test_read_exact_unexpected_eof() {
    let data = b"abc";
    let mut reader = data.as_slice();
    let mut buf = [0u8; 10];
    let err = reader.read_exact(&mut buf).unwrap_err();
    assert_eq!(err, Error::UnexpectedEof);
}
