// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    KernelAncillaryData, SendOptions,
    options::{PeerCredentials, UnixCredentials},
    unix::{BindEntry, UnixTransportOps},
};

fn test_peer_credentials() -> PeerCredentials {
    PeerCredentials::new(kidentity::PidHandle::fixed_root(7), 1000, 1001)
}

fn peer_credentials_for(pid: u32, uid: u32, gid: u32) -> PeerCredentials {
    PeerCredentials::new(kidentity::PidHandle::fixed_root(pid), uid, gid)
}

fn resolved_peer_credentials(socket: &DgramTransport) -> UnixCredentials {
    let mut peer = PeerCredentials::none();
    socket
        .get_option(GetSocketOption::PeerCredentials(&mut peer))
        .unwrap();
    peer.resolve_in(kidentity::root_pid_namespace())
}

macro_rules! assert_peer_credentials {
    ($socket:expr, $pid:expr, $uid:expr, $gid:expr) => {{
        let peer = resolved_peer_credentials($socket);
        assert_eq!(peer.pid, $pid);
        assert_eq!(peer.uid, $uid);
        assert_eq!(peer.gid, $gid);
    }};
}

#[def_test]
fn unix_datagram_receive_honors_per_call_nonblocking_after_empty_message() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    let mut data = [0; 1];
    let receive = |data: &mut [u8]| {
        receiver.recv(
            data,
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ..RecvOptions::default()
            },
        )
    };

    assert_eq!(receive(&mut data), Err(KError::WouldBlock));
    assert_eq!(sender.send(&b""[..], SendOptions::default()), Ok(0));
    assert_eq!(receive(&mut data), Ok(0));
    assert_eq!(receive(&mut data), Err(KError::WouldBlock));
}

fn test_credentials() -> crate::SenderCredentials {
    crate::SenderCredentials {
        pid: Some(kprocess::init_proc().pid_identity()),
        uid: 1000,
        gid: 1001,
    }
}

fn send_credentials(sender: &DgramTransport, credentials: crate::SenderCredentials) {
    sender
        .send(
            &b"x"[..],
            SendOptions {
                sender_credentials: Some(credentials),
                ..SendOptions::default()
            },
        )
        .unwrap();
}

fn recv_ancillary(receiver: &DgramTransport) -> Vec<AncillaryData> {
    let mut data = [0_u8; 1];
    let mut ancillary = Vec::new();
    receiver
        .recv(
            &mut data[..],
            RecvOptions {
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    if data != [b'x'] {
        panic!("unexpected datagram payload");
    }
    ancillary
}

#[def_test]
fn unix_dgram_passcred_returns_current_sender_credentials() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    send_credentials(&sender, test_credentials());

    let ancillary = recv_ancillary(&receiver);
    assert_eq!(ancillary.len(), 1);
    let credentials = ancillary
        .into_iter()
        .next()
        .unwrap()
        .downcast::<KernelAncillaryData>()
        .unwrap();
    // The pid stays a process identity so the receive side can resolve it in
    // the reader's PID namespace.
    let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
        panic!("expected sender credentials");
    };
    assert_eq!(credentials.uid, 1000);
    assert_eq!(credentials.gid, 1001);
}

#[def_test]
fn unix_dgram_without_passcred_omits_sender_credentials() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    send_credentials(&sender, test_credentials());

    assert!(recv_ancillary(&receiver).is_empty());
}

#[def_test]
fn unix_dgram_passcred_without_sender_snapshot_omits_credentials() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    sender.send(&b"x"[..], SendOptions::default()).unwrap();

    assert!(recv_ancillary(&receiver).is_empty());
}

#[def_test]
fn unix_dgram_passcred_preserves_each_sender_snapshot() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    for uid in [1042, 1043] {
        send_credentials(
            &sender,
            crate::SenderCredentials {
                pid: Some(kprocess::init_proc().pid_identity()),
                uid,
                gid: uid + 1000,
            },
        );
    }

    for uid in [1042, 1043] {
        let mut ancillary = recv_ancillary(&receiver);
        assert_eq!(ancillary.len(), 1);
        let credentials = ancillary
            .pop()
            .unwrap()
            .downcast::<KernelAncillaryData>()
            .unwrap();
        let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
            panic!("expected sender credentials");
        };
        assert_eq!(credentials.uid, uid);
        assert_eq!(credentials.gid, uid + 1000);
    }
}

/// Opaque stand-in for the `SCM_RIGHTS` payload `posix-net` attaches to a
/// send; these transports treat ancillary data as type-erased.
struct PassedFiles;

fn recv_without_control_buffer(receiver: &DgramTransport) -> (u8, bool) {
    let mut data = [0_u8; 1];
    let mut discarded = false;
    receiver
        .recv(
            &mut data[..],
            RecvOptions {
                ancillary: None,
                ancillary_discarded: Some(&mut discarded),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    (data[0], discarded)
}

#[def_test]
fn unix_dgram_without_control_buffer_reports_discarded_passed_files() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    sender
        .send(
            &b"x"[..],
            SendOptions {
                ancillary: alloc::vec![Arc::new(PassedFiles) as AncillaryData],
                ..SendOptions::default()
            },
        )
        .unwrap();

    // The datagram is consumed, but its descriptors cannot be delivered:
    // the caller must observe `MSG_CTRUNC` (`scm_detach_fds`).
    assert_eq!(recv_without_control_buffer(&receiver), (b'x', true));
}

#[def_test]
fn unix_dgram_without_control_buffer_reports_nothing_without_passed_files() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    sender.send(&b"x"[..], SendOptions::default()).unwrap();

    assert_eq!(recv_without_control_buffer(&receiver), (b'x', false));
}

#[def_test]
fn unix_dgram_without_control_buffer_reports_credentials_as_ctrunc() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    send_credentials(&sender, test_credentials());

    // Linux `__scm_recv_common` reports `MSG_CTRUNC` when no control buffer
    // is supplied and the receive would have delivered `SO_PASSCRED`
    // credentials, even without passed file descriptions.
    assert_eq!(recv_without_control_buffer(&receiver), (b'x', true));
}

#[def_test]
fn unix_dgram_without_control_buffer_ignores_unexposed_credentials() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    // No sender snapshot was captured, so the receive exposes no
    // credentials and dropping nothing is not reported.
    sender.send(&b"x"[..], SendOptions::default()).unwrap();

    assert_eq!(recv_without_control_buffer(&receiver), (b'x', false));
}

#[def_test]
fn unix_dgram_peer_credentials_follow_only_socketpair_creation() {
    let creator = peer_credentials_for(31, 3100, 3101);
    let (left, right) = DgramTransport::new_pair(creator);
    assert_peer_credentials!(&left, 31, 3100, 3101);
    assert_peer_credentials!(&right, 31, 3100, 3101);

    let independent = DgramTransport::new();
    assert_peer_credentials!(&independent, 0, u32::MAX, u32::MAX);
}

#[def_test]
fn unix_dgram_connect_does_not_create_peer_credentials() {
    let sender = DgramTransport::new();
    let receiver = DgramTransport::new();
    let sender_entry = BindEntry::default();
    let receiver_entry = BindEntry::default();

    sender.bind(&sender_entry, &UnixAddr::Unbound).unwrap();
    receiver.bind(&receiver_entry, &UnixAddr::Unbound).unwrap();
    sender
        .connect(
            &receiver_entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &peer_credentials_for(32, 3200, 3201),
        )
        .unwrap();

    // Linux datagram connect() installs a destination, not an SO_PEERCRED
    // identity, for either endpoint.
    assert_peer_credentials!(&sender, 0, u32::MAX, u32::MAX);
    assert_peer_credentials!(&receiver, 0, u32::MAX, u32::MAX);

    sender.send(&b"x"[..], SendOptions::default()).unwrap();
    let mut data = [0_u8; 1];
    assert_eq!(receiver.recv(&mut data[..], RecvOptions::default()), Ok(1));
    assert_eq!(data, [b'x']);
}

/// A datagram transport cannot answer `SIOCINQ`, so it says so.
///
/// `unix_inq_len` reports the head datagram's length for `SOCK_DGRAM`, but
/// these messages live in an `async_channel::Receiver`, which has no peek. The
/// ioctl turns the missing answer into ENOTTY; a zero would instead read as an
/// empty queue to a caller such as glibc's resolver, which abandons a lookup
/// when `FIONREAD` reports 0 even though the reply is already queued.
#[def_test]
fn queue_lengths_are_unavailable_for_the_datagram_transport() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    assert_eq!(sender.send(&b"hello"[..], SendOptions::default()), Ok(5));

    assert!(sender.queue_lengths().is_none());
    assert!(receiver.queue_lengths().is_none());
}
