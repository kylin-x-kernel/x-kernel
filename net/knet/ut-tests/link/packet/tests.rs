// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use kio::{Cursor, IoBufMut, Write};
use kpoll::{IoEvents, Pollable};
use unittest::def_test;

use super::*;
use crate::{RecvOptions, SendOptions, SocketAddrEx};

const ETH_P_IP: u16 = 0x0800;
const TEST_LOCAL_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 1];
const TEST_REMOTE_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 2];
const TEST_OTHER_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 3];

fn ethernet_frame(dst: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(ETHERNET_HEADER_LEN + payload.len());
    frame.extend_from_slice(&dst);
    frame.extend_from_slice(&TEST_REMOTE_MAC);
    frame.extend_from_slice(&ethertype.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn publish_test_frame(ifindex: i32, data: &[u8]) {
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, data);
    publish_link_frame(ifindex, &frame, TEST_LOCAL_MAC);
}

fn test_link_state(is_up: bool, mtu: usize) -> LinkSendSnapshot {
    LinkSendSnapshot {
        is_up,
        mtu,
        hardware_addr: TEST_LOCAL_MAC,
    }
}

#[def_test(serial)]
fn test_protocol_and_ifindex_filtering() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
    *socket.inner.local_addr.write() = Some(PacketAddr::new(ETH_P_IP.to_be(), 2));

    publish_test_frame(1, &[1, 2, 3, 4]);
    assert!(!socket.poll().contains(IoEvents::IN));

    publish_test_frame(2, &[5, 6, 7, 8]);
    assert!(socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_recvmsg_writes_packet_address() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    let mut buf = [0u8; 32];
    let mut from = SocketAddrEx::Packet(PacketAddr::new(0, 0));
    let received = socket
        .recv(
            Cursor::new(buf.as_mut_slice()),
            RecvOptions {
                from: Some(&mut from),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(received, frame.len());
    assert_eq!(&buf[..frame.len()], &frame);
    let SocketAddrEx::Packet(addr) = from else {
        panic!("expected packet address");
    };
    assert_eq!(addr.ifindex, 2);
    assert_eq!(addr.protocol, ETH_P_IP.to_be());
    assert_eq!(addr.pkttype, PACKET_HOST);
    assert_eq!(&addr.addr[..6], &TEST_REMOTE_MAC);
}

#[def_test(serial)]
fn test_datagram_recv_strips_ethernet_header() {
    let socket = PacketSocket::new(PacketSocketKind::Datagram, ETH_P_IP.to_be()).unwrap();
    let payload = [1, 2, 3, 4];
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &payload);
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    let mut buf = [0u8; 16];
    let mut from = SocketAddrEx::Packet(PacketAddr::new(0, 0));
    let received = socket
        .recv(
            Cursor::new(buf.as_mut_slice()),
            RecvOptions {
                from: Some(&mut from),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(received, payload.len());
    assert_eq!(&buf[..payload.len()], &payload);
    let SocketAddrEx::Packet(addr) = from else {
        panic!("expected packet address");
    };
    assert_eq!(addr.protocol, ETH_P_IP.to_be());
    assert_eq!(addr.ifindex, 2);
    assert_eq!(&addr.addr[..6], &TEST_REMOTE_MAC);
}

#[def_test(serial)]
fn test_peek_preserves_frame() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    let mut peek_buf = [0u8; 32];
    let peeked = socket
        .recv(
            Cursor::new(peek_buf.as_mut_slice()),
            RecvOptions {
                flags: RecvFlags::PEEK,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(peeked, frame.len());
    assert!(socket.poll().contains(IoEvents::IN));

    let mut recv_buf = [0u8; 32];
    let received = socket
        .recv(Cursor::new(recv_buf.as_mut_slice()), RecvOptions::default())
        .unwrap();
    assert_eq!(received, frame.len());
    assert_eq!(&peek_buf[..frame.len()], &recv_buf[..frame.len()]);
}

#[def_test(serial)]
fn test_truncate_returns_original_frame_len() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    let mut buf = [0u8; 2];
    let mut out_flags = RecvFlags::empty();
    let received = socket
        .recv(
            Cursor::new(buf.as_mut_slice()),
            RecvOptions {
                flags: RecvFlags::TRUNCATE,
                out_flags: Some(&mut out_flags),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(received, frame.len());
    assert_eq!(buf, [TEST_LOCAL_MAC[0], TEST_LOCAL_MAC[1]]);
    assert!(out_flags.contains(RecvFlags::TRUNCATE));
    assert!(!socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_small_buffer_without_truncate_returns_written_len() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    let mut buf = [0u8; 2];
    let mut out_flags = RecvFlags::empty();
    let received = socket
        .recv(
            Cursor::new(buf.as_mut_slice()),
            RecvOptions {
                out_flags: Some(&mut out_flags),
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(received, buf.len());
    assert_eq!(buf, [TEST_LOCAL_MAC[0], TEST_LOCAL_MAC[1]]);
    assert!(out_flags.contains(RecvFlags::TRUNCATE));
    assert!(!socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_peek_with_truncation_preserves_frame() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    let mut buf = [0u8; 2];
    let received = socket
        .recv(
            Cursor::new(buf.as_mut_slice()),
            RecvOptions {
                flags: RecvFlags::PEEK,
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(received, buf.len());
    assert!(socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_eth_p_all_receives_ipv6_and_unknown_protocols() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
    let ipv6_frame = ethernet_frame(TEST_LOCAL_MAC, 0x86DD, &[1, 2, 3, 4]);
    let unknown_frame = ethernet_frame(TEST_LOCAL_MAC, 0x88B5, &[5, 6, 7, 8]);

    publish_link_frame(2, &ipv6_frame, TEST_LOCAL_MAC);
    publish_link_frame(2, &unknown_frame, TEST_LOCAL_MAC);

    let mut buf = [0u8; 32];
    assert_eq!(
        socket
            .recv(Cursor::new(buf.as_mut_slice()), RecvOptions::default())
            .unwrap(),
        ipv6_frame.len()
    );
    assert_eq!(
        socket
            .recv(Cursor::new(buf.as_mut_slice()), RecvOptions::default())
            .unwrap(),
        unknown_frame.len()
    );
}

#[def_test(serial)]
fn test_eth_p_ip_filters_ipv6_and_unknown_protocols() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let ipv6_frame = ethernet_frame(TEST_LOCAL_MAC, 0x86DD, &[1, 2, 3, 4]);
    let unknown_frame = ethernet_frame(TEST_LOCAL_MAC, 0x88B5, &[5, 6, 7, 8]);

    publish_link_frame(2, &ipv6_frame, TEST_LOCAL_MAC);
    publish_link_frame(2, &unknown_frame, TEST_LOCAL_MAC);

    assert!(!socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_custom_ethertype_matches() {
    const ETH_P_CUSTOM: u16 = 0x88b5;

    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_CUSTOM.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_CUSTOM, &[1, 2, 3, 4]);

    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    let mut buf = [0u8; 32];
    let received = socket
        .recv(Cursor::new(buf.as_mut_slice()), RecvOptions::default())
        .unwrap();
    assert_eq!(received, frame.len());
    assert_eq!(&buf[..frame.len()], &frame);
}

#[def_test(serial)]
fn test_datagram_empty_payload_frames_are_accounted() {
    let socket = PacketSocket::new(PacketSocketKind::Datagram, ETH_P_ALL.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[]);

    for _ in 0..=PACKET_RX_QUEUE_LIMIT_FRAMES {
        publish_link_frame(2, &frame, TEST_LOCAL_MAC);
    }

    let stats = socket.statistics();
    assert_eq!(stats.packets, (PACKET_RX_QUEUE_LIMIT_FRAMES + 1) as u32);
    assert_eq!(stats.drops, 1);
}

#[def_test(serial)]
fn test_zero_protocol_does_not_receive_until_bound() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, 0).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);

    publish_link_frame(2, &frame, TEST_LOCAL_MAC);
    assert!(!socket.poll().contains(IoEvents::IN));

    socket
        .bind(SocketAddrEx::Packet(PacketAddr::new(ETH_P_IP.to_be(), 0)))
        .unwrap();
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);
    assert!(socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_outgoing_frame_is_published() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);

    publish_outgoing_frame(2, &frame, TEST_LOCAL_MAC);

    let mut buf = [0u8; 32];
    let mut from = SocketAddrEx::Packet(PacketAddr::new(0, 0));
    socket
        .recv(
            Cursor::new(buf.as_mut_slice()),
            RecvOptions {
                from: Some(&mut from),
                ..Default::default()
            },
        )
        .unwrap();
    let SocketAddrEx::Packet(addr) = from else {
        panic!("expected packet address");
    };
    assert_eq!(addr.pkttype, PACKET_OUTGOING);
}

#[def_test(serial)]
fn test_otherhost_is_filtered_without_promisc_support() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
    let frame = ethernet_frame(TEST_OTHER_MAC, ETH_P_IP, &[1, 2, 3, 4]);

    publish_link_frame(2, &frame, TEST_LOCAL_MAC);
    assert!(!socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_packet_membership_is_unsupported() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
    let membership = PacketMembership {
        ifindex: 2,
        membership_type: PACKET_MR_PROMISC,
        addr_len: 0,
        addr: [0; 8],
    };

    assert_eq!(
        socket.record_membership(membership, true),
        Err(KError::OperationNotSupported)
    );
    assert_eq!(
        socket.record_membership(membership, false),
        Err(KError::OperationNotSupported)
    );
}

#[def_test(serial)]
fn test_zero_destination_mac_is_otherhost() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_ALL.to_be()).unwrap();
    let frame = ethernet_frame([0; 6], ETH_P_IP, &[1, 2, 3, 4]);

    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    assert!(!socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_send_ifindex_errors() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);

    assert_eq!(
        socket.send(Cursor::new(frame.as_slice()), SendOptions::default()),
        Err(KError::NotConnected)
    );

    let mut any_addr = PacketAddr::new(ETH_P_IP.to_be(), 0);
    assert_eq!(
        socket.send(
            Cursor::new(frame.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Packet(any_addr)),
                ..Default::default()
            }
        ),
        Err(LinuxError::ENXIO.into())
    );

    any_addr.ifindex = 123;
    assert_eq!(
        socket.send(
            Cursor::new(frame.as_slice()),
            SendOptions {
                to: Some(SocketAddrEx::Packet(any_addr)),
                ..Default::default()
            }
        ),
        Err(LinuxError::ENODEV.into())
    );
}

#[def_test(serial)]
fn test_send_link_state_validation() {
    let up_link = test_link_state(true, 1500);
    let down_link = test_link_state(false, 1500);

    assert_eq!(
        validate_link_state_for_send(&down_link, ETHERNET_HEADER_LEN, PacketSocketKind::Raw),
        Err(LinuxError::ENETDOWN.into())
    );
    assert_eq!(
        validate_link_state_for_send(&up_link, ETHERNET_HEADER_LEN - 1, PacketSocketKind::Raw),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        validate_link_state_for_send(
            &up_link,
            ETHERNET_HEADER_LEN + up_link.mtu + 1,
            PacketSocketKind::Raw,
        ),
        Err(LinuxError::EMSGSIZE.into())
    );
    assert_eq!(
        validate_link_state_for_send(&up_link, up_link.mtu + 1, PacketSocketKind::Datagram,),
        Err(LinuxError::EMSGSIZE.into())
    );
    assert!(
        validate_link_state_for_send(
            &up_link,
            ETHERNET_HEADER_LEN + up_link.mtu,
            PacketSocketKind::Raw,
        )
        .is_ok()
    );
    assert!(
        validate_link_state_for_send(&up_link, up_link.mtu, PacketSocketKind::Datagram).is_ok()
    );
}

#[def_test(serial)]
fn test_unsupported_connection_ops() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let addr = SocketAddrEx::Packet(PacketAddr::new(ETH_P_IP.to_be(), 2));

    assert_eq!(
        socket.connect(addr, ConnectOptions::default()),
        Err(KError::OperationNotSupported)
    );
    assert_eq!(
        socket.peer_addr().err(),
        Some(KError::OperationNotSupported)
    );
    assert_eq!(
        socket.shutdown(Shutdown::Both),
        Err(KError::OperationNotSupported)
    );
}

#[def_test(serial)]
fn test_datagram_frame_builder() {
    let mut addr = PacketAddr::new(ETH_P_IP.to_be(), 2);
    addr.addr_len = 6;
    addr.addr[..6].copy_from_slice(&TEST_REMOTE_MAC);

    let payload = [1, 2, 3, 4];
    let frame = build_datagram_frame(addr, TEST_LOCAL_MAC, ETH_P_IP.to_be(), &payload).unwrap();

    assert_eq!(&frame[..6], &TEST_REMOTE_MAC);
    assert_eq!(&frame[6..12], &TEST_LOCAL_MAC);
    assert_eq!(&frame[12..14], &ETH_P_IP.to_be_bytes());
    assert_eq!(&frame[14..], &payload);

    addr.addr_len = 5;
    assert_eq!(
        build_datagram_frame(addr, TEST_LOCAL_MAC, ETH_P_IP.to_be(), &payload),
        Err(KError::InvalidInput)
    );
}

#[def_test(serial)]
fn test_recv_write_failure_preserves_packet() {
    struct FailingDst;

    impl Write for FailingDst {
        fn write(&mut self, _buf: &[u8]) -> kio::Result<usize> {
            Err(KError::BadAddress)
        }

        fn flush(&mut self) -> kio::Result<()> {
            Ok(())
        }
    }

    impl IoBufMut for FailingDst {
        fn remaining_mut(&self) -> usize {
            32
        }
    }

    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    assert_eq!(
        socket.recv(FailingDst, RecvOptions::default()),
        Err(KError::BadAddress)
    );
    assert!(socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_recv_retries_partial_writes() {
    struct ShortDst {
        buf: [u8; 32],
        pos: usize,
    }

    impl Write for ShortDst {
        fn write(&mut self, buf: &[u8]) -> kio::Result<usize> {
            let write_len = buf.len().min(1);
            self.buf[self.pos..self.pos + write_len].copy_from_slice(&buf[..write_len]);
            self.pos += write_len;
            Ok(write_len)
        }

        fn flush(&mut self) -> kio::Result<()> {
            Ok(())
        }
    }

    impl IoBufMut for ShortDst {
        fn remaining_mut(&self) -> usize {
            self.buf.len() - self.pos
        }
    }

    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    let mut dst = ShortDst {
        buf: [0; 32],
        pos: 0,
    };
    assert_eq!(
        socket.recv(&mut dst, RecvOptions::default()).unwrap(),
        frame.len()
    );
    assert_eq!(&dst.buf[..frame.len()], &frame);
    assert!(!socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_recv_partial_write_failure_preserves_packet() {
    struct PartialFailDst {
        wrote_once: bool,
    }

    impl Write for PartialFailDst {
        fn write(&mut self, buf: &[u8]) -> kio::Result<usize> {
            if self.wrote_once {
                return Err(KError::BadAddress);
            }
            self.wrote_once = true;
            Ok(buf.len().min(1))
        }

        fn flush(&mut self) -> kio::Result<()> {
            Ok(())
        }
    }

    impl IoBufMut for PartialFailDst {
        fn remaining_mut(&self) -> usize {
            32
        }
    }

    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(TEST_LOCAL_MAC, ETH_P_IP, &[1, 2, 3, 4]);
    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    assert_eq!(
        socket.recv(PartialFailDst { wrote_once: false }, RecvOptions::default()),
        Err(KError::BadAddress)
    );
    assert!(socket.poll().contains(IoEvents::IN));
}

#[def_test(serial)]
fn test_queue_drop_statistics() {
    let socket = PacketSocket::new(PacketSocketKind::Raw, ETH_P_IP.to_be()).unwrap();
    let frame = ethernet_frame(
        TEST_LOCAL_MAC,
        ETH_P_IP,
        &vec![0; PACKET_RX_QUEUE_LIMIT_BYTES],
    );

    publish_link_frame(2, &frame, TEST_LOCAL_MAC);

    let stats = socket.statistics();
    assert_eq!(stats.packets, 1);
    assert_eq!(stats.drops, 1);
    assert!(!socket.poll().contains(IoEvents::IN));
}
