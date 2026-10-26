// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The unix `getsockname` layout must match Linux byte for byte.
//!
//! Linux leads the address with `__kernel_sa_family_t` (an `unsigned short`,
//! two bytes) followed by `sun_path`. Emitting the `u32` `AF_UNIX` constant
//! instead shifts `sun_path` by two bytes, so user space reads an empty
//! address: Go's `net.ListenUnix` reports `"@"` from `Addr()` and dials that
//! empty abstract name instead of the bound path, which makes its accept pair
//! never connect.

use knet::unix::UnixAddr;
use linux_raw_sys::net::AF_UNIX;
use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn unix_path_address_layout_matches_linux() {
    let bytes = unix_addr_bytes(&UnixAddr::Path("/tmp/go-nettest".into()));

    assert_eq!(bytes.len(), 2 + "/tmp/go-nettest".len() + 1);
    assert_eq!(&bytes[..2], &(AF_UNIX as u16).to_ne_bytes());
    assert_eq!(&bytes[2..], b"/tmp/go-nettest\0");
}

#[def_test]
fn unix_abstract_address_keeps_the_leading_nul() {
    let bytes = unix_addr_bytes(&UnixAddr::Abstract(b"box".as_slice().into()));

    assert_eq!(bytes.len(), 2 + 1 + 3);
    assert_eq!(&bytes[..2], &(AF_UNIX as u16).to_ne_bytes());
    assert_eq!(&bytes[2..], b"\0box");
}

#[def_test]
fn unix_unbound_address_is_family_only() {
    let bytes = unix_addr_bytes(&UnixAddr::Unbound);

    assert_eq!(bytes.len(), 2);
    assert_eq!(&bytes, &(AF_UNIX as u16).to_ne_bytes());
}