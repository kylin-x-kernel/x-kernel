// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
use kthread::send_signal_to_process;
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

struct Shared {
    buffer: Mutex<HeapRb<u8>>,
    poll_rx: PollSet,
    poll_tx: PollSet,
    poll_close: PollSet,
}

/// One end of a pipe.
pub struct Pipe {
    read_side: bool,
    shared: Arc<Shared>,
    non_blocking: AtomicBool,
}

impl Drop for Pipe {
    fn drop(&mut self) {
        self.shared.poll_close.wake();
    }
}

impl Pipe {
    /// Create a new pipe and return its read and write ends.
    pub fn new() -> (Pipe, Pipe) {
        let shared = Arc::new(Shared {
            buffer: Mutex::new(HeapRb::new(RING_BUFFER_INIT_SIZE)),
            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
            poll_close: PollSet::new(),
        });
        let read_end = Pipe {
            read_side: true,
            shared: shared.clone(),
            non_blocking: AtomicBool::new(false),
        };
        let write_end = Pipe {
            read_side: false,
            shared,
            non_blocking: AtomicBool::new(false),
        };
        (read_end, write_end)
    }

    pub const fn is_read(&self) -> bool {
        self.read_side
    }

    pub const fn is_write(&self) -> bool {
        !self.read_side
    }

    pub fn closed(&self) -> bool {
        Arc::strong_count(&self.shared) == 1
    }

    pub fn capacity(&self) -> usize {
        self.shared.buffer.lock().capacity().get()
    }

    pub fn resize(&self, new_size: usize) -> KResult<()> {
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

        let mut buffer = self.shared.buffer.lock();
        let old_size = buffer.capacity().get();
        if new_size == old_size {
            return Ok(());
        }
        if new_size < buffer.occupied_len() {
            return Err(KError::ResourceBusy);
        }

        let new_buffer = HeapRb::try_new(new_size).map_err(|_| KError::NoMemory)?;
        let old_buffer = mem::replace(&mut *buffer, new_buffer);
        let (left, right) = old_buffer.as_slices();
        buffer.push_slice(left);
        buffer.push_slice(right);

        if new_size > old_size {
            self.shared.poll_tx.wake();
        }

        Ok(())
    }
}

fn raise_pipe() {
    send_signal_to_process(
        kthread::current_thread().pid(),
        Some(SignalInfo::new_kernel(Signo::SIGPIPE)),
    )
    .expect("Failed to send SIGPIPE");
}

impl FileLike for Pipe {
    fn read(&self, dst: &mut IoDst) -> KResult<usize> {
        if !self.is_read() {
            return Err(KError::BadFileDescriptor);
        }
        if dst.is_full() {
            return Ok(0);
        }

        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            let read = {
                let cons = self.shared.buffer.lock();
                let (left, right) = cons.as_slices();
                let mut count = dst.write(left)?;
                if count >= left.len() {
                    count += dst.write(right)?;
                }
                unsafe { cons.advance_read_index(count) };
                count
            };

            if read > 0 {
                self.shared.poll_tx.wake();
                Ok(read)
            } else if self.closed() {
                Ok(0)
            } else {
                Err(KError::WouldBlock)
            }
        }))
    }

    fn write(&self, src: &mut IoSrc) -> KResult<usize> {
        if !self.is_write() {
            return Err(KError::BadFileDescriptor);
        }
        let size = src.remaining();
        if size == 0 {
            return Ok(0);
        }

        let mut total_written = 0;
        block_on(poll_io(self, IoEvents::OUT, self.nonblocking(), || {
            if self.closed() {
                raise_pipe();
                return Err(KError::BrokenPipe);
            }

            let written = {
                let mut prod = self.shared.buffer.lock();
                let (left, right) = prod.vacant_slices_mut();
                let mut count = src.read(unsafe { left.assume_init_mut() })?;
                if count >= left.len() {
                    count += src.read(unsafe { right.assume_init_mut() })?;
                }
                unsafe { prod.advance_write_index(count) };
                count
            };

            if written > 0 {
                self.shared.poll_rx.wake();
                total_written += written;
                if total_written == size || self.nonblocking() {
                    return Ok(total_written);
                }
            }

            Err(KError::WouldBlock)
        }))
    }

    fn stat(&self) -> KResult<Kstat> {
        Ok(Kstat {
            mode: S_IFIFO | if self.is_read() { 0o444 } else { 0o222 },
            ..Default::default()
        })
    }

    fn path(&self) -> Cow<'_, str> {
        format!("pipe:[{}]", self as *const _ as usize).into()
    }

    fn open_flags(&self) -> u32 {
        if self.is_read() { O_RDONLY } else { O_WRONLY }
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
                (arg as *mut u32).write_vm(self.shared.buffer.lock().occupied_len() as u32)?;
                Ok(0)
            }
            _ => Err(KError::NotATty),
        }
    }
}

impl Pollable for Pipe {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let buf = self.shared.buffer.lock();
        if self.read_side {
            let closed = self.closed();
            events.set(IoEvents::IN, buf.occupied_len() > 0 || closed);
            events.set(IoEvents::HUP, closed);
        } else {
            events.set(IoEvents::OUT, buf.vacant_len() > 0);
        }

        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.shared.poll_rx.register(context.waker());
        }
        if events.contains(IoEvents::OUT) {
            self.shared.poll_tx.register(context.waker());
        }
        self.shared.poll_close.register(context.waker());
    }
}

#[cfg(unittest)]
mod pipe_tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_pipe_creation() {
        let (read_end, write_end) = Pipe::new();

        assert!(read_end.is_read());
        assert!(!read_end.is_write());
        assert!(!write_end.is_read());
        assert!(write_end.is_write());
    }

    #[def_test]
    fn test_pipe_constants() {
        assert_eq!(S_IFIFO, 0o010000);
        assert_eq!(FIONREAD, 0x541B);
    }

    #[def_test]
    fn test_pipe_initial_capacity() {
        let (read_end, _write_end) = Pipe::new();
        assert_eq!(read_end.capacity(), RING_BUFFER_INIT_SIZE);
    }

    #[def_test]
    fn test_pipe_not_closed_both_alive() {
        let (read_end, write_end) = Pipe::new();
        assert!(!read_end.closed());
        assert!(!write_end.closed());
    }

    #[def_test]
    fn test_pipe_closed_when_other_dropped() {
        let (read_end, write_end) = Pipe::new();
        drop(write_end);
        assert!(read_end.closed());
    }

    #[def_test]
    fn test_pipe_closed_read_dropped() {
        let (read_end, write_end) = Pipe::new();
        drop(read_end);
        assert!(write_end.closed());
    }

    #[def_test]
    fn test_pipe_nonblocking_default() {
        let (read_end, write_end) = Pipe::new();
        assert!(!read_end.nonblocking());
        assert!(!write_end.nonblocking());
    }

    #[def_test]
    fn test_pipe_set_nonblocking() {
        let (read_end, write_end) = Pipe::new();
        read_end.set_nonblocking(true).unwrap();
        assert!(read_end.nonblocking());
        assert!(!write_end.nonblocking());

        write_end.set_nonblocking(true).unwrap();
        assert!(write_end.nonblocking());
    }

    #[def_test]
    fn test_pipe_stat_read_end() {
        let (read_end, _write_end) = Pipe::new();
        let stat = read_end.stat().unwrap();
        assert_eq!(stat.mode, S_IFIFO | 0o444);
    }

    #[def_test]
    fn test_pipe_stat_write_end() {
        let (_read_end, write_end) = Pipe::new();
        let stat = write_end.stat().unwrap();
        assert_eq!(stat.mode, S_IFIFO | 0o222);
    }

    #[def_test]
    fn test_pipe_path_format() {
        let (read_end, _write_end) = Pipe::new();
        let path = read_end.path();
        assert!(path.starts_with("pipe:["));
        assert!(path.ends_with("]"));
    }

    #[def_test]
    fn test_pipe_poll_empty() {
        let (read_end, write_end) = Pipe::new();
        let r_events = read_end.poll();
        assert!(!r_events.contains(IoEvents::IN));
        let w_events = write_end.poll();
        assert!(w_events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_pipe_poll_closed() {
        let (read_end, write_end) = Pipe::new();
        drop(write_end);
        let events = read_end.poll();
        assert!(events.contains(IoEvents::HUP));
    }

    #[def_test]
    fn test_pipe_resize() {
        let (read_end, _write_end) = Pipe::new();
        read_end.resize(4096).unwrap();
        assert_eq!(read_end.capacity(), 4096);
    }

    #[def_test]
    fn test_pipe_resize_rounds_up() {
        let (read_end, _write_end) = Pipe::new();
        read_end.resize(5000).unwrap();
        assert_eq!(read_end.capacity(), 8192);
    }

    #[def_test]
    fn test_pipe_resize_rounds_up_to_power_of_two_pages() {
        let (read_end, _write_end) = Pipe::new();
        read_end.resize(12 * 1024).unwrap();
        assert_eq!(read_end.capacity(), 16 * 1024);

        read_end.resize(20 * 1024).unwrap();
        assert_eq!(read_end.capacity(), 32 * 1024);
    }

    #[def_test]
    fn test_pipe_resize_minimum() {
        let (read_end, _write_end) = Pipe::new();
        read_end.resize(0).unwrap();
        assert_eq!(read_end.capacity(), PAGE_SIZE_4K);
    }

    #[def_test]
    fn test_pipe_resize_to_maximum() {
        let (read_end, _write_end) = Pipe::new();
        read_end.resize(PIPE_MAX_SIZE).unwrap();
        assert_eq!(read_end.capacity(), PIPE_MAX_SIZE);
    }

    #[def_test]
    fn test_pipe_resize_rejects_excessive_size() {
        let (read_end, _write_end) = Pipe::new();
        assert_eq!(
            read_end.resize(PIPE_MAX_SIZE + 1),
            Err(KError::OperationNotPermitted)
        );
        assert_eq!(read_end.capacity(), RING_BUFFER_INIT_SIZE);
    }

    #[def_test]
    fn test_pipe_resize_rejects_overflow_size() {
        let (read_end, _write_end) = Pipe::new();
        assert_eq!(read_end.resize(usize::MAX), Err(KError::InvalidInput));
        assert_eq!(read_end.capacity(), RING_BUFFER_INIT_SIZE);
    }

    #[def_test]
    fn test_pipe_read_wrong_end() {
        let (_read_end, write_end) = Pipe::new();
        let mut buf = [0u8; 10];
        let mut dst = kio::Cursor::new(buf.as_mut_slice());
        assert!(write_end.read(&mut dst).is_err());
    }

    #[def_test]
    fn test_pipe_write_wrong_end() {
        let (read_end, _write_end) = Pipe::new();
        let data = b"hello";
        let mut src = kio::Cursor::new(data.as_slice());
        assert!(read_end.write(&mut src).is_err());
    }
}
