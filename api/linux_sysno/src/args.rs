// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raw register argument packing for [`SyscallArgs`].

/// Six raw syscall argument registers in ABI order.
///
/// Array conversions with fewer than six elements zero the unused registers.
/// Pointer-like values are not validated or dereferenced; syscall adapters must
/// interpret each value using the selected syscall's ABI.
#[derive(PartialEq, Debug, Eq, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SyscallArgs {
    /// Raw argument register 0; interpretation depends on the syscall.
    pub arg0: usize,
    /// Raw argument register 1; interpretation depends on the syscall.
    pub arg1: usize,
    /// Raw argument register 2; interpretation depends on the syscall.
    pub arg2: usize,
    /// Raw argument register 3; interpretation depends on the syscall.
    pub arg3: usize,
    /// Raw argument register 4; interpretation depends on the syscall.
    pub arg4: usize,
    /// Raw argument register 5; interpretation depends on the syscall.
    pub arg5: usize,
}

impl SyscallArgs {
    /// Packs `a0` through `a5` in register order without validation.
    pub fn new(a0: usize, a1: usize, a2: usize, a3: usize, a4: usize, a5: usize) -> Self {
        SyscallArgs {
            arg0: a0,
            arg1: a1,
            arg2: a2,
            arg3: a3,
            arg4: a4,
            arg5: a5,
        }
    }
}

impl From<&[usize; 6]> for SyscallArgs {
    fn from(args: &[usize; 6]) -> Self {
        SyscallArgs {
            arg0: args[0],
            arg1: args[1],
            arg2: args[2],
            arg3: args[3],
            arg4: args[4],
            arg5: args[5],
        }
    }
}

impl From<&[usize; 5]> for SyscallArgs {
    fn from(args: &[usize; 5]) -> Self {
        SyscallArgs {
            arg0: args[0],
            arg1: args[1],
            arg2: args[2],
            arg3: args[3],
            arg4: args[4],
            arg5: 0,
        }
    }
}

impl From<&[usize; 4]> for SyscallArgs {
    fn from(args: &[usize; 4]) -> Self {
        SyscallArgs {
            arg0: args[0],
            arg1: args[1],
            arg2: args[2],
            arg3: args[3],
            arg4: 0,
            arg5: 0,
        }
    }
}

impl From<&[usize; 3]> for SyscallArgs {
    fn from(args: &[usize; 3]) -> Self {
        SyscallArgs {
            arg0: args[0],
            arg1: args[1],
            arg2: args[2],
            arg3: 0,
            arg4: 0,
            arg5: 0,
        }
    }
}

impl From<&[usize; 2]> for SyscallArgs {
    fn from(args: &[usize; 2]) -> Self {
        SyscallArgs {
            arg0: args[0],
            arg1: args[1],
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
        }
    }
}

impl From<&[usize; 1]> for SyscallArgs {
    fn from(args: &[usize; 1]) -> Self {
        SyscallArgs {
            arg0: args[0],
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
        }
    }
}

impl From<&[usize; 0]> for SyscallArgs {
    fn from(_args: &[usize; 0]) -> Self {
        SyscallArgs {
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
        }
    }
}

/// Packs zero through six `usize` expressions into [`SyscallArgs`].
///
/// Arguments stay in ABI register order and omitted registers are zeroed.
/// Expressions are evaluated once; this macro does not execute a syscall.
///
/// # Examples
///
/// ```
/// use linux_sysno::syscall_args;
/// let args = syscall_args!(7, 16);
/// assert_eq!(args.arg0, 7);
/// assert_eq!(args.arg1, 16);
/// assert_eq!(args.arg5, 0);
/// ```
#[macro_export]
macro_rules! syscall_args {
    ($a:expr, $b:expr, $c:expr, $d:expr, $e:expr, $f:expr) => {
        $crate::SyscallArgs::new($a, $b, $c, $d, $e, $f)
    };
    ($a:expr, $b:expr, $c:expr, $d:expr, $e:expr) => {
        $crate::SyscallArgs::new($a, $b, $c, $d, $e, 0)
    };
    ($a:expr, $b:expr, $c:expr, $d:expr) => {
        $crate::SyscallArgs::new($a, $b, $c, $d, 0, 0)
    };
    ($a:expr, $b:expr, $c:expr) => {
        $crate::SyscallArgs::new($a, $b, $c, 0, 0, 0)
    };
    ($a:expr, $b:expr) => {
        $crate::SyscallArgs::new($a, $b, 0, 0, 0, 0)
    };
    ($a:expr) => {
        $crate::SyscallArgs::new($a, 0, 0, 0, 0, 0)
    };
    () => {
        $crate::SyscallArgs::new(0, 0, 0, 0, 0, 0)
    };
}

unittest::path_mod_test!(tests, "../ut-tests/args.rs");
