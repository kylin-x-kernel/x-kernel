// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The Linux DRM UAPI as this driver consumes it: wire structures, ioctl
//! numbers, capabilities, property ids and limits.
//!
//! Everything here is a pure description of the ABI. No device state and no
//! behaviour lives in this module, so a UAPI change is a change in exactly one
//! place.

mod consts;
mod structs;

pub(crate) use consts::*;
pub(crate) use structs::*;
