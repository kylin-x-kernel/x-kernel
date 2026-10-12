// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live classes for this layer: Deadline, RT, Fair, then Idle.
//!
//! Stop keeps its rank on the arbitrator and has no class module yet.

pub(crate) mod deadline;
pub(crate) mod fair;
pub(crate) mod idle;
pub(crate) mod rt;

pub(crate) use deadline::DeadlineClass;
pub(crate) use fair::FairClass;
pub(crate) use idle::IdleClass;
pub(crate) use rt::RtClass;
