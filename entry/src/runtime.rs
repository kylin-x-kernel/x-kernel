// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Entry-side runtime orchestration and unittest glue.

/// Initialize VFS mounts and the alarm task.
pub fn init_runtime() {
    crate::bootstrap::init_virtual_filesystems();
    crate::bootstrap::init_alarm_runtime();
}

#[cfg(feature = "unittest")]
mod unittest_runtime {
    use kprocess::Thread;

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    fn init_unittest_tee_context(thread: &Thread) {
        if kbuild_config::KFEAT_TEE {
            tee_kernel::tee::set_tee_session_ctx(thread);
        }
    }

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    fn init_unittest_tee_context(_: &Thread) {}

    pub fn register_unittest_runtime() {
        unittest_support::register_unittest_runtime(init_unittest_tee_context);
    }
}

#[cfg(feature = "unittest")]
pub use unittest_runtime::register_unittest_runtime;
