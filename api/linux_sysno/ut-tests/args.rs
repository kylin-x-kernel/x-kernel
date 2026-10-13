// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn syscall_args_macro_test() {
    assert_eq!(
        syscall_args!(1, 2, 3, 4, 5, 6),
        SyscallArgs::new(1, 2, 3, 4, 5, 6)
    );
    assert_eq!(
        syscall_args!(1, 2, 3, 4, 5),
        SyscallArgs::new(1, 2, 3, 4, 5, 0)
    );
    assert_eq!(
        syscall_args!(1, 2, 3, 4),
        SyscallArgs::new(1, 2, 3, 4, 0, 0)
    );
    assert_eq!(syscall_args!(1, 2, 3), SyscallArgs::new(1, 2, 3, 0, 0, 0));
    assert_eq!(syscall_args!(1, 2), SyscallArgs::new(1, 2, 0, 0, 0, 0));
    assert_eq!(syscall_args!(1), SyscallArgs::new(1, 0, 0, 0, 0, 0));
    assert_eq!(syscall_args!(), SyscallArgs::new(0, 0, 0, 0, 0, 0));
}

#[def_test]
fn syscall_args_from_u64_slice() {
    assert_eq!(
        SyscallArgs::from(&[1, 2, 3, 4, 5, 6]),
        syscall_args!(1, 2, 3, 4, 5, 6)
    );
    assert_eq!(
        SyscallArgs::from(&[1, 2, 3, 4, 5]),
        syscall_args!(1, 2, 3, 4, 5)
    );
    assert_eq!(SyscallArgs::from(&[1, 2, 3, 4]), syscall_args!(1, 2, 3, 4));
    assert_eq!(SyscallArgs::from(&[1, 2, 3]), syscall_args!(1, 2, 3));
    assert_eq!(SyscallArgs::from(&[1, 2]), syscall_args!(1, 2));
    assert_eq!(SyscallArgs::from(&[1]), syscall_args!(1));
    assert_eq!(SyscallArgs::from(&[0]), syscall_args!());
}
