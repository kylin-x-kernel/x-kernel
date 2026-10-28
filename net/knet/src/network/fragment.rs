// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IPv4 fragment reassembly for local input.
//!
//! [`Ipv4Reassembler`] groups fragments by addresses, identification,
//! protocol, and input interface. Each queue stores non-overlapping payload
//! ranges in offset order until the first header and complete payload are
//! available. [`crate::stack::ingress::IngressProcessor`] drives insertion and timeout removal,
//! then feeds complete packets back through IPv4 input validation.
//!
//! Queue count, retained bytes, and lifetime are bounded. Expired queues retain
//! enough of the first fragment to build an ICMPv4 reassembly-timeout response.

use alloc::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    vec::Vec,
};

use ktime_types::{MonotonicInstant, TimeSpan};

use super::ipv4::{self, Ipv4Header};
use crate::buf::{PacketBuf, PacketOwner, PacketType};

const IPV4_REASSEMBLY_TIMEOUT: TimeSpan = TimeSpan::from_secs(30);
const IPV4_REASSEMBLY_HIGH_BYTES: usize = 4 * 1024 * 1024;
const IPV4_REASSEMBLY_LOW_BYTES: usize = 3 * 1024 * 1024;
const IPV4_REASSEMBLY_MAX_QUEUES: usize = 64;
const IPV4_FRAGMENT_OFFSET_UNIT: usize = 8;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Ipv4FragKey {
    src_addr: [u8; 4],
    dst_addr: [u8; 4],
    identification: u16,
    protocol: u8,
    ifindex: i32,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Ipv4Expiration {
    expires_at: MonotonicInstant,
    key: Ipv4FragKey,
}

#[derive(Debug)]
struct Ipv4FirstFragmentHeader {
    bytes: Vec<u8>,
    ecn: u8,
}

#[derive(Debug)]
struct Ipv4ReassemblyQueue {
    packet_type: PacketType,
    first_header: Option<Ipv4FirstFragmentHeader>,
    fragments: BTreeMap<usize, Vec<u8>>,
    received_len: usize,
    total_payload_len: Option<usize>,
    ecn_mask: u8,
    expires_at: MonotonicInstant,
}

pub(crate) struct ExpiredIpv4Fragment {
    pub(crate) packet_type: PacketType,
    pub(crate) packet: Vec<u8>,
}

pub(crate) struct Ipv4ExpirationBatch {
    pub(crate) fragments: Vec<ExpiredIpv4Fragment>,
    pub(crate) timer_events: usize,
}

pub(crate) enum Ipv4ReassemblyResult {
    Complete(PacketBuf),
    Pending,
    Dropped,
}

#[derive(Default)]
pub(crate) struct Ipv4Reassembler {
    queues: BTreeMap<Ipv4FragKey, Ipv4ReassemblyQueue>,
    expirations: BTreeSet<Ipv4Expiration>,
    memory_bytes: usize,
}

enum InsertResult {
    Inserted { memory_bytes: usize },
    Duplicate,
    Overlap,
}

impl Ipv4Reassembler {
    pub(crate) fn new() -> Self {
        Self {
            queues: BTreeMap::new(),
            expirations: BTreeSet::new(),
            memory_bytes: 0,
        }
    }

    pub(crate) fn expiration_deadline(&self) -> Option<MonotonicInstant> {
        self.expirations
            .first()
            .map(|expiration| expiration.expires_at)
    }

    /// Consumes a fragment whose IPv4 bounds and checksum were checked by ingress.
    pub(crate) fn reassemble(
        &mut self,
        packet: PacketBuf,
        now: MonotonicInstant,
    ) -> Ipv4ReassemblyResult {
        let Some(header) = packet
            .network_packet()
            .and_then(|bytes| Ipv4Header::parse_header_only(bytes).ok())
        else {
            return Ipv4ReassemblyResult::Dropped;
        };
        let key = Ipv4FragKey {
            src_addr: header.src_addr().octets(),
            dst_addr: header.dst_addr().octets(),
            identification: header.identification(),
            protocol: header.protocol(),
            ifindex: packet.ifindex(),
        };
        let Some(ip_packet) = packet.network_packet() else {
            return Ipv4ReassemblyResult::Dropped;
        };
        let Some(payload) = packet.transport_packet() else {
            self.remove_queue(&key);
            return Ipv4ReassemblyResult::Dropped;
        };

        let offset = header.fragment_offset();
        let payload_len = aligned_payload_len(payload.len(), header.more_fragments());
        if payload_len == 0 {
            self.remove_queue(&key);
            return Ipv4ReassemblyResult::Dropped;
        }
        let end = match offset.checked_add(payload_len) {
            Some(end) => end,
            None => {
                self.remove_queue(&key);
                return Ipv4ReassemblyResult::Dropped;
            }
        };
        let fragment_payload = payload[..payload_len].to_vec();
        let queue = match self.queues.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let expires_at = now + IPV4_REASSEMBLY_TIMEOUT;
                self.expirations.insert(Ipv4Expiration { expires_at, key });
                entry.insert(Ipv4ReassemblyQueue {
                    packet_type: packet.packet_type(),
                    first_header: None,
                    fragments: BTreeMap::new(),
                    received_len: 0,
                    total_payload_len: None,
                    ecn_mask: 0,
                    expires_at,
                })
            }
        };

        if !header.more_fragments() {
            if queue
                .total_payload_len
                .is_some_and(|old_total| old_total != end)
                || queue
                    .last_fragment_end()
                    .is_some_and(|old_end| old_end > end)
            {
                self.remove_queue(&key);
                return Ipv4ReassemblyResult::Dropped;
            }
            queue.total_payload_len = Some(end);
        } else if queue
            .total_payload_len
            .is_some_and(|total_payload_len| end > total_payload_len)
        {
            self.remove_queue(&key);
            return Ipv4ReassemblyResult::Dropped;
        }

        match queue.insert_fragment(offset, end, fragment_payload) {
            InsertResult::Inserted { memory_bytes } => {
                self.memory_bytes = self.memory_bytes.saturating_add(memory_bytes);
            }
            InsertResult::Duplicate => return Ipv4ReassemblyResult::Pending,
            InsertResult::Overlap => {
                self.remove_queue(&key);
                return Ipv4ReassemblyResult::Dropped;
            }
        }
        queue.ecn_mask |= ecn_bit(header.ecn());
        if offset == 0 && queue.first_header.is_none() {
            let first_header = Ipv4FirstFragmentHeader {
                bytes: ip_packet[..header.header_len()].to_vec(),
                ecn: header.ecn(),
            };
            self.memory_bytes = self.memory_bytes.saturating_add(first_header.bytes.len());
            queue.first_header = Some(first_header);
        }

        if queue.is_complete() {
            let queue = self.remove_queue(&key).expect("queue exists after insert");
            return build_reassembled_packet(key.ifindex, queue);
        }

        self.enforce_limits();
        Ipv4ReassemblyResult::Pending
    }

    pub(crate) fn remove_expired(
        &mut self,
        now: MonotonicInstant,
        timer_budget: usize,
    ) -> Ipv4ExpirationBatch {
        let mut fragments = Vec::new();
        let mut timer_events = 0;
        while timer_events < timer_budget {
            let Some(expiration) = self.expirations.first().copied() else {
                break;
            };
            if expiration.expires_at > now {
                break;
            }

            let queue = self
                .remove_queue(&expiration.key)
                .expect("expiration references a live reassembly queue");
            timer_events += 1;
            let Some(first_packet) = queue.first_fragment_packet() else {
                continue;
            };
            // Non-final payloads may have been shortened to an eight-byte boundary.
            if ipv4::Ipv4Header::parse_input(&first_packet).is_err() {
                continue;
            }
            fragments.push(ExpiredIpv4Fragment {
                packet_type: queue.packet_type,
                packet: first_packet,
            });
        }
        Ipv4ExpirationBatch {
            fragments,
            timer_events,
        }
    }

    fn enforce_limits(&mut self) {
        while self.queues.len() > IPV4_REASSEMBLY_MAX_QUEUES {
            let Some(key) = self.oldest_queue_key() else {
                return;
            };
            self.remove_queue(&key);
        }

        while self.memory_bytes > IPV4_REASSEMBLY_HIGH_BYTES {
            let Some(key) = self.oldest_queue_key() else {
                return;
            };
            self.remove_queue(&key);
            if self.memory_bytes <= IPV4_REASSEMBLY_LOW_BYTES {
                break;
            }
        }
    }

    fn oldest_queue_key(&self) -> Option<Ipv4FragKey> {
        self.expirations.first().map(|expiration| expiration.key)
    }

    fn remove_queue(&mut self, key: &Ipv4FragKey) -> Option<Ipv4ReassemblyQueue> {
        let queue = self.queues.remove(key)?;
        self.memory_bytes = self.memory_bytes.saturating_sub(queue.retained_bytes());
        self.expirations.remove(&Ipv4Expiration {
            expires_at: queue.expires_at,
            key: *key,
        });
        Some(queue)
    }
}

impl Ipv4ReassemblyQueue {
    fn insert_fragment(&mut self, offset: usize, end: usize, payload: Vec<u8>) -> InsertResult {
        debug_assert_eq!(end.checked_sub(offset), Some(payload.len()));

        if let Some((&start, existing)) = self.fragments.range(..=offset).next_back() {
            let existing_end = start + existing.len();
            if existing_end > offset {
                return if end <= existing_end {
                    InsertResult::Duplicate
                } else {
                    InsertResult::Overlap
                };
            }
        }

        if let Some((&start, _)) = self.fragments.range(offset..).next()
            && start < end
        {
            return InsertResult::Overlap;
        }

        let memory_bytes = payload.len();
        self.fragments.insert(offset, payload);
        self.received_len = self.received_len.saturating_add(memory_bytes);
        InsertResult::Inserted { memory_bytes }
    }

    fn is_complete(&self) -> bool {
        let Some(total_payload_len) = self.total_payload_len else {
            return false;
        };
        self.first_header.is_some() && self.received_len == total_payload_len
    }

    fn first_fragment_packet(&self) -> Option<Vec<u8>> {
        let header = &self.first_header.as_ref()?.bytes;
        let payload = self.fragments.get(&0)?;
        let mut packet = Vec::with_capacity(header.len().checked_add(payload.len())?);
        packet.extend_from_slice(header);
        packet.extend_from_slice(payload);
        Some(packet)
    }

    fn last_fragment_end(&self) -> Option<usize> {
        self.fragments
            .iter()
            .next_back()
            .map(|(offset, payload)| offset + payload.len())
    }

    fn retained_bytes(&self) -> usize {
        let header_len = self
            .first_header
            .as_ref()
            .map_or(0, |first_header| first_header.bytes.len());
        self.received_len.saturating_add(header_len)
    }
}

fn build_reassembled_packet(ifindex: i32, queue: Ipv4ReassemblyQueue) -> Ipv4ReassemblyResult {
    let Some(first_header) = queue.first_header else {
        return Ipv4ReassemblyResult::Dropped;
    };
    let Some(total_payload_len) = queue.total_payload_len else {
        return Ipv4ReassemblyResult::Dropped;
    };
    let Some(ecn) = reassembled_ecn(queue.ecn_mask, first_header.ecn) else {
        return Ipv4ReassemblyResult::Dropped;
    };
    let first_header = first_header.bytes;
    let packet_len = match first_header.len().checked_add(total_payload_len) {
        Some(packet_len) if packet_len <= u16::MAX as usize => packet_len,
        _ => return Ipv4ReassemblyResult::Dropped,
    };

    let mut packet = alloc::vec![0u8; packet_len];
    packet[..first_header.len()].copy_from_slice(&first_header);
    for (offset, payload) in queue.fragments {
        let start = first_header.len() + offset;
        let end = start + payload.len();
        packet[start..end].copy_from_slice(&payload);
    }
    if ipv4::rewrite_reassembled_header(&mut packet, total_payload_len, ecn).is_err() {
        return Ipv4ReassemblyResult::Dropped;
    }

    Ipv4ReassemblyResult::Complete(PacketBuf::from_ip_packet_vec_with_type(
        ifindex,
        packet,
        PacketOwner::Ipv4Stack,
        queue.packet_type,
    ))
}

fn aligned_payload_len(payload_len: usize, more_fragments: bool) -> usize {
    if more_fragments {
        payload_len / IPV4_FRAGMENT_OFFSET_UNIT * IPV4_FRAGMENT_OFFSET_UNIT
    } else {
        payload_len
    }
}

fn ecn_bit(ecn: u8) -> u8 {
    1 << ecn
}

fn reassembled_ecn(ecn_mask: u8, first_ecn: u8) -> Option<u8> {
    let has_not_ect = ecn_mask & ecn_bit(0) != 0;
    let has_ect_or_ce = ecn_mask & !ecn_bit(0) != 0;
    if has_not_ect && has_ect_or_ce {
        return None;
    }
    if ecn_mask & ecn_bit(3) != 0 {
        Some(3)
    } else {
        Some(first_ecn)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/network/fragment/tests.rs");
