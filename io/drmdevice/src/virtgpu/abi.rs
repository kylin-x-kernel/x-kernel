// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `drm_virtgpu_*`: the wire structures of `uapi/drm/virtgpu_drm.h`.
//!
//! These are the virtio-gpu driver's private command range
//! (`DRM_COMMAND_BASE + 0x01 ..= 0x0B`), kept apart from the core `abi` module:
//! they describe one device family, not the DRM core, and a client of another
//! device never sees them.
//!
//! The layouts are copied field for field from the UAPI header, including its
//! padding, so an ABI test can compare sizes instead of trusting the port.

use macros::{UserRead, UserWrite};

/// `DRM_COMMAND_BASE`.
pub(crate) const DRM_COMMAND_BASE: u8 = 0x40;

/// `DRM_VIRTGPU_MAP`.
pub(crate) const VIRTGPU_MAP: u8 = 0x01;
/// `DRM_VIRTGPU_EXECBUFFER`.
pub(crate) const VIRTGPU_EXECBUFFER: u8 = 0x02;
/// `DRM_VIRTGPU_GETPARAM`.
pub(crate) const VIRTGPU_GETPARAM: u8 = 0x03;
/// `DRM_VIRTGPU_RESOURCE_CREATE`.
pub(crate) const VIRTGPU_RESOURCE_CREATE: u8 = 0x04;
/// `DRM_VIRTGPU_RESOURCE_INFO`.
pub(crate) const VIRTGPU_RESOURCE_INFO: u8 = 0x05;
/// `DRM_VIRTGPU_TRANSFER_FROM_HOST`.
pub(crate) const VIRTGPU_TRANSFER_FROM_HOST: u8 = 0x06;
/// `DRM_VIRTGPU_TRANSFER_TO_HOST`.
pub(crate) const VIRTGPU_TRANSFER_TO_HOST: u8 = 0x07;
/// `DRM_VIRTGPU_WAIT`.
pub(crate) const VIRTGPU_WAIT: u8 = 0x08;
/// `DRM_VIRTGPU_GET_CAPS`.
pub(crate) const VIRTGPU_GET_CAPS: u8 = 0x09;
/// `DRM_VIRTGPU_RESOURCE_CREATE_BLOB`.
pub(crate) const VIRTGPU_RESOURCE_CREATE_BLOB: u8 = 0x0a;
/// `DRM_VIRTGPU_CONTEXT_INIT`.
pub(crate) const VIRTGPU_CONTEXT_INIT: u8 = 0x0b;

/// `VIRTGPU_PARAM_3D_FEATURES`.
pub(crate) const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
/// `VIRTGPU_PARAM_CAPSET_QUERY_FIX`.
pub(crate) const VIRTGPU_PARAM_CAPSET_QUERY_FIX: u64 = 2;
/// `VIRTGPU_PARAM_RESOURCE_BLOB`.
pub(crate) const VIRTGPU_PARAM_RESOURCE_BLOB: u64 = 3;
/// `VIRTGPU_PARAM_HOST_VISIBLE`.
pub(crate) const VIRTGPU_PARAM_HOST_VISIBLE: u64 = 4;
/// `VIRTGPU_PARAM_CROSS_DEVICE`.
pub(crate) const VIRTGPU_PARAM_CROSS_DEVICE: u64 = 5;
/// `VIRTGPU_PARAM_CONTEXT_INIT`.
pub(crate) const VIRTGPU_PARAM_CONTEXT_INIT: u64 = 6;
/// `VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs`.
pub(crate) const VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS: u64 = 7;
/// `VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME`.
pub(crate) const VIRTGPU_PARAM_EXPLICIT_DEBUG_NAME: u64 = 8;

/// `VIRTGPU_EXECBUF_FENCE_FD_IN`.
pub(crate) const VIRTGPU_EXECBUF_FENCE_FD_IN: u32 = 0x01;
/// `VIRTGPU_EXECBUF_FENCE_FD_OUT`.
pub(crate) const VIRTGPU_EXECBUF_FENCE_FD_OUT: u32 = 0x02;
/// `VIRTGPU_EXECBUF_RING_IDX`.
pub(crate) const VIRTGPU_EXECBUF_RING_IDX: u32 = 0x04;
/// `VIRTGPU_EXECBUF_FLAGS`.
pub(crate) const VIRTGPU_EXECBUF_FLAGS: u32 =
    VIRTGPU_EXECBUF_FENCE_FD_IN | VIRTGPU_EXECBUF_FENCE_FD_OUT | VIRTGPU_EXECBUF_RING_IDX;
/// `VIRTGPU_EXECBUF_SYNCOBJ_RESET`.
pub(crate) const VIRTGPU_EXECBUF_SYNCOBJ_RESET: u32 = 0x01;
/// `VIRTGPU_EXECBUF_SYNCOBJ_FLAGS`.
pub(crate) const VIRTGPU_EXECBUF_SYNCOBJ_FLAGS: u32 = VIRTGPU_EXECBUF_SYNCOBJ_RESET;

/// `VIRTGPU_CONTEXT_PARAM_CAPSET_ID`.
pub(crate) const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 0x0001;
/// `VIRTGPU_CONTEXT_PARAM_NUM_RINGS`.
pub(crate) const VIRTGPU_CONTEXT_PARAM_NUM_RINGS: u64 = 0x0002;
/// `VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK`.
pub(crate) const VIRTGPU_CONTEXT_PARAM_POLL_RINGS_MASK: u64 = 0x0003;
/// `VIRTGPU_CONTEXT_PARAM_DEBUG_NAME`.
pub(crate) const VIRTGPU_CONTEXT_PARAM_DEBUG_NAME: u64 = 0x0004;

/// `VIRTGPU_BLOB_MEM_GUEST`: the guest allocates the backing memory.
pub(crate) const VIRTGPU_BLOB_MEM_GUEST: u32 = 0x1;
/// `VIRTGPU_BLOB_MEM_HOST3D`: the host renderer allocates the memory.
pub(crate) const VIRTGPU_BLOB_MEM_HOST3D: u32 = 0x2;
/// `VIRTGPU_BLOB_MEM_HOST3D_GUEST`: guest backing, host-created blob.
pub(crate) const VIRTGPU_BLOB_MEM_HOST3D_GUEST: u32 = 0x3;

/// `VIRTGPU_BLOB_FLAG_USE_MAPPABLE`.
pub(crate) const VIRTGPU_BLOB_FLAG_USE_MAPPABLE: u32 = 0x1;
/// `VIRTGPU_BLOB_FLAG_USE_SHAREABLE`.
pub(crate) const VIRTGPU_BLOB_FLAG_USE_SHAREABLE: u32 = 0x2;
/// `VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE`.
pub(crate) const VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE: u32 = 0x4;
/// Every `VIRTGPU_BLOB_FLAG_USE_*` bit (`VIRTGPU_BLOB_FLAG_USE_MASK`).
pub(crate) const VIRTGPU_BLOB_FLAG_USE_MASK: u32 = VIRTGPU_BLOB_FLAG_USE_MAPPABLE
    | VIRTGPU_BLOB_FLAG_USE_SHAREABLE
    | VIRTGPU_BLOB_FLAG_USE_CROSS_DEVICE;

/// `struct drm_virtgpu_map`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuMap {
    pub offset: u64,
    pub handle: u32,
    pub abi_pad: u32,
}

/// `struct drm_virtgpu_execbuffer_syncobj`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuExecbufferSyncobj {
    pub handle: u32,
    pub flags: u32,
    pub point: u64,
}

/// `struct drm_virtgpu_execbuffer`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuExecbuffer {
    pub flags: u32,
    pub size: u32,
    pub command: u64,
    pub bo_handles: u64,
    pub num_bo_handles: u32,
    pub fence_fd: i32,
    pub ring_idx: u32,
    pub syncobj_stride: u32,
    pub num_in_syncobjs: u32,
    pub num_out_syncobjs: u32,
    pub in_syncobjs: u64,
    pub out_syncobjs: u64,
}

/// `struct drm_virtgpu_getparam`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuGetparam {
    pub param: u64,
    pub value: u64,
}

/// `struct drm_virtgpu_resource_create`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuResourceCreate {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u32,
    pub stride: u32,
}

/// `struct drm_virtgpu_resource_info`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuResourceInfo {
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u32,
    pub blob_mem: u32,
}

/// `struct drm_virtgpu_3d_box`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpu3dBox {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

/// `struct drm_virtgpu_3d_transfer_to_host`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpu3dTransferToHost {
    pub bo_handle: u32,
    pub box_: DrmVirtgpu3dBox,
    pub level: u32,
    pub offset: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

/// `struct drm_virtgpu_3d_transfer_from_host`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpu3dTransferFromHost {
    pub bo_handle: u32,
    pub box_: DrmVirtgpu3dBox,
    pub level: u32,
    pub offset: u32,
    pub stride: u32,
    pub layer_stride: u32,
}

/// `struct drm_virtgpu_3d_wait`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpu3dWait {
    pub handle: u32,
    pub flags: u32,
}

/// `VIRTGPU_WAIT_NOWAIT`.
pub(crate) const VIRTGPU_WAIT_NOWAIT: u32 = 1;

/// `struct drm_virtgpu_get_caps`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuGetCaps {
    pub cap_set_id: u32,
    pub cap_set_ver: u32,
    pub addr: u64,
    pub size: u32,
    pub abi_pad: u32,
}

/// `struct drm_virtgpu_resource_create_blob`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuResourceCreateBlob {
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub bo_handle: u32,
    pub res_handle: u32,
    pub size: u64,
    pub abi_pad: u32,
    pub cmd_size: u32,
    pub cmd: u64,
    pub blob_id: u64,
}

/// `struct drm_virtgpu_context_set_param`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuContextSetParam {
    pub param: u64,
    pub value: u64,
}

/// `struct drm_virtgpu_context_init`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmVirtgpuContextInit {
    pub num_params: u32,
    pub abi_pad: u32,
    pub ctx_set_params: u64,
}
