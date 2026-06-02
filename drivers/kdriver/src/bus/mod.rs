// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus discovery backends (execution layer).
//!
//! This module contains concrete discovery implementations and probing glue.
//! Persistent metadata types (`BusId`, `BusTypeId`, `BusInfo`) stay in
//! `kdevice` as part of the shared model layer.

pub mod backend;
mod local_id;
pub mod manager;
pub mod pci_backend;
pub mod pci_support;
pub mod platform_backend;
