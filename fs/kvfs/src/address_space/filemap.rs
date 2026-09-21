// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cached folio access and generic buffered I/O.

use alloc::sync::Arc;

use iov_iter::{IovIterDest, IovIterSource};
use kerrno::KResult;
use memaddr::PAGE_SIZE_4K;
use pagecache::{Folio, PageIndex};

use super::{AddressSpace, PageMkwriteRequest, WriteBeginRequest, WriteEndRequest};
use crate::{Kiocb, VfsError, VfsResult};

impl AddressSpace {
    /// Runs `f` with the cached folio at `index`, if present.
    pub fn with_folio<R>(&self, index: PageIndex, f: impl FnOnce(Option<&mut Folio>) -> R) -> R {
        self.i_pages.with_folio(index, f)
    }

    /// Runs `f` with the folio at `index`, materializing it when absent.
    pub fn with_folio_or_create<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(&mut Folio) -> KResult<R>,
    ) -> KResult<R> {
        self.i_pages.with_folio_or_create(
            index,
            |index| {
                let mut folio = Folio::new_zeroed()?;
                self.read_folio(&mut folio, index)?;
                Ok(folio)
            },
            f,
        )
    }

    /// Prepares and dirties a cached folio for a shared write fault.
    ///
    /// The address-space invalidate lock excludes truncate and cache
    /// invalidation, while the folio lock covers the EOF recheck, filesystem
    /// preparation, dirtying, and the caller's PTE update.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::BadAddress`] if truncate moved the folio beyond EOF,
    /// or propagates filesystem preparation, dirtying, and PTE-update errors.
    pub fn page_mkwrite<R>(
        &self,
        index: PageIndex,
        f: impl FnOnce(&mut Folio) -> VfsResult<R>,
    ) -> VfsResult<R> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let pos = index
            .checked_mul(PAGE_SIZE_4K as u64)
            .ok_or(VfsError::InvalidInput)?;
        let _invalidate_guard = self.invalidate_lock.read();
        self.with_folio(index, |folio| {
            let folio = folio.ok_or(VfsError::InvalidData)?;
            let file_len = inode.size();
            if pos >= file_len {
                return Err(VfsError::BadAddress);
            }
            let len = usize::try_from((file_len - pos).min(PAGE_SIZE_4K as u64))
                .map_err(|_| VfsError::InvalidInput)?;
            self.ops
                .page_mkwrite(self, PageMkwriteRequest::new(pos, len))?;
            self.dirty_folio(folio)?;
            f(folio)
        })
    }

    fn read_folio(&self, folio: &mut Folio, index: PageIndex) -> VfsResult<usize> {
        self.ops.read_folio(self, folio, index)
    }

    fn dirty_folio(&self, folio: &mut Folio) -> VfsResult<bool> {
        self.ops.dirty_folio(self, folio)
    }

    /// Performs a buffered read from this address space into an iterator.
    pub(crate) fn read_iter(
        self: &Arc<Self>,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterDest<'_>,
    ) -> VfsResult<usize> {
        let file_len = self
            .inode()
            .map(|inode| inode.size())
            .ok_or(VfsError::InvalidInput)?;
        let pos = iocb.ki_pos();
        if iter.count() == 0 || pos >= file_len {
            return Ok(0);
        }

        let read = self.read_from_page_cache(iter, pos, file_len)?;
        iocb.advance(read);
        Ok(read)
    }

    fn read_from_page_cache(
        self: &Arc<Self>,
        iter: &mut IovIterDest<'_>,
        mut offset: u64,
        file_len: u64,
    ) -> VfsResult<usize> {
        let mut total = 0usize;
        while iter.count() != 0 && offset < file_len {
            self.prepare_read_window(offset, iter.count(), file_len)?;
            let copied = self.copy_folio_to_iter(iter, offset, file_len)?;
            if copied == 0 {
                break;
            }
            offset += copied as u64;
            total += copied;
            if copied < Self::copy_len(offset - copied as u64, file_len)? {
                break;
            }
        }
        Ok(total)
    }

    fn copy_folio_to_iter(
        &self,
        iter: &mut IovIterDest<'_>,
        offset: u64,
        file_len: u64,
    ) -> VfsResult<usize> {
        let index = offset / PAGE_SIZE_4K as u64;
        let page_off = (offset % PAGE_SIZE_4K as u64) as usize;
        let step = Self::copy_len(offset, file_len)?;
        self.with_folio_or_create(index, |folio| {
            iter.copy_to_iter(&folio.data()[page_off..page_off + step])
        })
    }

    fn copy_len(offset: u64, file_len: u64) -> VfsResult<usize> {
        let page_off = (offset % PAGE_SIZE_4K as u64) as usize;
        let end = offset
            .saturating_add((PAGE_SIZE_4K - page_off) as u64)
            .min(file_len);
        usize::try_from(end - offset).map_err(|_| VfsError::InvalidInput)
    }

    /// Performs a buffered write from an iterator into this address space.
    pub(crate) fn write_iter(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterSource<'_>,
    ) -> VfsResult<usize> {
        self.write_iter_with_checks(iocb, iter, |_, _| Ok(()))
    }

    pub(crate) fn write_iter_with_checks(
        &self,
        iocb: &mut Kiocb<'_>,
        iter: &mut IovIterSource<'_>,
        checks: impl FnOnce(u64, &mut usize) -> VfsResult<()>,
    ) -> VfsResult<usize> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let _data_guard = inode.lock_data();
        let mut count = iter.count();
        self.generic_write_check_limits(iocb, &mut count)?;
        if count != 0 {
            checks(iocb.ki_pos(), &mut count)?;
        }
        iter.truncate(count);
        let count = iter.count();
        if count == 0 {
            return Ok(0);
        }

        let written = self.perform_write(iocb.ki_pos(), iter, count)?;
        iocb.advance(written);
        Ok(written)
    }

    fn generic_write_check_limits(&self, iocb: &Kiocb<'_>, count: &mut usize) -> VfsResult<()> {
        if *count == 0 {
            return Ok(());
        }

        let max_file_size = iocb.file().max_file_size();
        let pos = iocb.ki_pos();
        if pos >= max_file_size {
            return Err(VfsError::FileTooLarge);
        }

        let count_u64 = u64::try_from(*count).map_err(|_| VfsError::InvalidInput)?;
        let count_u64 = count_u64.min(max_file_size - pos);
        *count = usize::try_from(count_u64).map_err(|_| VfsError::InvalidInput)?;
        Ok(())
    }

    fn perform_write(
        &self,
        offset: u64,
        iter: &mut IovIterSource<'_>,
        len: usize,
    ) -> VfsResult<usize> {
        let mut written = 0usize;
        let mut visible_len = self.visible_len()?;
        while written < len {
            let pos = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            let index = pos / memaddr::PAGE_SIZE_4K as u64;
            let page_off = (pos % memaddr::PAGE_SIZE_4K as u64) as usize;
            let step = (len - written).min(memaddr::PAGE_SIZE_4K - page_off);
            let prepared_end = pos.checked_add(step as u64).ok_or(VfsError::InvalidInput)?;
            self.ops
                .write_begin(self, WriteBeginRequest::new(pos, step))?;
            if prepared_end > visible_len {
                self.i_pages.resize_cached_folios(visible_len, prepared_end);
                // Another iteration is reached only after `write_end` accepts
                // this full range and publishes the same EOF under data_lock.
                visible_len = prepared_end;
            }
            let copied = match self.with_folio_or_create(index, |folio| {
                let data = folio.data();
                let copied = iter.copy_from_iter(&mut data[page_off..page_off + step])?;
                if copied != 0 {
                    self.dirty_folio(folio)?;
                }
                Ok(copied)
            }) {
                Ok(copied) => copied,
                Err(error) => {
                    let _ = self.ops.write_end(self, WriteEndRequest::new(pos, step, 0));
                    return Err(error);
                }
            };
            let accepted = self
                .ops
                .write_end(self, WriteEndRequest::new(pos, step, copied))?;
            if accepted > copied {
                return Err(VfsError::InvalidInput);
            }
            if accepted < copied {
                iter.revert(copied - accepted)?;
            }
            if accepted == 0 {
                break;
            }
            written += accepted;
            if accepted < step {
                break;
            }
        }

        Ok(written)
    }
}
