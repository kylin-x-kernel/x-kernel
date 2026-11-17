// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `SYNCOBJ_*`: the client-facing fence container ioctls (`drm_syncobj.c`).
//!
//! The object model lives in [`crate::model::syncobj`]; this module is its ABI
//! layer. Every command validates the flag word before it touches an object,
//! resolves and validates every named handle before the first side effect, and
//! copies every argument array completely before the first wait. A malformed
//! request therefore either does nothing or fails before it does anything.
//!
//! `WAIT` and `TIMELINE_WAIT` differ in what a handle addresses: the timeline
//! form names a point, the binary form names the object's single value. Flags
//! that only make sense for one form (`WAIT_AVAILABLE` for timelines) are
//! rejected on the other.

use alloc::{sync::Arc, vec::Vec};

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{
    model::{
        device::DrmDevice,
        error::{DrmError, DrmResult},
        file::DrmFile,
        syncobj::{Syncobj, SyncobjHandle, WaitTimeout, wait_first, wait_index},
    },
    prelude::*,
    sync::{
        dma_fence::new_signaled_fence,
        fence_access::{fence_from_fd, install_named},
    },
};

/// The `first_signaled` value of a wait that completed no specific object.
const FIRST_SIGNALED_NONE: u32 = u32::MAX;

/// Copies and validates the handle array of a request.
///
/// The count is bounded before the array is copied, so a hostile count cannot
/// drive an unbounded allocation or a long handle walk.
fn load_handles(handles: UserPtr<u32>, count: u32) -> DrmResult<Vec<u32>> {
    if count > DRM_SYNCOBJ_MAX_HANDLES {
        return Err(DrmError::InvalidInput);
    }
    // `copy_from_user(ptr, 0)` never touches the pointer, so an empty list
    // must not fault on it either.
    if count == 0 {
        return Ok(Vec::new());
    }
    handles
        .load_vm_vec(count as usize)
        .map_err(|_| DrmError::BadAddress)
}

/// Resolves handles into objects, failing on the first unknown one.
fn resolve(file: &Arc<DrmFile>, handles: &[u32]) -> DrmResult<Vec<Arc<Syncobj>>> {
    let table = file.syncobjs.lock();
    handles
        .iter()
        .map(|handle| table.get(SyncobjHandle::from_raw(*handle)))
        .collect()
}

/// Applies `SYNCOBJ_SIGNAL` / `SYNCOBJ_RESET` to a set of objects.
///
/// `struct drm_syncobj_array` is `handles`, `count_handles` and `pad`; a
/// non-zero pad is `EINVAL`, as in Linux.
fn apply_to_binary(file: &Arc<DrmFile>, array: &DrmSyncobjArray, op: BinaryOp) -> DrmResult<usize> {
    if array.abi_pad != 0 {
        return Err(DrmError::InvalidInput);
    }
    let handles = load_handles(array.handles, array.count_handles)?;
    let objects = resolve(file, &handles)?;
    for object in objects {
        match op {
            BinaryOp::Signal => object.signal_binary(),
            BinaryOp::Reset => object.reset(),
        }
    }
    Ok(0)
}

/// What a signal/reset array command does to each object.
#[derive(Clone, Copy)]
enum BinaryOp {
    /// Make the object available.
    Signal,
    /// Clear the object's value.
    Reset,
}

impl Ioctl for DrmSyncobjCreate {
    const NAME: &'static str = "SYNCOBJ_CREATE";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        create: &mut Self,
    ) -> DrmResult<usize> {
        if create.flags & !DRM_SYNCOBJ_CREATE_SIGNALED != 0 {
            return Err(DrmError::InvalidInput);
        }
        let object = Syncobj::new_binary();
        if create.flags & DRM_SYNCOBJ_CREATE_SIGNALED != 0 {
            object.signal_binary();
        }
        let handle = file.syncobjs.lock().insert(object)?;
        create.handle = handle.raw();
        Ok(0)
    }
}

impl Ioctl for DrmSyncobjDestroy {
    const NAME: &'static str = "SYNCOBJ_DESTROY";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        destroy: &mut Self,
    ) -> DrmResult<usize> {
        if destroy.abi_pad != 0 {
            return Err(DrmError::InvalidInput);
        }
        file.syncobjs
            .lock()
            .remove(SyncobjHandle::from_raw(destroy.handle))?;
        Ok(0)
    }
}

impl Ioctl for DrmSyncobjSignal {
    const NAME: &'static str = "SYNCOBJ_SIGNAL";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        apply_to_binary(file, &request.0, BinaryOp::Signal)
    }
}

impl Ioctl for DrmSyncobjReset {
    const NAME: &'static str = "SYNCOBJ_RESET";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        apply_to_binary(file, &request.0, BinaryOp::Reset)
    }
}

impl Ioctl for DrmSyncobjTimelineArray {
    const NAME: &'static str = "SYNCOBJ_TIMELINE_SIGNAL";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        array: &mut Self,
    ) -> DrmResult<usize> {
        if array.flags != 0 {
            return Err(DrmError::InvalidInput);
        }
        let handles = load_handles(array.handles, array.count_handles)?;
        if handles.is_empty() {
            return Err(DrmError::InvalidInput);
        }
        let points = array
            .points
            .load_vm_vec(handles.len())
            .map_err(|_| DrmError::BadAddress)?;
        let objects = resolve(file, &handles)?;
        for (object, point) in objects.into_iter().zip(points) {
            // Chained onto the value it replaces (see `model::syncobj`), so a
            // client that signals an object another client waits on keeps both
            // dependencies visible.
            object.set_timeline(point, new_signaled_fence());
        }
        Ok(0)
    }
}

impl Ioctl for DrmSyncobjTransfer {
    const NAME: &'static str = "SYNCOBJ_TRANSFER";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        transfer: &mut Self,
    ) -> DrmResult<usize> {
        if transfer.flags != 0 || transfer.abi_pad != 0 {
            return Err(DrmError::InvalidInput);
        }
        let objects = resolve(file, &[transfer.src_handle, transfer.dst_handle])?;
        let (source, destination) = (&objects[0], &objects[1]);
        // An unset source transfers a dependency on nothing, which is what
        // makes `TRANSFER` of an empty object a success.
        let fence = source
            .binary_fence()
            .or_else(|| source.timeline_fence(transfer.src_point))
            .unwrap_or_else(new_signaled_fence);
        destination.set_timeline(transfer.dst_point, fence);
        Ok(0)
    }
}

impl Ioctl for DrmSyncobjQuery {
    const NAME: &'static str = "SYNCOBJ_QUERY";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        query: &mut Self,
    ) -> DrmResult<usize> {
        // Exactly one query mode must be selected.
        if query.flags != DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED
            && query.flags != DRM_SYNCOBJ_QUERY_FLAGS_LAST_SIGNALED
        {
            return Err(DrmError::InvalidInput);
        }
        let handles = load_handles(query.handles, query.count_handles)?;
        let objects = resolve(file, &handles)?;
        let points: Vec<u64> = objects
            .iter()
            .map(|object| object.available_point())
            .collect();
        query
            .points
            .write_vm_slice(&points)
            .map_err(|_| DrmError::BadAddress)?;
        Ok(0)
    }
}

impl Ioctl for DrmSyncobjEventfd {
    const NAME: &'static str = "SYNCOBJ_EVENTFD";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        unit: u32,
        eventfd: &mut Self,
    ) -> DrmResult<usize> {
        // `drm_syncobj_eventfd_ioctl`: validate the flags, resolve the object
        // and the eventfd, then tie the two together for this node's unit.
        crate::model::syncobj_signal::validate_request(eventfd.flags, eventfd.abi_pad)?;
        let object = file
            .syncobjs
            .lock()
            .get(SyncobjHandle::from_raw(eventfd.handle))?;
        let eventfd_object = crate::model::syncobj_signal::eventfd_from_fd(eventfd.fd)?;
        let signal = crate::model::syncobj_signal::EventFdSignal::new(
            eventfd_object,
            unit,
            eventfd.point,
            eventfd.flags,
        );
        object.add_event(&signal);
        Ok(0)
    }
}

impl Ioctl for DrmSyncobjWait {
    const NAME: &'static str = "SYNCOBJ_WAIT";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        wait: &mut Self,
    ) -> DrmResult<usize> {
        wait_objects(file, wait, false, None)
    }
}

impl Ioctl for DrmSyncobjTimelineWait {
    const NAME: &'static str = "SYNCOBJ_TIMELINE_WAIT";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        wait: &mut Self,
    ) -> DrmResult<usize> {
        let mut request = DrmSyncobjWait {
            handles: wait.handles,
            timeout_nsec: wait.timeout_nsec,
            count_handles: wait.count_handles,
            flags: wait.flags,
            first_signaled: 0,
            abi_pad: wait.abi_pad,
            dead_line_seqno: wait.dead_line_seqno,
        };
        wait_objects(file, &mut request, true, Some(wait.points))?;
        wait.first_signaled = request.first_signaled;
        Ok(0)
    }
}

/// `SYNCOBJ_WAIT` / `SYNCOBJ_TIMELINE_WAIT`.
///
/// Without `WAIT_ALL` the call reports the index of the first object that was
/// available (or became available); with `WAIT_ALL` it waits for every object
/// and reports no index. `timeout_nsec` is an absolute CLOCK_MONOTONIC instant
/// (`-1` waits forever, `0` polls), and a timeout is reported as a successful
/// wait with no index, as `drm_syncobj_wait_ioctl` does. A signal reports
/// success with no index as well. An empty handle list succeeds at once.
fn wait_objects(
    file: &Arc<DrmFile>,
    wait: &mut DrmSyncobjWait,
    is_timeline: bool,
    points: Option<UserPtr<u64>>,
) -> DrmResult<usize> {
    let (objects, timeout) = prepare_wait(file, wait, is_timeline, points)?;
    wait.first_signaled = FIRST_SIGNALED_NONE;
    if wait.flags & DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0 {
        for index in 0..objects.len() {
            wait_index(&objects, is_timeline, index, timeout)?;
        }
        return Ok(0);
    }
    if let Some(index) = wait_first(&objects, is_timeline, timeout)? {
        wait.first_signaled = index;
    }
    Ok(0)
}

/// The objects one wait names, and the deadline it is bounded by.
type WaitRequest = (Vec<(Arc<Syncobj>, u64)>, WaitTimeout);

/// Validates a wait request and resolves the objects it names.
fn prepare_wait(
    file: &Arc<DrmFile>,
    wait: &DrmSyncobjWait,
    is_timeline: bool,
    points_ptr: Option<UserPtr<u64>>,
) -> DrmResult<WaitRequest> {
    // `drm_syncobj_wait_ioctl` and `drm_syncobj_timeline_wait_ioctl` accept
    // different flag sets: only the timeline form knows `WAIT_AVAILABLE`.
    let known = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL
        | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT
        | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE
        | if is_timeline {
            DRM_SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE
        } else {
            0
        };
    if wait.flags & !known != 0 || wait.abi_pad != 0 {
        return Err(DrmError::InvalidInput);
    }
    let timeout = wait_timeout(wait)?;
    // An empty wait succeeds at once; it is not an error.
    let handles = load_handles(wait.handles, wait.count_handles)?;
    if handles.is_empty() {
        return Ok((Vec::new(), timeout));
    }
    // `points` holds one `u64` per handle; the binary form does not read it.
    let points: Vec<u64> = if is_timeline {
        points_ptr
            .ok_or(DrmError::InvalidInput)?
            .load_vm_vec(handles.len())
            .map_err(|_| DrmError::BadAddress)?
    } else {
        alloc::vec![0; handles.len()]
    };
    let objects = resolve(file, &handles)?;
    Ok((objects.into_iter().zip(points).collect(), timeout))
}

/// Turns the request's `timeout_nsec` into the wait's bound.
///
/// `drm_timeout_abs_to_jiffies`: the field is an **absolute** CLOCK_MONOTONIC
/// instant in nanoseconds (the UAPI documents it as "absolute timeout"), `0`
/// means poll, and a value in the past is also a poll. `-1` waits forever.
fn wait_timeout(wait: &DrmSyncobjWait) -> DrmResult<WaitTimeout> {
    if wait.timeout_nsec < 0 {
        return if wait.timeout_nsec == -1 {
            Ok(WaitTimeout::Infinite)
        } else {
            Err(DrmError::InvalidInput)
        };
    }
    let now = khal::time::monotonic_time();
    let timeout_ns = wait.timeout_nsec as u64;
    if timeout_ns == 0 {
        // A poll that cannot be satisfied by a value already available.
        return Ok(WaitTimeout::Until(now));
    }
    // Absolute nanoseconds since CLOCK_MONOTONIC's origin.
    Ok(WaitTimeout::Until(
        MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(timeout_ns)),
    ))
}

impl Ioctl for DrmSyncobjHandle {
    const NAME: &'static str = "SYNCOBJ_HANDLE_TO_FD";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        handle_to_fd(file, request)
    }
}

impl Ioctl for DrmSyncobjFdToHandle {
    const NAME: &'static str = "SYNCOBJ_FD_TO_HANDLE";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        fd_to_handle(file, &mut request.0)
    }
}

/// `SYNCOBJ_HANDLE_TO_FD`: export an object (or one of its points).
///
/// The exported sync_file holds its own reference to the fence, so destroying
/// the syncobj afterwards does not affect the descriptor. An export of an unset
/// value yields an already signaled fence, as `drm_syncobj_export_sync_file`
/// does.
fn handle_to_fd(file: &Arc<DrmFile>, request: &mut DrmSyncobjHandle) -> DrmResult<usize> {
    if request.abi_pad != 0 || request.flags & !DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE != 0
    {
        return Err(DrmError::InvalidInput);
    }
    let object = file
        .syncobjs
        .lock()
        .get(SyncobjHandle::from_raw(request.handle))?;
    if request.flags & DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE != 0 {
        // `drm_syncobj_export_sync_file`: a sync_file wrapping the object's
        // current value. It is a snapshot: a later signal or reset of the
        // object does not reach the exported file.
        let fence = object
            .binary_fence()
            .or_else(|| object.timeline_fence(0))
            .unwrap_or_else(new_signaled_fence);
        let cred = kprocess::current_cred();
        request.fd = install_named(&fence, Some(object.name.clone()), &cred)?;
        return Ok(0);
    }
    // Plain `HANDLE_TO_FD` exports the syncobj *object* (`syncobj_file`), not
    // its fence: importing that descriptor elsewhere names the same object,
    // so a later signal is visible through every handle it was imported to.
    let cred = kprocess::current_cred();
    let descriptor = crate::sync::syncobj_file::new_syncobj_file(object, &cred)
        .map_err(|_| DrmError::NoMemory)?;
    request.fd = kprocess::current_files()
        .add_file(descriptor, true)
        .map_err(|_| DrmError::NoMemory)?;
    Ok(0)
}

/// `SYNCOBJ_FD_TO_HANDLE`: import a descriptor into this file's table.
///
/// With `IMPORT_SYNC_FILE` the descriptor is a sync_file and its fence becomes
/// the object's value (`drm_syncobj_replace_fence`). Without it the descriptor
/// is a syncobj object descriptor and the object itself is named here.
fn fd_to_handle(file: &Arc<DrmFile>, request: &mut DrmSyncobjHandle) -> DrmResult<usize> {
    if request.abi_pad != 0 || request.flags & !DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE != 0
    {
        return Err(DrmError::InvalidInput);
    }
    let object = file
        .syncobjs
        .lock()
        .get(SyncobjHandle::from_raw(request.handle))?;
    if request.flags & DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE != 0 {
        // Resolve the fence before touching the object, so a bad fd leaves it
        // unchanged.
        let fence = fence_from_fd(request.fd)?;
        object.replace_binary(fence);
        return Ok(0);
    }
    // The descriptor must be a syncobj object; anything else is `EINVAL`, as
    // `drm_syncobj_fd_to_handle` answers a foreign file.
    let descriptor = kprocess::current_files()
        .get_file(request.fd)
        .map_err(|_| DrmError::InvalidInput)?;
    let imported =
        crate::sync::syncobj_file::object_of(&descriptor).ok_or(DrmError::InvalidInput)?;
    if let Some(handle) = file.syncobjs.lock().handle_of(&imported) {
        request.handle = handle.raw();
        return Ok(0);
    }
    let handle = file.syncobjs.lock().insert(imported)?;
    request.handle = handle.raw();
    Ok(0)
}
