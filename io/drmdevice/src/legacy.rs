// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The commands Linux answers without doing anything (`drm_noop`,
//! `drm_invalid_op`).
//!
//! They exist so that a legacy client keeps running against a modern card:
//! `BLOCK`, `UNBLOCK`, the drawable commands and `FINISH` succeed as no-ops,
//! exactly as Linux's table routes them to `drm_noop`; `SET_UNIQUE` fails with
//! `EINVAL` like every `drm_invalid_op` entry; the two dead mode commands are
//! answered. The `ROOT_ONLY` admission Linux layers on some of them is
//! satisfied by construction here: this kernel only exposes the card to the
//! root user, which is the capability the bit checks.

use alloc::sync::Arc;

use macros::{UserRead, UserWrite};

use crate::prelude::*;

/// `DRM_IOCTL_SET_UNIQUE`: a distinct wrapper — `GET_UNIQUE` owns the wire
/// struct's one impl, and this is `drm_invalid_op` (`EINVAL`).
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, bytemuck::AnyBitPattern, UserRead, UserWrite)]
pub(crate) struct DrmSetUnique(pub(crate) DrmUnique);

impl Ioctl for DrmSetUnique {
    const NAME: &'static str = "SET_UNIQUE";

    fn handle(
        _dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        _request: &mut Self,
    ) -> DrmResult<usize> {
        Err(DrmError::InvalidInput)
    }
}

/// The `drm_noop` commands: one impl serves every number the table points at
/// it (`BLOCK` and `UNBLOCK` share `DrmBlock`, the drawable pair shares
/// `DrmDraw`).
macro_rules! noop_ioctl {
    ($ty:ty, $name:literal) => {
        impl Ioctl for $ty {
            const NAME: &'static str = $name;

            fn handle(
                _dev: &DrmDevice,
                _file: &Arc<DrmFile>,
                _unit: u32,
                _request: &mut Self,
            ) -> DrmResult<usize> {
                Ok(0)
            }
        }
    };
}

noop_ioctl!(DrmBlock, "BLOCK");
noop_ioctl!(DrmDraw, "ADD_DRAW");
noop_ioctl!(DrmUpdateDraw, "UPDATE_DRAW");
noop_ioctl!(DrmLock, "FINISH");
noop_ioctl!(DrmModeModeCmd, "ATTACHMODE");
