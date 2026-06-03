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

/// Shared state for both ends of a pipe.
struct Shared {
    /// Pipe protocol state.
    state: Mutex<PipeState>,
    /// Poll set for read-side notifications
    poll_rx: PollSet,
    /// Poll set for write-side notifications
    poll_tx: PollSet,
}

/// Protocol state for one pipe.
struct PipeState {
    /// Ring buffer for storing pipe data.
    buffer: HeapRb<u8>,
    /// Number of live read endpoints.
    readers: usize,
    /// Number of live write endpoints.
    writers: usize,
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
    /// Updates endpoint state and wakes waiters when one side disappears.
    fn drop(&mut self) {
        let should_wake = {
            let mut state = self.shared.state.lock();

            if self.read_side {
                state.readers = state
                    .readers
                    .checked_sub(1)
                    .expect("pipe reader count underflow");
            } else {
                state.writers = state
                    .writers
                    .checked_sub(1)
                    .expect("pipe writer count underflow");
            }

            (state.readers == 0) != (state.writers == 0)
        };

        if should_wake {
            self.shared.poll_rx.wake();
            self.shared.poll_tx.wake();
        }
    }
}

impl Pipe {
    /// Creates a new pipe, returning both read and write ends.
    pub fn new() -> (Pipe, Pipe) {
        let shared = Arc::new(Shared {
            state: Mutex::new(PipeState {
                buffer: HeapRb::new(RING_BUFFER_INIT_SIZE),
                readers: 1,
                writers: 1,
            }),
            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
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

    /// Returns the current capacity of the pipe buffer.
    pub fn capacity(&self) -> usize {
        self.shared.state.lock().buffer.capacity().get()
    }

    /// Resizes the pipe buffer to a new size (rounded up to page size).
    /// Returns error if new size is smaller than occupied data.
    pub fn resize(&self, new_size: usize) -> KResult<()> {
        let new_size = new_size.div_ceil(PAGE_SIZE_4K).max(1) * PAGE_SIZE_4K;

        let should_wake_writers = {
            let mut state = self.shared.state.lock();
            let old_size = state.buffer.capacity().get();
            if new_size == old_size {
                return Ok(());
            }
            if new_size < state.buffer.occupied_len() {
                return Err(KError::ResourceBusy);
            }
            let old_buffer = mem::replace(&mut state.buffer, HeapRb::new(new_size));
            let (left, right) = old_buffer.as_slices();
            state.buffer.push_slice(left);
            state.buffer.push_slice(right);
            new_size > old_size
        };

        if should_wake_writers {
            self.shared.poll_tx.wake();
        }

        Ok(())
    }
}

/// Sends SIGPIPE signal to the current process.
fn raise_pipe() {
    send_signal_to_process(
        kthread::current_thread().pid(),
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
            let (read, has_writers) = {
                let state = self.shared.state.lock();
                let (left, right) = state.buffer.as_slices();
                let mut count = dst.write(left)?;
                if count >= left.len() {
                    count += dst.write(right)?;
                }
                // SAFETY: `count` is the number of bytes copied from the
                // occupied slices returned by `as_slices`, so advancing by it
                // stays within initialized readable data.
                unsafe { state.buffer.advance_read_index(count) };
                (count, state.writers > 0)
            };
            if read > 0 {
                self.shared.poll_tx.wake();
                Ok(read)
            } else if !has_writers {
                Ok(0)
            } else {
                Err(KError::WouldBlock)
            }
        }))
    }

    /// Writes data to the pipe (write end only).
    /// Sends SIGPIPE if no read endpoint remains.
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
            let written = {
                let mut state = self.shared.state.lock();
                if state.readers == 0 {
                    None
                } else {
                    let (left, right) = state.buffer.vacant_slices_mut();
                    // SAFETY: `vacant_slices_mut` exposes uninitialized
                    // capacity that `IoSrc::read` treats as output storage.
                    let mut count = src.read(unsafe { left.assume_init_mut() })?;
                    if count >= left.len() {
                        // SAFETY: same as above for the second vacant slice.
                        count += src.read(unsafe { right.assume_init_mut() })?;
                    }
                    // SAFETY: `count` is exactly the number of bytes written
                    // into the vacant slices, so advancing by it marks only
                    // initialized bytes as readable.
                    unsafe { state.buffer.advance_write_index(count) };
                    Some(count)
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
                (arg as *mut u32).write_vm(self.shared.state.lock().buffer.occupied_len() as u32)?;
                Ok(0)
            }
            _ => Err(KError::NotATty),
        }
    }
}

impl Pollable for Pipe {
    /// Polls for available I/O events.
    /// Read end reports buffered data and writer hangup.
    /// Write end reports buffer space and reader error.
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let state = self.shared.state.lock();
        if self.read_side {
            events.set(IoEvents::IN, state.buffer.occupied_len() > 0);
            events.set(IoEvents::HUP, state.writers == 0);
        } else {
            events.set(IoEvents::OUT, state.buffer.vacant_len() > 0);
            events.set(IoEvents::ERR, state.readers == 0);
        }
        events
    }

    /// Registers the pipe for polling with the given context and events.
    fn register(&self, context: &mut Context<'_>, _events: IoEvents) {
        if self.read_side {
            self.shared.poll_rx.register(context.waker());
        } else {
            self.shared.poll_tx.register(context.waker());
        }
    }
}

#[cfg(unittest)]
mod pipe_tests {
    use alloc::sync::Arc;

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
    fn test_pipe_poll_hup_after_writer_dropped() {
        use kpoll::Pollable;
        let (read_end, write_end) = Pipe::new();
        drop(write_end);
        let events = read_end.poll();
        assert!(!events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::HUP));
    }

    #[def_test]
    fn test_pipe_poll_err_after_reader_dropped() {
        use kpoll::Pollable;
        let (read_end, write_end) = Pipe::new();
        drop(read_end);
        let events = write_end.poll();
        assert!(events.contains(IoEvents::OUT));
        assert!(events.contains(IoEvents::ERR));
    }

    #[def_test]
    fn test_pipe_arc_dup_writer_keeps_read_end_open() {
        use kpoll::Pollable;
        let (read_end, write_end) = Pipe::new();
        let writer = Arc::new(write_end);
        let writer_dup = writer.clone();

        drop(writer);
        assert!(!read_end.poll().contains(IoEvents::HUP));

        drop(writer_dup);
        assert!(read_end.poll().contains(IoEvents::HUP));
    }

    #[def_test]
    fn test_pipe_writer_close_with_buffered_data_reports_in_and_hup() {
        use kpoll::Pollable;
        let (read_end, write_end) = Pipe::new();
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
