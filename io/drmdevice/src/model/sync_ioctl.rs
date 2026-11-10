// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The `sync_file` and dma-buf descriptor commands (`sync_file.c`,
//! `dma-buf.c`).
//!
//! These commands are issued on a descriptor, not on a DRM card, so they never
//! reach the DRM ioctl table: the descriptor's own file operations dispatch
//! them here. Both families take the same arguments as their Linux
//! counterparts and report the same errors.
//!
//! | command | meaning |
//! |---|---|
//! | `SYNC_IOC_MERGE` | merge two sync_files into a new one |
//! | `SYNC_IOC_FILE_INFO` | report the fences of a sync_file |
//! | `DMA_BUF_IOCTL_SYNC` | wait for (or publish) CPU access |
//! | `DMA_BUF_IOCTL_INFO` | buffer size and flags |
//! | `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` | export the buffer's fence |
//! | `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` | record a fence on the buffer |

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{
    abi::{
        DMA_BUF_BASE, DMA_BUF_BASE_SEQ_1, DMA_BUF_IOCTL_EXPORT_SYNC_FILE_CMD,
        DMA_BUF_IOCTL_IMPORT_SYNC_FILE_CMD, DMA_BUF_IOCTL_SYNC_CMD, DmaBufExportSyncFile,
        DmaBufImportSyncFile, DmaBufSync, DrmSyncFenceInfo, DrmSyncFileInfo, DrmSyncMergeData,
        DrmSyncSetDeadline, iow,
    },
    model::{
        dma_buf,
        error::{DrmError, DrmResult},
    },
    prelude::*,
    sync::{
        fence_access::{sync_file_of_fd, unwrap_merge},
        sync_file::{
            SYNC_IOC_FILE_INFO, SYNC_IOC_MERGE, SYNC_IOC_SET_DEADLINE, SyncFenceInfo, SyncFile,
            c_string, new_sync_file, reported_name_bytes, sync_fence_info, sync_file_status,
        },
    },
};

/// Handles one command issued on a sync_file descriptor `file`.
///
/// Linux's `sync_file_ioctl` dispatches MERGE, FILE_INFO and SET_DEADLINE and
/// reports every other command as `ENOTTY`, which this crate reports as
/// [`DrmError::OpNotSupported`].
pub(crate) fn sync_file_ioctl(file: &VfsFile, cmd: u32, arg: usize) -> DrmResult<usize> {
    let state = crate::sync::sync_file::state_of(file).ok_or(DrmError::InvalidInput)?;
    match cmd {
        SYNC_IOC_MERGE => merge(&state, arg),
        SYNC_IOC_FILE_INFO => file_info(&state, arg),
        SYNC_IOC_SET_DEADLINE => set_deadline(&state, arg),
        _ => Err(DrmError::OpNotSupported),
    }
}

/// `SYNC_IOC_MERGE`: merge this sync_file with `fd2` into a new one.
///
/// The new file carries the unwrapped merge of both files' fences, so it is
/// readable exactly when every fence of both is done; the caller's name (from
/// `data.name`, clamped at the last byte) becomes the new file's user name.
fn merge(first: &SyncFile, arg: usize) -> DrmResult<usize> {
    let user = UserPtr::<DrmSyncMergeData>::from(arg);
    let mut request: DrmSyncMergeData = user.read_vm().map_err(|_| DrmError::BadAddress)?;
    if request.flags != 0 || request.abi_pad != 0 {
        return Err(DrmError::InvalidInput);
    }
    // `sync_file_fdget`: a closed or foreign descriptor is `ENOENT`.
    let second_file = sync_file_of_fd(request.fd2)?;
    let second = crate::sync::sync_file::state_of(&second_file).ok_or(DrmError::NotFound)?;
    let merged = unwrap_merge(&[first.fence().clone(), second.fence().clone()]);
    let name = c_string(&request.name);
    let cred = kprocess::current_cred();
    let file = new_sync_file(&merged, Some(name), &cred).map_err(|_| DrmError::NoMemory)?;
    request.fence = kprocess::current_files()
        .add_file(file, true)
        .map_err(|_| DrmError::NoMemory)?;
    user.write_vm(request).map_err(|_| DrmError::BadAddress)?;
    Ok(0)
}

/// `SYNC_IOC_FILE_INFO`: report the fences of a sync_file.
///
/// This is `sync_file_ioctl_fence_info`:
///
/// * `flags` and `pad` must be zero;
/// * one count of fences over the unwrapped fence;
/// * `num_fences == 0` is a pure count query: the full count and the whole
///   file's status are written, nothing else;
/// * otherwise `num_fences` is the caller's capacity and must be at least the
///   count (`EINVAL`), the records are copied, and the whole structure is
///   copied back with the caller's `num_fences` field restored.
fn file_info(file: &SyncFile, arg: usize) -> DrmResult<usize> {
    let user = UserPtr::<DrmSyncFileInfo>::from(arg);
    let mut request: DrmSyncFileInfo = user.read_vm().map_err(|_| DrmError::BadAddress)?;
    if request.flags != 0 || request.abi_pad != 0 {
        return Err(DrmError::InvalidInput);
    }
    let records = sync_fence_info(file);
    let count = records.len() as u32;
    request.name = reported_name_bytes(file);
    if request.num_fences == 0 {
        request.status = sync_file_status(file);
    } else {
        if request.num_fences < count {
            return Err(DrmError::InvalidInput);
        }
        // The whole fence set finished only when every member finished: the
        // fold keeps the first non-successful status.
        request.status = 1;
        for record in &records {
            if request.status > 0 {
                request.status = record.status;
            }
        }
        // Every record the file carries is written; a null buffer makes that
        // copy fault (`copy_to_user` answers `EFAULT`), not a silent no-op.
        write_records(request.sync_fence_info, &records)?;
    }
    // Except for `num_fences`, the caller's request is replaced wholesale by
    // what the kernel reports; `num_fences` keeps the caller's value.
    request.num_fences = count;
    request.abi_pad = 0;
    user.write_vm(request).map_err(|_| DrmError::BadAddress)?;
    Ok(0)
}

/// Copies whole `sync_fence_info` records to a caller buffer.
///
/// The record layout is fixed by the ABI, so a copy of `bytes` per record is
/// exact; the bytes are written through the caller's pointer without
/// reinterpreting it, which keeps the caller's alignment irrelevant.
fn write_records(destination: UserPtr<u8>, records: &[SyncFenceInfo]) -> DrmResult<()> {
    if records.is_empty() {
        return Ok(());
    }
    if destination.is_null() {
        return Err(DrmError::BadAddress);
    }
    let bytes: &[u8] = bytemuck::cast_slice(records);
    destination
        .write_vm_slice(bytes)
        .map_err(|_| DrmError::BadAddress)
}

/// `SYNC_IOC_SET_DEADLINE`: publish a deadline on the file's fence.
///
/// The timebase is CLOCK_MONOTONIC, the same as vblank: a waiter on the fence
/// is expected to finish by that instant. The hint is forwarded to a chain
/// node's contained work (`dma_fence_set_deadline` walks a chain).
fn set_deadline(file: &SyncFile, arg: usize) -> DrmResult<usize> {
    let request: DrmSyncSetDeadline = read_arg(arg)?;
    if request.abi_pad != 0 {
        return Err(DrmError::InvalidInput);
    }
    file.fence()
        .set_deadline(MonotonicInstant::from_span_since_origin(
            TimeSpan::from_nanos(request.deadline_ns),
        ));
    Ok(0)
}

/// Handles one command issued on a dma-buf descriptor `file`.
pub(crate) fn dma_buf_ioctl(file: &VfsFile, cmd: u32, arg: usize) -> DrmResult<usize> {
    let state = super::dma_buf_file::state_of(file)?;
    match cmd {
        sync if sync == iow::<DmaBufSync>(DMA_BUF_BASE, DMA_BUF_IOCTL_SYNC_CMD) => {
            let request: DmaBufSync = read_arg(arg)?;
            dma_buf::sync_fd(&state.buf, &request)?;
            Ok(0)
        }
        // Linux spells the name command once for each user-pointer width; the
        // kernel reads a string from the pointer, so both sizes are accepted
        // and only the copied cap matters.
        set_name
            if set_name == iow::<u64>(DMA_BUF_BASE, DMA_BUF_BASE_SEQ_1)
                || set_name == iow::<u32>(DMA_BUF_BASE, DMA_BUF_BASE_SEQ_1) =>
        {
            dma_buf::set_name_fd(&state.buf, posix_types::UserConstPtr::from(arg))?;
            Ok(0)
        }
        export
            if export
                == iow::<DmaBufExportSyncFile>(
                    DMA_BUF_BASE,
                    DMA_BUF_IOCTL_EXPORT_SYNC_FILE_CMD,
                ) =>
        {
            let mut request: DmaBufExportSyncFile = read_arg(arg)?;
            dma_buf::export_sync_file(&state.buf, &mut request)?;
            write_arg(arg, &request)?;
            Ok(0)
        }
        import
            if import
                == iow::<DmaBufImportSyncFile>(
                    DMA_BUF_BASE,
                    DMA_BUF_IOCTL_IMPORT_SYNC_FILE_CMD,
                ) =>
        {
            let request: DmaBufImportSyncFile = read_arg(arg)?;
            dma_buf::import_sync_file(&state.buf, &request)?;
            Ok(0)
        }
        _ => Err(DrmError::OpNotSupported),
    }
}

fn read_arg<T: UserRead>(arg: usize) -> DrmResult<T> {
    UserPtr::<T>::from(arg)
        .read_vm()
        .map_err(|_| DrmError::BadAddress)
}

fn write_arg<T: UserWrite + Copy>(arg: usize, value: &T) -> DrmResult<()> {
    UserPtr::<T>::from(arg)
        .write_vm(*value)
        .map_err(|_| DrmError::BadAddress)
}

/// Records the record size the ABI fixed, so a layout change is a compile
/// error rather than a wrong copy length.
const _: [(); 80] = [(); core::mem::size_of::<DrmSyncFenceInfo>()];
const _: [(); 56] = [(); core::mem::size_of::<DrmSyncFileInfo>()];

/// Length of the `name` field of `struct sync_merge_data`.
const FILE_NAME_LEN: usize = 32;
const _: () = assert!(FILE_NAME_LEN == 32);
