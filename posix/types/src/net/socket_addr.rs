// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing socket address structures.

use linux_raw_sys::net::{__kernel_sa_family_t, sockaddr_in, sockaddr_in6};

use crate::UserRead;

// SAFETY: these socket-address structs are POD syscall carriers with no extra
// validity invariants beyond their raw bytes.
unsafe impl UserRead for sockaddr_in {}
// SAFETY: these socket-address structs are POD syscall carriers with no extra
// validity invariants beyond their raw bytes.
unsafe impl UserRead for sockaddr_in6 {}

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Copy, Clone, UserRead)]
pub struct sockaddr_nl {
    pub nl_family: __kernel_sa_family_t,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}

// This type should be provided by `linux_raw_sys` but it's missing.
// See <https://github.com/sunfishcode/linux-raw-sys/issues/169>.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Copy, Clone, UserRead)]
pub struct sockaddr_vm {
    pub svm_family: __kernel_sa_family_t,
    pub svm_reserved1: u16,
    pub svm_port: u32,
    pub svm_cid: u32,
    pub svm_zero: [u8; 4],
}
