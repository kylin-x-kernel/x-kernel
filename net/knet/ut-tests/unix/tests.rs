// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kcred::Cred;
use kerrno::KError;
use unittest::{assert, assert_eq, def_test};

use super::{ABSTRACT_BINDINGS, AbstractBinding, BindEntry, UnixAddr, lookup_bind_entry};

fn test_peer_credentials() -> crate::options::PeerCredentials {
    crate::options::PeerCredentials::new(kidentity::PidHandle::fixed_root(7), 1000, 1001)
}

#[def_test]
fn accept_futures_are_send_and_fit_inline_budget() {
    use core::future::Future;

    use super::{DgramTransport, SeqpacketTransport, StreamTransport, UnixTransport};

    fn future_size(future: impl Future + Send) -> usize {
        let bytes = core::mem::size_of_val(&future);
        info!("Unix accept future: {} bytes", bytes);
        bytes
    }

    assert!(future_size(StreamTransport::new().accept(false)) <= 2048);
    assert!(future_size(SeqpacketTransport::new().accept(false)) <= 2048);
    assert!(future_size(DgramTransport::new().accept(false)) <= 2048);
    for transport in [
        UnixTransport::Stream(StreamTransport::new()),
        UnixTransport::Seqpacket(SeqpacketTransport::new()),
        UnixTransport::Dgram(DgramTransport::new()),
    ] {
        assert!(future_size(transport.accept(false)) <= 2048);
    }
    let datagram = UnixTransport::Dgram(DgramTransport::new());
    assert!(matches!(
        ktask::future::block_on(datagram.accept(true)),
        Err(KError::InvalidInput)
    ));
}

#[def_test]
fn cancelled_inline_accept_preserves_listener_and_pending_connection() {
    use core::{future::Future, pin::pin, task::Context};

    use kpoll::{IoEvents, Pollable};
    use ktask::future::block_on;

    use super::{SeqpacketTransport, StreamTransport, UnixTransport, UnixTransportOps};
    use crate::{ConnectOptions, RecvOptions, SendOptions};

    for (listener, client) in [
        (
            UnixTransport::Stream(StreamTransport::new()),
            UnixTransport::Stream(StreamTransport::new()),
        ),
        (
            UnixTransport::Seqpacket(SeqpacketTransport::new()),
            UnixTransport::Seqpacket(SeqpacketTransport::new()),
        ),
    ] {
        let entry = BindEntry::default();
        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(1, &test_peer_credentials()).unwrap();
        {
            let mut waiting = pin!(listener.accept(false));
            let mut context = Context::from_waker(core::task::Waker::noop());
            assert!(waiting.as_mut().poll(&mut context).is_pending());
        }
        client
            .connect(
                &entry,
                &UnixAddr::Unbound,
                ConnectOptions::default(),
                &test_peer_credentials(),
            )
            .unwrap();
        drop(listener.accept(false));
        assert!(listener.poll().contains(IoEvents::IN));
        let (accepted, _) = block_on(listener.accept(true)).unwrap();
        assert_eq!(client.send(&b"ok"[..], SendOptions::default()), Ok(2));
        let mut bytes = [0; 2];
        assert_eq!(accepted.recv(&mut bytes[..], RecvOptions::default()), Ok(2));
        assert_eq!(&bytes, b"ok");
        assert!(matches!(
            block_on(listener.accept(true)),
            Err(KError::WouldBlock)
        ));
    }
}

#[def_test]
fn unix_missing_abstract_address_returns_connection_refused() {
    let addr = UnixAddr::Abstract(Arc::from(&b"xkernel-missing-abstract-socket"[..]));

    assert_eq!(
        lookup_bind_entry(&addr, &Cred::root(), |_| Ok(())),
        Err(KError::ConnectionRefused)
    );
}

#[def_test(serial)]
fn unix_abstract_lookup_releases_registry_before_callback() {
    let name: Arc<[u8]> = Arc::from(&b"xkernel-abstract-lock-snapshot"[..]);
    let addr = UnixAddr::Abstract(name.clone());
    let _binding = AbstractBinding::reserve(name);

    let result = lookup_bind_entry(&addr, &Cred::root(), |_| {
        Ok(ABSTRACT_BINDINGS.try_lock().is_some())
    });

    assert_eq!(result, Ok(true));
}

#[def_test]
fn abstract_binding_lifetime_tracks_bound_sockets_and_failed_binds() {
    use super::{DgramTransport, StreamTransport, UnixDomainSocket, UnixSocketType};
    use crate::SocketAddrEx;

    let name: Arc<[u8]> = Arc::from(&b"xkernel-abstract-binding-lifetime"[..]);
    let addr = UnixAddr::Abstract(name.clone());
    let bind = |socket: &UnixDomainSocket| {
        socket.bind_with_cred(
            SocketAddrEx::Unix(addr.clone()),
            &Cred::root(),
            kvfs::NodePermission::from_bits_truncate(0o777),
        )
    };
    let stream = UnixDomainSocket::new(StreamTransport::new(), UnixSocketType::Stream);
    let dgram = UnixDomainSocket::new(DgramTransport::new(), UnixSocketType::Datagram);
    bind(&stream).unwrap();
    bind(&dgram).unwrap();
    let conflict = UnixDomainSocket::new(DgramTransport::new(), UnixSocketType::Datagram);
    assert_eq!(bind(&conflict), Err(KError::AddrInUse));
    drop(stream);
    assert!(ABSTRACT_BINDINGS.lock().contains_key(&name));
    drop(dgram);
    assert!(!ABSTRACT_BINDINGS.lock().contains_key(&name));
    bind(&conflict).unwrap();
    drop(conflict);
    assert!(!ABSTRACT_BINDINGS.lock().contains_key(&name));
}

#[def_test]
fn accept_owner_failure_preserves_pending_connection() {
    use kalloc_types::KBox;
    use kpoll::{IoEvents, Pollable};

    use super::{StreamTransport, UnixDomainSocket, UnixSocketType, UnixTransportOps};
    use crate::{AcceptOptions, ConnectOptions, RecvOptions, SendOptions, SocketOps};

    let entry = BindEntry::default();
    let listener = StreamTransport::new();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();
    let client = StreamTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    let listener = UnixDomainSocket::new(listener, UnixSocketType::Stream);
    assert!(listener.poll().contains(IoEvents::IN));
    let options = AcceptOptions { nonblocking: true };
    assert!(matches!(
        listener.accept_with_storage(options, || Err(KError::NoMemory)),
        Err(KError::NoMemory)
    ));
    assert!(listener.poll().contains(IoEvents::IN));
    let accepted = listener
        .accept_with_storage(options, KBox::<UnixDomainSocket>::new_uninit)
        .unwrap();
    assert!(!listener.poll().contains(IoEvents::IN));
    assert_eq!(client.send(&b"ok"[..], SendOptions::default()), Ok(2));
    let mut bytes = [0; 2];
    assert_eq!(accepted.recv(&mut bytes[..], RecvOptions::default()), Ok(2));
    assert_eq!(&bytes, b"ok");
    assert!(matches!(listener.accept(options), Err(KError::WouldBlock)));
}

#[def_test]
fn unix_stream_attaches_sender_credentials_only_after_passcred() {
    use super::{DgramTransport, StreamTransport, UnixDomainSocket, UnixSocketType};
    use crate::options::{Configurable, SetSocketOption};

    let stream = UnixDomainSocket::new(StreamTransport::new(), UnixSocketType::Stream);
    assert_eq!(stream.should_attach_sender_credentials(), false);

    stream
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    assert_eq!(stream.should_attach_sender_credentials(), true);

    // Datagram sends always carry a credential snapshot so a receiver with
    // SO_PASSCRED can observe the real sender, even though datagram connect()
    // never establishes an SO_PEERCRED identity.
    let dgram = UnixDomainSocket::new(DgramTransport::new(), UnixSocketType::Datagram);
    assert_eq!(dgram.should_attach_sender_credentials(), true);
}
