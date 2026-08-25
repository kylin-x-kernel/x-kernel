// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UDP socket implementation.

mod input;
mod output;
mod pcb;
mod registry;
mod socket;
mod state;
mod wait;

pub use self::socket::UdpSocket;
pub(crate) use self::{
    input::{InputDisposition, deliver_ipv4_packet, prepare_ipv4_packet},
    pcb::PreparedUdpPacket,
    registry::{init_udp_registry, lookup_udp_error_state},
    state::UdpSocketQueuedError,
};
#[cfg(unittest)]
pub(crate) use self::{
    registry::{clear_udp_registry_for_test, register_udp_state_for_test},
    state::UdpSocketState,
};

const IPV4_HEADER_LEN: usize = 20;
const IPV4_DEFAULT_TTL: u8 = 64;
const UDP_HEADER_LEN: usize = 8;
const UDP_RX_QUEUE_CAPACITY: usize = 1024;
const UDP_MAX_PAYLOAD_LEN: usize = u16::MAX as usize - IPV4_HEADER_LEN - UDP_HEADER_LEN;

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use ::core::net::{SocketAddr, SocketAddrV4};
    use unittest::def_test;

    use super::{
        output::{has_valid_udp_checksum, ipv4_to_core, read_u16_be, write_udp_header},
        pcb::RecvMode,
        registry::{
            bind_udp_auto_pcb_for_test, bind_udp_pcb, listen_endpoint, lookup_udp_pcb,
            udp_port_available,
        },
        *,
    };
    use crate::{
        RecvOptions, Shutdown, SocketAddrEx, SocketOps,
        buf::{PacketBuf, PacketOwner},
        device::{LoopbackDevice, NetDevice},
        ip::{IpAddress, IpEndpoint, Ipv4Address},
        ipv4,
    };

    fn endpoint(addr: Ipv4Address, port: u16) -> IpEndpoint {
        SocketAddrV4::new(ipv4_to_core(addr), port).into()
    }

    fn socket_addr(addr: Ipv4Address, port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(ipv4_to_core(addr), port))
    }

    fn clear_registry() {
        clear_udp_registry_for_test();
    }

    #[def_test]
    fn udp_checksum_accepts_zero_checksum() {
        let packet = [0, 1, 0, 2, 0, 8, 0, 0];
        assert!(has_valid_udp_checksum(
            Ipv4Address::new(192, 0, 2, 1),
            Ipv4Address::new(192, 0, 2, 2),
            &packet
        ));
    }

    #[def_test]
    fn ipv4_dont_fragment_header_sets_df() {
        let packet = ipv4::build_ipv4_packet_dont_fragment(
            Ipv4Address::new(192, 0, 2, 1),
            Ipv4Address::new(192, 0, 2, 2),
            ipv4::PROTOCOL_UDP,
            64,
            &[0; UDP_HEADER_LEN],
        )
        .expect("IPv4 packet length is valid");

        assert_eq!(read_u16_be(&packet, 6), 0x4000);
    }

    #[def_test]
    fn recv_after_read_shutdown_returns_eof() {
        let socket = UdpSocket::new();
        socket.state().shutdown(Shutdown::Read);

        let mut data = [0u8; 8];

        assert_eq!(socket.recv(&mut data[..], RecvOptions::default()), Ok(0));
    }

    #[def_test(serial)]
    fn af_unspec_disconnect_releases_auto_bound_udp_socket() {
        clear_registry();

        let socket = UdpSocket::new();
        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 49152);
        let remote = endpoint(Ipv4Address::new(192, 0, 2, 1), 5353);
        bind_udp_auto_pcb_for_test(socket.pcb.clone(), local, false).unwrap();
        socket.state().set_peer_endpoint(Some((remote, local.addr)));

        socket.connect(SocketAddrEx::Unspecified).unwrap();

        assert_eq!(socket.state().local_endpoint(), None);
        assert_eq!(socket.state().peer_endpoint(), None);
        assert!(
            lookup_udp_pcb(
                socket_addr(Ipv4Address::new(10, 0, 0, 2), 49152),
                socket_addr(Ipv4Address::new(192, 0, 2, 1), 5353)
            )
            .is_none()
        );
        assert!(udp_port_available(listen_endpoint(local)));
        clear_registry();
    }

    #[def_test(serial)]
    fn af_unspec_disconnect_preserves_explicit_udp_bind() {
        clear_registry();

        let socket = UdpSocket::new();
        let local = endpoint(Ipv4Address::new(10, 0, 0, 2), 49152);
        let remote = endpoint(Ipv4Address::new(192, 0, 2, 1), 5353);
        bind_udp_pcb(socket.pcb.clone(), local, false).unwrap();
        socket.state().set_peer_endpoint(Some((remote, local.addr)));

        socket.connect(SocketAddrEx::Unspecified).unwrap();

        assert_eq!(socket.state().local_endpoint(), Some(local));
        assert_eq!(socket.state().peer_endpoint(), None);
        let selected = lookup_udp_pcb(
            socket_addr(Ipv4Address::new(10, 0, 0, 2), 49152),
            socket_addr(Ipv4Address::new(192, 0, 2, 2), 5353),
        )
        .expect("disconnected UDP PCB should keep receiving on its local port");
        assert!(Arc::ptr_eq(&selected, &socket.pcb));
        assert!(!udp_port_available(listen_endpoint(local)));
        clear_registry();
    }

    #[def_test(serial)]
    fn loopback_net_rx_delivers_udp_without_poller() {
        clear_registry();

        let socket = UdpSocket::new();
        let local_ip = Ipv4Address::new(127, 0, 0, 1);
        let local = endpoint(local_ip, 5000);
        bind_udp_pcb(socket.pcb.clone(), local, false).unwrap();

        let mut udp = alloc::vec![0u8; UDP_HEADER_LEN + 1];
        udp[UDP_HEADER_LEN] = b'x';
        write_udp_header(&mut udp, local_ip, local_ip, 4000, 5000)
            .expect("UDP header length is valid");
        let packet = ipv4::build_ipv4_packet(local_ip, local_ip, ipv4::PROTOCOL_UDP, 64, &udp)
            .expect("IPv4 packet length is valid");
        let mut packet = PacketBuf::from_ip_packet_vec(1, packet, PacketOwner::Ipv4Stack);
        ipv4::Ipv4Header::prepare_output_packet(&mut packet).expect("IPv4 header is valid");

        let mut device = LoopbackDevice::new();
        assert!(device.send_ip_packet(
            1,
            IpAddress::Ipv4(local_ip),
            IpAddress::Ipv4(local_ip),
            packet,
            ktime_types::MonotonicInstant::ORIGIN,
        ));

        assert!(socket.pcb.has_recv_data());
        let datagram = socket
            .pcb
            .recv_datagram(RecvMode::Consume)
            .expect("NetRx should deliver the loopback datagram");
        assert_eq!(datagram.payload.as_slice(), b"x");
        assert!(
            device
                .poll_rx(1, ktime_types::MonotonicInstant::ORIGIN)
                .is_none()
        );

        clear_registry();
    }

    #[def_test(serial)]
    fn unmatched_loopback_udp_stays_queued_for_the_poller() {
        clear_registry();

        let local_ip = Ipv4Address::new(127, 0, 0, 1);
        let mut udp = alloc::vec![0u8; UDP_HEADER_LEN + 1];
        udp[UDP_HEADER_LEN] = b'x';
        write_udp_header(&mut udp, local_ip, local_ip, 4000, 5000)
            .expect("UDP header length is valid");
        let packet = ipv4::build_ipv4_packet(local_ip, local_ip, ipv4::PROTOCOL_UDP, 64, &udp)
            .expect("IPv4 packet length is valid");
        let mut packet = PacketBuf::from_ip_packet_vec(1, packet, PacketOwner::Ipv4Stack);
        ipv4::Ipv4Header::prepare_output_packet(&mut packet).expect("IPv4 header is valid");

        let mut device = LoopbackDevice::new();
        assert!(device.send_ip_packet(
            1,
            IpAddress::Ipv4(local_ip),
            IpAddress::Ipv4(local_ip),
            packet,
            ktime_types::MonotonicInstant::ORIGIN,
        ));

        let leftover = device
            .poll_rx(1, ktime_types::MonotonicInstant::ORIGIN)
            .expect("unmatched UDP should remain for the task poller");
        assert!(leftover.udp_metadata().is_none());

        clear_registry();
    }
}
