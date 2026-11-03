// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    sync::{Arc, Weak},
    task::Wake,
    vec,
    vec::Vec,
};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use kerrno::{KError, KResult, LinuxError};
use kio::{IoBufMut, Write};
use kpoll::{IoEvents, PollRegistrations, PollSet, Pollable};
use ksync::Mutex;
use ktask::future::block_on;
use ktime_types::TimeSpan;
use ringbuf::traits::Observer;
use unittest::{assert, assert_eq, def_test};

use super::{
    StreamTransport, UnixTransportOps,
    channel::{
        MAX_PENDING_ANCILLARY, MAX_PENDING_RECORDS, STREAM_BUF_BYTES,
        STREAM_WRITABLE_MAX_OCCUPIED_BYTES,
    },
};
use crate::{
    AncillaryData, ConnectOptions, KernelAncillaryData, RecvFlags, RecvOptions, SendFlags,
    SendOptions, Shutdown,
    options::{Configurable, GetSocketOption, PeerCredentials, SetSocketOption, UnixCredentials},
    unix::{BindEntry, UnixAddr, UnixDomainSocket, UnixSocketType, UnixTransport},
};

const TASK_WAIT_ROUNDS: usize = 100_000;

fn test_peer_credentials() -> PeerCredentials {
    PeerCredentials::new(kidentity::PidHandle::fixed_root(7), 1000, 1001)
}

fn peer_credentials_for(pid: u32, uid: u32, gid: u32) -> PeerCredentials {
    PeerCredentials::new(kidentity::PidHandle::fixed_root(pid), uid, gid)
}

fn resolved_peer_credentials(socket: &impl Configurable) -> UnixCredentials {
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

#[derive(Default)]
struct WakeCounter(AtomicUsize);

struct ChannelLockProbe {
    socket: Weak<StreamTransport>,
    was_unlocked: AtomicBool,
}

struct PausingWriter {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    byte: Option<u8>,
    has_paused: bool,
}

impl Write for PausingWriter {
    fn write(&mut self, buf: &[u8]) -> KResult<usize> {
        if !self.has_paused {
            self.has_paused = true;
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                ktask::yield_now();
            }
        }
        let Some(byte) = buf.first() else {
            return Ok(0);
        };
        self.byte = Some(*byte);
        Ok(1)
    }

    fn flush(&mut self) -> KResult<()> {
        Ok(())
    }
}

impl IoBufMut for PausingWriter {
    fn remaining_mut(&self) -> usize {
        usize::from(self.byte.is_none())
    }
}

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl Wake for ChannelLockProbe {
    fn wake(self: Arc<Self>) {
        let was_unlocked = self
            .socket
            .upgrade()
            .is_some_and(|socket| socket.channel.try_lock().is_some());
        self.was_unlocked.store(was_unlocked, Ordering::SeqCst);
    }
}

struct Registration {
    counter: Arc<WakeCounter>,
    _registrations: PollRegistrations,
}

impl core::ops::Deref for Registration {
    type Target = WakeCounter;

    fn deref(&self) -> &Self::Target {
        &self.counter
    }
}

fn register(socket: &StreamTransport, events: IoEvents) -> Registration {
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    socket
        .register(&mut registrations.context(&context), events)
        .unwrap();
    Registration {
        counter,
        _registrations: registrations,
    }
}

fn register_channel_lock_probe(
    poll_set: &PollSet,
    socket: &Arc<StreamTransport>,
) -> (Arc<ChannelLockProbe>, PollRegistrations) {
    let probe = Arc::new(ChannelLockProbe {
        socket: Arc::downgrade(socket),
        was_unlocked: AtomicBool::new(false),
    });
    let waker = Waker::from(probe.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    registrations.context(&context).register(poll_set).unwrap();
    (probe, registrations)
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
fn unix_stream_data_flow_wakes_only_the_affected_direction() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let left_read = register(&left, IoEvents::IN);
    let left_write = register(&left, IoEvents::OUT);
    let right_read = register(&right, IoEvents::IN);
    let right_read_second = register(&right, IoEvents::IN);
    let right_write = register(&right, IoEvents::OUT);
    let right_state = register(&right, IoEvents::HUP);

    assert_eq!(left.send(&b"x"[..], SendOptions::default()).unwrap(), 1);
    assert_eq!(left_read.0.load(Ordering::SeqCst), 0);
    assert_eq!(left_write.0.load(Ordering::SeqCst), 0);
    assert_eq!(right_read.0.load(Ordering::SeqCst), 1);
    assert_eq!(right_read_second.0.load(Ordering::SeqCst), 1);
    assert_eq!(right_write.0.load(Ordering::SeqCst), 0);
    assert_eq!(right_state.0.load(Ordering::SeqCst), 0);

    let mut byte = [0_u8; 1];
    assert_eq!(
        right.recv(&mut byte[..], RecvOptions::default()).unwrap(),
        1
    );
    assert_eq!(byte, [b'x']);
    assert_eq!(left_read.0.load(Ordering::SeqCst), 0);
    assert_eq!(left_write.0.load(Ordering::SeqCst), 0);
    assert_eq!(right_write.0.load(Ordering::SeqCst), 0);
    assert_eq!(right_state.0.load(Ordering::SeqCst), 0);
}

#[def_test]
fn unix_stream_wakes_writers_only_after_crossing_the_low_watermark() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let fill = vec![b'x'; STREAM_BUF_BYTES];
    assert_eq!(
        left.send(&fill[..], SendOptions::default()),
        Ok(STREAM_BUF_BYTES)
    );
    assert!(!left.poll().contains(IoEvents::OUT));

    let left_write = register(&left, IoEvents::OUT);
    let mut byte = [0_u8; 1];
    assert_eq!(right.recv(&mut byte[..], RecvOptions::default()), Ok(1));
    assert_eq!(left_write.0.load(Ordering::SeqCst), 0);
    assert!(!left.poll().contains(IoEvents::OUT));

    let bytes_to_low_watermark = STREAM_BUF_BYTES - STREAM_WRITABLE_MAX_OCCUPIED_BYTES - 1;
    let mut data = vec![0_u8; bytes_to_low_watermark];
    assert_eq!(
        right.recv(&mut data[..], RecvOptions::default()),
        Ok(bytes_to_low_watermark)
    );
    assert_eq!(left_write.0.load(Ordering::SeqCst), 1);
    assert!(left.poll().contains(IoEvents::OUT));
}

#[def_test]
fn unix_stream_nonblocking_send_returns_partial_or_would_block() {
    let (left, _right) = StreamTransport::new_pair(test_peer_credentials());
    let fill = vec![b'x'; STREAM_BUF_BYTES - 1];

    assert_eq!(
        left.send(&fill[..], SendOptions::default()),
        Ok(STREAM_BUF_BYTES - 1)
    );

    let nonblocking = || SendOptions {
        flags: SendFlags::DONT_WAIT,
        ..SendOptions::default()
    };
    assert_eq!(left.send(&b"yz"[..], nonblocking()), Ok(1));
    assert!(matches!(
        left.send(&b"z"[..], nonblocking()),
        Err(KError::WouldBlock)
    ));
    assert_eq!(left.send(&b""[..], nonblocking()), Ok(0));
}

#[def_test]
fn unix_stream_zero_length_recv_preserves_pending_data() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let nonblocking = || RecvOptions {
        flags: RecvFlags::DONT_WAIT,
        ..RecvOptions::default()
    };
    let mut empty = [];

    assert_eq!(
        right.recv(&mut empty[..], nonblocking()),
        Err(KError::WouldBlock)
    );
    assert_eq!(left.send(&b"x"[..], SendOptions::default()), Ok(1));
    assert_eq!(right.recv(&mut empty[..], nonblocking()), Ok(0));

    let mut byte = [0_u8; 1];
    assert_eq!(right.recv(&mut byte[..], nonblocking()), Ok(1));
    assert_eq!(byte, [b'x']);
}

#[def_test]
fn unix_stream_shutdown_write_wakes_peer_read_and_reports_eof() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let right_read = register(&right, IoEvents::IN);
    let right_write = register(&right, IoEvents::OUT);
    let right_state = register(&right, IoEvents::RDHUP);

    left.shutdown(Shutdown::Write).unwrap();

    assert_eq!(right_read.0.load(Ordering::SeqCst), 1);
    assert_eq!(right_write.0.load(Ordering::SeqCst), 0);
    assert_eq!(right_state.0.load(Ordering::SeqCst), 1);
    assert!(right.poll().contains(IoEvents::IN | IoEvents::RDHUP));

    let mut byte = [0_u8; 1];
    assert_eq!(
        right.recv(&mut byte[..], RecvOptions::default()).unwrap(),
        0
    );
    assert!(matches!(
        left.send(&b"x"[..], SendOptions::default()),
        Err(KError::BrokenPipe)
    ));
}

#[def_test]
fn unix_stream_hup_wakes_combined_direction_waiters() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    right.shutdown(Shutdown::Write).unwrap();
    assert!(!left.poll().contains(IoEvents::HUP));

    let read_or_hup = register(&left, IoEvents::IN | IoEvents::HUP);
    let hup = register(&left, IoEvents::HUP);
    left.shutdown(Shutdown::Write).unwrap();

    assert_eq!(read_or_hup.0.load(Ordering::SeqCst), 1);
    assert_eq!(hup.0.load(Ordering::SeqCst), 1);
    assert!(left.poll().contains(IoEvents::HUP));

    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    left.shutdown(Shutdown::Write).unwrap();
    assert!(!left.poll().contains(IoEvents::HUP));

    let write_or_hup = register(&left, IoEvents::OUT | IoEvents::HUP);
    let hup = register(&left, IoEvents::HUP);
    right.shutdown(Shutdown::Write).unwrap();

    assert_eq!(write_or_hup.0.load(Ordering::SeqCst), 1);
    assert_eq!(hup.0.load(Ordering::SeqCst), 1);
    assert!(left.poll().contains(IoEvents::HUP));
}

#[def_test]
fn unix_stream_shutdown_wakes_after_releasing_channel_lock() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let left = Arc::new(left);
    let (local_wake_saw_unlocked, _local_registration) =
        register_channel_lock_probe(&left.endpoint.polls.writable, &left);
    let (peer_wake_saw_unlocked, _peer_registration) =
        register_channel_lock_probe(&right.endpoint.polls.readable, &left);

    left.shutdown(Shutdown::Write).unwrap();

    assert!(local_wake_saw_unlocked.was_unlocked.load(Ordering::SeqCst));
    assert!(peer_wake_saw_unlocked.was_unlocked.load(Ordering::SeqCst));
}

#[def_test]
fn unix_stream_shutdown_read_drains_buffer_before_eof() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());

    assert_eq!(left.send(&b"abc"[..], SendOptions::default()).unwrap(), 3);
    right.shutdown(Shutdown::Read).unwrap();

    let mut data = [0_u8; 3];
    assert_eq!(
        right.recv(&mut data[..], RecvOptions::default()).unwrap(),
        3
    );
    assert_eq!(data, *b"abc");
    assert_eq!(
        right.recv(&mut data[..], RecvOptions::default()).unwrap(),
        0
    );
    assert!(matches!(
        left.send(&b"x"[..], SendOptions::default()),
        Err(KError::BrokenPipe)
    ));
}

#[def_test(serial)]
fn unix_stream_eof_waits_for_data_published_after_empty_snapshot() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let left = Arc::new(left);
    let right = Arc::new(right);
    let recv_entered = Arc::new(AtomicBool::new(false));
    let recv_release = Arc::new(AtomicBool::new(false));
    let recv_result = Arc::new(Mutex::new(None));

    let recv_task = ktask::spawn({
        let right = right.clone();
        let recv_entered = recv_entered.clone();
        let recv_release = recv_release.clone();
        let recv_result = recv_result.clone();
        move || {
            let mut writer = PausingWriter {
                entered: recv_entered,
                release: recv_release,
                byte: None,
                has_paused: false,
            };
            let result = right.recv(&mut writer, RecvOptions::default());
            *recv_result.lock() = Some((result, writer.byte));
        }
    });
    let recv_is_paused = wait_until(|| recv_entered.load(Ordering::Acquire));
    if !recv_is_paused {
        recv_release.store(true, Ordering::Release);
        left.shutdown(Shutdown::Write).unwrap();
        recv_task.join();
    }
    assert!(recv_is_paused);

    let send_result = Arc::new(Mutex::new(None));
    let send_task = ktask::spawn({
        let left = left.clone();
        let send_result = send_result.clone();
        move || {
            *send_result.lock() = Some(left.send(&b"x"[..], SendOptions::default()));
        }
    });
    send_task.join();

    let shutdown_result = Arc::new(Mutex::new(None));
    let shutdown_task = ktask::spawn({
        let left = left.clone();
        let shutdown_result = shutdown_result.clone();
        move || {
            *shutdown_result.lock() = Some(left.shutdown(Shutdown::Write));
        }
    });
    shutdown_task.join();

    recv_release.store(true, Ordering::Release);
    recv_task.join();

    assert_eq!(send_result.lock().take().unwrap(), Ok(1));
    assert_eq!(shutdown_result.lock().take().unwrap(), Ok(()));
    let (result, byte) = recv_result.lock().take().unwrap();
    assert_eq!(result, Ok(1));
    assert_eq!(byte, Some(b'x'));

    let mut byte = [0_u8; 1];
    assert_eq!(
        right.recv(&mut byte[..], RecvOptions::default()).unwrap(),
        0
    );
}

#[def_test(serial)]
fn unix_stream_shutdown_write_preserves_partial_send() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let left = Arc::new(left);
    let right = Arc::new(right);
    let send_result = Arc::new(Mutex::new(None));

    let send_task = ktask::spawn({
        let left = left.clone();
        let send_result = send_result.clone();
        move || {
            let data = vec![b'x'; STREAM_BUF_BYTES + 1];
            *send_result.lock() = Some(left.send(&data[..], SendOptions::default()));
        }
    });
    let buffer_is_full = wait_until(|| {
        right
            .channel
            .lock()
            .as_ref()
            .is_some_and(|channel| channel.rx.occupied_len() == STREAM_BUF_BYTES)
    });
    if !buffer_is_full {
        left.shutdown(Shutdown::Write).unwrap();
        send_task.join();
    }
    assert!(buffer_is_full);

    left.shutdown(Shutdown::Write).unwrap();
    send_task.join();
    assert_eq!(send_result.lock().take().unwrap(), Ok(STREAM_BUF_BYTES));

    let mut data: Vec<u8> = vec![0; STREAM_BUF_BYTES];
    assert_eq!(
        right.recv(&mut data[..], RecvOptions::default()).unwrap(),
        STREAM_BUF_BYTES
    );
    assert!(data.iter().all(|byte| *byte == b'x'));
    assert_eq!(
        right.recv(&mut data[..1], RecvOptions::default()).unwrap(),
        0
    );
}

#[def_test]
fn unix_stream_shutdown_read_does_not_wake_local_write_waiters() {
    let (left, _right) = StreamTransport::new_pair(test_peer_credentials());
    let left_read = register(&left, IoEvents::IN);
    let left_write = register(&left, IoEvents::OUT);

    left.shutdown(Shutdown::Read).unwrap();

    assert_eq!(left_read.0.load(Ordering::SeqCst), 1);
    assert_eq!(left_write.0.load(Ordering::SeqCst), 0);
}

#[def_test]
fn unix_stream_peer_drop_wakes_connection_waiters_once() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let right_read = register(&right, IoEvents::IN);
    let right_write = register(&right, IoEvents::OUT);

    drop(left);

    assert_eq!(right_read.0.load(Ordering::SeqCst), 1);
    assert_eq!(right_write.0.load(Ordering::SeqCst), 1);
    assert!(
        right
            .poll()
            .contains(IoEvents::IN | IoEvents::RDHUP | IoEvents::HUP)
    );

    let mut byte = [0_u8; 1];
    assert_eq!(
        right.recv(&mut byte[..], RecvOptions::default()).unwrap(),
        0
    );
    assert!(matches!(
        right.send(&b"x"[..], SendOptions::default()),
        Err(KError::BrokenPipe)
    ));
}

#[def_test]
fn unix_stream_peer_drop_with_unread_input_reports_reset_once() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    assert_eq!(left.send(&b"abc"[..], SendOptions::default()), Ok(3));
    assert_eq!(right.send(&b"r"[..], SendOptions::default()), Ok(1));
    drop(right);

    assert!(left.poll().contains(IoEvents::ERR));
    let mut byte = [0_u8; 1];
    assert_eq!(left.recv(&mut byte[..], RecvOptions::default()), Ok(1));
    assert_eq!(byte, [b'r']);
    assert_eq!(
        left.recv(&mut byte[..], RecvOptions::default()),
        Err(KError::ConnectionReset)
    );
    assert!(!left.poll().contains(IoEvents::ERR));
    assert_eq!(left.recv(&mut byte[..], RecvOptions::default()), Ok(0));

    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    assert_eq!(left.send(&b"abc"[..], SendOptions::default()), Ok(3));
    drop(right);
    let mut error = 0;
    left.get_option(GetSocketOption::Error(&mut error)).unwrap();
    assert_eq!(error, LinuxError::ECONNRESET.into_raw());
    left.get_option(GetSocketOption::Error(&mut error)).unwrap();
    assert_eq!(error, 0);
    assert_eq!(left.recv(&mut byte[..], RecvOptions::default()), Ok(0));
}

#[def_test]
fn unix_stream_connection_changes_rearm_existing_waiters() {
    let listener = StreamTransport::new();
    let client = StreamTransport::new();
    let entry = BindEntry::default();
    let listener_read = register(&listener, IoEvents::IN);

    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    assert_eq!(listener_read.0.load(Ordering::SeqCst), 1);
    listener.listen(1, &test_peer_credentials()).unwrap();

    let client_read = register(&client, IoEvents::IN);
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    assert_eq!(client_read.0.load(Ordering::SeqCst), 1);
}

#[def_test]
fn unix_stream_backlog_limits_pending_connections() {
    let listener = StreamTransport::new();
    let first = StreamTransport::new();
    let second = StreamTransport::new();
    let third = StreamTransport::new();
    let entry = BindEntry::default();
    let nonblocking = ConnectOptions { nonblocking: true };

    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();
    first
        .connect(
            &entry,
            &UnixAddr::Unbound,
            nonblocking,
            &test_peer_credentials(),
        )
        .unwrap();
    assert_eq!(
        first.connect(
            &entry,
            &UnixAddr::Unbound,
            nonblocking,
            &test_peer_credentials(),
        ),
        Err(KError::AlreadyConnected)
    );
    second
        .connect(
            &entry,
            &UnixAddr::Unbound,
            nonblocking,
            &test_peer_credentials(),
        )
        .unwrap();
    assert_eq!(
        third.connect(
            &entry,
            &UnixAddr::Unbound,
            nonblocking,
            &test_peer_credentials(),
        ),
        Err(KError::WouldBlock)
    );

    let accepted = block_on(listener.accept(true)).unwrap();
    third
        .connect(
            &entry,
            &UnixAddr::Unbound,
            nonblocking,
            &test_peer_credentials(),
        )
        .unwrap();
    drop(accepted);
}

#[def_test]
fn unix_stream_connect_honors_send_timeout_when_backlog_is_full() {
    let listener = StreamTransport::new();
    let queued = StreamTransport::new();
    let waiting = StreamTransport::new();
    let entry = BindEntry::default();

    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(0, &test_peer_credentials()).unwrap();
    queued
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();

    let send_timeout = TimeSpan::from_nanos(1);
    waiting
        .set_option(SetSocketOption::SendTimeout(&send_timeout))
        .unwrap();
    assert_eq!(
        waiting.connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        ),
        Err(KError::WouldBlock)
    );
}

#[def_test(serial)]
fn unix_stream_accept_releases_blocked_connector() {
    let listener = Arc::new(StreamTransport::new());
    let queued_client = StreamTransport::new();
    let waiting_client = Arc::new(StreamTransport::new());
    let entry = Arc::new(BindEntry::default());

    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(0, &test_peer_credentials()).unwrap();
    queued_client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();

    let connect_result = Arc::new(Mutex::new(None));
    let connect_task = ktask::spawn({
        let waiting_client = waiting_client.clone();
        let entry = entry.clone();
        let connect_result = connect_result.clone();
        move || {
            *connect_result.lock() = Some(waiting_client.connect(
                &entry,
                &UnixAddr::Unbound,
                ConnectOptions::default(),
                &test_peer_credentials(),
            ));
        }
    });
    let connector_is_waiting = wait_until(|| connect_task.state() == ktask::TaskState::Blocked);
    if !connector_is_waiting {
        listener.shutdown(Shutdown::Read).unwrap();
        connect_task.join();
    }
    assert!(connector_is_waiting);

    let first_accepted = block_on(listener.accept(false)).unwrap();
    connect_task.join();
    assert_eq!(connect_result.lock().take().unwrap(), Ok(()));
    let (second_accepted, _) = block_on(listener.accept(true)).unwrap();
    drop((first_accepted, second_accepted));
}

#[def_test]
fn unix_stream_repeated_listen_updates_backlog() {
    let listener = StreamTransport::new();
    let first = StreamTransport::new();
    let second = StreamTransport::new();
    let entry = BindEntry::default();
    let nonblocking = ConnectOptions { nonblocking: true };

    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(0, &test_peer_credentials()).unwrap();
    first
        .connect(
            &entry,
            &UnixAddr::Unbound,
            nonblocking,
            &test_peer_credentials(),
        )
        .unwrap();
    assert_eq!(
        second.connect(
            &entry,
            &UnixAddr::Unbound,
            nonblocking,
            &test_peer_credentials(),
        ),
        Err(KError::WouldBlock)
    );

    listener.listen(1, &test_peer_credentials()).unwrap();
    second
        .connect(
            &entry,
            &UnixAddr::Unbound,
            nonblocking,
            &test_peer_credentials(),
        )
        .unwrap();
}

#[def_test]
fn unix_stream_listening_socket_cannot_connect() {
    let listener = StreamTransport::new();
    let entry = BindEntry::default();

    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();
    assert_eq!(
        listener.connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        ),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        block_on(listener.accept(true)).map(|_| ()),
        Err(KError::WouldBlock)
    );
}

#[def_test]
fn unix_stream_stale_listener_returns_connection_refused() {
    let entry = BindEntry::default();
    {
        let listener = StreamTransport::new();
        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(1, &test_peer_credentials()).unwrap();
    }

    let client = StreamTransport::new();
    assert_eq!(
        client.connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        ),
        Err(KError::ConnectionRefused)
    );
}

#[def_test(serial)]
fn unix_stream_listener_shutdown_wakes_connect_and_rejects_empty_accept() {
    let listener = Arc::new(StreamTransport::new());
    let queued_client = StreamTransport::new();
    let waiting_client = Arc::new(StreamTransport::new());
    let entry = Arc::new(BindEntry::default());

    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(0, &test_peer_credentials()).unwrap();
    queued_client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();

    let connect_result = Arc::new(Mutex::new(None));
    let connect_task = ktask::spawn({
        let waiting_client = waiting_client.clone();
        let entry = entry.clone();
        let connect_result = connect_result.clone();
        move || {
            *connect_result.lock() = Some(waiting_client.connect(
                &entry,
                &UnixAddr::Unbound,
                ConnectOptions::default(),
                &test_peer_credentials(),
            ));
        }
    });
    let connector_is_waiting = wait_until(|| connect_task.state() == ktask::TaskState::Blocked);
    if !connector_is_waiting {
        listener.shutdown(Shutdown::Read).unwrap();
        connect_task.join();
    }
    assert!(connector_is_waiting);

    listener.shutdown(Shutdown::Read).unwrap();
    connect_task.join();
    assert_eq!(
        connect_result.lock().take().unwrap(),
        Err(KError::ConnectionRefused)
    );

    let accepted = block_on(listener.accept(false)).unwrap();
    drop(accepted);
    assert_eq!(
        block_on(listener.accept(false)).map(|_| ()),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        block_on(listener.accept(true)).map(|_| ()),
        Err(KError::WouldBlock)
    );
}

#[def_test(serial)]
fn unix_stream_listener_shutdown_wakes_blocked_accept() {
    let listener = Arc::new(StreamTransport::new());
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
fn unix_stream_listener_does_not_leave_a_stale_read_registration() {
    let listener = StreamTransport::new();
    let client = StreamTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();
    let listener_read = register(&listener, IoEvents::IN);

    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    assert_eq!(listener_read.0.load(Ordering::SeqCst), 1);

    listener.endpoint.polls.readable.wake();
    assert_eq!(listener_read.0.load(Ordering::SeqCst), 1);
}

#[def_test]
fn unix_stream_delivers_ancillary_once_with_sink() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    let send_options = SendOptions {
        ancillary: vec![Arc::new(7_u32) as AncillaryData, Arc::new(8_u32)],
        ..SendOptions::default()
    };
    sender.send(&b"xy"[..], send_options).unwrap();

    let mut sink: Vec<AncillaryData> = Vec::new();
    let mut buf = [0_u8; 2];
    let n = receiver
        .recv(
            &mut buf[..],
            RecvOptions {
                ancillary: Some(&mut sink),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(buf, *b"xy");
    // every fd of the consumed range arrives exactly once
    assert_eq!(sink.len(), 2);
    assert_eq!(sink[0].downcast_ref::<u32>(), Some(&7));
    assert_eq!(sink[1].downcast_ref::<u32>(), Some(&8));

    // no duplicate delivery on the next consume
    let mut sink2: Vec<AncillaryData> = Vec::new();
    let r = receiver.recv(
        &mut buf[..],
        RecvOptions {
            ancillary: Some(&mut sink2),
            ..RecvOptions::default()
        },
    );
    assert!(matches!(r, Err(KError::WouldBlock)));
    assert!(sink2.is_empty());
}

#[def_test]
fn unix_stream_drops_unsunk_ancillary_without_late_delivery() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    let send_options = SendOptions {
        ancillary: vec![Arc::new(9_u32) as AncillaryData],
        ..SendOptions::default()
    };
    sender.send(&b"a"[..], send_options).unwrap();

    let mut buf = [0_u8; 1];
    let n = receiver.recv(&mut buf[..], RecvOptions::default()).unwrap();
    assert_eq!(n, 1);
    assert_eq!(buf[0], b'a');

    // a later sink-consuming recv must not see the dropped record
    let send_options = SendOptions {
        ancillary: vec![Arc::new(10_u32) as AncillaryData],
        ..SendOptions::default()
    };
    sender.send(&b"b"[..], send_options).unwrap();
    let mut sink: Vec<AncillaryData> = Vec::new();
    let n = receiver
        .recv(
            &mut buf[..],
            RecvOptions {
                ancillary: Some(&mut sink),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(buf[0], b'b');
    assert_eq!(sink.len(), 1);
    assert_eq!(sink[0].downcast_ref::<u32>(), Some(&10));
}

#[def_test]
fn unix_stream_receiver_close_releases_unsunk_records() {
    // Drop-observable ancillary payload: if the queued record survives
    // the receiver's close (the pre-take_all behaviour, or a reference
    // cycle pinning the file), the flag stays unset.
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    let flag = Arc::new(AtomicBool::new(false));
    let send_options = SendOptions {
        ancillary: vec![Arc::new(DropFlag(flag.clone())) as AncillaryData],
        ..SendOptions::default()
    };
    sender.send(&b"a"[..], send_options).unwrap();

    drop(receiver);

    assert!(
        flag.load(Ordering::SeqCst),
        "queued record must be released when the receiver closes"
    );
}

#[def_test]
fn unix_stream_recv_stops_at_ancillary_barrier() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();

    // two sendmsg boundaries, each carrying its own record
    let so1 = SendOptions {
        ancillary: vec![Arc::new(1_u32) as AncillaryData],
        ..SendOptions::default()
    };
    sender.send(&b"a"[..], so1).unwrap();
    let so2 = SendOptions {
        ancillary: vec![Arc::new(2_u32) as AncillaryData],
        ..SendOptions::default()
    };
    sender.send(&b"bb"[..], so2).unwrap();

    // one large recv: Linux stops this read at the first fd segment
    let mut sink: Vec<AncillaryData> = Vec::new();
    let mut buf = [0_u8; 16];
    let n = receiver
        .recv(
            &mut buf[..],
            RecvOptions {
                ancillary: Some(&mut sink),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(buf[0], b'a');
    assert_eq!(sink.len(), 1);
    assert_eq!(sink[0].downcast_ref::<u32>(), Some(&1));

    // the second segment and its record wait for the next recv
    let mut sink2: Vec<AncillaryData> = Vec::new();
    let n = receiver
        .recv(
            &mut buf[..],
            RecvOptions {
                ancillary: Some(&mut sink2),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(&buf[..2], b"bb");
    assert_eq!(sink2.len(), 1);
    assert_eq!(sink2[0].downcast_ref::<u32>(), Some(&2));
}

#[def_test]
fn unix_stream_wrapped_recv_has_no_duplicate_bytes() {
    // Regression for the duplicated second-slice write: fill the ring
    // so the readable region wraps across the buffer end, then drain
    // it with a buffer larger than the occupied bytes. Pre-fix, the
    // tail slice was written twice (duplicate bytes, over-advanced
    // cursor); this test fails on that behaviour.
    const RING: usize = STREAM_BUF_BYTES;
    assert_eq!(RING, 65_536);
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    let sent: Vec<u8> = (0..125_536_u32).map(|i| (i % 251) as u8).collect();

    sender
        .send(&sent[..65_000], SendOptions::default())
        .unwrap();
    let mut buf = vec![0_u8; 60_000];
    let n = receiver.recv(&mut buf[..], RecvOptions::default()).unwrap();
    assert_eq!(n, 60_000);
    assert_eq!(&buf[..60_000], &sent[..60_000]);

    // free space is now 60_536: this send wraps the write index past
    // the buffer end
    let n = sender
        .send(&sent[65_000..], SendOptions::default())
        .unwrap();
    assert_eq!(n, 60_536);

    let mut buf = vec![0_u8; 70_000];
    let n = receiver.recv(&mut buf[..], RecvOptions::default()).unwrap();
    assert_eq!(n, 65_536);
    // the wrapped tail appears exactly once
    assert_eq!(&buf[..5_536], &sent[60_000..65_536]);
    let cont: Vec<u8> = (65_536_u32..125_536).map(|i| (i % 251) as u8).collect();
    assert_eq!(&buf[5_536..65_536], cont.as_slice());
}

#[def_test]
fn unix_stream_barrier_stops_before_plain_data_between_records() {
    // Review case: A+fd1 -> plain MID -> B+fd2. Linux returns "A"+fd1
    // first (the fd segment ends with its own sendmsg; the plain MID
    // in between does NOT extend it), then "MIDB"+fd2.
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();

    let so1 = SendOptions {
        ancillary: vec![Arc::new(1_u32) as AncillaryData],
        ..SendOptions::default()
    };
    sender.send(&b"A"[..], so1).unwrap();
    sender.send(&b"MID"[..], SendOptions::default()).unwrap();
    let so2 = SendOptions {
        ancillary: vec![Arc::new(2_u32) as AncillaryData],
        ..SendOptions::default()
    };
    sender.send(&b"B"[..], so2).unwrap();

    let mut sink: Vec<AncillaryData> = Vec::new();
    let mut buf = [0_u8; 16];
    let n = receiver
        .recv(
            &mut buf[..],
            RecvOptions {
                ancillary: Some(&mut sink),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(buf[0], b'A');
    assert_eq!(sink.len(), 1);
    assert_eq!(sink[0].downcast_ref::<u32>(), Some(&1));

    // second recv: plain MID followed by B with its fd
    let mut sink2: Vec<AncillaryData> = Vec::new();
    let n = receiver
        .recv(
            &mut buf[..],
            RecvOptions {
                ancillary: Some(&mut sink2),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    assert_eq!(n, 4);
    assert_eq!(&buf[..4], b"MIDB");
    assert_eq!(sink2.len(), 1);
    assert_eq!(sink2[0].downcast_ref::<u32>(), Some(&2));
}

#[def_test]
fn unix_stream_barrier_covers_fd_segment_followed_by_plain_data() {
    // Review case: plain PRE -> A+fd1 -> plain POST. Linux returns
    // "PREA"+fd1 first (prefix plus the fd segment), leaving "POST".
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();

    sender.send(&b"PRE"[..], SendOptions::default()).unwrap();
    let so = SendOptions {
        ancillary: vec![Arc::new(7_u32) as AncillaryData],
        ..SendOptions::default()
    };
    sender.send(&b"A"[..], so).unwrap();
    sender.send(&b"POST"[..], SendOptions::default()).unwrap();

    let mut sink: Vec<AncillaryData> = Vec::new();
    let mut buf = [0_u8; 16];
    let n = receiver
        .recv(
            &mut buf[..],
            RecvOptions {
                ancillary: Some(&mut sink),
                ..RecvOptions::default()
            },
        )
        .unwrap();
    assert_eq!(n, 4);
    assert_eq!(&buf[..4], b"PREA");
    assert_eq!(sink.len(), 1);

    let n = receiver.recv(&mut buf[..], RecvOptions::default()).unwrap();
    assert_eq!(n, 4);
    assert_eq!(&buf[..4], b"POST");
}

#[def_test]
fn unix_stream_recv_without_control_buffer_reports_discard() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());

    assert_eq!(
        left.send(
            &b"abc"[..],
            SendOptions {
                ancillary: vec![Arc::new(9_u32) as AncillaryData],
                ..SendOptions::default()
            }
        ),
        Ok(3)
    );

    // Consuming the record's segment without a control buffer drops the
    // record and reports the loss (MSG_CTRUNC) to the caller.
    let nonblocking = RecvFlags::DONT_WAIT;
    let mut data = [0_u8; 1];
    let mut discarded = false;
    let recv = right.recv(
        &mut data[..],
        RecvOptions {
            flags: nonblocking,
            ancillary: None,
            ancillary_discarded: Some(&mut discarded),
            ..RecvOptions::default()
        },
    );
    assert_eq!(recv, Ok(1));
    assert!(discarded);

    // Plain data afterwards reaches no record, so the flag stays clear.
    let mut data = [0_u8; 2];
    let mut discarded = false;
    let recv = right.recv(
        &mut data[..],
        RecvOptions {
            flags: nonblocking,
            ancillary: None,
            ancillary_discarded: Some(&mut discarded),
            ..RecvOptions::default()
        },
    );
    assert_eq!(recv, Ok(2));
    assert!(!discarded);
}

#[def_test]
fn unix_stream_peek_duplicates_control_data_and_preserves_data() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    right
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();

    assert_eq!(left.send(&b"abc"[..], SendOptions::default()), Ok(3));
    assert_eq!(
        left.send(
            &b"def"[..],
            SendOptions {
                ancillary: vec![control(7)],
                ..SendOptions::default()
            }
        ),
        Ok(3)
    );

    // The peek window reaches the record's segment: it duplicates the
    // shared entry references without consuming the record or the data,
    // and stops at the segment end like a consuming recv.
    let peek = RecvFlags::PEEK | RecvFlags::DONT_WAIT;
    let mut data = [0_u8; 6];
    let mut ancillary = Vec::new();
    assert_eq!(
        right.recv(
            &mut data[..],
            RecvOptions {
                flags: peek,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(6)
    );
    assert_eq!(&data[..6], b"abcdef");
    assert_eq!(received_controls(&ancillary), vec![7]);

    // A second peek duplicates the same control data again.
    let mut ancillary = Vec::new();
    assert_eq!(
        right.recv(
            &mut data[..],
            RecvOptions {
                flags: peek,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(6)
    );
    assert_eq!(received_controls(&ancillary), vec![7]);

    // A consuming recv still receives both the data and the record.
    let nonblocking = RecvFlags::DONT_WAIT;
    let mut ancillary = Vec::new();
    assert_eq!(
        right.recv(
            &mut data[..],
            RecvOptions {
                flags: nonblocking,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(6)
    );
    assert_eq!(&data[..6], b"abcdef");
    assert_eq!(received_controls(&ancillary), vec![7]);

    // The record is retired after the consuming recv; the stream is then
    // empty.
    let mut ancillary = Vec::new();
    assert_eq!(
        right.recv(
            &mut data[..],
            RecvOptions {
                flags: nonblocking,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Err(KError::WouldBlock)
    );
    assert!(received_controls(&ancillary).is_empty());
}

#[def_test]
fn unix_stream_peek_without_control_buffer_reports_discard_and_keeps_batch() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    right
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();

    assert_eq!(
        left.send(
            &b"abc"[..],
            SendOptions {
                ancillary: vec![control(9)],
                ..SendOptions::default()
            }
        ),
        Ok(3)
    );

    // A peek without a control buffer cannot emit the reached entries
    // either, so the loss is reported even though the peek keeps the
    // record queued for the consuming receive. A one-byte buffer keeps
    // the partial-touch path: reaching only the record's first byte
    // already duplicates -- and here reports -- the record.
    let peek = RecvFlags::PEEK | RecvFlags::DONT_WAIT;
    let mut data = [0_u8; 1];
    let mut discarded = false;
    let recv = right.recv(
        &mut data[..],
        RecvOptions {
            flags: peek,
            ancillary: None,
            ancillary_discarded: Some(&mut discarded),
            ..RecvOptions::default()
        },
    );
    assert_eq!(recv, Ok(1));
    assert_eq!(data[0], b'a');
    assert!(discarded);

    // The record is still delivered by the later consuming receive.
    let nonblocking = RecvFlags::DONT_WAIT;
    let mut data = [0_u8; 3];
    let mut ancillary = Vec::new();
    assert_eq!(
        right.recv(
            &mut data[..],
            RecvOptions {
                flags: nonblocking,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(3)
    );
    assert_eq!(&data[..3], b"abc");
    assert_eq!(received_controls(&ancillary), vec![9]);
    assert_eq!(
        right.recv(
            &mut data[..],
            RecvOptions {
                flags: nonblocking,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Err(KError::WouldBlock)
    );
}

/// Opaque control payload for the quota tests. The transport treats
/// `AncillaryData` as type-erased; tests downcast to verify that a record
/// was delivered rather than dropped or duplicated.
#[derive(Debug, PartialEq)]
struct TestControl(u32);

fn control(id: u32) -> AncillaryData {
    Arc::new(TestControl(id)) as AncillaryData
}

fn received_controls(ancillary: &[AncillaryData]) -> Vec<u32> {
    ancillary
        .iter()
        .filter_map(|item| item.downcast_ref::<TestControl>().map(|control| control.0))
        .collect()
}

fn received_sender_credentials(ancillary: &[AncillaryData]) -> Vec<(Option<u32>, u32, u32)> {
    ancillary
        .iter()
        .filter_map(|item| {
            let credential = item.downcast_ref::<KernelAncillaryData>()?;
            let KernelAncillaryData::Credentials(credential) = credential else {
                return None;
            };
            Some((
                credential.pid.as_ref().map(|pid| pid.root_nr()),
                credential.uid,
                credential.gid,
            ))
        })
        .collect()
}

#[def_test]
fn unix_stream_pending_record_quota_rejects_sends_until_drained() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let one_fd = |id: u32| SendOptions {
        ancillary: vec![control(id)],
        ..SendOptions::default()
    };
    let enobufs = KError::from(LinuxError::ENOBUFS);

    // One-byte sends each pin one retained record; the quota accepts
    // exactly MAX_PENDING_RECORDS of them, then fails the send.
    for id in 0..MAX_PENDING_RECORDS as u32 {
        assert_eq!(left.send(&b"x"[..], one_fd(id)), Ok(1));
    }
    assert_eq!(left.send(&b"x"[..], one_fd(u32::MAX)), Err(enobufs));

    // Plain data is not subject to the record quota.
    assert_eq!(left.send(&b"yy"[..], SendOptions::default()), Ok(2));

    // Retiring one record frees its retention; the next cmsg send
    // succeeds again.
    let mut data = [0_u8; 1];
    let mut ancillary = Vec::new();
    assert_eq!(
        right.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(1)
    );
    assert_eq!(received_controls(&ancillary), vec![0]);
    assert_eq!(left.send(&b"x"[..], one_fd(999)), Ok(1));
}

#[def_test]
fn unix_stream_oversized_control_batch_rejects_with_enobufs() {
    let (left, _right) = StreamTransport::new_pair(test_peer_credentials());

    // A single message whose entries exceed the retention bound is
    // rejected even on an empty queue; each entry is one control message
    // whose descriptor count `posix-net` caps at `SCM_MAX_FD` (253).
    let oversized = SendOptions {
        ancillary: (0..=MAX_PENDING_ANCILLARY as u32).map(control).collect(),
        ..SendOptions::default()
    };
    assert_eq!(
        left.send(&b"x"[..], oversized),
        Err(KError::from(LinuxError::ENOBUFS))
    );
}

#[def_test]
fn unix_stream_blocking_send_registers_its_record_once() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let left = Arc::new(left);
    let send_result = Arc::new(Mutex::new(None));
    // Larger than the ring buffer, so the blocking send is replayed over
    // several `poll_io` rounds and must still register one record.
    let payload = vec![b'x'; STREAM_BUF_BYTES + 16];

    let send_task = ktask::spawn({
        let left = left.clone();
        let send_result = send_result.clone();
        move || {
            let options = SendOptions {
                ancillary: vec![control(7)],
                ..SendOptions::default()
            };
            let result = left.send(&payload[..], options);
            // Close the write side so an early send failure cannot leave
            // the reader blocked.
            left.shutdown(Shutdown::Write).unwrap();
            *send_result.lock() = Some(result);
        }
    });

    let mut buf = vec![0_u8; 4096];
    let mut received = 0;
    let mut first_controls = None;
    let mut later_controls = Vec::new();
    loop {
        let mut ancillary = Vec::new();
        let count = right
            .recv(
                &mut buf[..],
                RecvOptions {
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            )
            .unwrap();
        if count == 0 {
            break;
        }
        if first_controls.is_none() {
            first_controls = Some(received_controls(&ancillary));
        } else {
            later_controls.extend(received_controls(&ancillary));
        }
        received += count;
    }
    send_task.join();

    assert_eq!(
        send_result.lock().take().unwrap(),
        Ok(STREAM_BUF_BYTES + 16)
    );
    assert_eq!(received, STREAM_BUF_BYTES + 16);
    // The one record is delivered with the first recv that reaches the
    // send's first byte, even though the send was replayed over several
    // poll rounds.
    assert_eq!(first_controls, Some(vec![7]));
    assert!(later_controls.is_empty());
}

#[def_test(serial)]
fn unix_stream_nonblocking_ready_recv_wins_over_pending_signal() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let curr = ktask::current();
    curr.clear_interrupt();
    assert_eq!(left.send(&b"x"[..], SendOptions::default()), Ok(1));
    curr.interrupt();

    let mut byte = [0_u8; 1];
    let result = right.recv(
        &mut byte[..],
        RecvOptions {
            flags: RecvFlags::DONT_WAIT,
            ..RecvOptions::default()
        },
    );

    assert_eq!(result, Ok(1));
    assert_eq!(byte, [b'x']);
    assert!(
        curr.is_interrupted(),
        "a ready nonblocking receive must leave the signal pending"
    );
    curr.clear_interrupt();
}

#[def_test(serial)]
fn unix_stream_nonblocking_would_block_preserves_pending_signal() {
    let (_left, right) = StreamTransport::new_pair(test_peer_credentials());
    let curr = ktask::current();
    curr.clear_interrupt();
    curr.interrupt();

    let mut byte = [0_u8; 1];
    let result = right.recv(
        &mut byte[..],
        RecvOptions {
            flags: RecvFlags::DONT_WAIT,
            ..RecvOptions::default()
        },
    );

    assert_eq!(result, Err(KError::WouldBlock));
    assert!(
        curr.is_interrupted(),
        "a nonblocking syscall has no interruptible wait to abort"
    );
    curr.clear_interrupt();
}

#[def_test(serial)]
fn unix_stream_blocking_recv_reports_pending_signal_only_when_no_data() {
    let (_left, right) = StreamTransport::new_pair(test_peer_credentials());
    let curr = ktask::current();
    curr.clear_interrupt();
    curr.interrupt();

    let mut byte = [0_u8; 1];
    let result = right.recv(&mut byte[..], RecvOptions::default());

    assert_eq!(result, Err(KError::Interrupted));
    assert!(
        !curr.is_interrupted(),
        "the signal that aborted the wait is consumed"
    );
}

#[def_test(serial)]
fn unix_stream_blocking_ready_recv_keeps_pending_signal() {
    let (left, right) = StreamTransport::new_pair(test_peer_credentials());
    let curr = ktask::current();
    curr.clear_interrupt();
    assert_eq!(left.send(&b"y"[..], SendOptions::default()), Ok(1));
    curr.interrupt();

    let mut byte = [0_u8; 1];
    let result = right.recv(&mut byte[..], RecvOptions::default());

    assert_eq!(result, Ok(1));
    assert_eq!(byte, [b'y']);
    assert!(
        curr.is_interrupted(),
        "a blocking receive with ready data must not report EINTR"
    );
    curr.clear_interrupt();
}

#[def_test]
fn unix_stream_peer_credentials_follow_listen_and_connect_snapshots() {
    let listener = StreamTransport::new();
    let entry = BindEntry::default();

    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    // Binding alone does not make the socket its own peer; Linux only records
    // the listener identity on a successful listen.
    assert_peer_credentials!(&listener, 0, u32::MAX, u32::MAX);

    let first_listener = peer_credentials_for(11, 1100, 1101);
    listener.listen(1, &first_listener).unwrap();
    assert_peer_credentials!(&listener, 11, 1100, 1101);

    let first_connector = peer_credentials_for(21, 2100, 2101);
    let first_client = StreamTransport::new();
    first_client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &first_connector,
        )
        .unwrap();
    let (first_accepted, _) = block_on(listener.accept(true)).unwrap();

    // Each endpoint reports the other side: the client copied the listener
    // snapshot from listen(), while accept() reports the connector identity.
    assert_peer_credentials!(&first_client, 11, 1100, 1101);
    assert_peer_credentials!(&first_accepted, 21, 2100, 2101);

    let second_listener = peer_credentials_for(12, 1200, 1201);
    listener.listen(1, &second_listener).unwrap();
    assert_peer_credentials!(&listener, 12, 1200, 1201);

    // Re-listening refreshes the listener snapshot and future clients copy it,
    // but an established client keeps the snapshot taken at connect().
    assert_peer_credentials!(&first_client, 11, 1100, 1101);

    let second_connector = peer_credentials_for(22, 2200, 2201);
    let second_client = StreamTransport::new();
    second_client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &second_connector,
        )
        .unwrap();
    let (second_accepted, _) = block_on(listener.accept(true)).unwrap();
    assert_peer_credentials!(&second_client, 12, 1200, 1201);
    assert_peer_credentials!(&second_accepted, 22, 2200, 2201);
}

#[def_test]
fn unix_stream_attaches_credentials_when_receiver_enabled_passcred() {
    use crate::options::GetSocketOption;

    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    let sender = UnixDomainSocket::new(sender, UnixSocketType::Stream);
    let receiver = UnixDomainSocket::new(receiver, UnixSocketType::Stream);
    assert!(!sender.should_attach_sender_credentials());
    assert!(!receiver.should_attach_sender_credentials());

    // Linux `unix_passcred_enabled` checks the receiving peer as well as the
    // sending socket. Enabling the option only on the receiver must still
    // make the sender capture a snapshot.
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    assert!(sender.should_attach_sender_credentials());
    assert!(receiver.should_attach_sender_credentials());

    let mut enabled = false;
    sender
        .get_option(GetSocketOption::PassCredentials(&mut enabled))
        .unwrap();
    assert!(!enabled);
    receiver
        .get_option(GetSocketOption::PassCredentials(&mut enabled))
        .unwrap();
    assert!(enabled);
}

#[def_test]
fn unix_stream_accepted_socket_inherits_listener_passcred_at_connect() {
    use crate::options::GetSocketOption;

    let listener = StreamTransport::new();
    listener
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    let entry = BindEntry::default();
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
    // Linux snapshots the SCM receive flags when `connect()` builds the
    // queued peer sock. Clearing the listener before accept() must not clear
    // the accepted socket's inherited selection.
    listener
        .set_option(SetSocketOption::PassCredentials(&false))
        .unwrap();
    let mut listener_enabled = true;
    listener
        .get_option(GetSocketOption::PassCredentials(&mut listener_enabled))
        .unwrap();
    assert!(!listener_enabled);

    let UnixTransport::Stream(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("stream listener returned a non-stream transport");
    };

    let mut enabled = false;
    accepted
        .get_option(GetSocketOption::PassCredentials(&mut enabled))
        .unwrap();
    assert!(enabled);

    // The accepted socket is the client's receiving peer, so the client now
    // captures automatically without enabling the option itself.
    let client = UnixDomainSocket::new(client, UnixSocketType::Stream);
    assert!(client.should_attach_sender_credentials());
}

#[def_test]
fn unix_stream_accepted_socket_ignores_passcred_enabled_after_connect() {
    use crate::{SocketOps, options::GetSocketOption};

    let listener = StreamTransport::new();
    let entry = BindEntry::default();
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
    // Enabling the listener after connect() must not grant the accepted
    // socket an option that was absent from the queued connection. The
    // connection still has no attached receive socket, so the client keeps
    // capturing credentials until accept().
    listener
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    let client = UnixDomainSocket::new(client, UnixSocketType::Stream);
    assert!(client.should_attach_sender_credentials());
    assert_eq!(
        client.send(
            &b"ab"[..],
            SendOptions {
                sender_credentials: Some(crate::SenderCredentials {
                    pid: Some(kidentity::PidHandle::fixed_root(46)),
                    uid: 4600,
                    gid: 4601,
                }),
                ..SendOptions::default()
            }
        ),
        Ok(2)
    );

    let UnixTransport::Stream(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("stream listener returned a non-stream transport");
    };
    accepted
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    let mut enabled = true;
    accepted
        .get_option(GetSocketOption::PassCredentials(&mut enabled))
        .unwrap();
    assert!(!enabled);

    let mut data = [0_u8; 2];
    let mut ancillary = Vec::new();
    assert_eq!(
        accepted.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(2)
    );
    assert_eq!(&data, b"ab");
    assert!(received_sender_credentials(&ancillary).is_empty());
    // accept() cleared the pre-accept gate; neither endpoint now requests
    // credentials for subsequent traffic.
    assert!(!client.should_attach_sender_credentials());
}

#[def_test(user)]
fn unix_stream_data_sent_before_accept_carries_credentials() {
    use kalloc_types::KBox;
    use kvfs::OpenFlags;

    use crate::{Socket, socket::file::sock_alloc_file};

    let listener = StreamTransport::new();
    let entry = BindEntry::default();
    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &test_peer_credentials()).unwrap();
    listener
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();

    let client = StreamTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();
    let client = UnixDomainSocket::new(client, UnixSocketType::Stream);
    // Linux `unix_passcred_enabled` treats the receiving socket as requesting
    // credentials until it has been accepted (`!other->sk_socket`), so the
    // client must snapshot even though the accepted socket does not exist yet.
    assert!(client.should_attach_sender_credentials());
    let client = sock_alloc_file(
        Socket::Unix(KBox::new(client).unwrap()),
        (OpenFlags::READ_WRITE | OpenFlags::NONBLOCK).bits(),
        kprocess::current_cred(),
    )
    .unwrap();
    assert_eq!(client.write(b"ab"), Ok(2));

    let UnixTransport::Stream(accepted) = block_on(listener.accept(true)).unwrap().0 else {
        panic!("stream listener returned a non-stream transport");
    };
    accepted
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    let mut data = [0_u8; 4];
    let mut ancillary = Vec::new();
    assert_eq!(
        accepted.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(2)
    );
    assert_eq!(&data[..2], b"ab");

    let thread = kprocess::current_user_thread();
    let cred = thread.cred();
    assert_eq!(
        received_sender_credentials(&ancillary),
        vec![(
            Some(thread.process().pid_identity().root_nr()),
            cred.ruid(),
            cred.rgid()
        )]
    );
}

#[def_test]
fn unix_stream_passcred_stops_before_credential_record_start() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();

    // The first send has no ancillary record at all. Its bytes precede the
    // credential-bearing record and must not be merged with it: Linux stops
    // before an skb whose credentials have not been collected yet.
    assert_eq!(sender.send(&b"ab"[..], SendOptions::default()), Ok(2));
    assert_eq!(
        sender.send(
            &b"cd"[..],
            SendOptions {
                sender_credentials: Some(crate::SenderCredentials {
                    pid: Some(kidentity::PidHandle::fixed_root(47)),
                    uid: 4700,
                    gid: 4701,
                }),
                ..SendOptions::default()
            }
        ),
        Ok(2)
    );

    let mut data = [0_u8; 8];
    let mut first_ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut first_ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(2)
    );
    assert_eq!(&data[..2], b"ab");
    assert!(received_sender_credentials(&first_ancillary).is_empty());

    let mut second_ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut second_ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(2)
    );
    assert_eq!(&data[..2], b"cd");
    assert_eq!(
        received_sender_credentials(&second_ancillary),
        vec![(Some(47), 4700, 4701)]
    );
}

#[def_test(serial)]
fn unix_stream_blocking_send_attaches_credentials_to_every_round() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    let sender = Arc::new(sender);
    let send_result = Arc::new(Mutex::new(None));
    let payload = vec![b'x'; STREAM_BUF_BYTES + 16];
    let credential = crate::SenderCredentials {
        pid: Some(kidentity::PidHandle::fixed_root(48)),
        uid: 4800,
        gid: 4801,
    };

    let send_task = ktask::spawn({
        let sender = sender.clone();
        let send_result = send_result.clone();
        let credential = credential.clone();
        move || {
            let result = sender.send(
                &payload[..],
                SendOptions {
                    sender_credentials: Some(credential),
                    ..SendOptions::default()
                },
            );
            *send_result.lock() = Some(result);
        }
    });

    let buffer_is_full = wait_until(|| {
        receiver
            .channel
            .lock()
            .as_ref()
            .is_some_and(|channel| channel.rx.occupied_len() == STREAM_BUF_BYTES)
    });
    if !buffer_is_full {
        sender.shutdown(Shutdown::Write).unwrap();
        send_task.join();
    }
    assert!(buffer_is_full);

    let mut data = vec![0_u8; STREAM_BUF_BYTES];
    let mut first_ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                ancillary: Some(&mut first_ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(STREAM_BUF_BYTES)
    );
    assert_eq!(
        received_sender_credentials(&first_ancillary),
        vec![(Some(48), 4800, 4801)]
    );

    // The first round filled the ring exactly. The residual 16 bytes can only
    // be published after this read frees capacity, so this receive reaches the
    // second skb of the same sendmsg.
    let mut tail = [0_u8; 16];
    let mut tail_ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut tail[..],
            RecvOptions {
                ancillary: Some(&mut tail_ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(16)
    );
    assert_eq!(
        received_sender_credentials(&tail_ancillary),
        vec![(Some(48), 4800, 4801)]
    );
    send_task.join();
    assert_eq!(send_result.lock().take(), Some(Ok(STREAM_BUF_BYTES + 16)));
}

#[def_test]
fn unix_stream_credential_only_filter_does_not_report_truncation() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    assert_eq!(
        sender.send(
            &b"abc"[..],
            SendOptions {
                sender_credentials: Some(crate::SenderCredentials {
                    pid: Some(kidentity::PidHandle::fixed_root(49)),
                    uid: 4900,
                    gid: 4901,
                }),
                ..SendOptions::default()
            }
        ),
        Ok(3)
    );

    let mut data = [0_u8; 3];
    let mut discarded = false;
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary_discarded: Some(&mut discarded),
                ..RecvOptions::default()
            },
        ),
        Ok(3)
    );
    assert!(!discarded);
}

#[def_test]
fn unix_stream_coalescing_compares_pid_identity_not_number() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    let first_pid = kidentity::PidHandle::fixed_root(50);
    let second_pid = kidentity::PidHandle::fixed_root(50);
    let first = crate::SenderCredentials {
        pid: Some(first_pid.clone()),
        uid: 5000,
        gid: 5001,
    };
    let second = crate::SenderCredentials {
        pid: Some(second_pid.clone()),
        uid: 5000,
        gid: 5001,
    };

    for byte in b"ab" {
        assert_eq!(
            sender.send(
                core::slice::from_ref(byte),
                SendOptions {
                    sender_credentials: Some(if *byte == b'a' {
                        first.clone()
                    } else {
                        second.clone()
                    }),
                    ..SendOptions::default()
                }
            ),
            Ok(1)
        );
    }

    let mut data = [0_u8; 2];
    let mut first_ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut first_ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(1)
    );
    assert_eq!(&data[..1], b"a");
    let delivered = first_ancillary
        .iter()
        .find_map(|item| match item.downcast_ref::<KernelAncillaryData>()? {
            KernelAncillaryData::Credentials(credentials) => Some(credentials),
            _ => None,
        })
        .unwrap();
    assert!(Arc::ptr_eq(delivered.pid.as_ref().unwrap(), &first_pid));

    let mut second_ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut second_ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(1)
    );
    assert_eq!(&data[..1], b"b");
    let delivered = second_ancillary
        .iter()
        .find_map(|item| match item.downcast_ref::<KernelAncillaryData>()? {
            KernelAncillaryData::Credentials(credentials) => Some(credentials),
            _ => None,
        })
        .unwrap();
    assert!(Arc::ptr_eq(delivered.pid.as_ref().unwrap(), &second_pid));
}

#[def_test]
fn unix_stream_peer_credentials_keep_effective_ids_independent_of_pid_visibility() {
    let listener = StreamTransport::new();
    let entry = BindEntry::default();
    let listener_namespace = Arc::new(kidentity::PidNamespace::new_root());
    let observer_namespace = Arc::new(kidentity::PidNamespace::new_root());
    let listener_peer = PeerCredentials::new(
        kidentity::PidHandle::allocate_in(&listener_namespace).unwrap(),
        0,
        65534,
    );

    listener.bind(&entry, &UnixAddr::Unbound).unwrap();
    listener.listen(1, &listener_peer).unwrap();
    let client = StreamTransport::new();
    client
        .connect(
            &entry,
            &UnixAddr::Unbound,
            ConnectOptions::default(),
            &test_peer_credentials(),
        )
        .unwrap();

    let mut snapshot = PeerCredentials::none();
    client
        .get_option(GetSocketOption::PeerCredentials(&mut snapshot))
        .unwrap();
    let visible = snapshot.resolve_in(&listener_namespace);
    assert_eq!(visible.pid, 1);
    assert_eq!(visible.uid, 0);
    assert_eq!(visible.gid, 65534);

    // The peer exists even when the observer cannot project its PID, so only
    // the PID becomes zero; the effective UID/GID stay real.
    let invisible = snapshot.resolve_in(&observer_namespace);
    assert_eq!(invisible.pid, 0);
    assert_eq!(invisible.uid, 0);
    assert_eq!(invisible.gid, 65534);
}

#[def_test]
fn unix_stream_peek_preserves_kernel_credential_record() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    let credential = crate::SenderCredentials {
        pid: Some(kidentity::PidHandle::fixed_root(41)),
        uid: 4100,
        gid: 4101,
    };
    assert_eq!(
        sender.send(
            &b"abc"[..],
            SendOptions {
                ancillary: vec![control(17)],
                sender_credentials: Some(credential.clone()),
                ..SendOptions::default()
            }
        ),
        Ok(3)
    );

    let peek = RecvFlags::PEEK | RecvFlags::DONT_WAIT;
    let mut data = [0_u8; 3];
    for _ in 0..2 {
        let mut ancillary = Vec::new();
        assert_eq!(
            receiver.recv(
                &mut data[..],
                RecvOptions {
                    flags: peek,
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            ),
            Ok(3)
        );
        assert_eq!(&data, b"abc");
        assert_eq!(received_controls(&ancillary), vec![17]);
        assert_eq!(
            received_sender_credentials(&ancillary),
            vec![(Some(41), 4100, 4101)]
        );
    }

    let nonblocking = RecvFlags::DONT_WAIT;
    let mut ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: nonblocking,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(3)
    );
    assert_eq!(&data, b"abc");
    assert_eq!(received_controls(&ancillary), vec![17]);
    assert_eq!(
        received_sender_credentials(&ancillary),
        vec![(Some(41), 4100, 4101)]
    );

    let mut ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: nonblocking,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Err(KError::WouldBlock)
    );
    assert!(ancillary.is_empty());
}

#[def_test]
fn unix_stream_peek_preserves_credential_only_record() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    assert_eq!(
        sender.send(
            &b"x"[..],
            SendOptions {
                sender_credentials: Some(crate::SenderCredentials {
                    pid: Some(kidentity::PidHandle::fixed_root(42)),
                    uid: 4200,
                    gid: 4201,
                }),
                ..SendOptions::default()
            }
        ),
        Ok(1)
    );

    // A record carrying credentials but no fds still has to survive peeks and
    // then be delivered exactly once by the consuming read.
    let mut data = [0_u8; 1];
    for _ in 0..2 {
        let mut ancillary = Vec::new();
        assert_eq!(
            receiver.recv(
                &mut data[..],
                RecvOptions {
                    flags: RecvFlags::PEEK | RecvFlags::DONT_WAIT,
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            ),
            Ok(1)
        );
        assert_eq!(&data, b"x");
        assert!(received_controls(&ancillary).is_empty());
        assert_eq!(
            received_sender_credentials(&ancillary),
            vec![(Some(42), 4200, 4201)]
        );
    }

    let mut ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(1)
    );
    assert_eq!(&data, b"x");
    assert_eq!(
        received_sender_credentials(&ancillary),
        vec![(Some(42), 4200, 4201)]
    );
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ..RecvOptions::default()
            }
        ),
        Err(KError::WouldBlock)
    );
}

#[def_test]
fn unix_stream_receive_filters_kernel_credentials_without_passcred() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    assert_eq!(
        sender.send(
            &b"abc"[..],
            SendOptions {
                ancillary: vec![control(18)],
                sender_credentials: Some(crate::SenderCredentials {
                    pid: Some(kidentity::PidHandle::fixed_root(42)),
                    uid: 4200,
                    gid: 4201,
                }),
                ..SendOptions::default()
            }
        ),
        Ok(3)
    );

    let mut data = [0_u8; 3];
    let mut ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(3)
    );
    // Credentials are not ordinary control data: unlike a discarded record,
    // the filter reports no MSG_CTRUNC and the unrelated record still arrives.
    assert_eq!(received_controls(&ancillary), vec![18]);
    assert!(received_sender_credentials(&ancillary).is_empty());
}

#[def_test]
fn unix_stream_passcred_coalesces_equal_credentials_and_stops_on_change() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    let first = crate::SenderCredentials {
        pid: Some(kidentity::PidHandle::fixed_root(43)),
        uid: 4300,
        gid: 4301,
    };
    let second = crate::SenderCredentials {
        pid: Some(kidentity::PidHandle::fixed_root(43)),
        uid: 4400,
        gid: 4401,
    };

    for byte in b"abc" {
        assert_eq!(
            sender.send(
                &[*byte][..],
                SendOptions {
                    sender_credentials: Some(first.clone()),
                    ..SendOptions::default()
                }
            ),
            Ok(1)
        );
    }

    let mut data = [0_u8; 8];
    let mut ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(3)
    );
    assert_eq!(&data[..3], b"abc");
    assert_eq!(
        received_sender_credentials(&ancillary),
        vec![(Some(43), 4300, 4301)]
    );

    assert_eq!(
        sender.send(
            &b"d"[..],
            SendOptions {
                sender_credentials: Some(first),
                ..SendOptions::default()
            }
        ),
        Ok(1)
    );
    assert_eq!(
        sender.send(
            &b"e"[..],
            SendOptions {
                sender_credentials: Some(second),
                ..SendOptions::default()
            }
        ),
        Ok(1)
    );

    let mut data = [0_u8; 8];
    let mut ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(1)
    );
    assert_eq!(&data[..1], b"d");
    assert_eq!(
        received_sender_credentials(&ancillary),
        vec![(Some(43), 4300, 4301)]
    );

    let mut ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(1)
    );
    assert_eq!(&data[..1], b"e");
    assert_eq!(
        received_sender_credentials(&ancillary),
        vec![(Some(43), 4400, 4401)]
    );
}

#[def_test]
fn unix_stream_without_passcred_reads_across_credential_records() {
    let (sender, receiver) = StreamTransport::new_pair(test_peer_credentials());
    receiver
        .set_option(SetSocketOption::NonBlocking(&true))
        .unwrap();
    let first = crate::SenderCredentials {
        pid: Some(kidentity::PidHandle::fixed_root(44)),
        uid: 4400,
        gid: 4401,
    };
    let second = crate::SenderCredentials {
        pid: Some(kidentity::PidHandle::fixed_root(45)),
        uid: 4500,
        gid: 4501,
    };

    for (bytes, credentials) in [
        (&b"x"[..], Some(first)),
        (&b"y"[..], None),
        (&b"z"[..], Some(second)),
    ] {
        assert_eq!(
            sender.send(
                bytes,
                SendOptions {
                    sender_credentials: credentials,
                    ..SendOptions::default()
                }
            ),
            Ok(1)
        );
    }

    let mut data = [0_u8; 8];
    let mut ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(3)
    );
    assert_eq!(&data[..3], b"xyz");
    assert!(received_sender_credentials(&ancillary).is_empty());

    // Credentials queued while the receiver filtered them must not leak into
    // a later read after SO_PASSCRED is enabled.
    receiver
        .set_option(SetSocketOption::PassCredentials(&true))
        .unwrap();
    let third = crate::SenderCredentials {
        pid: Some(kidentity::PidHandle::fixed_root(46)),
        uid: 4600,
        gid: 4601,
    };
    assert_eq!(
        sender.send(
            &b"w"[..],
            SendOptions {
                sender_credentials: Some(third),
                ..SendOptions::default()
            }
        ),
        Ok(1)
    );

    let mut ancillary = Vec::new();
    assert_eq!(
        receiver.recv(
            &mut data[..],
            RecvOptions {
                flags: RecvFlags::DONT_WAIT,
                ancillary: Some(&mut ancillary),
                ..RecvOptions::default()
            },
        ),
        Ok(1)
    );
    assert_eq!(&data[..1], b"w");
    assert_eq!(
        received_sender_credentials(&ancillary),
        vec![(Some(46), 4600, 4601)]
    );
}
