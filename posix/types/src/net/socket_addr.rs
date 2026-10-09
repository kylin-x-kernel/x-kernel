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

/// Linux netlink socket address; semantic validation belongs to the network adapter.
#[repr(C)]
#[derive(Copy, Clone, UserRead)]
pub struct sockaddr_nl {
    /// Address family; consumers must require `AF_NETLINK`.
    pub nl_family: __kernel_sa_family_t,
    /// Reserved ABI padding.
    pub nl_pad: u16,
    /// Netlink port identifier.
    pub nl_pid: u32,
    /// Multicast group membership bit mask.
    pub nl_groups: u32,
}

// This type should be provided by `linux_raw_sys` but it's missing.
// See <https://github.com/sunfishcode/linux-raw-sys/issues/169>.
/// Linux virtual socket address with explicit reserved bytes.
#[repr(C)]
#[derive(Copy, Clone, UserRead)]
pub struct sockaddr_vm {
    /// Address family; consumers must require `AF_VSOCK`.
    pub svm_family: __kernel_sa_family_t,
    /// Reserved ABI field.
    pub svm_reserved1: u16,
    /// Virtual socket port number.
    pub svm_port: u32,
    /// Virtual socket context identifier.
    pub svm_cid: u32,
    /// Reserved trailing bytes.
    pub svm_zero: [u8; 4],
}
