// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MSI-X vector allocation and the `kirq` MSI backend provider.
//!
//! Vectors in `MSIX_VECTOR_BASE..APIC_TIMER_VECTOR` are shared by every MSI-X
//! device. An allocation also binds the immutable message destination used by
//! retrigger, so a live route cannot be silently redirected by recomposition.

use kcpu_id_map::{LogicalCpuId, RawCpuId, raw_cpu_id};
use kspin::{SpinNoIrq, static_lock};

use crate::{APIC_TIMER_VECTOR, MSIX_VECTOR_BASE, send_ipi_raw};

const MSIX_VECTOR_COUNT: usize = (APIC_TIMER_VECTOR as usize) - (MSIX_VECTOR_BASE as usize);
const MSIX_VECTOR_WORD_BITS: usize = u64::BITS as usize;
const MSIX_VECTOR_WORDS: usize = MSIX_VECTOR_COUNT.div_ceil(MSIX_VECTOR_WORD_BITS);

static_lock! {
    static MSIX_VECTOR_ALLOCATOR: SpinNoIrq<MsixVectorAllocator> =
        SpinNoIrq::new(MsixVectorAllocator::new());
}

struct MsixVectorAllocator {
    allocated: [u64; MSIX_VECTOR_WORDS],
    destinations: [Option<u8>; MSIX_VECTOR_COUNT],
}

impl MsixVectorAllocator {
    const fn new() -> Self {
        Self {
            allocated: [0; MSIX_VECTOR_WORDS],
            destinations: [None; MSIX_VECTOR_COUNT],
        }
    }

    fn alloc(&mut self) -> Option<u8> {
        for index in 0..MSIX_VECTOR_COUNT {
            let word = index / MSIX_VECTOR_WORD_BITS;
            let bit = 1u64 << (index % MSIX_VECTOR_WORD_BITS);
            if self.allocated[word] & bit == 0 {
                self.allocated[word] |= bit;
                return Some(MSIX_VECTOR_BASE + index as u8);
            }
        }

        None
    }

    fn free(&mut self, vector: u8) -> bool {
        if !(MSIX_VECTOR_BASE..APIC_TIMER_VECTOR).contains(&vector) {
            return false;
        }

        let index = usize::from(vector - MSIX_VECTOR_BASE);
        let word = index / MSIX_VECTOR_WORD_BITS;
        let bit = 1u64 << (index % MSIX_VECTOR_WORD_BITS);
        let was_allocated = self.allocated[word] & bit != 0;
        self.allocated[word] &= !bit;
        self.destinations[index] = None;
        was_allocated
    }

    fn bind_destination(&mut self, vector: u8, destination: u8) -> bool {
        if !(MSIX_VECTOR_BASE..APIC_TIMER_VECTOR).contains(&vector) {
            return false;
        }
        let index = usize::from(vector - MSIX_VECTOR_BASE);
        if self.allocated[index / MSIX_VECTOR_WORD_BITS] & (1 << (index % MSIX_VECTOR_WORD_BITS))
            == 0
        {
            return false;
        }
        // A live allocation's message target is immutable. Recomposition must
        // not silently redirect retrigger away from the message given to its owner.
        match self.destinations[index] {
            Some(old) => old == destination,
            None => {
                self.destinations[index] = Some(destination);
                true
            }
        }
    }

    fn destination(&self, vector: u8) -> Option<u8> {
        if !(MSIX_VECTOR_BASE..APIC_TIMER_VECTOR).contains(&vector) {
            return None;
        }
        self.destinations[usize::from(vector - MSIX_VECTOR_BASE)]
    }

    fn matches_destination(&self, vector: u8, expected: Option<u8>) -> bool {
        self.destination(vector)
            .is_some_and(|actual| expected.is_none_or(|expected| actual == expected))
    }

    fn retrigger(&self, vector: u8, send: impl FnOnce(u8, u8)) -> bool {
        let Some(destination) = self.destination(vector) else {
            return false;
        };
        send(vector, destination);
        true
    }
}

/// Requests an MSI delivery using the immutable target of its live allocation.
/// The allocation lock covers request issuance, not remote completion. The
/// device owner must still stop and drain delivery before releasing a vector.
/// Call in task/ordinary IRQ context after local APIC preparation, never NMI,
/// retaining the source allocation across the call.
pub fn retrigger_msi(vector: usize) -> bool {
    let Ok(vector) = u8::try_from(vector) else {
        return false;
    };
    MSIX_VECTOR_ALLOCATOR
        .lock()
        .retrigger(vector, |vector, destination| {
            send_ipi_raw(usize::from(vector), RawCpuId::new(usize::from(destination)));
        })
}

/// Checks a live MSI allocation against its retained message target. `Any`
/// accepts that original target; a specific CPU must resolve to the same APIC id.
/// This is an admission check, not an allocation lease or remote-delivery drain.
/// Takes the MSI allocator spin lock; task/ordinary IRQ context is allowed,
/// but NMI and callers already holding that lock are not.
pub fn msi_route_matches(vector: usize, affinity: kirq::IrqAffinity) -> bool {
    let Ok(vector) = u8::try_from(vector) else {
        return false;
    };
    let expected = match affinity {
        kirq::IrqAffinity::Any => None,
        kirq::IrqAffinity::Cpu(cpu) => {
            let Some(raw) = raw_cpu_id(LogicalCpuId::new(cpu)) else {
                return false;
            };
            let Ok(destination) = u8::try_from(raw.as_usize()) else {
                return false;
            };
            Some(destination)
        }
    };
    MSIX_VECTOR_ALLOCATOR
        .lock()
        .matches_destination(vector, expected)
}

unittest::path_mod_test!(tests_routing, "../ut-tests/routing.rs");

#[kiface::provide]
impl kirq::MsiBackendIf {
    fn alloc_msi_vector(
        _token: kirq::MsiBackendToken,
        kind: kirq::MsiKind,
        _affinity: kirq::IrqAffinity,
    ) -> Option<usize> {
        match kind {
            kirq::MsiKind::PciMsix => MSIX_VECTOR_ALLOCATOR.lock().alloc().map(usize::from),
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: KIRQ constructs the reservation only from \
                  this backend's successful u8 vector allocation"
    )]
    fn free_msi_vector(reservation: kirq::MsiReservation) {
        let vector = u8::try_from(reservation.hwirq()).expect("owned MSI vector fits u8");
        let was_allocated = MSIX_VECTOR_ALLOCATOR.lock().free(vector);
        // One non-cloneable reservation is created per successful allocation;
        // no other production path can free this private allocator's bit.
        assert!(was_allocated, "owned MSI vector must still be allocated");
    }

    fn compose_msi_message(
        _token: kirq::MsiBackendToken,
        hwirq: usize,
        affinity: kirq::IrqAffinity,
    ) -> Option<kirq::MsiMessage> {
        let Ok(vector) = u8::try_from(hwirq) else {
            return None;
        };
        if !(MSIX_VECTOR_BASE..APIC_TIMER_VECTOR).contains(&vector) {
            return None;
        }

        let apic_id = apic_id_for_affinity(affinity)?;
        if !MSIX_VECTOR_ALLOCATOR
            .lock()
            .bind_destination(vector, apic_id)
        {
            return None;
        }
        let msg_addr = 0xFEE0_0000u64 | (u64::from(apic_id) << 12);
        let msg_data = u32::from(vector);
        Some(kirq::MsiMessage::new(msg_addr, msg_data))
    }
}

fn fallback_local_apic_id() -> u8 {
    raw_cpuid::CpuId::new()
        .get_feature_info()
        .map_or(0, |f| f.initial_local_apic_id())
}

fn apic_id_for_affinity(affinity: kirq::IrqAffinity) -> Option<u8> {
    match affinity {
        kirq::IrqAffinity::Any => Some(fallback_local_apic_id()),
        kirq::IrqAffinity::Cpu(cpu) => {
            let logical_cpu_id = LogicalCpuId::new(cpu);
            let Some(raw) = raw_cpu_id(logical_cpu_id) else {
                warn!("MSI affinity target CPU {cpu} has no raw APIC id");
                return None;
            };
            let Ok(apic_id) = u8::try_from(raw.as_usize()) else {
                warn!(
                    "MSI affinity target CPU {cpu} raw APIC id {} exceeds xAPIC MSI destination \
                     width",
                    raw.as_usize()
                );
                return None;
            };
            Some(apic_id)
        }
    }
}
