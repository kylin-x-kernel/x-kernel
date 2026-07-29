// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network stack orchestration around smoltcp.

use smoltcp::wire::IpAddress as SmoltcpIpAddress;

use crate::ip::IpAddress;

pub(crate) mod fragment;
pub(crate) mod ipv4;
pub(crate) mod listen_table;
pub(crate) mod router;
pub(crate) mod service;
pub(crate) mod wrapper;

fn from_smoltcp_ip_address(addr: SmoltcpIpAddress) -> IpAddress {
    match addr {
        SmoltcpIpAddress::Ipv4(addr) => IpAddress::Ipv4(addr.into()),
        SmoltcpIpAddress::Ipv6(addr) => IpAddress::Ipv6(addr.into()),
    }
}

fn to_smoltcp_ip_address(addr: IpAddress) -> SmoltcpIpAddress {
    match addr {
        IpAddress::Ipv4(addr) => SmoltcpIpAddress::Ipv4(addr.into()),
        IpAddress::Ipv6(addr) => SmoltcpIpAddress::Ipv6(addr.into()),
    }
}
