// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live classes for this layer: Fair then Idle.
//!
//! Stop, Deadline, and RT keep their rank on the arbitrator and have no
//! class module yet.

pub(crate) mod fair;
pub(crate) mod idle;

pub(crate) use fair::FairClass;
pub(crate) use idle::IdleClass;
