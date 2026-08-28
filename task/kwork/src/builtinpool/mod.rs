// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in system worker pools owned by `kwork`.

mod cpu;
mod entry;
mod kind;
mod pool;
mod system;

pub use cpu::{BuiltinBhPoolInitResult, BuiltinCpuPoolInitResult, BuiltinWorkerPoolInitResult};
pub(crate) use entry::{executor_entry, pool_owner};
pub(crate) use kind::SystemPoolKind;
pub(crate) use pool::{
    BuiltinPoolEnqueueError, BuiltinPoolRuntime, SystemPoolBinding, handle_actions,
};
pub(crate) use system::{
    account_system_execution_blocked, account_system_execution_resumed,
    account_system_execution_tick, init_system_worker_pools_for_cpu, is_system_worker_pool_ready,
    system_execution_tick_deadline, system_pool_for_cpu, system_pool_for_kind_cpu, system_pools,
};
