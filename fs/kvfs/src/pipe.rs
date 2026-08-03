// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Anonymous pipe and pathname FIFO implementation.

use alloc::{collections::VecDeque, sync::Arc};

use iov_iter::{IovIterDest, IovIterSource};
use kcred::Cred;
use kerrno::{KError, KResult, LinuxError};
use klazy::Lazy;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksignal::{Signo, send_sig_current};
use ktask::future::{block_on, poll_io};
use linux_raw_sys::ioctl::FIONREAD;
use memaddr::PAGE_SIZE_4K;
use osvm::VirtMutPtr;

use crate::{
    AnonInodeFs, FMode, FileOperations, Kiocb, OpenFlags, VfsFile, VfsFileBuilder, VfsInode,
    VfsResult,
};

/// Initial pipe buffer capacity in bytes.
pub(crate) const RING_BUFFER_INIT_SIZE: usize = 65536;
/// Maximum pipe buffer capacity in bytes.
pub(crate) const PIPE_MAX_SIZE: usize = 1024 * 1024;
/// Maximum size of an atomic pipe write.
///
/// See `PIPE_BUF` in <https://man7.org/linux/man-pages/man7/pipe.7.html>.
pub(crate) const PIPE_BUF: usize = 4096;

struct PipeState {
    buffer: VecDeque<u8>,
    capacity: usize,
    files: usize,
    readers: usize,
    writers: usize,
    r_counter: u64,
    w_counter: u64,
}

impl PipeState {
    fn available(&self) -> usize {
        self.capacity.saturating_sub(self.buffer.len())
    }

    fn read_into(&mut self, dst: &mut [u8]) -> usize {
        let count = dst.len().min(self.buffer.len());
        if count == 0 {
            return 0;
        }

        let (head, tail) = self.buffer.as_slices();
        let head_len = count.min(head.len());
        dst[..head_len].copy_from_slice(&head[..head_len]);
        let tail_len = count - head_len;
        if tail_len != 0 {
            dst[head_len..count].copy_from_slice(&tail[..tail_len]);
        }
        drop(self.buffer.drain(..count));
        count
    }

    fn read_to_iter(&mut self, iter: &mut IovIterDest<'_>) -> KResult<usize> {
        // Keep destination copying and buffer consumption in one serialized
        // operation, matching Linux `anon_pipe_read()`. Copying through a
        // staging buffer after unlocking would consume data before a possible
        // destination fault and would let concurrent readers interleave.
        let count = iter.count().min(self.buffer.len());
        if count == 0 {
            return Ok(0);
        }

        let (head_len, head_read) = {
            let (head, _) = self.buffer.as_slices();
            let head_len = count.min(head.len());
            (head_len, iter.copy_to_iter(&head[..head_len])?)
        };
        if head_read < head_len {
            drop(self.buffer.drain(..head_read));
            return Ok(head_read);
        }

        let tail_len = count - head_len;
        let tail_read = if tail_len == 0 {
            0
        } else {
            let (_, tail) = self.buffer.as_slices();
            match iter.copy_to_iter(&tail[..tail_len]) {
                Ok(read) => read,
                // `anon_pipe_read()` returns bytes already copied before a
                // later user-memory fault instead of discarding that progress.
                Err(_) if head_read != 0 => {
                    drop(self.buffer.drain(..head_read));
                    return Ok(head_read);
                }
                Err(error) => return Err(error),
            }
        };
        let read = head_read + tail_read;
        drop(self.buffer.drain(..read));
        Ok(read)
    }

    fn write_from(&mut self, src: &[u8]) {
        debug_assert!(src.len() <= self.available());
        // `VecDeque` specializes extending from a slice into wrapped bulk copies.
        self.buffer.extend(src);
    }
}

/// State shared by every open file description in one pipe session.
pub struct PipeObject {
    state: crate::Mutex<PipeState>,
    rd_wait: PollSet,
    wr_wait: PollSet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PipeAccess {
    Read,
    Write,
    ReadWrite,
}

impl PipeAccess {
    fn from_mode(mode: FMode) -> KResult<Self> {
        match (mode.contains(FMode::READ), mode.contains(FMode::WRITE)) {
            (true, false) => Ok(Self::Read),
            (false, true) => Ok(Self::Write),
            (true, true) => Ok(Self::ReadWrite),
            (false, false) => Err(KError::InvalidInput),
        }
    }

    fn can_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    fn can_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }
}

impl PipeObject {
    fn alloc() -> Arc<Self> {
        Arc::new(Self {
            state: crate::Mutex::new(PipeState {
                buffer: VecDeque::with_capacity(RING_BUFFER_INIT_SIZE),
                capacity: RING_BUFFER_INIT_SIZE,
                files: 0,
                readers: 0,
                writers: 0,
                // Match Linux `pipe_inode_info`: an uninitialized `file::f_pipe`
                // snapshot is zero, so both connection counters start at one.
                r_counter: 1,
                w_counter: 1,
            }),
            rd_wait: PollSet::new(),
            wr_wait: PollSet::new(),
        })
    }

    fn new_anonymous() -> Arc<Self> {
        let pipe = Self::alloc();
        {
            let mut state = pipe.state.lock();
            state.files = 2;
            state.readers = 1;
            state.writers = 1;
        }
        pipe
    }

    pub(crate) fn new_fifo() -> Arc<Self> {
        Self::alloc()
    }

    /// Resolves the pipe session stored in an open file description.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    pub(crate) fn acquire_file(&self) -> KResult<()> {
        let mut state = self.state.lock();
        state.files = state.files.checked_add(1).ok_or(KError::ResourceBusy)?;
        Ok(())
    }

    pub(crate) fn release_file(&self) -> bool {
        let mut state = self.state.lock();
        assert!(state.files > 0, "pipe file count underflow");
        state.files -= 1;
        state.files == 0
    }

    /// Returns the current pipe buffer capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.state.lock().capacity
    }

    /// Resizes the pipe buffer.
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

    fn read_with(
        &self,
        nonblocking: bool,
        mut read: impl FnMut(&mut PipeState) -> KResult<usize>,
    ) -> KResult<usize> {
        block_on(poll_io(self, IoEvents::IN, nonblocking, || {
            let (read, has_writers) = {
                let mut state = self.state.lock();
                let read = read(&mut state)?;
                (read, state.writers > 0)
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

    fn read(&self, nonblocking: bool, buf: &mut [u8]) -> KResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.read_with(nonblocking, |state| Ok(state.read_into(buf)))
    }

    fn read_iter(&self, nonblocking: bool, iter: &mut IovIterDest<'_>) -> KResult<usize> {
        if iter.count() == 0 {
            return Ok(0);
        }
        self.read_with(nonblocking, |state| state.read_to_iter(iter))
    }

    fn write(&self, nonblocking: bool, request_is_atomic: bool, buf: &[u8]) -> KResult<usize> {
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
                    let available = state.available();
                    if request_is_atomic && written_total == 0 && available < size {
                        return Err(KError::WouldBlock);
                    }
                    let count = available.min(size - written_total);
                    state.write_from(&buf[written_total..written_total + count]);
                    Some(count)
                }
            };

            let Some(written) = written else {
                // Linux treats SIGPIPE delivery as best effort: write(2) must
                // still report EPIPE or the already completed byte count.
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
        .or_else(|error| {
            // `anon_pipe_write()` returns a partial count instead of an
            // interruption or wait-registration error after making progress.
            (written_total != 0).then_some(written_total).ok_or(error)
        })
    }

    fn wake(&self, wake_readers: bool, wake_writers: bool) {
        if wake_readers {
            self.rd_wait.wake();
        }
        if wake_writers {
            self.wr_wait.wake();
        }
    }

    fn open_fifo(&self, access: PipeAccess, nonblocking: bool) -> KResult<u64> {
        let (partner_counter, pipe_generation, wake_readers, wake_writers) = {
            let mut state = self.state.lock();
            if access == PipeAccess::Write && nonblocking && state.readers == 0 {
                return Err(KError::from(LinuxError::ENXIO));
            }

            let partner_counter = match access {
                PipeAccess::Read if !nonblocking && state.writers == 0 => Some(state.w_counter),
                PipeAccess::Write if state.readers == 0 => Some(state.r_counter),
                PipeAccess::Read | PipeAccess::Write | PipeAccess::ReadWrite => None,
            };
            let pipe_generation = if access == PipeAccess::Read && nonblocking && state.writers == 0
            {
                state.w_counter
            } else {
                0
            };
            let (wake_readers, wake_writers) = add_access(&mut state, access)?;
            (partner_counter, pipe_generation, wake_readers, wake_writers)
        };

        self.wake(wake_readers, wake_writers);
        if let Some(partner_counter) = partner_counter {
            let events = if access == PipeAccess::Read {
                IoEvents::IN
            } else {
                IoEvents::OUT
            };
            if let Err(error) = block_on(poll_io(self, events, false, || {
                if self.partner_counter(access) != partner_counter {
                    Ok(())
                } else {
                    Err(KError::WouldBlock)
                }
            })) {
                let (is_satisfied, wake_readers, wake_writers) = {
                    let mut state = self.state.lock();
                    if peer_counter(&state, access) != partner_counter {
                        (true, false, false)
                    } else {
                        let (wake_readers, wake_writers) = remove_access(&mut state, access);
                        (false, wake_readers, wake_writers)
                    }
                };
                if is_satisfied {
                    return Ok(pipe_generation);
                }
                self.wake(wake_readers, wake_writers);
                return Err(error);
            }
        }

        Ok(pipe_generation)
    }

    fn partner_counter(&self, access: PipeAccess) -> u64 {
        peer_counter(&self.state.lock(), access)
    }

    fn close(&self, mode: FMode) -> KResult<()> {
        let access = PipeAccess::from_mode(mode)?;
        let (wake_readers, wake_writers) = remove_access(&mut self.state.lock(), access);
        self.wake(wake_readers, wake_writers);
        Ok(())
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

fn peer_counter(state: &PipeState, access: PipeAccess) -> u64 {
    if access == PipeAccess::Read {
        state.w_counter
    } else {
        state.r_counter
    }
}

fn add_access(state: &mut PipeState, access: PipeAccess) -> KResult<(bool, bool)> {
    let readers = access
        .can_read()
        .then(|| state.readers.checked_add(1).ok_or(KError::ResourceBusy))
        .transpose()?;
    let writers = access
        .can_write()
        .then(|| state.writers.checked_add(1).ok_or(KError::ResourceBusy))
        .transpose()?;
    let wake_writers = readers.is_some() && state.readers == 0;
    let wake_readers = writers.is_some() && state.writers == 0;

    if let Some(readers) = readers {
        state.readers = readers;
        state.r_counter = state.r_counter.wrapping_add(1);
    }
    if let Some(writers) = writers {
        state.writers = writers;
        state.w_counter = state.w_counter.wrapping_add(1);
    }
    Ok((wake_readers, wake_writers))
}

fn remove_access(state: &mut PipeState, access: PipeAccess) -> (bool, bool) {
    let readers_became_zero = if access.can_read() {
        assert!(state.readers > 0, "pipe reader count underflow");
        state.readers -= 1;
        state.readers == 0
    } else {
        false
    };
    let writers_became_zero = if access.can_write() {
        assert!(state.writers > 0, "pipe writer count underflow");
        state.writers -= 1;
        state.writers == 0
    } else {
        false
    };
    let peers_remain = state.readers != 0 || state.writers != 0;
    let wake_both = peers_remain && (readers_became_zero || writers_became_zero);
    (wake_both, wake_both)
}

fn pipe_read(file: &VfsFile, buf: &mut [u8]) -> VfsResult<usize> {
    file.verify_mode(FMode::READ)?;
    PipeObject::from_file(file)?.read(file.is_nonblocking(), buf)
}

fn pipe_write(file: &VfsFile, buf: &[u8]) -> VfsResult<usize> {
    pipe_write_with_atomicity(file, buf.len() <= PIPE_BUF, buf)
}

fn pipe_write_with_atomicity(
    file: &VfsFile,
    request_is_atomic: bool,
    buf: &[u8],
) -> VfsResult<usize> {
    file.verify_mode(FMode::WRITE)?;
    PipeObject::from_file(file)?.write(file.is_nonblocking(), request_is_atomic, buf)
}

fn pipe_read_iter(iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
    let read = {
        let file = iocb.file();
        file.verify_mode(FMode::READ)?;
        PipeObject::from_file(file)?.read_iter(file.is_nonblocking(), iter)?
    };
    iocb.advance(read);
    Ok(read)
}

fn pipe_write_iter(iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
    let mut total = 0usize;
    let mut chunk = [0u8; PAGE_SIZE_4K];
    let request_is_atomic = iter.count() <= PIPE_BUF;
    while iter.count() != 0 {
        let want = chunk.len().min(iter.count());
        let copied = match iter.copy_from_iter(&mut chunk[..want]) {
            Ok(copied) => copied,
            // Preserve already committed pipe data when a later user-memory
            // fetch fails, like `anon_pipe_write()` does for a short copy.
            Err(_) if total != 0 => break,
            Err(error) => return Err(error),
        };
        if copied == 0 {
            break;
        }
        let written =
            match pipe_write_with_atomicity(iocb.file(), request_is_atomic, &chunk[..copied]) {
                Ok(written) => written,
                Err(error) => {
                    iter.revert(copied)?;
                    if total != 0 {
                        break;
                    }
                    return Err(error);
                }
            };
        if written == 0 {
            return Err(KError::WriteZero);
        }
        total += written;
        iocb.advance(written);
        if written < copied {
            iter.revert(copied - written)?;
            break;
        }
    }
    Ok(total)
}

fn pipe_ioctl(file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
    let pipe = PipeObject::from_file(file)?;
    match cmd {
        FIONREAD => {
            // `write_vm` validates typed user-pointer alignment before writing.
            (arg as *mut i32).write_vm(pipe.readable_len() as i32)?;
            Ok(0)
        }
        _ => Err(KError::NotATty),
    }
}

fn pipe_poll(file: &VfsFile) -> IoEvents {
    let Ok(pipe) = PipeObject::from_file(file) else {
        return IoEvents::ERR;
    };
    let state = pipe.state.lock();
    let mode = file.mode();
    let mut events = IoEvents::empty();
    if mode.contains(FMode::READ) {
        events.set(IoEvents::IN, !state.buffer.is_empty());
        events.set(
            IoEvents::HUP,
            // Zero is the Linux-compatible uninitialized `f_pipe` snapshot
            // used by anonymous pipes and blocking FIFO opens.
            state.writers == 0
                && (file.pipe_generation() == 0 || file.pipe_generation() != state.w_counter),
        );
    }
    if mode.contains(FMode::WRITE) {
        events.set(IoEvents::OUT, state.buffer.len() < state.capacity);
        events.set(IoEvents::ERR, state.readers == 0);
    }
    events
}

fn register_pipe_poll(
    file: &VfsFile,
    context: &mut PollContext<'_>,
    _events: IoEvents,
) -> Result<(), PollRegisterError> {
    if let Ok(pipe) = PipeObject::from_file(file) {
        let mode = file.mode();
        if mode.contains(FMode::READ) {
            context.register(&pipe.rd_wait)?;
        }
        if mode.contains(FMode::WRITE) {
            context.register(&pipe.wr_wait)?;
        }
    }
    Ok(())
}

struct PipeFileOperations;

impl FileOperations for PipeFileOperations {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        pipe_read(file, buf)
    }

    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        pipe_read_iter(iocb, iter)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        pipe_write(file, buf)
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        pipe_write_iter(iocb, iter)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        pipe_ioctl(file, cmd, arg)
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        pipe_poll(file)
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        register_pipe_poll(file, context, events)
    }

    fn release(&self, _inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        let pipe = PipeObject::from_file(file)?;
        pipe.close(file.mode())?;
        // Anonymous pipes have no inode slot to clear when the final file drops.
        let _ = pipe.release_file();
        Ok(())
    }
}

struct FifoFileOperations;

impl FileOperations for FifoFileOperations {
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let access = PipeAccess::from_mode(file.mode())?;
        let pipe = inode.acquire_fifo_pipe()?;
        let pipe_generation = match pipe.open_fifo(access, file.is_nonblocking()) {
            Ok(pipe_generation) => pipe_generation,
            Err(error) => {
                inode.release_fifo_pipe(&pipe);
                return Err(error);
            }
        };
        file.stream_open();
        file.set_pipe_generation(pipe_generation);
        file.set_private_data(pipe);
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        let read = pipe_read(file, buf)?;
        if read > 0 {
            file.file_accessed();
        }
        Ok(read)
    }

    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        let read = pipe_read_iter(iocb, iter)?;
        if read > 0 {
            iocb.file().file_accessed();
        }
        Ok(read)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        let written = pipe_write(file, buf)?;
        if written > 0 {
            file.file_update_time()?;
        }
        Ok(written)
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        let written = pipe_write_iter(iocb, iter)?;
        if written > 0 {
            iocb.file().file_update_time()?;
        }
        Ok(written)
    }

    fn ioctl(&self, file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        pipe_ioctl(file, cmd, arg)
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        pipe_poll(file)
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        register_pipe_poll(file, context, events)
    }

    fn release(&self, inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        let pipe = PipeObject::from_file(file)?;
        pipe.close(file.mode())?;
        inode.release_fifo_pipe(&pipe);
        Ok(())
    }
}

static PIPE_FILE_OPERATIONS: Lazy<Arc<PipeFileOperations>> =
    Lazy::new(|| Arc::new(PipeFileOperations));
static FIFO_FILE_OPERATIONS: Lazy<Arc<FifoFileOperations>> =
    Lazy::new(|| Arc::new(FifoFileOperations));

pub(crate) fn fifo_file_operations() -> Arc<dyn FileOperations> {
    FIFO_FILE_OPERATIONS.clone()
}

fn pipe_file_operations() -> Arc<dyn FileOperations> {
    PIPE_FILE_OPERATIONS.clone()
}

/// Creates the read and write open files for an anonymous pipe.
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
    let operations = pipe_file_operations();
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

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    fn pipe_files() -> (Arc<VfsFile>, Arc<VfsFile>, Arc<PipeObject>) {
        let (read_file, write_file) =
            create_pipe_files(0, 0, kcred::initial_cred()).expect("anonymous pipe files open");
        let pipe = PipeObject::from_file(&read_file).expect("pipe state is installed");
        (read_file, write_file, pipe)
    }

    #[def_test]
    fn anonymous_pipe_files_share_state() {
        let (read_file, write_file, pipe) = pipe_files();
        let write_pipe = PipeObject::from_file(&write_file).expect("pipe state is installed");

        assert!(read_file.mode().contains(FMode::READ));
        assert!(write_file.mode().contains(FMode::WRITE));
        assert!(Arc::ptr_eq(&pipe, &write_pipe));
        assert_eq!(pipe.capacity(), RING_BUFFER_INIT_SIZE);
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
        assert_eq!(write_file.write(&fill).unwrap(), fill.len());

        let payload = [1u8; 128];
        assert_eq!(write_file.write(&payload), Err(KError::WouldBlock));
        assert_eq!(pipe.readable_len(), fill.len());
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
}
