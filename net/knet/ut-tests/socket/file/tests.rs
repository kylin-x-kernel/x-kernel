// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use kalloc_types::KBox;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    options::PeerCredentials,
    unix::{DgramTransport, StreamTransport, UnixDomainSocket, UnixSocketType},
};

fn test_peer_credentials() -> PeerCredentials {
    PeerCredentials::new(kidentity::PidHandle::fixed_root(7), 1000, 1001)
}

type SenderCredential = (Option<u32>, u32, u32);

#[def_test(user)]
fn unix_datagram_file_io_preserves_empty_and_large_messages() {
    let (sender, receiver) = DgramTransport::new_pair(test_peer_credentials());
    let files = [sender, receiver].map(|transport| {
        sock_alloc_file(
            Socket::Unix(
                KBox::new(UnixDomainSocket::new(transport, UnixSocketType::Datagram)).unwrap(),
            ),
            (OpenFlags::READ_WRITE | OpenFlags::NONBLOCK).bits(),
            kprocess::current_cred(),
        )
        .unwrap()
    });

    for len in [0, 4097, 8192] {
        let payload = vec![0x5a; len];
        assert_eq!(files[0].write(&payload).unwrap(), len);
        assert!(files[1].poll().contains(IoEvents::IN));
        assert_eq!(files[1].read(&mut []), Ok(0));
        assert!(files[1].poll().contains(IoEvents::IN));

        // Extra capacity must not cause a second receive after this message.
        let mut received = vec![0; len + 1];
        assert_eq!(files[1].read(&mut received).unwrap(), len);
        assert_eq!(&received[..len], &payload);
        assert!(!files[1].poll().contains(IoEvents::IN));
        assert_eq!(files[1].read(&mut received), Err(KError::WouldBlock));
    }
}

#[def_test(user)]
fn unix_stream_file_write_attaches_credentials_with_either_endpoint() {
    use crate::{
        KernelAncillaryData,
        options::{Configurable, SetSocketOption},
        unix::UnixTransportOps,
    };

    fn write_and_receive(has_passcred: bool) -> ([u8; 3], Vec<SenderCredential>) {
        let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
        let sender = sock_alloc_file(
            Socket::Unix(KBox::new(UnixDomainSocket::new(sender, UnixSocketType::Stream)).unwrap()),
            (OpenFlags::READ_WRITE | OpenFlags::NONBLOCK).bits(),
            kprocess::current_cred(),
        )
        .unwrap();
        sock_from_file(&sender)
            .unwrap()
            .set_option(SetSocketOption::PassCredentials(&has_passcred))
            .unwrap();
        receiver
            .set_option(SetSocketOption::PassCredentials(&true))
            .unwrap();
        receiver
            .set_option(SetSocketOption::NonBlocking(&true))
            .unwrap();

        sender.write(b"abc").unwrap();
        let mut data = [0_u8; 3];
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
        let credentials = ancillary
            .iter()
            .filter_map(|record| {
                let KernelAncillaryData::Credentials(credential) =
                    record.downcast_ref::<KernelAncillaryData>()?
                else {
                    return None;
                };
                Some((
                    credential.pid.as_ref().map(|pid| pid.root_nr()),
                    credential.uid,
                    credential.gid,
                ))
            })
            .collect();
        (data, credentials)
    }

    let thread = kprocess::current_user_thread();
    let cred = thread.cred();
    let (data, credentials) = write_and_receive(true);
    assert_eq!(&data, b"abc");
    assert_eq!(
        credentials,
        vec![(
            Some(thread.process().pid_identity().root_nr()),
            cred.ruid(),
            cred.rgid()
        )]
    );
    // Linux captures when either endpoint enabled SO_PASSCRED, so the
    // receiver-only case must still produce the sender snapshot.
    let (data, credentials) = write_and_receive(false);
    assert_eq!(&data, b"abc");
    assert_eq!(
        credentials,
        vec![(
            Some(thread.process().pid_identity().root_nr()),
            cred.ruid(),
            cred.rgid()
        )]
    );
}
