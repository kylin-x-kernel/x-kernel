// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network runtime service, polling, and queue ownership.

pub(crate) mod poller;
pub(crate) mod service;
pub(crate) mod tx_queue;
