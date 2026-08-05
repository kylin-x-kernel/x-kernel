// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ descriptor and wake-subscription state.

use alloc::collections::{BTreeMap, btree_map::Entry};

use kspin::SpinNoIrq;

use super::{
    Hwirq, IrqAffinity, IrqController, IrqDesc, IrqDescError, IrqDomainId, IrqFlags, IrqPolarity,
    IrqSource, IrqTrigger, Virq,
};
use crate::platform::Handler;

pub(super) static IRQ_STATE: SpinNoIrq<IrqState> = SpinNoIrq::new(IrqState::new());
pub const DYNAMIC_VIRQ_BASE: Virq = 4096;

pub(super) type WakeHandler = fn(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MappingKey {
    domain: IrqDomainId,
    hwirq: Hwirq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WakeupMode {
    Persistent,
    OneShot,
}

#[derive(Clone, Copy)]
pub(super) struct WakeSubscription {
    pub(super) mode: WakeupMode,
    pub(super) armed: bool,
    pub(super) handler: WakeHandler,
}

#[derive(Clone)]
pub(super) struct IrqStateDesc {
    pub(super) desc: IrqDesc,
    pub(super) handler: Option<Handler>,
    pub(super) wake_subscription: Option<WakeSubscription>,
}

impl IrqStateDesc {
    pub(super) const fn new(desc: IrqDesc) -> Self {
        Self {
            desc,
            handler: None,
            wake_subscription: None,
        }
    }

    pub(super) fn is_unused(&self) -> bool {
        self.handler.is_none() && self.wake_subscription.is_none()
    }
}

pub(super) struct IrqState {
    pub(super) descs: BTreeMap<Virq, IrqStateDesc>,
    mappings: BTreeMap<MappingKey, Virq>,
    mappings_dirty: bool,
    next_virq: Virq,
}

impl IrqState {
    const fn new() -> Self {
        Self {
            descs: BTreeMap::new(),
            mappings: BTreeMap::new(),
            mappings_dirty: false,
            next_virq: DYNAMIC_VIRQ_BASE,
        }
    }

    fn alloc_virq(&mut self) -> Result<Virq, IrqDescError> {
        loop {
            let virq = self.next_virq;
            self.next_virq = self
                .next_virq
                .checked_add(1)
                .ok_or(IrqDescError::VirqExhausted { next: virq })?;
            if !self.descs.contains_key(&virq) {
                return Ok(virq);
            }
        }
    }

    pub(super) fn try_resolve_desc(&mut self, mut desc: IrqDesc) -> Result<IrqDesc, IrqDescError> {
        let (virq, mapping_to_insert) = if let Some(virq) = desc.logical_irq() {
            if desc.domain.is_none()
                && desc.hwirq == virq
                && matches!(desc.trigger, IrqTrigger::Unknown(_))
                && desc.polarity == IrqPolarity::Unknown
                && desc.source == IrqSource::Unknown
                && desc.controller == IrqController::Unknown
                && desc.affinity == IrqAffinity::Any
                && desc.flags.is_empty()
                && let Some(existing) = self.stored_desc(virq)
            {
                return Ok(existing);
            }
            if let Some(existing) = self.descs.get(&virq) {
                existing.desc.try_merge(desc)?;
            }
            if let Some(domain) = desc.domain {
                let key = MappingKey {
                    domain,
                    hwirq: desc.hwirq,
                };
                if let Some(&existing) = self.mappings.get(&key)
                    && existing != virq
                {
                    return Err(IrqDescError::MappingConflict {
                        domain,
                        hwirq: desc.hwirq,
                        existing,
                        newer: virq,
                    });
                }
                (
                    virq,
                    (!self.mappings.contains_key(&key)).then_some((key, virq)),
                )
            } else {
                (virq, None)
            }
        } else if let Some(domain) = desc.domain {
            let key = MappingKey {
                domain,
                hwirq: desc.hwirq,
            };
            if let Some(&virq) = self.mappings.get(&key) {
                (virq, None)
            } else {
                let virq = self.alloc_virq()?;
                (virq, Some((key, virq)))
            }
        } else {
            (desc.hwirq, None)
        };
        desc = desc.with_virq(virq);
        let stored_desc = if let Some(existing) = self.descs.get(&virq) {
            existing.desc.try_merge(desc)?
        } else {
            desc
        };
        if let Some((key, virq)) = mapping_to_insert
            && let Entry::Vacant(entry) = self.mappings.entry(key)
        {
            entry.insert(virq);
            self.mappings_dirty = true;
        }
        self.descs
            .entry(virq)
            .and_modify(|state| state.desc = stored_desc)
            .or_insert_with(|| IrqStateDesc::new(stored_desc));
        Ok(stored_desc)
    }

    pub(super) fn lookup_virq(&self, desc: IrqDesc) -> Option<Virq> {
        desc.logical_irq().or_else(|| {
            desc.domain.and_then(|domain| {
                self.mappings
                    .get(&MappingKey {
                        domain,
                        hwirq: desc.hwirq,
                    })
                    .copied()
            })
        })
    }

    #[cfg(unittest)]
    pub(super) fn translated_hwirq(&self, domain: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
        self.mappings.get(&MappingKey { domain, hwirq }).copied()
    }

    #[cfg(unittest)]
    pub(super) fn set_next_virq_for_tests(&mut self, next_virq: Virq) {
        self.next_virq = next_virq;
    }

    #[cfg(unittest)]
    pub(super) const fn next_virq_for_tests(&self) -> Virq {
        self.next_virq
    }

    pub(super) fn stored_desc(&self, virq: Virq) -> Option<IrqDesc> {
        self.descs.get(&virq).map(|state| state.desc)
    }

    fn take_mappings_dirty(&mut self) -> bool {
        let dirty = self.mappings_dirty;
        self.mappings_dirty = false;
        dirty
    }

    pub(super) fn remove_if_unused(&mut self, virq: Virq) {
        self.remove_if_unused_inner(virq, false);
    }

    fn remove_if_unused_inner(&mut self, virq: Virq, remove_msi: bool) -> Option<Hwirq> {
        let entry = self.descs.get(&virq)?;
        if entry.desc.flags.contains(IrqFlags::MSI) && !remove_msi {
            return None;
        }
        if !entry.is_unused() {
            return None;
        }
        let hwirq = entry.desc.hwirq;
        if remove_msi && let Some(domain) = entry.desc.domain {
            self.mappings.remove(&MappingKey { domain, hwirq });
        }
        self.descs.remove(&virq);
        Some(hwirq)
    }

    #[cfg(any(target_arch = "x86_64", unittest))]
    pub(super) fn remove_msi_if_unused(&mut self, virq: Virq) -> Option<Hwirq> {
        let entry = self.descs.get(&virq)?;
        if !entry.desc.flags.contains(IrqFlags::MSI) {
            return None;
        }
        let domain = entry.desc.domain;
        let hwirq = self.remove_if_unused_inner(virq, true)?;
        if let Some(domain) = domain {
            let published = super::domain::publish_snapshot(domain, mappings_of(self, domain));
            if !published {
                warn!(
                    "remove_msi_if_unused: unregistered irq domain {domain:?}; removed MSI \
                     mapping will not be reflected on the data path"
                );
            }
        }
        Some(hwirq)
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn is_unused(&self, virq: Virq) -> bool {
        self.descs.get(&virq).is_some_and(IrqStateDesc::is_unused)
    }
}

pub(super) fn try_resolve_and_publish(
    state: &mut IrqState,
    desc: IrqDesc,
) -> Result<IrqDesc, IrqDescError> {
    let domain_id = desc.domain;
    if let Some(domain_id) = domain_id
        && super::domain::domain(domain_id).is_none()
    {
        return Err(IrqDescError::UnknownDomain { domain: domain_id });
    }
    let desc = state.try_resolve_desc(desc)?;
    if let Some(domain_id) = domain_id
        && state.take_mappings_dirty()
    {
        let published = super::domain::publish_snapshot(domain_id, mappings_of(state, domain_id));
        debug_assert!(published);
    }
    Ok(desc)
}

fn mappings_of(
    state: &IrqState,
    domain_id: IrqDomainId,
) -> impl Iterator<Item = (Hwirq, Virq)> + '_ {
    state
        .mappings
        .iter()
        .filter(move |(key, _)| key.domain == domain_id)
        .map(|(key, &virq)| (key.hwirq, virq))
}
