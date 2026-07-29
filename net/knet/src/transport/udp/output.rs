// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ::core::net::Ipv4Addr;
use etherparse::UdpHeader;
use kerrno::{KError, KResult, LinuxError};

use super::UDP_HEADER_LEN;
use crate::ip::{IpAddress, Ipv4Address};

pub(super) fn write_udp_header(
    udp_packet: &mut [u8],
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    source_port: u16,
    destination_port: u16,
) -> Option<()> {
    let payload = udp_packet.get(UDP_HEADER_LEN..)?;
    let Ok(mut header) =
        UdpHeader::without_ipv4_checksum(source_port, destination_port, payload.len())
    else {
        return None;
    };
    let Ok(checksum) = header.calc_checksum_ipv4_raw(src_addr.octets(), dst_addr.octets(), payload)
    else {
        return None;
    };
    header.checksum = checksum;
    udp_packet
        .get_mut(..UDP_HEADER_LEN)?
        .copy_from_slice(&header.to_bytes());
    Some(())
}

pub(super) fn ipv4_pair(src: IpAddress, dst: IpAddress) -> KResult<(Ipv4Address, Ipv4Address)> {
    match (src, dst) {
        (IpAddress::Ipv4(src), IpAddress::Ipv4(dst)) => Ok((src, dst)),
        _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
    }
}

pub(super) fn ipv4_to_core(addr: Ipv4Address) -> Ipv4Addr {
    let octets = addr.octets();
    Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])
}

pub(super) fn has_valid_udp_checksum(
    src_addr: Ipv4Address,
    dst_addr: Ipv4Address,
    udp_packet: &[u8],
) -> bool {
    let Ok((header, payload)) = UdpHeader::from_slice(udp_packet) else {
        return false;
    };
    header.checksum == 0
        || header
            .calc_checksum_ipv4_raw(src_addr.octets(), dst_addr.octets(), payload)
            .ok()
            == Some(header.checksum)
}

#[cfg(unittest)]
pub(super) fn read_u16_be(data: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([data[offset], data[offset + 1]])
}
