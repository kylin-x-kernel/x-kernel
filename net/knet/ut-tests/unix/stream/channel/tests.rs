// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::VecDeque, sync::Arc, vec, vec::Vec};

use unittest::{assert, assert_eq, def_test};

use super::{DirectionAncillary, EnqueueError, MAX_PENDING_ANCILLARY, MAX_PENDING_RECORDS};
use crate::socket::AncillaryData;

fn ancillary_queue(records: &[(u64, u64, u32)]) -> DirectionAncillary {
    let mut anc = DirectionAncillary {
        sent_total: 100,
        recv_total: 0,
        records: VecDeque::new(),
        retained_ancillary: records.len(),
        consumer_closed: false,
    };
    for (start, end, tag) in records {
        anc.records.push_back((
            *start,
            *end,
            Arc::new(vec![Arc::new(*tag) as AncillaryData]),
        ));
    }
    anc
}

fn tag(record: &[AncillaryData]) -> u32 {
    *record[0].downcast_ref::<u32>().expect("single tag payload")
}

#[def_test]
fn pop_reached_reached_requires_cursor_past_record_start() {
    let mut anc = ancillary_queue(&[(2, 3, 1)]);
    // cursor before the record start: not reached
    assert!(anc.pop_reached().is_none());
    // exactly at the record start: still not reached
    anc.recv_total = 2;
    assert!(anc.pop_reached().is_none());
    // past the start: reached, FIFO
    anc.recv_total = 3;
    assert_eq!(tag(&anc.pop_reached().unwrap().2), 1);
    assert!(anc.pop_reached().is_none());
}

#[def_test]
fn pop_reached_pops_all_reached_records_in_order() {
    let mut anc = ancillary_queue(&[(1, 2, 1), (5, 6, 2), (9, 10, 3)]);
    anc.recv_total = 10;
    assert_eq!(tag(&anc.pop_reached().unwrap().2), 1);
    assert_eq!(tag(&anc.pop_reached().unwrap().2), 2);
    assert_eq!(tag(&anc.pop_reached().unwrap().2), 3);
    assert!(anc.pop_reached().is_none());
}

#[def_test]
fn pop_reached_stops_at_unreached_records() {
    let mut anc = ancillary_queue(&[(1, 2, 1), (5, 6, 2), (90, 91, 3)]);
    anc.recv_total = 10;

    // both reached records come out FIFO...
    assert_eq!(tag(&anc.pop_reached().unwrap().2), 1);
    assert_eq!(tag(&anc.pop_reached().unwrap().2), 2);
    // ...and the unreached one stays queued
    assert_eq!(anc.records.len(), 1);
    assert_eq!(anc.records.front().unwrap().0, 90);
}

#[def_test]
fn pop_reached_drains_everything_when_all_is_reached() {
    let mut anc = ancillary_queue(&[(1, 2, 1), (5, 6, 2)]);
    anc.recv_total = 10;

    assert_eq!(tag(&anc.pop_reached().unwrap().2), 1);
    assert_eq!(tag(&anc.pop_reached().unwrap().2), 2);
    assert!(anc.records.is_empty());
}

#[def_test]
fn enqueue_record_quota_rejects_until_capacity_frees() {
    let mut anc = DirectionAncillary {
        sent_total: 0,
        recv_total: 0,
        records: VecDeque::new(),
        retained_ancillary: 0,
        consumer_closed: false,
    };

    // One-entry records each pin one retained entry; the quota accepts
    // exactly MAX_PENDING_RECORDS of them, then fails the registration
    // and hands the payload back to the caller.
    for id in 0..MAX_PENDING_RECORDS as u32 {
        assert!(
            anc.enqueue_record(0, 1, Arc::new(vec![Arc::new(id) as AncillaryData]))
                .is_ok()
        );
    }
    let (error, payload) = anc
        .enqueue_record(0, 1, Arc::new(vec![Arc::new(u32::MAX) as AncillaryData]))
        .unwrap_err();
    assert_eq!(error, EnqueueError::QuotaExceeded);
    assert_eq!(payload.len(), 1);
}

#[def_test]
fn enqueue_record_oversized_payload_rejects_on_an_empty_queue() {
    let mut anc = DirectionAncillary {
        sent_total: 0,
        recv_total: 0,
        records: VecDeque::new(),
        retained_ancillary: 0,
        consumer_closed: false,
    };

    // A single record whose entries exceed the retention bound is
    // rejected even with nothing queued; each entry is one control
    // message whose descriptor count `posix-net` caps at `SCM_MAX_FD`
    // (253).
    let oversized = Arc::new(
        (0..=MAX_PENDING_ANCILLARY as u32)
            .map(|id| Arc::new(id) as AncillaryData)
            .collect::<Vec<_>>(),
    );
    let (error, payload) = anc.enqueue_record(0, 1, oversized).unwrap_err();
    assert_eq!(error, EnqueueError::QuotaExceeded);
    assert_eq!(payload.len(), MAX_PENDING_ANCILLARY + 1);
}

#[def_test]
fn close_consumer_drains_pending_records_and_rejects_registrations() {
    let mut anc = DirectionAncillary {
        sent_total: 0,
        recv_total: 0,
        records: VecDeque::new(),
        retained_ancillary: 0,
        consumer_closed: false,
    };
    assert!(
        anc.enqueue_record(1, 2, Arc::new(vec![Arc::new(1_u32) as AncillaryData]))
            .is_ok()
    );
    assert!(
        anc.enqueue_record(
            3,
            4,
            Arc::new(vec![
                Arc::new(2_u32) as AncillaryData,
                Arc::new(3_u32) as AncillaryData,
            ])
        )
        .is_ok()
    );

    let drained = anc.close_consumer();
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[1].2.len(), 2);
    assert!(anc.records.is_empty());

    // A closed consumer accepts no new registrations and hands the
    // payload back: the endpoint close drained everything that could
    // ever be delivered.
    let (error, payload) = anc
        .enqueue_record(5, 6, Arc::new(vec![Arc::new(4_u32) as AncillaryData]))
        .unwrap_err();
    assert_eq!(error, EnqueueError::ConsumerClosed);
    assert_eq!(payload.len(), 1);
}
