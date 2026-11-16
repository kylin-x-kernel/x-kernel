// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KMS objects, properties and commits (`drm_crtc.c` and its siblings).
//!
//! There is one KMS state, one property table and one commit path:
//!
//! * [`KmsState`] is plain data. Legacy `SETCRTC` builds one and commits it
//!   through the same entry point `MODE_ATOMIC` uses, so a legacy modeset is
//!   visible to atomic clients and vice versa.
//! * [`PROPS`] is the only description of a property: id, wire flags, the value
//!   reader and the optional writer. `OBJ_GETPROPERTIES`, `GETPROPERTY` and the
//!   write path all consume the same table, so a property cannot be readable but
//!   unwritable, or report metadata that disagrees with its writer.
//! * [`DrmDevice::commit`] validates, publishes the state and presents the
//!   resulting scanout exactly once.
//!
//! The submodules follow the Linux source split: `state` owns the committed
//! state and its mode-blob codec, `props` the property table and the property
//! ioctls, `modeset` the query/modeset/atomic ioctls, `cursor` the legacy
//! cursor, and `vblank` `WAIT_VBLANK`.

mod cursor;
mod modeset;
mod props;
mod state;
mod vblank;

#[cfg(unittest)]
pub(crate) use modeset::current_mode;
pub(crate) use modeset::{DrmModeSetCrtc, DrmModeSetGamma};
pub(crate) use props::property;
#[cfg(unittest)]
pub(crate) use props::{PROPS, PropKind};
pub(crate) use state::{
    KmsState, ObjectType, apply, atomic_flags_valid, mode_blob_bytes, mode_from_bytes,
};
#[cfg(unittest)]
pub(crate) use vblank::{wait_vblank_crtc_valid, wait_vblank_flags_valid};
