// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{borrow::Cow, format, sync::Arc};
use core::{
    mem,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use kcore::task::{AsThread, send_signal_to_process};
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollSet, Pollable};
use ksignal::{SignalInfo, Signo};
use ksync::Mutex;
use ktask::{
    current,
    future::{block_on, poll_io},
};
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

use super::{FileLike, Kstat};
use crate::file::{IoDst, IoSrc};

const RING_BUFFER_INIT_SIZE: usize = 65536; // 64 KiB

/// Shared state for both ends of a pipe.
struct Shared {
    /// Ring buffer for storing pipe data
    buffer: Mutex<HeapRb<u8>>,
    /// Poll set for read-side notifications
    poll_rx: PollSet,
    /// Poll set for write-side notifications
    poll_tx: PollSet,
    /// Poll set for close notifications
    poll_close: PollSet,
}

/// One end of a pipe (either read or write).
///
/// A pipe consists of two `Pipe` instances sharing common state.
/// Data can flow from the write end to the read end through a ring buffer.
pub struct Pipe {
    /// True if this is the read end, false if write end
    read_side: bool,
    /// Shared state between both ends
    shared: Arc<Shared>,
    /// Non-blocking flag for this pipe end
    non_blocking: AtomicBool,
}
impl Drop for Pipe {
    /// Wakes all waiters on pipe close when this end is dropped.
    fn drop(&mut self) {
        self.shared.poll_close.wake();
    }
}

impl Pipe {
    /// Creates a new pipe, returning both read and write ends.
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

    /// Checks if this is the read end of the pipe.
    pub const fn is_read(&self) -> bool {
        self.read_side
    }

    /// Checks if this is the write end of the pipe.
    pub const fn is_write(&self) -> bool {
        !self.read_side
    }

    /// Checks if the other end of the pipe has been closed.
    pub fn closed(&self) -> bool {
        Arc::strong_count(&self.shared) == 1
    }

    /// Returns the current capacity of the pipe buffer.
    pub fn capacity(&self) -> usize {
        self.shared.buffer.lock().capacity().get()
    }

    /// Resizes the pipe buffer to a new size (rounded up to page size).
    /// Returns error if new size is smaller than occupied data.
    pub fn resize(&self, new_size: usize) -> KResult<()> {
        let new_size = new_size.div_ceil(PAGE_SIZE_4K).max(1) * PAGE_SIZE_4K;

        let mut buffer = self.shared.buffer.lock();
        if new_size == buffer.capacity().get() {
            return Ok(());
        }
        if new_size < buffer.occupied_len() {
            return Err(KError::ResourceBusy);
        }
        let old_buffer = mem::replace(&mut *buffer, HeapRb::new(new_size));
        let (left, right) = old_buffer.as_slices();
        buffer.push_slice(left);
        buffer.push_slice(right);
        Ok(())
    }
}

/// Sends SIGPIPE signal to the current process.
fn raise_pipe() {
    let curr = current();
    send_signal_to_process(
        curr.as_thread().proc_data.proc.pid(),
        Some(SignalInfo::new_kernel(Signo::SIGPIPE)),
    )
    .expect("Failed to send SIGPIPE");
}

impl FileLike for Pipe {
    /// Reads data from the pipe (read end only).
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

    /// Writes data to the pipe (write end only).
    /// Sends SIGPIPE if the read end is closed.
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

    /// Returns pipe statistics.
    fn stat(&self) -> KResult<Kstat> {
        Ok(Kstat {
            mode: S_IFIFO | if self.is_read() { 0o444 } else { 0o222 },
            ..Default::default()
        })
    }

    /// Returns a string representation of the pipe.
    fn path(&self) -> Cow<'_, str> {
        format!("pipe:[{}]", self as *const _ as usize).into()
    }

    /// Returns the open flags for this pipe end (O_RDONLY for read end, O_WRONLY for write end).
    fn open_flags(&self) -> u32 {
        if self.is_read() { O_RDONLY } else { O_WRONLY }
    }

    /// Sets or clears the non-blocking flag.
    fn set_nonblocking(&self, nonblocking: bool) -> KResult {
        self.non_blocking.store(nonblocking, Ordering::Release);
        Ok(())
    }

    /// Checks if non-blocking mode is enabled.
    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    /// Performs I/O control operations (supports FIONREAD).
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
    /// Polls for available I/O events.
    /// Read end: checks if data is available or if closed.
    /// Write end: checks if buffer space is available.
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let buf = self.shared.buffer.lock();
        if self.read_side {
            let closed = self.closed();
            // Report IN when there is data to read OR when write end is closed
            // (read will return 0 = EOF). This matches Linux behavior where
            // select() reports a pipe as readable when the write end closes.
            events.set(IoEvents::IN, buf.occupied_len() > 0 || closed);
            events.set(IoEvents::HUP, closed);
        } else {
            events.set(IoEvents::OUT, buf.vacant_len() > 0);
        }
        events
    }

    /// Registers the pipe for polling with the given context and events.
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

    /// Test pipe creation yields read and write ends
    #[def_test]
    fn test_pipe_creation() {
        let (read_end, write_end) = Pipe::new();

        assert!(read_end.is_read());
        assert!(!read_end.is_write());
        assert!(!write_end.is_read());
        assert!(write_end.is_write());
    }

    /// Test pipe constants
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
        use kpoll::Pollable;
        let (read_end, write_end) = Pipe::new();
        let r_events = read_end.poll();
        assert!(!r_events.contains(IoEvents::IN));
        let w_events = write_end.poll();
        assert!(w_events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_pipe_poll_closed() {
        use kpoll::Pollable;
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
