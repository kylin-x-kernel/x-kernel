// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `pipe` object implementation.

use alloc::{borrow::Cow, format, sync::Arc};
use core::{
    mem,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use kerrno::{KError, KResult};
use kfd::{FileLike, IoDst, IoSrc, Kstat};
use kpoll::{IoEvents, PollSet, Pollable};
use ksignal::{SignalInfo, Signo};
use ksync::Mutex;
use ktask::future::{block_on, poll_io};
use linux_raw_sys::{
    general::{O_RDONLY, O_WRONLY, S_IFIFO},
    ioctl::FIONREAD,
};
use memaddr::PAGE_SIZE_4K;
use osvm::VirtMutPtr;
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer},
};

const RING_BUFFER_INIT_SIZE: usize = 65536; // 64 KiB
const PIPE_MAX_SIZE: usize = 1024 * 1024; // 1 MiB, matching Linux pipe-max-size default.
const PIPE_BUF: usize = 4096;

/// Shared pipe state exposed through read and write endpoints in the fd table.
pub struct PipeObject {
    state: Mutex<PipeState>,
    poll_rx: PollSet,
    poll_tx: PollSet,
}

struct PipeState {
    buffer: HeapRb<u8>,
    readers: usize,
    writers: usize,
}

fn write_into_vacant_ringbuf(state: &mut PipeState, src: &mut IoSrc) -> KResult<usize> {
    let (left, right) = state.buffer.vacant_slices_mut();

    // SAFETY: `vacant_slices_mut` exposes writable spare capacity, and `IoSrc`
    // only initializes the bytes it reports as written.
    let mut count = unsafe { src.read(left.assume_init_mut()) }?;
    if count >= left.len() {
        // SAFETY: same as above for the second vacant slice.
        count += unsafe { src.read(right.assume_init_mut()) }?;
    }

    // SAFETY: `count` is exactly the number of bytes just initialized in the
    // vacant slices, so advancing by it only publishes initialized bytes.
    unsafe { state.buffer.advance_write_index(count) };
    Ok(count)
}

/// Read endpoint for a pipe object.
pub struct PipeReadEnd {
    pipe: Arc<PipeObject>,
    non_blocking: AtomicBool,
}

/// Write endpoint for a pipe object.
pub struct PipeWriteEnd {
    pipe: Arc<PipeObject>,
    non_blocking: AtomicBool,
}

/// Pipe endpoint resolved from a file descriptor lookup.
pub enum PipeEndpoint {
    Read(Arc<PipeReadEnd>),
    Write(Arc<PipeWriteEnd>),
}

impl Drop for PipeReadEnd {
    fn drop(&mut self) {
        let should_wake = {
            let mut state = self.pipe.state.lock();
            state.readers = state
                .readers
                .checked_sub(1)
                .expect("pipe reader count underflow");
            state.readers == 0 && state.writers > 0
        };

        if should_wake {
            self.pipe.poll_rx.wake();
            self.pipe.poll_tx.wake();
        }
    }
}

impl Drop for PipeWriteEnd {
    fn drop(&mut self) {
        let should_wake = {
            let mut state = self.pipe.state.lock();
            state.writers = state
                .writers
                .checked_sub(1)
                .expect("pipe writer count underflow");
            state.writers == 0 && state.readers > 0
        };

        if should_wake {
            self.pipe.poll_rx.wake();
            self.pipe.poll_tx.wake();
        }
    }
}

impl PipeObject {
    /// Create a new pipe object and return its read and write endpoints.
    pub fn create_endpoints() -> (PipeReadEnd, PipeWriteEnd) {
        let pipe = Arc::new(Self {
            state: Mutex::new(PipeState {
                buffer: HeapRb::new(RING_BUFFER_INIT_SIZE),
                readers: 1,
                writers: 1,
            }),
            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
        });
        let read_end = PipeReadEnd {
            pipe: pipe.clone(),
            non_blocking: AtomicBool::new(false),
        };
        let write_end = PipeWriteEnd {
            pipe,
            non_blocking: AtomicBool::new(false),
        };
        (read_end, write_end)
    }

    fn path(&self) -> Cow<'_, str> {
        format!("pipe:[{}]", self as *const _ as usize).into()
    }

    fn capacity(&self) -> usize {
        self.state.lock().buffer.capacity().get()
    }

    fn resize(&self, new_size: usize) -> KResult<()> {
        let pages = new_size
            .checked_add(PAGE_SIZE_4K - 1)
            .ok_or(KError::InvalidInput)?
            / PAGE_SIZE_4K;
        let pages = pages
            .max(1)
            .checked_next_power_of_two()
            .ok_or(KError::InvalidInput)?;
        let new_size = pages
            .checked_mul(PAGE_SIZE_4K)
            .ok_or(KError::InvalidInput)?;
        if new_size > PIPE_MAX_SIZE {
            return Err(KError::OperationNotPermitted);
        }

        let should_wake_writers = {
            let mut state = self.state.lock();
            let old_size = state.buffer.capacity().get();
            if new_size == old_size {
                return Ok(());
            }
            if new_size < state.buffer.occupied_len() {
                return Err(KError::ResourceBusy);
            }

            let new_buffer = HeapRb::try_new(new_size).map_err(|_| KError::NoMemory)?;
            let old_buffer = mem::replace(&mut state.buffer, new_buffer);
            let (left, right) = old_buffer.as_slices();
            state.buffer.push_slice(left);
            state.buffer.push_slice(right);
            new_size > old_size
        };

        if should_wake_writers {
            self.poll_tx.wake();
        }

        Ok(())
    }

    fn readable_len(&self) -> usize {
        self.state.lock().buffer.occupied_len()
    }

    fn read(&self, nonblocking: bool, dst: &mut IoDst) -> KResult<usize> {
        if dst.is_full() {
            return Ok(0);
        }

        block_on(poll_io(self, IoEvents::IN, nonblocking, || {
            let (read, has_writers) = {
                let state = self.state.lock();
                let (left, right) = state.buffer.as_slices();
                let mut count = dst.write(left)?;
                if count >= left.len() {
                    count += dst.write(right)?;
                }

                // SAFETY: `count` is derived from bytes copied out of the current readable
                // slices, so it never exceeds the readable length protected by this lock.
                unsafe { state.buffer.advance_read_index(count) };
                (count, state.writers > 0)
            };

            if read > 0 {
                self.poll_tx.wake();
                Ok(read)
            } else if !has_writers {
                Ok(0)
            } else {
                Err(KError::WouldBlock)
            }
        }))
    }

    fn write(&self, nonblocking: bool, src: &mut IoSrc) -> KResult<usize> {
        let size = src.remaining();
        if size == 0 {
            return Ok(0);
        }

        let mut total_written = 0;
        block_on(poll_io(self, IoEvents::OUT, nonblocking, || {
            let written = {
                let mut state = self.state.lock();
                if state.readers == 0 {
                    None
                } else {
                    let available = state.buffer.vacant_len();
                    if total_written == 0 && size <= PIPE_BUF && available < size {
                        return Err(KError::WouldBlock);
                    }
                    Some(write_into_vacant_ringbuf(&mut state, src)?)
                }
            };

            let Some(written) = written else {
                raise_pipe();
                if total_written > 0 {
                    return Ok(total_written);
                }
                return Err(KError::BrokenPipe);
            };

            if written > 0 {
                self.poll_rx.wake();
                total_written += written;
                if total_written == size || nonblocking {
                    return Ok(total_written);
                }
            }

            Err(KError::WouldBlock)
        }))
    }
}

impl Pollable for PipeObject {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let state = self.state.lock();
        events.set(IoEvents::IN, state.buffer.occupied_len() > 0);
        events.set(IoEvents::OUT, state.buffer.vacant_len() > 0);
        events.set(IoEvents::HUP, state.writers == 0);
        events.set(IoEvents::ERR, state.readers == 0);
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
        }
        if events.contains(IoEvents::OUT) {
            self.poll_tx.register(context.waker());
        }
    }
}

impl PipeReadEnd {
    /// Returns `true` because this endpoint is the read side.
    pub fn is_read(&self) -> bool {
        true
    }

    /// Returns the current pipe buffer capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.pipe.capacity()
    }

    /// Resize the shared pipe buffer.
    pub fn resize(&self, new_size: usize) -> KResult<()> {
        self.pipe.resize(new_size)
    }
}

impl PipeWriteEnd {
    /// Returns `true` because this endpoint is the write side.
    pub fn is_write(&self) -> bool {
        true
    }

    /// Returns the current pipe buffer capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.pipe.capacity()
    }

    /// Resize the shared pipe buffer.
    pub fn resize(&self, new_size: usize) -> KResult<()> {
        self.pipe.resize(new_size)
    }
}

impl PipeEndpoint {
    /// Returns whether this endpoint is readable.
    pub fn is_read(&self) -> bool {
        matches!(self, Self::Read(_))
    }

    /// Returns whether this endpoint is writable.
    pub fn is_write(&self) -> bool {
        matches!(self, Self::Write(_))
    }

    /// Returns the current pipe buffer capacity in bytes.
    pub fn capacity(&self) -> usize {
        match self {
            Self::Read(pipe) => pipe.capacity(),
            Self::Write(pipe) => pipe.capacity(),
        }
    }

    /// Resize the shared pipe buffer behind either endpoint.
    pub fn resize(&self, new_size: usize) -> KResult<()> {
        match self {
            Self::Read(pipe) => pipe.resize(new_size),
            Self::Write(pipe) => pipe.resize(new_size),
        }
    }
}

/// Resolve the current process file descriptor to a concrete pipe endpoint.
pub fn current_pipe_endpoint(fd: i32) -> KResult<PipeEndpoint> {
    let resources = kprocess::current_resources();
    if let Ok(pipe) = resources.get_file_like_as::<PipeReadEnd>(fd) {
        return Ok(PipeEndpoint::Read(pipe));
    }
    if let Ok(pipe) = resources.get_file_like_as::<PipeWriteEnd>(fd) {
        return Ok(PipeEndpoint::Write(pipe));
    }
    Err(KError::BadFileDescriptor)
}

fn raise_pipe() {
    kprocess::process_signals::send_to_process(
        kprocess::current_user_thread().pid(),
        Some(SignalInfo::new_kernel(Signo::SIGPIPE)),
    )
    .expect("Failed to send SIGPIPE");
}

impl FileLike for PipeReadEnd {
    fn read(&self, dst: &mut IoDst) -> KResult<usize> {
        self.pipe.read(self.nonblocking(), dst)
    }

    fn stat(&self) -> KResult<Kstat> {
        Ok(Kstat {
            mode: S_IFIFO | 0o444,
            ..Default::default()
        })
    }

    fn path(&self) -> Cow<'_, str> {
        self.pipe.path()
    }

    fn open_flags(&self) -> u32 {
        O_RDONLY
    }

    fn set_nonblocking(&self, nonblocking: bool) -> KResult {
        self.non_blocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> KResult<usize> {
        match cmd {
            FIONREAD => {
                (arg as *mut u32).write_vm(self.pipe.readable_len() as u32)?;
                Ok(0)
            }
            _ => Err(KError::NotATty),
        }
    }
}

impl FileLike for PipeWriteEnd {
    fn write(&self, src: &mut IoSrc) -> KResult<usize> {
        self.pipe.write(self.nonblocking(), src)
    }

    fn stat(&self) -> KResult<Kstat> {
        Ok(Kstat {
            mode: S_IFIFO | 0o222,
            ..Default::default()
        })
    }

    fn path(&self) -> Cow<'_, str> {
        self.pipe.path()
    }

    fn open_flags(&self) -> u32 {
        O_WRONLY
    }

    fn set_nonblocking(&self, nonblocking: bool) -> KResult {
        self.non_blocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> KResult<usize> {
        match cmd {
            FIONREAD => {
                (arg as *mut u32).write_vm(self.pipe.readable_len() as u32)?;
                Ok(0)
            }
            _ => Err(KError::NotATty),
        }
    }
}

impl Pollable for PipeReadEnd {
    fn poll(&self) -> IoEvents {
        let mut events = self.pipe.poll();
        events.remove(IoEvents::OUT);
        events.remove(IoEvents::ERR);
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.pipe.register(context, events);
    }
}

impl Pollable for PipeWriteEnd {
    fn poll(&self) -> IoEvents {
        let mut events = self.pipe.poll();
        events.remove(IoEvents::IN);
        events.remove(IoEvents::HUP);
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.pipe.register(context, events);
    }
}

#[cfg(unittest)]
mod pipe_tests {
    use alloc::sync::Arc;

    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_pipe_creation() {
        let (read_end, write_end) = PipeObject::create_endpoints();

        assert!(read_end.is_read());
        assert!(write_end.is_write());
    }

    #[def_test]
    fn test_pipe_constants() {
        assert_eq!(S_IFIFO, 0o010000);
        assert_eq!(FIONREAD, 0x541B);
    }

    #[def_test]
    fn test_pipe_initial_capacity() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        assert_eq!(read_end.capacity(), RING_BUFFER_INIT_SIZE);
    }

    #[def_test]
    fn test_pipe_not_closed_both_alive() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        let read_state = read_end.pipe.state.lock();
        assert!(read_state.writers > 0);
        drop(read_state);

        let write_state = write_end.pipe.state.lock();
        assert!(write_state.readers > 0);
    }

    #[def_test]
    fn test_pipe_closed_when_other_dropped() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        drop(write_end);
        assert_eq!(read_end.pipe.state.lock().writers, 0);
    }

    #[def_test]
    fn test_pipe_closed_read_dropped() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        drop(read_end);
        assert_eq!(write_end.pipe.state.lock().readers, 0);
    }

    #[def_test]
    fn test_pipe_nonblocking_default() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        assert!(!read_end.nonblocking());
        assert!(!write_end.nonblocking());
    }

    #[def_test]
    fn test_pipe_set_nonblocking() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        read_end.set_nonblocking(true).unwrap();
        assert!(read_end.nonblocking());
        assert!(!write_end.nonblocking());

        write_end.set_nonblocking(true).unwrap();
        assert!(write_end.nonblocking());
    }

    #[def_test]
    fn test_pipe_stat_read_end() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        let stat = read_end.stat().unwrap();
        assert_eq!(stat.mode, S_IFIFO | 0o444);
    }

    #[def_test]
    fn test_pipe_stat_write_end() {
        let (_read_end, write_end) = PipeObject::create_endpoints();
        let stat = write_end.stat().unwrap();
        assert_eq!(stat.mode, S_IFIFO | 0o222);
    }

    #[def_test]
    fn test_pipe_path_format() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        let path = read_end.path();
        assert!(path.starts_with("pipe:["));
        assert!(path.ends_with("]"));
    }

    #[def_test]
    fn test_pipe_poll_empty() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        let r_events = read_end.poll();
        assert!(!r_events.contains(IoEvents::IN));
        let w_events = write_end.poll();
        assert!(w_events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_pipe_poll_hup_after_writer_dropped() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        drop(write_end);
        let events = read_end.poll();
        assert!(!events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::HUP));
    }

    #[def_test]
    fn test_pipe_poll_err_after_reader_dropped() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        drop(read_end);
        let events = write_end.poll();
        assert!(events.contains(IoEvents::OUT));
        assert!(events.contains(IoEvents::ERR));
    }

    #[def_test]
    fn test_pipe_arc_dup_writer_keeps_read_end_open() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        let writer = Arc::new(write_end);
        let writer_dup = writer.clone();

        drop(writer);
        assert!(!read_end.poll().contains(IoEvents::HUP));

        drop(writer_dup);
        assert!(read_end.poll().contains(IoEvents::HUP));
    }

    #[def_test]
    fn test_pipe_writer_close_with_buffered_data_reports_in_and_hup() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        let data = b"hello";
        let mut src = kio::Cursor::new(data.as_slice());

        assert_eq!(write_end.write(&mut src).unwrap(), data.len());
        drop(write_end);

        let events = read_end.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::HUP));

        let mut buf = [0u8; 5];
        let mut dst = kio::Cursor::new(buf.as_mut_slice());
        assert_eq!(read_end.read(&mut dst).unwrap(), data.len());
        assert_eq!(&buf, data);

        let events = read_end.poll();
        assert!(!events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::HUP));

        let mut eof_buf = [0u8; 1];
        let mut eof_dst = kio::Cursor::new(eof_buf.as_mut_slice());
        assert_eq!(read_end.read(&mut eof_dst).unwrap(), 0);
    }

    #[def_test]
    fn test_pipe_resize() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        read_end.resize(4096).unwrap();
        assert_eq!(read_end.capacity(), 4096);
    }

    #[def_test]
    fn test_pipe_resize_rounds_up() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        read_end.resize(5000).unwrap();
        assert_eq!(read_end.capacity(), 8192);
    }

    #[def_test]
    fn test_pipe_resize_rounds_up_to_power_of_two_pages() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        read_end.resize(12 * 1024).unwrap();
        assert_eq!(read_end.capacity(), 16 * 1024);

        read_end.resize(20 * 1024).unwrap();
        assert_eq!(read_end.capacity(), 32 * 1024);
    }

    #[def_test]
    fn test_pipe_resize_minimum() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        read_end.resize(0).unwrap();
        assert_eq!(read_end.capacity(), PAGE_SIZE_4K);
    }

    #[def_test]
    fn test_pipe_resize_to_maximum() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        read_end.resize(PIPE_MAX_SIZE).unwrap();
        assert_eq!(read_end.capacity(), PIPE_MAX_SIZE);
    }

    #[def_test]
    fn test_pipe_resize_rejects_excessive_size() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        assert_eq!(
            read_end.resize(PIPE_MAX_SIZE + 1),
            Err(KError::OperationNotPermitted)
        );
        assert_eq!(read_end.capacity(), RING_BUFFER_INIT_SIZE);
    }

    #[def_test]
    fn test_pipe_resize_rejects_overflow_size() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        assert_eq!(read_end.resize(usize::MAX), Err(KError::InvalidInput));
        assert_eq!(read_end.capacity(), RING_BUFFER_INIT_SIZE);
    }

    #[def_test]
    fn test_pipe_read_wrong_end() {
        let (_read_end, write_end) = PipeObject::create_endpoints();
        let mut buf = [0u8; 10];
        let mut dst = kio::Cursor::new(buf.as_mut_slice());
        assert!(write_end.read(&mut dst).is_err());
    }

    #[def_test]
    fn test_pipe_write_wrong_end() {
        let (read_end, _write_end) = PipeObject::create_endpoints();
        let data = b"hello";
        let mut src = kio::Cursor::new(data.as_slice());
        assert!(read_end.write(&mut src).is_err());
    }

    #[def_test]
    fn test_pipe_nonblocking_pipe_buf_write_is_atomic() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        read_end.resize(PAGE_SIZE_4K).unwrap();
        write_end.set_nonblocking(true).unwrap();

        let fill = [0u8; PAGE_SIZE_4K - 64];
        let mut fill_src = kio::Cursor::new(fill.as_slice());
        assert_eq!(write_end.write(&mut fill_src).unwrap(), fill.len());

        let payload = [1u8; 128];
        let mut payload_src = kio::Cursor::new(payload.as_slice());
        assert_eq!(write_end.write(&mut payload_src), Err(KError::WouldBlock));
        assert_eq!(read_end.pipe.readable_len(), fill.len());
    }

    #[def_test]
    fn test_pipe_nonblocking_large_write_allows_partial_progress() {
        let (read_end, write_end) = PipeObject::create_endpoints();
        read_end.resize(PAGE_SIZE_4K).unwrap();
        write_end.set_nonblocking(true).unwrap();

        let fill = [0u8; PAGE_SIZE_4K - 64];
        let mut fill_src = kio::Cursor::new(fill.as_slice());
        assert_eq!(write_end.write(&mut fill_src).unwrap(), fill.len());

        let payload = [1u8; PIPE_BUF + 1];
        let mut payload_src = kio::Cursor::new(payload.as_slice());
        assert_eq!(write_end.write(&mut payload_src).unwrap(), 64);
        assert_eq!(read_end.pipe.readable_len(), PAGE_SIZE_4K);
    }
}
