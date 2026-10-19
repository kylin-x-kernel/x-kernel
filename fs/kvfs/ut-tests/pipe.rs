// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::def_test;

use super::*;

#[def_test]
fn splice_rejects_oversized_write_without_consuming_source() {
    for reported in [4, usize::MAX] {
        let pipe = PipeObject::new_anonymous();
        pipe.write(true, true, b"abcdef").unwrap();
        let mut scratch = [0; 3];
        assert_eq!(
            pipe.splice_read(true, &mut scratch, |bytes| {
                assert_eq!(bytes, b"abc");
                Ok(reported)
            }),
            Err(KError::Io)
        );
        let mut remaining = [0; 6];
        assert_eq!(pipe.read(true, &mut remaining), Ok(6));
        assert_eq!(&remaining, b"abcdef");
    }
}

#[def_test]
fn splice_rejects_oversized_read_without_publishing_data() {
    for reported in [4, usize::MAX] {
        let pipe = PipeObject::new_anonymous();
        pipe.resize(PIPE_BUF).unwrap();
        let original = vec![1; PIPE_BUF - 3];
        pipe.write(true, true, &original).unwrap();
        let mut scratch = [0; 8];
        assert_eq!(
            pipe.splice_write(true, &mut scratch, |bytes| {
                assert_eq!(bytes.len(), 3);
                bytes.fill(2);
                Ok(reported)
            }),
            Err(KError::Io)
        );
        let mut remaining = vec![0; PIPE_BUF];
        assert_eq!(pipe.read(true, &mut remaining), Ok(original.len()));
        assert_eq!(&remaining[..original.len()], original.as_slice());
        assert_eq!(pipe.read(true, &mut remaining), Err(KError::WouldBlock));
    }
}

#[def_test]
fn splice_retains_rejected_and_short_written_data() {
    let pipe = PipeObject::new_anonymous();
    pipe.write(true, true, b"abcdef").unwrap();
    let mut scratch = [0; 6];
    assert_eq!(
        pipe.splice_read(true, &mut scratch, |_| Err(KError::IoWaitInterrupted)),
        Err(KError::IoWaitInterrupted)
    );
    assert_eq!(
        pipe.splice_read(true, &mut scratch, |bytes| {
            assert_eq!(bytes, b"abcdef");
            Ok(2)
        }),
        Ok(2)
    );
    let mut remaining = [0; 6];
    assert_eq!(pipe.read(true, &mut remaining), Ok(4));
    assert_eq!(&remaining[..4], b"cdef");
}

#[def_test]
fn splice_reserves_space_before_reading_source() {
    let pipe = PipeObject::new_anonymous();
    pipe.resize(PIPE_BUF).unwrap();
    pipe.write(true, true, &vec![1; PIPE_BUF]).unwrap();
    let mut scratch = [0; 8];
    assert_eq!(
        pipe.splice_write(true, &mut scratch, |_| panic!(
            "full pipe must not consume source"
        )),
        Err(KError::WouldBlock)
    );
    let mut drain = [0; 3];
    pipe.read(true, &mut drain).unwrap();
    assert_eq!(
        pipe.splice_write(true, &mut scratch, |bytes| {
            assert_eq!(bytes.len(), 3);
            bytes.copy_from_slice(b"end");
            Ok(3)
        }),
        Ok(3)
    );
}

#[def_test]
fn splice_between_pipes_retains_untransferred_suffix() {
    let source = PipeObject::new_anonymous();
    let destination = PipeObject::new_anonymous();
    destination.resize(PIPE_BUF).unwrap();
    let data = [vec![1; PIPE_BUF], vec![2; PIPE_BUF]].concat();
    source.write(true, false, &data).unwrap();
    assert_eq!(
        source.splice_to(&destination, data.len(), true),
        Ok(PIPE_BUF)
    );
    assert_eq!(
        source.splice_to(&destination, data.len(), true),
        Err(KError::WouldBlock)
    );
    let mut bytes = vec![0; PIPE_BUF];
    assert_eq!(source.read(true, &mut bytes), Ok(PIPE_BUF));
    assert!(bytes.iter().all(|byte| *byte == 2));
    assert_eq!(destination.read(true, &mut bytes), Ok(PIPE_BUF));
    assert!(bytes.iter().all(|byte| *byte == 1));
    assert_eq!(
        source.splice_to(&source, 1, true),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn interrupted_pipe_write_preserves_committed_prefix() {
    let pipe = PipeObject::new_anonymous();
    pipe.resize(PIPE_BUF).unwrap();
    let bytes = vec![0x53; PIPE_BUF + 17];
    let task = ktask::current();
    task.interrupt();
    let result = pipe.write(false, false, &bytes);
    task.clear_interrupt();
    assert_eq!(result, Ok(PIPE_BUF));
    let mut output = vec![0; PIPE_BUF + 17];
    assert_eq!(pipe.read(true, &mut output), Ok(PIPE_BUF));
    assert_eq!(&output[..PIPE_BUF], &bytes[..PIPE_BUF]);
    assert_eq!(pipe.read(true, &mut output), Err(KError::WouldBlock));
}

#[def_test]
fn interrupted_pipe_iterator_preserves_previous_block_and_rewinds_tail() {
    let pipe = PipeObject::new_anonymous();
    pipe.resize(PIPE_BUF).unwrap();
    let root = crate::Mount::new_root(&crate::nullfs::new_superblock()).root_path();
    let mut builder = VfsFileBuilder::from_path_state(
        root,
        FMode::WRITE,
        crate::OpenFlags::empty(),
        pipe_file_operations(),
        kcred::initial_cred(),
    );
    builder.set_private_data(pipe.clone());
    let file = builder.finish().unwrap();
    let bytes = vec![0x64; PIPE_BUF + 31];
    let mut iter = iov_iter::iov_iter_kvec_source(&bytes);
    let mut iocb = Kiocb::new(&file, 7);
    let task = ktask::current();
    task.interrupt();
    let result = pipe_write_iter(&mut iocb, &mut iter);
    task.clear_interrupt();
    assert_eq!(result, Ok(PIPE_BUF));
    assert_eq!(iocb.ki_pos(), 7 + PIPE_BUF as u64);
    assert_eq!(iter.count(), 31);
    assert_eq!(pipe.readable_len(), PIPE_BUF);
    let mut output = vec![0; PIPE_BUF];
    assert_eq!(pipe.read(true, &mut output), Ok(PIPE_BUF));
    assert_eq!(&output, &bytes[..PIPE_BUF]);
}

#[def_test]
fn interrupted_small_pipe_write_remains_atomic() {
    let pipe = PipeObject::new_anonymous();
    pipe.resize(PIPE_BUF).unwrap();
    assert_eq!(
        pipe.write(true, false, &vec![0x21; PIPE_BUF - 2]),
        Ok(PIPE_BUF - 2)
    );
    let task = ktask::current();
    task.interrupt();
    let result = pipe.write(false, true, b"abcd");
    task.clear_interrupt();
    assert_eq!(result, Err(KError::IoWaitInterrupted));
    assert_eq!(pipe.readable_len(), PIPE_BUF - 2);
    let mut output = vec![0; PIPE_BUF];
    assert_eq!(pipe.read(true, &mut output), Ok(PIPE_BUF - 2));
    assert!(output[..PIPE_BUF - 2].iter().all(|byte| *byte == 0x21));
}

#[def_test]
fn pipe_readiness_and_eof_precede_interrupt_but_empty_wait_is_marked() {
    let pipe = PipeObject::new_anonymous();
    let task = ktask::current();
    let mut output = [0; 4];
    task.interrupt();
    let waiting = pipe.read(false, &mut output);
    task.clear_interrupt();
    assert_eq!(waiting, Err(KError::IoWaitInterrupted));
    assert_eq!(pipe.write(true, true, b"data"), Ok(4));
    task.interrupt();
    let ready = pipe.read(false, &mut output);
    let preserved = task.is_interrupted();
    task.clear_interrupt();
    assert_eq!(ready, Ok(4));
    assert_eq!(&output, b"data");
    assert!(preserved);
    pipe.state.lock().writers = 0;
    task.interrupt();
    let eof = pipe.read(false, &mut output);
    task.clear_interrupt();
    assert_eq!(eof, Ok(0));
}

#[def_test]
fn fifo_partner_counter_records_completed_rendezvous() {
    let pipe = PipeObject::new_fifo();
    let partner_counter = pipe.state.lock().w_counter;
    {
        let mut state = pipe.state.lock();
        add_access(&mut state, PipeAccess::Write).unwrap();
        remove_access(&mut state, PipeAccess::Write);
    }

    let state = pipe.state.lock();
    assert_eq!(state.writers, 0);
    assert_ne!(state.w_counter, partner_counter);
}
