// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live classes for this layer: RT, Fair, then Idle.
//!
//! Stop and Deadline keep their rank on the arbitrator and have no class
//! module yet.

pub(crate) mod fair;
pub(crate) mod idle;
pub(crate) mod rt;

pub(crate) use fair::FairClass;
pub(crate) use idle::IdleClass;
pub(crate) use rt::RtClass;
