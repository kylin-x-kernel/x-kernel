// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::string::String;
use core::fmt;

use unittest::{assert_eq, def_test};

use super::{SeqFile, SeqIterator};

struct LinesIter {
    lines: [&'static str; 2],
    next_index: usize,
}

impl LinesIter {
    fn new() -> Self {
        Self {
            lines: ["hello\n", "world\n"],
            next_index: 0,
        }
    }
}

impl SeqIterator for LinesIter {
    type Item = &'static str;

    fn rewind(&mut self) {
        self.next_index = 0;
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.lines.get(self.next_index).copied();
        if item.is_some() {
            self.next_index += 1;
        }
        item
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

#[def_test]
fn test_seq_file_reads_across_multiple_entries() {
    let mut seq = SeqFile::new(LinesIter::new());
    let mut buf = [0; 12];
    assert_eq!(seq.read(&mut buf, 0).unwrap(), 12);
    assert_eq!(&buf, b"hello\nworld\n");
}

#[def_test]
fn test_seq_file_rewinds_on_zero_offset() {
    let mut seq = SeqFile::new(LinesIter::new());
    let mut buf = [0; 6];
    assert_eq!(seq.read(&mut buf, 0).unwrap(), 6);
    assert_eq!(&buf, b"hello\n");
    assert_eq!(seq.read(&mut buf, 0).unwrap(), 6);
    assert_eq!(&buf, b"hello\n");
}

#[def_test]
fn test_seq_file_seeks_forward_from_beginning() {
    let mut seq = SeqFile::new(LinesIter::new());
    let mut buf = [0; 4];
    assert_eq!(seq.read(&mut buf, 4).unwrap(), 4);
    assert_eq!(&buf, b"o\nwo");
}

#[def_test]
fn test_seq_file_rewinds_and_seeks_from_middle_offset() {
    let mut seq = SeqFile::new(LinesIter::new());
    let mut buf = [0; 5];
    assert_eq!(seq.read(&mut buf, 7).unwrap(), 5);
    assert_eq!(&buf, b"orld\n");
}

#[def_test]
fn test_seq_file_preserves_typed_error_and_discards_failed_record() {
    struct RetryIter(core::cell::Cell<bool>);

    impl SeqIterator for RetryIter {
        type Item = ();

        fn rewind(&mut self) {}

        fn start(&mut self) -> Option<Self::Item> {
            Some(())
        }

        fn next(&mut self) -> Option<Self::Item> {
            None
        }

        fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
            self.try_show(item, buf).map_err(|_| fmt::Error)
        }

        fn try_show(&self, _item: &Self::Item, buf: &mut String) -> crate::VfsResult<()> {
            if !self.0.replace(true) {
                buf.push_str("discard");
                return Err(crate::VfsError::NoSuchProcess);
            }
            buf.push_str("ready\n");
            Ok(())
        }
    }

    let mut seq = SeqFile::new(RetryIter(core::cell::Cell::new(false)));
    let mut buf = [0; 6];
    assert_eq!(seq.read(&mut buf, 0), Err(crate::VfsError::NoSuchProcess));
    assert_eq!(seq.read(&mut buf, 0).unwrap(), 6);
    assert_eq!(&buf, b"ready\n");
}
