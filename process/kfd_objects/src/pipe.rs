// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `pipe` object implementation.

use alloc::{collections::VecDeque, sync::Arc};

use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksignal::{Signo, send_sig_current};
use ksync::Mutex;
use ktask::future::{block_on, poll_io};
use kvfs::{AnonInodeFs, FMode, FileOperations, OpenFlags, VfsFile, VfsInode, VfsResult};
use linux_raw_sys::ioctl::FIONREAD;
use osvm::VirtMutPtr;

/// Initial pipe buffer capacity in bytes.
pub const RING_BUFFER_INIT_SIZE: usize = 65536;
/// Maximum pipe buffer capacity in bytes.
pub const PIPE_MAX_SIZE: usize = 1024 * 1024;
/// Maximum size of an atomic pipe write.
pub const PIPE_BUF: usize = 4096;

const PAGE_SIZE_4K: usize = 4096;

struct PipeState {
    buffer: VecDeque<u8>,
    capacity: usize,
    readers: usize,
    writers: usize,
}

/// Shared pipe state exposed through read and write VFS files in the fd table.
pub struct PipeObject {
    state: Mutex<PipeState>,
    rd_wait: PollSet,
    wr_wait: PollSet,
}

/// Pipe endpoint resolved from a file descriptor lookup.
pub enum PipeEndpoint {
    Read(Arc<PipeObject>),
    Write(Arc<PipeObject>),
}

impl PipeObject {
    fn new_anonymous() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PipeState {
                buffer: VecDeque::with_capacity(RING_BUFFER_INIT_SIZE),
                capacity: RING_BUFFER_INIT_SIZE,
                readers: 1,
                writers: 1,
            }),
            rd_wait: PollSet::new(),
            wr_wait: PollSet::new(),
        })
    }

    fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Returns the current pipe buffer capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.state.lock().capacity
    }

    /// Resize the pipe buffer.
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

        let should_wake_writers = {
            let mut state = self.state.lock();
            let old_size = state.capacity;
            if new_size == old_size {
                return Ok(());
            }
            if new_size < state.buffer.len() {
                return Err(KError::ResourceBusy);
            }

            let mut new_buffer = VecDeque::with_capacity(new_size);
            new_buffer.extend(state.buffer.iter().copied());
            state.buffer = new_buffer;
            state.capacity = new_size;
            new_size > old_size
        };

        if should_wake_writers {
            self.wr_wait.wake();
        }

        Ok(())
    }

    fn readable_len(&self) -> usize {
        self.state.lock().buffer.len()
    }

    fn read(&self, nonblocking: bool, buf: &mut [u8]) -> KResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        block_on(poll_io(self, IoEvents::IN, nonblocking, || {
            let (read, has_writers) = {
                let mut state = self.state.lock();
                let count = buf.len().min(state.buffer.len());
                for slot in &mut buf[..count] {
                    *slot = state
                        .buffer
                        .pop_front()
                        .expect("pipe readable length changed");
                }
                (count, state.writers > 0)
            };

            if read > 0 {
                self.wr_wait.wake();
                Ok(read)
            } else if !has_writers {
                Ok(0)
            } else {
                Err(KError::WouldBlock)
            }
        }))
    }

    fn write(&self, nonblocking: bool, buf: &[u8]) -> KResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let size = buf.len();
        let mut written_total = 0usize;
        block_on(poll_io(self, IoEvents::OUT, nonblocking, || {
            let written = {
                let mut state = self.state.lock();
                if state.readers == 0 {
                    None
                } else {
                    let available = state.capacity.saturating_sub(state.buffer.len());
                    if written_total == 0 && size <= PIPE_BUF && available < size {
                        return Err(KError::WouldBlock);
                    }
                    let count = available.min(size - written_total);
                    state
                        .buffer
                        .extend(buf[written_total..written_total + count].iter().copied());
                    Some(count)
                }
            };

            let Some(written) = written else {
                let _ = send_sig_current(Signo::SIGPIPE);
                if written_total > 0 {
                    return Ok(written_total);
                }
                return Err(KError::BrokenPipe);
            };

            if written > 0 {
                written_total += written;
                self.rd_wait.wake();
                if written_total == size || nonblocking {
                    return Ok(written_total);
                }
            }

            Err(KError::WouldBlock)
        }))
    }
}

impl Pollable for PipeObject {
    fn poll(&self) -> IoEvents {
        let state = self.state.lock();
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, !state.buffer.is_empty());
        events.set(IoEvents::OUT, state.buffer.len() < state.capacity);
        events.set(IoEvents::HUP, state.writers == 0);
        events.set(IoEvents::ERR, state.readers == 0);
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.rd_wait)?;
        }
        if events.contains(IoEvents::OUT) {
            context.register(&self.wr_wait)?;
        }
        Ok(())
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
            Self::Read(pipe) | Self::Write(pipe) => pipe.capacity(),
        }
    }

    /// Resize the shared pipe buffer behind either endpoint.
    pub fn resize(&self, new_size: usize) -> KResult<()> {
        match self {
            Self::Read(pipe) | Self::Write(pipe) => pipe.resize(new_size),
        }
    }
}

struct PipeFileOperations;

impl PipeFileOperations {
    fn poll_for(file: &VfsFile, pipe: &PipeObject) -> IoEvents {
        let mut events = pipe.poll();
        if !file.mode().contains(FMode::READ) {
            events.remove(IoEvents::IN);
            events.remove(IoEvents::HUP);
        }
        if !file.mode().contains(FMode::WRITE) {
            events.remove(IoEvents::OUT);
            events.remove(IoEvents::ERR);
        }
        events
    }
}

impl FileOperations for PipeFileOperations {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if !file.mode().contains(FMode::READ) {
            return Err(KError::BadFileDescriptor);
        }
        PipeObject::from_file(file)?.read(file.is_nonblocking(), buf)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        if !file.mode().contains(FMode::WRITE) {
            return Err(KError::BadFileDescriptor);
        }
        PipeObject::from_file(file)?.write(file.is_nonblocking(), buf)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        let pipe = PipeObject::from_file(file)?;
        match cmd {
            FIONREAD => {
                (arg as *mut u32).write_vm(pipe.readable_len() as u32)?;
                Ok(0)
            }
            _ => Err(KError::NotATty),
        }
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        PipeObject::from_file(file).map_or(IoEvents::ERR, |pipe| Self::poll_for(file, &pipe))
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(pipe) = PipeObject::from_file(file) {
            pipe.register(context, events)?;
        }
        Ok(())
    }

    fn release(&self, _inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        let pipe = PipeObject::from_file(file)?;
        if file.mode().contains(FMode::READ) {
            pipe_close_reader(&pipe);
        }
        if file.mode().contains(FMode::WRITE) {
            pipe_close_writer(&pipe);
        }
        Ok(())
    }
}

fn pipe_close_reader(pipe: &PipeObject) {
    let should_wake = {
        let mut state = pipe.state.lock();
        state.readers = state.readers.saturating_sub(1);
        state.readers == 0 && state.writers > 0
    };
    if should_wake {
        pipe.rd_wait.wake();
        pipe.wr_wait.wake();
    }
}

fn pipe_close_writer(pipe: &PipeObject) {
    let should_wake = {
        let mut state = pipe.state.lock();
        state.writers = state.writers.saturating_sub(1);
        state.writers == 0 && state.readers > 0
    };
    if should_wake {
        pipe.rd_wait.wake();
        pipe.wr_wait.wake();
    }
}

/// Create the read and write VFS files for an anonymous pipe.
///
/// Both file views capture the same `cred` as their open credential.
pub fn create_pipe_files(
    read_flags: u32,
    write_flags: u32,
    cred: Arc<Cred>,
) -> KResult<(Arc<VfsFile>, Arc<VfsFile>)> {
    let read_flags = OpenFlags::from_bits(read_flags).ok_or(KError::InvalidInput)?;
    let write_flags = OpenFlags::from_bits(write_flags).ok_or(KError::InvalidInput)?;
    let pipe = PipeObject::new_anonymous();
    let operations: Arc<dyn FileOperations> = Arc::new(PipeFileOperations);
    let write_file = AnonInodeFs::global().get_file(
        "[pipe]",
        operations.clone(),
        pipe.clone(),
        FMode::WRITE | FMode::STREAM,
        write_flags,
        cred,
    )?;

    let read_file = write_file.alloc_clone_with_private_data(
        FMode::READ | FMode::STREAM,
        read_flags,
        operations,
        pipe,
    )?;

    Ok((read_file, write_file))
}

/// Resolve the current process file descriptor to a concrete pipe endpoint.
pub fn current_pipe_endpoint(fd: i32) -> KResult<PipeEndpoint> {
    let file = kprocess::current_resources().get_file(fd)?;
    let pipe = PipeObject::from_file(&file)?;
    if file.mode().contains(FMode::READ) {
        return Ok(PipeEndpoint::Read(pipe));
    }
    if file.mode().contains(FMode::WRITE) {
        return Ok(PipeEndpoint::Write(pipe));
    }
    Err(KError::BadFileDescriptor)
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    fn pipe_files() -> (Arc<VfsFile>, Arc<VfsFile>, Arc<PipeObject>) {
        let (read_file, write_file) =
            create_pipe_files(0, 0, kcred::initial_cred()).expect("anonymous pipe files open");
        let pipe = PipeObject::from_file(&read_file).expect("pipe private data is installed");
        (read_file, write_file, pipe)
    }

    #[def_test]
    fn anonymous_pipe_files_share_state() {
        let (read_file, write_file, pipe) = pipe_files();
        let write_pipe =
            PipeObject::from_file(&write_file).expect("pipe private data is installed");

        assert!(read_file.mode().contains(FMode::READ));
        assert!(write_file.mode().contains(FMode::WRITE));
        assert!(Arc::ptr_eq(&pipe, &write_pipe));
        assert_eq!(pipe.capacity(), RING_BUFFER_INIT_SIZE);
    }

    #[def_test]
    fn anonymous_pipe_poll_after_endpoint_drop() {
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
        let mut write_pos = 0;
        assert_eq!(
            write_file.write_from(data, &mut write_pos).unwrap(),
            data.len()
        );
        drop(write_file);

        let events = read_file.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::HUP));

        let mut buf = [0u8; 5];
        let mut read_pos = 0;
        assert_eq!(
            read_file.read_from(&mut buf, &mut read_pos).unwrap(),
            data.len()
        );
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
            pipe.resize(PIPE_MAX_SIZE + 1),
            Err(KError::OperationNotPermitted)
        );
        assert_eq!(pipe.capacity(), RING_BUFFER_INIT_SIZE);
        assert_eq!(pipe.resize(usize::MAX), Err(KError::InvalidInput));
        assert_eq!(pipe.capacity(), RING_BUFFER_INIT_SIZE);
    }

    #[def_test]
    fn anonymous_pipe_nonblocking_pipe_buf_write_is_atomic() {
        let (_read_file, write_file, pipe) = pipe_files();
        pipe.resize(PAGE_SIZE_4K).unwrap();
        write_file.set_nonblocking(true);

        let fill = [0u8; PAGE_SIZE_4K - 64];
        let mut pos = 0;
        assert_eq!(write_file.write_from(&fill, &mut pos).unwrap(), fill.len());

        let payload = [1u8; 128];
        assert_eq!(
            write_file.write_from(&payload, &mut pos),
            Err(KError::WouldBlock)
        );
        assert_eq!(pipe.readable_len(), fill.len());
    }
}
