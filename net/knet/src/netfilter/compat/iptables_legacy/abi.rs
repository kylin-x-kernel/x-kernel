// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native 64-bit Linux legacy IPv4 iptables ABI layouts.
//!
//! Mirrors Linux 6.8 `ip_tables.h`, `x_tables.h`, and `xt_tcpudp.h`.
//! Integers use native byte order; IPv4 addresses use network byte order.
//!
//! Local definitions support `zerocopy` and explicitly initialize padding.
//! Layouts are checked against `linux_raw_sys` bindings where available.

use core::mem::{align_of, offset_of, size_of};

use linux_raw_sys::net as linux;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub(super) const IPV4_ADDRESS_SIZE: usize = size_of::<u32>();
pub(super) const IFNAMSIZ: usize = linux::IFNAMSIZ as usize;
const XT_EXTENSION_MAXNAMELEN: usize = linux::XT_EXTENSION_MAXNAMELEN as usize;
const XT_FUNCTION_MAXNAMELEN: usize = linux::XT_FUNCTION_MAXNAMELEN as usize;
pub(super) const XT_ERROR_TARGET: &[u8] = linux::XT_ERROR_TARGET;

pub(super) const IPT_INV_VIA_IN: u8 = 0x01;
pub(super) const IPT_INV_VIA_OUT: u8 = 0x02;
pub(super) const IPT_INV_SRCIP: u8 = 0x08;
pub(super) const IPT_INV_DSTIP: u8 = 0x10;
pub(super) const IPT_INV_PROTO: u8 = linux::XT_INV_PROTO as u8;
pub(super) const XT_TCP_INV_SRCPT: u8 = linux::XT_TCP_INV_SRCPT as u8;
pub(super) const XT_TCP_INV_DSTPT: u8 = linux::XT_TCP_INV_DSTPT as u8;
pub(super) const XT_TCP_INV_FLAGS: u8 = linux::XT_TCP_INV_FLAGS as u8;
pub(super) const XT_UDP_INV_SRCPT: u8 = linux::XT_UDP_INV_SRCPT as u8;
pub(super) const XT_UDP_INV_DSTPT: u8 = linux::XT_UDP_INV_DSTPT as u8;
pub(super) const IPT_ICMP_INV: u8 = 0x01;

// Linux standard targets encode terminal verdicts as -NF_* - 1.
pub(super) const XT_VERDICT_DROP: i32 = -(linux::NF_DROP as i32) - 1;
pub(super) const XT_VERDICT_ACCEPT: i32 = -(linux::NF_ACCEPT as i32) - 1;
pub(super) const XT_RETURN: i32 = linux::XT_RETURN;

#[repr(C, align(4))]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct IptIp {
    pub(super) source: [u8; IPV4_ADDRESS_SIZE],
    pub(super) destination: [u8; IPV4_ADDRESS_SIZE],
    pub(super) source_mask: [u8; IPV4_ADDRESS_SIZE],
    pub(super) destination_mask: [u8; IPV4_ADDRESS_SIZE],
    pub(super) input_interface: [u8; IFNAMSIZ],
    pub(super) output_interface: [u8; IFNAMSIZ],
    pub(super) input_interface_mask: [u8; IFNAMSIZ],
    pub(super) output_interface_mask: [u8; IFNAMSIZ],
    pub(super) protocol: u16,
    pub(super) flags: u8,
    pub(super) inverse_flags: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtCounters {
    pub(super) packets: u64,
    pub(super) bytes: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct IptEntry {
    pub(super) ip: IptIp,
    pub(super) nfcache: u32,
    pub(super) target_offset: u16,
    pub(super) next_offset: u16,
    pub(super) comefrom: u32,
    pub(super) counters: XtCounters,
}

// The userspace member of the native match/target union occupies 32 bytes;
// its kernel pointer member gives that union 8-byte alignment.
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtEntryHeader {
    pub(super) size: u16,
    pub(super) name: [u8; XT_EXTENSION_MAXNAMELEN],
    pub(super) revision: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtStandardTarget {
    pub(super) header: XtEntryHeader,
    pub(super) verdict: i32,
    pub(super) padding: [u8; XT_STANDARD_TARGET_PADDING],
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtErrorTarget {
    pub(super) header: XtEntryHeader,
    pub(super) name: [u8; XT_FUNCTION_MAXNAMELEN],
    pub(super) padding: [u8; XT_ERROR_TARGET_PADDING],
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtTcp {
    pub(super) source_ports: [u16; 2],
    pub(super) destination_ports: [u16; 2],
    pub(super) option: u8,
    pub(super) flags_mask: u8,
    pub(super) flags_compare: u8,
    pub(super) inverse_flags: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct XtUdp {
    pub(super) source_ports: [u16; 2],
    pub(super) destination_ports: [u16; 2],
    pub(super) inverse_flags: u8,
    pub(super) padding: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(super) struct IptIcmp {
    pub(super) icmp_type: u8,
    pub(super) code: [u8; 2],
    pub(super) inverse_flags: u8,
}

pub(super) const XT_ALIGN: usize = align_of::<linux::_xt_align>();
const XT_STANDARD_TARGET_PADDING: usize =
    size_of::<linux::xt_standard_target>() - size_of::<XtEntryHeader>() - size_of::<i32>();
const XT_ERROR_TARGET_PADDING: usize =
    size_of::<linux::xt_error_target>() - size_of::<XtEntryHeader>() - XT_FUNCTION_MAXNAMELEN;
pub(super) const IPT_ENTRY_SIZE: usize = size_of::<IptEntry>();
pub(super) const XT_ENTRY_HEADER_SIZE: usize = size_of::<XtEntryHeader>();
pub(super) const XT_STANDARD_TARGET_SIZE: usize = size_of::<XtStandardTarget>();
pub(super) const XT_ERROR_TARGET_SIZE: usize = size_of::<XtErrorTarget>();
pub(super) const XT_TCP_MATCH_SIZE: usize = align_up(XT_ENTRY_HEADER_SIZE + size_of::<XtTcp>());
pub(super) const XT_UDP_MATCH_SIZE: usize = align_up(XT_ENTRY_HEADER_SIZE + size_of::<XtUdp>());
pub(super) const IPT_ICMP_MATCH_SIZE: usize = align_up(XT_ENTRY_HEADER_SIZE + size_of::<IptIcmp>());
pub(super) const STANDARD_ENTRY_SIZE: usize = IPT_ENTRY_SIZE + XT_STANDARD_TARGET_SIZE;
pub(super) const ERROR_ENTRY_SIZE: usize = IPT_ENTRY_SIZE + XT_ERROR_TARGET_SIZE;

pub(super) const fn align_up(size: usize) -> usize {
    (size + XT_ALIGN - 1) & !(XT_ALIGN - 1)
}

const _: () = {
    assert!(size_of::<usize>() == 8);
    assert!(size_of::<IptIp>() == 84);
    assert!(align_of::<IptIp>() == 4);
    assert!(offset_of!(IptIp, protocol) == 80);
    assert!(size_of::<XtCounters>() == size_of::<linux::xt_counters>());
    assert!(align_of::<XtCounters>() == align_of::<linux::xt_counters>());
    assert!(IPT_ENTRY_SIZE == 112);
    assert!(align_of::<IptEntry>() == XT_ALIGN);
    assert!(offset_of!(IptEntry, target_offset) == 88);
    assert!(offset_of!(IptEntry, next_offset) == 90);
    assert!(offset_of!(IptEntry, counters) == 96);
    assert!(XT_ENTRY_HEADER_SIZE == size_of::<linux::xt_entry_match>());
    assert!(XT_ENTRY_HEADER_SIZE == size_of::<linux::xt_entry_target>());
    assert!(align_of::<XtEntryHeader>() == align_of::<linux::xt_entry_match>());
    assert!(align_of::<XtEntryHeader>() == align_of::<linux::xt_entry_target>());
    assert!(offset_of!(XtEntryHeader, name) == offset_of!(linux::xt_entry_match, u.user.name));
    assert!(
        offset_of!(XtEntryHeader, revision) == offset_of!(linux::xt_entry_match, u.user.revision)
    );
    assert!(XT_STANDARD_TARGET_SIZE == size_of::<linux::xt_standard_target>());
    assert!(align_of::<XtStandardTarget>() == align_of::<linux::xt_standard_target>());
    assert!(
        offset_of!(XtStandardTarget, verdict) == offset_of!(linux::xt_standard_target, verdict)
    );
    assert!(XT_ERROR_TARGET_SIZE == size_of::<linux::xt_error_target>());
    assert!(align_of::<XtErrorTarget>() == align_of::<linux::xt_error_target>());
    assert!(offset_of!(XtErrorTarget, name) == offset_of!(linux::xt_error_target, errorname));
    assert!(size_of::<XtTcp>() == size_of::<linux::xt_tcp>());
    assert!(align_of::<XtTcp>() == align_of::<linux::xt_tcp>());
    assert!(size_of::<XtUdp>() == size_of::<linux::xt_udp>());
    assert!(align_of::<XtUdp>() == align_of::<linux::xt_udp>());
    assert!(size_of::<IptIcmp>() == 4);
};
