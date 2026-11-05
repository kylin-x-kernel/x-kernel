// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DRM (Direct Rendering Manager) device subsystem.
//!
//! The crate is split along one boundary: [`model`] owns the card object, the
//! object registries, the committed KMS state and the ioctl table, and knows
//! nothing about VFS or a specific display driver; the subsystem modules
//! (`gem`, `framebuffer`, `kms`, `blob`, `auth`, `scanout`) implement one part
//! of the Linux DRM semantics each, named after the corresponding Linux source
//! file; `abi` is the wire description and nothing else.
//!
//! A board integrates by handing a display device to [`DrmDevice::new`] and
//! registering [`DrmDevice::minor`] for each minor it wants to expose. The
//! `model::backend` trait is the only hardware boundary, so the object model is
//! testable without a display.

#![no_std]

extern crate alloc;

pub(crate) mod abi;
mod auth;
mod blob;
mod framebuffer;
mod gem;
mod kms;
mod model;
mod scanout;

pub use model::device::{DrmDevice, HotplugSink, MinorKind};

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/lib.rs");

/// Names every module of this crate shares.
///
/// This is not a dumping ground: it is the crate's vocabulary. Allocation,
/// user-copy, VFS adapter, the wire ABI, the object model and the error domain
/// are visible everywhere; subsystem-specific items are imported explicitly.
pub(crate) mod prelude {
    pub(crate) use alloc::{
        sync::{Arc, Weak},
        vec::Vec,
    };

    pub(crate) use bytemuck::bytes_of;
    pub(crate) use display::{DisplayInfo, ScanoutFormat};
    pub(crate) use kalloc::{PageRef, UsageKind, alloc_zeroed_pages_exact_ref};
    pub(crate) use kclass::{ClassDevice, DisplayDeviceImpl};
    pub(crate) use kpoll::{IoEvents, PollContext, PollRegisterError};
    pub(crate) use ksync::Mutex;
    pub(crate) use kvfs::{
        DeviceFileOps, MmapMapper, NodeFlags, VfsError, VfsFile, VfsFileBuilder, VfsInode,
        VfsResult,
    };
    pub(crate) use memaddr::{PAGE_SIZE_4K, PhysAddrRange};
    pub(crate) use posix_types::{UserPtr, UserRead, UserWrite};

    pub(crate) use crate::{
        abi::*,
        auth::*,
        blob::{Blob, build_in_formats_blob},
        framebuffer::{DrmModeRmFb, Framebuffer},
        gem::{DrmPrimeFdToHandle, GemObject},
        kms::*,
        model::{
            backend::{BackendCaps, BackendHandle, ClassBackend, Rect, ResourceId, ResourceSpec},
            device::{DrmDevice, MinorKind},
            error::{DrmError, DrmResult},
            event::{EventQueue, EventReservation},
            file::{AuthState, DrmFile},
            ids::{BlobId, BlobTag, CrtcId, DumbHandle, FbId, FbTag, IdRegistry, PropertyId},
            ioctl::{Ioctl, report_user_array},
        },
        scanout::ScanoutSlot,
    };
}
