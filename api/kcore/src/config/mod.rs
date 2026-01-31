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

/// Unit tests.
#[cfg(unittest)]
pub mod tests_config {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_user_space_range() {
        assert!(USER_SPACE_SIZE > 0);
        assert!(USER_SPACE_BASE < USER_SPACE_BASE + USER_SPACE_SIZE);
    }

    #[def_test]
    fn test_user_stack_range() {
        assert!(USER_STACK_SIZE > 0);
        assert!(USER_STACK_TOP > USER_STACK_SIZE);
    }

    #[def_test]
    fn test_heap_limits() {
        assert!(USER_HEAP_SIZE > 0);
        assert!(USER_HEAP_SIZE_MAX >= USER_HEAP_SIZE);
    }
}
