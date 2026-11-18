// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::string::String;
use core::fmt;

use iov_iter::{IovSink, IovSource, iov_iter_dest, iov_iter_source};
use unittest::{assert, assert_eq, def_test};

use super::{SeqFile, SeqIterator, SeqTextSink};

struct EmptySource;

impl IovSource for EmptySource {
    fn count(&self) -> usize {
        0
    }

    fn copy_from_iter(&mut self, _dst: &mut [u8]) -> kerrno::KResult<usize> {
        core::unreachable!("empty source must not copy")
    }
}

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

#[def_test]
fn fallible_start_error_is_not_eof_and_can_retry() {
    struct RetryStart(core::cell::Cell<usize>);

    impl SeqIterator for RetryStart {
        type Item = &'static str;

        fn rewind(&mut self) {}

        fn start(&mut self) -> Option<Self::Item> {
            None
        }

        fn next(&mut self) -> Option<Self::Item> {
            None
        }

        fn try_start(&mut self) -> crate::VfsResult<Option<Self::Item>> {
            let attempt = self.0.get();
            self.0.set(attempt + 1);
            if attempt == 0 {
                Err(crate::VfsError::NoSuchProcess)
            } else {
                Ok(Some("retry\n"))
            }
        }

        fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
            buf.push_str(item);
            Ok(())
        }
    }

    let mut seq = SeqFile::new(RetryStart(core::cell::Cell::new(0)));
    let mut buf = [0; 8];
    assert_eq!(seq.read(&mut buf, 0), Err(crate::VfsError::NoSuchProcess));
    assert_eq!(seq.read(&mut buf, 0).unwrap(), 6);
    assert_eq!(&buf[..6], b"retry\n");
}

#[def_test]
fn record_limit_preserves_file_too_large_error() {
    struct TooLarge;

    impl SeqIterator for TooLarge {
        type Item = ();

        fn rewind(&mut self) {}

        fn start(&mut self) -> Option<Self::Item> {
            Some(())
        }

        fn next(&mut self) -> Option<Self::Item> {
            None
        }

        fn show(&self, _item: &Self::Item, buf: &mut String) -> fmt::Result {
            buf.push_str("12345");
            Ok(())
        }
    }

    let mut seq = SeqFile::try_with_capacity(TooLarge, 4, 4).unwrap();
    let mut buf = [0; 8];
    assert_eq!(seq.read(&mut buf, 0), Err(crate::VfsError::FileTooLarge));
}

struct VecSink {
    data: alloc::vec::Vec<u8>,
    capacity: usize,
}

impl IovSink for VecSink {
    fn count(&self) -> usize {
        self.capacity - self.data.len()
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> kerrno::KResult<usize> {
        let count = src.len().min(self.count());
        self.data.extend_from_slice(&src[..count]);
        Ok(count)
    }

    fn copy_to_iter_nofault(&mut self, src: &[u8]) -> kerrno::KResult<usize> {
        self.copy_to_iter(src)
    }
}

#[def_test]
fn read_iter_spans_more_than_one_page_and_one_record() {
    struct Big;

    impl SeqIterator for Big {
        type Item = ();

        fn rewind(&mut self) {}

        fn start(&mut self) -> Option<Self::Item> {
            Some(())
        }

        fn next(&mut self) -> Option<Self::Item> {
            None
        }

        fn show(&self, _item: &Self::Item, buf: &mut String) -> fmt::Result {
            for _ in 0..6000 {
                buf.push('o');
            }
            Ok(())
        }
    }

    let mut seq = SeqFile::try_with_capacity(Big, 16, 7000).unwrap();
    let mut sink = VecSink {
        data: alloc::vec::Vec::new(),
        capacity: 7000,
    };
    let mut iter = iov_iter_dest(&mut sink);
    assert_eq!(seq.read_iter(0, &mut iter), Ok(6000));
    assert_eq!(sink.data.len(), 6000);
    assert!(sink.data.iter().all(|byte| *byte == b'o'));
}

struct SingleRecord {
    data: alloc::vec::Vec<u8>,
    starts: alloc::sync::Arc<core::sync::atomic::AtomicUsize>,
}

impl SeqIterator for SingleRecord {
    type Item = alloc::vec::Vec<u8>;

    fn rewind(&mut self) {}

    fn start(&mut self) -> Option<Self::Item> {
        self.starts
            .fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        Some(self.data.clone())
    }

    fn next(&mut self) -> Option<Self::Item> {
        None
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        let text = core::str::from_utf8(item).map_err(|_| fmt::Error)?;
        buf.push_str(text);
        Ok(())
    }
}

static TEST_SEQ_FS_TYPE: crate::FileSystemType = crate::FileSystemType::internal("seq-test");

#[def_test]
fn seq_file_inode_lazy_seek_read_iter_and_commands() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use crate::{
        DirMapping, EmptyCommandPolicy, Filename, Mount, NodePermission, SeqCommandOptions,
        SeqFileInode, SeqFileOptions, SeekEndPolicy, SimpleDir, SimpleFs,
    };

    let starts = Arc::new(AtomicUsize::new(0));
    let opens = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let commands = Arc::new(crate::Mutex::new(alloc::vec::Vec::new()));

    let observed_starts = starts.clone();
    let observed_opens = opens.clone();
    let observed_reads = reads.clone();
    let observed_commands = commands.clone();
    let scalar_slot = Arc::new(crate::Mutex::new(None));
    let scalar_slot_for_builder = scalar_slot.clone();
    let super_block = SimpleFs::new_with(&TEST_SEQ_FS_TYPE, 0, move |fs| {
        let mut root = DirMapping::new();
        let inode = SeqFileInode::new_regular_with_options(
                fs.clone(),
                move || {
                    Ok(SingleRecord {
                        data: alloc::vec![b'o'; 6000],
                        starts: observed_starts.clone(),
                    })
                },
                SeqFileOptions {
                    permission: NodePermission::from_bits_truncate(0o644),
                    lazy_open: true,
                    max_record_bytes: 7000,
                    reported_size: 0,
                    seek_end: SeekEndPolicy::Reject,
                    validate_open: Some(Arc::new(move || {
                        observed_opens.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    })),
                    validate_read: Some(Arc::new(move || {
                        observed_reads.fetch_add(1, Ordering::AcqRel);
                        Ok(())
                    })),
                    command: Some(SeqCommandOptions {
                        max_command_bytes: 8,
                        validate_write: None,
                        write: Arc::new(move |_file, data| {
                            observed_commands.lock().push(data.to_vec());
                            Ok(())
                        }),
                        empty: EmptyCommandPolicy::Callback,
                    }),
                },
            );
        *scalar_slot_for_builder.lock() = Some(inode.clone());
        root.add_file("seq", inode);
        SimpleDir::new_maker(fs, Arc::new(root))
    })
    .unwrap();
    let scalar_inode = scalar_slot
        .lock()
        .clone()
        .expect("scalar inode");

    let root = Mount::new_root(&super_block).root_path();
    let file = Filename::new("/seq")
        .open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_RDWR,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();

    assert_eq!(opens.load(Ordering::Acquire), 1);
    assert_eq!(starts.load(Ordering::Acquire), 0);
    assert_eq!(file.inode().metadata().size, 0);
    assert_eq!(
        file.llseek(0, crate::file::SEEK_END),
        Err(crate::VfsError::InvalidInput)
    );

    let mut first = [0u8; 4];
    assert_eq!(file.read(&mut first), Ok(4));
    assert_eq!(&first, b"oooo");
    assert_eq!(starts.load(Ordering::Acquire), 1);
    assert_eq!(reads.load(Ordering::Acquire), 1);

    assert_eq!(file.llseek(0, crate::file::SEEK_SET), Ok(0));
    let mut sink = VecSink {
        data: alloc::vec::Vec::new(),
        capacity: 7000,
    };
    let mut iter = iov_iter_dest(&mut sink);
    let mut pos = 0;
    assert_eq!(file.read_iter_from(&mut iter, &mut pos), Ok(6000));
    assert_eq!(sink.data.len(), 6000);
    assert!(sink.data.iter().all(|byte| *byte == b'o'));
    assert_eq!(starts.load(Ordering::Acquire), 2);
    assert_eq!(reads.load(Ordering::Acquire), 2);

    assert_eq!(file.write(b"a"), Ok(1));
    assert_eq!(file.write(b"b"), Ok(1));
    let mut source = iov_iter::iov_iter_kvec_source(b"c");
    let mut write_pos = 0;
    assert_eq!(file.write_iter_from(&mut source, &mut write_pos), Ok(1));
    assert_eq!(file.write(b""), Ok(0));
    let mut empty = EmptySource;
    let mut empty_iter = iov_iter_source(&mut empty);
    assert_eq!(file.write_iter(&mut empty_iter), Ok(0));
    assert_eq!(
        commands.lock().iter().cloned().collect::<alloc::vec::Vec<_>>(),
        alloc::vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );

    assert_eq!(file.write(b"123456789"), Err(crate::VfsError::FileTooLarge));
    let before = commands.lock().len();
    assert_eq!(
        <SeqFileInode<SingleRecord> as crate::FileOperations>::write(
            &scalar_inode,
            &file,
            b"123456789",
            0,
        ),
        Err(crate::VfsError::FileTooLarge)
    );
    assert_eq!(commands.lock().len(), before);
}

struct SharedRecord {
    data: alloc::sync::Arc<crate::Mutex<alloc::vec::Vec<u8>>>,
}

impl SeqIterator for SharedRecord {
    type Item = alloc::vec::Vec<u8>;

    fn rewind(&mut self) {}

    fn start(&mut self) -> Option<Self::Item> {
        Some(self.data.lock().clone())
    }

    fn next(&mut self) -> Option<Self::Item> {
        None
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        buf.push_str(core::str::from_utf8(item).map_err(|_| fmt::Error)?);
        Ok(())
    }
}

#[def_test]
fn seq_file_inode_rewind_observes_changes_from_another_fd() {
    use alloc::sync::Arc;

    use crate::{
        DirMapping, Filename, Mount, NodePermission, SeqFileInode, SeqFileOptions, SeekEndPolicy,
        SimpleDir, SimpleFs,
    };

    let source = Arc::new(crate::Mutex::new(b"old".to_vec()));
    let make_source = source.clone();
    let super_block = SimpleFs::new_with(&TEST_SEQ_FS_TYPE, 0, move |fs| {
        let mut root = DirMapping::new();
        root.add_file(
            "seq",
            SeqFileInode::new_regular_with_options(
                fs.clone(),
                {
                    let source = make_source.clone();
                    move || {
                        Ok(SharedRecord {
                            data: source.clone(),
                        })
                    }
                },
                SeqFileOptions {
                    permission: NodePermission::from_bits_truncate(0o444),
                    lazy_open: true,
                    max_record_bytes: 64,
                    reported_size: 0,
                    seek_end: SeekEndPolicy::Reject,
                    validate_open: None,
                    validate_read: None,
                    command: None,
                },
            ),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    })
    .unwrap();

    let root = Mount::new_root(&super_block).root_path();
    let open = || {
        Filename::new("/seq")
            .open_with_flags_at(
                &root,
                &root,
                linux_raw_sys::general::O_RDONLY,
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap()
    };
    let first = open();
    let mut buf = [0u8; 8];
    assert_eq!(first.read(&mut buf), Ok(3));
    assert_eq!(&buf[..3], b"old");
    *source.lock() = b"new!".to_vec();
    assert_eq!(first.read(&mut buf), Ok(0));
    assert_eq!(first.llseek(0, crate::file::SEEK_SET), Ok(0));
    assert_eq!(first.read(&mut buf), Ok(4));
    assert_eq!(&buf[..4], b"new!");
    let second = open();
    assert_eq!(second.read(&mut buf), Ok(4));
    assert_eq!(&buf[..4], b"new!");
}

#[def_test]
fn record_buffer_overflow_latches_first_error() {
    let mut buffer = super::RecordBuffer::try_new(4, usize::MAX).unwrap();
    core::fmt::Write::write_str(&mut buffer, "a").unwrap();
    assert_eq!(buffer.try_reserve(usize::MAX), Err(crate::VfsError::FileTooLarge));
    assert_eq!(buffer.take_error(), Some(crate::VfsError::FileTooLarge));
}

struct SecondRecordError {
    phase: usize,
}

impl SeqIterator for SecondRecordError {
    type Item = &'static str;

    fn rewind(&mut self) {
        self.phase = 0;
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.phase = 1;
        Some("first")
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.phase == 1 {
            self.phase = 2;
            None
        } else {
            None
        }
    }

    fn try_next(&mut self) -> crate::VfsResult<Option<Self::Item>> {
        self.phase = 2;
        Err(crate::VfsError::NoSuchProcess)
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

#[def_test]
fn partial_read_returns_copied_length_before_second_record_error() {
    let mut seq = SeqFile::new(SecondRecordError { phase: 0 });
    let mut buf = [0; 8];
    assert_eq!(seq.read(&mut buf, 0), Ok(5));
    assert_eq!(&buf[..5], b"first");
}

#[def_test]
fn partial_read_iter_returns_copied_length_before_second_record_error() {
    let mut seq = SeqFile::new(SecondRecordError { phase: 0 });
    let mut sink = VecSink {
        data: alloc::vec::Vec::new(),
        capacity: 8,
    };
    let mut iter = iov_iter_dest(&mut sink);
    assert_eq!(seq.read_iter(0, &mut iter), Ok(5));
    assert_eq!(sink.data, b"first");
}

struct FaultAfterFirst {
    copied: usize,
}

impl IovSink for FaultAfterFirst {
    fn count(&self) -> usize {
        8
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> kerrno::KResult<usize> {
        if self.copied == 0 {
            let count = src.len().min(2);
            self.copied = count;
            Ok(count)
        } else {
            Err(kerrno::KError::BadAddress)
        }
    }

    fn copy_to_iter_nofault(&mut self, src: &[u8]) -> kerrno::KResult<usize> {
        self.copy_to_iter(src)
    }
}

#[def_test]
fn read_iter_returns_copied_length_before_sink_fault() {
    struct One;

    impl SeqIterator for One {
        type Item = ();

        fn rewind(&mut self) {}

        fn start(&mut self) -> Option<Self::Item> {
            Some(())
        }

        fn next(&mut self) -> Option<Self::Item> {
            None
        }

        fn show(&self, _item: &Self::Item, buf: &mut String) -> fmt::Result {
            buf.push_str("record");
            Ok(())
        }
    }

    let mut seq = SeqFile::new(One);
    let mut sink = FaultAfterFirst { copied: 0 };
    let mut iter = iov_iter_dest(&mut sink);
    assert_eq!(seq.read_iter(0, &mut iter), Ok(2));
    assert_eq!(sink.copied, 2);
}

#[def_test]
fn vfs_partial_read_advances_position_before_second_record_error() {
    use alloc::sync::Arc;

    use crate::{
        DirMapping, Filename, Mount, NodePermission, SeqFileInode, SeqFileOptions, SeekEndPolicy,
        SimpleDir, SimpleFs,
    };

    let super_block = SimpleFs::new_with(&TEST_SEQ_FS_TYPE, 0, move |fs| {
        let mut root = DirMapping::new();
        root.add_file(
            "seq",
            SeqFileInode::new_regular_with_options(
                fs.clone(),
                || Ok(SecondRecordError { phase: 0 }),
                SeqFileOptions {
                    permission: NodePermission::from_bits_truncate(0o444),
                    lazy_open: true,
                    max_record_bytes: 64,
                    reported_size: 0,
                    seek_end: SeekEndPolicy::Reject,
                    validate_open: None,
                    validate_read: None,
                    command: None,
                },
            ),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    })
    .unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let file = Filename::new("/seq")
        .open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_RDONLY,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(file.read(&mut buf), Ok(5));
    assert_eq!(&buf[..5], b"first");
    assert_eq!(file.llseek(0, crate::file::SEEK_CUR), Ok(5));
    assert_eq!(file.read(&mut buf), Err(crate::VfsError::NoSuchProcess));
    assert_eq!(file.llseek(0, crate::file::SEEK_CUR), Ok(5));
}

struct ThreeRecordIter {
    phase: usize,
    fail_second_once: core::cell::Cell<bool>,
}

impl SeqIterator for ThreeRecordIter {
    type Item = &'static str;

    fn rewind(&mut self) {
        self.phase = 0;
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.phase = 1;
        Some("one\n")
    }

    fn next(&mut self) -> Option<Self::Item> {
        let item = match self.phase {
            1 => "two\n",
            2 => "three\n",
            _ => return None,
        };
        self.phase += 1;
        Some(item)
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        if *item == "two\n" && self.fail_second_once.replace(false) {
            return Err(fmt::Error);
        }
        buf.push_str(item);
        Ok(())
    }
}

#[def_test]
fn failed_second_record_is_retried_not_skipped() {
    let mut seq = SeqFile::new(ThreeRecordIter {
        phase: 0,
        fail_second_once: core::cell::Cell::new(true),
    });
    let mut first = [0u8; 8];
    assert_eq!(seq.read(&mut first, 0), Ok(4));
    assert_eq!(&first[..4], b"one\n");

    let mut rest = [0u8; 16];
    assert_eq!(seq.read(&mut rest, 4), Ok(10));
    assert_eq!(&rest[..10], b"two\nthree\n");
}

struct TwoRecords {
    phase: usize,
}

impl SeqIterator for TwoRecords {
    type Item = &'static str;

    fn rewind(&mut self) {
        self.phase = 0;
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.phase = 1;
        Some("ab")
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.phase == 1 {
            self.phase = 2;
            Some("cd")
        } else {
            None
        }
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

struct FaultSecondRecord {
    calls: usize,
    failed: bool,
    data: alloc::vec::Vec<u8>,
    capacity: usize,
}

impl IovSink for FaultSecondRecord {
    fn count(&self) -> usize {
        self.capacity - self.data.len()
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> kerrno::KResult<usize> {
        self.calls += 1;
        if self.calls == 2 && !self.failed {
            self.failed = true;
            return Err(kerrno::KError::BadAddress);
        }
        let count = src.len().min(self.count());
        self.data.extend_from_slice(&src[..count]);
        Ok(count)
    }

    fn copy_to_iter_nofault(&mut self, src: &[u8]) -> kerrno::KResult<usize> {
        self.copy_to_iter(src)
    }
}

#[def_test]
fn read_iter_sink_fault_keeps_unread_record_for_next_read() {
    let mut seq = SeqFile::new(TwoRecords { phase: 0 });
    let mut sink = FaultSecondRecord {
        calls: 0,
        failed: false,
        data: alloc::vec::Vec::new(),
        capacity: 16,
    };
    let mut iter = iov_iter_dest(&mut sink);
    assert_eq!(seq.read_iter(0, &mut iter), Ok(2));
    assert_eq!(sink.calls, 2);
    assert!(sink.failed);
    assert_eq!(sink.data, b"ab");

    let mut iter = iov_iter_dest(&mut sink);
    assert_eq!(seq.read_iter(2, &mut iter), Ok(2));
    assert_eq!(sink.data, b"abcd");
}

#[def_test]
fn vfs_read_iter_sink_fault_advances_only_copied_bytes() {
    use alloc::sync::Arc;

    use crate::{
        DirMapping, Filename, Mount, NodePermission, SeqFileInode, SeqFileOptions, SeekEndPolicy,
        SimpleDir, SimpleFs,
    };

    let super_block = SimpleFs::new_with(&TEST_SEQ_FS_TYPE, 0, move |fs| {
        let mut root = DirMapping::new();
        root.add_file(
            "seq",
            SeqFileInode::new_regular_with_options(
                fs.clone(),
                || Ok(TwoRecords { phase: 0 }),
                SeqFileOptions {
                    permission: NodePermission::from_bits_truncate(0o444),
                    lazy_open: true,
                    max_record_bytes: 64,
                    reported_size: 0,
                    seek_end: SeekEndPolicy::Reject,
                    validate_open: None,
                    validate_read: None,
                    command: None,
                },
            ),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    })
    .unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let file = Filename::new("/seq")
        .open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_RDONLY,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut sink = FaultSecondRecord {
        calls: 0,
        failed: false,
        data: alloc::vec::Vec::new(),
        capacity: 16,
    };
    let mut iter = iov_iter_dest(&mut sink);
    assert_eq!(file.read_iter(&mut iter), Ok(2));
    assert_eq!(sink.calls, 2);
    assert!(sink.failed);
    assert_eq!(sink.data, b"ab");
    assert_eq!(file.llseek(0, crate::file::SEEK_CUR), Ok(2));

    let mut iter = iov_iter_dest(&mut sink);
    assert_eq!(file.read_iter(&mut iter), Ok(2));
    assert_eq!(sink.data, b"abcd");
    assert_eq!(file.llseek(0, crate::file::SEEK_CUR), Ok(4));
}

struct ObservationErrorOnce {
    phase: usize,
    failed: core::cell::Cell<bool>,
}

impl SeqIterator for ObservationErrorOnce {
    type Item = &'static str;

    fn rewind(&mut self) {
        self.phase = 0;
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.phase = 1;
        Some("aa\n")
    }

    fn next(&mut self) -> Option<Self::Item> {
        match self.phase {
            1 => {
                self.phase = 2;
                Some("bb\n")
            }
            2 => {
                self.phase = 3;
                None
            }
            3 => {
                self.phase = 4;
                Some("cc\n")
            }
            _ => None,
        }
    }

    fn try_next(&mut self) -> crate::VfsResult<Option<Self::Item>> {
        if self.phase == 2 {
            if !self.failed.replace(true) {
                self.phase = 3;
                Err(crate::VfsError::NoSuchProcess)
            } else {
                self.phase = 4;
                Ok(Some("cc\n"))
            }
        } else {
            Ok(self.next())
        }
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

#[def_test]
fn observation_error_after_prefix_is_reported_then_position_is_rebuilt() {
    let mut seq = SeqFile::new(ObservationErrorOnce { phase: 0, failed: core::cell::Cell::new(false) });
    let mut first = [0u8; 8];
    assert_eq!(seq.read(&mut first, 0), Ok(6));
    assert_eq!(&first[..6], b"aa\nbb\n");

    assert_eq!(
        seq.read(&mut first, 6),
        Err(crate::VfsError::NoSuchProcess)
    );
    assert_eq!(&first[..6], b"aa\nbb\n");
    let mut third = [0u8; 8];
    assert_eq!(seq.read(&mut third, 6), Ok(3));
    assert_eq!(&third[..3], b"cc\n");
}

#[def_test]
fn vfs_observation_error_keeps_position_then_recovers_third_record() {
    use alloc::sync::Arc;

    use crate::{
        DirMapping, Filename, Mount, NodePermission, SeqFileInode, SeqFileOptions, SeekEndPolicy,
        SimpleDir, SimpleFs,
    };

    let super_block = SimpleFs::new_with(&TEST_SEQ_FS_TYPE, 0, move |fs| {
        let mut root = DirMapping::new();
        root.add_file(
            "seq",
            SeqFileInode::new_regular_with_options(
                fs.clone(),
                || Ok(ObservationErrorOnce { phase: 0, failed: core::cell::Cell::new(false) }),
                SeqFileOptions {
                    permission: NodePermission::from_bits_truncate(0o444),
                    lazy_open: true,
                    max_record_bytes: 64,
                    reported_size: 0,
                    seek_end: SeekEndPolicy::Reject,
                    validate_open: None,
                    validate_read: None,
                    command: None,
                },
            ),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    })
    .unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let file = Filename::new("/seq")
        .open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_RDONLY,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(file.read(&mut buf), Ok(6));
    assert_eq!(&buf[..6], b"aa\nbb\n");
    assert_eq!(file.llseek(0, crate::file::SEEK_CUR), Ok(6));

    assert_eq!(file.read(&mut buf), Err(crate::VfsError::NoSuchProcess));
    assert_eq!(file.llseek(0, crate::file::SEEK_CUR), Ok(6));

    let mut third = [0u8; 8];
    assert_eq!(file.read(&mut third), Ok(3));
    assert_eq!(&third[..3], b"cc\n");
    assert_eq!(file.llseek(0, crate::file::SEEK_CUR), Ok(9));
}

struct NoPrefixFormatError {
    phase: usize,
    fail_once: core::cell::Cell<bool>,
    rewinds: alloc::sync::Arc<core::sync::atomic::AtomicUsize>,
    starts: alloc::sync::Arc<core::sync::atomic::AtomicUsize>,
    nexts: alloc::sync::Arc<core::sync::atomic::AtomicUsize>,
}

impl SeqIterator for NoPrefixFormatError {
    type Item = &'static str;

    fn rewind(&mut self) {
        self.rewinds
            .fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        self.phase = 0;
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.starts
            .fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        self.phase = 1;
        Some("one\n")
    }

    fn next(&mut self) -> Option<Self::Item> {
        self.nexts
            .fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        let item = match self.phase {
            1 => "two\n",
            2 => "three\n",
            _ => return None,
        };
        self.phase += 1;
        Some(item)
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> fmt::Result {
        if *item == "two\n" && self.fail_once.replace(false) {
            return Err(fmt::Error);
        }
        buf.push_str(item);
        Ok(())
    }
}

#[def_test]
fn nonzero_offset_format_failure_retries_without_reposition() {
    let rewinds = alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let starts = alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let nexts = alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let mut seq = SeqFile::new(NoPrefixFormatError {
        phase: 0,
        fail_once: core::cell::Cell::new(true),
        rewinds: rewinds.clone(),
        starts: starts.clone(),
        nexts: nexts.clone(),
    });
    let mut first = [0u8; 4];
    assert_eq!(seq.read(&mut first, 0), Ok(4));
    assert_eq!(&first, b"one\n");
    let rewinds_after_first = rewinds.load(core::sync::atomic::Ordering::Acquire);

    let mut failed = [0u8; 16];
    assert_eq!(seq.read(&mut failed, 4), Err(crate::VfsError::Io));
    assert_eq!(
        rewinds.load(core::sync::atomic::Ordering::Acquire),
        rewinds_after_first
    );
    assert_eq!(starts.load(core::sync::atomic::Ordering::Acquire), 1);
    assert_eq!(nexts.load(core::sync::atomic::Ordering::Acquire), 1);

    let mut rest = [0u8; 16];
    assert_eq!(seq.read(&mut rest, 4), Ok(10));
    assert_eq!(&rest[..10], b"two\nthree\n");
    assert_eq!(
        rewinds.load(core::sync::atomic::Ordering::Acquire),
        rewinds_after_first
    );
    assert_eq!(starts.load(core::sync::atomic::Ordering::Acquire), 1);
    assert_eq!(nexts.load(core::sync::atomic::Ordering::Acquire), 3);
}
