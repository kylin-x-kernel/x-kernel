// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live classes: Stop, Deadline, RT, Fair, then Idle.

pub(crate) mod deadline;
pub(crate) mod fair;
pub(crate) mod idle;
pub(crate) mod rt;
pub(crate) mod stop;

pub(crate) use deadline::DeadlineClass;
pub(crate) use fair::FairClass;
pub(crate) use idle::IdleClass;
pub(crate) use rt::RtClass;
pub(crate) use stop::StopClass;
