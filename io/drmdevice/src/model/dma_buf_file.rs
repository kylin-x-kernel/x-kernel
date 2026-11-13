// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The descriptor a dma-buf is shared through (`dma-buf.c`'s fops).
//!
//! A dma-buf fd is an anonymous-inode file whose private data is the
//! exported [`DmaBuf`]. It carries:
//!
//! * `mmap`: the backing pages, so a client can fill a buffer it imported or
//!   inspect one it exported;
//! * `lseek(SEEK_END)`: the buffer size;
//! * the `DMA_BUF_IOCTL_*` commands, which forward to [`super::dma_buf`];
//! * `poll`: readable once the exclusive fence completed, the readiness a
//!   client waits on before reading a buffer someone else fills.
//!
//! The file keeps the buffer alive; the buffer keeps the exporter's pages
//! alive. Neither refers back to the exporting card, so a card may be removed
//! while its buffers are still imported.

use alloc::sync::Arc;

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kpoll::{IoEvents, PollContext, PollRegisterError};
use kvfs::{FMode, FileOperations, MmapMapper, OpenFlags, VfsError, VfsFile, VfsResult};

use super::dma_buf::{DmaBuf, usage_fences};
use crate::{
    model::sync_ioctl::dma_buf_ioctl,
    prelude::*,
    sync::{dma_fence::FenceCallback, fence_list::is_signaled, wakeable::Wakeable},
};

/// The state of one dma-buf descriptor.
///
/// Readiness follows `dma_buf_poll`: readable when the exclusive work is done,
/// writable when every user (readers and writers) is done. Each direction has
/// its own callback slot, and a slot stays armed until it fires, so a poll does
/// not accumulate callbacks.
pub(crate) struct DmaBufFile {
    /// The exported buffer.
    pub(crate) buf: Arc<DmaBuf>,
    /// Readiness source for `EPOLLIN`.
    read: Arc<Wakeable>,
    /// Readiness source for `EPOLLOUT`.
    write: Arc<Wakeable>,
}

impl DmaBufFile {
    /// Wraps `buf` in the state of a dma-buf descriptor.
    pub(crate) fn new(buf: Arc<DmaBuf>) -> Arc<Self> {
        Arc::new(Self {
            buf,
            read: Arc::new(Wakeable::new()),
            write: Arc::new(Wakeable::new()),
        })
    }

    /// Readiness as `poll()` reports it.
    pub(crate) fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, is_signaled(&usage_fences(&self.buf, false)));
        events.set(IoEvents::OUT, is_signaled(&usage_fences(&self.buf, true)));
        events
    }

    /// Arms the callbacks for the requested directions.
    ///
    /// This is `dma_buf_poll`'s callback installation: the arm flag keeps one
    /// callback per direction, and a direction whose fences are already done
    /// wakes its waiters at once instead of arming.
    pub(crate) fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            self.read.register(context, IoEvents::IN)?;
            self.arm(false);
        }
        if events.contains(IoEvents::OUT) {
            self.write.register(context, IoEvents::OUT)?;
            self.arm(true);
        }
        Ok(())
    }

    /// Arms one direction's callback (`dma_buf_poll_add_cb`).
    fn arm(&self, write: bool) {
        let source = if write { &self.write } else { &self.read };
        if !source.arm() {
            return;
        }
        let fences = usage_fences(&self.buf, write);
        // `dma_buf_poll_add_cb` arms the first fence that accepts a callback;
        // that fence's completion wakes the queue and clears the arm bit.
        let pending = fences.fences().iter().find(|fence| !fence.is_signaled());
        match pending {
            Some(fence) => fence.add_callback(FenceCallback::dma_buf_wakeable(
                &Arc::downgrade(source),
                write,
            )),
            None => source.wake_direction(write),
        }
    }
}

/// Operations of one dma-buf descriptor.
struct DmaBufFileOps;

impl FileOperations for DmaBufFileOps {
    fn llseek(&self, file: &VfsFile, offset: i64, whence: i32) -> VfsResult<u64> {
        let state = state_of(file).map_err(crate::model::error::DrmError::to_vfs)?;
        // `dma_buf_llseek` supports discovering the end of the buffer and also
        // accepts `SEEK_SET` so the idiomatic SEEK_END(0), SEEK_CUR(0) pattern
        // works; any other whence, or a non-zero offset, is `EINVAL`.
        let base = match whence {
            SEEK_END => state.buf.size,
            SEEK_SET => 0,
            _ => return Err(VfsError::InvalidInput),
        };
        if offset != 0 {
            return Err(VfsError::InvalidInput);
        }
        Ok(base)
    }

    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        dma_buf_ioctl(file, cmd, arg).map_err(crate::model::error::DrmError::to_vfs)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let state = state_of(file).map_err(crate::model::error::DrmError::to_vfs)?;
        // `dma_buf_mmap_internal`: the page offset is a byte position inside
        // the buffer, not a lookup key, so a client may map a range that does
        // not start at zero.
        let offset = mapper.offset();
        if !offset.is_multiple_of(PAGE_SIZE_4K) {
            return Err(VfsError::InvalidInput);
        }
        let length = state.buf.backing.length;
        if offset >= length {
            return Err(VfsError::InvalidInput);
        }
        let start = usize::try_from(state.buf.backing.start)
            .map_err(|_| VfsError::InvalidInput)?
            .checked_add(offset)
            .ok_or(VfsError::InvalidInput)?;
        // The window is whatever the exporter reported, so an imported export
        // maps the same memory its own importers see. The mapping length is
        // the mapper's (`vma_pages`); the VFS has already refused a mapping
        // longer than the file reports.
        mapper.map_physical(PhysAddrRange::from_start_size(
            memaddr::PhysAddr::from(start),
            length - offset,
        ))
    }

    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        state_of(file).ok().map(|state| state.poll())
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(state) = state_of(file) {
            state.register(context, events)?;
        }
        Ok(())
    }
}

/// The dma-buf state of a file, if it is a dma-buf descriptor.
pub(crate) fn state_of(file: &VfsFile) -> crate::model::error::DrmResult<Arc<DmaBufFile>> {
    file.private_data_get::<DmaBufFile>()
        .ok_or(crate::model::error::DrmError::InvalidInput)
}

/// Creates the descriptor an exported buffer is shared through.
///
/// The caller installs the descriptor into whichever task should own it. The
/// name is what `/proc/self/fd` prints for the descriptor.
pub(crate) fn new_dma_buf_file(buf: &Arc<DmaBuf>, cred: &Arc<Cred>) -> VfsResult<Arc<VfsFile>> {
    let state = DmaBufFile::new(buf.clone());
    let file = AnonInodeFs::global().get_file(
        buf.name(),
        Arc::new(DmaBufFileOps),
        state,
        // `dma_buf_getfile` hands out a regular file, so `lseek` works: Mesa
        // sizes an imported buffer with `lseek(SEEK_END)` before mapping it,
        // and `VfsFile::llseek` refuses the syscall with ESPIPE when the open
        // mode lacks the flag, before this file's `llseek` op is ever reached.
        FMode::READ | FMode::WRITE | FMode::LSEEK,
        OpenFlags::READ_WRITE,
        cred.clone(),
    )?;
    // `dma_buf_getfile` sets `inode->i_size` when the descriptor is created, so
    // `stat` and `SEEK_END` agree and a client can size its mapping from either.
    file.inode().set_size(buf.size);
    // Publish the export on the descriptor's dma-buf slot: every importer —
    // this card's `PRIME_FD_TO_HANDLE`, a second display engine, a media
    // device — finds the object the same way, as Linux does through `f_op`.
    file.set_slot(
        kvfs::DMA_BUF_SLOT,
        Arc::new(display::DmaBufSlot(buf.clone())),
    );
    Ok(file)
}

/// The dma-buf object behind a descriptor, whoever exported it.
///
/// This is the importer's entry point (`dma_buf_get`): asking the descriptor
/// is what lets an export from another exporter be imported at all, and a
/// descriptor without an object is reported as "not a dma-buf" rather than as
/// a malformed request.
pub(crate) fn exported_object(file: &VfsFile) -> Option<Arc<dyn display::DmaBufObject>> {
    display::find(file)
}

/// `SEEK_SET` / `SEEK_END`, the two `whence` values a dma-buf answers.
const SEEK_SET: i32 = 0;
const SEEK_END: i32 = 2;
