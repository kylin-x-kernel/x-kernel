// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kpoll::{IoEvents, Pollable};
use ksync::Mutex;
use ktask::future::block_on;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    options::{Configurable, GetSocketOption},
    unix::BindEntry,
};

const TASK_WAIT_ROUNDS: usize = 100_000;

fn test_peer_credentials() -> PeerCredentials {
    PeerCredentials::new(kidentity::PidHandle::fixed_root(7), 1000, 1001)
}

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..TASK_WAIT_ROUNDS {
        if predicate() {
            return true;
        }
        ktask::yield_now();
    }
    false
}

#[def_test]
fn socketpair_keeps_message_boundaries() {
    let (sender, receiver) = SeqpacketTransport::new_pair(test_peer_credentials());
    assert_eq!(sender.send(&b"hello"[..], SendOptions::default()), Ok(5));
    assert_eq!(sender.send(&b"world!"[..], SendOptions::default()), Ok(6));

    let mut buf = [0u8; 16];
    assert_eq!(receiver.recv(&mut buf[..], RecvOptions::default()), Ok(5));
    assert_eq!(&buf[..5], b"hello");
    assert_eq!(receiver.recv(&mut buf[..], RecvOptions::default()), Ok(6));
    assert_eq!(&buf[..6], b"world!");
}

#[def_test]
fn short_receive_truncates_the_whole_message() {
    let (sender, receiver) = SeqpacketTransport::new_pair(test_peer_credentials());
    assert_eq!(sender.send(&b"abcdef"[..], SendOptions::default()), Ok(6));

    let mut buf = [0u8; 3];
    assert_eq!(receiver.recv(&mut buf[..], RecvOptions::default()), Ok(3));
    assert_eq!(&buf, b"abc");
    // The remainder is discarded rather than left for the next receive.
    let nonblocking = RecvOptions {
        flags: RecvFlags::DONT_WAIT,
        ..RecvOptions::default()
    };
    assert_eq!(receiver.recv(&mut buf[..], nonblocking), Err(KError::WouldBlock));
}

#[def_test]
fn listener_accepts_a_queued_connection() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    let client = SeqpacketTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();

    let UnixTransport::Seqpacket(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("seqpacket listener returned another transport");
    };

    assert_eq!(client.send(&b"ping"[..], SendOptions::default()), Ok(4));
    let mut buf = [0u8; 16];
    assert_eq!(accepted.recv(&mut buf[..], RecvOptions::default()), Ok(4));
    assert_eq!(&buf[..4], b"ping");

    assert_eq!(accepted.send(&b"pong"[..], SendOptions::default()), Ok(4));
    assert_eq!(client.recv(&mut buf[..], RecvOptions::default()), Ok(4));
    assert_eq!(&buf[..4], b"pong");
}

/// A send rejected by a released receive direction leaves no charge behind.
///
/// The charge is taken before the enqueue, so the rejection path has to give it
/// back; otherwise a `SIOCOUTQ` nobody can read would stay on the socket.
#[def_test]
fn rejected_send_leaves_the_outq_unchanged() {
    let (sender, receiver) = SeqpacketTransport::new_pair(test_peer_credentials());
    drop(receiver);
    assert_eq!(
        sender.send(&b"lost"[..], SendOptions::default()),
        Err(KError::BrokenPipe)
    );
    assert_eq!(sender.queue_lengths().unwrap().outq, 0);
}

/// A receive shutdown does not release the sender's `SIOCOUTQ`.
///
/// `shutdown(SHUT_RD)` closes the receiving side but leaves the records queued
/// before it readable, so the bytes the sender queued are still going to be
/// read and the count must not drop to zero with the channel.
#[def_test]
fn peer_read_shutdown_keeps_the_queued_outq() {
    let (sender, receiver) = SeqpacketTransport::new_pair(test_peer_credentials());
    assert_eq!(sender.send(&b"hello"[..], SendOptions::default()), Ok(5));
    assert_eq!(sender.queue_lengths().unwrap().outq, 5);

    receiver.shutdown(Shutdown::Read).unwrap();
    assert_eq!(
        sender.queue_lengths().unwrap().outq,
        5,
        "the queued message is still readable"
    );

    let mut buf = [0u8; 8];
    assert_eq!(receiver.recv(&mut buf[..], RecvOptions::default()), Ok(5));
    assert_eq!(&buf[..5], b"hello");
    assert_eq!(sender.queue_lengths().unwrap().outq, 0);
}

/// A client's `SIOCOUTQ` falls to 0 when the connection it queued data into is
/// released before it is accepted.
///
/// Linux's `unix_release_sock` purges a listening socket's queued children,
/// which frees the skbs and drops the sender's `sk_wmem_alloc`
/// (`unix_outq_len`). No transport runs `Drop` for a pending connection the
/// listener never accepted, so the closed channel is what has to show the
/// sender that its bytes can never be read.
#[def_test]
fn released_pending_connection_clears_the_client_outq() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    let client = SeqpacketTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    assert_eq!(client.send(&b"queued"[..], SendOptions::default()), Ok(6));
    assert_eq!(client.queue_lengths().unwrap().outq, 6);

    // Released without `accept`: the queued connection is discarded and those
    // bytes can never be read.
    drop(listener);
    assert_eq!(client.queue_lengths().unwrap().outq, 0);
}

#[def_test]
fn accept_would_block_without_a_connection() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    assert_eq!(
        block_on(listener.accept(true)).err(),
        Some(KError::WouldBlock)
    );
}

#[def_test]
fn accept_on_a_bound_but_unlistened_socket_fails() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();

    assert_eq!(
        block_on(listener.accept(false)).err(),
        Some(KError::InvalidInput)
    );
    assert_eq!(
        block_on(listener.accept(true)).err(),
        Some(KError::InvalidInput)
    );
}

#[def_test]
fn only_an_active_listener_hides_writability() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    assert!(listener.poll().contains(IoEvents::OUT));

    listener.listen(1, &test_peer_credentials()).unwrap();
    assert!(!listener.poll().contains(IoEvents::OUT));

    let client = SeqpacketTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    let events = listener.poll();
    assert!(events.contains(IoEvents::IN));
    assert!(!events.contains(IoEvents::OUT));

    listener.shutdown(Shutdown::Read).unwrap();
    let events = listener.poll();
    assert!(events.contains(IoEvents::IN | IoEvents::RDHUP));
    assert!(!events.contains(IoEvents::OUT | IoEvents::HUP));
}

#[def_test]
fn connecting_to_a_bound_but_unlistened_socket_is_refused() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();

    let client = SeqpacketTransport::new();
    assert_eq!(
        client
            .connect(
                &entry,
                &UnixAddr::Unbound,
                ConnectOptions::default(),
                &test_peer_credentials(),
            )
            .err(),
        Some(KError::ConnectionRefused)
    );
}

#[def_test]
fn shutdown_write_makes_the_peer_see_eof() {
    let (sender, receiver) = SeqpacketTransport::new_pair(test_peer_credentials());
    sender.shutdown(Shutdown::Write).unwrap();

    let mut buf = [0u8; 8];
    let nonblocking = RecvOptions {
        flags: RecvFlags::DONT_WAIT,
        ..RecvOptions::default()
    };
    // The peer's receive side is closed: it reports EOF instead of blocking.
    assert_eq!(receiver.recv(&mut buf[..], nonblocking), Ok(0));
    // The closed direction refuses further sends.
    assert_eq!(
        sender.send(&b"late"[..], SendOptions::default()),
        Err(KError::BrokenPipe)
    );
}

#[def_test]
fn accepted_socket_inherits_listener_pass_credentials() {
    let listener = SeqpacketTransport::new();
    listener
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    let client = SeqpacketTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    let UnixTransport::Seqpacket(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("seqpacket listener returned another transport");
    };

    let mut enabled = false;
    accepted
        .get_option(GetSocketOption::PassCredentials(&mut enabled))
        .unwrap();
    assert!(enabled);
}

#[def_test]
fn shutdown_write_keeps_the_connection_and_queued_input() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    let client = SeqpacketTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    let UnixTransport::Seqpacket(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("seqpacket listener returned another transport");
    };

    // Data already in flight when the client half-closes must remain readable.
    accepted.send(&b"queued"[..], SendOptions::default()).unwrap();
    accepted.shutdown(Shutdown::Write).unwrap();
    client.shutdown(Shutdown::Write).unwrap();

    assert_eq!(
        client
            .connect(
                &entry,
                &UnixAddr::Unbound,
                ConnectOptions::default(),
                &test_peer_credentials(),
            )
            .err(),
        Some(KError::AlreadyConnected)
    );

    // A half-closed connection still has an identity.
    let mut peer = PeerCredentials::none();
    client
        .get_option(GetSocketOption::PeerCredentials(&mut peer))
        .unwrap();
    let peer = peer.resolve_in(kidentity::root_pid_namespace());
    assert_eq!(peer.pid, 7);
    assert_eq!(peer.uid, 1000);
    assert_eq!(peer.gid, 1001);

    let mut buf = [0u8; 8];
    assert_eq!(client.recv(&mut buf[..], RecvOptions::default()), Ok(6));
    assert_eq!(&buf[..6], b"queued");
    assert_eq!(client.recv(&mut buf[..], RecvOptions::default()), Ok(0));
}

#[def_test]
fn a_bound_client_still_reports_connection_readiness() {
    let listener = SeqpacketTransport::new();
    let server_entry = BindEntry::default();
    listener.bind(&server_entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    // A client can bind before connecting. Its own listener queue must not
    // mask readiness of the accepted connection.
    let client = SeqpacketTransport::new();
    let client_entry = BindEntry::default();
    client.bind(&client_entry, &UnixAddr::Unbound).unwrap();
    client
        .connect(
            &server_entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    let UnixTransport::Seqpacket(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("seqpacket listener returned another transport");
    };

    accepted.send(&b"ping"[..], SendOptions::default()).unwrap();
    assert!(client.poll().contains(IoEvents::IN));

    accepted.shutdown(Shutdown::Write).unwrap();
    let events = client.poll();
    assert!(events.contains(IoEvents::IN | IoEvents::RDHUP));
    assert!(!events.contains(IoEvents::HUP));

    accepted.shutdown(Shutdown::Read).unwrap();
    let events = client.poll();
    assert!(events.contains(IoEvents::IN | IoEvents::RDHUP | IoEvents::HUP));
}

#[def_test]
fn read_shutdown_before_connect_applies_to_the_connection() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    let client = SeqpacketTransport::new();
    client.shutdown(Shutdown::Read).unwrap();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    let UnixTransport::Seqpacket(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("seqpacket listener returned another transport");
    };

    // The peer cannot enqueue records the client would read after EOF.
    assert_eq!(
        accepted.send(&b"late"[..], SendOptions::default()),
        Err(KError::BrokenPipe)
    );
    let events = client.poll();
    assert!(events.contains(IoEvents::IN | IoEvents::RDHUP));
    assert!(!events.contains(IoEvents::HUP));

    let mut buf = [0u8; 8];
    let nonblocking = RecvOptions {
        flags: RecvFlags::DONT_WAIT,
        ..RecvOptions::default()
    };
    assert_eq!(client.recv(&mut buf[..], nonblocking), Ok(0));

    // Only the read direction was shut down, so sending still works.
    assert_eq!(client.send(&b"pong"[..], SendOptions::default()), Ok(4));
    assert_eq!(accepted.recv(&mut buf[..], RecvOptions::default()), Ok(4));
    assert_eq!(&buf[..4], b"pong");
}

#[def_test]
fn peer_write_shutdown_reports_rdhup_not_hup() {
    let (left, right) = SeqpacketTransport::new_pair(test_peer_credentials());
    right.shutdown(Shutdown::Write).unwrap();

    let events = left.poll();
    assert!(events.contains(IoEvents::IN | IoEvents::RDHUP));
    assert!(!events.contains(IoEvents::HUP));
    // Only the peer's write direction is gone; the local direction still works.
    assert_eq!(left.send(&b"still-open"[..], SendOptions::default()), Ok(10));

    left.shutdown(Shutdown::Write).unwrap();
    assert!(left.poll().contains(IoEvents::HUP));
}

#[def_test]
fn listener_read_shutdown_drains_accepts_then_refuses_connections() {
    let listener = SeqpacketTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    let client = SeqpacketTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();

    listener.shutdown(Shutdown::Read).unwrap();
    assert!(listener.poll().contains(IoEvents::IN | IoEvents::RDHUP));
    assert!(!listener.poll().contains(IoEvents::HUP));

    // A queued connection is not discarded by the read shutdown.
    assert!(block_on(listener.accept(true)).is_ok());
    assert_eq!(
        block_on(listener.accept(true)).map(|_| ()),
        Err(KError::InvalidInput)
    );

    let late_client = SeqpacketTransport::new();
    assert_eq!(
        late_client
            .connect(
                &entry,
                &UnixAddr::Unbound,
                ConnectOptions::default(),
                &test_peer_credentials(),
            )
            .err(),
        Some(KError::ConnectionRefused)
    );
}

#[def_test(serial)]
fn listener_read_shutdown_wakes_blocked_accept() {
    let listener = Arc::new(SeqpacketTransport::new());
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();

    let accept_result = Arc::new(Mutex::new(None));
    let accept_task = ktask::spawn({
        let listener = listener.clone();
        let accept_result = accept_result.clone();
        move || {
            *accept_result.lock() = Some(block_on(listener.accept(false)).map(|_| ()));
        }
    });
    let acceptor_is_waiting = wait_until(|| accept_task.state() == ktask::TaskState::Blocked);
    if !acceptor_is_waiting {
        listener.shutdown(Shutdown::Read).unwrap();
        accept_task.join();
    }
    assert!(acceptor_is_waiting);

    listener.shutdown(Shutdown::Read).unwrap();
    accept_task.join();
    assert_eq!(
        accept_result.lock().take().unwrap(),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn read_shutdown_drains_queued_records_and_rejects_peer_sends() {
    let (left, right) = SeqpacketTransport::new_pair(test_peer_credentials());
    right.send(&b"queued"[..], SendOptions::default()).unwrap();

    left.shutdown(Shutdown::Read).unwrap();
    assert!(left.poll().contains(IoEvents::RDHUP));

    let mut buf = [0u8; 8];
    assert_eq!(left.recv(&mut buf[..], RecvOptions::default()), Ok(6));
    assert_eq!(&buf[..6], b"queued");
    assert_eq!(left.recv(&mut buf[..], RecvOptions::default()), Ok(0));
    assert_eq!(
        right.send(&b"late"[..], SendOptions::default()),
        Err(KError::BrokenPipe)
    );
}

/// `SO_ACCEPTCONN` reports whether the socket reached `TCP_LISTEN`.
///
/// `unix_listen` accepts `SOCK_SEQPACKET`, so `sk_getsockopt` answers from
/// `sk_state` here too. The transport delegates to the shared general options,
/// and that handler answers `AcceptConn` with a constant `false` *and* reports
/// the option handled, so a seqpacket socket must answer before delegating; the
/// regression this pins made a listening seqpacket socket report 0.
#[def_test]
fn accept_conn_follows_the_listener_state() {
    use super::{Listener, ListenerState};

    let transport = SeqpacketTransport::new();
    let mut listening = false;
    transport
        .get_option(GetSocketOption::AcceptConn(&mut listening))
        .expect("AcceptConn is a get-only option");
    assert!(!listening, "a fresh socket is not listening");

    // Installing a listener in the `Listening` state is what `listen()` leaves
    // behind; nothing else changes the answer.
    let (_sender, pending) = async_channel::unbounded();
    *transport.listener.lock() = Some(Listener {
        pending,
        poll: Arc::new(PollSet::new()),
        state: Arc::new(Mutex::new(ListenerState::Listening)),
    });
    let mut listening = false;
    transport
        .get_option(GetSocketOption::AcceptConn(&mut listening))
        .expect("AcceptConn is a get-only option");
    assert!(listening, "a listening seqpacket socket reports SO_ACCEPTCONN");

    // A listener whose accept side was shut down is no longer listening, which
    // is what `unix_listen`'s state machine reports too.
    *transport.listener.lock() = Some(Listener {
        pending: async_channel::unbounded().1,
        poll: Arc::new(PollSet::new()),
        state: Arc::new(Mutex::new(ListenerState::AcceptShutdown)),
    });
    let mut listening = true;
    transport
        .get_option(GetSocketOption::AcceptConn(&mut listening))
        .expect("AcceptConn is a get-only option");
    assert!(!listening, "a shut-down listener is not accepting");
}

/// `SIOCINQ`/`SIOCOUTQ` report the queued byte totals for a seqpacket pair.
///
/// Linux `unix_inq_len` sums every queued skb for `SOCK_STREAM`/`SOCK_SEQPACKET`
/// and `unix_outq_len` reports the sender's queued bytes, so both sides are
/// plain running totals. A socket that only inherited the trait default answered
/// 0/0 for a connected pair with data in flight.
#[def_test]
fn queue_lengths_report_queued_bytes_per_direction() {
    let (sender, receiver) = SeqpacketTransport::new_pair(test_peer_credentials());

    let lengths = sender
        .queue_lengths()
        .expect("seqpacket reports queue lengths");
    assert_eq!((lengths.outq, lengths.inq), (0, 0), "an idle pair queues nothing");

    assert_eq!(sender.send(&b"hello"[..], SendOptions::default()), Ok(5));
    assert_eq!(sender.send(&b"world!"[..], SendOptions::default()), Ok(6));

    // The sender reports what it handed to the peer and has nothing to read.
    let lengths = sender.queue_lengths().unwrap();
    assert_eq!((lengths.outq, lengths.inq), (11, 0));

    // The receiver sees the same 11 bytes as readable.
    let lengths = receiver.queue_lengths().unwrap();
    assert_eq!((lengths.outq, lengths.inq), (0, 11));

    // Reading one message drops exactly its bytes from both views.
    let mut buf = [0u8; 16];
    assert_eq!(receiver.recv(&mut buf[..], RecvOptions::default()), Ok(5));
    let lengths = sender.queue_lengths().unwrap();
    assert_eq!(lengths.outq, 6, "the acknowledged message is no longer queued");
    let lengths = receiver.queue_lengths().unwrap();
    assert_eq!(lengths.inq, 6);
}
