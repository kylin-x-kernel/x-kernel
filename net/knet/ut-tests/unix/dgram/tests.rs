// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::KernelAncillaryData;

#[def_test]
fn unix_datagram_receive_honors_per_call_nonblocking_after_empty_message() {
    let (sender, receiver) = DgramTransport::new_pair(7);
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

fn send_credentials(sender: &DgramTransport, credentials: UnixCredentials) {
    sender
        .send(
            &b"x"[..],
            SendOptions {
                unix_sender_credentials: Some(credentials),
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
    let (sender, receiver) = DgramTransport::new_pair(7);
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    send_credentials(
        &sender,
        UnixCredentials {
            pid: 42,
            uid: 1000,
            gid: 1001,
        },
    );

    let ancillary = recv_ancillary(&receiver);
    assert_eq!(ancillary.len(), 1);
    let credentials = ancillary
        .into_iter()
        .next()
        .unwrap()
        .downcast::<KernelAncillaryData>()
        .unwrap();
    let KernelAncillaryData::Credentials(credentials) = credentials.as_ref() else {
        panic!("expected sender credentials");
    };
    assert_eq!(credentials.pid, 42);
    assert_eq!(credentials.uid, 1000);
    assert_eq!(credentials.gid, 1001);
}

#[def_test]
fn unix_dgram_without_passcred_omits_sender_credentials() {
    let (sender, receiver) = DgramTransport::new_pair(7);
    send_credentials(
        &sender,
        UnixCredentials {
            pid: 42,
            uid: 1000,
            gid: 1001,
        },
    );

    assert!(recv_ancillary(&receiver).is_empty());
}

#[def_test]
fn unix_dgram_passcred_without_sender_snapshot_omits_credentials() {
    let (sender, receiver) = DgramTransport::new_pair(7);
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    sender.send(&b"x"[..], SendOptions::default()).unwrap();

    assert!(recv_ancillary(&receiver).is_empty());
}

#[def_test]
fn unix_dgram_passcred_preserves_each_sender_snapshot() {
    let (sender, receiver) = DgramTransport::new_pair(7);
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    for pid in [42, 43] {
        send_credentials(
            &sender,
            UnixCredentials {
                pid,
                uid: pid + 1000,
                gid: pid + 2000,
            },
        );
    }

    for pid in [42, 43] {
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
        assert_eq!(credentials.pid, pid);
        assert_eq!(credentials.uid, pid + 1000);
        assert_eq!(credentials.gid, pid + 2000);
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
    let (sender, receiver) = DgramTransport::new_pair(7);
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
    let (sender, receiver) = DgramTransport::new_pair(7);
    sender.send(&b"x"[..], SendOptions::default()).unwrap();

    assert_eq!(recv_without_control_buffer(&receiver), (b'x', false));
}

#[def_test]
fn unix_dgram_without_control_buffer_reports_credentials_as_ctrunc() {
    let (sender, receiver) = DgramTransport::new_pair(7);
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    send_credentials(
        &sender,
        UnixCredentials {
            pid: 42,
            uid: 1000,
            gid: 1001,
        },
    );

    // Linux `__scm_recv_common` reports `MSG_CTRUNC` when no control buffer
    // is supplied and the receive would have delivered `SO_PASSCRED`
    // credentials, even without passed file descriptions.
    assert_eq!(recv_without_control_buffer(&receiver), (b'x', true));
}

#[def_test]
fn unix_dgram_without_control_buffer_ignores_unexposed_credentials() {
    let (sender, receiver) = DgramTransport::new_pair(7);
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    // No sender snapshot was captured, so the receive exposes no
    // credentials and dropping nothing is not reported.
    sender.send(&b"x"[..], SendOptions::default()).unwrap();

    assert_eq!(recv_without_control_buffer(&receiver), (b'x', false));
}
