// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-specific configurations.

cfg_if::cfg_if! {
    if #[cfg(target_arch = "riscv64")] {
    #[rustfmt::skip]
        mod riscv64;
        pub use riscv64::*;
    } else if #[cfg(target_arch = "loongarch64")] {
        #[rustfmt::skip]
        mod loongarch64;
        pub use loongarch64::*;
    } else if #[cfg(target_arch = "x86_64")] {
        #[rustfmt::skip]
        mod x86_64;
        pub use x86_64::*;
    } else if #[cfg(target_arch = "aarch64")] {
        #[rustfmt::skip]
        mod aarch64;
        pub use aarch64::*;
    } else {
        compile_error!("Unsupported architecture");
    }
}

#[cfg(feature = "kcore_test")]
pub mod tests_config {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;

    test_fn! {
        using TestResult;

        fn test_user_space_range() {
            assert!(USER_SPACE_SIZE > 0);
            assert!(USER_SPACE_BASE < USER_SPACE_BASE + USER_SPACE_SIZE);
        }
    }

    test_fn! {
        using TestResult;

        fn test_user_stack_range() {
            assert!(USER_STACK_SIZE > 0);
            assert!(USER_STACK_TOP > USER_STACK_SIZE);
        }
    }

    test_fn! {
        using TestResult;

        fn test_heap_limits() {
            assert!(USER_HEAP_SIZE > 0);
            assert!(USER_HEAP_SIZE_MAX >= USER_HEAP_SIZE);
        }
    }

    tests_name! {
        TEST_CONFIG;
        test_user_space_range,
        test_user_stack_range,
        test_heap_limits,
    }
}
