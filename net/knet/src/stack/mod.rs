// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network stack orchestration around smoltcp.

pub(crate) mod device;
pub(crate) mod ingress;
pub(crate) mod router;
pub(crate) mod socket_set;
pub(crate) mod time;
