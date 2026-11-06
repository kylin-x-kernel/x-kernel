// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device model.
//!
//! The layers below `io/drmdevice` see this crate through [`device::DrmDevice`]
//! and [`device::minor::DrmMinor`] only. Everything here is independent of the
//! VFS: VFS-free code returns [`error::DrmError`], and the adapter in
//! `device/minor.rs` is the single place that converts it to `VfsError`.

pub(crate) mod backend;
pub(crate) mod device;
pub(crate) mod error;
pub(crate) mod event;
pub(crate) mod file;
pub(crate) mod ids;
pub(crate) mod scanout;
