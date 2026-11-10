// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `virtio-gpu`'s command range (`virtgpu_drm.h`, `virtgpu_ioctl.c`).
//!
//! The commands are the device's private ioctls, so they live here rather
//! than in `abi/`: a client of another device never sees them, and the wire
//! structures are the ones a virgl-capable host expects.
//!
//! Two rules shape this module:
//!
//! * **Only what the device negotiated is advertised.** `GETPARAM` answers
//!   from the backend's negotiated capabilities, so a 2D-only host reports no
//!   3D, no blob resources and no context support, and the commands that need
//!   them answer `EOPNOTSUPP` instead of pretending.
//! * **Completion is a fence.** `EXECBUFFER` with `FENCE_FD_OUT` hands out a
//!   sync_file descriptor from the card's fence layer (G2), completed when the
//!   host finished the command stream. A client waits on it exactly like on an
//!   out-fence of a commit.

mod abi;

use alloc::{string::String, sync::Arc, vec::Vec};

pub(crate) use abi::*;
use display::{CapsetInfo, DriverError, GpuCommand, ResourceCreate3D, Transfer3D};
use posix_types::UserPtr;

use crate::{
    gem::GemObject,
    ioctl::{Ioctl, IoctlDesc, IoctlFlags},
    model::syncobj::SyncobjHandle,
    prelude::*,
    sync::{
        dma_fence::FenceSignal,
        fence_access::{fence_from_fd, install},
    },
};

/// Per-open-file virtio-gpu state, the equivalent of `struct virtio_gpu_fpriv`.
///
/// Linux creates exactly one 3D context per file. `CONTEXT_INIT` only configures
/// that context before it is created; it does not hand a context id back to
/// userspace. Every submit and resource from the file belongs to this context.
#[derive(Default)]
pub(crate) struct VirtgpuFile {
    /// Host context id, assigned on first use.
    pub(crate) ctx_id: u32,
    /// Capset selected by `CONTEXT_INIT` (`VIRGL` or `VIRGL2`).
    pub(crate) capset: u32,
    /// Whether `CONTEXT_INIT` already selected a capset.
    pub(crate) capset_set: bool,
    /// Number of command rings declared by `CONTEXT_INIT`.
    pub(crate) num_rings: u32,
    /// Whether `CONTEXT_INIT` already declared the ring count.
    pub(crate) rings_set: bool,
    /// Rings whose completion events are polled (`POLL_RINGS_MASK`).
    pub(crate) ring_mask: u64,
    /// Debug name sent to the host.
    pub(crate) debug_name: String,
    /// Whether `CONTEXT_INIT` already set a debug name.
    pub(crate) debug_name_set: bool,
    /// Whether the host context was created.
    pub(crate) created: bool,
    /// Resources already attached to the context, as host resource ids.
    pub(crate) attached: alloc::collections::BTreeSet<u32>,
}

/// Creates the file's context on first use, as `virtio_gpu_create_context`.
///
/// The context is created with the capset `CONTEXT_INIT` selected (zero when it
/// never ran) and the file's debug name. The call is idempotent: once the host
/// created the context, later users reuse its id.
fn ensure_context(dev: &DrmDevice, file: &Arc<DrmFile>) -> DrmResult<u32> {
    let mut state = file.virtgpu.lock();
    if state.created {
        return Ok(state.ctx_id);
    }
    let gpu = gpu(dev)?;
    let ctx_id = dev.next_context_id();
    gpu.create_context(ctx_id, &state.debug_name, state.capset)
        .map_err(gpu_error)?;
    state.ctx_id = ctx_id;
    state.created = true;
    Ok(ctx_id)
}

impl Ioctl for DrmVirtgpuMap {
    const NAME: &'static str = "VIRTGPU_MAP";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        // The mapping offset is the object's, exactly what `MAP_DUMB` reports;
        // the handle must belong to the calling file.
        let object = file
            .handle(DumbHandle::from_raw(request.handle))
            .ok_or(DrmError::InvalidInput)?;
        request.offset = object.offset_bytes();
        Ok(0)
    }
}

impl Ioctl for DrmVirtgpuResourceCreate {
    const NAME: &'static str = "VIRTGPU_RESOURCE_CREATE";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        let gpu = gpu(dev)?;
        let ctx_id = ensure_context(dev, file)?;
        // The host resource and the GEM handle are two ids (`res_handle` and
        // `bo_handle` in Linux): the host names the resource, this file names
        // the object. Userspace does not pick either.
        let resource_id = dev.next_resource_id();
        gpu.create_resource_3d(ResourceCreate3D {
            ctx_id,
            resource_id: resource_id.raw(),
            target: request.target,
            format: request.format,
            bind: request.bind,
            width: request.width,
            height: request.height,
            depth: request.depth,
            array_size: request.array_size,
            last_level: request.last_level,
            nr_samples: request.nr_samples,
            flags: request.flags,
        })
        .map_err(gpu_error)?;
        // `virtio_gpu_resource_create_ioctl`: the backing is `params.size`
        // (zero means one page) and the stride is the requested `stride`.
        let pitch = if request.stride == 0 {
            request.width.saturating_mul(4)
        } else {
            request.stride
        };
        let size = usize::try_from(request.size).map_err(|_| DrmError::InvalidInput)?;
        let spec = harness_spec(request.width, request.height, pitch);
        let object = match GemObject::create_user_backing(dev, spec, size) {
            Ok(object) => object,
            Err(error) => {
                // No object exists to own the host resource yet, so release it
                // through the backend's raw resource path.
                if let Ok(backend) = dev.backend() {
                    let _ = backend.destroy_resource(resource_id);
                }
                return Err(error);
            }
        };
        // From here the object owns the host resource: every failure drops it
        // and its destructor releases the resource.
        object.set_resource(resource_id);
        let (base, length) = object.memory_range();
        let Ok(length) = u32::try_from(length) else {
            return Err(DrmError::InvalidInput);
        };
        // SAFETY: `base..base + length` is the object's own live allocation.
        if let Err(error) = unsafe { gpu.attach_backing(resource_id.raw(), base, length) } {
            return Err(gpu_error(error));
        }
        // `virtio_gpu_gem_object_open` attaches a resource to the file's
        // context when its GEM handle is created, not when a command stream
        // happens to name it in `bo_handles`: a shader or upload buffer is
        // referenced by id inside the stream and still must be resolvable.
        if let Err(error) = gpu.attach_resource(ctx_id, resource_id.raw()) {
            return Err(gpu_error(error));
        }
        file.virtgpu.lock().attached.insert(resource_id.raw());
        // Linux leaves the request's size/stride as the caller supplied them
        // and only reports the two ids.
        request.res_handle = resource_id.raw();
        request.bo_handle = object.handle().raw();
        file.handles.lock().insert(object.handle(), object);
        Ok(0)
    }
}

impl Ioctl for DrmVirtgpuGetparam {
    const NAME: &'static str = "VIRTGPU_GETPARAM";

    fn handle(
        dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        // Only negotiated facts are reported, and only the ones a working
        // call stands behind: guest blobs are servable, so `RESOURCE_BLOB`
        // follows the negotiated feature; host-visible memory and cross-device
        // uuid assignment are not implemented, so they stay zero.
        let caps = dev.caps();
        let value: u64 = match request.param {
            VIRTGPU_PARAM_3D_FEATURES => u64::from(caps.virgl),
            VIRTGPU_PARAM_CAPSET_QUERY_FIX => 1,
            VIRTGPU_PARAM_RESOURCE_BLOB => u64::from(caps.blob),
            VIRTGPU_PARAM_HOST_VISIBLE => 0,
            VIRTGPU_PARAM_CROSS_DEVICE => 0,
            VIRTGPU_PARAM_CONTEXT_INIT => u64::from(caps.context_init),
            VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS => {
                // Linux reports `capset_id_mask`, one bit *per capset id*; a
                // client (Mesa) reads bit `id` to decide the id is servable.
                supported_capset_mask(dev).unwrap_or(0)
            }
            VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME => 0,
            _ => return Err(DrmError::InvalidInput),
        };
        write_u64(request.value, value)?;
        Ok(0)
    }
}

impl Ioctl for DrmVirtgpuResourceInfo {
    const NAME: &'static str = "VIRTGPU_RESOURCE_INFO";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        let object = file
            .handle(DumbHandle::from_raw(request.bo_handle))
            .ok_or(DrmError::InvalidInput)?;
        // Linux reports the object's host resource id (`hw_res_handle`), not
        // the GEM handle. A PRIME importer learns the resource id only through
        // this call, so returning the handle makes it render into a resource
        // that does not exist.
        request.res_handle = object.resource()?.raw();
        request.size = object.length_bytes() as u32;
        request.blob_mem = object.blob_mem();
        Ok(0)
    }
}

impl Ioctl for DrmVirtgpu3dTransferToHost {
    const NAME: &'static str = "VIRTGPU_TRANSFER_TO_HOST";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        let gpu = gpu(dev)?;
        let ctx_id = ensure_context(dev, file)?;
        let object = file
            .handle(DumbHandle::from_raw(request.bo_handle))
            .ok_or(DrmError::InvalidInput)?;
        check_transfer(dev, &object, request.stride, request.layer_stride)?;
        let resource_id = object.resource()?.raw();
        gpu_call(gpu.transfer_to_host_3d(transfer(
            &request.box_,
            ctx_id,
            resource_id,
            request.level,
            request.offset,
            request.stride,
            request.layer_stride,
        )))?;
        Ok(0)
    }
}

impl Ioctl for DrmVirtgpu3dTransferFromHost {
    const NAME: &'static str = "VIRTGPU_TRANSFER_FROM_HOST";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        let gpu = gpu(dev)?;
        let ctx_id = ensure_context(dev, file)?;
        let object = file
            .handle(DumbHandle::from_raw(request.bo_handle))
            .ok_or(DrmError::InvalidInput)?;
        check_transfer(dev, &object, request.stride, request.layer_stride)?;
        let resource_id = object.resource()?.raw();
        gpu_call(gpu.transfer_from_host_3d(transfer(
            &request.box_,
            ctx_id,
            resource_id,
            request.level,
            request.offset,
            request.stride,
            request.layer_stride,
        )))?;
        Ok(0)
    }
}

impl Ioctl for DrmVirtgpu3dWait {
    const NAME: &'static str = "VIRTGPU_WAIT";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        if request.flags & !VIRTGPU_WAIT_NOWAIT != 0 {
            return Err(DrmError::InvalidInput);
        }
        let object = file
            .handle(DumbHandle::from_raw(request.handle))
            .ok_or(DrmError::InvalidInput)?;
        // The host work on a resource is ordered by the resource's
        // reservation, which is where the submitting client recorded it.
        let fences = object
            .resv()
            .snapshot(crate::sync::dma_resv::ResvUsage::Write);
        if request.flags & VIRTGPU_WAIT_NOWAIT != 0 {
            crate::sync::wait::check_error(&fences)?;
        } else {
            crate::sync::wait::wait_for_fences(&fences)?;
        }
        Ok(0)
    }
}

impl Ioctl for DrmVirtgpuGetCaps {
    const NAME: &'static str = "VIRTGPU_GET_CAPS";

    fn handle(
        dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        let gpu = gpu(dev)?;
        let info = capset_info(gpu, request.cap_set_id)?;
        // `virtio_gpu_get_caps_ioctl`: a zero-size request is the capability
        // query that reports the version and blob size back to userspace.
        if request.size == 0 {
            request.cap_set_ver = info.max_version;
            request.size = info.max_size;
            return Ok(0);
        }
        if request.addr == 0
            || request.cap_set_ver > info.max_version
            || request.size > info.max_size
        {
            return Err(DrmError::InvalidInput);
        }
        let blob = gpu
            .capset(request.cap_set_id, request.cap_set_ver, request.size)
            .map_err(gpu_error)?;
        let size = blob.len().min(request.size as usize);
        UserPtr::<u8>::from(usize::try_from(request.addr).map_err(|_| DrmError::InvalidInput)?)
            .write_vm_slice(&blob[..size])
            .map_err(|_| DrmError::BadAddress)?;
        request.size = size as u32;
        Ok(0)
    }
}

/// The most capsets a virtio-gpu device may report.
///
/// Linux walks `vgdev->num_capsets`, which the device configuration fixes at
/// probe time; this driver does not cache that count, so the walk is bounded
/// by the protocol's own limit instead of trusting the host to ever answer
/// `None`.
const MAX_CAPSETS: u32 = 8;

/// Validates the strides a 3D transfer may name (`virtio_gpu_transfer_*_ioctl`).
///
/// A guest-only blob cannot be transferred through the 3D path, and only a
/// host3d blob carries a stride; for an ordinary resource both must be zero.
fn check_transfer(
    _dev: &DrmDevice,
    object: &GemObject,
    stride: u32,
    layer_stride: u32,
) -> DrmResult<()> {
    match object.blob_mem() {
        VIRTGPU_BLOB_MEM_GUEST => Err(DrmError::InvalidInput),
        VIRTGPU_BLOB_MEM_HOST3D => Ok(()),
        _ if stride != 0 || layer_stride != 0 => Err(DrmError::InvalidInput),
        _ => Ok(()),
    }
}

/// The backing spec of one `RESOURCE_CREATE`.
fn harness_spec(width: u32, height: u32, pitch: u32) -> crate::model::backend::ResourceSpec {
    crate::model::backend::ResourceSpec {
        width,
        height,
        pitch,
        format: display::ScanoutFormat::Bgra8888,
    }
}

/// Finds the capability set with id `id` among the device's capsets.
fn capset_info(gpu: &dyn GpuCommand, id: u32) -> DrmResult<CapsetInfo> {
    for index in 0..MAX_CAPSETS {
        match gpu.capset_info(index).map_err(gpu_error)? {
            // A device answers indices past its list with an all-zero
            // descriptor; capset ids are 1-based, so zero is the terminator.
            Some(info) if info.id == 0 => break,
            Some(info) => {
                if info.id == id {
                    return Ok(info);
                }
            }
            None => break,
        }
    }
    Err(DrmError::InvalidInput)
}

/// `capset_id_mask`: every capset id the host answered for, as `1 << id`.
fn supported_capset_mask(dev: &DrmDevice) -> DrmResult<u64> {
    let gpu = gpu(dev)?;
    let mut mask = 0u64;
    for index in 0..MAX_CAPSETS {
        match gpu.capset_info(index).map_err(gpu_error)? {
            Some(info) if info.id == 0 => break,
            Some(info) if info.id < 64 => mask |= 1u64 << info.id,
            Some(_) => {}
            None => break,
        }
    }
    Ok(mask)
}

impl Ioctl for DrmVirtgpuContextInit {
    const NAME: &'static str = "VIRTGPU_CONTEXT_INIT";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        let gpu = gpu(dev)?;
        if request.abi_pad != 0 {
            return Err(DrmError::InvalidInput);
        }
        // `virtio_gpu_context_init_ioctl` bounds the parameter list at the
        // four parameters the ABI defines.
        if request.num_params > MAX_CONTEXT_PARAMS {
            return Err(DrmError::InvalidInput);
        }
        // The parameter array is read before the context lock, so a faulting
        // `ctx_set_params` cannot hold a client onto the card.
        let params: Vec<DrmVirtgpuContextSetParam> = if request.num_params == 0 {
            Vec::new()
        } else {
            UserPtr::<DrmVirtgpuContextSetParam>::from(
                usize::try_from(request.ctx_set_params).map_err(|_| DrmError::InvalidInput)?,
            )
            .load_vm_vec(request.num_params as usize)
            .map_err(|_| DrmError::BadAddress)?
        };
        let mut next = VirtgpuFile::default();
        let mut seen = [false; CONTEXT_PARAM_COUNT];
        for param in params {
            let slot = match param.param {
                VIRTGPU_CONTEXT_PARAM_CAPSET_ID => {
                    if next.capset_set {
                        return Err(DrmError::InvalidInput);
                    }
                    // Linux rejects a capset the host did not report; this
                    // host offers `VIRGL` and `VIRGL2` when it offers 3D.
                    if !(CONTEXT_CAPSET_VIRGL..=CONTEXT_CAPSET_VIRGL2).contains(&param.value) {
                        return Err(DrmError::InvalidInput);
                    }
                    next.capset = param.value as u32;
                    next.capset_set = true;
                    CONTEXT_PARAM_CAPSET
                }
                VIRTGPU_CONTEXT_PARAM_NUM_RINGS => {
                    if next.rings_set {
                        return Err(DrmError::InvalidInput);
                    }
                    if param.value > u64::from(MAX_CONTEXT_RINGS) {
                        return Err(DrmError::InvalidInput);
                    }
                    next.num_rings = param.value as u32;
                    next.rings_set = true;
                    CONTEXT_PARAM_RINGS
                }
                VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK => {
                    if next.ring_mask != 0 {
                        return Err(DrmError::InvalidInput);
                    }
                    next.ring_mask = param.value;
                    CONTEXT_PARAM_POLL
                }
                VIRTGPU_CONTEXT_PARAM_DEBUG_NAME => {
                    if next.debug_name_set {
                        return Err(DrmError::InvalidInput);
                    }
                    next.debug_name = read_name(param.value)?;
                    next.debug_name_set = true;
                    CONTEXT_PARAM_NAME
                }
                _ => return Err(DrmError::InvalidInput),
            };
            if seen[slot] {
                return Err(DrmError::InvalidInput);
            }
            seen[slot] = true;
        }
        // A polled ring must be one of the rings the file declared.
        if next.ring_mask != 0 {
            let valid = if next.num_rings >= 64 {
                u64::MAX
            } else {
                (1u64 << next.num_rings) - 1
            };
            if next.ring_mask & !valid != 0 {
                return Err(DrmError::InvalidInput);
            }
        }
        // The context is created once, on this call: a second `CONTEXT_INIT`
        // finds the created context and reports `EEXIST`, as Linux does.
        let mut state = file.virtgpu.lock();
        if state.created {
            return Err(DrmError::AlreadyExists);
        }
        let ctx_id = dev.next_context_id();
        gpu.create_context(ctx_id, &next.debug_name, next.capset)
            .map_err(gpu_error)?;
        *state = next;
        state.ctx_id = ctx_id;
        state.created = true;
        Ok(0)
    }
}

impl Ioctl for DrmVirtgpuExecbuffer {
    const NAME: &'static str = "VIRTGPU_EXECBUFFER";

    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: one submit owns the fence, dependency, attachment and \
                  rollback protocol; splitting the parts without the others would hide \
                  partial-failure cleanup"
    )]
    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        let gpu = gpu(dev)?;
        if request.flags & !VIRTGPU_EXECBUF_FLAGS != 0 {
            return Err(DrmError::InvalidInput);
        }
        // Every submit runs in the file's context (`vfpriv->ctx_id`); a ring
        // index is a ring of *that* context, valid only when `CONTEXT_INIT`
        // declared rings.
        let ctx_id = ensure_context(dev, file)?;
        if request.flags & VIRTGPU_EXECBUF_RING_IDX != 0 {
            let state = file.virtgpu.lock();
            if state.num_rings == 0 || request.ring_idx >= state.num_rings {
                return Err(DrmError::InvalidInput);
            }
        }
        if request.size == 0 || !request.size.is_multiple_of(4) || request.size > MAX_COMMAND_BYTES
        {
            return Err(DrmError::InvalidInput);
        }
        if request.syncobj_stride != 0
            && (request.syncobj_stride as usize)
                < core::mem::size_of::<DrmVirtgpuExecbufferSyncobj>()
        {
            return Err(DrmError::InvalidInput);
        }
        let command: Vec<u8> = UserPtr::<u8>::from(
            usize::try_from(request.command).map_err(|_| DrmError::InvalidInput)?,
        )
        .load_vm_vec(request.size as usize)
        .map_err(|_| DrmError::BadAddress)?;
        // Every buffer the stream may touch must belong to the calling file,
        // so one client cannot drive another client's memory.
        let bo_handles: Vec<u32> = if request.bo_handles != 0 {
            UserPtr::<u32>::from(
                usize::try_from(request.bo_handles).map_err(|_| DrmError::InvalidInput)?,
            )
            .load_vm_vec(request.num_bo_handles as usize)
            .map_err(|_| DrmError::BadAddress)?
        } else {
            Vec::new()
        };
        let mut objects = Vec::with_capacity(bo_handles.len());
        for handle in bo_handles {
            let object = file
                .handle(DumbHandle::from_raw(handle))
                .ok_or(DrmError::InvalidInput)?;
            objects.push(object);
        }

        // The in-fence is a sync_file, and the deps are syncobj or sync_file
        // fences; both are resolved before anything is submitted. Linux parses
        // the out dependencies first, so a malformed out array fails before
        // any in dependency was waited on.
        if request.num_out_syncobjs != 0 && request.out_syncobjs == 0 {
            return Err(DrmError::InvalidInput);
        }
        if request.num_in_syncobjs != 0 && request.in_syncobjs == 0 {
            return Err(DrmError::InvalidInput);
        }
        // Linux attaches every buffer the stream names to the file's context
        // before submitting (`virtio_gpu_cmd_context_attach_resource`); the
        // host renderer resolves no resource that is not attached.
        for object in &objects {
            let resource = object.resource()?;
            let needs_attach = {
                let state = file.virtgpu.lock();
                !state.attached.contains(&resource.raw())
            };
            if needs_attach {
                gpu.attach_resource(ctx_id, resource.raw())
                    .map_err(gpu_error)?;
                file.virtgpu.lock().attached.insert(resource.raw());
            }
        }
        let out_deps = parse_out_syncobjs(file, request)?;
        let mut reset = Vec::new();
        if let Err(error) = wait_for_execbuffer(file, request, &mut reset) {
            for object in &reset {
                object.reset();
            }
            return Err(error);
        }
        // The fence exists before the submit, so the command stream, its
        // out-fence and the buffers it names share one completion point.
        let (fence, signal) =
            crate::sync::dma_fence::new_anonymous_fence_named(file.dev.driver_info().name);
        let fence_id = fence.seqno();
        let mut previous_fences = Vec::with_capacity(objects.len());
        for object in &objects {
            previous_fences.push(object.resv().set_write_fence(fence.clone()));
        }
        let cred = kprocess::current_cred();
        // The submission is synchronous here, so the fence completes inside
        // `submit`. The out dependencies are published and the reset
        // dependencies cleared only for a submission that reached the host,
        // which is where Linux installs them (`virtio_gpu_process_post_deps`
        // runs after the submit; `cleanup_submit` resets in any case). The
        // out-fence descriptor is installed only after this succeeded, so a
        // failed submit cannot leak an fd into the caller.
        let submitted = submit(dev, gpu, ctx_id, request, &command, fence_id, signal);
        if submitted.is_err() {
            for (object, previous) in objects.iter().zip(previous_fences) {
                object.resv().restore_write_fence(previous);
            }
            for object in &reset {
                object.reset();
            }
            submitted?;
        }
        let fd = if request.flags & VIRTGPU_EXECBUF_FENCE_FD_OUT != 0 {
            match install(&fence, &cred) {
                Ok(fd) => Some(fd),
                Err(error) => {
                    for object in &reset {
                        object.reset();
                    }
                    return Err(error);
                }
            }
        } else {
            None
        };
        for (object, point) in &out_deps {
            if *point != 0 {
                object.set_timeline(*point, fence.clone());
            } else {
                object.replace_binary(fence.clone());
            }
        }
        for object in &reset {
            object.reset();
        }
        if let Some(fd) = fd {
            request.fence_fd = fd;
        }
        Ok(0)
    }
}

/// Slot index of each `CONTEXT_INIT` parameter, for the duplicate check.
const CONTEXT_PARAM_CAPSET: usize = 0;
const CONTEXT_PARAM_RINGS: usize = 1;
const CONTEXT_PARAM_POLL: usize = 2;
const CONTEXT_PARAM_NAME: usize = 3;
const CONTEXT_PARAM_COUNT: usize = 4;

/// `VIRGL`, the first capset id a VIRGL-capable host offers.
const CONTEXT_CAPSET_VIRGL: u64 = 1;
/// `VIRGL2`, the second capset id.
const CONTEXT_CAPSET_VIRGL2: u64 = 2;

/// Upper bound on `CONTEXT_INIT` parameters, the four the ABI defines.
const MAX_CONTEXT_PARAMS: u32 = 4;
/// Upper bound on a context's command rings (`MAX_RINGS` in `virtgpu_drv.h`).
const MAX_CONTEXT_RINGS: u32 = 64;
/// Upper bound on one command stream, as virgl's ring buffer is 16 MiB.
const MAX_COMMAND_BYTES: u32 = 16 * 1024 * 1024;

/// Maps a driver-contract failure onto this crate's error domain.
///
/// `Unsupported` is "the device cannot do this" and everything else is the
/// device reporting a fault, which is what the host answer is.
const fn gpu_error(error: DriverError) -> DrmError {
    match error {
        DriverError::Unsupported => DrmError::OpNotSupported,
        DriverError::InvalidInput => DrmError::InvalidInput,
        DriverError::WouldBlock => DrmError::WouldBlock,
        _ => DrmError::Io,
    }
}

/// Runs one GPU command and maps its failure.
fn gpu_call(result: Result<(), DriverError>) -> DrmResult<()> {
    result.map_err(gpu_error)
}

/// The backend's 3D channel, or `EOPNOTSUPP` when the device has none.
fn gpu(dev: &DrmDevice) -> DrmResult<&dyn GpuCommand> {
    dev.backend()?
        .gpu()
        .ok_or(DrmError::OpNotSupported)
        .and_then(|gpu| {
            gpu.is_available()
                .then_some(gpu)
                .ok_or(DrmError::OpNotSupported)
        })
}

/// Builds the box transfer the host receives.
fn transfer(
    box_: &DrmVirtgpu3dBox,
    ctx_id: u32,
    resource_id: u32,
    level: u32,
    offset: u32,
    stride: u32,
    layer_stride: u32,
) -> Transfer3D {
    Transfer3D {
        ctx_id,
        resource_id,
        x: box_.x,
        y: box_.y,
        z: box_.z,
        w: box_.w,
        h: box_.h,
        d: box_.d,
        offset: u64::from(offset),
        level,
        stride,
        layer_stride,
    }
}

/// Reads one `drm_virtgpu_execbuffer_syncobj` of a stride-width array.
fn syncobj_entry(base: u64, stride: usize, index: usize) -> DrmResult<DrmVirtgpuExecbufferSyncobj> {
    let offset = index
        .checked_mul(stride)
        .and_then(|offset| u64::try_from(offset).ok())
        .ok_or(DrmError::InvalidInput)?;
    let address = base.checked_add(offset).ok_or(DrmError::InvalidInput)?;
    UserPtr::<DrmVirtgpuExecbufferSyncobj>::from(
        usize::try_from(address).map_err(|_| DrmError::InvalidInput)?,
    )
    .read_vm()
    .map_err(|_| DrmError::BadAddress)
}

/// Resolves and waits on the in dependencies of one submit.
///
/// This is `virtio_gpu_parse_deps` plus `virtio_gpu_wait_in_fence`: the
/// explicit in-fence descriptor must name a sync_file, every in-syncobj names
/// a fence that must complete before the stream runs, and the objects a
/// `SYNCOBJ_RESET` entry names are collected for the caller to reset after
/// the submit reached the host.
fn wait_for_execbuffer(
    file: &Arc<DrmFile>,
    request: &DrmVirtgpuExecbuffer,
    reset: &mut Vec<alloc::sync::Arc<crate::model::syncobj::Syncobj>>,
) -> DrmResult<()> {
    if request.flags & VIRTGPU_EXECBUF_FENCE_FD_IN == 0 {
        if request.fence_fd != -1 {
            return Err(DrmError::InvalidInput);
        }
    } else {
        let fence = fence_from_fd(request.fence_fd)?;
        crate::sync::wait::wait_for_fences(&crate::sync::fence_list::FenceList::new(alloc::vec![
            fence
        ]))?;
    }
    if request.num_in_syncobjs == 0 {
        return Ok(());
    }
    let stride = request.syncobj_stride as usize;
    let base = request.in_syncobjs;
    for index in 0..request.num_in_syncobjs as usize {
        let entry = syncobj_entry(base, stride, index)?;
        if entry.flags & !VIRTGPU_EXECBUF_SYNCOBJ_FLAGS != 0 {
            return Err(DrmError::InvalidInput);
        }
        let object = file
            .syncobjs
            .lock()
            .get(SyncobjHandle::from_raw(entry.handle))?;
        let fence = object
            .timeline_fence(entry.point)
            .or_else(|| object.binary_fence());
        let Some(fence) = fence else {
            return Err(DrmError::InvalidInput);
        };
        crate::sync::wait::wait_for_fences(&crate::sync::fence_list::FenceList::new(alloc::vec![
            fence
        ]))?;
        if entry.flags & VIRTGPU_EXECBUF_SYNCOBJ_RESET != 0 {
            reset.push(object);
        }
    }
    Ok(())
}

/// Resolves the out dependencies of one submit.
///
/// This is `virtio_gpu_parse_post_deps`: an out entry carries no flags, and a
/// non-zero point turns the object into a timeline whose point the submit's
/// fence completes.
fn parse_out_syncobjs(
    file: &Arc<DrmFile>,
    request: &DrmVirtgpuExecbuffer,
) -> DrmResult<Vec<(alloc::sync::Arc<crate::model::syncobj::Syncobj>, u64)>> {
    if request.num_out_syncobjs == 0 {
        return Ok(Vec::new());
    }
    let stride = request.syncobj_stride as usize;
    let base = request.out_syncobjs;
    let mut deps = Vec::new();
    for index in 0..request.num_out_syncobjs as usize {
        let entry = syncobj_entry(base, stride, index)?;
        if entry.flags != 0 {
            return Err(DrmError::InvalidInput);
        }
        let object = file
            .syncobjs
            .lock()
            .get(SyncobjHandle::from_raw(entry.handle))?;
        deps.push((object, entry.point));
    }
    Ok(deps)
}

/// Submits the stream and arranges for the fence to complete with it.
///
/// The host answers a fenced `SUBMIT_3D` only when the stream is done, so the
/// answer itself is the completion.
fn submit(
    _dev: &DrmDevice,
    gpu: &dyn GpuCommand,
    ctx_id: u32,
    _request: &DrmVirtgpuExecbuffer,
    command: &[u8],
    fence_id: u64,
    signal: Arc<FenceSignal>,
) -> DrmResult<()> {
    match gpu.submit(ctx_id, fence_id, command).map_err(gpu_error) {
        Ok(()) => {
            signal.signal();
            Ok(())
        }
        Err(error) => {
            signal.signal_error(crate::model::error::DrmError::Io.errno());
            Err(error)
        }
    }
}

/// Reads a NUL-terminated debug name from userspace.
fn read_name(address: u64) -> DrmResult<String> {
    if address == 0 {
        return Ok(String::new());
    }
    let mut bytes = Vec::with_capacity(64);
    for index in 0..64usize {
        let byte: u8 = UserPtr::<u8>::from(
            usize::try_from(address).map_err(|_| DrmError::InvalidInput)? + index,
        )
        .read_vm()
        .map_err(|_| DrmError::BadAddress)?;
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Writes one `__u64` through a userspace pointer.
fn write_u64(address: u64, value: u64) -> DrmResult<()> {
    UserPtr::<u64>::from(usize::try_from(address).map_err(|_| DrmError::InvalidInput)?)
        .write_vm(value)
        .map_err(|_| DrmError::BadAddress)
}

/// The virtio-gpu command table, for the dispatcher's lookup.
pub(crate) fn table() -> &'static [IoctlDesc] {
    VIRTGPU_IOCTLS
}

/// One virtio-gpu command number.
///
/// `DRM_COMMAND_BASE` is the device range's first byte, so a command number is
/// the base plus its index (`DRM_IOCTL_VIRTGPU_MAP` and friends).
const fn virtgpu_cmd(index: u8) -> u8 {
    DRM_COMMAND_BASE + index
}

/// The virtio-gpu commands, in command-number order.
///
/// They live in the device's own range, so a lookup checks this table as well
/// as the core one; a card that is not virtio-gpu still answers
/// `EOPNOTSUPP` because the handlers themselves ask the backend.
pub(crate) static VIRTGPU_IOCTLS: &[IoctlDesc] = &[
    IoctlDesc::new::<abi::DrmVirtgpuMap>(
        iowr::<abi::DrmVirtgpuMap>(DRM_TYPE, virtgpu_cmd(abi::VIRTGPU_MAP)),
        abi::DrmVirtgpuMap::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpuExecbuffer>(
        iowr::<abi::DrmVirtgpuExecbuffer>(DRM_TYPE, virtgpu_cmd(abi::VIRTGPU_EXECBUFFER)),
        abi::DrmVirtgpuExecbuffer::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpuGetparam>(
        iowr::<abi::DrmVirtgpuGetparam>(DRM_TYPE, virtgpu_cmd(abi::VIRTGPU_GETPARAM)),
        abi::DrmVirtgpuGetparam::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpuResourceCreate>(
        iowr::<abi::DrmVirtgpuResourceCreate>(DRM_TYPE, virtgpu_cmd(abi::VIRTGPU_RESOURCE_CREATE)),
        abi::DrmVirtgpuResourceCreate::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpuResourceInfo>(
        iowr::<abi::DrmVirtgpuResourceInfo>(DRM_TYPE, virtgpu_cmd(abi::VIRTGPU_RESOURCE_INFO)),
        abi::DrmVirtgpuResourceInfo::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpu3dTransferFromHost>(
        iowr::<abi::DrmVirtgpu3dTransferFromHost>(
            DRM_TYPE,
            virtgpu_cmd(abi::VIRTGPU_TRANSFER_FROM_HOST),
        ),
        abi::DrmVirtgpu3dTransferFromHost::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpu3dTransferToHost>(
        iowr::<abi::DrmVirtgpu3dTransferToHost>(
            DRM_TYPE,
            virtgpu_cmd(abi::VIRTGPU_TRANSFER_TO_HOST),
        ),
        abi::DrmVirtgpu3dTransferToHost::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpu3dWait>(
        iowr::<abi::DrmVirtgpu3dWait>(DRM_TYPE, virtgpu_cmd(abi::VIRTGPU_WAIT)),
        abi::DrmVirtgpu3dWait::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpuGetCaps>(
        iowr::<abi::DrmVirtgpuGetCaps>(DRM_TYPE, virtgpu_cmd(abi::VIRTGPU_GET_CAPS)),
        abi::DrmVirtgpuGetCaps::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpuResourceCreateBlob>(
        iowr::<abi::DrmVirtgpuResourceCreateBlob>(
            DRM_TYPE,
            virtgpu_cmd(abi::VIRTGPU_RESOURCE_CREATE_BLOB),
        ),
        abi::DrmVirtgpuResourceCreateBlob::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
    IoctlDesc::new::<abi::DrmVirtgpuContextInit>(
        iowr::<abi::DrmVirtgpuContextInit>(DRM_TYPE, virtgpu_cmd(abi::VIRTGPU_CONTEXT_INIT)),
        abi::DrmVirtgpuContextInit::NAME,
        IoctlFlags::RENDER_ALLOW,
    ),
];

impl Ioctl for DrmVirtgpuResourceCreateBlob {
    const NAME: &'static str = "VIRTGPU_RESOURCE_CREATE_BLOB";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        request: &mut Self,
    ) -> DrmResult<usize> {
        // Blob resources need `VIRTIO_GPU_F_RESOURCE_BLOB`; without it the
        // command is refused instead of being emulated with 2D resources.
        if !dev.caps().blob {
            return Err(DrmError::OpNotSupported);
        }
        // `verify_blob`: an undefined use bit, or a cross-device use without
        // uuid assignment support, is `EINVAL`.
        if request.blob_flags & !VIRTGPU_BLOB_FLAG_USE_MASK != 0 {
            return Err(DrmError::InvalidInput);
        }
        if request.blob_flags & VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE != 0 {
            return Err(DrmError::InvalidInput);
        }
        // Only guest-backed blobs are servable: the pages are this driver's
        // allocation, so `MAP`, PRIME export and framebuffer views work on
        // them like on a dumb buffer. A host-renderer blob
        // (`HOST3D`/`HOST3D_GUEST`) needs the host-visible memory window,
        // which this card does not expose.
        match request.blob_mem {
            VIRTGPU_BLOB_MEM_GUEST => {}
            VIRTGPU_BLOB_MEM_HOST3D | VIRTGPU_BLOB_MEM_HOST3D_GUEST => {
                return Err(DrmError::OpNotSupported);
            }
            _ => return Err(DrmError::InvalidInput),
        }
        if request.blob_id != 0 || request.cmd_size != 0 {
            return Err(DrmError::InvalidInput);
        }
        let size = usize::try_from(request.size).map_err(|_| DrmError::InvalidInput)?;
        let object = GemObject::create_blob_backing(dev, size)?;
        let gpu = gpu(dev)?;
        let ctx_id = ensure_context(dev, file)?;
        let resource_id = dev.next_resource_id();
        // SAFETY: the entry list names the object's own allocation. The object
        // keeps the pages alive and unshared, and its destructor unrefs the
        // host resource before the pages are freed — the lifetime
        // `resource_create_blob` demands of the entry list.
        let (base, length) = object.memory_range();
        let Ok(length) = u32::try_from(length) else {
            return Err(DrmError::InvalidInput);
        };
        // SAFETY: `base..base + length` is the object's own live allocation.
        unsafe {
            gpu.create_blob(
                0,
                resource_id.raw(),
                request.blob_mem,
                request.blob_flags,
                request.size,
                0,
                base,
                length,
            )
        }
        .map_err(gpu_error)?;
        object.set_resource(resource_id);
        object.set_blob_mem(request.blob_mem);
        if let Err(error) = gpu.attach_resource(ctx_id, resource_id.raw()) {
            return Err(gpu_error(error));
        }
        file.virtgpu.lock().attached.insert(resource_id.raw());
        request.res_handle = resource_id.raw();
        request.bo_handle = object.handle().raw();
        file.handles.lock().insert(object.handle(), object);
        Ok(0)
    }
}
