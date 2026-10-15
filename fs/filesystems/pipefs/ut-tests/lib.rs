// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use kpoll::IoEvents;
use ktime_types::SystemTime;
use kvfs::{SetattrTime, VfsError};
use unittest::def_test;

use super::*;

fn pipe_files() -> (Arc<VfsFile>, Arc<VfsFile>, Arc<PipeObject>) {
    init_pipefs();
    let (read_file, write_file) = create_pipe_files(OpenFlags::empty(), kcred::initial_cred())
        .expect("anonymous pipe files open");
    let pipe = PipeObject::from_file(&read_file).expect("pipe state is installed");
    (read_file, write_file, pipe)
}

#[def_test]
fn anonymous_pipe_files_share_inode_owned_state() {
    let (read_file, write_file, pipe) = pipe_files();
    let write_pipe = PipeObject::from_file(&write_file).expect("pipe state is installed");
    let inode_pipe = read_file
        .path()
        .inode()
        .downcast::<PipeObject>()
        .expect("pipe inode owns pipe state");

    assert!(read_file.mode().contains(FMode::READ));
    assert!(write_file.mode().contains(FMode::WRITE));
    assert!(Arc::ptr_eq(&pipe, &write_pipe));
    assert!(Arc::ptr_eq(&pipe, &inode_pipe));
    assert_eq!(pipe.capacity(), 64 * 1024);
    assert!(
        read_file
            .path()
            .display_path()
            .unwrap()
            .starts_with("pipe:[")
    );
}

#[def_test]
fn distinct_anonymous_pipes_have_distinct_inodes() {
    let (first_read, first_write, _) = pipe_files();
    let (second_read, ..) = pipe_files();

    assert_eq!(
        first_read.path().inode().inode(),
        first_write.path().inode().inode()
    );
    assert_ne!(
        first_read.path().inode().inode(),
        second_read.path().inode().inode()
    );
}

#[def_test]
fn anonymous_pipe_derives_endpoint_access_flags() {
    init_pipefs();
    let (read_file, write_file) = create_pipe_files(
        OpenFlags::APPEND | OpenFlags::NONBLOCK,
        kcred::initial_cred(),
    )
    .expect("anonymous pipe files open");

    assert_eq!(read_file.flags(), OpenFlags::NONBLOCK);
    assert_eq!(
        write_file.flags(),
        OpenFlags::WRITE_ONLY | OpenFlags::NONBLOCK
    );
}

#[def_test]
fn anonymous_pipe_poll_after_writer_drop() {
    let (read_file, write_file, _) = pipe_files();
    drop(write_file);

    let events = read_file.poll();
    assert!(!events.contains(IoEvents::IN));
    assert!(events.contains(IoEvents::HUP));
}

#[def_test]
fn anonymous_pipe_buffered_data_survives_writer_drop() {
    let (read_file, write_file, _) = pipe_files();
    let data = b"hello";
    assert_eq!(write_file.write(data).unwrap(), data.len());
    drop(write_file);

    let events = read_file.poll();
    assert!(events.contains(IoEvents::IN));
    assert!(events.contains(IoEvents::HUP));

    let mut buf = [0u8; 5];
    assert_eq!(read_file.read(&mut buf).unwrap(), data.len());
    assert_eq!(&buf, data);
}

#[def_test]
fn anonymous_pipe_resize_rounds_to_power_of_two_pages() {
    let (_, _, pipe) = pipe_files();

    pipe.resize(5000).unwrap();
    assert_eq!(pipe.capacity(), 8192);
    pipe.resize(12 * 1024).unwrap();
    assert_eq!(pipe.capacity(), 16 * 1024);
}

#[def_test]
fn anonymous_pipe_resize_rejects_invalid_sizes() {
    let (_, _, pipe) = pipe_files();

    assert_eq!(
        pipe.resize(1024 * 1024 + 1),
        Err(KError::OperationNotPermitted)
    );
    assert_eq!(pipe.capacity(), 64 * 1024);
    assert_eq!(pipe.resize(usize::MAX), Err(KError::InvalidInput));
    assert_eq!(pipe.capacity(), 64 * 1024);
}

#[def_test]
fn anonymous_pipe_nonblocking_small_write_is_atomic() {
    let (_read_file, write_file, pipe) = pipe_files();
    pipe.resize(PAGE_SIZE_4K).unwrap();
    write_file.set_nonblocking(true);

    let fill = [0u8; PAGE_SIZE_4K - 64];
    assert_eq!(write_file.write(&fill).unwrap(), fill.len());

    let payload = [1u8; 128];
    assert_eq!(write_file.write(&payload), Err(KError::WouldBlock));
}

#[def_test]
fn anonymous_pipe_accepts_owner_update() {
    let (read_file, _write_file, _) = pipe_files();
    let cred = kcred::initial_cred();

    read_file
        .path()
        .chown(Some(1234), Some(5678), &cred)
        .expect("pipe fd accepts chown");

    let metadata = read_file.path().getattr().unwrap();
    assert_eq!(metadata.uid, 1234);
    assert_eq!(metadata.gid, 5678);
}

#[def_test]
fn anonymous_pipe_accepts_mode_update() {
    let (read_file, _write_file, _) = pipe_files();
    let cred = kcred::initial_cred();

    read_file
        .path()
        .chmod(NodePermission::from_bits_truncate(0o640), &cred)
        .expect("pipe fd accepts chmod");

    let mode = read_file.path().getattr().unwrap().mode;
    let permission = mode.permission();
    assert!(permission.contains(NodePermission::OWNER_READ));
    assert!(permission.contains(NodePermission::OWNER_WRITE));
    assert!(permission.contains(NodePermission::GROUP_READ));
    assert!(!permission.contains(NodePermission::GROUP_WRITE));
    assert!(!permission.contains(NodePermission::OTHER_READ));
}

#[def_test]
fn anonymous_pipe_accepts_time_update() {
    let (read_file, _write_file, _) = pipe_files();
    let cred = kcred::initial_cred();
    let past = SetattrTime::Explicit(SystemTime::from_unix_seconds(1));

    read_file
        .path()
        .set_times(Some(past), Some(past), &cred)
        .expect("pipe fd accepts utimens");

    let metadata = read_file.path().getattr().unwrap();
    assert_eq!(metadata.atime.unix_seconds(), 1);
    assert_eq!(metadata.mtime.unix_seconds(), 1);
}

#[def_test]
fn anonymous_pipe_truncate_is_invalid() {
    let (_read_file, write_file, _) = pipe_files();

    assert!(matches!(
        write_file.truncate(16),
        Err(VfsError::InvalidInput)
    ));
}

#[def_test]
fn anonymous_pipe_io_updates_timestamps() {
    let (read_file, write_file, _) = pipe_files();
    let cred = kcred::initial_cred();
    let past = SetattrTime::Explicit(SystemTime::from_unix_seconds(1));
    read_file
        .path()
        .set_times(Some(past), Some(past), &cred)
        .expect("pipe fd accepts utimens");

    let data = b"hello";
    assert_eq!(write_file.write(data).unwrap(), data.len());
    let after_write = read_file.path().getattr().unwrap();
    assert!(after_write.mtime.unix_seconds() > 1);
    assert!(after_write.ctime.unix_seconds() > 1);

    let mut buf = [0u8; 5];
    assert_eq!(read_file.read(&mut buf).unwrap(), data.len());
    let after_read = read_file.path().getattr().unwrap();
    assert!(after_read.atime.unix_seconds() > 1);
}

/// `splice(pipe -> file, &off)` must publish the transferred count into
/// the caller's explicit offset: the pipe-source pump writes its local
/// position back after settlement. Without that write-back a caller
/// looping on the manual-page example would rewrite the same range.
#[def_test]
fn splice_pipe_to_file_advances_explicit_offset() {
    init_pipefs();
    let (read_file, write_file) = create_pipe_files(OpenFlags::empty(), kcred::initial_cred())
        .expect("anonymous pipe files open");
    let data: alloc::vec::Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(write_file.write(&data).unwrap(), data.len());
    let output =
        memfs::shmem::create_memfd_file("splice-off-dst", false, kcred::initial_cred())
            .unwrap()
            .into_file(kcred::initial_cred())
            .unwrap();
    let mut offset = 0;
    assert_eq!(
        kvfs::transfer_files(
            &read_file,
            kvfs::TransferPosition::Managed,
            &output,
            kvfs::TransferPosition::At(&mut offset),
            data.len(),
            false,
        )
        .unwrap(),
        data.len()
    );
    assert_eq!(offset, data.len() as u64);
    assert_eq!(output.position(), 0);
}
