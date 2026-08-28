// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Encoding for ktask-local worker-pool execution identity.

use kcpu_id_map::LogicalCpuId;

use crate::{PoolId, PoolKind, WorkerExecutionToken, WorkerId};

const CONTEXT_POOL_KIND: usize = 0;
const CONTEXT_CPU_ID: usize = 1;
const CONTEXT_WORKER_ID: usize = 2;
const CONTEXT_TOKEN: usize = 3;

/// Decoded worker-pool execution identity stored in a ktask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerPoolExecutionContext {
    pub pool_id: PoolId,
    pub worker: WorkerId,
    pub token: WorkerExecutionToken,
}

/// Encodes worker-pool execution identity into ktask's opaque task context.
pub fn encode_task_context(
    pool_id: PoolId,
    worker: WorkerId,
    token: WorkerExecutionToken,
) -> ktask::TaskExecutionContext {
    ktask::TaskExecutionContext::new([
        pool_id.kind().as_usize(),
        pool_id.cpu().as_usize(),
        worker.as_usize(),
        token.as_usize(),
    ])
}

/// Decodes ktask's opaque task context back into worker-pool identity.
pub fn decode_task_context(
    context: ktask::TaskExecutionContext,
) -> Option<WorkerPoolExecutionContext> {
    let words = context.words();
    Some(WorkerPoolExecutionContext {
        pool_id: PoolId::new(
            PoolKind::new(words[CONTEXT_POOL_KIND]),
            LogicalCpuId::new(words[CONTEXT_CPU_ID]),
        ),
        worker: WorkerId::new(words[CONTEXT_WORKER_ID]),
        token: WorkerExecutionToken::from_usize(words[CONTEXT_TOKEN]),
    })
}
