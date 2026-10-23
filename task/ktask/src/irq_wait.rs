// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KIRQ synchronization wait provider.

use kcompletion::Completion;

#[kiface::provide]
impl kirq::IrqSyncWaitIf {
    fn can_wait() -> bool {
        crate::is_sleepable_context()
    }

    fn wait_for_completion(completion: &Completion) -> Result<(), kirq::IrqDescError> {
        crate::completion::wait_for_completion(completion).map_err(|_| {
            kirq::IrqDescError::InvalidContext {
                operation: "wait_for_completion",
            }
        })
    }
}

unittest::path_mod_test!(tests, "../ut-tests/irq_wait/tests.rs");
