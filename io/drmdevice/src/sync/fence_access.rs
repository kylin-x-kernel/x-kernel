// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The user-facing fence syscall surface (`sync_file.c`'s ioctls).
//!
//! Userspace reaches fences through sync_file descriptors, and every DRM
//! interface that takes or returns a fence takes or returns such a descriptor:
//! `IN_FENCE_FD`, `OUT_FENCE_PTR` and `PRIME_HANDLE_TO_FD` (with
//! `DMA_BUF_SYNC_*`). This module owns the three operations that turn fds into
//! kernel fences and back:
//!
//! * [`fence_from_fd`] — resolve an fd of the calling task into a fence. The
//!   kernel keeps its own reference, so the caller may close the fd
//!   immediately, which is what an ioctl's `IN_FENCE_FD` does after reading it.
//! * [`fence_array`] — merge several fences into one. An array is returned only
//!   when there is more than one pending fence: wrapping a single fence would
//!   change the identity that `sync_file_info` reports.
//! * [`import`]/[`install`] — the fd-level entry points shared by the
//!   `SYNC_IOC_*` handlers and the DMA-BUF export path.
//!
//! Resolving an fd never dereferences it: the descriptor is looked up in the
//! calling process's table and must point at one of this driver's sync_files,
//! so a foreign fd is [`DrmError::InvalidInput`], exactly like Linux's
//! `sync_file_get_fence` on an fd that is not a sync_file.

use alloc::sync::Arc;

use kcred::Cred;
use kvfs::VfsFile;

use super::{
    dma_fence::{DmaFence, SyncContext, new_signaled_fence},
    fence_array::FenceArray,
    fence_list::FenceList,
    sync_file::{SyncFile, new_sync_file, state_of},
};
use crate::prelude::*;

/// The fence behind a sync_file descriptor of the calling task.
///
/// # Errors
///
/// [`DrmError::InvalidInput`] when `fd` is negative, closed, or not one of this
/// driver's sync_files.
pub(crate) fn fence_from_fd(fd: i32) -> DrmResult<Arc<DmaFence>> {
    if fd < 0 {
        return Err(DrmError::InvalidInput);
    }
    let file = kprocess::current_files()
        .get_file(fd)
        .map_err(|_| DrmError::InvalidInput)?;
    let state = sync_file_of(&file).ok_or(DrmError::InvalidInput)?;
    Ok(state.fence().clone())
}

/// Resolves an `IN_FENCE_FD` value.
///
/// `-1` means "no fence", as Linux's property documentation states; any other
/// negative value is rejected by [`fence_from_fd`]. Ownership of the descriptor
/// stays with the caller of the ioctl, but the kernel keeps its own reference
/// to the fence, so the fd may be closed at any time afterwards.
pub(crate) fn input_fence(fd: i64) -> DrmResult<Option<Arc<DmaFence>>> {
    if fd == -1 {
        return Ok(None);
    }
    if !(0..=i64::from(i32::MAX)).contains(&fd) {
        return Err(DrmError::InvalidInput);
    }
    // `fd` is inside `i32` by the check above.
    Ok(Some(fence_from_fd(fd as i32)?))
}

/// Resolves a descriptor that names a `sync_file`, reporting who owns it.
///
/// This is `sync_file_fdget`: the file is returned, not only its fence, so a
/// caller that needs the file (the merge keeps the caller's `user_name` and
/// the source's fence) can hold it for the operation.
///
/// # Errors
///
/// [`DrmError::NotFound`] for a closed or foreign descriptor, which is what
/// Linux's `sync_file_fdget` reports through `-ENOENT`; [`DrmError::InvalidInput`]
/// for a negative one.
pub(crate) fn sync_file_of_fd(fd: i32) -> DrmResult<Arc<VfsFile>> {
    if fd < 0 {
        return Err(DrmError::InvalidInput);
    }
    let file = kprocess::current_files()
        .get_file(fd)
        .map_err(|_| DrmError::NotFound)?;
    state_of(&file).ok_or(DrmError::NotFound)?;
    Ok(file)
}

/// Merges `fences` into one completion point.
///
/// An empty set is an already signaled fence and a single fence is itself, so
/// the identity of a plain dependency survives wrapping; every other set
/// becomes a fence array that completes when every member did. A `sync_file`
/// contributes its own fence as one member (that is what `sync_file_get_fence`
/// returns), so merging two sync_files completes when both files did.
pub(crate) fn fence_array(fences: &FenceList) -> Arc<DmaFence> {
    match fences.fences() {
        [only] => only.clone(),
        [] => new_signaled_fence(),
        members => {
            FenceArray::from_list(members, SyncContext::new(MERGE_CONTEXT), next_merge_seqno())
        }
    }
}

/// Context of merged fences created by [`fence_array`].
const MERGE_CONTEXT: u64 = 3;

static MERGE_SEQNO: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn next_merge_seqno() -> u64 {
    MERGE_SEQNO.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/// Flattens `fence` into the records `sync_file_info` reports.
///
/// A fence array reports its members; everything else reports itself.
pub(crate) fn flatten(fence: &Arc<DmaFence>) -> Vec<Arc<DmaFence>> {
    let mut out = Vec::new();
    flatten_into(fence, &mut out);
    if out.is_empty() {
        out.push(fence.clone());
    }
    out
}

fn flatten_into(fence: &Arc<DmaFence>, out: &mut Vec<Arc<DmaFence>>) {
    match fence.array() {
        Some(array) => {
            for member in array.members() {
                flatten_into(member, out);
            }
        }
        None => out.push(fence.clone()),
    }
}

/// Creates a sync_file descriptor for `fence` in the calling task.
///
/// The descriptor is close-on-exec, as Linux creates every fd the kernel hands
/// out for a fence.
pub(crate) fn install(fence: &Arc<DmaFence>, cred: &Arc<Cred>) -> DrmResult<i32> {
    install_named(fence, None, cred)
}

/// Creates a sync_file descriptor for `fence` with an explicit name.
///
/// The descriptor is close-on-exec, as Linux creates every fd the kernel hands
/// out for a fence.
pub(crate) fn install_named(
    fence: &Arc<DmaFence>,
    name: Option<alloc::string::String>,
    cred: &Arc<Cred>,
) -> DrmResult<i32> {
    let file = new_sync_file(fence, name, cred).map_err(|_| DrmError::NoMemory)?;
    kprocess::current_files()
        .add_file(file, true)
        .map_err(|_| DrmError::NoMemory)
}

/// The sync_file state behind `file`.
fn sync_file_of(file: &VfsFile) -> Option<Arc<SyncFile>> {
    state_of(file)
}

/// Creates the fence a merge returns when both sources carry nothing.
///
/// This is `dma_fence_allocate_private_stub`: a fence that is already done,
/// whose timestamp defaults to now when the caller has no explicit stamp.
pub(crate) fn private_stub() -> Arc<DmaFence> {
    new_signaled_fence()
}

/// Merges many fences into one, as `dma_fence_unwrap_merge` does.
///
/// The members are the unwrapped leaves of every source, in order: a fence
/// array contributes its members and everything else contributes itself.
/// Duplicates are dropped, as the unwrap merge compares contexts and sequence
/// numbers. An empty merge is [`private_stub`] and a single member is that
/// member (`dma_fence_unwrap_array`).
pub(crate) fn unwrap_merge(sources: &[Arc<DmaFence>]) -> Arc<DmaFence> {
    let mut members: Vec<Arc<DmaFence>> = Vec::new();
    for source in sources {
        for leaf in flatten(source) {
            let duplicate = members
                .iter()
                .any(|known| known.context() == leaf.context() && known.seqno() == leaf.seqno());
            if !duplicate {
                members.push(leaf);
            }
        }
    }
    match members.as_slice() {
        [] => private_stub(),
        [only] => only.clone(),
        members => fence_array(&FenceList::new(members.to_vec())),
    }
}
