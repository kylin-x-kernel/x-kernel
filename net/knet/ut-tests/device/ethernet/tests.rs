// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec, vec::Vec};
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::{RawWaker, RawWakerVTable, Waker},
};

use unittest::{assert_eq, def_test};

use super::*;

/// # Safety
///
/// `data` must be the aligned pointer to a live static `AtomicUsize`
/// installed by `make_waker`. It must remain valid for every cloned waker;
/// these callbacks borrow the counter and never reclaim its allocation.
unsafe fn waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &WAKER_VTABLE)
}

/// # Safety
///
/// `data` must be the aligned pointer to a live static `AtomicUsize`
/// installed by `make_waker`. It must remain valid for every cloned waker;
/// these callbacks borrow the counter and never reclaim its allocation.
unsafe fn waker_wake(data: *const ()) {
    // SAFETY: test wakers install a leaked, aligned `AtomicUsize` pointer.
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

/// # Safety
///
/// `data` must be the aligned pointer to a live static `AtomicUsize`
/// installed by `make_waker`. It must remain valid for every cloned waker;
/// these callbacks borrow the counter and never reclaim its allocation.
unsafe fn waker_wake_by_ref(data: *const ()) {
    // SAFETY: test wakers install a leaked, aligned `AtomicUsize` pointer.
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

/// # Safety
///
/// `data` must be the aligned pointer to a live static `AtomicUsize`
/// installed by `make_waker`. It must remain valid for every cloned waker;
/// these callbacks borrow the counter and never reclaim its allocation.
unsafe fn waker_drop(_data: *const ()) {}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

fn new_counter() -> &'static AtomicUsize {
    Box::leak(Box::new(AtomicUsize::new(0)))
}

fn make_waker(counter: &'static AtomicUsize) -> Waker {
    let raw = RawWaker::new(counter as *const _ as *const (), &WAKER_VTABLE);
    // SAFETY: the raw waker data pointer is the leaked `AtomicUsize` above
    // and the vtable only performs atomic increments on that allocation.
    unsafe { Waker::from_raw(raw) }
}

fn pending_packet(source: crate::ip::Ipv4Address) -> PacketBuf {
    let mut bytes = vec![0; 20];
    bytes[0] = 0x45;
    let packet_len = bytes.len() as u16;
    bytes[2..4].copy_from_slice(&packet_len.to_be_bytes());
    bytes[12..16].copy_from_slice(&source.octets());
    bytes[16..20].copy_from_slice(&[198, 51, 100, 1]);
    PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceTx)
}

#[def_test]
fn remove_pending_packets_with_deleted_source() {
    let deleted = crate::ip::Ipv4Address::new(192, 0, 2, 7);
    let retained = crate::ip::Ipv4Address::new(192, 0, 2, 8);
    let mut pending = VecDeque::from([
        PendingTxPacket {
            next_hop: IpAddress::Ipv4(crate::ip::Ipv4Address::new(198, 51, 100, 1)),
            source_addr: deleted,
            packet: pending_packet(deleted),
        },
        PendingTxPacket {
            next_hop: IpAddress::Ipv4(crate::ip::Ipv4Address::new(198, 51, 100, 2)),
            source_addr: retained,
            packet: pending_packet(retained),
        },
    ]);

    EthernetDevice::remove_pending_packets_with_source(&mut pending, deleted);

    assert_eq!(pending.len(), 1);
    assert_eq!(pending.front().unwrap().source_addr, retained);
}

#[def_test]
fn limited_broadcast_accepts_unspecified_ipv4_source() {
    assert!(is_ipv4_source_usable_for_egress(
        IpAddress::Ipv4(crate::ip::Ipv4Address::BROADCAST),
        crate::ip::Ipv4Address::UNSPECIFIED,
        &[],
    ));
    assert!(!is_ipv4_source_usable_for_egress(
        IpAddress::Ipv4(crate::ip::Ipv4Address::new(192, 0, 2, 1)),
        crate::ip::Ipv4Address::UNSPECIFIED,
        &[],
    ));
}

fn test_source(sources: &mut NetRxSources, id: usize) -> Arc<NetRxPollSource> {
    let source = Arc::new(NetRxPollSource::new(id));
    sources.push(source.clone());
    source
}

fn collect_and_wake_test_sources(sources: &mut NetRxSources) -> (usize, bool) {
    let mut wake_batch: [Option<PollSet>; NET_RX_SOFTIRQ_BATCH] =
        core::array::from_fn(|_| None);
    let result = sources.collect_scheduled_fallback_batch(&mut wake_batch);
    for waiters in wake_batch.into_iter().flatten() {
        waiters.wake();
    }
    result
}

#[def_test(serial)]
fn test_net_rx_softirq_wakes_only_pending_sources() {
    let mut sources = NetRxSources::new();
    let pending_id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
    let idle_id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
    let pending_source = test_source(&mut sources, pending_id);
    let idle_source = test_source(&mut sources, idle_id);
    let pending_counter = new_counter();
    let idle_counter = new_counter();
    let pending_registration = pending_source
        .waiters()
        .register(&make_waker(pending_counter))
        .unwrap();
    let idle_registration = idle_source
        .waiters()
        .register(&make_waker(idle_counter))
        .unwrap();

    pending_source.pending.store(true, Ordering::Release);
    assert_eq!(collect_and_wake_test_sources(&mut sources), (1, false));

    assert_eq!(pending_counter.load(Ordering::SeqCst), 1);
    assert_eq!(idle_counter.load(Ordering::SeqCst), 0);

    drop(pending_registration);
    drop(idle_registration);
}

#[def_test(serial)]
fn test_net_rx_softirq_round_robin_cursor_reaches_sources_after_batch() {
    let mut local_sources = NetRxSources::new();
    let mut sources = Vec::new();
    let mut counters = Vec::new();
    let mut registrations = Vec::new();
    for _ in 0..(NET_RX_SOFTIRQ_BATCH + 2) {
        let id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
        let source = test_source(&mut local_sources, id);
        let counter = new_counter();
        registrations.push(source.waiters().register(&make_waker(counter)).unwrap());
        counters.push(counter);
        sources.push(source);
    }

    for source in &sources {
        source.pending.store(true, Ordering::Release);
    }

    assert_eq!(
        collect_and_wake_test_sources(&mut local_sources),
        (NET_RX_SOFTIRQ_BATCH, true)
    );
    assert_eq!(
        collect_and_wake_test_sources(&mut local_sources),
        (2, false)
    );

    for counter in &counters {
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    drop(registrations);
}

#[def_test(serial)]
fn test_net_rx_softirq_full_batch_without_remaining_pending_does_not_defer() {
    let mut local_sources = NetRxSources::new();
    let mut sources = Vec::new();
    let mut counters = Vec::new();
    let mut registrations = Vec::new();
    for _ in 0..(NET_RX_SOFTIRQ_BATCH + 2) {
        let id = NEXT_NET_RX_SOURCE_ID.fetch_add(1, Ordering::Relaxed);
        let source = test_source(&mut local_sources, id);
        let counter = new_counter();
        registrations.push(source.waiters().register(&make_waker(counter)).unwrap());
        counters.push(counter);
        sources.push(source);
    }

    for source in sources.iter().take(NET_RX_SOFTIRQ_BATCH) {
        source.pending.store(true, Ordering::Release);
    }

    assert_eq!(
        collect_and_wake_test_sources(&mut local_sources),
        (NET_RX_SOFTIRQ_BATCH, false)
    );
    assert_eq!(
        collect_and_wake_test_sources(&mut local_sources),
        (0, false)
    );

    for counter in counters.iter().take(NET_RX_SOFTIRQ_BATCH) {
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
    for counter in counters.iter().skip(NET_RX_SOFTIRQ_BATCH) {
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    drop(registrations);
}
